#!/bin/sh
# Build, sign, notarise and package Graviton.app for distribution.
#
#   scripts/release.sh             # Developer ID signing + notarisation -> dist/
#   scripts/release.sh --adhoc     # sign ad hoc, skip notarisation (checks the pipeline)
#   scripts/release.sh --no-notarize   # Developer ID signing only
#
# The app bundles everything it needs to run containers:
#   Contents/MacOS/Graviton        the app
#   Contents/MacOS/gravitond       the engine (virtualization entitlement)
#   Contents/MacOS/grav            the CLI
#   Contents/Resources/guest/      the guest kernel and initramfs
#
# One-time setup (see the README's Releasing section):
#   1. A "Developer ID Application" certificate in your login keychain.
#   2. Your team ID in apps/Config/Signing.local.xcconfig (GRAVITON_TEAM_ID = ...).
#   3. A notarytool profile in the keychain:
#        xcrun notarytool store-credentials graviton-notary --apple-id YOU --team-id TEAM
#      (it asks for an app-specific password and keeps it in the keychain).
#
# Overrides: GRAVITON_SIGNING_IDENTITY (a name or SHA-1 from `security
# find-identity -v -p codesigning`), GRAVITON_NOTARY_PROFILE (default graviton-notary).
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
dist="$repo/dist"
adhoc=0
notarize=1
for arg in "$@"; do
  case "$arg" in
    --adhoc) adhoc=1; notarize=0 ;;
    --no-notarize) notarize=0 ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "release: unknown option $arg" >&2; exit 2 ;;
  esac
done

step() { printf '\n==> %s\n' "$*"; }
fail() { printf 'release: %s\n' "$*" >&2; exit 1; }

for tool in cargo xcodegen xcodebuild codesign ditto hdiutil; do
  command -v "$tool" >/dev/null 2>&1 || fail "$tool isn't installed"
done

version=$(sed -n 's/^ *MARKETING_VERSION: *"\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' "$repo/apps/project.yml" | head -1)
[ -n "$version" ] || fail "couldn't read MARKETING_VERSION from apps/project.yml"

# --- Signing identity -------------------------------------------------------
team=$(sed -n 's/^ *GRAVITON_TEAM_ID *= *\([A-Z0-9]*\).*/\1/p' "$repo/apps/Config/Signing.local.xcconfig" 2>/dev/null | head -1)
if [ "$adhoc" = 1 ]; then
  identity="-"
else
  identity=${GRAVITON_SIGNING_IDENTITY:-}
  if [ -z "$identity" ]; then
    # The Developer ID Application certificate for your team (any, if no team is set).
    identity=$(security find-identity -v -p codesigning |
      sed -n 's/.*\([0-9A-F]\{40\}\) "\(Developer ID Application: .*(\([A-Z0-9]*\))\)"/\1 \3/p' |
      while read -r sha t; do [ -z "$team" ] || [ "$t" = "$team" ] && { echo "$sha"; break; }; done)
  fi
  [ -n "$identity" ] || fail "no Developer ID Application certificate${team:+ for team $team} in your keychain.
  Create one in Xcode (Settings > Accounts > Manage Certificates > +) or on developer.apple.com,
  or run with --adhoc to check the pipeline without one."
fi
sign_flags="--force --options runtime"
[ "$adhoc" = 1 ] || sign_flags="$sign_flags --timestamp"
profile=${GRAVITON_NOTARY_PROFILE:-graviton-notary}
if [ "$notarize" = 1 ]; then
  xcrun notarytool history --keychain-profile "$profile" >/dev/null 2>&1 ||
    fail "no notarytool profile named $profile in your keychain. Create it once with:
  xcrun notarytool store-credentials $profile --apple-id YOUR_APPLE_ID --team-id ${team:-YOUR_TEAM_ID}
  or run with --no-notarize."
fi

rm -rf "$dist/Graviton.app" "$dist/stage"
mkdir -p "$dist"

# --- Build ------------------------------------------------------------------
step "Building gravitond and grav (release)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=/build --remap-path-prefix=$repo=/src --remap-path-prefix=$cargo_home=/cargo"
cargo build --quiet --release --manifest-path "$repo/Cargo.toml" -p gravitond -p grav
target_dir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$repo/Cargo.toml" |
  python3 -I -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')

step "Building the guest kernel and initramfs"
guest=$("$repo/scripts/build-guest.sh" | tail -1 | sed 's/^Guest written to //')
[ -f "$guest/vmlinux" ] && [ -f "$guest/initrd.img" ] || fail "build-guest.sh didn't produce a guest in $guest"

step "Building Graviton.app (Release, signed below)"
(cd "$repo/apps" && xcodegen generate --quiet </dev/null)
xcodebuild -quiet -project "$repo/apps/Graviton.xcodeproj" -scheme GravitonMac -configuration Release \
  -destination 'platform=macOS,arch=arm64' ARCHS=arm64 -derivedDataPath "$dist/build" -skipPackagePluginValidation \
  OTHER_SWIFT_FLAGS="-debug-prefix-map $repo=/src -debug-prefix-map $HOME=/build" \
  OTHER_CFLAGS="-fdebug-prefix-map=$repo=/src -fdebug-prefix-map=$HOME=/build" CODE_SIGN_ALLOWED=NO build
cp -R "$dist/build/Build/Products/Release/Graviton.app" "$dist/Graviton.app"
app="$dist/Graviton.app"

