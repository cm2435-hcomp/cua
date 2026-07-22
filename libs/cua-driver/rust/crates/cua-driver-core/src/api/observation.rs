//! Observation identity, AX revisions, and the only perception-to-action bridge.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use super::{
    capabilities::{Framework, WindowStateKind},
    contracts::{
        AccessibilityElement, AccessibilityState, AxRevision, AxTreeMode, AxTreeUpdate,
        CaptureRevision, CapturedSurface, ElementId, ElementRef, GeometryRevision, MenuId,
        ObservationId, Point, Rect, ReplaceAxLines, Size, SurfaceId, SurfaceKind, WindowGeneration,
        WindowRef,
    },
    errors::{ErrorCode, ErrorPhase, NativeError},
    menu::{MenuState, NativeMenuObservation},
    settlement::SettlementEvidence,
};

macro_rules! native_handle {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, NativeError> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(NativeError::invalid(concat!(
                        stringify!($name),
                        " cannot be empty"
                    )));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

native_handle!(NativeWindowHandle);
native_handle!(NativeProcessHandle);
native_handle!(NativeElementHandle);

#[derive(Debug, Clone, PartialEq)]
pub struct WindowGeometry {
    pub bounds: Rect,
    pub scale_factor: f64,
    pub revision: GeometryRevision,
}

impl WindowGeometry {
    pub fn validate(&self) -> Result<(), NativeError> {
        self.bounds.validate().map_err(invalid_native_geometry)?;
        if !self.scale_factor.is_finite() || self.scale_factor <= 0.0 {
            return Err(invalid_native_geometry(
                "window scale_factor must be finite and positive",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedWindow {
    pub public: WindowRef,
    pub native: NativeWindowHandle,
    pub process: NativeProcessHandle,
    pub framework: Framework,
    pub geometry: WindowGeometry,
    pub generation: WindowGeneration,
    pub state: WindowStateKind,
}

impl ResolvedWindow {
    pub fn stamp(&self) -> ResolvedWindowStamp {
        ResolvedWindowStamp {
            app_id: self.public.app.id.clone(),
            window_id: self.public.id.clone(),
            generation: self.generation,
            geometry_revision: self.geometry.revision.clone(),
            native_window: self.native.clone(),
            process: self.process.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWindowStamp {
    pub app_id: super::contracts::AppId,
    pub window_id: super::contracts::WindowId,
    pub generation: WindowGeneration,
    pub geometry_revision: GeometryRevision,
    pub native_window: NativeWindowHandle,
    pub process: NativeProcessHandle,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceToWindowTransform {
    pub scale_x: f64,
    pub scale_y: f64,
    pub offset_x: f64,
    pub offset_y: f64,
}

impl SurfaceToWindowTransform {
    pub fn transform(&self, point: Point) -> Point {
        Point {
            x: point.x * self.scale_x + self.offset_x,
            y: point.y * self.scale_y + self.offset_y,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFreshness {
    Fresh,
    ReusedWithFreshCompletion,
    Frozen,
    Unavailable,
}

/// Opaque monotonic epoch produced by the native observation journal. Core
/// carries it with captured pixels so the platform can reject a point if a
/// native content/focus/AX signal races before the first post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeObservationEpoch(pub u64);

impl CaptureFreshness {
    pub fn action_safe(self) -> bool {
        matches!(self, Self::Fresh | Self::ReusedWithFreshCompletion)
    }
}

#[derive(Debug, Clone)]
pub struct SurfaceRecord {
    pub id: SurfaceId,
    pub kind: SurfaceKind,
    pub owner_window: WindowRef,
    pub image_url: String,
    /// Native artifact bytes retained by this surface. The public URL length
    /// is not a useful proxy for observation-store memory/disk pressure.
    pub approximate_bytes: usize,
    pub raster_size: Size,
    pub window_bounds: Option<Rect>,
    pub capture_revision: CaptureRevision,
    pub observation_epoch: Option<NativeObservationEpoch>,
    pub transform: SurfaceToWindowTransform,
    pub freshness: CaptureFreshness,
    /// Exact native ownership for the pixels. Related transient surfaces are
    /// action-safe only while their recorded parent is the current target.
    pub owner: SurfaceOwner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceOwner {
    Target(ResolvedWindowStamp),
    RelatedTransient {
        owner: ResolvedWindowStamp,
        parent: ResolvedWindowStamp,
    },
}

impl SurfaceRecord {
    pub fn validate(&self) -> Result<(), NativeError> {
        self.raster_size
            .validate()
            .map_err(invalid_native_geometry)?;
        if let Some(bounds) = self.window_bounds {
            bounds.validate().map_err(invalid_native_geometry)?;
        }
        let transform = self.transform;
        if !transform.scale_x.is_finite()
            || !transform.scale_y.is_finite()
            || !transform.offset_x.is_finite()
            || !transform.offset_y.is_finite()
            || transform.scale_x == 0.0
            || transform.scale_y == 0.0
        {
            return Err(invalid_native_geometry(
                "surface transform must be finite and invertible",
            ));
        }
        Ok(())
    }

    pub fn validate_for_window(&self, window: &ResolvedWindow) -> Result<(), NativeError> {
        self.validate()?;
        validate_surface_owner(self, window)
    }

    pub fn public(&self) -> CapturedSurface {
        CapturedSurface {
            id: self.id.clone(),
            owner_window: self.owner_window.clone(),
            kind: self.kind,
            image_url: self.image_url.clone(),
            size: self.raster_size,
            window_bounds: self.window_bounds,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NativeAccessibilityElement {
    pub id: ElementId,
    pub native: NativeElementHandle,
    /// Exact target or related-transient window that owned this AX element
    /// when the observation was captured.
    pub owner: ResolvedWindowStamp,
    pub role: Option<String>,
    pub label: Option<String>,
    pub value: Option<String>,
    pub bounds: Option<Rect>,
    pub actions: Vec<String>,
    /// Native attribution for elements published by a related menu surface.
    pub menu_id: Option<MenuId>,
}

#[derive(Debug, Clone)]
pub struct NativeAccessibilityUpdate {
    pub normalized_tree: String,
    pub elements: Vec<NativeAccessibilityElement>,
    pub focused_element: Option<ElementId>,
    pub selected_text: Option<String>,
    pub selected_elements: Vec<ElementId>,
    pub document_text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NativeObservationUpdate {
    pub window: ResolvedWindow,
    pub surfaces: Vec<SurfaceRecord>,
    pub accessibility: Option<NativeAccessibilityUpdate>,
    pub menu: NativeMenuObservation,
    pub captured_at_unix_ms: u64,
    pub warnings: Vec<String>,
    /// Observation-owned native resources (for example screenshot files).
    /// Cleanup runs exactly once when the final stored handle is dropped.
    pub artifacts: Vec<ObservationArtifactHandle>,
}

type ArtifactCleanup = Box<dyn FnOnce() -> Result<(), NativeError> + Send + 'static>;

struct ObservationArtifactInner {
    label: String,
    cleanup: Mutex<Option<ArtifactCleanup>>,
}

impl Drop for ObservationArtifactInner {
    fn drop(&mut self) {
        let cleanup = self
            .cleanup
            .get_mut()
            .expect("observation artifact cleanup lock poisoned")
            .take();
        if let Some(cleanup) = cleanup {
            if let Err(error) = cleanup() {
                tracing::error!(artifact = %self.label, error = %error, "failed to clean observation artifact");
            }
        }
    }
}

/// Cloneable ownership token whose final drop performs artifact cleanup.
#[derive(Clone)]
pub struct ObservationArtifactHandle(Arc<ObservationArtifactInner>);

impl ObservationArtifactHandle {
    pub fn new(
        label: impl Into<String>,
        cleanup: impl FnOnce() -> Result<(), NativeError> + Send + 'static,
    ) -> Self {
        Self(Arc::new(ObservationArtifactInner {
            label: label.into(),
            cleanup: Mutex::new(Some(Box::new(cleanup))),
        }))
    }

    pub fn label(&self) -> &str {
        &self.0.label
    }
}

impl std::fmt::Debug for ObservationArtifactHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ObservationArtifactHandle")
            .field(&self.0.label)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct ElementRecord {
    pub id: ElementId,
    pub native: NativeElementHandle,
    pub owner: ResolvedWindowStamp,
    pub role: Option<String>,
    pub bounds: Option<Rect>,
    pub actions: Vec<String>,
    pub ax_revision: AxRevision,
    pub menu_id: Option<MenuId>,
}

#[derive(Debug, Clone)]
pub struct AccessibilityRecord {
    pub revision: AxRevision,
    pub elements: HashMap<ElementId, ElementRecord>,
    pub focused_element: Option<ElementId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidationReason {
    Superseded,
    MutationDispatched,
    WindowChanged,
    AppTerminated,
    DisplayChanged,
    /// Ordinary value/focus/content changed. Current observations are stale,
    /// but the last delivered AX tree remains a valid diff base.
    ContentChanged,
    AccessibilityInvalidated,
    TransientDismissed,
    MenuChanged,
    DiffBaseInvalidated,
    Evicted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationState {
    Current,
    Consumed {
        action_id: super::contracts::ActionId,
    },
    Invalidated {
        reason: InvalidationReason,
    },
    Expired,
}

#[derive(Debug, Clone)]
pub struct ObservationRecord {
    pub id: ObservationId,
    pub window: ResolvedWindowStamp,
    pub captured_at: Instant,
    pub surfaces: HashMap<SurfaceId, SurfaceRecord>,
    pub accessibility: Option<AccessibilityRecord>,
    pub menu: MenuState,
    pub settlement: SettlementEvidence,
    pub state: ObservationState,
    pub approximate_bytes: usize,
    pub artifacts: Vec<ObservationArtifactHandle>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPoint {
    pub window: ResolvedWindow,
    pub surface_id: SurfaceId,
    pub surface_owner: SurfaceOwner,
    pub capture_revision: CaptureRevision,
    pub observation_epoch: Option<NativeObservationEpoch>,
    pub surface_point: Point,
    pub window_point: Point,
    pub screen_point: Point,
    pub geometry_revision: GeometryRevision,
}

#[derive(Debug, Clone)]
pub struct ResolvedDrag {
    pub start: ResolvedPoint,
    pub end: ResolvedPoint,
    pub duration_ms: u32,
    pub button: super::contracts::MouseButton,
    pub modifiers: Vec<super::contracts::Modifier>,
}

#[derive(Debug, Clone)]
pub struct ResolvedScroll {
    pub point: ResolvedPoint,
    pub delta_x: f64,
    pub delta_y: f64,
}

#[derive(Debug, Clone)]
pub struct ResolvedElement {
    pub window: ResolvedWindow,
    pub observation_id: ObservationId,
    pub element_id: ElementId,
    pub native: NativeElementHandle,
    pub owner: ResolvedWindowStamp,
    pub ax_revision: AxRevision,
    pub role: Option<String>,
    pub bounds: Option<Rect>,
    pub actions: Vec<String>,
    pub menu_id: Option<MenuId>,
}

#[derive(Debug, Clone)]
pub struct ResolvedFocus {
    pub window: ResolvedWindow,
    pub observation_id: ObservationId,
    pub focused_element: Option<ElementId>,
    pub ax_revision: Option<AxRevision>,
}

#[derive(Debug)]
pub struct ObservationStore {
    records: HashMap<ObservationId, ObservationRecord>,
    /// Least-recently-used at the front, most-recently-used at the back.
    access_order: VecDeque<ObservationId>,
    max_count: usize,
    max_bytes: usize,
    ttl: Duration,
    current_bytes: usize,
}

impl ObservationStore {
    pub fn new(max_count: usize, max_bytes: usize, ttl: Duration) -> Self {
        Self {
            records: HashMap::new(),
            access_order: VecDeque::new(),
            max_count,
            max_bytes,
            ttl,
            current_bytes: 0,
        }
    }

    pub fn insert(&mut self, record: ObservationRecord) -> Result<(), NativeError> {
        self.expire();
        let id = record.id.clone();
        if self.max_count == 0 || record.approximate_bytes > self.max_bytes {
            return Err(NativeError::new(
                ErrorCode::Internal,
                ErrorPhase::Verify,
                false,
                "observation exceeds the configured native store capacity",
            )
            .with_detail("observation_bytes", record.approximate_bytes)
            .with_detail("max_bytes", self.max_bytes));
        }
        let replaced_bytes = self
            .records
            .get(&id)
            .map_or(0, |existing| existing.approximate_bytes);
        let projected_bytes = self
            .current_bytes
            .saturating_sub(replaced_bytes)
            .checked_add(record.approximate_bytes)
            .ok_or_else(|| {
                NativeError::new(
                    ErrorCode::Internal,
                    ErrorPhase::Verify,
                    false,
                    "observation store byte accounting overflowed",
                )
            })?;
        self.invalidate_all(InvalidationReason::Superseded);
        if self.records.insert(id.clone(), record).is_some() {
            self.access_order.retain(|candidate| candidate != &id);
        }
        self.current_bytes = projected_bytes;
        self.access_order.push_back(id.clone());
        self.evict(&id);
        debug_assert!(self.records.contains_key(&id));
        Ok(())
    }

    pub fn current(
        &mut self,
        observation_id: &ObservationId,
        window: &ResolvedWindow,
    ) -> Result<&ObservationRecord, NativeError> {
        self.expire();
        {
            let record = self.records.get(observation_id).ok_or_else(|| {
                NativeError::stale(
                    ErrorCode::ObservationStale,
                    "observation is missing or was evicted from the native store",
                )
                .with_detail("observation_id", observation_id.to_string())
            })?;
            ensure_window_stamp(&record.window, window)?;
            if record.state != ObservationState::Current {
                return Err(NativeError::stale(
                    ErrorCode::ObservationStale,
                    format!("observation is not current: {:?}", record.state),
                )
                .with_detail("observation_id", observation_id.to_string()));
            }
        }
        self.touch(observation_id);
        Ok(self
            .records
            .get(observation_id)
            .expect("touched observation remains in store"))
    }

    pub fn consume(
        &mut self,
        observation_id: &ObservationId,
        action_id: super::contracts::ActionId,
    ) -> Result<(), NativeError> {
        self.expire();
        {
            let record = self.records.get_mut(observation_id).ok_or_else(|| {
                NativeError::stale(
                    ErrorCode::ObservationStale,
                    "cannot consume an observation that is not in the native store",
                )
            })?;
            if record.state != ObservationState::Current {
                return Err(NativeError::stale(
                    ErrorCode::ObservationStale,
                    "observation was already consumed or invalidated",
                ));
            }
            record.state = ObservationState::Consumed { action_id };
        }
        self.touch(observation_id);
        Ok(())
    }

    /// Remove an observation immediately. Dropping its final artifact handles
    /// performs deterministic native-file cleanup.
    pub fn remove(&mut self, observation_id: &ObservationId) -> bool {
        self.remove_record(observation_id).is_some()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn current_bytes(&self) -> usize {
        self.current_bytes
    }

    pub fn invalidate_all(&mut self, reason: InvalidationReason) {
        for record in self.records.values_mut() {
            if record.state == ObservationState::Current {
                record.state = ObservationState::Invalidated {
                    reason: reason.clone(),
                };
            }
        }
    }

    pub fn resolve_point(
        &mut self,
        window: &ResolvedWindow,
        observation_id: &ObservationId,
        surface_id: &SurfaceId,
        point: Point,
    ) -> Result<ResolvedPoint, NativeError> {
        let record = self.current(observation_id, window)?;
        let surface = record.surfaces.get(surface_id).ok_or_else(|| {
            NativeError::stale(
                ErrorCode::SurfaceStale,
                "surface does not belong to the supplied observation",
            )
            .with_detail("surface_id", surface_id.to_string())
        })?;
        validate_surface_owner(surface, window)?;
        if !surface.freshness.action_safe() {
            return Err(NativeError::stale(
                ErrorCode::SurfaceStale,
                "surface capture is not fresh enough for a point action",
            ));
        }
        if point.x < 0.0
            || point.y < 0.0
            || point.x >= f64::from(surface.raster_size.width)
            || point.y >= f64::from(surface.raster_size.height)
        {
            return Err(NativeError::invalid(
                "point lies outside the captured surface raster",
            ));
        }
        let window_point = surface.transform.transform(point);
        let screen_point = Point {
            x: window.geometry.bounds.x + window_point.x,
            y: window.geometry.bounds.y + window_point.y,
        };
        Ok(ResolvedPoint {
            window: window.clone(),
            surface_id: surface_id.clone(),
            surface_owner: surface.owner.clone(),
            capture_revision: surface.capture_revision.clone(),
            observation_epoch: surface.observation_epoch,
            surface_point: point,
            window_point,
            screen_point,
            geometry_revision: record.window.geometry_revision.clone(),
        })
    }

    pub fn resolve_element(
        &mut self,
        window: &ResolvedWindow,
        element: &ElementRef,
    ) -> Result<ResolvedElement, NativeError> {
        let record = self.current(&element.observation_id, window)?;
        let accessibility = record.accessibility.as_ref().ok_or_else(|| {
            NativeError::stale(
                ErrorCode::ElementStale,
                "observation did not contain accessibility state",
            )
        })?;
        let resolved = accessibility.elements.get(&element.id).ok_or_else(|| {
            NativeError::stale(
                ErrorCode::ElementStale,
                "element does not belong to the supplied observation",
            )
            .with_detail("element_id", element.id.to_string())
        })?;
        if let Some(menu_id) = &resolved.menu_id {
            match &record.menu {
                MenuState::Open { id, .. } if id == menu_id => {}
                _ => {
                    return Err(NativeError::stale(
                        ErrorCode::MenuStateStale,
                        "menu element no longer matches the observation's active menu",
                    ))
                }
            }
        }
        Ok(ResolvedElement {
            window: window.clone(),
            observation_id: element.observation_id.clone(),
            element_id: resolved.id.clone(),
            native: resolved.native.clone(),
            owner: resolved.owner.clone(),
            ax_revision: resolved.ax_revision.clone(),
            role: resolved.role.clone(),
            bounds: resolved.bounds,
            actions: resolved.actions.clone(),
            menu_id: resolved.menu_id.clone(),
        })
    }

    pub fn resolve_element_point(
        &mut self,
        window: &ResolvedWindow,
        element: &ElementRef,
    ) -> Result<ResolvedPoint, NativeError> {
        let resolved = self.resolve_element(window, element)?;
        let center = resolved
            .bounds
            .ok_or_else(|| {
                NativeError::stale(
                    ErrorCode::ElementStale,
                    "element has no current bounds for targeted pointer dispatch",
                )
            })?
            .center();
        let record = self.current(&element.observation_id, window)?;
        let surface = record
            .surfaces
            .values()
            .find(|surface| {
                surface_owner_stamp(surface) == &resolved.owner && surface.freshness.action_safe()
            })
            .ok_or_else(|| {
                NativeError::stale(
                    ErrorCode::SurfaceStale,
                    "observation has no fresh surface for the element's exact owning window",
                )
            })?;
        if surface.transform.scale_x == 0.0 || surface.transform.scale_y == 0.0 {
            return Err(NativeError::stale(
                ErrorCode::SurfaceStale,
                "surface transform is not invertible",
            ));
        }
        let surface_point = Point {
            x: (center.x - surface.transform.offset_x) / surface.transform.scale_x,
            y: (center.y - surface.transform.offset_y) / surface.transform.scale_y,
        };
        let surface_id = surface.id.clone();
        self.resolve_point(window, &element.observation_id, &surface_id, surface_point)
    }

    pub fn validate_focus(
        &mut self,
        window: &ResolvedWindow,
        observation_id: &ObservationId,
    ) -> Result<ResolvedFocus, NativeError> {
        let record = self.current(observation_id, window)?;
        Ok(ResolvedFocus {
            window: window.clone(),
            observation_id: observation_id.clone(),
            focused_element: record
                .accessibility
                .as_ref()
                .and_then(|accessibility| accessibility.focused_element.clone()),
            ax_revision: record
                .accessibility
                .as_ref()
                .map(|accessibility| accessibility.revision.clone()),
        })
    }

    fn expire(&mut self) {
        let ttl = self.ttl;
        let expired: Vec<_> = self
            .records
            .iter()
            .filter(|(_, record)| record.captured_at.elapsed() > ttl)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.remove_record(&id);
        }
    }

    fn evict(&mut self, protected: &ObservationId) {
        while self.records.len() > self.max_count || self.current_bytes > self.max_bytes {
            let candidate = self
                .access_order
                .iter()
                .find(|id| {
                    *id != protected
                        && self.records.get(*id).is_some_and(|record| {
                            !matches!(record.state, ObservationState::Consumed { .. })
                        })
                })
                .or_else(|| self.access_order.iter().find(|id| *id != protected))
                .cloned();
            let Some(id) = candidate else {
                break;
            };
            self.remove_record(&id);
        }
    }

    fn touch(&mut self, observation_id: &ObservationId) {
        self.access_order
            .retain(|candidate| candidate != observation_id);
        self.access_order.push_back(observation_id.clone());
    }

    fn remove_record(&mut self, observation_id: &ObservationId) -> Option<ObservationRecord> {
        self.access_order
            .retain(|candidate| candidate != observation_id);
        let record = self.records.remove(observation_id)?;
        self.current_bytes = self.current_bytes.saturating_sub(record.approximate_bytes);
        Some(record)
    }
}

fn validate_surface_owner(
    surface: &SurfaceRecord,
    window: &ResolvedWindow,
) -> Result<(), NativeError> {
    let target = window.stamp();
    let owner = match &surface.owner {
        SurfaceOwner::Target(owner) if owner == &target => owner,
        SurfaceOwner::RelatedTransient { owner, parent } if parent == &target => owner,
        SurfaceOwner::Target(_) | SurfaceOwner::RelatedTransient { .. } => {
            return Err(NativeError::stale(
                ErrorCode::SurfaceStale,
                "surface native ownership no longer matches the current target",
            ))
        }
    };
    if owner.app_id != surface.owner_window.app.id || owner.window_id != surface.owner_window.id {
        return Err(NativeError::stale(
            ErrorCode::SurfaceStale,
            "surface public owner does not match its native owner stamp",
        ));
    }
    Ok(())
}

fn surface_owner_stamp(surface: &SurfaceRecord) -> &ResolvedWindowStamp {
    match &surface.owner {
        SurfaceOwner::Target(owner) | SurfaceOwner::RelatedTransient { owner, .. } => owner,
    }
}

fn invalid_native_geometry(message: impl Into<String>) -> NativeError {
    NativeError::new(
        ErrorCode::Internal,
        super::errors::ErrorPhase::Verify,
        false,
        message,
    )
}

impl Default for ObservationStore {
    fn default() -> Self {
        Self::new(32, 128 * 1024 * 1024, Duration::from_secs(120))
    }
}

fn ensure_window_stamp(
    stamp: &ResolvedWindowStamp,
    window: &ResolvedWindow,
) -> Result<(), NativeError> {
    if stamp.app_id != window.public.app.id
        || stamp.window_id != window.public.id
        || stamp.generation != window.generation
        || stamp.native_window != window.native
        || stamp.process != window.process
    {
        return Err(NativeError::stale(
            ErrorCode::WindowIdentityChanged,
            "observation target no longer matches the resolved window identity/generation",
        ));
    }
    if stamp.geometry_revision != window.geometry.revision {
        return Err(NativeError::stale(
            ErrorCode::SurfaceStale,
            "window geometry changed after observation",
        ));
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct AxRevisionState {
    last_delivered: Option<(AxRevision, String)>,
    force_full: bool,
}

impl AxRevisionState {
    pub fn invalidate_base(&mut self) {
        self.force_full = true;
    }

    pub fn reset(&mut self) {
        self.last_delivered = None;
        self.force_full = false;
    }

    pub fn prepare(
        &self,
        current_tree: &str,
        mode: AxTreeMode,
    ) -> Result<PreparedAxRevision, NativeError> {
        let normalized = normalize_tree(current_tree);
        let revision = AxRevision::new();
        let update = match (&self.last_delivered, mode, self.force_full) {
            (Some((base_revision, base_tree)), AxTreeMode::DiffIfAvailable, false) => {
                let operations = diff_lines(base_tree, &normalized);
                let reconstructed = apply_ax_diff(base_tree, &operations)?;
                if reconstructed != normalized {
                    return Err(NativeError::new(
                        ErrorCode::AxRevisionMismatch,
                        super::errors::ErrorPhase::Verify,
                        true,
                        "generated AX diff did not reconstruct the normalized current tree",
                    ));
                }
                AxTreeUpdate::Diff {
                    base_revision: base_revision.clone(),
                    revision: revision.clone(),
                    operations,
                }
            }
            _ => AxTreeUpdate::Full {
                revision: revision.clone(),
                tree: normalized.clone(),
            },
        };
        Ok(PreparedAxRevision {
            expected_base: self
                .last_delivered
                .as_ref()
                .map(|(revision, _)| revision.clone()),
            revision,
            normalized_tree: normalized,
            update,
        })
    }

    /// Commits a revision only after the complete observation has been
    /// validated and inserted into the observation store. The target state
    /// lock makes an intervening AX commit impossible.
    pub fn commit(&mut self, prepared: PreparedAxRevision) {
        debug_assert_eq!(
            self.last_delivered.as_ref().map(|(revision, _)| revision),
            prepared.expected_base.as_ref()
        );
        self.last_delivered = Some((prepared.revision, prepared.normalized_tree));
        self.force_full = false;
    }

    pub fn last_revision(&self) -> Option<&AxRevision> {
        self.last_delivered.as_ref().map(|(revision, _)| revision)
    }
}

pub struct PreparedAxRevision {
    expected_base: Option<AxRevision>,
    revision: AxRevision,
    normalized_tree: String,
    pub update: AxTreeUpdate,
}

pub fn normalize_tree(tree: &str) -> String {
    tree.replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn diff_lines(base: &str, current: &str) -> Vec<ReplaceAxLines> {
    let base_lines: Vec<_> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('\n').collect()
    };
    let current_lines: Vec<_> = if current.is_empty() {
        Vec::new()
    } else {
        current.split('\n').collect()
    };
    let prefix = base_lines
        .iter()
        .zip(current_lines.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let max_suffix = base_lines
        .len()
        .min(current_lines.len())
        .saturating_sub(prefix);
    let suffix = (0..max_suffix)
        .take_while(|offset| {
            base_lines[base_lines.len() - 1 - offset]
                == current_lines[current_lines.len() - 1 - offset]
        })
        .count();
    if prefix == base_lines.len() && prefix == current_lines.len() {
        return Vec::new();
    }
    vec![ReplaceAxLines {
        start_line: prefix,
        delete_count: base_lines.len() - prefix - suffix,
        lines: current_lines[prefix..current_lines.len() - suffix]
            .iter()
            .map(|line| (*line).to_owned())
            .collect(),
    }]
}

pub fn apply_ax_diff(base: &str, operations: &[ReplaceAxLines]) -> Result<String, NativeError> {
    let mut base_lines: Vec<String> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('\n').map(str::to_owned).collect()
    };
    let mut previous_end = 0;
    for operation in operations {
        let end = operation
            .start_line
            .checked_add(operation.delete_count)
            .ok_or_else(|| ax_diff_error("AX diff range overflow"))?;
        if operation.start_line < previous_end || end > base_lines.len() {
            return Err(ax_diff_error(
                "AX diff operations overlap, are unsorted, or exceed the base tree",
            ));
        }
        previous_end = end;
    }
    for operation in operations.iter().rev() {
        let end = operation.start_line + operation.delete_count;
        base_lines.splice(operation.start_line..end, operation.lines.clone());
    }
    Ok(base_lines.join("\n"))
}

fn ax_diff_error(message: impl Into<String>) -> NativeError {
    NativeError::new(
        ErrorCode::AxRevisionMismatch,
        super::errors::ErrorPhase::Verify,
        true,
        message,
    )
}

pub struct RevisionedAccessibility {
    pub public: AccessibilityState,
    pub record: AccessibilityRecord,
    pub prepared_revision: PreparedAxRevision,
}

pub fn revision_accessibility(
    state: &AxRevisionState,
    observation_id: &ObservationId,
    native: NativeAccessibilityUpdate,
    mode: AxTreeMode,
) -> Result<RevisionedAccessibility, NativeError> {
    let mut element_ids = std::collections::HashSet::with_capacity(native.elements.len());
    for element in &native.elements {
        if let Some(bounds) = element.bounds {
            bounds.validate().map_err(ax_manifest_error)?;
        }
        if !element_ids.insert(element.id.clone()) {
            return Err(ax_manifest_error(
                "native accessibility update returned a duplicate element id",
            ));
        }
    }
    if native
        .focused_element
        .as_ref()
        .is_some_and(|id| !element_ids.contains(id))
    {
        return Err(ax_manifest_error(
            "native accessibility focus references an element outside the manifest",
        ));
    }
    if native
        .selected_elements
        .iter()
        .any(|id| !element_ids.contains(id))
    {
        return Err(ax_manifest_error(
            "native accessibility selection references an element outside the manifest",
        ));
    }
    let prepared_revision = state.prepare(&native.normalized_tree, mode)?;
    let tree_update = prepared_revision.update.clone();
    let revision = tree_update.revision().clone();
    let mut records = HashMap::new();
    let mut elements = Vec::with_capacity(native.elements.len());
    for element in native.elements {
        let element_ref = ElementRef {
            observation_id: observation_id.clone(),
            id: element.id.clone(),
        };
        elements.push(AccessibilityElement {
            element_ref,
            role: element.role.clone(),
            label: element.label,
            value: element.value,
            bounds: element.bounds,
            actions: element.actions.clone(),
        });
        records.insert(
            element.id.clone(),
            ElementRecord {
                id: element.id,
                native: element.native,
                owner: element.owner,
                role: element.role,
                bounds: element.bounds,
                actions: element.actions,
                ax_revision: revision.clone(),
                menu_id: element.menu_id,
            },
        );
    }
    let focused_element = native.focused_element.as_ref().map(|id| ElementRef {
        observation_id: observation_id.clone(),
        id: id.clone(),
    });
    let selected_elements = native
        .selected_elements
        .iter()
        .map(|id| ElementRef {
            observation_id: observation_id.clone(),
            id: id.clone(),
        })
        .collect();
    Ok(RevisionedAccessibility {
        public: AccessibilityState {
            tree_update,
            elements,
            focused_element,
            selected_text: native.selected_text,
            selected_elements,
            document_text: native.document_text,
        },
        record: AccessibilityRecord {
            revision,
            elements: records,
            focused_element: native.focused_element,
        },
        prepared_revision,
    })
}

fn ax_manifest_error(message: impl Into<String>) -> NativeError {
    NativeError::new(
        ErrorCode::AxRevisionMismatch,
        super::errors::ErrorPhase::Verify,
        false,
        message,
    )
}

#[cfg(test)]
mod store_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::api::contracts::{AppId, AppRef, WindowId};

    fn window() -> ResolvedWindow {
        let public = WindowRef {
            id: WindowId::parse("window-1").unwrap(),
            app: AppRef {
                id: AppId::parse("app-1").unwrap(),
                name: Some("Fixture".to_owned()),
                pid: Some(100),
                running: true,
            },
            title: Some("Fixture".to_owned()),
        };
        ResolvedWindow {
            public,
            native: NativeWindowHandle::new("native-window-1").unwrap(),
            process: NativeProcessHandle::new("native-process-1").unwrap(),
            framework: Framework::AppKit,
            geometry: WindowGeometry {
                bounds: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 100.0,
                },
                scale_factor: 2.0,
                revision: GeometryRevision::parse("geometry-1").unwrap(),
            },
            generation: WindowGeneration(1),
            state: WindowStateKind::Visible,
        }
    }

    fn record(
        id: &str,
        bytes: usize,
        captured_at: Instant,
        cleaned: Arc<AtomicUsize>,
    ) -> ObservationRecord {
        ObservationRecord {
            id: ObservationId::parse(id).unwrap(),
            window: window().stamp(),
            captured_at,
            surfaces: HashMap::new(),
            accessibility: None,
            menu: MenuState::Closed {
                revision: super::super::contracts::MenuRevision::new(),
            },
            settlement: SettlementEvidence::initial(),
            state: ObservationState::Current,
            approximate_bytes: bytes,
            artifacts: vec![ObservationArtifactHandle::new(id, move || {
                cleaned.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })],
        }
    }

    #[test]
    fn count_and_byte_eviction_share_accounting_and_cleanup() {
        let cleaned = Arc::new(AtomicUsize::new(0));
        let mut store = ObservationStore::new(2, 15, Duration::from_secs(60));
        store
            .insert(record("one", 5, Instant::now(), cleaned.clone()))
            .unwrap();
        store
            .insert(record("two", 5, Instant::now(), cleaned.clone()))
            .unwrap();
        store
            .insert(record("three", 10, Instant::now(), cleaned.clone()))
            .unwrap();

        assert_eq!(store.len(), 2);
        assert_eq!(store.current_bytes(), 15);
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        assert!(store.remove(&ObservationId::parse("three").unwrap()));
        assert_eq!(store.len(), 1);
        assert_eq!(store.current_bytes(), 5);
        assert_eq!(cleaned.load(Ordering::SeqCst), 2);
        assert!(store.remove(&ObservationId::parse("two").unwrap()));
        assert!(store.is_empty());
        assert_eq!(store.current_bytes(), 0);
        assert_eq!(cleaned.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn ttl_removal_drops_artifacts_instead_of_retaining_expired_records() {
        let cleaned = Arc::new(AtomicUsize::new(0));
        let mut store = ObservationStore::new(4, 1_000, Duration::from_millis(5));
        let id = ObservationId::parse("expired").unwrap();
        store
            .insert(record(
                "expired",
                100,
                Instant::now() - Duration::from_secs(1),
                cleaned.clone(),
            ))
            .unwrap();

        let error = store.current(&id, &window()).unwrap_err();
        assert_eq!(error.code, ErrorCode::ObservationStale);
        assert!(store.is_empty());
        assert_eq!(store.current_bytes(), 0);
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn insert_expires_old_records_before_capacity_eviction() {
        let cleaned = Arc::new(AtomicUsize::new(0));
        let mut store = ObservationStore::new(1, 1_000, Duration::from_secs(1));
        store
            .insert(record(
                "expired-before-insert",
                900,
                Instant::now() - Duration::from_secs(2),
                cleaned.clone(),
            ))
            .unwrap();
        store
            .insert(record(
                "fresh-after-expiry",
                100,
                Instant::now(),
                cleaned.clone(),
            ))
            .unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(store.current_bytes(), 100);
        assert!(store
            .records
            .contains_key(&ObservationId::parse("fresh-after-expiry").unwrap()));
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn oversize_insert_fails_before_invalidating_or_evicting_current() {
        let cleaned = Arc::new(AtomicUsize::new(0));
        let mut store = ObservationStore::new(2, 10, Duration::from_secs(60));
        let current_id = ObservationId::parse("retained-current").unwrap();
        store
            .insert(record(
                "retained-current",
                5,
                Instant::now(),
                cleaned.clone(),
            ))
            .unwrap();

        let error = store
            .insert(record("oversize", 11, Instant::now(), cleaned.clone()))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(store.len(), 1);
        assert_eq!(store.current_bytes(), 5);
        assert!(store.current(&current_id, &window()).is_ok());
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn eviction_prefers_invalidated_lru_over_consumed_evidence() {
        let cleaned = Arc::new(AtomicUsize::new(0));
        let mut store = ObservationStore::new(2, 1_000, Duration::from_secs(60));
        let consumed = ObservationId::parse("consumed-evidence").unwrap();
        let invalidated = ObservationId::parse("invalidated-evidence").unwrap();
        let newest = ObservationId::parse("new-current").unwrap();
        store
            .insert(record(
                "consumed-evidence",
                10,
                Instant::now(),
                cleaned.clone(),
            ))
            .unwrap();
        store
            .consume(&consumed, crate::api::contracts::ActionId::new())
            .unwrap();
        store
            .insert(record(
                "invalidated-evidence",
                10,
                Instant::now(),
                cleaned.clone(),
            ))
            .unwrap();
        store
            .insert(record("new-current", 10, Instant::now(), cleaned.clone()))
            .unwrap();

        assert!(store.records.contains_key(&consumed));
        assert!(!store.records.contains_key(&invalidated));
        assert!(store.records.contains_key(&newest));
    }

    #[test]
    fn element_point_uses_the_surface_owned_by_the_exact_related_window() {
        let target = window();
        let mut related = target.stamp();
        related.window_id = WindowId::parse("related-1").unwrap();
        related.generation = WindowGeneration(2);
        related.geometry_revision = GeometryRevision::parse("related-geometry-1").unwrap();
        related.native_window = NativeWindowHandle::new("native-related-1").unwrap();
        let related_window = WindowRef {
            id: related.window_id.clone(),
            app: target.public.app.clone(),
            title: Some("Popover".to_owned()),
        };
        let observation_id = ObservationId::parse("related-observation").unwrap();
        let element_id = ElementId::parse("related-element").unwrap();
        let related_surface_id = SurfaceId::parse("related-surface").unwrap();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let mut observation = record(observation_id.as_str(), 1, Instant::now(), cleaned);
        observation.surfaces.insert(
            related_surface_id.clone(),
            SurfaceRecord {
                id: related_surface_id.clone(),
                kind: SurfaceKind::Popover,
                owner_window: related_window,
                image_url: "file:///tmp/related.png".to_owned(),
                raster_size: Size {
                    width: 100,
                    height: 100,
                },
                window_bounds: None,
                capture_revision: CaptureRevision::parse("related-capture").unwrap(),
                observation_epoch: None,
                transform: SurfaceToWindowTransform {
                    scale_x: 0.5,
                    scale_y: 0.5,
                    offset_x: 20.0,
                    offset_y: 30.0,
                },
                freshness: CaptureFreshness::Fresh,
                owner: SurfaceOwner::RelatedTransient {
                    owner: related.clone(),
                    parent: target.stamp(),
                },
                approximate_bytes: 1,
            },
        );
        observation.accessibility = Some(AccessibilityRecord {
            revision: AxRevision::parse("ax-related").unwrap(),
            elements: HashMap::from([(
                element_id.clone(),
                ElementRecord {
                    id: element_id.clone(),
                    native: NativeElementHandle::new("native-related-element").unwrap(),
                    owner: related,
                    role: Some("AXButton".to_owned()),
                    bounds: Some(Rect {
                        x: 30.0,
                        y: 40.0,
                        width: 10.0,
                        height: 10.0,
                    }),
                    actions: vec!["AXPress".to_owned()],
                    ax_revision: AxRevision::parse("ax-related").unwrap(),
                    menu_id: None,
                },
            )]),
            focused_element: None,
        });
        let mut store = ObservationStore::default();
        store.insert(observation).unwrap();

        let point = store
            .resolve_element_point(
                &target,
                &ElementRef {
                    observation_id,
                    id: element_id,
                },
            )
            .unwrap();
        assert_eq!(point.surface_id, related_surface_id);
        assert_eq!(point.surface_point, Point { x: 30.0, y: 30.0 });
        assert_eq!(point.window_point, Point { x: 35.0, y: 45.0 });
    }
}
