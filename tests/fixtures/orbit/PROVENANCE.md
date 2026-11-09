# Orbit fixtures

| File | Source | SHA-256 of the source | Notes |
|---|---|---|---|
| stations.json | CelesTrak `gp.php?GROUP=stations&FORMAT=json`, fetched 2026-09-29 (one request, HTTP 200) | aba8e968be18ff16264b54d0aa474a838d8fba8919c376ab17e562bcb363f962 | 22 records, 9,310 B, unmodified. Data: US Space Force via CelesTrak. |
| vallado_subset.tle | Vallado et al., AIAA 2006-6753, `SGP4-VER.TLE`, via github.com/brandon-rhodes/python-sgp4 (`sgp4/SGP4-VER.TLE`, master) | d246d1d9d768ace445a38a965713fa9ba52d80fd8a41a0502ff83d7acffe2881 | cases 00005 (near-Earth, "TEME example"), 08195 (MOLNIYA 2-14, "12h resonant ecc in 0.65 to 0.7 range"), 11801 ("Original STR#3 SDP4 test", e = 0.7318) only; line 2 cut to its 69 TLE columns (the start/stop/step columns dropped) |
| vallado_subset.out | same, `tcppver.out` (`sgp4/tcppver.out`, master) | 687bf28dbe52df86e8e60ab5cb4a08d1aa3dbcaf4e63b1f7ab95f044fbe3833b | rows t = 0, 360, 1440 min for each of those cases; only the t, x, y, z, vx, vy, vz columns kept (values unchanged) |

The data terms and credit for every CelesTrak body (this fixture and the demo's own downloads) are in `demos/README.md`, *Satellite data (CelesTrak)*. A live `stations` fetch on 2026-09-29T05:13:40Z (satellites Task 8) returned this same body byte for byte (same SHA-256).
