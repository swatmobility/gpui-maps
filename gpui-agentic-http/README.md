# gpui-agentic-http

A local-only HTTP surface for driving and inspecting a gpui app without a
human at the keyboard: open a screen, click a button, read back what is on
screen. These are actions that an agent calls over HTTP, not OS-level GUI
automation.

Part of [SWAT Maps](https://github.com/swatmobility/gpui-maps). The engine's
examples and `swat-maps-kit` use it, and any gpui app can use it alone.

## What this crate provides

- `Action`, `Param` and `ParamLocation`: a small, serializable description
  of one HTTP-callable capability.
- `ModuleRegistration<T>`: the action list of one app module, plus its
  dispatch function. A prefix keeps two modules from colliding.
- `AgenticServer<T>`: binds a loopback port and runs the accept loop on a
  thread. It sends each request to the app's foreground thread through a
  one-shot channel. It answers `GET /` with the actions of every registered
  module, merged into one self-describing list, plus an `"app"` field that
  names the app.
- A registry drift-test helper: a test fails when an action is documented
  but not routed, or routed but not documented.
- `skills/agentic-app/SKILL.md`: a bootstrap Claude Code skill. It scans
  the shared port range, reads `GET /` from what it finds, and uses what
  that says. It never holds a fixed list of actions.
- `agentic_mcp_server`: an MCP server over stdio. On every `tools/list` it
  reads the running app's `GET /` and offers each action as an MCP tool, so
  a new action appears as a tool with no change here.

## Using it

```toml
gpui-agentic-http = "0.1"
```

## Connecting Claude Code

Start your app with its agentic port open (the gpui-maps examples use
`GPUI_MAP_AGENTIC_PORT=7334`). Then either, or both:

**MCP server.** The app's actions become Claude Code tools:

```bash
cargo install gpui-agentic-http
claude mcp add gpui-app -e AGENTIC_HTTP_PORT=7334 -- agentic_mcp_server
```

`AGENTIC_HTTP_PORT` must be the port your app opened (7330 if unset).

**Skill.** Claude Code finds the port itself and calls the actions over
HTTP:

```bash
mkdir -p .claude/skills
cp -r gpui-agentic-http/skills/agentic-app .claude/skills/
```

The skill scans ports 7330 to 7339, so it needs no configuration.

## Licence

MIT OR Apache-2.0, as the rest of SWAT Maps: see `LICENSE-MIT` and
`LICENSE-APACHE` at the repository root.
