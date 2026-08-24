//! AT-SPI element cache for Linux.
//! Stores element keys and observation-time bounds indexed by
//! (pid, xid) → element_index.
//!
//! The locked-HashMap plumbing lives in `cua_driver_core::element_cache` — see
//! `docs/dedup-audit.md` item #3. This module owns the Linux-specific
//! `CacheKey` and `CachedSnapshot` (no Drop needed — `Vec<u64>` frees
//! itself).

use super::AtspiNode;
use cua_driver_core::element_cache::ElementCacheCore;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub pid: u32,
    pub xid: u64,
}

pub struct CachedSnapshot {
    /// element_index → element_key (opaque AT-SPI path hash).
    pub elements: Vec<u64>,
    /// Bounds captured by the same walk which assigned each element index.
    pub bounds: HashMap<usize, (i32, i32, u32, u32)>,
}

pub struct ElementCache {
    core: ElementCacheCore<CacheKey, CachedSnapshot>,
}

impl ElementCache {
    pub fn new() -> Self {
        Self {
            core: ElementCacheCore::new(),
        }
    }

    pub fn update(
        &self,
        pid: u32,
        xid: u64,
        nodes: &[AtspiNode],
        bounds: &[(usize, i32, i32, u32, u32)],
    ) {
        let elements: Vec<u64> = nodes
            .iter()
            .filter(|n| n.element_index.is_some())
            .map(|n| n.element_key)
            .collect();
        let bounds = bounds
            .iter()
            .map(|&(index, x, y, width, height)| (index, (x, y, width, height)))
            .collect();
        self.core
            .insert(CacheKey { pid, xid }, CachedSnapshot { elements, bounds });
    }

    pub fn get_element_key(&self, pid: u32, xid: u64, idx: usize) -> Option<u64> {
        self.core
            .with_snapshot(&CacheKey { pid, xid }, |s| s.elements.get(idx).copied())
            .flatten()
    }

    pub fn get_element_bounds(
        &self,
        pid: u32,
        xid: u64,
        idx: usize,
    ) -> Option<(i32, i32, u32, u32)> {
        self.core
            .with_snapshot(&CacheKey { pid, xid }, |snapshot| {
                snapshot.bounds.get(&idx).copied()
            })
            .flatten()
    }

    pub fn element_count(&self, pid: u32, xid: u64) -> usize {
        self.core
            .with_snapshot(&CacheKey { pid, xid }, |s| s.elements.len())
            .unwrap_or(0)
    }
}

impl Default for ElementCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(index: usize) -> AtspiNode {
        AtspiNode {
            element_index: Some(index),
            role: "push button".to_owned(),
            name: None,
            value: None,
            checked: None,
            enabled: Some(true),
            selected: None,
            description: None,
            actions: vec!["press".to_owned()],
            element_key: index as u64,
            depth: 0,
            parent_element_index: None,
            in_web_content: false,
        }
    }

    #[test]
    fn retains_bounds_from_the_index_assigning_observation() {
        let cache = ElementCache::new();
        cache.update(7, 11, &[node(0), node(1)], &[(1, 20, 30, 40, 50)]);

        assert_eq!(cache.get_element_bounds(7, 11, 1), Some((20, 30, 40, 50)));
        assert_eq!(cache.get_element_bounds(7, 11, 0), None);
        assert_eq!(cache.get_element_bounds(7, 12, 1), None);
    }
}
