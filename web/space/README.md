# Space look textures

The two textures of the optional space look (`?look=space` in the web demo,
`src/space_assets.rs` in the engine). They are copies of the upstream bytes,
pinned by sha256 in `src/space_assets.rs`; `scripts/fetch-space-assets.sh`
uses them before it tries the network.

| File | Source | Credit |
|---|---|---|
| `starmap_2020_3840.jpg` | Deep Star Maps 2020, <https://svs.gsfc.nasa.gov/4851>, Wikimedia Commons 3840 px rendition | NASA/Goddard Space Flight Center Scientific Visualization Studio. Gaia DR2: ESA/Gaia/DPAC. Attribution required. |
| `cloud_combined_2048.jpg` | NASA Visible Earth, Blue Marble: Clouds, <https://visibleearth.nasa.gov/images/57747> | NASA Goddard Space Flight Center image by Reto Stoeckli. |

Both credits join the map's attribution while the space look is drawn.
