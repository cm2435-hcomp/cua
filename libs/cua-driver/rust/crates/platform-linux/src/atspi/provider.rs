//! Retained Linux AT-SPI reads with fail-closed event invalidation.
//!
//! LibreOffice Calc can allocate gigabytes when an accessibility client repeats
//! an unchanged full traversal. Keep only complete default-shape reads, and
//! reuse them only while the AT-SPI event stream proves that none of the D-Bus
//! peers which supplied the tree emitted an object event. Any uncertainty falls
//! back to the existing complete walk. Actions are deliberately not cached.

use super::AtspiTreeResult;
use atspi::connection::AccessibilityConnection;
use atspi::EventProperties;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, OnceLock,
};

const EVENT_HISTORY_CAP: usize = 4096;

#[derive(Clone, Debug)]
struct EventRecord {
    sequence: u64,
    sender: String,
}

#[derive(Debug)]
struct JournalInner {
    sequence: u64,
    reliable: bool,
    events: VecDeque<EventRecord>,
}

#[derive(Debug)]
pub(super) struct EventJournal {
    inner: Mutex<JournalInner>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lineage {
    Clean { through: u64 },
    Dirty,
    Uncertain,
}

impl EventJournal {
    fn new() -> Self {
        Self {
            inner: Mutex::new(JournalInner {
                sequence: 0,
                reliable: false,
                events: VecDeque::with_capacity(EVENT_HISTORY_CAP),
            }),
        }
    }

    fn set_reliable(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.reliable = true;
        }
    }

    fn mark_unreliable(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.reliable = false;
        }
    }

    fn record(&self, sender: String) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.sequence = inner.sequence.wrapping_add(1);
        let sequence = inner.sequence;
        inner.events.push_back(EventRecord { sequence, sender });
        if inner.events.len() > EVENT_HISTORY_CAP {
            inner.events.pop_front();
        }
    }

    fn snapshot(&self) -> Option<u64> {
        let inner = self.inner.lock().ok()?;
        inner.reliable.then_some(inner.sequence)
    }

    fn lineage_since(&self, since: u64, sources: &HashSet<String>) -> Lineage {
        let Ok(inner) = self.inner.lock() else {
            return Lineage::Uncertain;
        };
        if !inner.reliable || since > inner.sequence {
            return Lineage::Uncertain;
        }
        if since == inner.sequence {
            return Lineage::Clean {
                through: inner.sequence,
            };
        }
        if inner
            .events
            .front()
            .is_none_or(|event| event.sequence > since.saturating_add(1))
        {
            return Lineage::Uncertain;
        }
        if inner
            .events
            .iter()
            .filter(|event| event.sequence > since)
            .any(|event| sources.contains(&event.sender))
        {
            return Lineage::Dirty;
        }
        Lineage::Clean {
            through: inner.sequence,
        }
    }
}

fn journal() -> &'static EventJournal {
    static JOURNAL: OnceLock<EventJournal> = OnceLock::new();
    JOURNAL.get_or_init(EventJournal::new)
}

static MONITOR_STARTED: AtomicBool = AtomicBool::new(false);

pub(super) fn start_event_monitor(connection: &'static AccessibilityConnection) {
    if MONITOR_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    journal().set_reliable();
    tokio::spawn(async move {
        let events = connection.event_stream();
        futures_util::pin_mut!(events);
        while let Some(event) = events.next().await {
            match event {
                Ok(event) => journal().record(event.sender().as_str().to_owned()),
                Err(error) => {
                    tracing::warn!(%error, "AT-SPI event stream became unreliable");
                    journal().mark_unreliable();
                    return;
                }
            }
        }
        journal().mark_unreliable();
    });
}

#[derive(Clone)]
struct CachedTree {
    tree: AtspiTreeResult,
    sources: HashSet<String>,
    through: u64,
}

pub struct AtspiTreeProvider {
    windows: Mutex<HashMap<(u32, u64), CachedTree>>,
}

