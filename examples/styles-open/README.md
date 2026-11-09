# Open look-alike basemap styles

Keyless stand-ins for the Mapbox-named gallery pages, drawn entirely from
open sources: no request goes to any Mapbox host for tiles, styles, sprites
or glyphs.

| File | Stands in for | Look |
|---|---|---|
| `open-streets-style.json` | Mapbox Streets (Streets, Nav drive, Tokyo) | light and warm, teal water, orange motorways, yellow trunks |
| `open-dark-style.json` | Mapbox Standard Dark | neutral night monochrome, quiet grey admin lines, no POIs or road names |
| `open-swat-routes-dark-style.json` | SWAT Routes Dark | quiet slate backdrop for route lines, near-black water |

## Provenance and licences

- **Base style:** OpenFreeMap Liberty (`examples/liberty-style.json`, as
  served by `tiles.openfreemap.org/styles/liberty`), a fork of
  maputnik/osm-liberty. Code BSD-3-Clause, design CC BY 4.0
  (openfreemap-styles `LICENSE.md`).
- **Our modifications:** colours, casings, label colours and density, set
  by `scripts/gen-open-styles.py` (layer ids are Liberty's). The colours
  were chosen by eye to resemble each Mapbox look. No Mapbox style
  document, layer list, sprite or code was copied or read into these files.
- **Tiles:** OpenFreeMap planet, OpenMapTiles schema, OpenStreetMap data
  (ODbL). Credit: "© OpenFreeMap © OpenMapTiles © OpenStreetMap contributors".
- **Glyphs:** Noto Sans (SIL OFL 1.1), served by OpenFreeMap.
- **Sprite:** OpenFreeMap's (`tiles.openfreemap.org/sprites/ofm_f384/ofm`).

Regenerate after editing the script: `python3 -I scripts/gen-open-styles.py`.
