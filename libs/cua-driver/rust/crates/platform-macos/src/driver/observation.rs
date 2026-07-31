//! Coherent, window-scoped native observation for the macOS v2 driver.

use std::{
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use core_foundation::{
    base::{CFEqual, CFGetTypeID, CFRelease, CFRetain, CFTypeRef, TCFType},
    string::CFString,
};
use cua_driver_core::api::{
    capabilities::WindowStateKind,
    contracts::{CaptureRevision, ElementId, Rect, Size, SurfaceId, SurfaceKind, WindowRef},
    errors::{ErrorCode, ErrorPhase, NativeError},
    menu::NativeMenuObservation,
    observation::{
        CaptureFreshness, NativeAccessibilityElement, NativeAccessibilityUpdate,
        NativeElementHandle, NativeObservationUpdate, ObservationArtifactHandle, ResolvedWindow,
        ResolvedWindowStamp, SurfaceOwner, SurfaceRecord, SurfaceToWindowTransform,
    },
    platform::{ObservationProvider, ObserveRequest},
    settlement::{DirtyState, SettlementAttempt},
};
use screencapturekit::cm::SCFrameStatus;
use tokio::sync::Semaphore;

use crate::{
    ax::bindings::{
        self, copy_action_names_exact, copy_attr_value, copy_ax_windows, copy_children,
        copy_string_attr, copy_string_attr_exact, element_screen_rect, focused_element_of_pid,
        AXUIElementCreateApplication, AXUIElementRef,
    },
    video_sckit::{capture_window_sample, WindowFrameEvidence, WindowFrameSample},
};

use super::{
    target::MacTargetState,
    windows::{MacRelatedWindowFacts, MacWindowFacts, MacWindowRegistry},
};

const MAX_AX_ELEMENTS: usize = 2_000;
const MAX_AX_DEPTH: usize = 64;
const MAX_NOTIFICATION_ELEMENTS: usize = 8;
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(10);
const SCK_CAPTURE_TEARDOWN_RESERVE: Duration = Duration::from_millis(100);
static SCK_CAPTURE_SLOTS: Semaphore = Semaphore::const_new(2);

#[derive(Clone)]
pub struct MacObservationProvider {
    windows: MacWindowRegistry,
    artifact_root: Arc<PathBuf>,
}

impl MacObservationProvider {
    pub fn new(windows: MacWindowRegistry) -> Self {
        Self {
            windows,
            artifact_root: Arc::new(
                std::env::temp_dir()
                    .join(format!("cua-driver-v2-observations-{}", std::process::id())),
            ),
        }
    }

    async fn attempt(
        &self,
        target: &mut MacTargetState,
        window: &ResolvedWindow,
        request: ObserveRequest,
        deadline: Instant,
    ) -> Result<AttemptResult, AttemptError> {
        let epoch = target.signals.epoch();
        let facts_a = self
            .windows
            .facts_for_identity(&window.stamp())
            .await
            .map_err(classify_stamp_error)?;
        validate_observable(&facts_a).map_err(AttemptError::Fatal)?;
        let pid = facts_a.pid;
        let cg_window_id = facts_a.cg_window_id;
        let include_text = request.include_text;
        let ax_snapshot = tokio::task::spawn_blocking(move || {
            capture_ax_snapshot(pid, cg_window_id, include_text)
        })
        .await
        .map_err(|error| AttemptError::Fatal(join_error("AX capture", error)))?
        .map_err(AttemptError::Fatal)?;

        let related = related_surfaces(&self.windows, &facts_a, &ax_snapshot.related_windows)
            .await
            .map_err(classify_stamp_error)?;
        let related_a: Vec<_> = related
            .iter()
            .map(|surface| surface.facts.clone())
            .collect();
        let mut captures = Vec::new();
        if request.include_screenshots {
            captures.push(
                capture(&facts_a, SurfaceKind::Window, deadline)
                    .await
                    .map_err(classify_capture_error)?,
            );
            for related_surface in related {
                captures.push(
                    capture_related(related_surface, deadline)
                        .await
                        .map_err(classify_capture_error)?,
                );
            }
        }

        let related_b_snapshot = tokio::task::spawn_blocking(move || {
            capture_ax_snapshot(pid, cg_window_id, include_text)
        })
        .await
        .map_err(|error| AttemptError::Fatal(join_error("related AX revalidation", error)))?
        .map_err(AttemptError::Fatal)?;
        if let Err(error) =
            target.replace_observed_elements(related_b_snapshot.notification_elements())
        {
            // The signed helper's observer registration is internal,
            // best-effort state. A slow AX descendant cannot turn an otherwise
            // coherent app-state read into a caller-visible failure; action
            // settlement and the next exact observation remain authoritative.
            tracing::debug!(%error, "skipping slow AX descendant notification refresh");
        }
        if !same_related_identities(
            &ax_snapshot.related_windows,
            &related_b_snapshot.related_windows,
        ) {
            return Err(AttemptError::Raced {
                stage: "related_ax_identity_revalidation",
            });
        }
        let related_b = self
            .windows
            .register_related_windows(
                &facts_a,
                related_candidates(&related_b_snapshot.related_windows),
            )
            .await
            .map_err(classify_stamp_error)?;
        if related_a != related_b {
            return Err(AttemptError::Raced {
                stage: "related_window_registry_revalidation",
            });
        }

        let facts_b = self
            .windows
            .facts_for_identity(&facts_a.stamp)
            .await
            .map_err(classify_stamp_error)?;
        if facts_a != facts_b {
            return Err(AttemptError::Raced {
                stage: "window_facts_bracket",
            });
        }
        if !same_stable_identity(&target.window, &facts_b.stamp) {
            return Err(AttemptError::Raced {
                stage: "target_window_identity_bracket",
            });
        }
        let current_epoch = target.signals.epoch();
        let publish_epoch = if current_epoch == epoch {
            epoch
        } else if ax_snapshot_contract_difference(&ax_snapshot, &related_b_snapshot).is_none() {
            // Some apps (notably Finder) emit AXValueChanged while their AX
            // tree is queried or descendant notifications are installed. A
            // notification alone is not evidence that the published state is
            // stale: accept the later epoch only when two complete snapshots
            // prove the exact same identities and public AX contract. The
            // publication commit still catches any event after this point.
            current_epoch
        } else {
            return Err(AttemptError::Raced {
                stage: ax_snapshot_contract_difference(&ax_snapshot, &related_b_snapshot)
                    .expect("non-equivalent AX snapshots have a named difference"),
            });
        };

        Ok(AttemptResult {
            facts: facts_b,
            related: related_a,
            ax_snapshot: related_b_snapshot,
            captures,
            epoch: publish_epoch,
        })
    }

    fn publish(
        &self,
        target: &mut MacTargetState,
        window: &ResolvedWindow,
        attempt: AttemptResult,
    ) -> Result<NativeObservationUpdate, AttemptError> {
        let epoch = attempt.epoch;
        let journal = target.signals.clone();
        journal
            .commit_if_epoch(epoch, || self.publish_unbracketed(target, window, attempt))
            .map_err(AttemptError::Fatal)?
            .ok_or(AttemptError::Raced {
                stage: "publish_signal_epoch_commit",
            })
    }

    fn publish_unbracketed(
        &self,
        target: &mut MacTargetState,
        window: &ResolvedWindow,
        attempt: AttemptResult,
    ) -> Result<NativeObservationUpdate, NativeError> {
        let observed_window = resolved_from_facts(window, &attempt.facts)?;
        let accessibility = target.elements.reconcile(
            attempt.ax_snapshot,
            attempt.facts.bounds,
            attempt.facts.cg_window_id,
            &attempt.facts.stamp,
            &attempt.related,
        )?;
        let mut surfaces = Vec::with_capacity(attempt.captures.len());
        let mut artifacts = Vec::with_capacity(attempt.captures.len());
        let mut captured_at_unix_ms = now_unix_ms();
        for capture in attempt.captures {
            let freshness = target.frames.classify_and_commit(
                capture.cg_window_id,
                &capture.sample,
                capture.scale_factor,
            )?;
            if !freshness.action_safe() {
                return Err(NativeError::stale(
                    ErrorCode::SurfaceStale,
                    "ScreenCaptureKit did not provide action-safe freshness evidence",
                )
                .with_detail("cg_window_id", capture.cg_window_id));
            }
            captured_at_unix_ms =
                captured_at_unix_ms.max(capture.sample.metadata.completion_unix_ms);
            let (image_url, artifact) = materialize_png(
                self.artifact_root.as_ref(),
                capture.cg_window_id,
                &capture.sample.png_bytes,
            )?;
            artifacts.push(artifact);

            let (owner_window, owner) = match capture.related_owner {
                Some((owner_window, owner)) => (
                    owner_window,
                    SurfaceOwner::RelatedTransient {
                        owner,
                        parent: observed_window.stamp(),
                    },
                ),
                None => (
                    observed_window.public.clone(),
                    SurfaceOwner::Target(observed_window.stamp()),
                ),
            };
            surfaces.push(SurfaceRecord {
                id: SurfaceId::new(),
                kind: capture.kind,
                owner_window,
                image_url,
                raster_size: Size {
                    width: capture.sample.pixel_width,
                    height: capture.sample.pixel_height,
                },
                window_bounds: Some(capture.bounds),
                capture_revision: CaptureRevision::new(),
                observation_epoch: Some(cua_driver_core::api::observation::NativeObservationEpoch(
                    attempt.epoch,
                )),
                transform: capture.transform,
                freshness,
                owner,
                approximate_bytes: capture.sample.png_bytes.len(),
                menu_id: None,
            });
        }

        let mut warnings = Vec::new();
        if accessibility.truncated {
            warnings.push(format!(
                "AX tree truncated at the bounded {MAX_AX_ELEMENTS}-element limit"
            ));
        }
        let update = NativeObservationUpdate {
            window: observed_window.clone(),
            surfaces,
            accessibility: Some(accessibility.update),
            menu: NativeMenuObservation::Unchanged,
            captured_at_unix_ms,
            warnings,
            artifacts,
        };
        target.window = observed_window.stamp();
        Ok(update)
    }
}

#[async_trait]
impl ObservationProvider<MacTargetState> for MacObservationProvider {
    async fn settle(
        &self,
        target: &mut MacTargetState,
        dirty: &DirtyState,
        deadline: std::time::Instant,
    ) -> Result<SettlementAttempt, NativeError> {
        if dirty.profile.target_may_disappear {
            loop {
                match self.windows.facts_for_identity(&target.window).await {
                    Err(missing_error)
                        if matches!(
                            missing_error.code,
                            ErrorCode::WindowNotFound | ErrorCode::WindowIdentityChanged
                        ) =>
                    {
                        target.signals.record(
                            cua_driver_core::api::settlement::SettlementSignal::WindowListChanged,
                        );
                        return Ok(target
                            .signals
                            .settle(dirty, &dirty.profile.relevant_signals, deadline)
                            .await);
                    }
                    Ok(_) => {}
                    Err(error) => return Err(error),
                }
                let now = Instant::now();
                let slice_deadline = (now + Duration::from_millis(20)).min(deadline);
                let attempt = target
                    .signals
                    .settle(dirty, &dirty.profile.relevant_signals, slice_deadline)
                    .await;
                if matches!(attempt, SettlementAttempt::Settled(_)) || slice_deadline == deadline {
                    return Ok(attempt);
                }
            }
        }
        Ok(target
            .signals
            .settle(dirty, &dirty.profile.relevant_signals, deadline)
            .await)
    }

    async fn observe(
        &self,
        target: &mut MacTargetState,
        window: &ResolvedWindow,
        request: ObserveRequest,
    ) -> Result<NativeObservationUpdate, NativeError> {
        if target.invalidated() {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "macOS target resources were invalidated",
            ));
        }
        let deadline = Instant::now() + OBSERVATION_TIMEOUT;
        let mut race_count = 0_u64;
        let mut last_raced_stage = None;
        loop {
            if Instant::now() >= deadline {
                let mut error = NativeError::new(
                    ErrorCode::ObservationRaced,
                    ErrorPhase::Verify,
                    true,
                    "window identity or geometry did not stabilize before the observation deadline",
                )
                .with_detail("race_count", race_count);
                if let Some(stage) = last_raced_stage {
                    error = error.with_detail("raced_stage", stage);
                }
                return Err(error);
            }
            let result = match self.attempt(target, window, request, deadline).await {
                Ok(attempt) => self.publish(target, window, attempt),
                Err(error) => Err(error),
            };
            match result {
                Ok(update) => return Ok(update),
                Err(AttemptError::Raced { stage }) => {
                    race_count = race_count.saturating_add(1);
                    last_raced_stage = Some(stage);
                    tracing::debug!(
                        race_count,
                        raced_stage = stage,
                        "retrying coherent observation inside its bounded deadline"
                    );
                    continue;
                }
                Err(AttemptError::Fatal(error)) => return Err(error),
            }
        }
    }
}

