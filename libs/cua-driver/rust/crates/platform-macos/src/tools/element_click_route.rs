//! Signed-helper-compatible route selection for element-addressed clicks.

use core_foundation::base::{CFEqual, CFRelease, CFRetain, CFTypeRef};

use crate::ax::bindings::{
    copy_action_names, copy_element_attr, copy_string_attr, element_at_screen_position,
    element_screen_rect, AXUIElementRef,
};

const AX_PRESS: &str = "AXPress";
const AX_PICK: &str = "AXPick";
const ATLAS_BUNDLE_ID: &str = "com.openai.atlas";
const MAX_AX_WALK_DEPTH: usize = 300;
const PAGE_CLIP_ACTIONS: [&str; 4] = [
    "AXScrollLeftByPage",
    "AXScrollRightByPage",
    "AXScrollUpByPage",
    "AXScrollDownByPage",
];

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ElementClickRoute {
    Semantic {
        action: String,
        reason: String,
    },
    TargetedPointer {
        screen_x: f64,
        screen_y: f64,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ElementClickRouteError {
    pub code: &'static str,
    pub message: String,
}

/// Select the same ordinary element-click route as the signed macOS helper.
///
/// Explicit secondary actions stay semantic. Ordinary clicks prefer AXPress,
/// then AXPick. A click shape that AX cannot represent, or a target without
/// either action, uses targeted pointer delivery. A web element uses pointer
/// delivery only when its live center hit-tests inside its own subtree.
///
/// # Safety
///
/// `element` must remain retained for this call. The caller owns that retain.
pub(super) unsafe fn select_element_click_route(
    element: AXUIElementRef,
    pid: i32,
    bundle_id: Option<&str>,
    requested_action: &str,
    button: &str,
    click_count: usize,
    has_modifiers: bool,
) -> Result<ElementClickRoute, ElementClickRouteError> {
    if requested_action != "press" {
        return Ok(ElementClickRoute::Semantic {
            action: requested_action.to_owned(),
            reason: "explicit_secondary_action".to_owned(),
        });
    }

    let advertised = copy_action_names(element);
    let semantic_action = ordered_semantic_action(&advertised);
    if pointer_required_by_shape(button, click_count, has_modifiers) {
        let (screen_x, screen_y) = clickable_point(element)?;
        return Ok(ElementClickRoute::TargetedPointer {
            screen_x,
            screen_y,
            reason: "signed_click_shape_requires_targeted_pointer".to_owned(),
        });
    }
    let Some(action) = semantic_action else {
        let (screen_x, screen_y) = clickable_point(element)?;
        return Ok(ElementClickRoute::TargetedPointer {
            screen_x,
            screen_y,
            reason: "signed_no_semantic_action_requires_targeted_pointer".to_owned(),
        });
    };

    match web_context(element, bundle_id) {
        WebContext::Native => Ok(ElementClickRoute::Semantic {
            action: action.to_owned(),
            reason: "signed_native_ordered_ax_action".to_owned(),
        }),
        WebContext::NotProven(reason) => Ok(ElementClickRoute::Semantic {
            action: action.to_owned(),
            reason: format!("signed_web_context_not_proven_uses_semantic:{reason}"),
        }),
        WebContext::Web => {
            let (screen_x, screen_y) = match clickable_point(element) {
                Ok(point) => point,
                Err(error) => {
                    return Ok(ElementClickRoute::Semantic {
                        action: action.to_owned(),
                        reason: format!(
                            "signed_web_click_point_not_proven_uses_semantic:{}",
                            error.code
                        ),
                    });
                }
            };
            match hit_contains_target(element, pid, screen_x, screen_y) {
                HitContainment::Contains => Ok(ElementClickRoute::TargetedPointer {
                    screen_x,
                    screen_y,
                    reason: "signed_web_click_hit_requires_targeted_pointer".to_owned(),
                }),
                HitContainment::Outside => Ok(ElementClickRoute::Semantic {
                    action: action.to_owned(),
                    reason: "signed_web_click_hit_outside_target_uses_semantic".to_owned(),
                }),
                HitContainment::NotProven(reason) => Ok(ElementClickRoute::Semantic {
                    action: action.to_owned(),
                    reason: format!(
                        "signed_web_click_containment_not_proven_uses_semantic:{reason}"
                    ),
                }),
            }
        }
    }
}

fn ordered_semantic_action(actions: &[String]) -> Option<&'static str> {
    [AX_PRESS, AX_PICK]
        .into_iter()
        .find(|action| actions.iter().any(|candidate| candidate == action))
        .map(|action| if action == AX_PRESS { "press" } else { "pick" })
}

fn pointer_required_by_shape(button: &str, click_count: usize, has_modifiers: bool) -> bool {
    button != "left" || click_count != 1 || has_modifiers
}

enum WebContext {
    Native,
    Web,
    NotProven(String),
}

unsafe fn web_context(element: AXUIElementRef, bundle_id: Option<&str>) -> WebContext {
    if bundle_id == Some(ATLAS_BUNDLE_ID) {
        return WebContext::Web;
    }
    match prove_web_ancestry(element) {
        Ok(true) => WebContext::Web,
        Ok(false) => WebContext::Native,
        Err(reason) => WebContext::NotProven(reason),
    }
}

unsafe fn prove_web_ancestry(element: AXUIElementRef) -> Result<bool, String> {
    let mut current = RetainedAxElement::retain(element);
    let mut seen = Vec::new();
    for _ in 0..MAX_AX_WALK_DEPTH {
        if seen
            .iter()
            .any(|prior: &RetainedAxElement| prior.same_identity(&current))
        {
            return Err("ax_parent_identity_cycle".to_owned());
        }
        let role = copy_string_attr(current.as_ptr(), "AXRole")
            .ok_or_else(|| "ax_role_query_failed".to_owned())?;
        if role == "AXWebArea" {
            return Ok(true);
        }
        if role == "AXApplication" {
            return Ok(false);
        }
        let Some(parent) = copy_element_attr(current.as_ptr(), "AXParent") else {
            return Ok(false);
        };
        seen.push(current);
        current = RetainedAxElement::from_owned(parent);
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
    fn from_frame([x, y, width, height]: [f64; 4]) -> Result<Self, ElementClickRouteError> {
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

    fn contains(self, x: f64, y: f64) -> bool {
        x >= self.min_x && x < self.max_x && y >= self.min_y && y < self.max_y
    }
}

unsafe fn clickable_point(element: AXUIElementRef) -> Result<(f64, f64), ElementClickRouteError> {
    let target_frame = live_frame(element)?;
    let center = (
        (target_frame.min_x + target_frame.max_x) / 2.0,
        (target_frame.min_y + target_frame.max_y) / 2.0,
    );
    let mut visible = LiveRect::unbounded();
    let mut current = RetainedAxElement::retain(element);
    let mut seen = vec![current.clone()];

    for _ in 0..MAX_AX_WALK_DEPTH {
        let Some(parent) = copy_element_attr(current.as_ptr(), "AXParent") else {
            return checked_center(center, visible);
        };
        let parent = RetainedAxElement::from_owned(parent);
        if seen.iter().any(|prior| prior.same_identity(&parent)) {
            return Err(stale_geometry(
                "live AX clickable-point parent chain contains an identity cycle",
            ));
        }
        let role = copy_string_attr(parent.as_ptr(), "AXRole")
            .ok_or_else(|| stale_geometry("live AX clickable-point ancestor has no exact role"))?;
        if role == "AXApplication" {
            return checked_center(center, visible);
        }
        if clipping_ancestor(&parent, &role) {
            visible = visible
                .intersect(live_frame(parent.as_ptr())?)
                .ok_or_else(offscreen_error)?;
        }
        seen.push(parent.clone());
        current = parent;
    }
    Err(stale_geometry(
        "live AX clickable-point parent walk exceeded its bound",
    ))
}

unsafe fn clipping_ancestor(element: &RetainedAxElement, role: &str) -> bool {
    if matches!(
        role,
        "AXScrollArea" | "AXWindow" | "AXSheet" | "AXMenu" | "AXPopover"
    ) {
        return true;
    }
    role == "AXGroup"
        && copy_action_names(element.as_ptr())
            .iter()
            .any(|action| PAGE_CLIP_ACTIONS.contains(&action.as_str()))
}

fn checked_center(
    center: (f64, f64),
    visible: LiveRect,
) -> Result<(f64, f64), ElementClickRouteError> {
    visible
        .contains(center.0, center.1)
        .then_some(center)
        .ok_or_else(offscreen_error)
}

unsafe fn live_frame(element: AXUIElementRef) -> Result<LiveRect, ElementClickRouteError> {
    element_screen_rect(element)
        .ok_or_else(|| stale_geometry("live AX element has no exact frame"))
        .and_then(LiveRect::from_frame)
}

enum HitContainment {
    Contains,
    Outside,
    NotProven(String),
}

unsafe fn hit_contains_target(
    element: AXUIElementRef,
    pid: i32,
    screen_x: f64,
    screen_y: f64,
) -> HitContainment {
    let Some(hit) = element_at_screen_position(pid, screen_x, screen_y) else {
        return HitContainment::NotProven("ax_hit_element_missing".to_owned());
    };
    let mut current = RetainedAxElement::from_owned(hit);
    let mut seen = Vec::new();
    for _ in 0..MAX_AX_WALK_DEPTH {
        if current.matches_raw(element) {
            return HitContainment::Contains;
        }
        if seen
            .iter()
            .any(|prior: &RetainedAxElement| prior.same_identity(&current))
        {
            return HitContainment::NotProven("ax_hit_parent_identity_cycle".to_owned());
        }
        let Some(parent) = copy_element_attr(current.as_ptr(), "AXParent") else {
            return HitContainment::Outside;
        };
        seen.push(current);
        current = RetainedAxElement::from_owned(parent);
    }
    HitContainment::NotProven("ax_hit_parent_walk_limit_exhausted".to_owned())
}

fn offscreen_error() -> ElementClickRouteError {
    ElementClickRouteError {
        code: "cannot_click_offscreen_element",
        message: "element center lies outside its visible AX ancestor clip".to_owned(),
    }
}

fn stale_geometry(message: impl Into<String>) -> ElementClickRouteError {
    ElementClickRouteError {
        code: "element_stale",
        message: message.into(),
    }
}

struct RetainedAxElement(AXUIElementRef);

impl RetainedAxElement {
    unsafe fn retain(element: AXUIElementRef) -> Self {
        CFRetain(element as CFTypeRef);
        Self(element)
    }

    unsafe fn from_owned(element: AXUIElementRef) -> Self {
        Self(element)
    }

    fn as_ptr(&self) -> AXUIElementRef {
        self.0
    }

    fn same_identity(&self, other: &Self) -> bool {
        unsafe { CFEqual(self.0 as CFTypeRef, other.0 as CFTypeRef) != 0 }
    }

    fn matches_raw(&self, other: AXUIElementRef) -> bool {
        unsafe { CFEqual(self.0 as CFTypeRef, other as CFTypeRef) != 0 }
    }
}

impl Clone for RetainedAxElement {
    fn clone(&self) -> Self {
        unsafe { Self::retain(self.0) }
    }
}

impl Drop for RetainedAxElement {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_actions_preserve_the_signed_contract() {
        assert_eq!(
            ordered_semantic_action(&[AX_PICK.to_owned(), AX_PRESS.to_owned()]),
            Some("press")
        );
        assert_eq!(ordered_semantic_action(&[AX_PICK.to_owned()]), Some("pick"));
        assert_eq!(ordered_semantic_action(&[]), None);
    }

    #[test]
    fn non_plain_click_shapes_require_pointer_delivery() {
        assert!(!pointer_required_by_shape("left", 1, false));
        assert!(pointer_required_by_shape("left", 2, false));
        assert!(pointer_required_by_shape("right", 1, false));
        assert!(pointer_required_by_shape("middle", 1, false));
        assert!(pointer_required_by_shape("left", 1, true));
    }

    #[test]
    fn clipping_keeps_the_target_center_or_refuses() {
        let target = LiveRect::from_frame([0.0, 0.0, 100.0, 40.0]).unwrap();
        let center = (50.0, 20.0);
        let visible = LiveRect::unbounded()
            .intersect(LiveRect::from_frame([20.0, 0.0, 80.0, 40.0]).unwrap())
            .unwrap();
        assert!(visible.contains(center.0, center.1));
        assert!(!target
            .intersect(LiveRect::from_frame([80.0, 0.0, 20.0, 40.0]).unwrap())
            .unwrap()
            .contains(center.0, center.1));
    }
}
