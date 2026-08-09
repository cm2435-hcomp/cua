use cua_driver_core::api::{
    contracts::{MouseButton, Point},
    errors::{ErrorCode, NativeError},
    platform::ClickSpec,
};

use crate::{
    ax::bindings::{
        copy_action_names_exact, copy_element_at_position, copy_element_attr, copy_point_attr,
        copy_size_attr, copy_string_attr_exact, AXUIElementCreateApplication,
    },
    driver::observation::RetainedAxElement,
};

use super::{scroll::AX_PRESS, semantic::LiveAxElement};

pub(super) const AX_PICK: &str = "AXPick";
const MAX_AX_WALK_DEPTH: usize = 300;
const ATLAS_BUNDLE_ID: &str = "com.openai.atlas";
const PAGE_CLIP_ACTIONS: [&str; 4] = [
    "AXScrollLeftByPage",
    "AXScrollRightByPage",
    "AXScrollUpByPage",
    "AXScrollDownByPage",
];

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ClickRoute {
    Semantic {
        action: &'static str,
        reason: String,
    },
    TargetedPointer {
        screen_point: Point,
        reason: String,
    },
}

pub(super) fn select_click_route(
    element: &LiveAxElement,
    click: &ClickSpec,
    bundle_id: Option<&str>,
    pid: i32,
) -> Result<ClickRoute, NativeError> {
    let action = ordered_semantic_action(&element.snapshot.actions);
    if pointer_required_by_shape(click) {
        return Ok(ClickRoute::TargetedPointer {
            screen_point: clickable_point(element)?,
            reason: "signed_click_shape_requires_targeted_pointer".to_owned(),
        });
    }
    let Some(action) = action else {
        return Ok(ClickRoute::TargetedPointer {
            screen_point: clickable_point(element)?,
            reason: "signed_no_semantic_action_requires_targeted_pointer".to_owned(),
        });
    };

    match web_context(element, bundle_id) {
        WebContext::Native => Ok(ClickRoute::Semantic {
            action,
            reason: "signed_native_ordered_ax_action".to_owned(),
        }),
        WebContext::NotProven(reason) => Ok(ClickRoute::Semantic {
            action,
            reason: format!("signed_web_context_not_proven_uses_semantic:{reason}"),
        }),
        WebContext::Web => {
            let screen_point = match clickable_point(element) {
                Ok(point) => point,
                Err(error) => {
                    return Ok(ClickRoute::Semantic {
                        action,
                        reason: format!(
                            "signed_web_click_point_not_proven_uses_semantic:{}",
                            error.message
                        ),
                    })
                }
            };
            match hit_contains_target(element, pid, screen_point) {
                HitContainment::Contains => Ok(ClickRoute::TargetedPointer {
                    screen_point,
                    reason: "signed_web_click_hit_requires_targeted_pointer".to_owned(),
                }),
                HitContainment::Outside => Ok(ClickRoute::Semantic {
                    action,
                    reason: "signed_web_click_hit_outside_target_uses_semantic".to_owned(),
                }),
                HitContainment::NotProven(reason) => Ok(ClickRoute::Semantic {
                    action,
                    reason: format!(
                        "signed_web_click_containment_not_proven_uses_semantic:{reason}"
                    ),
                }),
            }
        }
    }
}

fn pointer_required_by_shape(click: &ClickSpec) -> bool {
    click.button != MouseButton::Left || click.click_count != 1 || !click.modifiers.is_empty()
}

fn ordered_semantic_action(actions: &[String]) -> Option<&'static str> {
    [AX_PRESS, AX_PICK]
        .into_iter()
        .find(|action| actions.iter().any(|candidate| candidate == action))
}

enum WebContext {
    Native,
    Web,
    NotProven(String),
}

fn web_context(element: &LiveAxElement, bundle_id: Option<&str>) -> WebContext {
    if bundle_id == Some(ATLAS_BUNDLE_ID) {
        return WebContext::Web;
    }
    match prove_web_ancestry(element) {
        Ok(true) => WebContext::Web,
        Ok(false) => WebContext::Native,
        Err(reason) => WebContext::NotProven(reason),
    }
}

fn prove_web_ancestry(element: &LiveAxElement) -> Result<bool, String> {
    let mut current = unsafe { RetainedAxElement::retain(element.as_ptr()) };
    let mut seen = Vec::new();
    for _ in 0..MAX_AX_WALK_DEPTH {
        if seen
            .iter()
            .any(|prior: &RetainedAxElement| prior.same_identity(&current))
        {
            return Err("ax_parent_identity_cycle".to_owned());
        }
        let role = unsafe { copy_string_attr_exact(current.as_ptr(), "AXRole") }
            .map_err(|error| format!("ax_role_query_failed:{error}"))?;
        if role.as_deref() == Some("AXWebArea") {
            return Ok(true);
        }
        if role.as_deref() == Some("AXApplication") {
            return Ok(false);
        }
        let parent = unsafe { copy_element_attr(current.as_ptr(), "AXParent") }
            .map_err(|error| format!("ax_parent_query_failed:{error}"))?;
        let Some(parent) = parent else {
            return Ok(false);
        };
        seen.push(current);
        current = unsafe { RetainedAxElement::from_owned(parent) };
    }
    Err("ax_parent_walk_limit_exhausted".to_owned())
}

