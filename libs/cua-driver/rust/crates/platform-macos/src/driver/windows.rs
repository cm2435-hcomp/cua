use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use core_foundation::base::{CFEqual, CFRelease, CFRetain, CFTypeRef};
use cua_driver_core::api::{
    capabilities::{Framework, WindowStateKind},
    contracts::{AppId, AppRef, GeometryRevision, Rect, WindowGeneration, WindowId, WindowRef},
    errors::{ErrorCode, ErrorPhase, NativeError},
    observation::{
        NativeProcessHandle, NativeWindowHandle, ResolvedWindow, ResolvedWindowStamp,
        WindowGeometry,
    },
    platform::{TargetInvalidation, WindowProvider},
};

use crate::{
    apps::nsworkspace::{self, RunningApplicationInfo},
    ax::bindings::{self, copy_ax_windows, AXUIElementCreateApplication, AXUIElementRef},
    permissions, windows,
};

use super::target::MacInvalidationHub;

const MAX_WINDOW_TOMBSTONES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NativeWindowKey {
    pid: i32,
    process_generation: u64,
    cg_window_id: u32,
}

#[derive(Debug, Clone)]
struct ProcessSnapshot {
    pid: i32,
    generation: u64,
    name: Option<String>,
    bundle_id: Option<String>,
    hidden: bool,
}

#[derive(Debug)]
struct RetainedAxWindow(usize);

impl RetainedAxWindow {
    unsafe fn from_owned(element: AXUIElementRef) -> Self {
        Self(element as usize)
    }

    unsafe fn from_borrowed(element: AXUIElementRef) -> Self {
        CFRetain(element as CFTypeRef);
        Self(element as usize)
    }

    fn same_identity(&self, other: &Self) -> bool {
        unsafe { CFEqual(self.0 as CFTypeRef, other.0 as CFTypeRef) != 0 }
    }

    fn as_ptr(&self) -> AXUIElementRef {
        self.0 as AXUIElementRef
    }
}

impl Clone for RetainedAxWindow {
    fn clone(&self) -> Self {
        unsafe { CFRetain(self.0 as CFTypeRef) };
        Self(self.0)
    }
}

unsafe impl Send for RetainedAxWindow {}
unsafe impl Sync for RetainedAxWindow {}

type AxWindowMatches = HashMap<u32, Vec<(RetainedAxWindow, Option<bool>)>>;

