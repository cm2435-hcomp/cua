//! Typed seam for future exact per-target menu dismissal suppression.
//!
//! Plan 004 does not have a proved event-tap predicate. Production therefore
//! refuses every required menu lease instead of installing a broader filter.

use cua_driver_core::api::{contracts::MenuId, errors::NativeError, interaction::NativeEvidence};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuSuppressionPlan {
    NotApplicable,
    ExactPredicateRequired { pid: i32, menu_id: MenuId },
}

pub(crate) trait MenuSuppressionResource: Send {
    fn release(&mut self) -> Result<NativeEvidence, NativeError>;
}

pub(crate) fn acquire_production(
    plan: &MenuSuppressionPlan,
) -> Result<Option<Box<dyn MenuSuppressionResource>>, NativeError> {
    match plan {
        MenuSuppressionPlan::NotApplicable => Ok(None),
        MenuSuppressionPlan::ExactPredicateRequired { pid, menu_id } => {
            Err(NativeError::unsupported(
                "recipe_unproven: exact per-pid and per-menu event-tap predicate is not proved",
            )
            .with_detail("recipe_status", "recipe_unproven")
            .with_detail("pid", *pid)
            .with_detail("menu_id", menu_id.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_refuses_required_menu_suppression() {
        let plan = MenuSuppressionPlan::ExactPredicateRequired {
            pid: 44,
            menu_id: MenuId::parse("menu").unwrap(),
        };
        let error = acquire_production(&plan).err().unwrap();
        assert_eq!(error.details["recipe_status"], "recipe_unproven");
    }
}
