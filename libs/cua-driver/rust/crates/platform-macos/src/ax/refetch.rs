//! Fail-closed reacquisition of an AppKit element replaced after observation.

use std::time::Instant;

use core_foundation::base::{CFRelease, CFRetain, CFTypeRef};

use super::{
    bindings::AXUIElementRef,
    cache::RetainedElement,
    exact_target::element_window_id,
    tree::{walk_tree_for_refetch, ElementLocator},
    WindowScope,
};

const MAX_REFETCH_NODES: usize = 2_000;
const MAX_REFETCH_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefetchFailure {
    Missing,
    Ambiguous,
    Incomplete,
}

impl RefetchFailure {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Missing => "refetch_missing",
            Self::Ambiguous => "refetch_ambiguous",
            Self::Incomplete => "refetch_incomplete",
        }
    }
}

/// Search one exact AX window for one exact captured locator. The full bounded
/// walk is required to prove uniqueness; a partial walk is never interpreted
/// as "one match" because another identical row may lie beyond the boundary.
pub fn reacquire(
    pid: i32,
    window_id: u32,
    locator: &ElementLocator,
) -> Result<RetainedElement, RefetchFailure> {
    let started = Instant::now();
    let result = walk_tree_for_refetch(pid, window_id, MAX_REFETCH_NODES, MAX_REFETCH_DEPTH);

    let complete = !result.truncated
        && result
            .window_scope
            .as_ref()
            .is_some_and(WindowScope::is_matched);
    let mut matches = Vec::new();
    if complete {
        for (index, candidate) in result.element_locators.iter().enumerate() {
            if candidate != locator {
                continue;
            }
            let Some(node) = result
                .nodes
                .iter()
                .find(|node| node.element_index == Some(index))
            else {
                continue;
            };
            let exact_window =
                unsafe { element_window_id(node.element_ptr as AXUIElementRef) == Some(window_id) };
            if exact_window {
                matches.push(node.element_ptr);
            }
        }
    }

    let outcome = if !complete {
        Err(RefetchFailure::Incomplete)
    } else if matches.is_empty() {
        Err(RefetchFailure::Missing)
    } else if matches.len() != 1 {
        Err(RefetchFailure::Ambiguous)
    } else {
        let selected = matches[0];
        // The walk owns one retain for every actionable node. Add a distinct
        // action guard before releasing those temporary walk retains below.
        unsafe { CFRetain(selected as AXUIElementRef as CFTypeRef) };
        Ok(unsafe { RetainedElement::from_owned(selected) })
    };

    for node in result
        .nodes
        .iter()
        .filter(|node| node.element_index.is_some())
    {
        if node.element_ptr != 0 {
            unsafe { CFRelease(node.element_ptr as AXUIElementRef as CFTypeRef) };
        }
    }
    tracing::info!(
        pid,
        window_id,
        elapsed_ms = started.elapsed().as_millis(),
        visited_nodes = result.nodes.len(),
        matched = matches.len(),
        complete,
        outcome = ?outcome.as_ref().err(),
        "macOS stale-element refetch"
    );
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::tree::ElementIdentity;

    fn identity(role: &str, title: &str) -> ElementIdentity {
        ElementIdentity {
            role: role.to_owned(),
            subrole: None,
            title: Some(title.to_owned()),
            description: None,
            identifier: None,
        }
    }

    #[test]
    fn locator_identity_includes_collapsed_parent_path() {
        let alice = ElementLocator {
            ancestors: vec![identity("AXGroup", "Alice")],
            target: identity("AXButton", "Delete"),
        };
        let bob = ElementLocator {
            ancestors: vec![identity("AXGroup", "Bob")],
            target: identity("AXButton", "Delete"),
        };
        assert_ne!(alice, bob);
    }
}