impl Drop for RetainedAxWindow {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

#[derive(Debug, Clone)]
struct NativeWindowSnapshot {
    key: NativeWindowKey,
    process: ProcessSnapshot,
    owner_name: String,
    title: Option<String>,
    bounds: Rect,
    layer: i32,
    z_index: usize,
    is_on_screen: bool,
    on_current_space: Option<bool>,
    space_ids: Option<Vec<u64>>,
    minimized: Option<bool>,
    scale_factor: Option<f64>,
    ax_identity: RetainedAxWindow,
}

/// Exact native facts for route preflight and observation setup. Callers get
/// these only through `facts_for_stamp`, which first revalidates every stable
/// identity component and the geometry revision against the live registry.
#[derive(Debug, Clone, PartialEq)]
pub struct MacWindowFacts {
    pub stamp: ResolvedWindowStamp,
    pub pid: i32,
    pub process_generation: u64,
    pub cg_window_id: u32,
    pub owner_name: String,
    pub layer: i32,
    pub bounds: Rect,
    pub scale_factor: Option<f64>,
    pub state: WindowStateKind,
    pub is_on_screen: bool,
    pub on_current_space: Option<bool>,
    pub space_ids: Option<Vec<u64>>,
    pub minimized: Option<bool>,
}

/// Exact registered identity for an AX-owned popover or sheet related to a
/// public target window. These entries never appear in public window listing;
/// they exist only so captured related pixels can be revalidated before use.
#[derive(Debug, Clone, PartialEq)]
pub struct MacRelatedWindowFacts {
    pub public: WindowRef,
    pub stamp: ResolvedWindowStamp,
    pub parent: ResolvedWindowStamp,
    pub pid: i32,
    pub process_generation: u64,
    pub cg_window_id: u32,
    pub bounds: Rect,
    pub scale_factor: f64,
    pub layer: i32,
    pub is_on_screen: bool,
}

trait WindowSnapshotSource: Send + Sync {
    fn snapshot(&self) -> Result<Vec<NativeWindowSnapshot>, NativeError>;
}

#[derive(Default)]
struct SystemWindowSnapshotSource;

impl WindowSnapshotSource for SystemWindowSnapshotSource {
    fn snapshot(&self) -> Result<Vec<NativeWindowSnapshot>, NativeError> {
        if !permissions::status::accessibility_granted() {
            return Err(NativeError::new(
                ErrorCode::PermissionDenied,
                ErrorPhase::Preflight,
                true,
                "Accessibility permission is required to establish exact macOS window identity",
            ));
        }

        match self.snapshot_once() {
            Ok(snapshots) => Ok(snapshots),
            Err(_) => self.snapshot_once().map_err(|racing_pids| {
                NativeError::new(
                    ErrorCode::WindowIdentityChanged,
                    ErrorPhase::Preflight,
                    true,
                    "process identity changed while joining WindowServer and Accessibility windows",
                )
                .with_detail(
                    "pids",
                    serde_json::to_value(racing_pids).unwrap_or_default(),
                )
            }),
        }
    }
}

impl SystemWindowSnapshotSource {
    fn snapshot_once(&self) -> Result<Vec<NativeWindowSnapshot>, Vec<i32>> {
        let applications_before: HashMap<i32, RunningApplicationInfo> =
            nsworkspace::running_applications()
                .into_iter()
                .map(|application| (application.pid, application))
                .collect();
        let server_windows = windows::all_windows();
        let mut ax_by_pid: HashMap<i32, AxWindowMatches> = HashMap::new();
        let mut snapshots = Vec::new();

        for window in &server_windows {
            let Some(application) = applications_before.get(&window.pid) else {
                continue;
            };
            let Some(process_generation) = application.process_generation else {
                continue;
            };
            let matches = ax_by_pid
                .entry(window.pid)
                .or_insert_with(|| ax_windows_for_process(window.pid));
            let Some(candidates) = matches.get(&window.window_id) else {
                continue;
            };
            if candidates.len() != 1 {
                tracing::warn!(
                    pid = window.pid,
                    window_id = window.window_id,
                    candidate_count = candidates.len(),
                    "ambiguous AX-to-WindowServer identity; omitting window"
                );
                continue;
            }
            let (ax_identity, minimized) = &candidates[0];
            let space = windows::space_facts(window.window_id);
            let scale_factor = windows::display_scale_for_bounds(&window.bounds);
            snapshots.push(NativeWindowSnapshot {
                key: NativeWindowKey {
                    pid: window.pid,
                    process_generation,
                    cg_window_id: window.window_id,
                },
                process: ProcessSnapshot {
                    pid: window.pid,
                    generation: process_generation,
                    name: application.name.clone(),
                    bundle_id: application.bundle_id.clone(),
                    hidden: application.hidden,
                },
                owner_name: window.app_name.clone(),
                title: (!window.title.is_empty()).then_some(window.title.clone()),
                bounds: Rect {
                    x: window.bounds.x,
                    y: window.bounds.y,
                    width: window.bounds.width,
                    height: window.bounds.height,
                },
                layer: window.layer,
                z_index: window.z_index,
                is_on_screen: window.is_on_screen,
                on_current_space: space.as_ref().map(|facts| facts.on_current_space),
                space_ids: space.map(|facts| facts.space_ids),
                minimized: *minimized,
                scale_factor,
                ax_identity: ax_identity.clone(),
            });
        }
        let applications_after: HashMap<i32, RunningApplicationInfo> =
            nsworkspace::running_applications()
                .into_iter()
                .map(|application| (application.pid, application))
                .collect();
        let racing_pids = racing_process_pids(
            server_windows.iter().map(|window| window.pid),
            &applications_before,
            &applications_after,
        );
        if racing_pids.is_empty() {
            Ok(snapshots)
        } else {
            Err(racing_pids)
        }
    }
}

fn racing_process_pids(
    window_pids: impl IntoIterator<Item = i32>,
    before: &HashMap<i32, RunningApplicationInfo>,
    after: &HashMap<i32, RunningApplicationInfo>,
) -> Vec<i32> {
    let mut racing_pids = HashSet::new();
    for pid in window_pids {
        let before = before.get(&pid);
        let after = after.get(&pid);
        if before.is_none() && after.is_none() {
            continue;
        }
        let stable = matches!(
            (before, after),
            (Some(before), Some(after))
                if before.process_generation.is_some()
                    && before.process_generation == after.process_generation
        );
        if !stable {
            racing_pids.insert(pid);
        }
    }
    let mut racing_pids: Vec<_> = racing_pids.into_iter().collect();
    racing_pids.sort_unstable();
    racing_pids
}

fn ax_windows_for_process(pid: i32) -> AxWindowMatches {
    let application = unsafe { AXUIElementCreateApplication(pid) };
    if application.is_null() {
        return HashMap::new();
    }
    let elements = unsafe { copy_ax_windows(application) };
    unsafe { CFRelease(application as CFTypeRef) };
    let mut result = AxWindowMatches::new();
    for element in elements {
        let Some(window_id) = (unsafe { bindings::ax_get_window_id(element) }) else {
            unsafe { CFRelease(element as CFTypeRef) };
            continue;
        };
        let minimized = unsafe { bindings::copy_bool_attr(element, "AXMinimized") };
        let retained = unsafe { RetainedAxWindow::from_owned(element) };
        result
            .entry(window_id)
            .or_default()
            .push((retained, minimized));
    }
    result
}

#[derive(Debug, Clone)]
struct RegistryEntry {
    public: WindowRef,
    key: NativeWindowKey,
    process: NativeProcessHandle,
    native: NativeWindowHandle,
    generation: WindowGeneration,
    geometry_revision: GeometryRevision,
    snapshot: NativeWindowSnapshot,
}

#[derive(Debug, Clone, Copy)]
enum WindowTombstone {
    Missing,
    IdentityChanged,
}

#[derive(Debug, Clone)]
struct RelatedRegistryEntry {
    public: WindowRef,
    key: NativeWindowKey,
    process: NativeProcessHandle,
    native: NativeWindowHandle,
    generation: WindowGeneration,
    geometry_revision: GeometryRevision,
    parent: ResolvedWindowStamp,
    bounds: Rect,
    scale_factor: f64,
    layer: i32,
    is_on_screen: bool,
    ax_identity: RetainedAxWindow,
}

#[derive(Default)]
struct RegistryState {
    next_generation: u64,
    by_id: HashMap<WindowId, RegistryEntry>,
    by_native: HashMap<NativeWindowKey, WindowId>,
    related_by_id: HashMap<WindowId, RelatedRegistryEntry>,
    related_by_native: HashMap<NativeWindowKey, WindowId>,
    tombstones: HashMap<WindowId, WindowTombstone>,
    tombstone_order: VecDeque<WindowId>,
}

impl RegistryState {
    fn tombstone(&mut self, id: WindowId, reason: WindowTombstone) {
        self.tombstone_order.retain(|candidate| candidate != &id);
        self.tombstones.insert(id.clone(), reason);
        self.tombstone_order.push_back(id);
        while self.tombstones.len() > MAX_WINDOW_TOMBSTONES {
            if let Some(expired) = self.tombstone_order.pop_front() {
                self.tombstones.remove(&expired);
            }
        }
    }

