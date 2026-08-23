//! Retained macOS AX trees with fail-closed notification invalidation.
//!
//! The provider deliberately caches only complete, exact-window walks. One
//! AXObserver on the application root receives descendant notifications on
//! macOS. Value/title notifications may refresh the exact retained callback
//! element; structural notifications and any event-log uncertainty require a
//! complete resynchronization.

use super::{
    bindings::{
        kAXErrorNotificationAlreadyRegistered, kAXErrorSuccess, AXObserverAddNotification,
        AXObserverCallback, AXObserverCreate, AXObserverGetRunLoopSource, AXObserverRef,
        AXUIElementCreateApplication, AXUIElementRef,
    },
    cache::ElementCache,
    tree::{
        refresh_cached_node, render_nodes, walk_tree_bounded, TreeWalkResult, DEFAULT_MAX_DEPTH,
        DEFAULT_MAX_ELEMENTS,
    },
};
use core_foundation::{
    base::{CFHash, CFRelease, CFRetain, CFTypeRef, TCFType},
    runloop::{kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopAddSource, CFRunLoopRemoveSource},
    string::{CFString, CFStringRef},
};
use std::{
    collections::{HashMap, VecDeque},
    ffi::c_void,
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
};

const INVALIDATING_NOTIFICATIONS: [&str; 10] = [
    "AXValueChanged",
    "AXTitleChanged",
    "AXUIElementDestroyed",
    "AXFocusedUIElementChanged",
    "AXSelectedChildrenChanged",
    "AXRowCountChanged",
    "AXLayoutChanged",
    "AXCreated",
    "AXMoved",
    "AXResized",
];

struct ObserverState {
    epoch: AtomicU64,
    events: Mutex<VecDeque<ObserverEvent>>,
    alive: AtomicBool,
    stop: AtomicBool,
}

