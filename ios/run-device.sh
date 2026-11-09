#!/usr/bin/env bash
# Builds, signs and installs the spike on a PHYSICAL iPhone.
#
#   ./run-device.sh          build, sign, install, launch
#   ./run-device.sh --list   just show the devices and profiles it can see
#
# The difference from `run.sh` is entirely code signing. A simulator accepts
# an unsigned directory; a phone accepts nothing that is not signed by a
# development identity AND accompanied by a provisioning profile that names
# both the App ID and the device's UDID. So this script does three things
# `run.sh` does not:
#
#   1. copies a matching profile in as `embedded.mobileprovision`;
#   2. derives the ENTITLEMENTS from that profile rather than writing them
#      by hand -- `application-identifier` must be `<team>.<bundle id>`
#      exactly, and getting it wrong is a install-time error with a message
#      that names neither value;
#   3. `codesign`s the bundle with the development identity.
#
# A drive from the HOME SCREEN. A launch from the home screen gets no launch
# arguments, so `--nav-button`/`--nav-sim` never reach it. Set GPUI_MAP_NAV
# at deploy time and the value is baked into the bundle instead:
#
#   GPUI_MAP_NAV=button ./run-device.sh     the engine-drawn Simulate control
#   GPUI_MAP_NAV=sim    ./run-device.sh     the drive starts at launch
#
# It becomes `LSEnvironment` -> `GPUI_MAP_NAV` in the bundle's Info.plist,
# written before signing. So do the optional GPUI_MAP_NAV_ROUTE,
# GPUI_MAP_NAV_SPEED, GPUI_MAP_NAV_AT, GPUI_MAP_NAV_PAUSED and
# GPUI_MAP_NAV_OFFROUTE, when they are set. The app reads the process
# environment first and this dictionary second (`bundle_env` in
# `src/app.rs`), so it works whether or not iOS applies the key itself.
# Without GPUI_MAP_NAV the bundle is exactly as before: no LSEnvironment.
#
# Population dots (Task 17). The 30″ world archive (~102 MB) is not
# bundled; it is copied into the app's data container, once:
#
#   GPUI_MAP_DOTS_ARCHIVE=~/.cache/gpui-map-data/ghsl/E2025_30ss.pmtiles ./run-device.sh
#
# lands it as Documents/E2025_30ss.pmtiles (the name the app looks for:
# `shell_fetch::PHONE_DOT_ARCHIVE`) after the install. The container
# survives re-installs of the same bundle id, so later deploys need not set
# it; when they do, the copy is skipped ONLY if the phone's listing
# (`devicectl device info files`) positively shows a file of that name and
# the same size. Anything else -- no listing, no such file, another size --
# copies, and says why (`copy_archive` below). The app draws dots
# whenever the file is there; GPUI_MAP_DOTS=off (baked like GPUI_MAP_NAV
# below) turns them off. GPUI_MAP_AT=india|dhaka|lon,lat,zoom (baked the
# same way) starts the camera at MG-25's views instead of Singapore.
#
# Race dots (race plan, MG-R15): the 2020 Census race archive (~786 MB) is
# copied the same way (and skipped the same way when already there):
#
#   GPUI_MAP_RACE_ARCHIVE=~/.cache/gpui-map-data/census2020/race-dots-2020.pmtiles \
#   GPUI_MAP_AT=nyc ./run-device.sh
#
# lands it as Documents/race-dots-2020.pmtiles (`shell_fetch::PHONE_RACE_ARCHIVE`)
# and, unless GPUI_MAP_DOTS is set, bakes GPUI_MAP_DOTS=race-dots-2020.pmtiles
# so the app draws it: it sees the "race" metadata and draws Walker's
# categorical dots (dot mode only). GPUI_MAP_AT=us|nyc|nyc13 are its views
# (nyc13: part tiles, up to 2x the budget -- the phone's ceiling).
# A later deploy without GPUI_MAP_RACE_ARCHIVE needs GPUI_MAP_DOTS=race-dots-2020.pmtiles.
#
# The build goes to ios/target, always: this script copies the binary from
# there, and a CARGO_TARGET_DIR inherited from the shell (or the user's
# cargo config's target-dir) would build somewhere else and install
# whatever stale binary ios/target last held.
#
# It CREATES nothing: no certificate, no profile, no keychain entry, no
# account change. It picks from what is already installed, and says so and
# stops if there is nothing that fits.
set -euo pipefail
cd "$(dirname "$0")"

