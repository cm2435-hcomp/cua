//! Platform-neutral accessibility-tree revisions.
//!
//! Native providers own collection and supply a stable render identity for
//! each sibling. This module only preserves public IDs and describes the
//! resulting full/diff update. It never accepts a partial tree.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedAxNode<K> {
    pub render_id: K,
    pub text: String,
    pub detail_text: Option<String>,
    pub action_index: Option<usize>,
    pub children: Vec<Self>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RevisionNode<K> {
    render_id: K,
    element_id: usize,
    text: String,
    action_index: Option<usize>,
    children: Vec<Self>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxTreeUpdateKind {
    Full,
    Diff,
    NoChange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentActionElement {
    pub element_id: usize,
    pub action_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AxTreeUpdate {
    pub kind: AxTreeUpdateKind,
    pub lineage_id: Uuid,
    pub text: String,
    pub elements: Vec<CurrentActionElement>,
}

#[derive(Clone, Debug)]
pub struct AxTreeRevision<K> {
    lineage_id: Uuid,
    roots: Vec<RevisionNode<K>>,
    next_element_id: usize,
}

#[derive(Clone, Debug)]
struct Change<K> {
    kind: ChangeKind,
    path: Vec<usize>,
    node: RevisionNode<K>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ChangeKind {
    Remove,
    Insert,
    Update,
}

impl<K: Clone + Eq + Hash> AxTreeRevision<K> {
    pub fn new(
        roots: Vec<ObservedAxNode<K>>,
        window_description: &str,
        focused_render_id: Option<&K>,
    ) -> (Self, AxTreeUpdate) {
        let lineage_id = Uuid::new_v4();
        let mut next_element_id = 0;
        let roots = roots
            .into_iter()
            .map(|root| assign_new_ids(root, &mut next_element_id))
            .collect::<Vec<_>>();
        let update = AxTreeUpdate {
            kind: AxTreeUpdateKind::Full,
            lineage_id,
            text: render_full(
                &roots,
                window_description,
                focused_line(&roots, focused_render_id).as_deref(),
            ),
            elements: current_action_elements(&roots),
        };
        (
            Self {
                lineage_id,
                roots,
                next_element_id,
            },
            update,
        )
    }

    pub fn append(
        &mut self,
        roots: Vec<ObservedAxNode<K>>,
        window_description: &str,
        focused_render_id: Option<&K>,
    ) -> AxTreeUpdate {
        let mut changes = Vec::new();
        let roots = diff_sibling_lists(
            &self.roots,
            roots,
            &[],
            &mut changes,
            &mut self.next_element_id,
        );
        changes.sort_by(|left, right| left.path.cmp(&right.path).then(left.kind.cmp(&right.kind)));

        let focus_line = focused_line(&roots, focused_render_id);
        let diff = render_diff(&changes, window_description, focus_line.as_deref());
        let full = render_full(&roots, window_description, focus_line.as_deref());
        let full_line_count = roots.iter().map(node_count).sum::<usize>();
        let removed_line_count = usize::from(
            changes
                .iter()
                .any(|change| change.kind == ChangeKind::Remove),
        );
        let diff_line_count = removed_line_count
            + changes
                .iter()
                .filter(|change| matches!(change.kind, ChangeKind::Insert | ChangeKind::Update))
                .map(|change| node_count(&change.node))
                .sum::<usize>();
        let kind = if changes.is_empty() {
            AxTreeUpdateKind::NoChange
        } else if diff_line_count > full_line_count {
            AxTreeUpdateKind::Full
        } else {
            AxTreeUpdateKind::Diff
        };
        let text = match kind {
            AxTreeUpdateKind::Full => full,
            AxTreeUpdateKind::Diff => diff,
            AxTreeUpdateKind::NoChange => {
                render_no_change(window_description, focus_line.as_deref())
            }
        };
        self.roots = roots;
        AxTreeUpdate {
            kind,
            lineage_id: self.lineage_id,
            text,
            elements: current_action_elements(&self.roots),
        }
    }
}

fn focused_line<K: Eq>(roots: &[RevisionNode<K>], render_id: Option<&K>) -> Option<String> {
    let render_id = render_id?;
    for root in roots {
        let mut result = None;
        visit(root, 0, &mut |node, _| {
            if result.is_none() && &node.render_id == render_id {
                result = Some(format!("{} {}", node.element_id, node.text));
            }
        });
        if result.is_some() {
            return result;
        }
    }
    None
}

fn assign_new_ids<K>(node: ObservedAxNode<K>, next: &mut usize) -> RevisionNode<K> {
    let element_id = *next;
    *next += 1;
    RevisionNode {
        render_id: node.render_id,
        element_id,
        text: node.text,
        action_index: node.action_index,
        children: node
            .children
            .into_iter()
            .map(|child| assign_new_ids(child, next))
            .collect(),
    }
}

fn diff_matched_node<K: Clone + Eq + Hash>(
    old: &RevisionNode<K>,
    new: ObservedAxNode<K>,
    path: Vec<usize>,
    changes: &mut Vec<Change<K>>,
    next: &mut usize,
) -> RevisionNode<K> {
    if old.render_id != new.render_id {
        changes.push(Change {
            kind: ChangeKind::Remove,
            path: path.clone(),
            node: old.clone(),
        });
        let inserted = assign_new_ids(new, next);
        changes.push(Change {
            kind: ChangeKind::Insert,
            path,
            node: inserted.clone(),
        });
        return inserted;
    }

    let mut result = RevisionNode {
        render_id: new.render_id,
        element_id: old.element_id,
        text: new.text,
        action_index: new.action_index,
        children: Vec::new(),
    };
    if old.text != result.text {
        changes.push(Change {
            kind: ChangeKind::Update,
            path: path.clone(),
            node: result.clone(),
        });
    }

    result.children = diff_sibling_lists(&old.children, new.children, &path, changes, next);
    result
}

fn diff_sibling_lists<K: Clone + Eq + Hash>(
    old: &[RevisionNode<K>],
    new: Vec<ObservedAxNode<K>>,
    parent_path: &[usize],
    changes: &mut Vec<Change<K>>,
    next: &mut usize,
) -> Vec<RevisionNode<K>> {
    let mut old_by_id: HashMap<&K, Vec<usize>> = HashMap::new();
    for (index, child) in old.iter().enumerate() {
        old_by_id.entry(&child.render_id).or_default().push(index);
    }
    let mut matched_old = HashSet::new();
    let mut result = Vec::with_capacity(new.len());
    for (new_index, child) in new.into_iter().enumerate() {
        let old_index = old_by_id.get(&child.render_id).and_then(|indexes| {
            indexes
                .iter()
                .copied()
                .find(|index| !matched_old.contains(index))
        });
        if let Some(old_index) = old_index {
            matched_old.insert(old_index);
            result.push(diff_matched_node(
                &old[old_index],
                child,
                child_path(parent_path, new_index),
                changes,
                next,
            ));
        } else {
            let inserted = assign_new_ids(child, next);
            changes.push(Change {
                kind: ChangeKind::Insert,
                path: child_path(parent_path, new_index),
                node: inserted.clone(),
            });
            result.push(inserted);
        }
    }
    for (old_index, child) in old.iter().enumerate() {
        if !matched_old.contains(&old_index) {
            changes.push(Change {
                kind: ChangeKind::Remove,
                path: child_path(parent_path, old_index),
                node: child.clone(),
            });
        }
    }
    result
}

fn child_path(parent: &[usize], index: usize) -> Vec<usize> {
    let mut path = parent.to_vec();
    path.push(index);
    path
}

fn render_full<K>(roots: &[RevisionNode<K>], window: &str, focus: Option<&str>) -> String {
    let mut lines = vec![window.to_string()];
    for root in roots {
        visit(root, 0, &mut |node, depth| {
            lines.push(format!(
                "{}{} {}",
                "\t".repeat(depth),
                node.element_id,
                node.text
            ));
        });
    }
    append_focus(&mut lines, focus);
    lines.join("\n")
}

fn render_diff<K>(changes: &[Change<K>], window: &str, focus: Option<&str>) -> String {
    let mut lines = vec![format!(
        "The following is a diff from the previous accessibility tree for {window} with ~ and + representing changed and added elements, respectively. Removed elements are summarized by ID range."
    )];
    let mut removed = Vec::new();
    for change in changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Remove)
    {
        visit(&change.node, 0, &mut |node, _| {
            removed.push(node.element_id)
        });
    }
    if !removed.is_empty() {
        lines.push(format_removed_ids(&removed));
    }
    for change in changes {
        let marker = match change.kind {
            ChangeKind::Insert => "+",
            ChangeKind::Update => "~",
            ChangeKind::Remove => continue,
        };
        visit(
            &change.node,
            change.path.len().saturating_sub(1),
            &mut |node, depth| {
                lines.push(format!(
                    "{marker}{}{} {}",
                    "\t".repeat(depth),
                    node.element_id,
                    node.text
                ));
            },
        );
    }
    append_focus(&mut lines, focus);
    lines.join("\n")
}

fn render_no_change(window: &str, focus: Option<&str>) -> String {
    let mut lines = vec![format!(
        "There has been no change in the accessibility tree for {window}."
    )];
    append_focus(&mut lines, focus);
    lines.join("\n")
}

fn append_focus(lines: &mut Vec<String>, focus: Option<&str>) {
    if let Some(focus) = focus {
        lines.push(format!("The focused UI element is {focus}"));
    }
}

fn current_action_elements<K>(roots: &[RevisionNode<K>]) -> Vec<CurrentActionElement> {
    let mut elements = Vec::new();
    for root in roots {
        visit(root, 0, &mut |node, _| {
            if let Some(action_index) = node.action_index {
                elements.push(CurrentActionElement {
                    element_id: node.element_id,
                    action_index,
                });
            }
        });
    }
    elements
}

fn visit<K>(
    node: &RevisionNode<K>,
    depth: usize,
    callback: &mut impl FnMut(&RevisionNode<K>, usize),
) {
    callback(node, depth);
    for child in &node.children {
        visit(child, depth + 1, callback);
    }
}

fn node_count<K>(node: &RevisionNode<K>) -> usize {
    1 + node.children.iter().map(node_count).sum::<usize>()
}

fn format_removed_ids(ids: &[usize]) -> String {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut ranges = Vec::new();
    for id in ids {
        match ranges.last_mut() {
            Some((_, end)) if id == *end + 1 => *end = id,
            _ => ranges.push((id, id)),
        }
    }
    let ranges = ranges
        .into_iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("Removed element IDs: {ranges}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, text: &str, children: Vec<ObservedAxNode<String>>) -> ObservedAxNode<String> {
        ObservedAxNode {
            render_id: id.into(),
            text: text.into(),
            detail_text: None,
            action_index: None,
            children,
        }
    }

    #[test]
    fn signed_contract_preserves_ids_across_insert_reorder_and_update() {
        let focused = "R".to_string();
        let (mut revision, full) = AxTreeRevision::new(
            vec![node(
                "R",
                "root",
                vec![node("A", "a", vec![]), node("B", "b", vec![])],
            )],
            "Window: \"Fixture\"",
            Some(&focused),
        );
        assert_eq!(full.kind, AxTreeUpdateKind::Full);
        assert!(full.text.contains("\t1 a\n\t2 b"));

        let inserted = revision.append(
            vec![node(
                "R",
                "root",
                vec![
                    node("X", "x", vec![]),
                    node("A", "a", vec![]),
                    node("B", "b", vec![]),
                ],
            )],
            "Window: \"Fixture\"",
            Some(&focused),
        );
        assert_eq!(inserted.kind, AxTreeUpdateKind::Diff);
        assert!(inserted.text.contains("+\t3 x"), "{}", inserted.text);

        let reordered_and_changed = revision.append(
            vec![node(
                "R",
                "root",
                vec![node("B", "after", vec![]), node("A", "a", vec![])],
            )],
            "Window: \"Fixture\"",
            Some(&focused),
        );
        assert!(reordered_and_changed.text.contains("~\t2 after"));
        assert!(reordered_and_changed
            .text
            .contains("The focused UI element is 0 root"));
        assert!(reordered_and_changed
            .text
            .contains("Removed element IDs: 3"));
    }

    #[test]
    fn no_change_ignores_detail_text_but_returns_current_action_manifest() {
        let mut root = node("R", "root", vec![]);
        root.detail_text = Some("before".into());
        root.action_index = Some(7);
        let (mut revision, _) = AxTreeRevision::new(vec![root], "Window: \"Fixture\"", None);
        let mut next = node("R", "root", vec![]);
        next.detail_text = Some("after".into());
        next.action_index = Some(9);
        let update = revision.append(vec![next], "Window: \"Fixture\"", None);
        assert_eq!(update.kind, AxTreeUpdateKind::NoChange);
        assert_eq!(
            update.elements,
            vec![CurrentActionElement {
                element_id: 0,
                action_index: 9
            }]
        );
    }

    #[test]
    fn replacement_removes_before_inserting_and_compresses_subtree_ids() {
        let (mut revision, _) = AxTreeRevision::new(
            vec![node(
                "R",
                "root",
                vec![node("A", "a", vec![node("B", "b", vec![])])],
            )],
            "Window: \"Fixture\"",
            None,
        );
        let update = revision.append(
            vec![node("R", "root", vec![node("X", "x", vec![])])],
            "Window: \"Fixture\"",
            None,
        );
        assert!(update.text.contains("Removed element IDs: 1-2"));
        assert!(update.text.contains("+\t3 x"), "{}", update.text);
    }
}
