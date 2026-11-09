#!/usr/bin/env bash
# Per-frame proof for re-blessing story goldens (spec §14.1).
#   scripts/story-golden-proof.sh old <task-slug>                        # dump PNGs from the CURRENT code
#   scripts/story-golden-proof.sh dry <task-slug> [--mask M] name...     # dump the CURRENT code into new/,
#        # re-bless nothing, diff each named entry against old/ (and check the mask)
#   scripts/story-golden-proof.sh new <task-slug> [--mask M] name=reason [name=reason ...]
#        # ONE call naming every entry the change moves: dump, re-bless
#        # (all or nothing), then diff and write one proof row per entry
#
# --mask (P14): every differing pixel must lie inside the given rects, or
# the call fails (and `new` restores story_golden.txt and writes no proof
# row). M is either `x0,y0,x1,y1[;x0,y0,x1,y1...]` (physical pixels,
# half-open, the same rects for every entry) or `auto`: each entry's own
# card and fine-print rects, which the golden test prints as
# `story mask <name> <rects>` for the frame it just rendered -- the union
# of the rects `old` mode's run printed (the BASE code's layout, when it
# prints them; saved in old.log) and this run's (the head's), so a card
# that moves or shrinks is covered where it was and where it is. An entry
# with no rects printed at all is refused (fails closed). (At R12's base,
# f2b79615, the test did not print rects yet; R12's head prints a
# "before R12" footprint of its own -- story_chrome::LayoutFonts::
# design_rects' unstyled pass -- so its masks still cover both.)
set -euo pipefail
: "${CARGO_TARGET_DIR:?set the build env}"
mode=$1 slug=$2; shift 2
mask=
if [ "${1:-}" = --mask ]; then
  [ $# -ge 2 ] || { echo "--mask needs rects or auto" >&2; exit 2; }
  mask=$2; shift 2
fi
root=$(git rev-parse --show-toplevel)
dir=${GPUI_MAP_PROOF_DIR:-$HOME/.cache/gpui-map-proof}/story-design/$slug
mkdir -p "$dir/old" "$dir/new"
free=$(df -g "$HOME" | awk 'NR==2{print $4}'); [ "$free" -ge 4 ] || { echo "only ${free} GB free: stop" >&2; exit 3; }
test_name=capture::golden_tests::every_story_golden_is_bit_identical
# The golden test run. STORY_PROOF_RUNNER replaces it with a script of
# your own (it sees the same GPUI_MAP_* environment) -- for exercising
# this script's refusals without a GPU run.
run_test() {
  export GPUI_MAP_IOU_DUMP
  if [ -n "${STORY_PROOF_RUNNER:-}" ]; then "$STORY_PROOF_RUNNER"; else cargo test --lib "$test_name" -- --exact --nocapture; fi
}
if [ "$mode" = old ]; then
  (unset GPUI_MAP_BLESS_STORY_GOLDEN GPUI_MAP_REBLESS_STORY_GOLDEN; GPUI_MAP_IOU_DUMP="$dir/old" run_test) 2>&1 | tee "$dir/old.log" || true
  exit 0
fi
case $mode in dry|new) ;; *) echo "usage: $0 old|dry|new <task-slug> [--mask M] ..." >&2; exit 2 ;; esac
[ $# -ge 1 ] || { echo "usage: $0 $mode <task-slug> [--mask M] name[=reason]..." >&2; exit 2; }
names=() reasons=()
for pair in "$@"; do
  if [ "$mode" = new ]; then
    case $pair in *=?*) ;; *) echo "$pair: expected name=reason" >&2; exit 2 ;; esac
    names+=("${pair%%=*}"); reasons+=("${pair#*=}")
  else
    names+=("${pair%%=*}"); reasons+=("")
  fi
done
csv=$(IFS=,; echo "${names[*]}")
log="$dir/$mode.log"
golden=$root/tests/fixtures/story_golden.txt
before=$dir/story_golden.before
if [ "$mode" = new ]; then
  # What the file held before this call: a refused mask restores THIS,
  # not HEAD (an earlier, uncommitted re-bless must survive).
  cp "$golden" "$before"
  # Fails closed: ANY exit before the proof rows are written (a refused
  # mask, a missing PNG, an unexpected error) puts the file back.
  done_ok=0
  trap '[ "$done_ok" = 1 ] || { cp "$before" "$golden"; echo "restored story_golden.txt to its state before this call" >&2; }' EXIT
  # Fails (and writes nothing) unless every listed name rendered and no
  # unlisted entry changed: set -e then stops before any proof row.
  GPUI_MAP_IOU_DUMP="$dir/new" GPUI_MAP_REBLESS_STORY_GOLDEN="$csv" run_test 2>&1 | tee "$log"
  [ "${PIPESTATUS[0]}" -eq 0 ] || exit 1
else
  # Re-blesses nothing, whatever the caller's environment holds.
  (unset GPUI_MAP_BLESS_STORY_GOLDEN GPUI_MAP_REBLESS_STORY_GOLDEN; GPUI_MAP_IOU_DUMP="$dir/new" run_test) 2>&1 | tee "$log" || true
fi
# Never fails: a missing `story mask` line yields "" and the caller
# refuses the entry (set -e must not end the script half-way, with the
# golden already re-blessed and no proof row written).
mask_for() {
  case $mask in
    '') echo ;;
    auto)
      local base head
      base=$(grep -E "^story mask $1 " "$dir/old.log" 2>/dev/null | tail -1 | cut -d' ' -f4- || true)
      head=$(grep -E "^story mask $1 " "$log" 2>/dev/null | tail -1 | cut -d' ' -f4- || true)
      [ -n "$head" ] || { echo; return 0; }
      echo "${base:+$base;}$head" | tr ';' '\n' | awk 'NF && !seen[$0]++' | paste -sd';' - ;;
    *) echo "$mask" ;;
  esac
}
rows=() bad=0
for i in "${!names[@]}"; do
  n=${names[$i]} r=${reasons[$i]}
  if [ -f "$dir/old/$n.png" ]; then
    m=$(mask_for "$n")
    if [ -n "$mask" ] && [ -z "$m" ]; then echo "$n: no mask rects printed" >&2; bad=1; continue; fi
    if [ -n "$m" ]; then
      d=$(python3 "$root/scripts/png-diff.py" "$dir/old/$n.png" "$dir/new/$n.png" --mask "$m") || bad=1
      d="$d mask=$m"
    else
      d=$(python3 "$root/scripts/png-diff.py" "$dir/old/$n.png" "$dir/new/$n.png") || bad=1
    fi
  else d="new entry"; fi
  rows+=("| $slug | \`$n\` | $d | $r | \`$dir/{old,new}/$n.png\` |")
  echo "$n: $d${r:+ -- $r}"
done
if [ "$bad" -ne 0 ]; then
  echo "refused: a differing pixel lies outside the mask, or an entry printed no mask" >&2
  exit 1
fi
if [ "$mode" = new ]; then
  proof=$root/docs/superpowers/proof/2026-10-story-design-rebless.md
  printf '%s\n' "${rows[@]}" >> "$proof"
  done_ok=1
fi
