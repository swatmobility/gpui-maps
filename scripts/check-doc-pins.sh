#!/usr/bin/env bash
# Fails when the install instructions a new user copies (the README, the
# getting-started guide and the site, English and Japanese) name a different
# gpui-pre or gpui-maps version from the engine's own Cargo.toml. A stale
# `=0.3.x` pin there makes a fresh app fail to resolve against the published
# crate, which is how the guide went wrong once.
#
#     ./scripts/check-doc-pins.sh [TREE]    (default: this repository)
set -euo pipefail
cd "${1:-$(dirname "$0")/..}"

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
minor=${version%.*}
gpui=$(sed -n 's/^gpui = { package = "gpui-pre", version = "=\([0-9.]*\)".*/\1/p' Cargo.toml | head -1)
if [ -z "$version" ] || [ -z "$gpui" ]; then
  echo "check-doc-pins: cannot read the version or the gpui-pre pin from Cargo.toml" >&2
  exit 1
fi

# The README is docs/public/README.md here and README.md after the cut.
files=""
for f in README.md docs/public/README.md docs/public/getting-started.md site/index.html site/ja/index.html; do
  [ -f "$f" ] && files="$files $f"
done

status=0
fail() { echo "check-doc-pins: $1" >&2; status=1; }

# Every exact gpui-pre pin (`version = "=0.3.N"` or `gpui-pre@=0.3.N`).
while IFS= read -r hit; do
  pin=$(printf '%s' "$hit" | grep -oE '(gpui-pre(-platform)?@=|version = "=)0\.[0-9]+\.[0-9]+' | grep -oE '0\.[0-9]+\.[0-9]+' | head -1)
  [ "$pin" = "$gpui" ] || fail "$hit  (expected =$gpui)"
done < <(grep -nHE 'gpui-pre(-platform)?(@=|", version = "=)' $files || true)

# Every `gpui-maps = "X.Y"` dependency line.
while IFS= read -r hit; do
  printf '%s' "$hit" | grep -qF "gpui-maps = \"$minor\"" || fail "$hit  (expected \"$minor\")"
done < <(grep -nHE '^gpui-maps = "' $files || true)

# The sample build output on the site.
while IFS= read -r hit; do
  printf '%s' "$hit" | grep -qF "gpui-maps v$version" || fail "$hit  (expected v$version)"
done < <(grep -nHE 'Compiling gpui-maps v' $files || true)

if [ "$status" -eq 0 ]; then
  echo "check-doc-pins: clean (gpui-maps $version, gpui-pre =$gpui)"
fi
exit "$status"
