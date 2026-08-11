//! Durable menu lifecycle state with revisioned public observations.

use serde::{Deserialize, Serialize};

use super::contracts::{
    ActionId, ElementId, ElementRef, MenuId, MenuRevision, ObservationId, SurfaceId,
    WindowGeneration, WindowRef,
};
use super::errors::{ErrorCode, NativeError};
use super::observation::{NativeProcessHandle, NativeWindowHandle, ResolvedWindowStamp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeMenuIdentity {
    pub process: NativeProcessHandle,
    pub window: NativeWindowHandle,
    pub generation: WindowGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeMenuEvidence {
    Opened {
        menu_id: MenuId,
        opened_by_action_id: ActionId,
        owner: ResolvedWindowStamp,
        identity: NativeMenuIdentity,
        surface_ids: Vec<SurfaceId>,
        focused_item: Option<ElementId>,
    },
    Targeted {
        menu_id: MenuId,
        action_id: ActionId,
        owner: ResolvedWindowStamp,
        identity: NativeMenuIdentity,
    },
    Dismissed {
        menu_id: MenuId,
        action_id: ActionId,
        owner: ResolvedWindowStamp,
        identity: NativeMenuIdentity,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(clippy::large_enum_variant)] // Exact owner/action/native evidence stays one causal value.
pub enum NativeMenuObservation {
    #[default]
    Unchanged,
    Closed {
        identity: NativeMenuIdentity,
    },
    Open {
        menu_id: MenuId,
        opened_by_action_id: ActionId,
        owner: ResolvedWindowStamp,
        identity: NativeMenuIdentity,
        surface_ids: Vec<SurfaceId>,
        focused_item: Option<ElementId>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuMutationIntent {
    Opening {
        menu_id: MenuId,
    },
    Targeting {
        menu_id: MenuId,
        identity: NativeMenuIdentity,
    },
    Dismissing {
        menu_id: MenuId,
        identity: NativeMenuIdentity,
    },
}

impl MenuMutationIntent {
    pub fn menu_id(&self) -> &MenuId {
        match self {
            Self::Opening { menu_id }
            | Self::Targeting { menu_id, .. }
            | Self::Dismissing { menu_id, .. } => menu_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)] // Wire shape intentionally matches the public tagged union.
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MenuState {
    Closed {
        revision: MenuRevision,
    },
    Open {
        revision: MenuRevision,
        id: MenuId,
        opened_by_action_id: ActionId,
        owner_window: WindowRef,
        #[serde(default)]
        surface_ids: Vec<SurfaceId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focused_item: Option<ElementRef>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum MenuLifecycle {
    Closed,
    Opening {
        id: MenuId,
        action_id: ActionId,
        owner: WindowRef,
        owner_native: ResolvedWindowStamp,
    },
    Open {
        id: MenuId,
        opened_by_action_id: ActionId,
        owner: WindowRef,
        surface_ids: Vec<SurfaceId>,
        focused_item: Option<ElementRef>,
        native_identity: NativeMenuIdentity,
        owner_native: ResolvedWindowStamp,
    },
    Targeting {
        id: MenuId,
        opened_by_action_id: ActionId,
        owner: WindowRef,
        action_id: ActionId,
        surface_ids: Vec<SurfaceId>,
        focused_item: Option<ElementRef>,
        native_identity: NativeMenuIdentity,
        owner_native: ResolvedWindowStamp,
    },
    Dismissing {
        id: MenuId,
        opened_by_action_id: ActionId,
        owner: WindowRef,
        action_id: ActionId,
        surface_ids: Vec<SurfaceId>,
        focused_item: Option<ElementRef>,
        native_identity: NativeMenuIdentity,
        owner_native: ResolvedWindowStamp,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuControllerState {
    revision: MenuRevision,
    lifecycle: MenuLifecycle,
}

impl Default for MenuControllerState {
    fn default() -> Self {
        Self {
            revision: MenuRevision::new(),
            lifecycle: MenuLifecycle::Closed,
        }
    }
}

impl MenuControllerState {
    pub fn lifecycle(&self) -> &MenuLifecycle {
        &self.lifecycle
    }

    pub fn begin_open(
        &mut self,
        action_id: ActionId,
        owner: WindowRef,
        owner_native: ResolvedWindowStamp,
    ) -> MenuId {
        let id = MenuId::new();
        self.revision = MenuRevision::new();
        self.lifecycle = MenuLifecycle::Opening {
            id: id.clone(),
            action_id,
            owner,
            owner_native,
        };
        id
    }

    #[allow(clippy::too_many_arguments)] // Each causal field is validated independently here.
    pub fn record_open(
        &mut self,
        id: &MenuId,
        evidence_menu_id: &MenuId,
        evidence_action_id: &ActionId,
        evidence_owner: &ResolvedWindowStamp,
        native_identity: NativeMenuIdentity,
        surface_ids: Vec<SurfaceId>,
        focused_item: Option<ElementRef>,
    ) -> Result<(), NativeError> {
        let (opened_by_action_id, owner, owner_native) = match &self.lifecycle {
            MenuLifecycle::Opening {
                id: current,
                action_id,
                owner,
                owner_native,
            } if current == id
                && current == evidence_menu_id
                && action_id == evidence_action_id
                && owner_native == evidence_owner =>
            {
                (action_id.clone(), owner.clone(), owner_native.clone())
            }
            _ => {
                return Err(menu_stale(
                    "opened menu does not match the active opening state",
                ))
            }
        };
        self.revision = MenuRevision::new();
        self.lifecycle = MenuLifecycle::Open {
            id: id.clone(),
            opened_by_action_id,
            owner,
            surface_ids,
            focused_item,
            native_identity,
            owner_native,
        };
        Ok(())
    }

    pub fn record_dispatch_evidence(
        &mut self,
        action_id: &ActionId,
        owner: &ResolvedWindowStamp,
        evidence: &NativeMenuEvidence,
    ) -> Result<(), NativeError> {
        match evidence {
            NativeMenuEvidence::Opened {
                menu_id,
                opened_by_action_id,
                owner: evidence_owner,
                identity,
                surface_ids,
                focused_item: _,
            } => {
                if opened_by_action_id != action_id || evidence_owner != owner {
                    return Err(menu_stale(
                        "native menu-open evidence does not match the dispatch action/owner",
                    ));
                }
                self.record_open(
                    menu_id,
                    menu_id,
                    opened_by_action_id,
                    evidence_owner,
                    identity.clone(),
                    surface_ids.clone(),
                    None,
                )
            }
            NativeMenuEvidence::Dismissed {
                menu_id,
                action_id: evidence_action_id,
                owner: evidence_owner,
                identity,
            } => {
                self.validate_dispatch_identity(
                    menu_id,
                    evidence_action_id,
                    evidence_owner,
                    identity,
                    action_id,
                    owner,
                )?;
                self.close();
                Ok(())
            }
            NativeMenuEvidence::Targeted {
                menu_id,
                action_id: evidence_action_id,
                owner: evidence_owner,
                identity,
            } => {
                self.validate_dispatch_identity(
                    menu_id,
                    evidence_action_id,
                    evidence_owner,
                    identity,
                    action_id,
                    owner,
                )?;
                self.finish_target()?;
                Ok(())
            }
        }
    }

    pub fn begin_target(&mut self, id: &MenuId, action_id: ActionId) -> Result<(), NativeError> {
        let MenuLifecycle::Open {
            id: current,
            opened_by_action_id,
            owner,
            surface_ids,
            focused_item,
            native_identity,
            owner_native,
        } = &self.lifecycle
        else {
            return Err(menu_stale("no open menu is available for targeting"));
        };
        if current != id {
            return Err(menu_stale("menu id does not match the currently open menu"));
        }
        self.lifecycle = MenuLifecycle::Targeting {
            id: current.clone(),
            opened_by_action_id: opened_by_action_id.clone(),
            owner: owner.clone(),
            action_id,
            surface_ids: surface_ids.clone(),
            focused_item: focused_item.clone(),
            native_identity: native_identity.clone(),
            owner_native: owner_native.clone(),
        };
        Ok(())
    }

    pub fn begin_dismiss(&mut self, action_id: ActionId) -> Result<(), NativeError> {
        let (
            id,
            opened_by_action_id,
            owner,
            surface_ids,
            focused_item,
            native_identity,
            owner_native,
        ) = match &self.lifecycle {
            MenuLifecycle::Open {
                id,
                opened_by_action_id,
                owner,
                surface_ids,
                focused_item,
                native_identity,
                owner_native,
                ..
            }
            | MenuLifecycle::Targeting {
                id,
                opened_by_action_id,
                owner,
                surface_ids,
                focused_item,
                native_identity,
                owner_native,
                ..
            } => (
                id.clone(),
                opened_by_action_id.clone(),
                owner.clone(),
                surface_ids.clone(),
                focused_item.clone(),
                native_identity.clone(),
                owner_native.clone(),
            ),
            _ => return Err(menu_stale("no active menu is available for dismissal")),
        };
        self.lifecycle = MenuLifecycle::Dismissing {
            id,
            opened_by_action_id,
            owner,
            action_id,
            surface_ids,
            focused_item,
            native_identity,
            owner_native,
        };
        Ok(())
    }

    pub fn abort_transition(&mut self, action_id: &ActionId) -> Result<(), NativeError> {
        let replacement = match &self.lifecycle {
            MenuLifecycle::Opening {
                action_id: current, ..
            } if current == action_id => Some(MenuLifecycle::Closed),
            MenuLifecycle::Targeting {
                id,
                opened_by_action_id,
                owner,
                action_id: current,
                surface_ids,
                focused_item,
                native_identity,
                owner_native,
            } if current == action_id => Some(MenuLifecycle::Open {
                id: id.clone(),
                opened_by_action_id: opened_by_action_id.clone(),
                owner: owner.clone(),
                surface_ids: surface_ids.clone(),
                focused_item: focused_item.clone(),
                native_identity: native_identity.clone(),
                owner_native: owner_native.clone(),
            }),
            MenuLifecycle::Dismissing {
                id,
                opened_by_action_id,
                owner,
                action_id: current,
                surface_ids,
                focused_item,
                native_identity,
                owner_native,
            } if current == action_id => Some(MenuLifecycle::Open {
                id: id.clone(),
                opened_by_action_id: opened_by_action_id.clone(),
                owner: owner.clone(),
                surface_ids: surface_ids.clone(),
                focused_item: focused_item.clone(),
                native_identity: native_identity.clone(),
                owner_native: owner_native.clone(),
            }),
            _ => None,
        };
        let Some(replacement) = replacement else {
            return Err(menu_stale(
                "menu transition does not belong to the action being aborted",
            ));
        };
        self.revision = MenuRevision::new();
        self.lifecycle = replacement;
        Ok(())
    }

    pub fn reconcile_observation(
        &mut self,
        observation: NativeMenuObservation,
        observation_id: &ObservationId,
    ) -> Result<(), NativeError> {
        match observation {
            NativeMenuObservation::Unchanged => Ok(()),
            NativeMenuObservation::Closed { identity } => {
                if self.native_identity() != Some(&identity) {
                    return Err(menu_stale(
                        "closed native menu identity does not match the active menu",
                    ));
                }
                self.close();
                Ok(())
            }
            NativeMenuObservation::Open {
                menu_id,
                opened_by_action_id,
                owner,
                identity,
                surface_ids,
                focused_item,
            } => match &mut self.lifecycle {
                MenuLifecycle::Opening { id, .. } => {
                    let id = id.clone();
                    self.record_open(
                        &id,
                        &menu_id,
                        &opened_by_action_id,
                        &owner,
                        identity,
                        surface_ids,
                        focused_item.map(|id| ElementRef {
                            observation_id: observation_id.clone(),
                            id,
                        }),
                    )
                }
                MenuLifecycle::Open {
                    id,
                    opened_by_action_id: current_action,
                    owner_native,
                    native_identity,
                    surface_ids: current_surfaces,
                    focused_item: current_focus,
                    ..
                } if id == &menu_id
                    && current_action == &opened_by_action_id
                    && owner_native == &owner
                    && native_identity == &identity =>
                {
                    *current_surfaces = surface_ids;
                    *current_focus = focused_item.map(|id| ElementRef {
                        observation_id: observation_id.clone(),
                        id,
                    });
                    self.revision = MenuRevision::new();
                    Ok(())
                }
                _ => Err(menu_stale(
                    "observed native menu identity has no matching controller lifecycle",
                )),
            },
        }
    }

    pub fn native_identity(&self) -> Option<&NativeMenuIdentity> {
        match &self.lifecycle {
            MenuLifecycle::Open {
                native_identity, ..
            }
            | MenuLifecycle::Targeting {
                native_identity, ..
            }
            | MenuLifecycle::Dismissing {
                native_identity, ..
            } => Some(native_identity),
            _ => None,
        }
    }

    fn validate_dispatch_identity(
        &self,
        menu_id: &MenuId,
        evidence_action_id: &ActionId,
        evidence_owner: &ResolvedWindowStamp,
        identity: &NativeMenuIdentity,
        action_id: &ActionId,
        owner: &ResolvedWindowStamp,
    ) -> Result<(), NativeError> {
        let (current_id, current_action, current_owner, current_identity) = match &self.lifecycle {
            MenuLifecycle::Targeting {
                id,
                action_id,
                owner_native,
                native_identity,
                ..
            }
            | MenuLifecycle::Dismissing {
                id,
                action_id,
                owner_native,
                native_identity,
                ..
            } => (id, action_id, owner_native, native_identity),
            _ => {
                return Err(menu_stale(
                    "native menu evidence has no matching controller transition",
                ))
            }
        };
        if current_id != menu_id
            || current_action != evidence_action_id
            || current_action != action_id
            || current_owner != evidence_owner
            || current_owner != owner
            || current_identity != identity
        {
            return Err(menu_stale(
                "native menu evidence does not match the current menu/action/owner identity",
            ));
        }
        Ok(())
    }

    fn finish_target(&mut self) -> Result<(), NativeError> {
        let MenuLifecycle::Targeting {
            id,
            opened_by_action_id,
            owner,
            surface_ids,
            focused_item,
            native_identity,
            owner_native,
            ..
        } = &self.lifecycle
        else {
            return Err(menu_stale("no menu targeting transition is active"));
        };
        self.lifecycle = MenuLifecycle::Open {
            id: id.clone(),
            opened_by_action_id: opened_by_action_id.clone(),
            owner: owner.clone(),
            surface_ids: surface_ids.clone(),
            focused_item: focused_item.clone(),
            native_identity: native_identity.clone(),
            owner_native: owner_native.clone(),
        };
        Ok(())
    }

    pub fn validate_current_menu_id(&self, menu_id: &MenuId) -> Result<(), NativeError> {
        match &self.lifecycle {
            MenuLifecycle::Open { id, .. } | MenuLifecycle::Targeting { id, .. }
                if id == menu_id =>
            {
                Ok(())
            }
            _ => Err(menu_stale(
                "menu element does not match the controller's current MenuId",
            )),
        }
    }

    pub fn close(&mut self) {
        self.revision = MenuRevision::new();
        self.lifecycle = MenuLifecycle::Closed;
    }

    pub fn observation(&self) -> Result<MenuState, NativeError> {
        match &self.lifecycle {
            MenuLifecycle::Closed => Ok(MenuState::Closed {
                revision: self.revision.clone(),
            }),
            MenuLifecycle::Open {
                id,
                opened_by_action_id,
                owner,
                surface_ids,
                focused_item,
                ..
            } => Ok(MenuState::Open {
                revision: self.revision.clone(),
                id: id.clone(),
                opened_by_action_id: opened_by_action_id.clone(),
                owner_window: owner.clone(),
                surface_ids: surface_ids.clone(),
                focused_item: focused_item.clone(),
            }),
            _ => Err(menu_stale(
                "menu lifecycle is transitional and cannot be published as settled state",
            )),
        }
    }
}

fn menu_stale(message: impl Into<String>) -> NativeError {
    NativeError::stale(ErrorCode::MenuStateStale, message)
}
