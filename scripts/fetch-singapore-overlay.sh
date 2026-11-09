#!/usr/bin/env bash
# Downloads the `overlay-features` panel's Singapore data into
# ~/.cache/gpui-map-data/singapore/ -- the same files, from the same URLs,
# the panel fetches itself on first open. The URLs live in ONE place,
# scripts/singapore-overlay-sources.txt, which the Rust panel also compiles in.
#
# Nothing here is committed: the data is licensed for reuse with attribution
# (Singapore Open Data Licence v1.0 for data.gov.sg; see the panel's credits).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
list="$here/singapore-overlay-sources.txt"
dest="${GPUI_MAP_SG_DATA:-$HOME/.cache/gpui-map-data/singapore}"
max_bytes=$((50 * 1024 * 1024))
ua="gpui-map-gallery (overlay-features)"
mkdir -p "$dest"

poll=$(awk '$1 == "POLL" { print $2 }' "$list")

fetch_to() { # url file
  local url="$1" out="$2" tmp="$2.part"
  curl -fsSL --proto '=https' --proto-redir '=https' --max-time 300 \
    --max-filesize "$max_bytes" -A "$ua" -o "$tmp" -- "$url"
  mv "$tmp" "$out"
}

grep -v '^#' "$list" | awk 'NF == 3 { print $1, $2, $3 }' | while read -r file kind target; do
  out="$dest/$file"
  if [[ -s "$out" ]]; then
    echo "have  $file"
    continue
  fi
  case "$kind" in
    url) fetch_to "$target" "$out" ;;
    datagovsg)
      url=""
      for attempt in 1 2 3 4 5; do
        body=$(curl -fsSL --proto '=https' --proto-redir '=https' --max-time 60 \
          -A "$ua" -- "${poll/\{id\}/$target}" || true)
        url=$(printf '%s' "$body" | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["data"]["url"])
except Exception: pass')
        [[ -n "$url" ]] && break
        echo "      data.gov.sg rate limit or error for $target; waiting 11 s (attempt $attempt)" >&2
        sleep 11
      done
      [[ -n "$url" ]] || { echo "could not resolve $target" >&2; exit 1; }
      fetch_to "$url" "$out"
      ;;
    *) echo "unknown kind $kind" >&2; exit 1 ;;
  esac
  echo "got   $file ($(wc -c < "$out" | tr -d ' ') bytes)"
done
echo "done: $dest"