impl Default for ObserverState {
    fn default() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            events: Mutex::new(VecDeque::with_capacity(512)),
            alive: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ObserverEventKind {
    Targeted,
    FullResync,
    Ignored,
}

struct ObserverEvent {
    epoch: u64,
    kind: ObserverEventKind,
    render_id: Option<usize>,
    element_ptr: usize,
}

impl Drop for ObserverEvent {
    fn drop(&mut self) {
        if self.element_ptr != 0 {
            unsafe { CFRelease(self.element_ptr as AXUIElementRef as CFTypeRef) };
        }
    }
}

struct DirtyElement {
    kind: ObserverEventKind,
    render_id: usize,
    element_ptr: usize,
}

impl Drop for DirtyElement {
    fn drop(&mut self) {
        if self.element_ptr != 0 {
            unsafe { CFRelease(self.element_ptr as AXUIElementRef as CFTypeRef) };
        }
    }
}

enum InvalidationSince {
    Clean,
    Targeted {
        through_epoch: u64,
        elements: Vec<DirtyElement>,
    },
    FullResync,
}

fn apply_invalidations(result: &mut TreeWalkResult, elements: &[DirtyElement]) -> Result<bool, ()> {
    let mut refreshed_any = false;
    for dirty in elements {
        let node = result
            .nodes
            .iter_mut()
            .find(|node| node.render_id == dirty.render_id);
        match (dirty.kind, node) {
            (ObserverEventKind::Ignored, _) | (_, None) => {}
            (ObserverEventKind::FullResync, Some(_)) => return Err(()),
            (ObserverEventKind::Targeted, Some(node)) => {
                refreshed_any = true;
                if !unsafe { refresh_cached_node(node, dirty.element_ptr as AXUIElementRef) } {
                    return Err(());
                }
            }
        }
    }
    if refreshed_any {
        result.tree_markdown = render_nodes(&result.nodes);
    }
    Ok(refreshed_any)
}

impl ObserverState {
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    fn invalidations_since(&self, since_epoch: u64) -> InvalidationSince {
        let through_epoch = self.epoch();
        if since_epoch == through_epoch {
            return InvalidationSince::Clean;
        }
        let Ok(events) = self.events.lock() else {
            return InvalidationSince::FullResync;
        };
        let relevant = events
            .iter()
            .filter(|event| event.epoch > since_epoch)
            .collect::<Vec<_>>();
        if relevant.is_empty()
            || relevant
                .first()
                .is_none_or(|event| event.epoch != since_epoch + 1)
            || relevant
                .last()
                .is_none_or(|event| event.epoch != through_epoch)
        {
            return InvalidationSince::FullResync;
        }

        let mut elements = Vec::with_capacity(relevant.len());
        for event in relevant {
            let (Some(render_id), element_ptr) = (event.render_id, event.element_ptr) else {
                return InvalidationSince::FullResync;
            };
            if element_ptr != 0 {
                unsafe { CFRetain(element_ptr as AXUIElementRef as CFTypeRef) };
            }
            elements.push(DirtyElement {
                kind: event.kind,
                render_id,
                element_ptr,
            });
        }
        InvalidationSince::Targeted {
            through_epoch,
            elements,
        }
    }
}

unsafe extern "C" fn observer_callback(
    _observer: AXObserverRef,
    element: AXUIElementRef,
    notification: CFStringRef,
    refcon: *mut c_void,
) {
    if let Some(state) = (refcon as *const ObserverState).as_ref() {
        let epoch = state.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let name = (!notification.is_null())
            .then(|| CFString::wrap_under_get_rule(notification).to_string())
            .unwrap_or_default();
        let kind = match name.as_str() {
            "AXValueChanged" | "AXTitleChanged" => ObserverEventKind::Targeted,
            "AXFocusedUIElementChanged" => ObserverEventKind::Ignored,
            _ => ObserverEventKind::FullResync,
        };
        let globally_uncertain = name == "AXCreated";
        let render_id =
            (!element.is_null() && !globally_uncertain).then(|| CFHash(element as CFTypeRef));
        let element_ptr = if kind == ObserverEventKind::Targeted && !element.is_null() {
            CFRetain(element as CFTypeRef);
            element as usize
        } else {
            0
        };
        let event = ObserverEvent {
            epoch,
            kind,
            render_id,
            element_ptr,
        };
        if let Ok(mut events) = state.events.lock() {
            events.push_back(event);
            if events.len() > 512 {
                events.pop_front();
            }
        }
    }
}

struct ProcessObserver {
    state: Arc<ObserverState>,
    reliable: bool,
    run_loop: CFRunLoop,
    thread: Option<thread::JoinHandle<()>>,
}

impl ProcessObserver {
    fn start(pid: i32) -> Result<Self, String> {
        let state = Arc::new(ObserverState::default());
        let thread_state = state.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name(format!("cua-ax-observer-{pid}"))
            .spawn(move || unsafe {
                let application = AXUIElementCreateApplication(pid);
                if application.is_null() {
                    let _ = ready_tx.send(Err("AX application element was null".to_owned()));
                    return;
                }

                let mut observer = ptr::null_mut();
                let create =
                    AXObserverCreate(pid, observer_callback as AXObserverCallback, &mut observer);
                if create != kAXErrorSuccess || observer.is_null() {
                    CFRelease(application as CFTypeRef);
                    let _ = ready_tx.send(Err(format!("AXObserverCreate failed: {create}")));
                    return;
                }

                let mut reliable = true;
                for notification in INVALIDATING_NOTIFICATIONS {
                    let notification = CFString::new(notification);
                    let status = AXObserverAddNotification(
                        observer,
                        application,
                        notification.as_concrete_TypeRef(),
                        Arc::as_ptr(&thread_state) as *mut c_void,
                    );
                    reliable &= status == kAXErrorSuccess
                        || status == kAXErrorNotificationAlreadyRegistered;
                }

                let run_loop = CFRunLoop::get_current();
                let source = AXObserverGetRunLoopSource(observer);
                CFRunLoopAddSource(
                    run_loop.as_concrete_TypeRef(),
                    source,
                    kCFRunLoopDefaultMode,
                );
                thread_state.alive.store(true, Ordering::Release);
                if ready_tx.send(Ok((run_loop.clone(), reliable))).is_ok() {
                    while !thread_state.stop.load(Ordering::Acquire) {
                        CFRunLoop::run_in_mode(
                            kCFRunLoopDefaultMode,
                            std::time::Duration::from_secs(1),
                            false,
                        );
                    }
                }
                thread_state.alive.store(false, Ordering::Release);
                CFRunLoopRemoveSource(
                    run_loop.as_concrete_TypeRef(),
                    source,
                    kCFRunLoopDefaultMode,
                );
                CFRelease(observer as CFTypeRef);
                CFRelease(application as CFTypeRef);
            })
            .map_err(|error| format!("could not spawn AX observer thread: {error}"))?;

        let (run_loop, reliable) = ready_rx
            .recv()
            .map_err(|_| "AX observer thread exited before initialization".to_owned())??;
        Ok(Self {
            state,
            reliable,
            run_loop,
            thread: Some(handle),
        })
    }

