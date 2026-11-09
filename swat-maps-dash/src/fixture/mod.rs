//! Fixture data: an in-memory [`Database`] and a small evaluator for the
//! SQL subset fixture pages compile to, behind [`FixtureSource`], so a page
//! renders with no server while its real compiled SQL and bound parameters
//! still choose the rows.
//!
//! Test and demo infrastructure, not a SQL engine (W6 holds: nothing here
//! links a database). Built for tests and with the `fixture` feature.

pub mod db;
pub mod eval;
mod parse;
pub mod source;

pub use db::{Database, Table, Val};
pub use eval::{execute, EvalError, ResultSet};
pub use source::{ExecutedQuery, FixtureSource, NoTransport};
