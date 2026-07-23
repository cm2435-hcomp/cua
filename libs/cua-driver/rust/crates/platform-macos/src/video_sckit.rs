//! Native ScreenCaptureKit video backend (macOS).
//!
//! Replaces the ffmpeg subprocess pipeline with an in-process SCStream +
//! SCRecordingOutput. The key win is TCC: ScreenCaptureKit runs in the
//! same process as cua-driver, so it inherits the daemon's Screen
//! Recording grant. No per-binary subprocess gotcha, no second prompt,
//! no fast-fail-on-hang heuristic.
//!
//! Requires macOS 15.0+ (SCRecordingOutput introduced in macOS 15). The
//! Swift impl this is modelled on lives at
//! `libs/cua-driver/swift/Sources/CuaDriverCore/Recording/VideoRecorder.swift`,
//! though that version composes SCStream + AVAssetWriter manually so it
//! also runs on macOS 14. We use SCRecordingOutput here because the
//! Rust binding doesn't expose AVAssetWriter and macOS 15 is already
//! widespread enough that requiring it is acceptable for the Rust port.
//!
//! Lifecycle:
//!   1. `start(path)` resolves the main display, builds a 30fps full-display
//!      SCStream config + SCRecordingOutput pointing at the mp4 path,
//!      attaches the recording output, calls `start_capture()`.
//!   2. Caller stays alive while recording.
//!   3. `stop()` calls `stop_capture()` (which finalises the mp4 moov
//!      atom) and returns the elapsed-time metadata.

