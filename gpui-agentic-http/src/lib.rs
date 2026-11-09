//! `gpui-agentic-http`: a local-only HTTP surface for driving and
//! inspecting a gpui app without a human at the keyboard. See this crate's
//! own `README.md` for the full design rationale, ported from a
//! downstream gpui app's own debug HTTP module.

mod registry;
mod server;

pub use registry::{assert_registry_routable, Action, ModuleRegistration, Param, ParamLocation};
pub use server::{parse_query_string, AgenticCall, AgenticServer};