enum AttemptError {
    Raced { stage: &'static str },
    Fatal(NativeError),
}

fn classify_stamp_error(error: NativeError) -> AttemptError {
    if matches!(
        error.code,
        ErrorCode::ObservationStale
            | ErrorCode::ObservationRaced
            | ErrorCode::WindowIdentityChanged
            | ErrorCode::WindowNotFound
    ) {
        AttemptError::Raced {
            stage: "window_registry_identity_revalidation",
        }
    } else {
        AttemptError::Fatal(error)
    }
}

fn classify_capture_error(error: NativeError) -> AttemptError {
    if error.code == ErrorCode::SurfaceStale {
        let stage = if error.message.contains("source frame") {
            "capture_surface_transform"
        } else if error.message.contains("freshness metadata") {
            "capture_freshness_metadata"
        } else if error.message.contains("transiently unavailable")
            || error.message.contains("bounded deadline")
        {
            "capture_sample_timeout"
        } else {
            "capture_geometry_revalidation"
        };
        AttemptError::Raced { stage }
    } else {
        AttemptError::Fatal(error)
    }
}

struct AttemptResult {
    facts: MacWindowFacts,
    related: Vec<MacRelatedWindowFacts>,
    ax_snapshot: RawAxSnapshot,
    captures: Vec<PendingCapture>,
    epoch: u64,
}

fn same_stable_identity(left: &ResolvedWindowStamp, right: &ResolvedWindowStamp) -> bool {
    left.app_id == right.app_id
        && left.window_id == right.window_id
        && left.generation == right.generation
        && left.native_window == right.native_window
        && left.process == right.process
}

#[cfg(test)]
fn coherent_window_bracket(
    target: &ResolvedWindowStamp,
    facts_a: &MacWindowFacts,
    facts_b: &MacWindowFacts,
    epoch_a: u64,
    epoch_b: u64,
) -> bool {
    epoch_a == epoch_b && facts_a == facts_b && same_stable_identity(target, &facts_b.stamp)
}

fn resolved_from_facts(
    prior: &ResolvedWindow,
    facts: &MacWindowFacts,
) -> Result<ResolvedWindow, NativeError> {
    if !same_stable_identity(&prior.stamp(), &facts.stamp) {
        return Err(NativeError::stale(
            ErrorCode::WindowIdentityChanged,
            "refreshed macOS facts changed a stable target identity component",
        ));
    }
    let scale_factor = facts
        .scale_factor
        .ok_or_else(|| NativeError::unsupported("target window has no exact backing scale"))?;
    let mut resolved = prior.clone();
    resolved.geometry.bounds = facts.bounds;
    resolved.geometry.scale_factor = scale_factor;
    resolved.geometry.revision = facts.stamp.geometry_revision.clone();
    resolved.state = facts.state.clone();
    Ok(resolved)
}

fn validate_observable(facts: &MacWindowFacts) -> Result<(), NativeError> {
    let reason = match facts.state {
        WindowStateKind::Minimized => Some("minimized"),
        WindowStateKind::OffSpace => Some("off-space"),
        WindowStateKind::Unknown => Some("visibility is unknown"),
        WindowStateKind::Visible | WindowStateKind::Occluded => None,
    };
    if let Some(reason) = reason {
        return Err(NativeError::unsupported(format!(
            "background observation refuses a {reason} macOS window"
        ))
        .with_detail("cg_window_id", facts.cg_window_id));
    }
    if facts.minimized != Some(false) || facts.on_current_space == Some(false) {
        return Err(NativeError::unsupported(
            "background observation requires an exactly known non-minimized window on the current Space",
        ));
    }
    if !facts.is_on_screen && facts.state != WindowStateKind::Occluded {
        return Err(NativeError::unsupported(
            "WindowServer does not report an observable target surface",
        ));
    }
    let scale = facts.scale_factor.ok_or_else(|| {
        NativeError::unsupported("target window has no exact display backing scale")
    })?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(NativeError::unsupported(
            "target window has an invalid display backing scale",
        ));
    }
    Ok(())
}

struct PendingCapture {
    cg_window_id: u32,
    kind: SurfaceKind,
    bounds: Rect,
    scale_factor: f64,
    related_owner: Option<(WindowRef, ResolvedWindowStamp)>,
    transform: SurfaceToWindowTransform,
    sample: WindowFrameSample,
}

