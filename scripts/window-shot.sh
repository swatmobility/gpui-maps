#!/usr/bin/env bash
# Captures one process's window to a PNG, found by PID only:
#
#     scripts/window-shot.sh <pid> <out.png> [timeout_s]
#
# Picks the largest layer-0 window the PID owns (CGWindowListCopyWindowInfo,
# so helper and stray windows are skipped) and captures it with
# `screencapture -x -o -l<window id>`: no sound, no shadow, that window's
# own pixels whatever covers it. Never by title or by the frontmost app --
# two maps open at once must not swap shots -- and it never raises,
# clicks or focuses anything (no osascript): a window that is not
# frontmost may not paint, so prove the frame painted (`draw_calls_total
# > 0` from the app's own counters) before trusting the PNG.
#
# Waits up to `timeout_s` (default 20) for the window to appear, then
# gives up: the deadline is checked before every lookup, each lookup is
# killed at the deadline, and the capture itself at 30 s. Exit: 0 written, 2 usage, 3 no such process, 4 no window by
# the deadline, 5 the capture failed or wrote nothing.
set -uo pipefail

if [ $# -lt 2 ] || [ $# -gt 3 ]; then
  echo "usage: $0 <pid> <out.png> [timeout_s]" >&2
  exit 2
fi
pid="$1"
out="$2"
timeout_s="${3:-20}"
case "$pid" in ''|*[!0-9]*) echo "window-shot: pid must be a number, got '$pid'" >&2; exit 2 ;; esac
case "$timeout_s" in ''|*[!0-9]*) echo "window-shot: timeout must be whole seconds, got '$timeout_s'" >&2; exit 2 ;; esac

# Runs "$@" killed after $1 seconds (perl's alarm: macOS has no
# `timeout`), so a hung swift or screencapture cannot outlive the deadline.
with_timeout() {
  local secs="$1"; shift
  perl -e 'alarm shift; exec @ARGV or exit 127' "$secs" "$@"
}

# The process exists: `kill -0` succeeds, or fails only for permission
# (EPERM: someone else's process, still alive).
process_exists() {
  local err
  err="$(kill -0 "$1" 2>&1)" && return 0
  case "$err" in *"not permitted"*) return 0 ;; esac
  return 1
}

# The largest layer-0 window of `pid` with a non-zero area, or nothing.
# Given at most $2 seconds.
window_of() {
  with_timeout "$2" swift - "$1" 2>/dev/null <<'SWIFT'
import CoreGraphics
let pid = Int32(CommandLine.arguments[1]) ?? -1
let list = (CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as? [[String: Any]]) ?? []
var best: (Int, Double) = (0, 0)
for w in list where (w[kCGWindowOwnerPID as String] as? Int32) == pid && (w[kCGWindowLayer as String] as? Int) == 0 {
  let b = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
  let a = (b["Width"] as? Double ?? 0) * (b["Height"] as? Double ?? 0)
  if a > best.1, let n = w[kCGWindowNumber as String] as? Int { best = (n, a) }
}
if best.0 != 0 { print(best.0) }
SWIFT
}

deadline=$(( $(date +%s) + timeout_s ))
wid=""
while :; do
  if ! process_exists "$pid"; then
    echo "window-shot: no process $pid" >&2
    exit 3
  fi
  left=$(( deadline - $(date +%s) ))
  if [ "$left" -le 0 ]; then
    echo "window-shot: process $pid showed no window within ${timeout_s}s" >&2
    exit 4
  fi
  wid="$(window_of "$pid" "$left")"
  [ -n "$wid" ] && break
  sleep 1
done

mkdir -p "$(dirname "$out")"
rm -f "$out"
if ! with_timeout 30 screencapture -x -o -l"$wid" "$out" || [ ! -s "$out" ]; then
  echo "window-shot: screencapture of window $wid (pid $pid) failed" >&2
  exit 5
fi
echo "window-shot: pid $pid window $wid -> $out"
