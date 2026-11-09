//! A lenient reader for static STAC 1.1 catalogs and the Portolan profile
//! (portolan-sdi.org, spec v0.1–v0.2).
//!
//! - [`Node`] is one catalog, collection or item, parsed leniently: spec
//!   deviations become [`Warning`]s, never refusals ([`model`]).
//! - [`Catalog`] walks a catalog's `child` links lazily through an
//!   [`HttpCache`] that keeps copies on disk and revalidates them with
//!   ETag / Last-Modified ([`cache`]); the host supplies the HTTP
//!   [`Transport`].
//! - [`OverlayStyle`] turns a publisher's MapLibre style into the layers a
//!   fill/line/circle overlay draws, and reads its [`Legend`] ([`style`]).
//! - [`Licence`] classifies a collection's licence; [`portolan`] gates a
//!   curated listing on it and says what a host can draw ([`assess`]).
//! - [`agents_md::summary`] shortens an AGENTS.md for a card or a tool.
//!
//! No gpui, no engine, no HTTP client: it builds for wasm and iOS as it is.

pub mod agents_md;
pub mod cache;
pub mod catalog;
pub mod licence;
pub mod model;
pub mod portolan;
pub mod style;
pub mod url;

pub use cache::{Conditional, Fetched, HttpCache, Origin, Response, Transport};
pub use catalog::{Catalog, Tree};
pub use licence::{Licence, LicenceClass};
pub use model::{Asset, Column, Format, Link, Node, NodeKind, Provider, Warning};
pub use portolan::{assess, Assessment, MapPath, Support, TablePath};
pub use style::{Legend, LegendEntry, LegendKind, OverlayStyle};

#[cfg(test)]
mod tests;