async fn capture(
    facts: &MacWindowFacts,
    kind: SurfaceKind,
    deadline: Instant,
) -> Result<PendingCapture, NativeError> {
    let window_id = facts.cg_window_id;
    let scale = facts
        .scale_factor
        .ok_or_else(|| NativeError::unsupported("window has no exact backing scale"))?;
    let sample = capture_sample_until(window_id, scale, deadline).await?;
    let transform = validated_surface_transform(&sample, facts.bounds, facts.bounds, scale)?;
    Ok(PendingCapture {
        cg_window_id: window_id,
        kind,
        bounds: facts.bounds,
        scale_factor: scale,
        related_owner: None,
        transform,
        sample,
    })
}

#[derive(Clone)]
struct RelatedSurface {
    facts: MacRelatedWindowFacts,
    kind: SurfaceKind,
    target_bounds: Rect,
}

async fn capture_related(
    surface: RelatedSurface,
    deadline: Instant,
) -> Result<PendingCapture, NativeError> {
    let window_id = surface.facts.cg_window_id;
    let scale = surface.facts.scale_factor;
    let sample = capture_sample_until(window_id, scale, deadline).await?;
    let transform =
        validated_surface_transform(&sample, surface.facts.bounds, surface.target_bounds, scale)?;
    Ok(PendingCapture {
        cg_window_id: window_id,
        kind: surface.kind,
        bounds: surface.facts.bounds,
        scale_factor: scale,
        related_owner: Some((surface.facts.public, surface.facts.stamp)),
        transform,
        sample,
    })
}

async fn capture_sample_until(
    window_id: u32,
    scale: f64,
    deadline: Instant,
) -> Result<WindowFrameSample, NativeError> {
    let deadline = tokio::time::Instant::from_std(deadline);
    let permit = tokio::time::timeout_at(deadline, SCK_CAPTURE_SLOTS.acquire())
        .await
        .map_err(|_| capture_deadline_error(window_id))?
        .map_err(|_| capture_deadline_error(window_id))?;
    let capture_timeout = deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .saturating_sub(SCK_CAPTURE_TEARDOWN_RESERVE);
    if capture_timeout.is_zero() {
        return Err(capture_deadline_error(window_id));
    }
    let capture = async move {
        let _permit = permit;
        capture_window_sample(window_id, scale, capture_timeout).await
    };
    tokio::time::timeout_at(deadline, capture)
        .await
        .map_err(|_| capture_deadline_error(window_id))?
        .map_err(capture_error)
}

fn capture_deadline_error(window_id: u32) -> NativeError {
    NativeError::stale(
        ErrorCode::SurfaceStale,
        "ScreenCaptureKit observation exceeded its bounded deadline",
    )
    .with_detail("cg_window_id", window_id)
}

async fn related_surfaces(
    registry: &MacWindowRegistry,
    facts: &MacWindowFacts,
    related: &[RawRelatedWindow],
) -> Result<Vec<RelatedSurface>, NativeError> {
    let target_bounds = facts.bounds;
    let registered = registry
        .register_related_windows(facts, related_candidates(related))
        .await?;
    let kinds: HashMap<_, _> = related
        .iter()
        .map(|related| (related.window_id, related.kind))
        .collect();
    let mut surfaces = Vec::with_capacity(registered.len());
    for related_facts in registered {
        if !related_facts.is_on_screen {
            return Err(NativeError::unsupported(format!(
                "related AX surface {} is not on-screen",
                related_facts.cg_window_id
            )));
        }
        let kind = kinds
            .get(&related_facts.cg_window_id)
            .copied()
            .ok_or_else(|| {
                NativeError::stale(
                    ErrorCode::SurfaceStale,
                    "registered related surface has no structural AX owner",
                )
            })?;
        surfaces.push(RelatedSurface {
            facts: related_facts,
            kind,
            target_bounds,
        });
    }
    Ok(surfaces)
}

fn validated_surface_transform(
    sample: &WindowFrameSample,
    source: Rect,
    target: Rect,
    expected_scale: f64,
) -> Result<SurfaceToWindowTransform, NativeError> {
    if matches!(
        sample.evidence,
        WindowFrameEvidence::ScreenshotCompletion | WindowFrameEvidence::WindowServerFallback
    ) {
        let sampled = sample.source_frame;
        let scale_x = f64::from(sample.pixel_width) / sampled.width;
        let scale_y = f64::from(sample.pixel_height) / sampled.height;
        let coherent = sample.pixel_width > 0
            && sample.pixel_height > 0
            && approximately(sampled.x, source.x)
            && approximately(sampled.y, source.y)
            && approximately(sampled.width, source.width)
            && approximately(sampled.height, source.height)
            && (scale_x - expected_scale).abs() <= 0.05
            && (scale_y - expected_scale).abs() <= 0.05;
        if !coherent {
            return Err(NativeError::stale(
                ErrorCode::SurfaceStale,
                "ScreenCaptureKit screenshot geometry disagrees with the bracketed window",
            )
            .with_detail("expected_source_bounds", rect_detail(source))
            .with_detail("sample_source_frame", frame_rect_detail(sampled))
            .with_detail("target_bounds", rect_detail(target))
            .with_detail(
                "pixel_size",
                serde_json::json!({
                    "width": sample.pixel_width,
                    "height": sample.pixel_height,
                }),
            )
            .with_detail("expected_scale_factor", expected_scale)
            .with_detail("sample_scale_x", scale_x)
            .with_detail("sample_scale_y", scale_y));
        }
        return Ok(SurfaceToWindowTransform {
            scale_x: 1.0 / scale_x,
            scale_y: 1.0 / scale_y,
            offset_x: source.x - target.x,
            offset_y: source.y - target.y,
        });
    }

    let metadata = &sample.metadata;
    let scale = metadata
        .scale_factor
        .ok_or_else(|| missing_frame_metadata(metadata))?;
    let content_scale = metadata
        .content_scale
        .ok_or_else(|| missing_frame_metadata(metadata))?;
    let content = metadata
        .content_rect
        .ok_or_else(|| missing_frame_metadata(metadata))?;
    let sampled = sample.source_frame;
    let surface_width = f64::from(sample.pixel_width) / scale;
    let surface_height = f64::from(sample.pixel_height) / scale;
    let coherent = approximately(sampled.x, source.x)
        && approximately(sampled.y, source.y)
        && approximately(sampled.width, source.width)
        && approximately(sampled.height, source.height)
        && (scale - expected_scale).abs() <= 0.05
        && content_scale.is_finite()
        && content_scale > 0.0
        && approximately(content.x, 0.0)
        && approximately(content.y, 0.0)
        && approximately(content.width, surface_width)
        && approximately(content.height, surface_height)
        && approximately(content.width / content_scale, sampled.width)
        && approximately(content.height / content_scale, sampled.height);
    if !coherent {
        return Err(NativeError::stale(
            ErrorCode::SurfaceStale,
            "ScreenCaptureKit source frame, content rectangle and raster geometry disagree",
        )
        .with_detail("expected_source_bounds", rect_detail(source))
        .with_detail("sample_source_frame", frame_rect_detail(sampled))
        .with_detail("target_bounds", rect_detail(target))
        .with_detail(
            "pixel_size",
            serde_json::json!({
                "width": sample.pixel_width,
                "height": sample.pixel_height,
            }),
        )
        .with_detail("expected_scale_factor", expected_scale)
        .with_detail("sample_scale_factor", scale)
        .with_detail("content_scale", content_scale)
        .with_detail("content_rect", frame_rect_detail(content))
        .with_detail(
            "surface_points_from_raster",
            serde_json::json!({
                "width": surface_width,
                "height": surface_height,
            }),
        ));
    }
    Ok(SurfaceToWindowTransform {
        scale_x: 1.0 / (scale * content_scale),
        scale_y: 1.0 / (scale * content_scale),
        offset_x: source.x - target.x,
        offset_y: source.y - target.y,
    })
}

fn approximately(left: f64, right: f64) -> bool {
    left.is_finite() && right.is_finite() && (left - right).abs() <= 0.75
}

fn rect_detail(rect: Rect) -> serde_json::Value {
    serde_json::json!({
        "x": rect.x,
        "y": rect.y,
        "width": rect.width,
        "height": rect.height,
    })
}

fn frame_rect_detail(rect: crate::video_sckit::FrameRect) -> serde_json::Value {
    serde_json::json!({
        "x": rect.x,
        "y": rect.y,
        "width": rect.width,
        "height": rect.height,
    })
}

#[derive(Default)]
pub struct MacFrameHistory {
    previous: HashMap<u32, PreviousFrame>,
}

struct PreviousFrame {
    png: Vec<u8>,
    completion_unix_ms: u64,
    display_time: u64,
}

