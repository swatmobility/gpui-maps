//! `swat-maps-dash`: the declarative dashboard framework's pure crate.
//!
//! A dashboard is a *page*: one self-contained JSON file with its SQL inline
//! (spec `docs/superpowers/specs/2026-10-03-dashboard-framework-design.md`).
//! This crate holds what every host needs to produce the same dashboard:
//!
//! - [`spec`]: the page types (§3), strict (`deny_unknown_fields`);
//! - [`validate`]: [`validate::Diagnostic`] and the load-time codes;
//! - [`sql`]: the template lexer and compiler, the ClickHouse and Postgres
//!   dialects, the read guard, and [`sql::CompiledQuery::guarded`], the only
//!   way to read compiled SQL text (explorer-sql D2);
//! - [`hook`]: the mandatory result hook, the column-stored
//!   [`hook::RawTable`] and [`hook::Results`] (D4), and [`hook::run_query`],
//!   the only maker of `Results`;
//! - [`connect`]: connections, secrets, redaction, the
//!   [`connect::HttpTransport`] trait and the sans-IO ClickHouse connector
//!   with its RowBinary decoder;
//! - [`layout`]: the pure grid solver;
//! - [`testing`]: `check_collection`, the stable API a consumer runs in CI.
//!
//! No database engine is linked (W6), and nothing here does IO by itself.

pub mod connect;
/// In-memory fixture data and a SQL-subset evaluator, so pages render with
/// no server (tests, and the `fixture` feature).
#[cfg(any(test, feature = "fixture"))]
pub mod fixture;
pub mod format;
pub mod geo;
pub mod hook;
pub mod layout;
pub mod model;
pub mod store;
pub mod spec;
pub mod sql;
pub mod testing;
pub mod validate;

/// A boxed future: `Send` natively, and not on wasm32, where `fetch`
/// futures cannot be.
#[cfg(not(target_arch = "wasm32"))]
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
/// A boxed future: `Send` natively, and not on wasm32, where `fetch`
/// futures cannot be.
#[cfg(target_arch = "wasm32")]
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

/// Drives a future to completion on the current thread by polling it with a
/// no-op waker. For tests and for stub transports whose futures are ready
/// immediately; a host drives real transports on its own executor.
#[doc(hidden)]
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}
