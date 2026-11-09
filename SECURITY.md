# Security policy

## Reporting a vulnerability

Report a vulnerability privately through GitHub:
[Report a vulnerability](https://github.com/swatmobility/gpui-maps/security/advisories/new)
on the repository's Security tab. Do not open a public issue for it.

Tell us the affected crate and version, what an attacker can do, and how to
reproduce it. A malformed tile, style, GeoJSON, PMTiles or MBTiles file
that crashes or hangs the engine counts: the engine reads untrusted data
from the network.

## What happens next

- We confirm that we received the report within 3 business days.
- We tell you whether we accept it, and our plan, within 10 business days.
- We fix accepted issues in the latest release, and say in the release
  notes and a GitHub security advisory what was fixed. We credit you
  unless you ask us not to.

## Supported versions

Security fixes go into the latest minor release (`0.x`). Older versions
get fixes only under a commercial long-term support agreement (see
[SUPPORT.md](SUPPORT.md)).

## Scope

In scope: the crates in this repository (`gpui-maps`, `swat-maps-doc`,
`swat-maps-dash`, `swat-maps-stac`, `gpui-agentic-http`) and the example
shells. The agent HTTP port (`gpui-agentic-http`) binds to the loopback
address only and is off unless an app turns it on; a way to reach it from
another machine is in scope.

Out of scope: third-party tile servers and styles, and the hosted SWAT
services, which have their own channels.
