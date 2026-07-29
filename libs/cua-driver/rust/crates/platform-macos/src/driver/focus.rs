//! Exact macOS interaction recipes and durable synthetic app-focus belief.

use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_core::api::{
    capabilities::Framework,
    contracts::{MouseButton, Point, Rect, Route},
    errors::{ErrorCode, ErrorPhase, NativeError},
    interaction::{NativeEvidence, ScopeRequirements},
    platform::ResolvedAction,
};

use crate::{
    apps,
    ax::bindings::{copy_bool_attr, AXUIElementCreateApplication},
    input::synthesized_event,
};

use super::{target::MacFocusState, windows::MacWindowFacts};

const PROVEN_OS_VERSION: &str = "26.5.1";
const PROVEN_ARCHITECTURE: &str = "arm64";
const BELIEF_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const BELIEF_ACK_POLL_INTERVAL: Duration = Duration::from_millis(10);

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
    SwiftCoordinateClick,
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
            TargetBeliefRecipe::SwiftCoordinateClick => {
                "swift_coordinate_click_cps_key_focus_returned_macos_26_5_1_arm64"
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
    if route == Route::TargetedPointer && *framework == Framework::Catalyst {
        return Err(recipe_unproven(
            host,
            framework,
            route,
            action,
            "the helper's Catalyst preparation and focus reassertion branch is not ported",
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
                && is_exact_proven_point_click(action) =>
        {
            TargetBeliefRecipe::SwiftCoordinateClick
        }
        _ => {
            return Err(recipe_unproven(
                host,
                framework,
                route,
                action,
                "no exact target-belief recipe is proved for this host/action cell",
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

pub(crate) trait TargetFocusPoster: Send + Sync {
    fn post_key_focus_returned(&self, pid: i32) -> Result<(), NativeError>;
    fn post_app_activated(
        &self,
        pid: i32,
        cg_window_id: u32,
        window_bounds: Rect,
        activation_point: Option<Point>,
    ) -> Result<usize, NativeError>;
}

pub(crate) trait TargetFocusReader: Send + Sync {
    fn application_is_active(&self, pid: i32) -> bool;
    fn application_believes_frontmost(&self, pid: i32) -> Option<bool>;
}

#[derive(Default)]
pub(crate) struct SystemTargetFocusPoster;

impl TargetFocusPoster for SystemTargetFocusPoster {
    fn post_key_focus_returned(&self, pid: i32) -> Result<(), NativeError> {
        synthesized_event::post_key_focus_returned(pid).map_err(|mut error| {
            error.phase = ErrorPhase::Preflight;
            error
                .with_detail("pid", pid)
                .with_detail("cps_notification", "key_focus_returned")
        })
    }

    fn post_app_activated(
        &self,
        pid: i32,
        cg_window_id: u32,
        window_bounds: Rect,
        activation_point: Option<Point>,
    ) -> Result<usize, NativeError> {
        synthesized_event::post_app_activated(pid, cg_window_id, window_bounds, activation_point)
            .map_err(|mut error| {
                error.phase = ErrorPhase::Preflight;
                error
                    .with_detail("pid", pid)
                    .with_detail("appkit_notification", "app_activated")
            })
    }
}

#[derive(Default)]
pub(crate) struct SystemTargetFocusReader;

impl TargetFocusReader for SystemTargetFocusReader {
    fn application_is_active(&self, pid: i32) -> bool {
        apps::frontmost_pid() == Some(pid)
    }

    fn application_believes_frontmost(&self, pid: i32) -> Option<bool> {
        unsafe {
            let application = AXUIElementCreateApplication(pid);
            if application.is_null() {
                return None;
            }
            let frontmost = copy_bool_attr(application, "AXFrontmost");
            CFRelease(application as CFTypeRef);
            frontmost
        }
    }
}

pub(crate) fn prepare_target_focus(
    recipe: TargetBeliefRecipe,
    facts: &MacWindowFacts,
    state: Arc<Mutex<MacFocusState>>,
    deadline: Instant,
) -> Result<Option<NativeEvidence>, NativeError> {
    match recipe {
        TargetBeliefRecipe::NotApplicable => Ok(None),
        TargetBeliefRecipe::SwiftCoordinateClick => prepare_target_focus_with(
            facts,
            state,
            deadline,
            &SystemTargetFocusPoster,
            &SystemTargetFocusReader,
        )
        .map(Some),
    }
}

fn prepare_target_focus_with(
    facts: &MacWindowFacts,
    state: Arc<Mutex<MacFocusState>>,
    deadline: Instant,
    poster: &dyn TargetFocusPoster,
    reader: &dyn TargetFocusReader,
) -> Result<NativeEvidence, NativeError> {
    let pid = facts.pid;
    let cg_window_id = facts.cg_window_id;
    let application_is_active = reader.application_is_active(pid);
    let (should_post_key_focus_returned, should_post_app_activated) = {
        let mut state = state.lock().expect("macOS focus coordinator poisoned");
        if state.shutdown {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "target focus coordinator is shut down",
            ));
        }
        if state.pid != pid || state.cg_window_id != cg_window_id {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "target focus coordinator identity no longer matches the interaction target",
            ));
        }
        state.application_is_active = application_is_active;
        if application_is_active {
            state.application_believes_it_is_active = true;
            state.application_believes_it_has_focus = true;
        }
        (
            !state.application_is_active && !state.application_believes_it_has_focus,
            !state.application_is_active && !state.application_believes_it_is_active,
        )
    };

    if should_post_key_focus_returned {
        poster.post_key_focus_returned(pid)?;
        let mut state = state.lock().expect("macOS focus coordinator poisoned");
        state.application_believes_it_has_focus = true;
    }
    let app_activation_event_count = if should_post_app_activated {
        let event_count =
            poster.post_app_activated(pid, cg_window_id, facts.bounds, facts.activation_point)?;
        let mut state = state.lock().expect("macOS focus coordinator poisoned");
        state.application_believes_it_is_active = true;
        event_count
    } else {
        0
    };

    let ack_deadline = deadline.min(Instant::now() + BELIEF_ACK_TIMEOUT);
    let mut ax_frontmost_acknowledged = application_is_active;
    while !ax_frontmost_acknowledged && Instant::now() <= ack_deadline {
        ax_frontmost_acknowledged = reader.application_believes_frontmost(pid) == Some(true);
        if !ax_frontmost_acknowledged {
            thread::sleep(
                BELIEF_ACK_POLL_INTERVAL
                    .min(ack_deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    if !ax_frontmost_acknowledged {
        return Err(NativeError::new(
            ErrorCode::DispatchFailed,
            ErrorPhase::Preflight,
            true,
            "target did not acknowledge the synthetic focus belief through AXFrontmost",
        )
        .with_detail("pid", pid)
        .with_detail("cg_window_id", cg_window_id)
        .with_detail(
            "cps_key_focus_returned_posted",
            should_post_key_focus_returned,
        )
        .with_detail("app_activated_posted", should_post_app_activated)
        .with_detail("app_activation_event_count", app_activation_event_count)
        .with_detail(
            "belief_ack_timeout_ms",
            BELIEF_ACK_TIMEOUT.as_millis() as u64,
        ));
    }

    let mut evidence = NativeEvidence::default();
    evidence.fields.insert(
        "target_application_was_active".to_owned(),
        application_is_active.into(),
    );
    evidence.fields.insert(
        "target_focus_notification_posted".to_owned(),
        should_post_key_focus_returned.into(),
    );
    evidence.fields.insert(
        "target_focus_notification".to_owned(),
        "cps_key_focus_returned".into(),
    );
    evidence.fields.insert(
        "target_app_activation_posted".to_owned(),
        should_post_app_activated.into(),
    );
    evidence.fields.insert(
        "target_app_activation_event_count".to_owned(),
        app_activation_event_count.into(),
    );
    evidence.fields.insert(
        "target_ax_frontmost_acknowledged".to_owned(),
        ax_frontmost_acknowledged.into(),
    );
    evidence.fields.insert(
        "target_focus_lifetime".to_owned(),
        "target_controller".into(),
    );
    evidence.fields.insert(
        "target_focus_event_taps".to_owned(),
        "process_notification,target_mouse,view_bridge_keyboard_when_present".into(),
    );
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

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
        posts: Mutex<Vec<(&'static str, i32)>>,
    }

    impl TargetFocusPoster for FakePoster {
        fn post_key_focus_returned(&self, pid: i32) -> Result<(), NativeError> {
            self.posts.lock().unwrap().push(("key_focus_returned", pid));
            Ok(())
        }

        fn post_app_activated(
            &self,
            pid: i32,
            cg_window_id: u32,
            window_bounds: Rect,
            activation_point: Option<Point>,
        ) -> Result<usize, NativeError> {
            assert_eq!(cg_window_id, 99);
            assert_eq!(window_bounds.width, 10.0);
            assert_eq!(activation_point, Some(Point { x: 1.0, y: 1.0 }));
            self.posts.lock().unwrap().push(("app_activated", pid));
            Ok(3)
        }
    }

    struct FakeReader {
        active: bool,
        frontmost: Mutex<VecDeque<Option<bool>>>,
    }

    impl TargetFocusReader for FakeReader {
        fn application_is_active(&self, _pid: i32) -> bool {
            self.active
        }

        fn application_believes_frontmost(&self, _pid: i32) -> Option<bool> {
            self.frontmost
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Some(true))
        }
    }

    fn point_click(framework: Framework, button: MouseButton, count: u8) -> ResolvedAction {
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
            framework,
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

    fn focus_facts() -> MacWindowFacts {
        let ResolvedAction::PointClick { point, .. } =
            point_click(Framework::Unknown, MouseButton::Left, 1)
        else {
            unreachable!("fixture is a point click");
        };
        MacWindowFacts {
            stamp: point.window.stamp(),
            pid: 44,
            process_generation: 1,
            cg_window_id: 99,
            owner_name: "Fixture".to_owned(),
            layer: 0,
            bounds: point.window.geometry.bounds,
            activation_point: Some(Point { x: 1.0, y: 1.0 }),
            scale_factor: Some(1.0),
            state: cua_driver_core::api::capabilities::WindowStateKind::Visible,
            is_on_screen: true,
            on_current_space: Some(true),
            space_ids: Some(vec![1]),
            minimized: Some(false),
        }
    }

    #[test]
    fn recipe_selector_uses_exact_host_and_action_facts_not_framework_guessing() {
        let host = HostRecipeContext {
            os_version: PROVEN_OS_VERSION.to_owned(),
            architecture: PROVEN_ARCHITECTURE.to_owned(),
        };
        let requirements = ScopeRequirements::for_route(Route::TargetedPointer);
        for framework in [Framework::Unknown, Framework::Chromium, Framework::Electron] {
            let recipe = select_scope_recipe(
                &host,
                &framework,
                Route::TargetedPointer,
                &point_click(framework.clone(), MouseButton::Left, 1),
                &requirements,
            )
            .unwrap();
            assert_eq!(
                recipe.target_belief,
                TargetBeliefRecipe::SwiftCoordinateClick
            );
        }
        let catalyst_error = select_scope_recipe(
            &host,
            &Framework::Catalyst,
            Route::TargetedPointer,
            &point_click(Framework::Catalyst, MouseButton::Left, 1),
            &requirements,
        )
        .unwrap_err();
        assert_eq!(catalyst_error.code, ErrorCode::UnsupportedInBackground);
        let error = select_scope_recipe(
            &host,
            &Framework::Unknown,
            Route::TargetedPointer,
            &point_click(Framework::Unknown, MouseButton::Right, 1),
            &requirements,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedInBackground);
    }

    #[test]
    fn focus_belief_posts_once_and_remains_owned_by_the_target_controller() {
        let facts = focus_facts();
        let state = Arc::new(Mutex::new(MacFocusState::new(44, 99)));
        let poster = FakePoster {
            posts: Mutex::new(Vec::new()),
        };
        let reader = FakeReader {
            active: false,
            frontmost: Mutex::new(VecDeque::from([Some(false), Some(true)])),
        };
        prepare_target_focus_with(
            &facts,
            Arc::clone(&state),
            Instant::now() + Duration::from_secs(1),
            &poster,
            &reader,
        )
        .unwrap();
        prepare_target_focus_with(
            &facts,
            Arc::clone(&state),
            Instant::now() + Duration::from_secs(1),
            &poster,
            &reader,
        )
        .unwrap();

        assert_eq!(
            *poster.posts.lock().unwrap(),
            [("key_focus_returned", 44), ("app_activated", 44)]
        );
        let state = state.lock().unwrap();
        assert!(state.application_believes_it_is_active);
        assert!(state.application_believes_it_has_focus);
        assert!(!state.application_is_active);
    }
}
