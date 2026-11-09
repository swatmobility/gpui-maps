//! The Studio Explorer's document model, kept free of gpui and of the
//! engine so it can be tested alone and compiled for the browser.
//!
//! - [`doc`]: the project document (`*.mapdoc.json`), round-trip safe.
//! - [`action`]: every edit is a [`action::DocAction`]; applying one returns
//!   the [`action::Effect`]s a host must perform on its map.
//! - [`table`]: the columnar, in-memory dataset a loader fills.
//! - [`fields`]: field-type detection (Kepler's types, our rules).
//! - [`defaults`]: the layer a freshly loaded dataset gets.
//! - [`filters`]: filter kinds, GPU slot allocation, the CPU mask.
//! - [`scales`]: which colour and size scales a field offers, and its
//!   first binding.
//! - [`labels`]: one name label per place, at its heaviest row.
//! - [`tooltip`]: the hover card's fields, order and formats.
//! - [`history`]: undo and redo as inverse actions.
//! - [`project`]: paths relative to the document, fingerprints, migrations.
//! - [`rows`]: the data table's listed rows, sort and column stats.
//! - [`sql`]: a database query as a source (explorer-sql §6.1).

pub mod action;
pub mod dashboard;
pub mod defaults;
pub mod doc;
pub mod fields;
pub mod filters;
pub mod history;
pub mod kepler;
pub mod labels;
pub mod project;
pub mod rows;
pub mod scales;
pub mod sql;
pub mod table;
pub mod tooltip;

pub use action::{Channel, DocAction, Effect, VisPatch};
pub use doc::{ChannelBinding, Doc, DocError, Filter, Layer, LayerType, Source, Vis};
pub use filters::{FilterKind, FilterValue, Placement};
pub use table::{Column, Field, FieldHint, FieldType, Geometry, Table};
