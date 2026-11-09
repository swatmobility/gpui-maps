---
name: agentic-app
description: Use when a task needs to interact with a running gpui app that might expose a local agentic HTTP surface (open a screen, click a button, read what's on screen) -- before defaulting to "I can't control a native GUI window" or asking the user to click through the UI by hand.
---

# Driving a gpui app through its own agentic HTTP surface

Some gpui apps expose a local-only HTTP surface for exactly this: driving
and inspecting the app without a human at the keyboard. **Check for one
before telling the user you can't interact with a native window.**

This skill's own content is deliberately a bootstrap, not a reference --
the actual list of what an app can do is never written down here, because
it would go stale the day that app's own registry grows. The live app is
always the source of truth.

## How to find one

1. Scan the shared discovery range, `7330`–`7339`, for anything
   listening on `127.0.0.1`:

   ```bash
   for port in $(seq 7330 7339); do
     curl -s --max-time 1 "http://127.0.0.1:$port/" && echo " <- port $port"
   done
   ```

2. Each response that comes back is JSON with an `"app"` field naming the
   app and an `"actions"` array -- read `"app"` to find the one relevant
   to your current task. If nothing responds on any port in the range, no
   agentic-enabled app is currently running; fall back to telling the
   user what you'd have driven and why.

## How to use one, once found

`GET /` on that port again (or reuse the response from the scan) for the
full, current action list -- each entry names its own `method`, `path`,
and `params` (with each param's `location`: `path`, `query`, or `body`).
Call actions exactly as that response describes them, not from memory of
a previous session or this file's own examples -- the registry is the
only thing that can't drift out of sync with what the app actually does.

**Never hardcode a specific app's action names in your own responses to
the user as if they were guaranteed to exist.** Always confirm against a
fresh `GET /` first; an action can be renamed or removed between one
session and the next the same way any other code changes.