#[derive(Debug, Clone, Copy)]
struct LiveRect {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl LiveRect {
    fn from_frame(frame: [f64; 4]) -> Result<Self, NativeError> {
        let [x, y, width, height] = frame;
        let max_x = x + width;
        let max_y = y + height;
        if !x.is_finite()
            || !y.is_finite()
            || !width.is_finite()
            || !height.is_finite()
            || !max_x.is_finite()
            || !max_y.is_finite()
            || width <= 0.0
            || height <= 0.0
        {
            return Err(stale_geometry("live AX frame is invalid"));
        }
        Ok(Self {
            min_x: x,
            min_y: y,
            max_x,
            max_y,
        })
    }

    fn unbounded() -> Self {
        Self {
            min_x: f64::NEG_INFINITY,
            min_y: f64::NEG_INFINITY,
            max_x: f64::INFINITY,
            max_y: f64::INFINITY,
        }
    }

    fn intersect(self, other: Self) -> Option<Self> {
        let intersection = Self {
            min_x: self.min_x.max(other.min_x),
            min_y: self.min_y.max(other.min_y),
            max_x: self.max_x.min(other.max_x),
            max_y: self.max_y.min(other.max_y),
        };
        (intersection.min_x < intersection.max_x && intersection.min_y < intersection.max_y)
            .then_some(intersection)
    }