step "Adding the engine, CLI and guest to the app"
cp "$target_dir/release/gravitond" "$target_dir/release/grav" "$app/Contents/MacOS/"
mkdir -p "$app/Contents/Resources/guest"
cp "$guest/vmlinux" "$guest/initrd.img" "$app/Contents/Resources/guest/"
[ -f "$guest/empty-disk.gvsp.gz" ] && cp "$guest/empty-disk.gvsp.gz" "$app/Contents/Resources/guest/"
cat > "$app/Contents/Resources/guest/SOURCES.txt" <<'TXT'
The guest kernel (vmlinux) is Linux 6.12 built from upstream kernel.org source
with scripts/kernel.config (or Alpine Linux's linux-virt package when built
with --alpine), and the initramfs (initrd.img) contains only the static Rust
graviton-agent binary as /init. Linux is licensed under the GNU GPL version 2.
Its source is available from:
  https://cdn.kernel.org/pub/linux/kernel/v6.x/
  https://pkgs.alpinelinux.org/packages?name=linux-virt&branch=v3.22&arch=aarch64
  https://gitlab.alpinelinux.org/alpine/aports (build recipes)
TXT

# --- Sign, inside out -------------------------------------------------------
step "Signing with ${identity}"
# shellcheck disable=SC2086
codesign $sign_flags --sign "$identity" --identifier dev.graviton.gravitond --entitlements "$repo/crates/gravitond/gravitond.entitlements" "$app/Contents/MacOS/gravitond"
# shellcheck disable=SC2086
codesign $sign_flags --sign "$identity" --identifier dev.graviton.grav "$app/Contents/MacOS/grav"
if [ -d "$app/Contents/PlugIns/GravitonWidgetMac.appex" ]; then
  # shellcheck disable=SC2086
  codesign $sign_flags --sign "$identity" --entitlements "$repo/apps/Widget/GravitonWidgetMac.entitlements" "$app/Contents/PlugIns/GravitonWidgetMac.appex"
fi
# shellcheck disable=SC2086
codesign $sign_flags --sign "$identity" --entitlements "$repo/apps/Mac/Graviton.entitlements" "$app"

step "Checking the signatures"
codesign --verify --strict --deep "$app"
codesign -d --entitlements - "$app/Contents/MacOS/gravitond" 2>/dev/null | grep -q com.apple.security.virtualization ||
  fail "gravitond lost its virtualization entitlement"
"$app/Contents/MacOS/grav" --version >/dev/null || fail "the bundled grav doesn't run"
"$app/Contents/MacOS/gravitond" --version >/dev/null || fail "the bundled gravitond doesn't run"
echo "Signatures are valid; gravitond has the virtualization entitlement."

# --- Notarise ---------------------------------------------------------------
notarize_file() {
  out=$(xcrun notarytool submit "$1" --keychain-profile "$profile" --wait --output-format json) || true
  id=$(printf '%s' "$out" | python3 -I -c 'import json,sys; print(json.load(sys.stdin).get("id",""))' 2>/dev/null || true)
  status=$(printf '%s' "$out" | python3 -I -c 'import json,sys; print(json.load(sys.stdin).get("status",""))' 2>/dev/null || true)
  if [ "$status" != "Accepted" ]; then
    [ -n "$id" ] && xcrun notarytool log "$id" --keychain-profile "$profile" >&2
    fail "notarisation of $(basename "$1") ended with status '${status:-unknown}'"
  fi
  echo "Notarised $(basename "$1") (submission $id)."
}

if [ "$notarize" = 1 ]; then
  step "Notarising the app (this usually takes a few minutes)"
  ditto -c -k --keepParent "$app" "$dist/notarize.zip"
  notarize_file "$dist/notarize.zip"
  rm -f "$dist/notarize.zip"
  xcrun stapler staple "$app"
  spctl --assess --type execute -vv "$app"
fi

# --- Package ----------------------------------------------------------------
step "Packaging"
zip="$dist/Graviton-$version.zip"
dmg="$dist/Graviton-$version.dmg"
rm -f "$zip" "$dmg"
ditto -c -k --keepParent "$app" "$zip"
mkdir -p "$dist/stage"
cp -R "$app" "$dist/stage/"
ln -s /Applications "$dist/stage/Applications"
hdiutil create -quiet -volname "Graviton $version" -srcfolder "$dist/stage" -ov -format UDZO "$dmg"
rm -rf "$dist/stage"
ts=""; [ "$adhoc" = 1 ] || ts="--timestamp"
codesign --force --sign "$identity" $ts "$dmg"
if [ "$notarize" = 1 ]; then
  step "Notarising the disk image"
  notarize_file "$dmg"
  xcrun stapler staple "$dmg"
fi

sums="$dist/SHA256SUMS.txt"
(cd "$dist" && shasum -a 256 "$(basename "$dmg")" "$(basename "$zip")" > "$(basename "$sums")")
dmg_sha=$(awk 'NR==1 {print $1}' "$sums")

step "Done"
if [ "$adhoc" = 1 ]; then
  echo "Ad hoc build (not for distribution: Gatekeeper will block it on other Macs):"
elif [ "$notarize" = 0 ]; then
  echo "Signed with Developer ID but not notarised (Gatekeeper will warn on other Macs):"
else
  echo "Signed, notarised and stapled:"
fi
echo "  $app"
echo "  $zip"
echo "  $dmg (sha256: $dmg_sha)"
echo "  $sums"
