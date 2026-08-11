use cua_driver_core::api::contracts::ScrollDirection;

use super::semantic::LiveAxElement;
use crate::driver::observation::RegisteredElementSnapshot;

pub(super) const AX_PRESS: &str = "AXPress";

pub(super) enum PageRoute {
    Direct {
        element: LiveAxElement,
        action: &'static str,
    },
    ScrollbarPageChild {
        element: LiveAxElement,
        action: &'static str,
    },
}

pub(super) fn direct_page_action(
    direction: ScrollDirection,
    actions: &[String],
    orientation: Option<&str>,
) -> Option<&'static str> {
    let directional_action = match direction {
        ScrollDirection::Up => "AXPageUp",
        ScrollDirection::Down => "AXPageDown",
        ScrollDirection::Left => "AXPageLeft",
        ScrollDirection::Right => "AXPageRight",
    };
    if actions
        .iter()
        .any(|candidate| candidate == directional_action)
    {
        return Some(directional_action);
    }
    let (required_orientation, relative_action) = match direction {
        ScrollDirection::Up => ("AXVerticalOrientation", "AXDecrementPage"),
        ScrollDirection::Down => ("AXVerticalOrientation", "AXIncrementPage"),
        ScrollDirection::Left => ("AXHorizontalOrientation", "AXDecrementPage"),
        ScrollDirection::Right => ("AXHorizontalOrientation", "AXIncrementPage"),
    };
    (orientation == Some(required_orientation)
        && actions.iter().any(|candidate| candidate == relative_action))
    .then_some(relative_action)
}

pub(super) fn page_child_snapshot(
    root: &RegisteredElementSnapshot,
    elements: &[RegisteredElementSnapshot],
    direction: ScrollDirection,
) -> Option<RegisteredElementSnapshot> {
    let (orientation, subrole) = match direction {
        ScrollDirection::Up => ("AXVerticalOrientation", "AXDecrementPage"),
        ScrollDirection::Down => ("AXVerticalOrientation", "AXIncrementPage"),
        ScrollDirection::Left => ("AXHorizontalOrientation", "AXDecrementPage"),
        ScrollDirection::Right => ("AXHorizontalOrientation", "AXIncrementPage"),
    };

    let mut matches = elements.iter().filter(|candidate| {
        candidate.subrole.as_deref() == Some(subrole)
            && candidate.actions.iter().any(|action| action == AX_PRESS)
            && candidate.owner == root.owner
            && candidate.parent.as_ref().is_some_and(|parent| {
                elements.iter().any(|scrollbar| {
                    scrollbar.element.same_identity(parent)
                        && scrollbar.role == "AXScrollBar"
                        && scrollbar.orientation.as_deref() == Some(orientation)
                        && is_descendant_or_same(scrollbar, root, elements)
                })
            })
    });
    let only = matches.next()?.clone();
    matches.next().is_none().then_some(only)
}

fn is_descendant_or_same(
    candidate: &RegisteredElementSnapshot,
    root: &RegisteredElementSnapshot,
    elements: &[RegisteredElementSnapshot],
) -> bool {
    if candidate.element.same_identity(&root.element) {
        return true;
    }
    let mut current = candidate.parent.clone();
    for _ in 0..elements.len().min(64) {
        let Some(parent) = current else {
            return false;
        };
        if parent.same_identity(&root.element) {
            return true;
        }
        current = elements
            .iter()
            .find(|element| element.element.same_identity(&parent))
            .and_then(|element| element.parent.clone());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_page_actions_are_exact_and_case_sensitive() {
        assert_eq!(
            direct_page_action(ScrollDirection::Down, &["AXPageDown".to_owned()], None,),
            Some("AXPageDown")
        );
        assert_eq!(
            direct_page_action(ScrollDirection::Down, &["axpagedown".to_owned()], None,),
            None
        );
        assert_eq!(
            direct_page_action(
                ScrollDirection::Down,
                &["AXIncrement".to_owned()],
                Some("AXVerticalOrientation"),
            ),
            None
        );
        assert_eq!(
            direct_page_action(
                ScrollDirection::Right,
                &["AXIncrementPage".to_owned()],
                Some("AXHorizontalOrientation"),
            ),
            Some("AXIncrementPage")
        );
        assert_eq!(
            direct_page_action(
                ScrollDirection::Right,
                &["AXIncrementPage".to_owned()],
                Some("AXVerticalOrientation"),
            ),
            None
        );
    }
}