    fn epoch(&self) -> u64 {
        self.state.epoch()
    }

    fn invalidations_since(&self, since_epoch: u64) -> InvalidationSince {
        self.state.invalidations_since(since_epoch)
    }

    fn is_reliable(&self) -> bool {
        self.reliable && self.state.alive.load(Ordering::Acquire)
    }
}

impl Drop for ProcessObserver {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        self.run_loop.stop();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone)]
struct CachedWindowTree {
    result: TreeWalkResult,
    observer_epoch: u64,
    action_borrow_epoch: u64,
}

pub struct ProviderTree {
    pub result: TreeWalkResult,
    pub cache_status: &'static str,
}

pub struct MacAxTreeProvider {
    windows: Mutex<HashMap<(i32, u32), CachedWindowTree>>,
    observers: Mutex<HashMap<i32, Arc<ProcessObserver>>>,
    element_cache: Arc<ElementCache>,
}

impl MacAxTreeProvider {
    pub fn new(element_cache: Arc<ElementCache>) -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
            observers: Mutex::new(HashMap::new()),
            element_cache,
        }
    }

    fn observer_for(&self, pid: i32) -> Option<Arc<ProcessObserver>> {
        let mut observers = self.observers.lock().unwrap();
        if let Some(observer) = observers.get(&pid) {
            return Some(observer.clone());
        }
        match ProcessObserver::start(pid) {
            Ok(observer) => {
                let observer = Arc::new(observer);
                observers.insert(pid, observer.clone());
                Some(observer)
            }
            Err(error) => {
                tracing::warn!(pid, %error, "AX notification cache disabled");
                None
            }
        }
    }

    fn cached_window(&self, pid: i32, window_id: u32) -> Option<CachedWindowTree> {
        self.windows.lock().unwrap().get(&(pid, window_id)).cloned()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn observe(
        &self,
        pid: i32,
        window_id: u32,
        query: Option<&str>,
        max_elements: usize,
        max_depth: usize,
        allow_cache_reuse: bool,
        observation_only: bool,
    ) -> ProviderTree {
        let cache_shape = query.is_none()
            && max_elements == DEFAULT_MAX_ELEMENTS
            && max_depth == DEFAULT_MAX_DEPTH
            && !observation_only;
        let observer = cache_shape.then(|| self.observer_for(pid)).flatten();
        let observer_epoch = observer.as_ref().map(|value| value.epoch());
        let action_borrow_epoch = self.element_cache.action_borrow_epoch();

        if allow_cache_reuse {
            if let Some(observer) = &observer {
                if observer.is_reliable() {
                    if let Some(mut cached) = self.cached_window(pid, window_id) {
                        if cached.action_borrow_epoch == action_borrow_epoch {
                            match observer.invalidations_since(cached.observer_epoch) {
                                InvalidationSince::Clean => {
                                    if observer.epoch() == cached.observer_epoch
                                        && self.element_cache.action_borrow_epoch()
                                            == action_borrow_epoch
                                    {
                                        return ProviderTree {
                                            result: cached.result,
                                            cache_status: "clean_cache",
                                        };
                                    }
                                }
                                InvalidationSince::Targeted {
                                    through_epoch,
                                    elements,
                                } => {
                                    if let Ok(refreshed_any) =
                                        apply_invalidations(&mut cached.result, &elements)
                                    {
                                        if observer.epoch() == through_epoch
                                            && self.element_cache.action_borrow_epoch()
                                                == action_borrow_epoch
                                        {
                                            cached.observer_epoch = through_epoch;
                                            self.windows
                                                .lock()
                                                .unwrap()
                                                .insert((pid, window_id), cached.clone());
                                            return ProviderTree {
                                                result: cached.result,
                                                cache_status: if refreshed_any {
                                                    "targeted_refetch"
                                                } else {
                                                    "clean_cache"
                                                },
                                            };
                                        }
                                    }
                                }
                                InvalidationSince::FullResync => {}
                            }
                        }
                    }
                }
            }
        }

        let mut result = walk_tree_bounded(pid, Some(window_id), query, max_elements, max_depth);
        let observer_epoch_after = observer.as_ref().map(|value| value.epoch());
        let cache_epoch = observer.as_ref().and_then(|value| {
            if !value.is_reliable() {
                return None;
            }
            match value.invalidations_since(observer_epoch.unwrap_or_default()) {
                InvalidationSince::Clean => observer_epoch_after,
                InvalidationSince::Targeted {
                    through_epoch,
                    elements,
                } if apply_invalidations(&mut result, &elements).is_ok()
                    && value.epoch() == through_epoch =>
                {
                    Some(through_epoch)
                }
                InvalidationSince::Targeted { .. } | InvalidationSince::FullResync => None,
            }
        });
        let scope_matched = result
            .window_scope
            .as_ref()
            .is_some_and(super::window_scope::WindowScope::is_matched);

        if !observation_only {
            if scope_matched {
                self.element_cache.update(pid, window_id, &result.nodes);
            } else {
                self.element_cache.update(pid, window_id, &[]);
            }
        }

        if cache_shape && scope_matched && !result.truncated && cache_epoch.is_some() {
            self.windows.lock().unwrap().insert(
                (pid, window_id),
                CachedWindowTree {
                    result: result.clone(),
                    observer_epoch: cache_epoch.unwrap_or_default(),
                    action_borrow_epoch: self.element_cache.action_borrow_epoch(),
                },
            );
        } else {
            self.windows.lock().unwrap().remove(&(pid, window_id));
        }

        ProviderTree {
            result,
            cache_status: "full_walk",
        }
    }
}

