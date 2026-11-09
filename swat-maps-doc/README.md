# swat-maps-doc

The crate is published as `swat-maps-doc`; its library path is `swat_maps_doc`.

The map document format of [SWAT Maps](https://github.com/swatmobility/gpui-maps):
a project file (`*.mapdoc.json`) of datasets, layers, filters, colour and size
scales and tooltips, and the actions that edit it. Applying an action returns
the effects a host must perform on its map, and undo/redo are inverse actions.

Pure Rust (serde, serde_json, sha2, thiserror): no GPU, no gpui, no engine
dependency, and it compiles for `wasm32-unknown-unknown`.

Modules: `doc`, `action`, `table`, `fields`, `defaults`, `filters`, `scales`,
`tooltip`, `history`, `project`, `rows`, `sql` and `kepler` (import and export
of Kepler.gl map configs).

See the repository README for the licence and attribution.