impl MacFrameHistory {
    fn classify_and_commit(
        &mut self,
        window_id: u32,
        sample: &WindowFrameSample,
        expected_scale: f64,
    ) -> Result<CaptureFreshness, NativeError> {
        let metadata = &sample.metadata;
        if matches!(
            sample.evidence,
            WindowFrameEvidence::ScreenshotCompletion | WindowFrameEvidence::WindowServerFallback
        ) {
            if metadata.completion_unix_ms == 0 {
                return Err(NativeError::stale(
                    ErrorCode::SurfaceStale,
                    "ScreenCaptureKit screenshot has no completion timestamp",
                ));
            }
            let freshness = match self.previous.get(&window_id) {
                Some(previous) if metadata.completion_unix_ms <= previous.completion_unix_ms => {
                    CaptureFreshness::Frozen
                }
                _ => CaptureFreshness::FreshSnapshot,
            };
            if freshness.action_safe() {
                self.previous.insert(
                    window_id,
                    PreviousFrame {
                        png: sample.png_bytes.clone(),
                        completion_unix_ms: metadata.completion_unix_ms,
                        display_time: metadata.completion_unix_ms,
                    },
                );
            }
            return Ok(freshness);
        }

        let status = metadata
            .frame_status
            .ok_or_else(|| missing_frame_metadata(metadata))?;
        let display_time = metadata
            .display_time
            .ok_or_else(|| missing_frame_metadata(metadata))?;
        let scale = metadata
            .scale_factor
            .ok_or_else(|| missing_frame_metadata(metadata))?;
        let content_scale = metadata
            .content_scale
            .ok_or_else(|| missing_frame_metadata(metadata))?;
        let content_rect = metadata
            .content_rect
            .ok_or_else(|| missing_frame_metadata(metadata))?;
        if metadata.completion_unix_ms == 0
            || !scale.is_finite()
            || (scale - expected_scale).abs() > 0.05
            || !content_scale.is_finite()
            || content_scale <= 0.0
            || !content_rect.width.is_finite()
            || !content_rect.height.is_finite()
            || content_rect.width <= 0.0
            || content_rect.height <= 0.0
        {
            return Err(missing_frame_metadata(metadata)
                .with_detail("expected_scale_factor", expected_scale));
        }

        let freshness = if status.has_content() {
            match self.previous.get(&window_id) {
                Some(previous)
                    if metadata.completion_unix_ms > previous.completion_unix_ms
                        && display_time >= previous.display_time =>
                {
                    CaptureFreshness::Fresh
                }
                Some(_) => CaptureFreshness::Frozen,
                None => CaptureFreshness::Fresh,
            }
        } else if status == SCFrameStatus::Idle {
            match self.previous.get(&window_id) {
                Some(previous)
                    if previous.png == sample.png_bytes
                        && metadata.completion_unix_ms > previous.completion_unix_ms
                        && display_time >= previous.display_time =>
                {
                    CaptureFreshness::ReusedWithFreshCompletion
                }
                Some(_) => CaptureFreshness::Frozen,
                None => CaptureFreshness::Unavailable,
            }
        } else {
            CaptureFreshness::Unavailable
        };
        if freshness.action_safe() {
            self.previous.insert(
                window_id,
                PreviousFrame {
                    png: sample.png_bytes.clone(),
                    completion_unix_ms: metadata.completion_unix_ms,
                    display_time,
                },
            );
        }
        Ok(freshness)
    }
}

fn missing_frame_metadata(metadata: &crate::video_sckit::WindowFrameMetadata) -> NativeError {
    let mut missing_fields = Vec::new();
    if metadata.completion_unix_ms == 0 {
        missing_fields.push("completion_unix_ms");
    }
    if metadata.display_time.is_none() {
        missing_fields.push("display_time");
    }
    if metadata.frame_status.is_none() {
        missing_fields.push("frame_status");
    }
    if metadata.scale_factor.is_none() {
        missing_fields.push("scale_factor");
    }
    if metadata.content_scale.is_none() {
        missing_fields.push("content_scale");
    }
    if metadata.content_rect.is_none() {
        missing_fields.push("content_rect");
    }

    let mut invalid_fields = Vec::new();
    if metadata
        .scale_factor
        .is_some_and(|value| !value.is_finite() || value <= 0.0)
    {
        invalid_fields.push("scale_factor");
    }
    if metadata
        .content_scale
        .is_some_and(|value| !value.is_finite() || value <= 0.0)
    {
        invalid_fields.push("content_scale");
    }
    if metadata.content_rect.is_some_and(|rect| {
        !rect.x.is_finite()
            || !rect.y.is_finite()
            || !rect.width.is_finite()
            || !rect.height.is_finite()
            || rect.width <= 0.0
            || rect.height <= 0.0
    }) {
        invalid_fields.push("content_rect");
    }

    let mut error = NativeError::stale(
        ErrorCode::SurfaceStale,
        "same-sample ScreenCaptureKit freshness metadata is missing or incoherent",
    )
    .with_detail("missing_fields", serde_json::json!(missing_fields))
    .with_detail("invalid_fields", serde_json::json!(invalid_fields));
    if let Some(status) = metadata.frame_status {
        error = error.with_detail("frame_status", status.to_string());
    }
    error
}

pub struct RetainedAxElement(usize);

impl RetainedAxElement {
    unsafe fn retain(element: AXUIElementRef) -> Self {
        CFRetain(element as CFTypeRef);
        Self(element as usize)
    }

    /// Adopt a reference returned under Core Foundation's create rule.
    pub(crate) unsafe fn from_owned(element: AXUIElementRef) -> Self {
        Self(element as usize)
    }

    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        unsafe { CFEqual(self.0 as CFTypeRef, other.0 as CFTypeRef) != 0 }
    }

    pub fn as_ptr(&self) -> AXUIElementRef {
        self.0 as AXUIElementRef
    }
}

impl Clone for RetainedAxElement {
    fn clone(&self) -> Self {
        unsafe { CFRetain(self.0 as CFTypeRef) };
        Self(self.0)
    }
}

unsafe impl Send for RetainedAxElement {}
unsafe impl Sync for RetainedAxElement {}

impl Drop for RetainedAxElement {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

pub(crate) struct RetainedCfValue(usize);

impl RetainedCfValue {
    pub(crate) unsafe fn from_owned(value: CFTypeRef) -> Self {
        Self(value as usize)
    }

    pub(crate) fn same_value(&self, other: &Self) -> bool {
        unsafe { CFEqual(self.0 as CFTypeRef, other.0 as CFTypeRef) != 0 }
    }