    fn remove_related_for_parent(&mut self, parent: &WindowId, generation: WindowGeneration) {
        let stale: Vec<_> = self
            .related_by_id
            .iter()
            .filter(|(_, entry)| {
                &entry.parent.window_id == parent && entry.parent.generation == generation
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some(entry) = self.related_by_id.remove(&id) {
                self.related_by_native.remove(&entry.key);
            }
        }
    }
}

#[derive(Clone)]
pub struct MacWindowRegistry {
    state: Arc<Mutex<RegistryState>>,
    source: Arc<dyn WindowSnapshotSource>,
    invalidations: MacInvalidationHub,
}

impl MacWindowRegistry {
    pub fn new(invalidations: MacInvalidationHub) -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState::default())),
            source: Arc::new(SystemWindowSnapshotSource),
            invalidations,
        }
    }

    #[cfg(test)]
    fn with_source(
        source: Arc<dyn WindowSnapshotSource>,
        invalidations: MacInvalidationHub,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState::default())),
            source,
            invalidations,
        }
    }

    fn refresh(&self) -> Result<(), NativeError> {
        let snapshots = self.source.snapshot()?;
        let mut state = self
            .state
            .lock()
            .expect("macOS window registry lock poisoned");
        let current_keys: HashSet<_> = snapshots
            .iter()
            .map(|snapshot| snapshot.key.clone())
            .collect();

        let missing: Vec<_> = state
            .by_native
            .keys()
            .filter(|key| !current_keys.contains(*key))
            .cloned()
            .collect();
        for key in missing {
            if let Some(id) = state.by_native.remove(&key) {
                if let Some(entry) = state.by_id.remove(&id) {
                    self.invalidations
                        .publish(TargetInvalidation::WindowGenerationChanged {
                            app_id: entry.public.app.id.clone(),
                            window_id: entry.public.id.clone(),
                            previous: entry.generation,
                            current: WindowGeneration(entry.generation.0.saturating_add(1)),
                        });
                    state.remove_related_for_parent(&entry.public.id, entry.generation);
                    state.tombstone(id, WindowTombstone::Missing);
                }
            }
        }

        for snapshot in snapshots {
            if let Some(existing_id) = state.by_native.get(&snapshot.key).cloned() {
                let same_identity = state.by_id.get(&existing_id).is_some_and(|entry| {
                    entry
                        .snapshot
                        .ax_identity
                        .same_identity(&snapshot.ax_identity)
                });
                if same_identity {
                    let entry = state
                        .by_id
                        .get_mut(&existing_id)
                        .expect("native index points to entry");
                    if entry.snapshot.bounds != snapshot.bounds
                        || entry.snapshot.scale_factor != snapshot.scale_factor
                    {
                        entry.geometry_revision = GeometryRevision::new();
                    }
                    entry.public.title = snapshot.title.clone();
                    entry.snapshot = snapshot;
                    continue;
                }
                state.by_native.remove(&snapshot.key);
                if let Some(entry) = state.by_id.remove(&existing_id) {
                    self.invalidations
                        .publish(TargetInvalidation::WindowGenerationChanged {
                            app_id: entry.public.app.id.clone(),
                            window_id: entry.public.id.clone(),
                            previous: entry.generation,
                            current: WindowGeneration(entry.generation.0.saturating_add(1)),
                        });
                    state.remove_related_for_parent(&entry.public.id, entry.generation);
                    state.tombstone(existing_id, WindowTombstone::IdentityChanged);
                }
            }

            state.next_generation = state.next_generation.saturating_add(1).max(1);
            let generation = WindowGeneration(state.next_generation);
            let id = WindowId::new();
            let app = app_ref_for_process(&snapshot.process);
            let public = WindowRef {
                id: id.clone(),
                app,
                title: snapshot.title.clone(),
            };
            let process = NativeProcessHandle::new(format!(
                "macos:{}:{:016x}",
                snapshot.key.pid, snapshot.key.process_generation
            ))?;
            let native = NativeWindowHandle::new(format!(
                "macos:{}:{:016x}:{}",
                snapshot.key.pid, snapshot.key.process_generation, snapshot.key.cg_window_id
            ))?;
            state.by_native.insert(snapshot.key.clone(), id.clone());
            state.by_id.insert(
                id,
                RegistryEntry {
                    public,
                    key: snapshot.key.clone(),
                    process,
                    native,
                    generation,
                    geometry_revision: GeometryRevision::new(),
                    snapshot,
                },
            );
        }
        Ok(())
    }

    fn list(&self, app: Option<&AppRef>) -> Result<Vec<WindowRef>, NativeError> {
        self.refresh()?;
        let state = self
            .state
            .lock()
            .expect("macOS window registry lock poisoned");
        let mut windows: Vec<_> = state
            .by_id
            .values()
            .filter(|entry| app.is_none_or(|app| entry.public.app.id == app.id))
            .map(|entry| entry.public.clone())
            .collect();
        windows.sort_by_key(|window| {
            state
                .by_id
                .get(&window.id)
                .map(|entry| std::cmp::Reverse(entry.snapshot.z_index))
        });
        Ok(windows)
    }

    fn entry(&self, id: &WindowId, app: Option<&AppRef>) -> Result<RegistryEntry, NativeError> {
        self.refresh()?;
        let state = self
            .state
            .lock()
            .expect("macOS window registry lock poisoned");
        let entry = match state.by_id.get(id) {
            Some(entry) => entry,
            None => {
                let (code, message) = match state.tombstones.get(id) {
                    Some(WindowTombstone::IdentityChanged) => (
                        ErrorCode::WindowIdentityChanged,
                        "native window identity changed",
                    ),
                    Some(WindowTombstone::Missing) => (
                        ErrorCode::WindowNotFound,
                        "window closed or disappeared from WindowServer",
                    ),
                    None => (
                        ErrorCode::WindowNotFound,
                        "window id is not known to the macOS registry",
                    ),
                };
                return Err(NativeError::new(code, ErrorPhase::Preflight, true, message)
                    .with_detail("window_id", id.to_string()));
            }
        };
        if let Some(app) = app {
            if entry.public.app.id != app.id {
                return Err(identity_error(entry, "window belongs to a different app"));
            }
        }
        Ok(entry.clone())
    }

    /// Rehydrate native facts from an exact core observation stamp. A caller
    /// may not silently roll a stale observation forward across replacement,
    /// process reuse, or geometry changes.
    pub async fn facts_for_stamp(
        &self,
        stamp: &ResolvedWindowStamp,
    ) -> Result<MacWindowFacts, NativeError> {
        let facts = self.facts_for_identity(stamp).await?;
        if facts.stamp.geometry_revision != stamp.geometry_revision {
            return Err(NativeError::stale(
                ErrorCode::ObservationStale,
                "window geometry changed after the observation stamp was issued",
            )
            .with_detail("window_id", stamp.window_id.to_string())
            .with_detail(
                "expected_geometry_revision",
                stamp.geometry_revision.to_string(),
            )
            .with_detail(
                "current_geometry_revision",
                facts.stamp.geometry_revision.to_string(),
            ));
        }
        Ok(facts)
    }

    /// Revalidate the stable native identity while allowing the caller to
    /// observe a newer geometry revision. This is used to bracket capture;
    /// input routes continue to require `facts_for_stamp`.
    pub async fn facts_for_identity(
        &self,
        stamp: &ResolvedWindowStamp,
    ) -> Result<MacWindowFacts, NativeError> {
        let registry = self.clone();
        let stamp = stamp.clone();
        tokio::task::spawn_blocking(move || {
            let entry = registry.entry(&stamp.window_id, None)?;
            if entry.public.app.id != stamp.app_id
                || entry.generation != stamp.generation
                || entry.native != stamp.native_window
                || entry.process != stamp.process
            {
                return Err(identity_error(
                    &entry,
                    "resolved window stamp no longer identifies this native window",
                ));
            }
            Ok(facts_for_entry(&entry))
        })
        .await
        .map_err(join_error)?
    }

    /// Register the complete current set of AX-descendant related surfaces for
    /// one exact target. The join is bracketed by process generation, requires
    /// an exact AX native window id and WindowServer row, and retries once as a
    /// whole. Missing/dismissed/reused candidates are never rolled forward.
    pub async fn register_related_windows(
        &self,
        parent: &MacWindowFacts,
        candidates: Vec<(u32, usize)>,
    ) -> Result<Vec<MacRelatedWindowFacts>, NativeError> {
        let current_parent = self.facts_for_identity(&parent.stamp).await?;
        if current_parent != *parent {
            return Err(NativeError::stale(
                ErrorCode::ObservationRaced,
                "target facts changed before related-window registration",
            ));
        }
        let registry = self.clone();
        let parent = parent.clone();
        tokio::task::spawn_blocking(move || {
            let snapshots = match related_snapshots_once(&parent, &candidates) {
                Ok(snapshots) => snapshots,
                Err(_) => related_snapshots_once(&parent, &candidates).map_err(|message| {
                    NativeError::new(
                        ErrorCode::ObservationRaced,
                        ErrorPhase::Preflight,
                        true,
                        format!(
                            "related AX/WindowServer ownership changed twice during registration: {message}"
                        ),
                    )
                })?,
            };
            registry.replace_related(&parent, snapshots)
        })
        .await
        .map_err(join_error)?
    }

    /// Revalidate a captured related transient immediately before it is used.
    /// The parent stamp, retained AX identity, process generation, native
    /// WindowServer id, geometry and display scale must all still match the
    /// observation. A dismissed, moved or id-reused transient is stale; it is
    /// never rolled forward to the new native object.
    pub async fn facts_for_related_stamp(
        &self,
        stamp: &ResolvedWindowStamp,
        parent: &ResolvedWindowStamp,
    ) -> Result<MacRelatedWindowFacts, NativeError> {
        let parent_facts = self.facts_for_stamp(parent).await?;
        let registry = self.clone();
        let stamp = stamp.clone();
        let parent = parent.clone();
        tokio::task::spawn_blocking(move || {
            let entry = {
                let state = registry
                    .state
                    .lock()
                    .expect("macOS window registry lock poisoned");
                state.related_by_id.get(&stamp.window_id).cloned()
            }
            .ok_or_else(|| related_stale(&stamp, "related window is not registered"))?;
            if related_stamp_for_entry(&entry) != stamp || entry.parent != parent {
                return Err(related_stale(
                    &stamp,
                    "related window stamp or parent ownership changed",
                ));
            }
            let candidates = vec![(entry.key.cg_window_id, entry.ax_identity.as_ptr() as usize)];
            let mut snapshots = related_snapshots_once(&parent_facts, &candidates)
                .map_err(|message| related_stale(&stamp, message))?;
            if snapshots.len() != 1 || !related_snapshot_matches_entry(&snapshots.remove(0), &entry)
            {
                return Err(related_stale(
                    &stamp,
                    "related window identity, geometry or display scale changed",
                ));
            }
            Ok(related_facts_for_entry(&entry))
        })
        .await
        .map_err(join_error)?
    }

    fn replace_related(
        &self,
        parent: &MacWindowFacts,
        snapshots: Vec<RelatedNativeSnapshot>,
    ) -> Result<Vec<MacRelatedWindowFacts>, NativeError> {
        let live_keys: HashSet<_> = snapshots
            .iter()
            .map(|snapshot| snapshot.key.clone())
            .collect();
        let mut state = self
            .state
            .lock()
            .expect("macOS window registry lock poisoned");
        let stale_ids: Vec<_> = state
            .related_by_id
            .iter()
            .filter(|(_, entry)| {
                same_stable_stamp(&entry.parent, &parent.stamp) && !live_keys.contains(&entry.key)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale_ids {
            if let Some(entry) = state.related_by_id.remove(&id) {
                state.related_by_native.remove(&entry.key);
                self.invalidations
                    .publish(TargetInvalidation::ObservationChanged {
                    app_id: parent.stamp.app_id.clone(),
                    window_id: parent.stamp.window_id.clone(),
                    generation: parent.stamp.generation,
                    reason:
                        cua_driver_core::api::observation::InvalidationReason::TransientDismissed,
                });
            }
        }

        let mut result = Vec::with_capacity(snapshots.len());
        for snapshot in snapshots {
            let existing_id = state.related_by_native.get(&snapshot.key).cloned();
            if let Some(existing_id) = existing_id {
                let same_identity = state
                    .related_by_id
                    .get(&existing_id)
                    .is_some_and(|entry| entry.ax_identity.same_identity(&snapshot.ax_identity));
                if same_identity {
                    let entry = state
                        .related_by_id
                        .get_mut(&existing_id)
                        .expect("related native index points to entry");
                    if entry.bounds != snapshot.bounds
                        || entry.scale_factor != snapshot.scale_factor
                    {
                        entry.geometry_revision = GeometryRevision::new();
                    }
                    entry.public.title = snapshot.title;
                    entry.parent = parent.stamp.clone();
                    entry.bounds = snapshot.bounds;
                    entry.scale_factor = snapshot.scale_factor;
                    entry.layer = snapshot.layer;
                    entry.is_on_screen = snapshot.is_on_screen;
                    entry.ax_identity = snapshot.ax_identity;
                    result.push(related_facts_for_entry(entry));
                    continue;
                }
                state.related_by_native.remove(&snapshot.key);
                state.related_by_id.remove(&existing_id);
            }

            state.next_generation = state.next_generation.saturating_add(1).max(1);
            let generation = WindowGeneration(state.next_generation);
            let id = WindowId::new();
            let public = WindowRef {
                id: id.clone(),
                app: AppRef {
                    id: parent.stamp.app_id.clone(),
                    name: Some(parent.owner_name.clone()),
                    pid: u32::try_from(parent.pid).ok(),
                    running: true,
                },
                title: snapshot.title,
            };
            let process = parent.stamp.process.clone();
            let native = NativeWindowHandle::new(format!(
                "macos:{}:{:016x}:{}",
                snapshot.key.pid, snapshot.key.process_generation, snapshot.key.cg_window_id
            ))?;
            let entry = RelatedRegistryEntry {
                public,
                key: snapshot.key.clone(),
                process,
                native,
                generation,
                geometry_revision: GeometryRevision::new(),
                parent: parent.stamp.clone(),
                bounds: snapshot.bounds,
                scale_factor: snapshot.scale_factor,
                layer: snapshot.layer,
                is_on_screen: snapshot.is_on_screen,
                ax_identity: snapshot.ax_identity,
            };
            result.push(related_facts_for_entry(&entry));
            state.related_by_native.insert(snapshot.key, id.clone());
            state.related_by_id.insert(id, entry);
        }
        result.sort_by_key(|facts| facts.cg_window_id);
        Ok(result)
    }
}

struct RelatedNativeSnapshot {
    key: NativeWindowKey,
    title: Option<String>,
    bounds: Rect,
    scale_factor: f64,
    layer: i32,
    is_on_screen: bool,
    ax_identity: RetainedAxWindow,
}

fn related_snapshots_once(
    parent: &MacWindowFacts,
    candidates: &[(u32, usize)],
) -> Result<Vec<RelatedNativeSnapshot>, String> {
    let application_before = nsworkspace::running_applications()
        .into_iter()
        .find(|application| application.pid == parent.pid)
        .ok_or_else(|| "owner process disappeared before related-window join".to_owned())?;
    if application_before.process_generation != Some(parent.process_generation) {
        return Err("owner process generation changed before related-window join".to_owned());
    }
    let server_windows = windows::all_windows_including_transients();
    let mut seen = HashSet::new();
    let mut snapshots = Vec::with_capacity(candidates.len());
    for (window_id, element) in candidates {
        if !seen.insert(*window_id) {
            return Err(format!("duplicate related AX window id {window_id}"));
        }
        let element = *element as AXUIElementRef;
        if element.is_null() || unsafe { bindings::ax_get_window_id(element) } != Some(*window_id) {
            return Err(format!(
                "related AX identity no longer maps to window {window_id}"
            ));
        }
        let role = unsafe { bindings::copy_string_attr(element, "AXRole") };
        if !matches!(role.as_deref(), Some("AXPopover" | "AXSheet")) {
            return Err(format!(
                "related window {window_id} is not an AX popover or sheet"
            ));
        }
        let matches: Vec<_> = server_windows
            .iter()
            .filter(|window| window.pid == parent.pid && window.window_id == *window_id)
            .collect();
        if matches.len() != 1 {
            return Err(format!(
                "related window {window_id} has {} WindowServer rows",
                matches.len()
            ));
        }
        let window = matches[0];
        let scale_factor = windows::display_scale_for_bounds(&window.bounds)
            .ok_or_else(|| format!("related window {window_id} has no backing scale"))?;
        let bounds = Rect {
            x: window.bounds.x,
            y: window.bounds.y,
            width: window.bounds.width,
            height: window.bounds.height,
        };
        bounds
            .validate()
            .map_err(|error| format!("related window {window_id}: {error}"))?;
        snapshots.push(RelatedNativeSnapshot {
            key: NativeWindowKey {
                pid: parent.pid,
                process_generation: parent.process_generation,
                cg_window_id: *window_id,
            },
            title: (!window.title.is_empty()).then_some(window.title.clone()),
            bounds,
            scale_factor,
            layer: window.layer,
            is_on_screen: window.is_on_screen,
            ax_identity: unsafe { RetainedAxWindow::from_borrowed(element) },
        });
    }
    let application_after = nsworkspace::running_applications()
        .into_iter()
        .find(|application| application.pid == parent.pid)
        .ok_or_else(|| "owner process disappeared after related-window join".to_owned())?;
    if application_after.process_generation != Some(parent.process_generation) {
        return Err("owner process generation changed during related-window join".to_owned());
    }
    Ok(snapshots)
}

fn same_stable_stamp(left: &ResolvedWindowStamp, right: &ResolvedWindowStamp) -> bool {
    left.app_id == right.app_id
        && left.window_id == right.window_id
        && left.generation == right.generation
        && left.native_window == right.native_window
        && left.process == right.process
}

fn related_facts_for_entry(entry: &RelatedRegistryEntry) -> MacRelatedWindowFacts {
    MacRelatedWindowFacts {
        public: entry.public.clone(),
        stamp: ResolvedWindowStamp {
            app_id: entry.public.app.id.clone(),
            window_id: entry.public.id.clone(),
            generation: entry.generation,
            geometry_revision: entry.geometry_revision.clone(),
            native_window: entry.native.clone(),
            process: entry.process.clone(),
        },
        parent: entry.parent.clone(),
        pid: entry.key.pid,
        process_generation: entry.key.process_generation,
        cg_window_id: entry.key.cg_window_id,
        bounds: entry.bounds,
        scale_factor: entry.scale_factor,
        layer: entry.layer,
        is_on_screen: entry.is_on_screen,
    }
}

fn related_stamp_for_entry(entry: &RelatedRegistryEntry) -> ResolvedWindowStamp {
    ResolvedWindowStamp {
        app_id: entry.public.app.id.clone(),
        window_id: entry.public.id.clone(),
        generation: entry.generation,
        geometry_revision: entry.geometry_revision.clone(),
        native_window: entry.native.clone(),
        process: entry.process.clone(),
    }
}

fn related_snapshot_matches_entry(
    snapshot: &RelatedNativeSnapshot,
    entry: &RelatedRegistryEntry,
) -> bool {
    snapshot.key == entry.key
        && snapshot.ax_identity.same_identity(&entry.ax_identity)
        && snapshot.bounds == entry.bounds
        && snapshot.scale_factor == entry.scale_factor
        && snapshot.layer == entry.layer
        && snapshot.is_on_screen == entry.is_on_screen
}

fn related_stale(stamp: &ResolvedWindowStamp, message: impl Into<String>) -> NativeError {
    NativeError::stale(ErrorCode::SurfaceStale, message)
        .with_detail("window_id", stamp.window_id.to_string())
}

#[async_trait]
impl WindowProvider for MacWindowRegistry {
    async fn list_windows(&self, app: Option<&AppRef>) -> Result<Vec<WindowRef>, NativeError> {
        let registry = self.clone();
        let app = app.cloned();
        tokio::task::spawn_blocking(move || registry.list(app.as_ref()))
            .await
            .map_err(join_error)?
    }

    async fn rehydrate(
        &self,
        id: &WindowId,
        app: Option<&AppRef>,
    ) -> Result<WindowRef, NativeError> {
        let registry = self.clone();
        let id = id.clone();
        let app = app.cloned();
        tokio::task::spawn_blocking(move || {
            registry.entry(&id, app.as_ref()).map(|entry| entry.public)
        })
        .await
        .map_err(join_error)?
    }

    async fn resolve(&self, window: &WindowRef) -> Result<ResolvedWindow, NativeError> {
        let registry = self.clone();
        let window = window.clone();
        tokio::task::spawn_blocking(move || {
            let entry = registry.entry(&window.id, Some(&window.app))?;
            let scale_factor = entry.snapshot.scale_factor.ok_or_else(|| {
                NativeError::unsupported(
                    "window does not intersect a display with a known backing scale",
                )
                .with_detail("window_id", entry.public.id.to_string())
            })?;
            Ok(ResolvedWindow {
                public: entry.public,
                native: entry.native,
                process: entry.process,
                framework: classify_framework(entry.snapshot.process.bundle_id.as_deref()),
                geometry: WindowGeometry {
                    bounds: entry.snapshot.bounds,
                    scale_factor,
                    revision: entry.geometry_revision,
                },
                generation: entry.generation,
                state: window_state(&entry.snapshot),
            })
        })
        .await
        .map_err(join_error)?
    }
}

fn app_ref_for_process(process: &ProcessSnapshot) -> AppRef {
    let identity = format!("macos:process:{}:{:016x}", process.pid, process.generation);
    AppRef {
        id: AppId::parse(identity).expect("constructed macOS app id is nonempty"),
        name: process.name.clone(),
        pid: u32::try_from(process.pid).ok(),
        running: true,
    }
}

fn classify_framework(bundle_id: Option<&str>) -> Framework {
    match bundle_id {
        Some(
            "com.google.Chrome"
            | "com.google.Chrome.beta"
            | "com.google.Chrome.dev"
            | "com.google.Chrome.canary"
            | "org.chromium.Chromium"
            | "com.microsoft.edgemac"
            | "com.microsoft.edgemac.Beta"
            | "com.microsoft.edgemac.Dev"
            | "com.microsoft.edgemac.Canary"
            | "com.brave.Browser"
            | "com.brave.Browser.beta"
            | "com.brave.Browser.nightly",
        ) => Framework::Chromium,
        Some("com.apple.Safari" | "com.apple.SafariTechnologyPreview") => Framework::WebKit,
        _ => Framework::Unknown,
    }
}

fn window_state(snapshot: &NativeWindowSnapshot) -> WindowStateKind {
    if snapshot.minimized == Some(true) {
        WindowStateKind::Minimized
    } else if snapshot.minimized.is_none() {
        WindowStateKind::Unknown
    } else if snapshot.on_current_space == Some(false) {
        WindowStateKind::OffSpace
    } else if snapshot.process.hidden || snapshot.on_current_space.is_none() {
        WindowStateKind::Unknown
    } else if snapshot.is_on_screen {
        WindowStateKind::Visible
    } else {
        WindowStateKind::Unknown
    }
}

fn facts_for_entry(entry: &RegistryEntry) -> MacWindowFacts {
    MacWindowFacts {
        stamp: ResolvedWindowStamp {
            app_id: entry.public.app.id.clone(),
            window_id: entry.public.id.clone(),
            generation: entry.generation,
            geometry_revision: entry.geometry_revision.clone(),
            native_window: entry.native.clone(),
            process: entry.process.clone(),
        },
        pid: entry.key.pid,
        process_generation: entry.key.process_generation,
        cg_window_id: entry.key.cg_window_id,
        owner_name: entry.snapshot.owner_name.clone(),
        layer: entry.snapshot.layer,
        bounds: entry.snapshot.bounds,
        scale_factor: entry.snapshot.scale_factor,
        state: window_state(&entry.snapshot),
        is_on_screen: entry.snapshot.is_on_screen,
        on_current_space: entry.snapshot.on_current_space,
        space_ids: entry.snapshot.space_ids.clone(),
        minimized: entry.snapshot.minimized,
    }
}

fn identity_error(entry: &RegistryEntry, message: &str) -> NativeError {
    NativeError::new(
        ErrorCode::WindowIdentityChanged,
        ErrorPhase::Preflight,
        false,
        message,
    )
    .with_detail("window_id", entry.public.id.to_string())
    .with_detail("pid", entry.key.pid)
    .with_detail("cg_window_id", entry.key.cg_window_id)
    .with_detail("generation", entry.generation.0)
}

fn join_error(error: tokio::task::JoinError) -> NativeError {
    NativeError::new(
        ErrorCode::Internal,
        ErrorPhase::Preflight,
        true,
        format!("macOS blocking lifecycle task failed: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use core_foundation::{base::TCFType, string::CFString};
    use cua_driver_core::api::{
        contracts::{AppId, AppRef},
        errors::ErrorCode,
        platform::{InvalidationSubscription, TargetInvalidation, WindowProvider},
    };

    use super::*;

    #[derive(Default)]
    struct FakeSnapshotSource {
        snapshots: Mutex<Vec<NativeWindowSnapshot>>,
    }

    impl FakeSnapshotSource {
        fn replace(&self, snapshots: Vec<NativeWindowSnapshot>) {
            *self.snapshots.lock().expect("fake source lock poisoned") = snapshots;
        }
    }

    impl WindowSnapshotSource for FakeSnapshotSource {
        fn snapshot(&self) -> Result<Vec<NativeWindowSnapshot>, NativeError> {
            Ok(self
                .snapshots
                .lock()
                .expect("fake source lock poisoned")
                .clone())
        }
    }

    fn ax_identity(label: &str) -> RetainedAxWindow {
        let value = CFString::new(label);
        let raw = value.as_CFTypeRef();
        unsafe { CFRetain(raw) };
        unsafe { RetainedAxWindow::from_owned(raw.cast_mut().cast()) }
    }

    fn snapshot(
        bundle_id: &str,
        process_generation: u64,
        cg_window_id: u32,
        title: Option<&str>,
        ax_label: &str,
    ) -> NativeWindowSnapshot {
        NativeWindowSnapshot {
            key: NativeWindowKey {
                pid: 101,
                process_generation,
                cg_window_id,
            },
            process: ProcessSnapshot {
                pid: 101,
                generation: process_generation,
                name: Some("Fixture".to_owned()),
                bundle_id: Some(bundle_id.to_owned()),
                hidden: false,
            },
            owner_name: "Fixture".to_owned(),
            title: title.map(str::to_owned),
            bounds: Rect {
                x: 10.0,
                y: 20.0,
                width: 800.0,
                height: 600.0,
            },
            layer: 0,
            z_index: 4,
            is_on_screen: true,
            on_current_space: Some(true),
            space_ids: Some(vec![7]),
            minimized: Some(false),
            scale_factor: Some(2.0),
            ax_identity: ax_identity(ax_label),
        }
    }

    fn registry() -> (
        MacWindowRegistry,
        Arc<FakeSnapshotSource>,
        MacInvalidationHub,
    ) {
        let source = Arc::new(FakeSnapshotSource::default());
        let invalidations = MacInvalidationHub::default();
        (
            MacWindowRegistry::with_source(source.clone(), invalidations.clone()),
            source,
            invalidations,
        )
    }

    #[tokio::test]
    async fn no_title_window_keeps_opaque_identity_across_metadata_changes() {
        let (registry, source, _) = registry();
        source.replace(vec![snapshot("com.example.fixture", 1, 44, None, "ax-1")]);
        let first = registry.list_windows(None).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].title, None);

        source.replace(vec![snapshot(
            "com.example.fixture",
            1,
            44,
            Some("Document"),
            "ax-1",
        )]);
        let second = registry.list_windows(None).await.unwrap();
        assert_eq!(second[0].id, first[0].id);
        assert_eq!(second[0].title.as_deref(), Some("Document"));
    }

    #[test]
    fn framework_classification_is_exact_and_never_infers_electron() {
        assert_eq!(
            classify_framework(Some("com.google.Chrome")),
            Framework::Chromium
        );
        assert_eq!(
            classify_framework(Some("com.apple.Safari")),
            Framework::WebKit
        );
        assert_eq!(
            classify_framework(Some("com.example.electron-looking-app")),
            Framework::Unknown
        );
        assert_eq!(classify_framework(None), Framework::Unknown);
    }

    #[tokio::test]
    async fn native_facts_require_the_exact_live_geometry_stamp() {
        let (registry, source, _) = registry();
        let first = snapshot("com.example.fixture", 1, 44, None, "ax-1");
        source.replace(vec![first.clone()]);
        let public = registry.list_windows(None).await.unwrap().remove(0);
        let resolved = registry.resolve(&public).await.unwrap();
        let facts = registry.facts_for_stamp(&resolved.stamp()).await.unwrap();
        assert_eq!(facts.pid, 101);
        assert_eq!(facts.cg_window_id, 44);
        assert_eq!(facts.layer, 0);
        assert_eq!(facts.space_ids, Some(vec![7]));

        let mut moved = first;
        moved.bounds.x += 20.0;
        source.replace(vec![moved]);
        let refreshed = registry
            .facts_for_identity(&resolved.stamp())
            .await
            .unwrap();
        assert_eq!(refreshed.bounds.x, 30.0);
        let error = registry
            .facts_for_stamp(&resolved.stamp())
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ObservationStale);
    }

    #[tokio::test]
    async fn reused_native_key_with_different_ax_identity_revokes_old_window() {
        let (registry, source, invalidations) = registry();
        let mut events = invalidations.subscribe();
        source.replace(vec![snapshot("com.example.fixture", 1, 44, None, "ax-1")]);
        let old = registry.list_windows(None).await.unwrap().remove(0);

        source.replace(vec![snapshot("com.example.fixture", 1, 44, None, "ax-2")]);
        let replacement = registry.list_windows(None).await.unwrap().remove(0);
        assert_ne!(replacement.id, old.id);
        let error = registry.rehydrate(&old.id, None).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::WindowIdentityChanged);
        assert!(matches!(
            events.next().await,
            Some(TargetInvalidation::WindowGenerationChanged { window_id, .. })
                if window_id == old.id
        ));
    }

    #[tokio::test]
    async fn identity_churn_releases_live_entries_and_bounds_tombstones() {
        let (registry, source, _) = registry();
        for generation in 0..(MAX_WINDOW_TOMBSTONES + 8) {
            source.replace(vec![snapshot(
                "com.example.fixture",
                1,
                44,
                None,
                &format!("ax-{generation}"),
            )]);
            assert_eq!(registry.list_windows(None).await.unwrap().len(), 1);
        }

        let state = registry
            .state
            .lock()
            .expect("macOS window registry lock poisoned");
        assert_eq!(state.by_id.len(), 1);
        assert_eq!(state.by_native.len(), 1);
        assert_eq!(state.tombstones.len(), MAX_WINDOW_TOMBSTONES);
        assert_eq!(state.tombstone_order.len(), MAX_WINDOW_TOMBSTONES);
    }

    #[tokio::test]
    async fn owner_generation_change_never_rehydrates_through_pid_and_cg_id_reuse() {
        let (registry, source, _) = registry();
        source.replace(vec![snapshot("com.example.fixture", 10, 44, None, "ax-1")]);
        let old = registry.list_windows(None).await.unwrap().remove(0);

        source.replace(vec![snapshot("com.example.fixture", 11, 44, None, "ax-1")]);
        let replacement = registry.list_windows(None).await.unwrap().remove(0);
        assert_ne!(replacement.id, old.id);
        assert_ne!(replacement.app.id, old.app.id);
        let error = registry.rehydrate(&old.id, None).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::WindowNotFound);
    }

    #[tokio::test]
    async fn duplicate_bundle_instances_have_distinct_running_app_ids() {
        let (registry, source, _) = registry();
        let first = snapshot("com.example.fixture", 10, 44, None, "ax-1");
        let mut second = snapshot("com.example.fixture", 20, 45, None, "ax-2");
        second.key.pid = 202;
        second.process.pid = 202;
        source.replace(vec![first, second]);

        let windows = registry.list_windows(None).await.unwrap();
        assert_eq!(windows.len(), 2);
        assert_ne!(windows[0].app.id, windows[1].app.id);
    }

    #[tokio::test]
    async fn cross_app_rehydration_is_an_identity_error() {
        let (registry, source, _) = registry();
        source.replace(vec![snapshot("com.example.fixture", 1, 44, None, "ax-1")]);
        let window = registry.list_windows(None).await.unwrap().remove(0);
        let other_app = AppRef {
            id: AppId::parse("macos:bundle:com.example.other").unwrap(),
            name: Some("Other".to_owned()),
            pid: Some(102),
            running: true,
        };

        let error = registry
            .rehydrate(&window.id, Some(&other_app))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::WindowIdentityChanged);
    }

    #[test]
    fn process_generation_bracket_rejects_missing_or_reused_processes() {
        fn application(pid: i32, generation: Option<u64>) -> RunningApplicationInfo {
            RunningApplicationInfo {
                pid,
                name: Some("Fixture".to_owned()),
                bundle_id: Some("com.example.fixture".to_owned()),
                executable_path: None,
                process_generation: generation,
                active: false,
                hidden: false,
                finished_launching: true,
                regular: true,
            }
        }

        let before = HashMap::from([(101, application(101, Some(7)))]);
        let stable = HashMap::from([(101, application(101, Some(7)))]);
        assert!(racing_process_pids([101], &before, &stable).is_empty());

        let reused = HashMap::from([(101, application(101, Some(8)))]);
        assert_eq!(racing_process_pids([101], &before, &reused), vec![101]);

        let unknown = HashMap::from([(101, application(101, None))]);
        assert_eq!(racing_process_pids([101], &unknown, &unknown), vec![101]);
    }

    #[test]
    fn unreadable_ax_minimized_state_is_unknown() {
        let mut fixture = snapshot("com.example.fixture", 1, 44, None, "ax-1");
        fixture.minimized = None;
        assert_eq!(window_state(&fixture), WindowStateKind::Unknown);
    }
}