impl Default for MacAxTreeProvider {
    fn default() -> Self {
        Self::new(Arc::new(ElementCache::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::window_scope::WindowScope;

    #[test]
    fn notification_lineage_targets_only_contiguous_value_events() {
        let state = ObserverState::default();
        assert!(matches!(
            state.invalidations_since(0),
            InvalidationSince::Clean
        ));

        state.events.lock().unwrap().push_back(ObserverEvent {
            epoch: 1,
            kind: ObserverEventKind::Targeted,
            render_id: Some(42),
            element_ptr: 0,
        });
        state.epoch.store(1, Ordering::Release);
        assert!(matches!(
            state.invalidations_since(0),
            InvalidationSince::Targeted {
                through_epoch: 1,
                ref elements
            } if elements.len() == 1 && elements[0].render_id == 42
        ));

        state.events.lock().unwrap().push_back(ObserverEvent {
            epoch: 2,
            kind: ObserverEventKind::FullResync,
            render_id: Some(43),
            element_ptr: 0,
        });
        state.epoch.store(2, Ordering::Release);
        assert!(matches!(
            state.invalidations_since(1),
            InvalidationSince::Targeted {
                through_epoch: 2,
                ref elements
            } if elements.len() == 1 && elements[0].kind == ObserverEventKind::FullResync
        ));

        state.events.lock().unwrap().push_back(ObserverEvent {
            epoch: 3,
            kind: ObserverEventKind::FullResync,
            render_id: None,
            element_ptr: 0,
        });
        state.epoch.store(3, Ordering::Release);
        assert!(matches!(
            state.invalidations_since(2),
            InvalidationSince::FullResync
        ));

        state.events.lock().unwrap().clear();
        state.events.lock().unwrap().push_back(ObserverEvent {
            epoch: 4,
            kind: ObserverEventKind::Targeted,
            render_id: Some(42),
            element_ptr: 0,
        });
        state.epoch.store(4, Ordering::Release);
        assert!(matches!(
            state.invalidations_since(2),
            InvalidationSince::FullResync
        ));
    }

    #[test]
    fn cached_window_read_releases_map_before_publish() {
        let provider = MacAxTreeProvider::default();
        provider.windows.lock().unwrap().insert(
            (7, 11),
            CachedWindowTree {
                result: TreeWalkResult {
                    tree_markdown: String::new(),
                    nodes: Vec::new(),
                    truncated: false,
                    window_scope: Some(WindowScope::Matched),
                },
                observer_epoch: 1,
                action_borrow_epoch: 2,
            },
        );

        let cached = provider.cached_window(7, 11).expect("cached window");
        provider
            .windows
            .try_lock()
            .expect("cache read must release the map lock before publish")
            .insert((7, 11), cached);
    }

    #[test]
    fn structural_event_from_another_window_does_not_evict_exact_cache() {
        let mut result = TreeWalkResult {
            tree_markdown: String::new(),
            nodes: Vec::new(),
            truncated: false,
            window_scope: Some(WindowScope::Matched),
        };
        let event = DirtyElement {
            kind: ObserverEventKind::FullResync,
            render_id: 99,
            element_ptr: 0,
        };

        assert_eq!(apply_invalidations(&mut result, &[event]), Ok(false));
    }
}