    fn contains(self, point: Point) -> bool {
        point.x >= self.min_x
            && point.x < self.max_x
            && point.y >= self.min_y
            && point.y < self.max_y
    }
}

fn clickable_point(element: &LiveAxElement) -> Result<Point, NativeError> {
    let target_frame = live_frame(element.as_ptr())?;
    let center = Point {
        x: (target_frame.min_x + target_frame.max_x) / 2.0,
        y: (target_frame.min_y + target_frame.max_y) / 2.0,
    };
    let mut visible = LiveRect::unbounded();
    let mut current = unsafe { RetainedAxElement::retain(element.as_ptr()) };
    let mut seen = vec![current.clone()];

    for _ in 0..MAX_AX_WALK_DEPTH {
        let parent = unsafe { copy_element_attr(current.as_ptr(), "AXParent") }
            .map_err(|error| stale_ax("AXParent query for clickable point", error))?;
        let Some(parent) = parent else {
            break;
        };
        let parent = unsafe { RetainedAxElement::from_owned(parent) };
        if seen.iter().any(|prior| prior.same_identity(&parent)) {
            return Err(NativeError::stale(
                ErrorCode::ElementStale,
                "live AX clickable-point parent chain contains an identity cycle",
            ));
        }
        let role = unsafe { copy_string_attr_exact(parent.as_ptr(), "AXRole") }
            .map_err(|error| stale_ax("AXRole query for clickable point", error))?
            .ok_or_else(|| {
                NativeError::stale(
                    ErrorCode::ElementStale,
                    "live AX clickable-point ancestor has no exact role",
                )
            })?;
        if role == "AXApplication" {
            return checked_center(center, visible);
        }
        if clipping_ancestor(&parent, &role)? {
            let frame = live_frame(parent.as_ptr())?;
            visible = visible.intersect(frame).ok_or_else(offscreen_error)?;
        }
        seen.push(parent.clone());
        current = parent;
    }
    if seen.len() >= MAX_AX_WALK_DEPTH {
        return Err(NativeError::stale(
            ErrorCode::ElementStale,
            "live AX clickable-point parent walk exceeded its bound",
        ));
    }
    checked_center(center, visible)
}

fn clipping_ancestor(element: &RetainedAxElement, role: &str) -> Result<bool, NativeError> {
    if matches!(
        role,
        "AXScrollArea" | "AXWindow" | "AXSheet" | "AXMenu" | "AXPopover"
    ) {
        return Ok(true);
    }
    if role != "AXGroup" {
        return Ok(false);
    }
    let actions = unsafe { copy_action_names_exact(element.as_ptr()) }
        .map_err(|error| stale_ax("AXGroup action query for clickable point", error))?;
    Ok(actions
        .iter()
        .any(|action| PAGE_CLIP_ACTIONS.contains(&action.as_str())))
}

fn checked_center(center: Point, visible: LiveRect) -> Result<Point, NativeError> {
    if visible.contains(center) {
        Ok(center)
    } else {
        Err(offscreen_error())
    }
}

fn live_frame(element: crate::ax::bindings::AXUIElementRef) -> Result<LiveRect, NativeError> {
    let position = unsafe { copy_point_attr(element, "AXPosition") }
        .map_err(|error| stale_ax("AXPosition query for clickable point", error))?
        .ok_or_else(|| stale_geometry("live AX element has no exact position"))?;
    let size = unsafe { copy_size_attr(element, "AXSize") }
        .map_err(|error| stale_ax("AXSize query for clickable point", error))?
        .ok_or_else(|| stale_geometry("live AX element has no exact size"))?;
    LiveRect::from_frame([position.0, position.1, size.0, size.1])
}

enum HitContainment {
    Contains,
    Outside,
    NotProven(String),
}

fn hit_contains_target(element: &LiveAxElement, pid: i32, point: Point) -> HitContainment {
    let application = unsafe { AXUIElementCreateApplication(pid) };
    if application.is_null() {
        return HitContainment::NotProven("ax_application_root_unavailable".to_owned());
    }
    let _application = unsafe { RetainedAxElement::from_owned(application) };
    let hit = match unsafe { copy_element_at_position(application, point.x, point.y) } {
        Ok(Some(hit)) => hit,
        Ok(None) => return HitContainment::NotProven("ax_hit_element_missing".to_owned()),
        Err(error) => return HitContainment::NotProven(format!("ax_hit_test_failed:{error}")),
    };
    let mut current = unsafe { RetainedAxElement::from_owned(hit) };
    let mut seen = Vec::new();
    for _ in 0..MAX_AX_WALK_DEPTH {
        if element.native_identity_matches(&current) {
            return HitContainment::Contains;
        }
        if seen
            .iter()
            .any(|prior: &RetainedAxElement| prior.same_identity(&current))
        {
            return HitContainment::NotProven("ax_hit_parent_identity_cycle".to_owned());
        }
        let parent = match unsafe { copy_element_attr(current.as_ptr(), "AXParent") } {
            Ok(Some(parent)) => parent,
            Ok(None) => return HitContainment::Outside,
            Err(error) => {
                return HitContainment::NotProven(format!("ax_hit_parent_query_failed:{error}"))
            }
        };
        seen.push(current);
        current = unsafe { RetainedAxElement::from_owned(parent) };
    }
    HitContainment::NotProven("ax_hit_parent_walk_limit_exhausted".to_owned())
}

fn offscreen_error() -> NativeError {
    NativeError::unsupported("element center lies outside its visible AX ancestor clip")
        .with_detail("reason", "element_center_outside_visible_ancestor_clip")
}

fn stale_geometry(message: impl Into<String>) -> NativeError {
    NativeError::stale(ErrorCode::ElementStale, message)
}

fn stale_ax(operation: &str, error: i32) -> NativeError {
    NativeError::stale(
        ErrorCode::ElementStale,
        format!("{operation} failed during live click routing"),
    )
    .with_detail("ax_error", error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::api::contracts::Modifier;

    #[test]
    fn ordered_actions_preserve_the_signed_contract() {
        assert_eq!(
            ordered_semantic_action(&[AX_PICK.to_owned(), AX_PRESS.to_owned()]),
            Some(AX_PRESS)
        );
        assert_eq!(
            ordered_semantic_action(&[AX_PICK.to_owned()]),
            Some(AX_PICK)
        );
        assert_eq!(ordered_semantic_action(&[]), None);
    }

    #[test]
    fn double_secondary_middle_and_modified_clicks_require_pointer_delivery() {
        let click = |button, click_count, modifiers| ClickSpec {
            button,
            click_count,
            modifiers,
        };
        assert!(!pointer_required_by_shape(&click(
            MouseButton::Left,
            1,
            vec![]
        )));
        for spec in [
            click(MouseButton::Left, 2, vec![]),
            click(MouseButton::Right, 1, vec![]),
            click(MouseButton::Middle, 1, vec![]),
            click(MouseButton::Left, 1, vec![Modifier::Shift]),
        ] {
            assert!(pointer_required_by_shape(&spec));
        }
    }

    #[test]
    fn clipping_intersection_keeps_the_target_center_or_refuses() {
        let target = LiveRect::from_frame([0.0, 0.0, 100.0, 40.0]).unwrap();
        let center = Point { x: 50.0, y: 20.0 };
        let visible = LiveRect::unbounded()
            .intersect(LiveRect::from_frame([20.0, 0.0, 80.0, 40.0]).unwrap())
            .unwrap();
        assert!(visible.contains(center));
        assert!(!target
            .intersect(LiveRect::from_frame([80.0, 0.0, 20.0, 40.0]).unwrap())
            .unwrap()
            .contains(center));
    }
}
