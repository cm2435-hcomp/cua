//! Bounded native protocol fixture for the Python v2 transport contract test.
//!
//! This binary does not dispatch platform work. It accepts the canonical v2
//! request types and emits canonical API results through the canonical v2
//! response envelope, then records that stdin reached EOF.

use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, BufRead, Write},
    path::PathBuf,
};

use cua_driver_core::{
    api::{
        AccessibilityElement, AccessibilityState, ActionId, ActionReceipt, AppId, AppRef,
        AxRevision, AxTreeUpdate, CapturedSurface, ElementId, ElementRef, ErrorCode, ErrorPhase,
        MenuRevision, MenuState, NativeError, NativeEvidence, ObservationId, PartialEvidence,
        PartialNativeDispatch, PendingSettlementEvidence, PendingSettlementState, Rect,
        ReplaceAxLines, Route, SettledState, SettlementEvidence, SettlementSignal, Size, SurfaceId,
        SurfaceKind, VerificationLevel, WindowId, WindowRef, WindowState,
    },
    protocol::{
        V2Command, V2Failure, V2HandshakeRequest, V2HandshakeResponse, V2RequestEnvelope,
        V2ResponseBody, V2ResponseEnvelope, V2Success, V2_METHODS, V2_PROTOCOL_VERSION,
    },
};
use serde::Serialize;

const MAX_FIXTURE_LINE_BYTES: usize = 64 * 1024;
const MAX_FIXTURE_REQUESTS: usize = 4;

fn main() -> anyhow::Result<()> {
    let eof_marker = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("expected EOF marker path"))?;
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let handshake_line = lines
        .next()
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("expected handshake"))?;
    anyhow::ensure!(
        handshake_line.len() <= MAX_FIXTURE_LINE_BYTES,
        "handshake exceeded fixture line bound"
    );
    let handshake: V2HandshakeRequest = serde_json::from_str(&handshake_line)?;
    write_line(&V2HandshakeResponse {
        request_id: handshake.request_id,
        minimum_version: V2_PROTOCOL_VERSION,
        maximum_version: V2_PROTOCOL_VERSION,
        driver_name: "cua-driver-v2-protocol-fixture".to_owned(),
        driver_version: env!("CARGO_PKG_VERSION").to_owned(),
        build: "test-only".to_owned(),
        methods: V2_METHODS
            .iter()
            .map(|method| (*method).to_owned())
            .collect(),
    })?;

    let mut observation_count = 0_u8;
    let mut request_count = 0_usize;
    for line in lines {
        request_count += 1;
        anyhow::ensure!(
            request_count <= MAX_FIXTURE_REQUESTS,
            "request count exceeded fixture bound"
        );
        let line = line?;
        anyhow::ensure!(
            line.len() <= MAX_FIXTURE_LINE_BYTES,
            "request exceeded fixture line bound"
        );
        let request: V2RequestEnvelope = serde_json::from_str(&line)?;
        request
            .validate_version()
            .map_err(|error| anyhow::anyhow!(error))?;
        match request.command {
            V2Command::GetWindowState(_) => {
                observation_count += 1;
                let state = fixture_state(observation_count)?;
                write_result(request.request_id, state)?;
            }
            V2Command::Click(_) => {
                write_result(request.request_id, fixture_receipt()?)?;
            }
            V2Command::TypeText(_) => {
                write_error(request.request_id, fixture_partial_error()?)?;
            }
            command => {
                let error = NativeError::invalid(format!(
                    "protocol fixture does not implement {}",
                    command.method()
                ));
                write_error(request.request_id, error)?;
            }
        }
    }

    fs::write(eof_marker, b"eof\n")?;
    Ok(())
}

fn fixture_state(sequence: u8) -> anyhow::Result<WindowState> {
    let observation_id = parse_id(ObservationId::parse(format!("observation-{sequence}")))?;
    let revision = parse_id(AxRevision::parse(format!("revision-{sequence}")))?;
    let tree_update = match sequence {
        1 => AxTreeUpdate::Full {
            revision,
            tree: "root\nold".to_owned(),
        },
        2 => AxTreeUpdate::Diff {
            base_revision: parse_id(AxRevision::parse("revision-1"))?,
            revision,
            operations: vec![ReplaceAxLines {
                start_line: 1,
                delete_count: 1,
                lines: vec!["new".to_owned()],
            }],
        },
        _ => anyhow::bail!("fixture serves exactly two observations"),
    };
    let window = fixture_window()?;
    let surface_id = parse_id(SurfaceId::parse("surface-1"))?;
    let element_ref = ElementRef {
        observation_id: observation_id.clone(),
        id: parse_id(ElementId::parse("element-1"))?,
    };
    Ok(WindowState {
        observation_id,
        window: window.clone(),
        surfaces: vec![CapturedSurface {
            id: surface_id,
            owner_window: window,
            kind: SurfaceKind::Window,
            image_url: "file:///opaque/native-protocol-fixture.png".to_owned(),
            size: Size {
                width: 640,
                height: 480,
            },
            window_bounds: Some(Rect {
                x: 10.0,
                y: 20.0,
                width: 640.0,
                height: 480.0,
            }),
        }],
        accessibility: Some(AccessibilityState {
            tree_update,
            elements: vec![AccessibilityElement {
                element_ref: element_ref.clone(),
                role: Some("AXButton".to_owned()),
                label: Some("Fixture".to_owned()),
                value: None,
                bounds: None,
                actions: vec!["AXPress".to_owned()],
            }],
            focused_element: Some(element_ref),
            selected_text: None,
            selected_elements: Vec::new(),
            document_text: None,
        }),
        menu: MenuState::Closed {
            revision: parse_id(MenuRevision::parse(format!("menu-revision-{sequence}")))?,
        },
        settlement: SettlementEvidence::initial(),
        captured_at_unix_ms: 1_753_200_000_000 + u64::from(sequence),
        warnings: Vec::new(),
    })
}