use std::{
    ffi::c_void,
    path::Path,
    sync::mpsc::{sync_channel, RecvTimeoutError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cua_driver_core::video::{VideoBackend, VideoBackendFactory, VideoMetadata};

use screencapturekit::cm::{CMSampleBufferExt, CMSampleBufferSCExt, SCFrameStatus};
use screencapturekit::prelude::{
    CMSampleBuffer, SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutputType,
};
use screencapturekit::recording_output::{
    SCRecordingOutput, SCRecordingOutputCodec, SCRecordingOutputConfiguration,
    SCRecordingOutputFileType,
};

pub struct SckitVideoBackendFactory;

/// Metadata read from the same `CMSampleBuffer` that produced the returned
/// PNG. Keeping pixels and freshness evidence in one value prevents callers
/// from accidentally pairing a new image with stale stream metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowFrameMetadata {
    pub completion_unix_ms: u64,
    pub display_time: Option<u64>,
    pub frame_status: Option<SCFrameStatus>,
    pub scale_factor: Option<f64>,
    pub content_scale: Option<f64>,
    pub content_rect: Option<FrameRect>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowFrameSample {
    pub png_bytes: Vec<u8>,
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// ScreenCaptureKit's exact source-window frame at sample construction.
    pub source_frame: FrameRect,
    pub metadata: WindowFrameMetadata,
}

struct TemporaryCapturePath(std::path::PathBuf);

impl Drop for TemporaryCapturePath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Capture exactly one desktop-independent window through ScreenCaptureKit.
///
/// This is intentionally separate from the long-lived recording stream. A
/// v2 observation needs one bounded sample whose pixels, frame status,
/// display timestamp and scale attachments are coherent. Apple's one-shot
/// `SCScreenshotManager.captureSampleBuffer` returns pixels without
/// `SCStreamFrameInfo` attachments on current macOS, so observations use one
/// exact-window `SCStream` callback instead. The old `screencapture` CLI
/// helper cannot provide this contract and is never called here.
pub fn capture_window_sample(
    window_id: u32,
    expected_scale_factor: f64,
    timeout: Duration,
) -> anyhow::Result<WindowFrameSample> {
    if !expected_scale_factor.is_finite() || expected_scale_factor <= 0.0 {
        anyhow::bail!("invalid expected scale factor for window {window_id}");
    }
    if timeout.is_zero() {
        anyhow::bail!("ScreenCaptureKit window {window_id} sample deadline elapsed");
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| anyhow::anyhow!("invalid ScreenCaptureKit sample timeout"))?;

    let content = SCShareableContent::get()
        .map_err(|error| anyhow::anyhow!("SCShareableContent::get failed: {error}"))?;
    let window = content
        .windows()
        .into_iter()
        .find(|candidate| candidate.window_id() == window_id)
        .ok_or_else(|| anyhow::anyhow!("ScreenCaptureKit window {window_id} is unavailable"))?;
    let frame = window.frame();
    let width = scaled_dimension(frame.size.width, expected_scale_factor, "width")?;
    let height = scaled_dimension(frame.size.height, expected_scale_factor, "height")?;
    let filter = SCContentFilter::create().with_window(&window).build();
    let configuration = SCStreamConfiguration::new()
        .with_width(width)
        .with_height(height)
        .with_queue_depth(2)
        .with_ignores_shadows_single_window(true)
        .with_shows_cursor(false)
        .with_captures_audio(false);

    let (sender, receiver) = sync_channel(2);
    let mut stream = SCStream::new(&filter, &configuration);
    stream
        .add_output_handler(
            move |sample, _| {
                let completion_unix_ms = unix_ms_now();
                let _ = sender.try_send((sample, completion_unix_ms));
            },
            SCStreamOutputType::Screen,
        )
        .ok_or_else(|| {
            anyhow::anyhow!("ScreenCaptureKit failed to register output for window {window_id}")
        })?;
    stream
        .start_capture()
        .map_err(|error| anyhow::anyhow!("ScreenCaptureKit stream start failed: {error}"))?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    let received = if remaining.is_zero() {
        Err(anyhow::anyhow!(
            "ScreenCaptureKit window {window_id} sample deadline elapsed"
        ))
    } else {
        receiver
            .recv_timeout(remaining)
            .map_err(|error| match error {
                RecvTimeoutError::Timeout => anyhow::anyhow!(
                    "ScreenCaptureKit window {window_id} produced no sample before its deadline"
                ),
                RecvTimeoutError::Disconnected => anyhow::anyhow!(
                    "ScreenCaptureKit output for window {window_id} disconnected before a sample"
                ),
            })
    };
    let stop_result = stream.stop_capture().map_err(|error| {
        anyhow::anyhow!("ScreenCaptureKit stream stop failed for window {window_id}: {error}")
    });
    let (sample, completion_unix_ms) = received?;
    stop_result?;

    let frame_info = sample.frame_info();
    let image = sample
        .cg_image()
        .map_err(|status| anyhow::anyhow!("ScreenCaptureKit sample has no image: {status}"))?;
    let pixel_width = u32::try_from(image.width())
        .map_err(|_| anyhow::anyhow!("captured image width exceeds u32"))?;
    let pixel_height = u32::try_from(image.height())
        .map_err(|_| anyhow::anyhow!("captured image height exceeds u32"))?;

    let temporary_path = TemporaryCapturePath(std::env::temp_dir().join(format!(
        "cua-v2-sck-sample-{}-{}.png",
        std::process::id(),
        uuid::Uuid::new_v4()
    )));
    image.save_png(&temporary_path.0).map_err(|error| {
        anyhow::anyhow!("failed to encode ScreenCaptureKit sample for window {window_id}: {error}")
    })?;
    let png_bytes = std::fs::read(&temporary_path.0).map_err(|error| {
        anyhow::anyhow!("failed to read encoded ScreenCaptureKit sample: {error}")
    })?;
    if png_bytes.is_empty() {
        anyhow::bail!("ScreenCaptureKit produced an empty PNG for window {window_id}");
    }

    let metadata = WindowFrameMetadata {
        completion_unix_ms,
        display_time: frame_info.as_ref().and_then(|info| info.display_time),
        // screencapturekit 6.0.1 casts the attachment's NSNumber directly to
        // Swift's SCFrameStatus enum and silently loses it. Read the numeric
        // attachment through CoreFoundation until the dependency fixes that
        // bridge; never infer status from pixels or timestamps.
        frame_status: frame_info
            .as_ref()
            .and_then(|info| info.frame_status)
            .or_else(|| frame_status_from_attachment(&sample)),
        scale_factor: frame_info.as_ref().and_then(|info| info.scale_factor),
        content_scale: frame_info.as_ref().and_then(|info| info.content_scale),
        content_rect: frame_info
            .as_ref()
            .and_then(|info| info.content_rect)
            .map(|rect| FrameRect {
                x: rect.origin.x,
                y: rect.origin.y,
                width: rect.size.width,
                height: rect.size.height,
            }),
    };
    Ok(WindowFrameSample {
        png_bytes,
        pixel_width,
        pixel_height,
        source_frame: FrameRect {
            x: frame.origin.x,
            y: frame.origin.y,
            width: frame.size.width,
            height: frame.size.height,
        },
        metadata,
    })
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// Decode `SCStreamFrameInfoStatus` from the sample attachment dictionary.
///
/// ScreenCaptureKit stores the enum as a CFNumber/NSNumber. This deliberately
/// uses Apple's exported key constant rather than copying its string value.
fn frame_status_from_attachment(sample: &CMSampleBuffer) -> Option<SCFrameStatus> {
    type CFIndex = isize;
    type CFTypeId = usize;
    const CF_NUMBER_SINT32_TYPE: i32 = 3;

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMSampleBufferGetSampleAttachmentsArray(
            sample_buffer: *mut c_void,
            create_if_necessary: u8,
        ) -> *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFArrayGetCount(array: *const c_void) -> CFIndex;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: CFIndex) -> *const c_void;
        fn CFDictionaryGetValue(dictionary: *const c_void, key: *const c_void) -> *const c_void;
        fn CFGetTypeID(value: *const c_void) -> CFTypeId;
        fn CFNumberGetTypeID() -> CFTypeId;
        fn CFNumberGetValue(number: *const c_void, number_type: i32, value: *mut c_void) -> u8;
    }
    #[link(name = "ScreenCaptureKit", kind = "framework")]
    unsafe extern "C" {
        static SCStreamFrameInfoStatus: *const c_void;
    }

    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample.as_ptr(), 0);
        if attachments.is_null() || CFArrayGetCount(attachments) < 1 {
            return None;
        }
        let dictionary = CFArrayGetValueAtIndex(attachments, 0);
        if dictionary.is_null() {
            return None;
        }
        let number = CFDictionaryGetValue(dictionary, SCStreamFrameInfoStatus);
        if number.is_null() || CFGetTypeID(number) != CFNumberGetTypeID() {
            return None;
        }
        let mut raw = 0_i32;
        if CFNumberGetValue(number, CF_NUMBER_SINT32_TYPE, (&mut raw as *mut i32).cast()) == 0 {
            return None;
        }
        SCFrameStatus::from_raw(raw)
    }
}