# The archives are checked BEFORE the build and install, so a typo costs
# nothing. A race archive with no GPUI_MAP_DOTS implies drawing it.
RACE_ARCHIVE_NAME=race-dots-2020.pmtiles
for key in GPUI_MAP_DOTS_ARCHIVE GPUI_MAP_RACE_ARCHIVE; do
    value="${!key:-}"
    [ -z "$value" ] || [ -f "$value" ] || { echo "$key: no file at $value" >&2; exit 1; }
done
if [ -n "${GPUI_MAP_RACE_ARCHIVE:-}" ] && [ -z "${GPUI_MAP_DOTS:-}" ]; then
    GPUI_MAP_DOTS="$RACE_ARCHIVE_NAME"
    echo "dots:     GPUI_MAP_RACE_ARCHIVE set, so GPUI_MAP_DOTS=$GPUI_MAP_DOTS"
fi

TARGET=aarch64-apple-ios
PROFILE_DIR=debug
BUNDLE_ID=dev.gpuimaps.iosdemo
APP=target/gpui-map-device.app
PROFILES="$HOME/Library/Developer/Xcode/UserData/Provisioning Profiles"

# --- pick a device -------------------------------------------------------
# The identifier is picked out by its SHAPE, not by column number: the table
# has a device name, a state and a model that are all several words wide and
# all vary per device, so `$(NF-2)` lands somewhere different on every row.
DEVICE_ID="${IOS_DEVICE_ID:-$(xcrun devicectl list devices 2>/dev/null \
    | awk '/iPhone|iPad/ { for (i = 1; i <= NF; i++)
        if ($i ~ /^[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}$/) {
            print $i; exit } }')}"