fn fixture_receipt() -> anyhow::Result<ActionReceipt> {
    let action_id = parse_id(ActionId::parse("action-click-1"))?;
    let mut fields = BTreeMap::new();
    fields.insert("fixture".to_owned(), serde_json::json!("rust-serde"));
    Ok(ActionReceipt {
        action_id: action_id.clone(),
        window: fixture_window()?,
        consumed_observation_id: parse_id(ObservationId::parse("observation-2"))?,
        route: Route::TargetedPointer,
        verification: VerificationLevel::DispatchVerified,
        settlement: settled_after(action_id),
        native_evidence: NativeEvidence {
            fields,
            interaction_scope: None,
        },
        warnings: Vec::new(),
    })
}

fn fixture_partial_error() -> anyhow::Result<NativeError> {
    let action_id = parse_id(ActionId::parse("action-type-1"))?;
    let observation_id = parse_id(ObservationId::parse("observation-2"))?;
    let pending = PendingSettlementEvidence {
        state: PendingSettlementState::Pending,
        trigger_action_id: action_id.clone(),
        profile: "fixture-keyboard".to_owned(),
        elapsed_ms: 3,
        observed_signals: vec![SettlementSignal::DispatchStarted],
        missing_signals: vec![SettlementSignal::DispatchComplete],
    };
    let mut fields = BTreeMap::new();
    fields.insert("delivered_events".to_owned(), serde_json::json!(1));
    let evidence = NativeEvidence {
        fields,
        interaction_scope: None,
    };
    let mut error = NativeError::new(
        ErrorCode::DispatchFailed,
        ErrorPhase::Dispatch,
        false,
        "fixture keyboard sequence stopped after its first targeted event",
    )
    .with_detail("delivered_events", 1);
    error.partial_evidence = Some(Box::new(PartialEvidence::Action {
        action_id,
        window: fixture_window()?,
        consumed_observation_id: observation_id,
        route: Route::TargetedKeyboard,
        dispatch: Some(PartialNativeDispatch {
            verification: VerificationLevel::DispatchUnverified,
            native_evidence: evidence.clone(),
            warnings: vec!["fixture forced a typed partial dispatch".to_owned()],
        }),
        native_evidence: evidence,
        pending_settlement: Some(Box::new(pending.clone())),
    }));
    error.pending_settlement = Some(Box::new(pending));
    Ok(error)
}

fn fixture_window() -> anyhow::Result<WindowRef> {
    Ok(WindowRef {
        id: parse_id(WindowId::parse("window-1"))?,
        app: AppRef {
            id: parse_id(AppId::parse("app-1"))?,
            canonical_id: None,
            name: Some("Fixture".to_owned()),
            pid: Some(42),
            running: true,
        },
        title: Some("Fixture window".to_owned()),
    })
}

fn parse_id<T>(result: Result<T, String>) -> anyhow::Result<T> {
    result.map_err(anyhow::Error::msg)
}

fn settled_after(action_id: ActionId) -> SettlementEvidence {
    SettlementEvidence {
        state: SettledState::Settled,
        trigger_action_id: Some(action_id),
        profile: "fixture-pointer".to_owned(),
        elapsed_ms: 2,
        observed_signals: vec![SettlementSignal::DispatchComplete],
        terminal_signal: "dispatch_complete".to_owned(),
        quiet_window_ms: 1,
        resumed_from_prior_call: false,
    }
}

fn write_result<T: Serialize>(request_id: String, result: T) -> anyhow::Result<()> {
    write_line(&V2ResponseEnvelope {
        request_id,
        protocol_version: V2_PROTOCOL_VERSION,
        body: V2ResponseBody::Result(V2Success { result }),
    })
}

fn write_error(request_id: String, error: NativeError) -> anyhow::Result<()> {
    write_line(&V2ResponseEnvelope::<serde_json::Value> {
        request_id,
        protocol_version: V2_PROTOCOL_VERSION,
        body: V2ResponseBody::Error(V2Failure { error }),
    })
}

fn write_line(value: &impl Serialize) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}