fn scaled_dimension(points: f64, scale: f64, name: &str) -> anyhow::Result<u32> {
    let pixels = (points * scale).round();
    if !pixels.is_finite() || pixels < 1.0 || pixels > f64::from(u32::MAX) {
        anyhow::bail!("invalid ScreenCaptureKit {name}: {points} points at {scale}x");
    }
    Ok(pixels as u32)
}

impl VideoBackendFactory for SckitVideoBackendFactory {
    fn start(&self, output_path: &Path) -> anyhow::Result<Box<dyn VideoBackend>> {
        SckitVideoBackend::start(output_path).map(|b| Box::new(b) as Box<dyn VideoBackend>)
    }
}

pub struct SckitVideoBackend {
    stream: SCStream,
    // SCStream's add_recording_output is non-owning — Apple's API requires
    // the SCRecordingOutput stay alive for the stream's lifetime, so we
    // keep it parked here. Dropping it before stop_capture aborts the
    // encode mid-file.
    _recording: SCRecordingOutput,
    output_path: std::path::PathBuf,
    started_at: Instant,
}

impl SckitVideoBackend {
    fn start(output_path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow::anyhow!(
                    "failed to create recording output directory {}: {e}",
                    parent.display()
                )
            })?;
        }
        // SCRecordingOutput appends-or-fails on an existing file; match the
        // Swift impl by clearing any stale recording.mp4 from a prior run.
        match std::fs::remove_file(output_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                anyhow::bail!(
                    "failed to remove stale recording file {}: {e}",
                    output_path.display()
                );
            }
        }

        let content = SCShareableContent::get()
            .map_err(|e| anyhow::anyhow!("SCShareableContent::get failed: {e}"))?;
        let displays = content.displays();
        let display = displays
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no displays available for ScreenCaptureKit"))?;

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        // Match the Swift recorder's pixel resolution + 30fps target. The
        // display's reported width/height are in pixels (already
        // backing-scale-multiplied) on SCDisplay, so passing them through
        // gives a native-resolution capture.
        let pixel_width = display.width();
        let pixel_height = display.height();
        let frame_interval = screencapturekit::cm::CMTime::new(1, 30);
        let config = SCStreamConfiguration::new()
            .with_width(pixel_width)
            .with_height(pixel_height)
            .with_minimum_frame_interval(&frame_interval)
            .with_shows_cursor(true);

        let rec_config = SCRecordingOutputConfiguration::new()
            .with_output_url(output_path)
            .with_video_codec(SCRecordingOutputCodec::H264)
            .with_output_file_type(SCRecordingOutputFileType::MP4);

        let recording = SCRecordingOutput::new(&rec_config).ok_or_else(|| {
            anyhow::anyhow!(
                "SCRecordingOutput::new returned nil — macOS 15.0+ is required for \
                 native ScreenCaptureKit video; older macOS needs to use the ffmpeg \
                 backend (currently disabled on macOS)."
            )
        })?;

        let stream = SCStream::new(&filter, &config);
        stream
            .add_recording_output(&recording)
            .map_err(|e| anyhow::anyhow!("SCStream::add_recording_output failed: {e}"))?;
        stream
            .start_capture()
            .map_err(|e| anyhow::anyhow!("SCStream::start_capture failed: {e}"))?;

        tracing::info!(
            target: "recording",
            path = %output_path.display(),
            width = pixel_width,
            height = pixel_height,
            "sckit video capture started"
        );

        Ok(Self {
            stream,
            _recording: recording,
            output_path: output_path.to_path_buf(),
            started_at: Instant::now(),
        })
    }
}

impl VideoBackend for SckitVideoBackend {
    fn stop(self: Box<Self>) -> anyhow::Result<VideoMetadata> {
        let elapsed = self.started_at.elapsed();
        // SCStream::stop_capture finalises the mp4 moov atom synchronously
        // on the recording output before returning. Errors here mean the
        // file may be unplayable — surface as `finalized: false`.
        let finalized = self.stream.stop_capture().is_ok();
        if !finalized {
            tracing::warn!(
                target: "recording",
                "SCStream::stop_capture failed; recording.mp4 may be incomplete"
            );
        }
        Ok(VideoMetadata {
            path: self.output_path,
            duration_ms: elapsed.as_millis() as u64,
            finalized,
        })
    }
}
