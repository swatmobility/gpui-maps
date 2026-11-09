# `src/story` — where the ported code came from

## `story::fly` (`src/story/fly.rs`) — MapLibre GL JS `flyTo`, ported

The `fly` camera segment follows van Wijk and Nuij, "Smooth and efficient
zooming and panning" (InfoVis 2003), as **MapLibre GL JS** parameterises
it. The Rust is a port, line by line, of these files, read at
`maplibre-gl-js` (github.com/maplibre/maplibre-gl-js), commit
`0d4b6f1e7d53beac00a4423ce53e7c16a3353b5f`:

    src/ui/camera.ts                                              flyTo (w0, w1, u1, rho cap,
                                                                  zoomOutFactor, r0, w(s), u(s), S,
                                                                  the zoom-only case, duration,
                                                                  maxDuration, s = k * S)
    src/geo/projection/mercator_camera_helper.ts                  handleFlyTo: flat pixelPathLength,
                                                                  scaleOfZoom
    src/geo/projection/vertical_perspective_camera_helper.ts      handleFlyTo: globe pixelPathLength,
                                                                  normalised zooms, deltaLng, the
                                                                  per-frame zoom adjustment
    src/geo/projection/globe_utils.ts                             getGlobeRadiusPixels,
                                                                  getZoomAdjustment, integrateSecX,
                                                                  interpolateLngLatForGlobe

`src/story/fly.rs`'s module doc cites each formula at its line in those
files. **Mapbox GL JS 3.x was not read or ported.** `flyTo` predates the
MapLibre fork, so `LICENSE.txt`'s second notice (mapbox-gl-js v1.13 and
earlier, BSD-3) is carried below as well as MapLibre's own.

**BSD-3.** `LICENSE.txt` in that checkout, read rather than assumed:

    Copyright (c) 2023, MapLibre contributors

    All rights reserved.

    Redistribution and use in source and binary forms, with or without modification,
    are permitted provided that the following conditions are met:

        * Redistributions of source code must retain the above copyright notice,
          this list of conditions and the following disclaimer.
        * Redistributions in binary form must reproduce the above copyright notice,
          this list of conditions and the following disclaimer in the documentation
          and/or other materials provided with the distribution.
        * Neither the name of MapLibre GL JS nor the names of its contributors
          may be used to endorse or promote products derived from this software
          without specific prior written permission.

    THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
    "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
    LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
    A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR
    CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
    EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
    PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
    PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
    LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
    NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
    SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

    -------------------------------------------------------------------------------

    Contains code from mapbox-gl-js v1.13 and earlier

    Version v1.13 of mapbox-gl-js and earlier are licensed under a BSD-3-Clause license

    Copyright (c) 2020, Mapbox
    Redistribution and use in source and binary forms, with or without modification,
    are permitted provided that the following conditions are met:

    * Redistributions of source code must retain the above copyright notice,
      this list of conditions and the following disclaimer.
    * Redistributions in binary form must reproduce the above copyright notice,
      this list of conditions and the following disclaimer in the documentation
      and/or other materials provided with the distribution.
    * Neither the name of Mapbox GL JS nor the names of its contributors
      may be used to endorse or promote products derived from this software
      without specific prior written permission.

    THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
    "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
    LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
    A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR
    CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
    EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
    PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
    PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
    LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
    NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE,
    EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