    pub(crate) fn as_string(&self) -> Option<String> {
        let value = self.0 as CFTypeRef;
        if unsafe { CFGetTypeID(value) } != CFString::type_id() {
            return None;
        }
        Some(unsafe { CFString::wrap_under_get_rule(value as _) }.to_string())
    }
}

impl Clone for RetainedCfValue {
    fn clone(&self) -> Self {
        unsafe { CFRetain(self.0 as CFTypeRef) };
        Self(self.0)
    }
}

unsafe impl Send for RetainedCfValue {}
unsafe impl Sync for RetainedCfValue {}

impl Drop for RetainedCfValue {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

struct RawAxNode {
    element: RetainedAxElement,
    parent: Option<RetainedAxElement>,
    depth: usize,
    owner_window_id: u32,
    role: String,
    role_proven: bool,
    subrole: Option<String>,
    orientation: Option<String>,
    label: Option<String>,
    value: Option<String>,
    value_proof: Option<RetainedCfValue>,
    value_query_proven: bool,
    string_value: Option<String>,
    value_settable: Option<bool>,
    selected_text_range: Option<bindings::AxCfRange>,
    selected_text_range_settable: Option<bool>,
    selected_text_settable: Option<bool>,
    bounds: Option<Rect>,
    actions: Vec<String>,
    actions_proven: bool,
    selected: bool,
}

struct RawAxSnapshot {
    nodes: Vec<RawAxNode>,
    focused: Option<RetainedAxElement>,
    selected_text: Option<String>,
    document_text: Option<String>,
    related_windows: Vec<RawRelatedWindow>,
    truncated: bool,
}

impl RawAxSnapshot {
    fn notification_elements(&self) -> Vec<RetainedAxElement> {
        let mut elements = Vec::with_capacity(MAX_NOTIFICATION_ELEMENTS);
        let mut push_unique = |element: &RetainedAxElement| {
            if elements.len() < MAX_NOTIFICATION_ELEMENTS
                && !elements
                    .iter()
                    .any(|candidate: &RetainedAxElement| candidate.same_identity(element))
            {
                elements.push(element.clone());
            }
        };

        if let Some(focused) = &self.focused {
            push_unique(focused);
        }
        for node in &self.nodes {
            if node.selected
                || node.value_settable == Some(true)
                || node.selected_text_settable == Some(true)
                || node.selected_text_range_settable == Some(true)
            {
                push_unique(&node.element);
            }
        }
        for node in &self.nodes {
            if matches!(
                node.role.as_str(),
                "AXWindow"
                    | "AXMenu"
                    | "AXScrollArea"
                    | "AXScrollBar"
                    | "AXTextArea"
                    | "AXTextField"
            ) {
                push_unique(&node.element);
            }
        }
        for node in &self.nodes {
            push_unique(&node.element);
        }
        elements
    }
}

struct RawRelatedWindow {
    window_id: u32,
    kind: SurfaceKind,
    element: RetainedAxElement,
}

fn related_candidates(related: &[RawRelatedWindow]) -> Vec<(u32, usize)> {
    related
        .iter()
        .map(|related| (related.window_id, related.element.as_ptr() as usize))
        .collect()
}

fn same_related_identities(left: &[RawRelatedWindow], right: &[RawRelatedWindow]) -> bool {
    left.len() == right.len()
        && left.iter().all(|candidate| {
            right.iter().any(|current| {
                candidate.window_id == current.window_id
                    && candidate.kind == current.kind
                    && candidate.element.same_identity(&current.element)
            })
        })
}

fn ax_snapshot_contract_difference(
    left: &RawAxSnapshot,
    right: &RawAxSnapshot,
) -> Option<&'static str> {
    if left.truncated != right.truncated {
        return Some("ax_snapshot_truncation_changed");
    }
    if left.nodes.len() != right.nodes.len() {
        return Some("ax_snapshot_node_count_changed");
    }
    for (left, right) in left.nodes.iter().zip(&right.nodes) {
        if !left.element.same_identity(&right.element) {
            return Some("ax_snapshot_element_identity_changed");
        }
        if !same_optional_ax_identity(left.parent.as_ref(), right.parent.as_ref()) {
            return Some("ax_snapshot_parent_identity_changed");
        }
        if left.depth != right.depth || left.owner_window_id != right.owner_window_id {
            return Some("ax_snapshot_ownership_changed");
        }
        if left.role != right.role
            || left.role_proven != right.role_proven
            || left.subrole != right.subrole
            || left.orientation != right.orientation
        {
            return Some("ax_snapshot_role_changed");
        }
        if left.label != right.label {
            return Some("ax_snapshot_label_changed");
        }
        if left.value != right.value
            || left.value_query_proven != right.value_query_proven
            || left.string_value != right.string_value
            || left.value_settable != right.value_settable
        {
            return Some("ax_snapshot_value_changed");
        }
        if left.selected_text_range != right.selected_text_range
            || left.selected_text_range_settable != right.selected_text_range_settable
            || left.selected_text_settable != right.selected_text_settable
            || left.selected != right.selected
        {
            return Some("ax_snapshot_selection_changed");
        }
        if left.bounds != right.bounds {
            return Some("ax_snapshot_bounds_changed");
        }
        if left.actions != right.actions || left.actions_proven != right.actions_proven {
            return Some("ax_snapshot_actions_changed");
        }
    }
    if !same_optional_ax_identity(left.focused.as_ref(), right.focused.as_ref()) {
        return Some("ax_snapshot_focus_changed");
    }
    if left.selected_text != right.selected_text {
        return Some("ax_snapshot_selected_text_changed");
    }
    if left.document_text != right.document_text {
        return Some("ax_snapshot_document_text_changed");
    }
    None
}

fn same_optional_ax_identity(
    left: Option<&RetainedAxElement>,
    right: Option<&RetainedAxElement>,
) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.same_identity(right),
        (None, None) => true,
        _ => false,
    }
}