# --- pick a profile ------------------------------------------------------
# Any profile whose `application-identifier` matches this bundle id, either
# exactly or through a `<team>.*` wildcard, and which lists at least one
# device (a store profile lists none and cannot be used for development).
pick_profile() {
    local f
    for f in "$PROFILES"/*.mobileprovision; do
        [ -e "$f" ] || continue
        security cms -D -i "$f" 2>/dev/null | python3 -c "
import sys, plistlib
p = plistlib.loads(sys.stdin.buffer.read())
e = p.get('Entitlements', {})
app_id = e.get('application-identifier', '')
team = e.get('com.apple.developer.team-identifier', '')
want = team + '.' + '$BUNDLE_ID'
ok = app_id == want or (app_id.endswith('.*') and want.startswith(app_id[:-1]))
if ok and p.get('ProvisionedDevices'):
    print(team)
    sys.exit(0)
sys.exit(1)
" > /tmp/gpui-map-team.$$ 2>/dev/null && { echo "$f"; return 0; }
    done
    return 1
}

if [ "${1:-}" = "--list" ]; then
    xcrun devicectl list devices
    echo "profile: $(pick_profile || echo NONE)"
    exit 0
fi

PROFILE="$(pick_profile)" || {
    echo "No installed provisioning profile matches $BUNDLE_ID and lists a device." >&2
    echo "This script does not create one -- that is an Xcode/account action." >&2
    exit 1
}
TEAM="$(cat /tmp/gpui-map-team.$$)"; rm -f /tmp/gpui-map-team.$$
IDENTITY="${IOS_IDENTITY:-$(security find-identity -v -p codesigning \
    | awk -F'"' '/Apple Development/ {print $2; exit}')}"

echo "device:   $DEVICE_ID"
echo "profile:  $PROFILE"
echo "team:     $TEAM"
echo "identity: $IDENTITY"

export CARGO_TARGET_DIR="$PWD/target"
cargo build --target "$TARGET"

rm -rf "$APP"
mkdir -p "$APP"
cp "target/$TARGET/$PROFILE_DIR/gpui-map-demo" "$APP/gpui-map-demo"
cp Info.plist "$APP/Info.plist"
# The one key that differs from the simulator bundle. Kept as a patch of the
# single shared Info.plist rather than a second copy of it, so the two
# bundles cannot drift in anything else.
/usr/libexec/PlistBuddy -c "Set :CFBundleSupportedPlatforms:0 iPhoneOS" "$APP/Info.plist"
/usr/libexec/PlistBuddy -c "Add :DTPlatformName string iphoneos" "$APP/Info.plist" 2>/dev/null || true
# The opt-in home-screen drive (see the header). Only when asked for, and
# only the two values the app understands: anything else would be baked in
# and then silently ignored on the phone.
if [ -n "${GPUI_MAP_NAV:-}" ] || [ -n "${GPUI_MAP_DOTS:-}" ] || [ -n "${GPUI_MAP_AT:-}" ]; then
    /usr/libexec/PlistBuddy -c "Add :LSEnvironment dict" "$APP/Info.plist"
fi
for key in GPUI_MAP_DOTS GPUI_MAP_AT; do
    value="${!key:-}"
    [ -n "$value" ] || continue
    /usr/libexec/PlistBuddy -c "Add :LSEnvironment:$key string $value" "$APP/Info.plist"
    echo "dots:     LSEnvironment $key=$value"
done
if [ -n "${GPUI_MAP_NAV:-}" ]; then
    case "$GPUI_MAP_NAV" in
        sim|button) ;;
        *) echo "GPUI_MAP_NAV must be 'sim' or 'button', not '$GPUI_MAP_NAV'" >&2; exit 1 ;;
    esac
    for key in GPUI_MAP_NAV GPUI_MAP_NAV_ROUTE GPUI_MAP_NAV_SPEED GPUI_MAP_NAV_AT GPUI_MAP_NAV_PAUSED GPUI_MAP_NAV_OFFROUTE; do
        value="${!key:-}"
        [ -n "$value" ] || continue
        /usr/libexec/PlistBuddy -c "Add :LSEnvironment:$key string $value" "$APP/Info.plist"
        echo "nav:      LSEnvironment $key=$value"
    done
fi
cp "$PROFILE" "$APP/embedded.mobileprovision"

# Entitlements, taken FROM the profile. Everything the profile grants, with
# the wildcard resolved to this bundle's real identifier -- writing these by
# hand is how you get an `application-identifier` that disagrees with the
# embedded profile, which fails at install time with a message that names
# neither value.
security cms -D -i "$PROFILE" | python3 -c "
import sys, plistlib
p = plistlib.loads(sys.stdin.buffer.read())
e = dict(p['Entitlements'])
e['application-identifier'] = '$TEAM.$BUNDLE_ID'
sys.stdout.buffer.write(plistlib.dumps(e))
" > target/entitlements.plist

codesign --force --sign "$IDENTITY" \
    --entitlements target/entitlements.plist \
    --generate-entitlement-der \
    --timestamp=none \
    "$APP"
codesign --verify --verbose=2 "$APP"

xcrun devicectl device install app --device "$DEVICE_ID" "$APP"
# Copies archive $1 into the app's Documents as $2, unless the phone's
# listing positively shows $2 there at the same size. Every other case
# copies (a reset container must never be skipped), and prints why.
copy_archive() {
    local src="$1" name="$2" size listing device_size why
    size="$(stat -f %z "$src")"
    listing="$(mktemp -t gpui-map-files)"
    device_size=""
    why="the phone's Documents could not be listed"
    if xcrun devicectl device info files --device "$DEVICE_ID" \
        --domain-type appDataContainer --domain-identifier "$BUNDLE_ID" \
        --subdirectory Documents --json-output "$listing" >/dev/null 2>&1; then
        # The listing's shape is not documented: find any object naming the
        # file and read a size-like key from it; empty when none is found.
        device_size="$(python3 - "$listing" "$name" <<'PYEOF' 2>/dev/null || true
import json, sys
name = sys.argv[2]
def walk(o):
    if isinstance(o, dict):
        label = str(o.get("name") or o.get("relativePath") or o.get("path") or "")
        if label.rsplit("/", 1)[-1] == name:
            for k in ("size", "fileSize", "sizeInBytes"):
                if isinstance(o.get(k), int):
                    return o[k]
        for v in o.values():
            r = walk(v)
            if r is not None:
                return r
    elif isinstance(o, list):
        for v in o:
            r = walk(v)
            if r is not None:
                return r
    return None
r = walk(json.load(open(sys.argv[1])))
print("" if r is None else r)
PYEOF
)"
        why="the listing shows no Documents/$name with a size"
    fi
    rm -f "$listing"
    if [ -n "$device_size" ]; then
        if [ "$device_size" = "$size" ]; then
            echo "dots:     Documents/$name already on the phone ($size bytes), not copied"
            return 0
        fi
        why="the phone's copy is $device_size bytes, this one $size"
    fi
    echo "dots:     copying $src -> Documents/$name ($size bytes): $why"
    xcrun devicectl device copy to --device "$DEVICE_ID" \
        --domain-type appDataContainer --domain-identifier "$BUNDLE_ID" \
        --source "$src" --destination "Documents/$name"
}
if [ -n "${GPUI_MAP_DOTS_ARCHIVE:-}" ]; then
    copy_archive "$GPUI_MAP_DOTS_ARCHIVE" E2025_30ss.pmtiles
fi
if [ -n "${GPUI_MAP_RACE_ARCHIVE:-}" ]; then
    copy_archive "$GPUI_MAP_RACE_ARCHIVE" "$RACE_ARCHIVE_NAME"
fi
xcrun devicectl device process launch --console --device "$DEVICE_ID" "$BUNDLE_ID"
