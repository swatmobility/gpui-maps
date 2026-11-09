# swat-maps-dash

The crate is published as `swat-maps-dash`; its library path is `swat_maps_dash`.

The dashboard format of [SWAT Maps](https://github.com/swatmobility/gpui-maps):
a dashboard page is one self-contained JSON file with its SQL inline. This
crate holds what every host needs to produce the same page:

- `spec`: the page types (strict, `deny_unknown_fields`) and `validate`;
- `sql`: the template lexer and compiler, the ClickHouse and Postgres
  dialects, and the read guard;
- `hook`: the mandatory result hook and its column-stored tables;
- `connect`: connections, secrets and redaction, an `HttpTransport` trait and
  a sans-IO ClickHouse connector with its RowBinary decoder;
- `layout`: the grid solver; `testing`: `check_collection` for a consumer's CI.

No database engine is linked and nothing does IO by itself. Features: `fs`
(the JSON-file state store) and `http-native` (a reqwest transport), both on
by default; `http-web` (the browser's `fetch`); `fixture` (an in-memory
database for tests and demos).

See the repository README for the licence and attribution.