fn capture_ax_snapshot(
    pid: i32,
    target_window_id: u32,
    include_text: bool,
) -> Result<RawAxSnapshot, NativeError> {
    unsafe {
        let application = AXUIElementCreateApplication(pid);
        if application.is_null() {
            return Err(ax_error("AX application root is unavailable"));
        }
        let windows = copy_ax_windows(application);
        let target = windows
            .iter()
            .copied()
            .find(|window| bindings::ax_get_window_id(*window) == Some(target_window_id));
        let Some(target) = target else {
            for window in windows {
                CFRelease(window as CFTypeRef);
            }
            CFRelease(application as CFTypeRef);
            return Err(ax_error("exact AX target window is unavailable"));
        };

        let mut nodes = Vec::new();
        let mut related_windows = Vec::new();
        let mut visited = HashSet::new();
        let mut truncated = false;
        walk_ax(
            target,
            None,
            0,
            target_window_id,
            target_window_id,
            &mut nodes,
            &mut related_windows,
            &mut visited,
            &mut truncated,
        );
        replace_proxy_transient_trees(
            &windows,
            target,
            target_window_id,
            &mut nodes,
            &mut related_windows,
            &mut visited,
            &mut truncated,
        );
        if nodes.len() == 1
            && nodes[0].role == "AXWindow"
            && nodes[0].actions.is_empty()
            && nodes[0].value.is_none()
        {
            for window in windows {
                CFRelease(window as CFTypeRef);
            }
            CFRelease(application as CFTypeRef);
            return Err(ax_error(
                "target AX tree is not materialized; Plan003 observation is read-only and will not durably enable application accessibility",
            ));
        }
        let focused = focused_element_of_pid(pid)
            .map(|element| RetainedAxElement(element as usize))
            .filter(|focused| nodes.iter().any(|node| node.element.same_identity(focused)));
        let selected_text = focused
            .as_ref()
            .and_then(|element| copy_string_attr(element.as_ptr(), "AXSelectedText"));
        let document_text = include_text.then(|| {
            nodes
                .iter()
                .filter(|node| {
                    matches!(
                        node.role.as_str(),
                        "AXStaticText" | "AXTextArea" | "AXWebArea" | "AXDocument"
                    )
                })
                .filter_map(|node| node.value.as_deref())
                .collect::<Vec<_>>()
                .join("\n")
        });
        for window in windows {
            CFRelease(window as CFTypeRef);
        }
        CFRelease(application as CFTypeRef);
        Ok(RawAxSnapshot {
            nodes,
            focused,
            selected_text,
            document_text,
            related_windows,
            truncated,
        })
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn replace_proxy_transient_trees(
    windows: &[AXUIElementRef],
    target: AXUIElementRef,
    target_window_id: u32,
    nodes: &mut Vec<RawAxNode>,
    related_windows: &mut Vec<RawRelatedWindow>,
    visited: &mut HashSet<usize>,
    truncated: &mut bool,
) {
    let proxy_related: HashMap<u32, SurfaceKind> = related_windows
        .iter()
        .map(|related| (related.window_id, related.kind))
        .collect();
    if proxy_related.is_empty() {
        return;
    }

    let mut live_related_ids = HashSet::new();
    for window in windows.iter().copied().filter(|window| *window != target) {
        let Some(window_id) = bindings::ax_get_window_id(window) else {
            continue;
        };
        let Some(kind) = proxy_related.get(&window_id).copied() else {
            continue;
        };
        live_related_ids.insert(window_id);

        // AppKit can expose an inert, zero-sized menu proxy beneath the
        // originating window and the live menu as a separate AXWindow with
        // the same WindowServer id. AXPress/AXPick can return success against
        // the proxy without invoking the menu item. Replace that branch with
        // the exact live related-window tree, matching the signed helper's
        // menu-root observation posture.
        nodes.retain(|node| node.owner_window_id != window_id);
        related_windows.retain(|related| related.window_id != window_id);
        let mut nested_related = Vec::new();
        walk_ax(
            window,
            None,
            0,
            target_window_id,
            window_id,
            nodes,
            &mut nested_related,
            visited,
            truncated,
        );
        related_windows.push(RawRelatedWindow {
            window_id,
            kind,
            element: RetainedAxElement::retain(window),
        });
    }

    for (window_id, kind) in proxy_related {
        if kind == SurfaceKind::Menu && !live_related_ids.contains(&window_id) {
            // Finder can retain the inert menu proxy beneath its originating
            // window after the separate live menu AXWindow has disappeared.
            // The signed helper no longer publishes that proxy as an open
            // menu, so drop it once no live AXWindow backs the identity.
            nodes.retain(|node| node.owner_window_id != window_id);
            related_windows.retain(|related| related.window_id != window_id);
        }
    }
}

pub(crate) fn discover_native_menu(
    pid: i32,
    target_window_id: u32,
) -> Result<Option<(u32, RetainedAxElement)>, NativeError> {
    let snapshot = capture_ax_snapshot(pid, target_window_id, false)?;
    let Some(root_menu_depth) = snapshot
        .nodes
        .iter()
        .filter(|node| node.role == "AXMenu")
        .map(|node| node.depth)
        .min()
    else {
        return Ok(None);
    };
    let mut menus = snapshot
        .nodes
        .into_iter()
        .filter(|node| node.role == "AXMenu" && node.depth == root_menu_depth);
    let menu = menus
        .next()
        .expect("minimum AXMenu depth came from one menu node");
    if menus.next().is_some() {
        return Err(NativeError::stale(
            ErrorCode::MenuStateStale,
            "more than one outermost native menu element was attributable to the target action",
        ));
    }
    let window_id =
        unsafe { bindings::ax_get_window_id(menu.element.as_ptr()) }.unwrap_or(target_window_id);
    Ok(Some((window_id, menu.element)))
}

#[allow(clippy::too_many_arguments)]
unsafe fn walk_ax(
    element: AXUIElementRef,
    parent: Option<RetainedAxElement>,
    depth: usize,
    target_window_id: u32,
    inherited_owner_window_id: u32,
    nodes: &mut Vec<RawAxNode>,
    related_windows: &mut Vec<RawRelatedWindow>,
    visited: &mut HashSet<usize>,
    truncated: &mut bool,
) {
    if depth > MAX_AX_DEPTH || nodes.len() >= MAX_AX_ELEMENTS {
        *truncated = true;
        return;
    }
    if !visited.insert(element as usize) {
        return;
    }
    let (role, role_proven) = match copy_string_attr_exact(element, "AXRole") {
        Ok(Some(role)) => (role, true),
        _ => ("AXUnknown".to_owned(), false),
    };
    let subrole = copy_string_attr(element, "AXSubrole");
    let orientation = copy_string_attr(element, "AXOrientation");
    let label = copy_string_attr(element, "AXTitle")
        .filter(|value| !value.trim().is_empty())
        .or_else(|| copy_string_attr(element, "AXDescription"))
        .filter(|value| !value.trim().is_empty());
    let (value_proof, value_query_proven) = match copy_attr_value(element, "AXValue") {
        Ok(Some(value)) => (Some(RetainedCfValue::from_owned(value)), true),
        Ok(None) => (None, true),
        Err(_) => (None, false),
    };
    let string_value = value_proof.as_ref().and_then(RetainedCfValue::as_string);
    let value = string_value
        .clone()
        .or_else(|| copy_string_attr(element, "AXPlaceholderValue"));
    let value_settable = string_value
        .as_ref()
        .and_then(|_| bindings::is_attribute_settable(element, "AXValue").ok());
    let selected_text_range = string_value.as_ref().and_then(|_| {
        bindings::copy_cf_range_attr(element, "AXSelectedTextRange")
            .ok()
            .flatten()
    });
    let selected_text_range_settable = selected_text_range
        .as_ref()
        .and_then(|_| bindings::is_attribute_settable(element, "AXSelectedTextRange").ok());
    let selected_text_settable = selected_text_range
        .as_ref()
        .and_then(|_| bindings::is_attribute_settable(element, "AXSelectedText").ok());
    let bounds = element_screen_rect(element).map(|frame| Rect {
        x: frame[0],
        y: frame[1],
        width: frame[2],
        height: frame[3],
    });
    let native_window_id = bindings::ax_get_window_id(element);
    let related_kind = transient_kind(&role);
    let owner_window_id = structural_owner_window_id(
        native_window_id,
        related_kind,
        target_window_id,
        inherited_owner_window_id,
    );
    if let (Some(window_id), Some(kind)) = (native_window_id, related_kind) {
        if window_id != target_window_id {
            related_windows.push(RawRelatedWindow {
                window_id,
                kind,
                element: RetainedAxElement::retain(element),
            });
        }
    }
    let retained = RetainedAxElement::retain(element);
    let (actions, actions_proven) = match copy_action_names_exact(element) {
        Ok(actions) => (actions, true),
        Err(_) => (Vec::new(), false),
    };
    nodes.push(RawAxNode {
        element: retained.clone(),
        parent,
        depth,
        owner_window_id,
        role,
        role_proven,
        subrole,
        orientation,
        label,
        value,
        value_proof,
        value_query_proven,
        string_value,
        value_settable,
        selected_text_range,
        selected_text_range_settable,
        selected_text_settable,
        bounds,
        actions,
        actions_proven,
        selected: bindings::copy_bool_attr(element, "AXSelected").unwrap_or(false),
    });
    for child in copy_children(element) {
        walk_ax(
            child,
            Some(retained.clone()),
            depth + 1,
            target_window_id,
            owner_window_id,
            nodes,
            related_windows,
            visited,
            truncated,
        );
        CFRelease(child as CFTypeRef);
        // A depth-limited child truncates only that branch. Keep walking its
        // siblings; abort the whole traversal only when the global node
        // budget is exhausted.
        if nodes.len() >= MAX_AX_ELEMENTS {
            break;
        }
    }
}

fn transient_kind(role: &str) -> Option<SurfaceKind> {
    match role {
        "AXMenu" => Some(SurfaceKind::Menu),
        "AXPopover" => Some(SurfaceKind::Popover),
        "AXSheet" => Some(SurfaceKind::Sheet),
        _ => None,
    }
}

fn structural_owner_window_id(
    native_window_id: Option<u32>,
    transient_kind: Option<SurfaceKind>,
    target_window_id: u32,
    inherited_owner_window_id: u32,
) -> u32 {
    match (native_window_id, transient_kind) {
        (Some(window_id), Some(_)) if window_id != target_window_id => window_id,
        _ => inherited_owner_window_id,
    }
}

struct RegisteredElement {
    id: ElementId,
    native: NativeElementHandle,
    element: RetainedAxElement,
    parent: Option<RetainedAxElement>,
    owner: ResolvedWindowStamp,
    owner_window_id: u32,
    role: String,
    role_proven: bool,
    subrole: Option<String>,
    orientation: Option<String>,
    label: Option<String>,
    value_proof: Option<RetainedCfValue>,
    value_query_proven: bool,
    string_value: Option<String>,
    value_settable: Option<bool>,
    selected_text_range: Option<bindings::AxCfRange>,
    selected_text_range_settable: Option<bool>,
    selected_text_settable: Option<bool>,
    actions: Vec<String>,
    actions_proven: bool,
}

impl RegisteredElement {
    fn snapshot(&self) -> RegisteredElementSnapshot {
        RegisteredElementSnapshot {
            element: self.element.clone(),
            parent: self.parent.clone(),
            owner: self.owner.clone(),
            owner_window_id: self.owner_window_id,
            role: self.role.clone(),
            role_proven: self.role_proven,
            subrole: self.subrole.clone(),
            orientation: self.orientation.clone(),
            label: self.label.clone(),
            value_proof: self.value_proof.clone(),
            value_query_proven: self.value_query_proven,
            string_value: self.string_value.clone(),
            value_settable: self.value_settable,
            selected_text_range: self.selected_text_range,
            selected_text_range_settable: self.selected_text_range_settable,
            selected_text_settable: self.selected_text_settable,
            actions: self.actions.clone(),
            actions_proven: self.actions_proven,
        }
    }
}

#[derive(Clone)]
pub(crate) struct RegisteredElementSnapshot {
    pub element: RetainedAxElement,
    pub parent: Option<RetainedAxElement>,
    pub owner: ResolvedWindowStamp,
    pub owner_window_id: u32,
    pub role: String,
    pub role_proven: bool,
    pub subrole: Option<String>,
    pub orientation: Option<String>,
    pub label: Option<String>,
    pub value_proof: Option<RetainedCfValue>,
    pub value_query_proven: bool,
    pub string_value: Option<String>,
    pub value_settable: Option<bool>,
    pub selected_text_range: Option<bindings::AxCfRange>,
    pub selected_text_range_settable: Option<bool>,
    pub selected_text_settable: Option<bool>,
    pub actions: Vec<String>,
    pub actions_proven: bool,
}

#[derive(Default)]
pub struct MacElementRegistry {
    elements: Vec<RegisteredElement>,
}

pub struct AxPublication {
    update: NativeAccessibilityUpdate,
    truncated: bool,
}

impl MacElementRegistry {
    fn reconcile(
        &mut self,
        snapshot: RawAxSnapshot,
        window_bounds: Rect,
        window_id: u32,
        target_owner: &ResolvedWindowStamp,
        related_windows: &[MacRelatedWindowFacts],
    ) -> Result<AxPublication, NativeError> {
        let old = std::mem::take(&mut self.elements);
        let mut used = HashSet::new();
        let mut next = Vec::with_capacity(snapshot.nodes.len());
        let mut manifest = Vec::with_capacity(snapshot.nodes.len());
        let mut lines = Vec::with_capacity(snapshot.nodes.len());
        let mut focused_element = None;
        let mut selected_elements = Vec::new();

        for node in snapshot.nodes {
            let prior = old.iter().enumerate().find(|(index, candidate)| {
                !used.contains(index) && candidate.element.same_identity(&node.element)
            });
            let (id, native) = if let Some((index, prior)) = prior {
                used.insert(index);
                (prior.id.clone(), prior.native.clone())
            } else {
                let id = ElementId::new();
                let native =
                    NativeElementHandle::new(format!("macos-ax:{window_id}:{}", id.as_str()))?;
                (id, native)
            };
            if snapshot
                .focused
                .as_ref()
                .is_some_and(|focused| focused.same_identity(&node.element))
            {
                focused_element = Some(id.clone());
            }
            if node.selected {
                selected_elements.push(id.clone());
            }
            let bounds = node.bounds.map(|bounds| Rect {
                x: bounds.x - window_bounds.x,
                y: bounds.y - window_bounds.y,
                width: bounds.width,
                height: bounds.height,
            });
            let owner = if node.owner_window_id == window_id {
                target_owner.clone()
            } else {
                related_windows
                    .iter()
                    .find(|related| related.cg_window_id == node.owner_window_id)
                    .map(|related| related.stamp.clone())
                    .ok_or_else(|| {
                        NativeError::stale(
                            ErrorCode::ElementStale,
                            "AX element belongs to an unregistered native window",
                        )
                        .with_detail("cg_window_id", node.owner_window_id)
                    })?
            };
            lines.push(render_ax_line(node.depth, &id, &node));
            manifest.push(NativeAccessibilityElement {
                id: id.clone(),
                native: native.clone(),
                owner: owner.clone(),
                role: Some(node.role.clone()),
                subrole: node.subrole.clone(),
                label: node.label.clone(),
                value: node.value,
                bounds,
                actions: node.actions.clone(),
                menu_id: None,
            });
            next.push(RegisteredElement {
                id,
                native,
                element: node.element,
                parent: node.parent,
                owner,
                owner_window_id: node.owner_window_id,
                role: node.role,
                role_proven: node.role_proven,
                subrole: node.subrole,
                orientation: node.orientation,
                label: node.label,
                value_proof: node.value_proof,
                value_query_proven: node.value_query_proven,
                string_value: node.string_value,
                value_settable: node.value_settable,
                selected_text_range: node.selected_text_range,
                selected_text_range_settable: node.selected_text_range_settable,
                selected_text_settable: node.selected_text_settable,
                actions: node.actions,
                actions_proven: node.actions_proven,
            });
        }
        self.elements = next;
        Ok(AxPublication {
            update: NativeAccessibilityUpdate {
                normalized_tree: lines.join("\n"),
                elements: manifest,
                focused_element,
                selected_text: snapshot.selected_text,
                selected_elements,
                document_text: snapshot.document_text,
            },
            truncated: snapshot.truncated,
        })
    }

    pub(crate) fn registered(
        &self,
        native: &NativeElementHandle,
        id: &ElementId,
    ) -> Option<RegisteredElementSnapshot> {
        self.elements
            .iter()
            .find(|element| &element.native == native && &element.id == id)
            .map(RegisteredElement::snapshot)
    }

    pub(crate) fn registered_by_id(&self, id: &ElementId) -> Option<RegisteredElementSnapshot> {
        let mut matches = self.elements.iter().filter(|element| &element.id == id);
        let element = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(element.snapshot())
    }

    pub(crate) fn registered_snapshots(&self) -> Vec<RegisteredElementSnapshot> {
        self.elements
            .iter()
            .map(RegisteredElement::snapshot)
            .collect()
    }
}

fn render_ax_line(depth: usize, id: &ElementId, node: &RawAxNode) -> String {
    let mut line = format!(
        "{}- [{}] {}",
        "  ".repeat(depth),
        id.as_str(),
        sanitize(&node.role)
    );
    if let Some(subrole) = &node.subrole {
        line.push_str(&format!(" subrole={:?}", sanitize(subrole)));
    }
    if let Some(label) = &node.label {
        line.push_str(&format!(" label={:?}", sanitize(label)));
    }
    if let Some(value) = &node.value {
        line.push_str(&format!(" value={:?}", sanitize(value)));
    }
    if !node.actions.is_empty() {
        line.push_str(&format!(" actions={:?}", node.actions));
    }
    line
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn materialize_png(
    root: &Path,
    window_id: u32,
    bytes: &[u8],
) -> Result<(String, ObservationArtifactHandle), NativeError> {
    let mut directory = std::fs::DirBuilder::new();
    directory.recursive(true).mode(0o700);
    directory.create(root).map_err(artifact_error)?;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
        .map_err(artifact_error)?;
    let path = root.join(format!(
        "observation-{window_id}-{}.png",
        uuid::Uuid::new_v4()
    ));
    let temporary = root.join(format!(
        ".observation-{window_id}-{}.partial",
        uuid::Uuid::new_v4()
    ));
    let mut temporary_cleanup = TemporaryArtifact::new(temporary.clone());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(artifact_error)?;
    file.write_all(bytes).map_err(artifact_error)?;
    file.sync_data().map_err(artifact_error)?;
    drop(file);
    std::fs::rename(&temporary, &path).map_err(artifact_error)?;
    temporary_cleanup.disarm();
    let url = format!("file://{}", path.display());
    let cleanup_path = path.clone();
    let artifact = ObservationArtifactHandle::new(path.display().to_string(), move || {
        match std::fs::remove_file(&cleanup_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(artifact_error(error)),
        }
    });
    Ok((url, artifact))
}

struct TemporaryArtifact(Option<PathBuf>);

impl TemporaryArtifact {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TemporaryArtifact {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn capture_error(error: anyhow::Error) -> NativeError {
    let message = error.to_string();
    if message.contains("sample deadline elapsed")
        || message.contains("produced no sample before its deadline")
    {
        return NativeError::stale(
            ErrorCode::SurfaceStale,
            format!("ScreenCaptureKit sample was transiently unavailable: {message}"),
        );
    }
    NativeError::new(
        ErrorCode::UnsupportedInBackground,
        ErrorPhase::Preflight,
        true,
        format!("native window capture unavailable: {message}"),
    )
}

fn artifact_error(error: std::io::Error) -> NativeError {
    NativeError::new(
        ErrorCode::Internal,
        ErrorPhase::Verify,
        true,
        format!("observation artifact I/O failed: {error}"),
    )
}

fn ax_error(message: impl Into<String>) -> NativeError {
    NativeError::new(
        ErrorCode::UnsupportedInBackground,
        ErrorPhase::Preflight,
        true,
        message,
    )
}

fn join_error(operation: &str, error: tokio::task::JoinError) -> NativeError {
    NativeError::new(
        ErrorCode::Internal,
        ErrorPhase::Preflight,
        true,
        format!("{operation} worker failed: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_sckit::{FrameRect, WindowFrameMetadata};
    use core_foundation::{base::TCFType, string::CFString};
    use cua_driver_core::api::{
        contracts::{AppId, GeometryRevision, WindowGeneration, WindowId},
        observation::{NativeProcessHandle, NativeWindowHandle},
    };

    fn sample(status: SCFrameStatus, completion: u64, bytes: &[u8]) -> WindowFrameSample {
        WindowFrameSample {
            png_bytes: bytes.to_vec(),
            pixel_width: 200,
            pixel_height: 100,
            source_frame: FrameRect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 50.0,
            },
            evidence: WindowFrameEvidence::StreamAttachments,
            metadata: WindowFrameMetadata {
                completion_unix_ms: completion,
                display_time: Some(completion),
                frame_status: Some(status),
                scale_factor: Some(2.0),
                content_scale: Some(1.0),
                content_rect: Some(FrameRect {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 50.0,
                }),
            },
        }
    }

    #[test]
    fn static_pixels_require_fresh_completion_metadata() {
        let mut history = MacFrameHistory::default();
        assert_eq!(
            history.classify_and_commit(1, &sample(SCFrameStatus::Complete, 10, b"same"), 2.0),
            Ok(CaptureFreshness::Fresh)
        );
        assert_eq!(
            history.classify_and_commit(1, &sample(SCFrameStatus::Idle, 11, b"same"), 2.0),
            Ok(CaptureFreshness::ReusedWithFreshCompletion)
        );
        assert_eq!(
            history.classify_and_commit(1, &sample(SCFrameStatus::Idle, 11, b"same"), 2.0),
            Ok(CaptureFreshness::Frozen)
        );
    }

    #[test]
    fn incoherent_capture_geometry_is_retried_inside_the_observation() {
        let error = NativeError::stale(
            ErrorCode::SurfaceStale,
            "fixture ScreenCaptureKit geometry raced",
        );
        assert!(matches!(
            classify_capture_error(error),
            AttemptError::Raced {
                stage: "capture_geometry_revalidation"
            }
        ));
    }

    #[test]
    fn screen_capture_sample_deadline_is_retryable_surface_staleness() {
        let error = capture_error(anyhow::anyhow!(
            "ScreenCaptureKit window 42 sample deadline elapsed"
        ));
        assert_eq!(error.code, ErrorCode::SurfaceStale);
        assert_eq!(error.phase, ErrorPhase::Preflight);
        assert!(error.retryable);
    }

    #[test]
    fn related_surface_transform_maps_pixels_into_target_window_points() {
        let mut frame = sample(SCFrameStatus::Complete, 10, b"pixels");
        frame.source_frame = FrameRect {
            x: 120.0,
            y: 80.0,
            width: 100.0,
            height: 50.0,
        };
        let transform = validated_surface_transform(
            &frame,
            Rect {
                x: 120.0,
                y: 80.0,
                width: 100.0,
                height: 50.0,
            },
            Rect {
                x: 100.0,
                y: 50.0,
                width: 400.0,
                height: 300.0,
            },
            2.0,
        )
        .unwrap();
        let point =
            transform.transform(cua_driver_core::api::contracts::Point { x: 20.0, y: 10.0 });
        assert_eq!(point.x, 30.0);
        assert_eq!(point.y, 35.0);
    }

    #[test]
    fn surface_transform_refuses_unrepresented_crop_or_source_geometry() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 50.0,
        };
        let mut cropped = sample(SCFrameStatus::Complete, 10, b"pixels");
        cropped.metadata.content_rect.as_mut().unwrap().x = 2.0;
        let crop_error = validated_surface_transform(&cropped, bounds, bounds, 2.0).unwrap_err();
        assert_eq!(crop_error.code, ErrorCode::SurfaceStale);

        let mut wrong_source = sample(SCFrameStatus::Complete, 11, b"pixels");
        wrong_source.source_frame.x = 10.0;
        let source_error =
            validated_surface_transform(&wrong_source, bounds, bounds, 2.0).unwrap_err();
        assert_eq!(source_error.code, ErrorCode::SurfaceStale);
    }

    #[test]
    fn materialized_artifact_is_private_atomic_and_cleanup_owned() {
        let root = std::env::temp_dir().join(format!(
            "cua-observation-artifact-test-{}",
            uuid::Uuid::new_v4()
        ));
        let (_, artifact) = materialize_png(&root, 7, b"fixture-bytes").unwrap();
        let path = PathBuf::from(artifact.label());
        assert_eq!(std::fs::read(&path).unwrap(), b"fixture-bytes");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".partial")));
        drop(artifact);
        assert!(!path.exists());
        std::fs::remove_dir(&root).unwrap();
    }

    fn raw_snapshot(element: &CFString, label: &str) -> RawAxSnapshot {
        let pointer = element.as_CFTypeRef().cast_mut().cast();
        RawAxSnapshot {
            nodes: vec![RawAxNode {
                element: unsafe { RetainedAxElement::retain(pointer) },
                parent: None,
                depth: 0,
                owner_window_id: 1,
                role: "AXButton".to_owned(),
                role_proven: true,
                subrole: None,
                orientation: None,
                label: Some(label.to_owned()),
                value: None,
                value_proof: None,
                value_query_proven: true,
                string_value: None,
                value_settable: Some(false),
                selected_text_range: None,
                selected_text_range_settable: Some(false),
                selected_text_settable: Some(false),
                bounds: None,
                actions: vec!["AXPress".to_owned()],
                actions_proven: true,
                selected: false,
            }],
            focused: None,
            selected_text: None,
            document_text: None,
            related_windows: Vec::new(),
            truncated: false,
        }
    }

    #[test]
    fn rendered_tree_exposes_exact_subrole_for_safe_model_targeting() {
        let native = CFString::new("close-button");
        let mut snapshot = raw_snapshot(&native, "");
        snapshot.nodes[0].label = None;
        snapshot.nodes[0].subrole = Some("AXCloseButton".to_owned());

        let line = render_ax_line(
            0,
            &ElementId::parse("close-button").unwrap(),
            &snapshot.nodes[0],
        );

        assert_eq!(
            line,
            r#"- [close-button] AXButton subrole="AXCloseButton" actions=["AXPress"]"#
        );
    }

    #[test]
    fn stable_ids_use_native_cf_identity_not_label_or_index() {
        let first_native = CFString::new("native-identity-one");
        let replacement_native = CFString::new("native-identity-two");
        let mut registry = MacElementRegistry::default();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let owner = stamp("geometry", "native");
        let first = registry
            .reconcile(
                raw_snapshot(&first_native, "Same label"),
                bounds,
                1,
                &owner,
                &[],
            )
            .unwrap();
        let stable = registry
            .reconcile(
                raw_snapshot(&first_native, "Renamed"),
                bounds,
                1,
                &owner,
                &[],
            )
            .unwrap();
        let replacement = registry
            .reconcile(
                raw_snapshot(&replacement_native, "Same label"),
                bounds,
                1,
                &owner,
                &[],
            )
            .unwrap();

        assert_eq!(first.update.elements[0].id, stable.update.elements[0].id);
        assert_ne!(
            stable.update.elements[0].id,
            replacement.update.elements[0].id
        );
    }

    #[test]
    fn notification_watch_set_is_bounded_and_keeps_high_signal_elements() {
        let natives: Vec<_> = (0..40)
            .map(|index| CFString::new(&format!("native-{index}")))
            .collect();
        let mut snapshot = raw_snapshot(&natives[0], "0");
        snapshot.nodes.clear();
        for (index, native) in natives.iter().enumerate() {
            snapshot
                .nodes
                .push(raw_snapshot(native, &index.to_string()).nodes.remove(0));
        }
        snapshot.focused = Some(snapshot.nodes[35].element.clone());
        snapshot.nodes[35].selected = true;
        snapshot.nodes[36].selected = true;
        snapshot.nodes[37].value_settable = Some(true);
        snapshot.nodes[38].role = "AXWindow".to_owned();

        let watched = snapshot.notification_elements();

        assert_eq!(watched.len(), MAX_NOTIFICATION_ELEMENTS);
        for expected in [35, 36, 37, 38] {
            assert!(watched
                .iter()
                .any(|element| element.same_identity(&snapshot.nodes[expected].element)));
        }
        assert_eq!(
            watched
                .iter()
                .filter(|element| element.same_identity(&snapshot.nodes[35].element))
                .count(),
            1
        );
    }

    fn stamp(geometry: &str, native: &str) -> ResolvedWindowStamp {
        ResolvedWindowStamp {
            app_id: AppId::parse("app").unwrap(),
            window_id: WindowId::parse("window").unwrap(),
            generation: WindowGeneration(7),
            geometry_revision: GeometryRevision::parse(geometry).unwrap(),
            native_window: NativeWindowHandle::new(native).unwrap(),
            process: NativeProcessHandle::new("process").unwrap(),
        }
    }

    fn fake_facts(geometry: &str, x: f64) -> MacWindowFacts {
        MacWindowFacts {
            stamp: stamp(geometry, "native"),
            pid: 42,
            process_generation: 9,
            cg_window_id: 7,
            owner_name: "Fixture".to_owned(),
            layer: 0,
            bounds: Rect {
                x,
                y: 0.0,
                width: 100.0,
                height: 50.0,
            },
            activation_point: Some(cua_driver_core::api::contracts::Point {
                x: x + 10.0,
                y: 16.0,
            }),
            scale_factor: Some(2.0),
            state: WindowStateKind::Visible,
            is_on_screen: true,
            on_current_space: Some(true),
            space_ids: Some(vec![1]),
            minimized: Some(false),
        }
    }

    #[test]
    fn fake_stamp_or_epoch_race_invalidates_the_observation_bracket() {
        let target = stamp("geometry-a", "native");
        let facts_a = fake_facts("geometry-a", 0.0);
        assert!(coherent_window_bracket(&target, &facts_a, &facts_a, 4, 4));
        assert!(!coherent_window_bracket(
            &target,
            &facts_a,
            &fake_facts("geometry-b", 1.0),
            4,
            4
        ));
        assert!(!coherent_window_bracket(&target, &facts_a, &facts_a, 4, 5));
    }

    #[test]
    fn refreshed_geometry_preserves_stable_identity_but_native_reuse_does_not() {
        assert!(same_stable_identity(
            &stamp("geometry-a", "native"),
            &stamp("geometry-b", "native")
        ));
        assert!(!same_stable_identity(
            &stamp("geometry-a", "native"),
            &stamp("geometry-b", "replacement")
        ));
    }

    #[test]
    fn missing_same_sample_metadata_is_not_action_safe() {
        let mut history = MacFrameHistory::default();
        let mut incomplete = sample(SCFrameStatus::Complete, 10, b"pixels");
        incomplete.metadata.display_time = None;
        let error = history
            .classify_and_commit(1, &incomplete, 2.0)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::SurfaceStale);
        assert_eq!(
            error.details.get("missing_fields"),
            Some(&serde_json::json!(["display_time"]))
        );
        assert_eq!(
            error.details.get("invalid_fields"),
            Some(&serde_json::json!([]))
        );
    }

    #[test]
    fn embedded_remote_ax_children_inherit_target_ownership_but_transients_do_not() {
        assert_eq!(structural_owner_window_id(Some(99), None, 1, 1), 1);
        assert_eq!(
            structural_owner_window_id(Some(99), Some(SurfaceKind::Popover), 1, 1),
            99
        );
    }
}
