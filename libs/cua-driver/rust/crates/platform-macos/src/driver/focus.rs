//! Pure macOS interaction recipes and target-only SLPS make-key leases.

use std::sync::{Arc, Mutex};

use cua_driver_core::api::{
    capabilities::Framework,
    contracts::{ActionId, MouseButton, Route},
    errors::{ErrorCode, ErrorPhase, NativeError},
    interaction::ScopeRequirements,
    platform::ResolvedAction,
};

use crate::input::slps_make_key::{self, SlpsMakeKeyState};

use super::target::{MacActiveBelief, MacFocusState};

const PROVEN_OS_VERSION: &str = "26.5.1";
const PROVEN_ARCHITECTURE: &str = "arm64";
// The live F1b recipe posted KeyFocusReturned followed by NewFront. Route B's
// recovered SLPS record has no subtype field, so those two grants are the same
// make-key byte shape but must still be posted twice, in order.
const PROVEN_CHROMIUM_GRANT_RECORDS: [SlpsMakeKeyState; 2] =
    [SlpsMakeKeyState::MakeKey, SlpsMakeKeyState::MakeKey];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRecipeContext {
    pub os_version: String,
    pub architecture: String,
}

impl HostRecipeContext {
    pub fn current() -> Self {
        let os_version = std::process::Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".to_owned());
        let architecture = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            value => value,
        }
        .to_owned();
        Self {
            os_version,
            architecture,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessibilityRecipe {
    NotApplicable,
    ChromiumPriorStatePreserving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuSuppressionRecipe {
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetBeliefRecipe {
    NotApplicable,
    ChromiumPointClickSlpsMakeKeyHost26_5_1Arm64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacScopeRecipe {
    pub accessibility: AccessibilityRecipe,
    pub menu: MenuSuppressionRecipe,
    pub target_belief: TargetBeliefRecipe,
}

impl MacScopeRecipe {
    pub fn evidence_name(self) -> &'static str {
        match self.target_belief {
            TargetBeliefRecipe::NotApplicable => "semantic_no_target_belief",
            TargetBeliefRecipe::ChromiumPointClickSlpsMakeKeyHost26_5_1Arm64 => {
                "chromium_point_click_two_slps_make_key_records_macos_26_5_1_arm64"
            }
        }
    }
}

pub fn select_scope_recipe(
    host: &HostRecipeContext,
    framework: &Framework,
    route: Route,
    action: &ResolvedAction,
    requirements: &ScopeRequirements,
) -> Result<MacScopeRecipe, NativeError> {
    if requirements.menu_dismissal {
        return Err(recipe_unproven(
            host,
            framework,
            route,
            action,
            "exact per-pid and per-menu event-tap predicate is not proved",
        ));
    }

    let accessibility = if requirements.accessibility && *framework == Framework::Chromium {
        AccessibilityRecipe::ChromiumPriorStatePreserving
    } else {
        AccessibilityRecipe::NotApplicable
    };

    let target_belief = match route {
        Route::Semantic if !requirements.target_belief => TargetBeliefRecipe::NotApplicable,
        Route::TargetedPointer
            if requirements.target_belief
                && host.os_version == PROVEN_OS_VERSION
                && host.architecture == PROVEN_ARCHITECTURE
                && *framework == Framework::Chromium
                && is_exact_proven_point_click(action) =>
        {
            TargetBeliefRecipe::ChromiumPointClickSlpsMakeKeyHost26_5_1Arm64
        }
        _ => {
            return Err(recipe_unproven(
                host,
                framework,
                route,
                action,
                "no exact target-belief recipe is proved for this host/framework/action cell",
            ));
        }
    };

    Ok(MacScopeRecipe {
        accessibility,
        menu: MenuSuppressionRecipe::NotApplicable,
        target_belief,
    })
}

fn is_exact_proven_point_click(action: &ResolvedAction) -> bool {
    matches!(
        action,
        ResolvedAction::PointClick { spec, .. }
            if spec.button == MouseButton::Left
                && spec.click_count == 1
                && spec.modifiers.is_empty()
    )
}

fn action_shape(action: &ResolvedAction) -> &'static str {
    match action {
        ResolvedAction::ElementClick { .. } => "element_click",
        ResolvedAction::PointClick { .. } => "point_click",
        ResolvedAction::Drag(_) => "drag",
        ResolvedAction::ElementScroll { .. } => "element_scroll",
        ResolvedAction::DeltaScroll(_) => "delta_scroll",
        ResolvedAction::PressKey { .. } => "press_key",
        ResolvedAction::TypeText { .. } => "type_text",
        ResolvedAction::SetValue { .. } => "set_value",
        ResolvedAction::SelectText { .. } => "select_text",
        ResolvedAction::Secondary { .. } => "secondary",
    }
}

fn recipe_unproven(
    host: &HostRecipeContext,
    framework: &Framework,
    route: Route,
    action: &ResolvedAction,
    reason: &'static str,
) -> NativeError {
    NativeError::unsupported(format!("recipe_unproven: {reason}"))
        .with_detail("recipe_status", "recipe_unproven")
        .with_detail("os_version", host.os_version.clone())
        .with_detail("architecture", host.architecture.clone())
        .with_detail("framework", format!("{framework:?}"))
        .with_detail("route", format!("{route:?}"))
        .with_detail("action", action_shape(action))
}

pub(crate) trait TargetBeliefPoster: Send + Sync {
    fn post(&self, pid: i32, window_id: u32, state: SlpsMakeKeyState) -> Result<(), NativeError>;
}

#[derive(Default)]
pub(crate) struct SystemTargetBeliefPoster;

impl TargetBeliefPoster for SystemTargetBeliefPoster {
    fn post(&self, pid: i32, window_id: u32, state: SlpsMakeKeyState) -> Result<(), NativeError> {
        slps_make_key::post_target_only(pid, window_id, &[state]).map_err(|error| {
            NativeError::new(
                ErrorCode::UnsupportedInBackground,
                ErrorPhase::Preflight,
                false,
                format!("target-only SLPS make-key record failed: {error}"),
            )
            .with_detail("pid", pid)
            .with_detail("cg_window_id", window_id)
            .with_detail("slps_record_state", state.evidence_name())
        })
    }
}

pub(crate) struct TargetBeliefLease {
    action_id: ActionId,
    pid: i32,
    window_id: u32,
    state: Arc<Mutex<MacFocusState>>,
    poster: Arc<dyn TargetBeliefPoster>,
    released: bool,
}

impl TargetBeliefLease {
    pub fn acquire(
        action_id: ActionId,
        pid: i32,
        window_id: u32,
        state: Arc<Mutex<MacFocusState>>,
    ) -> Result<Self, NativeError> {
        Self::acquire_with(
            action_id,
            pid,
            window_id,
            state,
            Arc::new(SystemTargetBeliefPoster),
        )
    }

    fn acquire_with(
        action_id: ActionId,
        pid: i32,
        window_id: u32,
        state: Arc<Mutex<MacFocusState>>,
        poster: Arc<dyn TargetBeliefPoster>,
    ) -> Result<Self, NativeError> {
        {
            let mut state_guard = state.lock().expect("macOS focus coordinator poisoned");
            if state_guard.shutdown {
                return Err(NativeError::new(
                    ErrorCode::WindowIdentityChanged,
                    ErrorPhase::Preflight,
                    false,
                    "target focus coordinator is shut down",
                ));
            }
            if let Some(active) = &state_guard.active_belief {
                return Err(NativeError::new(
                    ErrorCode::TargetBusy,
                    ErrorPhase::Preflight,
                    true,
                    "target already owns an active belief lease",
                )
                .with_detail("active_action_id", active.action_id.to_string()));
            }
            state_guard.active_belief = Some(MacActiveBelief {
                action_id: action_id.clone(),
                pid,
                cg_window_id: window_id,
            });
        }

        let mut grants_posted = 0usize;
        for grant in PROVEN_CHROMIUM_GRANT_RECORDS {
            if let Err(error) = poster.post(pid, window_id, grant) {
                let mut error = error
                    .with_detail("grant_records_posted", grants_posted)
                    .with_detail(
                        "grant_records_expected",
                        PROVEN_CHROMIUM_GRANT_RECORDS.len(),
                    );
                let mut rollback_complete = true;
                if grants_posted > 0 {
                    if let Err(cleanup) = poster.post(pid, window_id, SlpsMakeKeyState::RemoveKey) {
                        rollback_complete = false;
                        error = error.with_related(&cleanup);
                    }
                }
                if rollback_complete {
                    state
                        .lock()
                        .expect("macOS focus coordinator poisoned")
                        .active_belief = None;
                }
                return Err(error);
            }
            grants_posted += 1;
        }

        Ok(Self {
            action_id,
            pid,
            window_id,
            state,
            poster,
            released: false,
        })
    }

    pub fn release(&mut self) -> Result<(), NativeError> {
        if self.released {
            return Ok(());
        }
        self.poster
            .post(self.pid, self.window_id, SlpsMakeKeyState::RemoveKey)
            .map_err(|error| {
                NativeError::new(
                    error.code,
                    ErrorPhase::Verify,
                    error.retryable,
                    error.message,
                )
                .with_detail("action_id", self.action_id.to_string())
                .with_detail("pid", self.pid)
                .with_detail("cg_window_id", self.window_id)
            })?;
        self.state
            .lock()
            .expect("macOS focus coordinator poisoned")
            .active_belief = None;
        self.released = true;
        Ok(())
    }
}

impl Drop for TargetBeliefLease {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            tracing::error!(error = %error, "failed to post target-only SLPS remove-key record");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use cua_driver_core::api::{
        contracts::{CaptureRevision, GeometryRevision, Point, Rect, SurfaceId, WindowGeneration},
        observation::{
            NativeProcessHandle, NativeWindowHandle, ResolvedPoint, ResolvedWindow, SurfaceOwner,
            WindowGeometry,
        },
        platform::ClickSpec,
    };

    use super::*;

    struct FakePoster {
        signals: Mutex<Vec<SlpsMakeKeyState>>,
        fail_at: Vec<usize>,
    }

    impl TargetBeliefPoster for FakePoster {
        fn post(
            &self,
            _pid: i32,
            _window_id: u32,
            signal: SlpsMakeKeyState,
        ) -> Result<(), NativeError> {
            let mut signals = self.signals.lock().unwrap();
            let index = signals.len();
            signals.push(signal);
            if self.fail_at.contains(&index) {
                Err(NativeError::new(
                    ErrorCode::Internal,
                    ErrorPhase::Preflight,
                    false,
                    "injected SLPS failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn point_click(button: MouseButton, count: u8) -> ResolvedAction {
        let app = cua_driver_core::api::contracts::AppRef {
            id: cua_driver_core::api::contracts::AppId::parse("app").unwrap(),
            name: None,
            pid: Some(1),
            running: true,
        };
        let public = cua_driver_core::api::contracts::WindowRef {
            id: cua_driver_core::api::contracts::WindowId::parse("window").unwrap(),
            app,
            title: None,
        };
        let window = ResolvedWindow {
            public,
            native: NativeWindowHandle::new("native").unwrap(),
            process: NativeProcessHandle::new("process").unwrap(),
            framework: Framework::Chromium,
            geometry: WindowGeometry {
                bounds: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 10.0,
                    height: 10.0,
                },
                scale_factor: 1.0,
                revision: GeometryRevision::parse("geometry").unwrap(),
            },
            generation: WindowGeneration(1),
            state: cua_driver_core::api::capabilities::WindowStateKind::Visible,
        };
        ResolvedAction::PointClick {
            point: ResolvedPoint {
                window: window.clone(),
                surface_id: SurfaceId::parse("surface").unwrap(),
                surface_owner: SurfaceOwner::Target(window.stamp()),
                capture_revision: CaptureRevision::parse("capture").unwrap(),
                observation_epoch: Some(cua_driver_core::api::observation::NativeObservationEpoch(
                    0,
                )),
                surface_point: Point { x: 1.0, y: 1.0 },
                window_point: Point { x: 1.0, y: 1.0 },
                screen_point: Point { x: 1.0, y: 1.0 },
                geometry_revision: GeometryRevision::parse("geometry").unwrap(),
            },
            spec: ClickSpec {
                button,
                click_count: count,
                modifiers: Vec::new(),
            },
        }
    }

    #[test]
    fn recipe_selector_accepts_only_the_proven_pointer_cell() {
        let host = HostRecipeContext {
            os_version: PROVEN_OS_VERSION.to_owned(),
            architecture: PROVEN_ARCHITECTURE.to_owned(),
        };
        let requirements = ScopeRequirements::for_route(Route::TargetedPointer);
        let recipe = select_scope_recipe(
            &host,
            &Framework::Chromium,
            Route::TargetedPointer,
            &point_click(MouseButton::Left, 1),
            &requirements,
        )
        .unwrap();
        assert_eq!(
            recipe.target_belief,
            TargetBeliefRecipe::ChromiumPointClickSlpsMakeKeyHost26_5_1Arm64
        );

        for (host, framework, action) in [
            (
                HostRecipeContext {
                    os_version: "26.5.2".to_owned(),
                    architecture: "arm64".to_owned(),
                },
                Framework::Chromium,
                point_click(MouseButton::Left, 1),
            ),
            (
                HostRecipeContext {
                    os_version: PROVEN_OS_VERSION.to_owned(),
                    architecture: "x86_64".to_owned(),
                },
                Framework::Chromium,
                point_click(MouseButton::Left, 1),
            ),
            (
                host.clone(),
                Framework::Electron,
                point_click(MouseButton::Left, 1),
            ),
            (
                host.clone(),
                Framework::Chromium,
                point_click(MouseButton::Right, 1),
            ),
        ] {
            let error = select_scope_recipe(
                &host,
                &framework,
                Route::TargetedPointer,
                &action,
                &requirements,
            )
            .unwrap_err();
            assert_eq!(error.code, ErrorCode::UnsupportedInBackground);
            assert_eq!(error.details["recipe_status"], "recipe_unproven");
        }
    }

    #[test]
    fn target_belief_grant_and_revoke_are_target_only_and_stateful() {
        let state = Arc::new(Mutex::new(MacFocusState::default()));
        let poster = Arc::new(FakePoster {
            signals: Mutex::new(Vec::new()),
            fail_at: Vec::new(),
        });
        let mut lease = TargetBeliefLease::acquire_with(
            ActionId::parse("action").unwrap(),
            44,
            99,
            Arc::clone(&state),
            poster.clone(),
        )
        .unwrap();
        assert_eq!(
            poster.signals.lock().unwrap().as_slice(),
            [SlpsMakeKeyState::MakeKey, SlpsMakeKeyState::MakeKey]
        );
        assert!(state.lock().unwrap().active_belief.is_some());
        lease.release().unwrap();
        assert_eq!(
            poster.signals.lock().unwrap().as_slice(),
            [
                SlpsMakeKeyState::MakeKey,
                SlpsMakeKeyState::MakeKey,
                SlpsMakeKeyState::RemoveKey
            ]
        );
        assert!(state.lock().unwrap().active_belief.is_none());
    }

    #[test]
    fn failed_make_key_record_clears_controller_ownership_without_claiming_a_grant() {
        let state = Arc::new(Mutex::new(MacFocusState::default()));
        let poster = Arc::new(FakePoster {
            signals: Mutex::new(Vec::new()),
            fail_at: vec![0],
        });
        let error = TargetBeliefLease::acquire_with(
            ActionId::parse("action").unwrap(),
            44,
            99,
            Arc::clone(&state),
            poster.clone(),
        )
        .err()
        .unwrap();
        assert_eq!(error.phase, ErrorPhase::Preflight);
        assert_eq!(
            poster.signals.lock().unwrap().as_slice(),
            [SlpsMakeKeyState::MakeKey]
        );
        assert!(state.lock().unwrap().active_belief.is_none());
    }

    #[test]
    fn second_grant_failure_posts_paired_revoke_and_clears_ownership() {
        let state = Arc::new(Mutex::new(MacFocusState::default()));
        let poster = Arc::new(FakePoster {
            signals: Mutex::new(Vec::new()),
            fail_at: vec![1],
        });
        let error = TargetBeliefLease::acquire_with(
            ActionId::parse("action").unwrap(),
            44,
            99,
            Arc::clone(&state),
            poster.clone(),
        )
        .err()
        .expect("second grant failure must refuse the lease");
        assert_eq!(error.phase, ErrorPhase::Preflight);
        assert_eq!(error.details["grant_records_posted"], 1);
        assert_eq!(
            poster.signals.lock().unwrap().as_slice(),
            [
                SlpsMakeKeyState::MakeKey,
                SlpsMakeKeyState::MakeKey,
                SlpsMakeKeyState::RemoveKey
            ]
        );
        assert!(state.lock().unwrap().active_belief.is_none());
    }

    #[test]
    fn failed_partial_grant_rollback_keeps_ownership_for_poison_and_shutdown_retry() {
        let state = Arc::new(Mutex::new(MacFocusState::default()));
        let poster = Arc::new(FakePoster {
            signals: Mutex::new(Vec::new()),
            fail_at: vec![1, 2],
        });
        let error = TargetBeliefLease::acquire_with(
            ActionId::parse("action").unwrap(),
            44,
            99,
            Arc::clone(&state),
            poster.clone(),
        )
        .err()
        .expect("grant and rollback failure must refuse the lease");
        assert_eq!(error.related_failures.len(), 1);
        assert_eq!(
            poster.signals.lock().unwrap().as_slice(),
            [
                SlpsMakeKeyState::MakeKey,
                SlpsMakeKeyState::MakeKey,
                SlpsMakeKeyState::RemoveKey
            ]
        );
        assert!(state.lock().unwrap().active_belief.is_some());
    }
}
