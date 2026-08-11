//! Typed, background-only driver v2 contracts and orchestration.
//!
//! This module is additive. The MCP/v1 surface remains available while v2 is
//! built and verified independently.

pub mod capabilities;
pub mod contracts;
pub mod controller;
pub mod dispatch;
pub mod errors;
pub mod interaction;
pub mod menu;
pub mod observation;
pub mod platform;
pub mod settlement;
pub mod target;

pub use capabilities::*;
pub use contracts::*;
pub use controller::*;
pub use dispatch::*;
pub use errors::*;
pub use interaction::*;
pub use menu::*;
pub use observation::*;
pub use platform::*;
pub use settlement::*;
pub use target::*;