impl AtspiTreeProvider {
    fn new() -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
        }
    }

    pub fn observe(
        &self,
        pid: u32,
        xid: u64,
        query: Option<&str>,
        max_elements: Option<usize>,
        max_depth: Option<usize>,
        allow_cache: bool,
    ) -> (AtspiTreeResult, &'static str) {
        self.observe_with(
            pid,
            xid,
            query,
            max_elements,
            max_depth,
            allow_cache,
            journal(),
            || super::walk_tree_bounded_uncached(pid, xid, query, max_elements, max_depth),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn observe_with(
        &self,
        pid: u32,
        xid: u64,
        query: Option<&str>,
        max_elements: Option<usize>,
        max_depth: Option<usize>,
        allow_cache: bool,
        events: &EventJournal,
        walk: impl FnOnce() -> AtspiTreeResult,
    ) -> (AtspiTreeResult, &'static str) {
        let cache_shape =
            allow_cache && query.is_none() && max_elements.is_none() && max_depth.is_none();

        if cache_shape {
            let cached = self
                .windows
                .lock()
                .ok()
                .and_then(|windows| windows.get(&(pid, xid)).cloned());
            if let Some(mut cached) = cached {
                if let Lineage::Clean { through } =
                    events.lineage_since(cached.through, &cached.sources)
                {
                    cached.through = through;
                    if let Ok(mut windows) = self.windows.lock() {
                        windows.insert((pid, xid), cached.clone());
                    }
                    return (cached.tree, "clean_cache");
                }
                self.invalidate_pid(pid);
            }
        }

        let tree = walk();
        // Event signals carry the sender's unique bus name. WebKit may publish
        // descendants through a well-known peer name, which cannot be compared
        // to that sender without another owner-resolution contract. Do not
        // cache such trees instead of risking a missed invalidation.
        let sources_are_comparable = !tree.event_sources.is_empty()
            && tree
                .event_sources
                .iter()
                .all(|source| source.starts_with(':'));
        let cache_status = if !cache_shape {
            "full_walk_custom_shape"
        } else if !tree.trusted {
            "full_walk_untrusted"
        } else if !tree.window_scoped {
            "full_walk_app_scoped"
        } else if !sources_are_comparable {
            "full_walk_uncomparable_source"
        } else if let Some(through) = events.snapshot() {
            let sources = tree.event_sources.iter().cloned().collect::<HashSet<_>>();
            // A successful full walk is already the product's accepted
            // best-effort snapshot. LibreOffice emits object events in response
            // to accessibility reads themselves, so requiring a quiet interval
            // during that walk makes every large Calc baseline uncacheable.
            // Establish lineage at completion, as the retained Swift provider
            // does, then fail closed on every later event from a source peer.
            if let Ok(mut windows) = self.windows.lock() {
                windows.insert(
                    (pid, xid),
                    CachedTree {
                        tree: tree.clone(),
                        sources,
                        through,
                    },
                );
            }
            "full_walk_cached"
        } else {
            "full_walk_listener_unavailable"
        };
        if cache_status != "full_walk_cached" {
            if let Ok(mut windows) = self.windows.lock() {
                windows.remove(&(pid, xid));
            }
        }
        (tree, cache_status)
    }

    pub fn invalidate_pid(&self, pid: u32) {
        if let Ok(mut windows) = self.windows.lock() {
            windows.retain(|(cached_pid, _), _| *cached_pid != pid);
        }
    }
}

pub(super) fn global() -> &'static AtspiTreeProvider {
    static PROVIDER: OnceLock<AtspiTreeProvider> = OnceLock::new();
    PROVIDER.get_or_init(AtspiTreeProvider::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_lineage_is_clean_dirty_or_uncertain_without_guessing() {
        let journal = EventJournal::new();
        let sources = HashSet::from([":1.20".to_owned()]);
        assert_eq!(journal.lineage_since(0, &sources), Lineage::Uncertain);

        journal.set_reliable();
        assert_eq!(journal.snapshot(), Some(0));
        journal.record(":1.99".to_owned());
        assert_eq!(
            journal.lineage_since(0, &sources),
            Lineage::Clean { through: 1 }
        );
        journal.record(":1.20".to_owned());
        assert_eq!(journal.lineage_since(1, &sources), Lineage::Dirty);

        journal.mark_unreliable();
        assert_eq!(journal.lineage_since(2, &sources), Lineage::Uncertain);
    }

    #[test]
    fn event_history_overflow_fails_closed() {
        let journal = EventJournal::new();
        let sources = HashSet::from([":1.20".to_owned()]);
        journal.set_reliable();
        for index in 0..=EVENT_HISTORY_CAP {
            journal.record(format!(":1.{index}"));
        }
        assert_eq!(journal.lineage_since(0, &sources), Lineage::Uncertain);
    }

    fn fixture_tree() -> AtspiTreeResult {
        AtspiTreeResult {
            tree_markdown: "fixture".to_owned(),
            nodes: Vec::new(),
            bounds: Vec::new(),
            trusted: true,
            degraded_reason: None,
            window_scoped: true,
            event_sources: vec![":1.20".to_owned()],
        }
    }

    fn well_known_source_tree() -> AtspiTreeResult {
        let mut tree = fixture_tree();
        tree.event_sources = vec!["org.a11y.atspi.WebKit.WebProcess.fixture".to_owned()];
        tree
    }

    #[test]
    fn retained_tree_reuses_only_clean_lineage_and_refreshes_after_event() {
        let provider = AtspiTreeProvider::new();
        let events = EventJournal::new();
        events.set_reliable();
        let walks = std::cell::Cell::new(0);
        let observe = || {
            provider.observe_with(20, 99, None, None, None, true, &events, || {
                walks.set(walks.get() + 1);
                fixture_tree()
            })
        };

        assert_eq!(observe().1, "full_walk_cached");
        assert_eq!(observe().1, "clean_cache");
        events.record(":1.99".to_owned());
        assert_eq!(observe().1, "clean_cache");
        events.record(":1.20".to_owned());
        assert_eq!(observe().1, "full_walk_cached");
        assert_eq!(walks.get(), 2);

        events.mark_unreliable();
        assert_eq!(observe().1, "full_walk_listener_unavailable");
        assert_eq!(walks.get(), 3);
    }

    #[test]
    fn events_during_full_walk_are_before_the_retained_baseline() {
        let provider = AtspiTreeProvider::new();
        let events = EventJournal::new();
        events.set_reliable();

        let first = provider.observe_with(20, 99, None, None, None, true, &events, || {
            // Calc emits this while its accessibility tree is being read. The
            // completed result remains the baseline; only later events dirty it.
            events.record(":1.20".to_owned());
            fixture_tree()
        });
        assert_eq!(first.1, "full_walk_cached");
        assert_eq!(
            provider
                .observe_with(20, 99, None, None, None, true, &events, fixture_tree)
                .1,
            "clean_cache"
        );

        events.record(":1.20".to_owned());
        assert_eq!(
            provider
                .observe_with(20, 99, None, None, None, true, &events, fixture_tree)
                .1,
            "full_walk_cached"
        );
    }

    #[test]
    fn well_known_event_sources_are_never_cached_by_unique_sender_name() {
        let provider = AtspiTreeProvider::new();
        let events = EventJournal::new();
        events.set_reliable();

        assert_eq!(
            provider
                .observe_with(20, 99, None, None, None, true, &events, || {
                    well_known_source_tree()
                })
                .1,
            "full_walk_uncomparable_source"
        );
        assert_eq!(
            provider
                .observe_with(20, 99, None, None, None, true, &events, || {
                    well_known_source_tree()
                })
                .1,
            "full_walk_uncomparable_source"
        );
    }
}
