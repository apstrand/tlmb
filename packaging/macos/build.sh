#!/usr/bin/env bash
# Builds the macOS desktop app, for trying here or for release.
#
#   packaging/macos/build.sh [--install | --run] [--no-dmg]
#       Build Mailbox.app into .build and, with --install, copy it to
#       /Applications; --run also opens it.
#
#   packaging/macos/build.sh --release [--dry-run]
#       Build the release for the version in Cargo.toml, sign it, notarize it,
#       staple the ticket, and attach the .dmg and the .zip to the GitHub
#       release v<version> — creating that release if the Flatpak job has not
#       already, and uploading into it if it has.
#
#       --dry-run does everything except the upload and the release, and
#       leaves the bundles in .build to look at.
#
# There is no Tauri CLI here, and no npm: `mailbox gui` is a subcommand of one
# Rust binary rather than a JavaScript project with a Rust shell, so the crate
# that holds tauri.conf.json (mailbox-ui) is a library and the binary is a
# different workspace member. `tauri build` cannot bundle that arrangement —
# it builds the crate its config sits in. What the bundler would do on macOS
# is a directory, a plist, an icon and codesign, which is what this does.
#
# The app does not update itself on macOS: mailbox-ui's update.rs speaks to
# the Flatpak portal and reports "unsupported" anywhere else. A new version is
# a new download until that changes.
#
# Environment:
#   APPLE_SIGNING_IDENTITY   "Developer ID Application: … (TEAMID)". Unset
#                            builds unsigned — fine here, useless to ship.
#   APPLE_API_KEY            App Store Connect key id, for notarization
#   APPLE_API_ISSUER         its issuer UUID
#   APPLE_API_KEY_PATH       the .p8 holding it
#   MAILBOX_MACOS_GITHUB     owner/repo the release is created in
#   MAILBOX_GOOGLE_CLIENT_ID …  OAuth2 client credentials, compiled in exactly
#                            as a `cargo build` would (README, "Building")

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
work="$here/.build"

github="${MAILBOX_MACOS_GITHUB:-apstrand/tlmb}"
targets=(aarch64-apple-darwin x86_64-apple-darwin)

install=0 run=0 release=0 dry_run=0 dmg=1
for arg in "$@"; do
    case "$arg" in
        --install) install=1 ;;
        --run) install=1 run=1 ;;
        --no-dmg) dmg=0 ;;
        --release) release=1 ;;
        --dry-run) dry_run=1 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done
if [[ $release == 1 && $install == 1 ]]; then
    echo "--release publishes; install the result from the .dmg it uploads" >&2
    exit 2
fi
if [[ $dry_run == 1 && $release == 0 ]]; then
    echo "--dry-run only means something with --release" >&2
    exit 2
fi

fail() { echo "$*" >&2; exit 1; }

[[ "$(uname -s)" == Darwin ]] || fail "this builds a macOS app, and only macOS has the tools to"
command -v xcrun >/dev/null || fail "needs the Xcode command line tools: xcode-select --install"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
revision="$(git -C "$root" describe --always --dirty 2>/dev/null || echo unknown)"
tag="v$version"
app="$work/Mailbox.app"
dmg_path="$work/Mailbox-$version-universal.dmg"
zip_path="$work/Mailbox-$version-universal.zip"

identity="${APPLE_SIGNING_IDENTITY:-}"
notarize=0
if [[ -n ${APPLE_API_KEY:-} && -n ${APPLE_API_ISSUER:-} && -n ${APPLE_API_KEY_PATH:-} ]]; then
    notarize=1
fi

# ---- before spending twenty minutes on two builds ----

if [[ -n $identity ]]; then
    security find-identity -v -p codesigning | grep -qF "$identity" \
        || fail "the signing identity is not in the keychain: $identity"
elif [[ $release == 1 ]]; then
    fail "a release needs APPLE_SIGNING_IDENTITY — an unsigned app is refused by Gatekeeper"
fi

if [[ $notarize == 1 ]]; then
    [[ -f ${APPLE_API_KEY_PATH} ]] || fail "APPLE_API_KEY_PATH is not a file: $APPLE_API_KEY_PATH"
elif [[ $release == 1 ]]; then
    fail "a release needs APPLE_API_KEY, APPLE_API_ISSUER and APPLE_API_KEY_PATH to notarize"
elif [[ -n $identity ]]; then
    echo "warning: signing without notarizing — a downloaded build still gets a Gatekeeper warning"
fi

if [[ $release == 1 && $dry_run == 0 ]]; then
    [[ $revision != *-dirty ]] || fail "the working tree has uncommitted changes"
    command -v gh >/dev/null || fail "needs the GitHub CLI (gh) to attach the release assets"
    # The Flatpak release may already own the tag: this job adds to it rather
    # than replacing it. What must not happen twice is the same version's own
    # assets, which is what says a version was released and never rebuilt.
    if gh release view "$tag" -R "$github" --json assets --jq '.assets[].name' 2>/dev/null \
        | grep -qx "$(basename "$zip_path")"; then
        fail "$github's release $tag already has $(basename "$zip_path") — bump the version in Cargo.toml"
    fi
fi

rm -rf "$work"
mkdir -p "$work"

# ---- the universal binary ----

if command -v rustup >/dev/null; then
    rustup target add "${targets[@]}" >/dev/null
fi

slices=()
for target in "${targets[@]}"; do
    echo "building $target"
    (cd "$root" && cargo build --release --locked -p mailbox-cli --target "$target")
    slices+=("$root/target/$target/release/mailbox")
done

echo "joining the slices"
lipo -create -output "$work/mailbox" "${slices[@]}"
lipo -info "$work/mailbox"

# ---- the bundle ----

echo "assembling Mailbox.app $version ($revision)"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 755 "$work/mailbox" "$app/Contents/MacOS/mailbox"
sed "s/__VERSION__/$version/g" "$here/Info.plist" >"$app/Contents/Info.plist"
# Classic, and still what a few Finder paths look at first.
printf 'APPL????' >"$app/Contents/PkgInfo"

# The icon is the same 1024px source the other packages scale from; macOS
# wants every size in one .icns, and iconutil is the only thing that writes
# one. @2x and the plain size below it are the same pixels under two names,
# which is what a Retina display and a non-Retina one each ask for.
iconset="$work/mailbox.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z $size $size "$root/crates/mailbox-ui/icons/icon.png" \
        --out "$iconset/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) "$root/crates/mailbox-ui/icons/icon.png" \
        --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/mailbox.icns"
rm -rf "$iconset"

# ---- signing ----

if [[ -n $identity ]]; then
    echo "signing with $identity"
    # No nested code and no --deep, which signs what it finds with the wrong
    # identity and is deprecated for exactly that. --options runtime is the
    # hardened runtime, which notarization requires; --timestamp is the
    # trusted timestamp, without which the signature dies with the certificate.
    codesign --force --timestamp --options runtime --sign "$identity" "$app"
    codesign --verify --strict --verbose=2 "$app"
fi

# ---- notarization ----

# Apple staples the ticket to the thing it was asked about, so the .app and
# the .dmg are each submitted: the .app so a copy out of the disk image
# launches offline, the .dmg so opening the download does.
submit() {
    echo "notarizing $(basename "$1")"
    xcrun notarytool submit "$1" \
        --key "$APPLE_API_KEY_PATH" \
        --key-id "$APPLE_API_KEY" \
        --issuer "$APPLE_API_ISSUER" \
        --wait --timeout 30m
}

if [[ $notarize == 1 ]]; then
    ditto -c -k --keepParent "$app" "$work/notarize.zip"
    submit "$work/notarize.zip"
    rm -f "$work/notarize.zip"
    xcrun stapler staple "$app"
fi

# ditto, not zip: it is what preserves a bundle's symlinks and resource forks,
# and what Archive Utility produces on the other end.
ditto -c -k --keepParent "$app" "$zip_path"

# ---- the disk image ----

if [[ $dmg == 1 ]]; then
    echo "building $(basename "$dmg_path")"
    staging="$work/dmg"
    mkdir -p "$staging"
    cp -R "$app" "$staging/"
    ln -s /Applications "$staging/Applications"
    hdiutil create -volname "Mailbox" -srcfolder "$staging" \
        -ov -format UDZO "$dmg_path" >/dev/null
    rm -rf "$staging"

    if [[ -n $identity ]]; then
        codesign --force --timestamp --sign "$identity" "$dmg_path"
    fi
    if [[ $notarize == 1 ]]; then
        submit "$dmg_path"
        xcrun stapler staple "$dmg_path"
    fi
fi

# ---- trying it here ----

if [[ $release == 0 ]]; then
    if [[ $install == 1 ]]; then
        echo "installing to /Applications"
        rm -rf /Applications/Mailbox.app
        cp -R "$app" /Applications/Mailbox.app
    fi
    [[ $run == 1 ]] && open -a /Applications/Mailbox.app
    echo "built $app"
    [[ $dmg == 1 ]] && echo "       $dmg_path"
    exit 0
fi

# ---- the release ----

assets=("$zip_path")
[[ $dmg == 1 ]] && assets+=("$dmg_path")

notes="Download Mailbox-$version-universal.dmg below, open it, and drag
Mailbox to Applications. Universal: Apple silicon and Intel. The app does not
update itself on macOS yet — a new version is a new download."

if [[ $dry_run == 1 ]]; then
    cat <<EOF

dry run — nothing was published. The bundles are in $work. A real run would now:
  gh release create $tag -R $github --target $(git -C "$root" rev-parse HEAD) \\
      ${assets[*]}
  (or, if the Flatpak release already made $tag, upload them into it)
EOF
    exit 0
fi

if gh release view "$tag" -R "$github" >/dev/null 2>&1; then
    echo "adding the macOS assets to release $tag"
    gh release upload "$tag" -R "$github" --clobber "${assets[@]}"
else
    echo "creating release $tag"
    gh release create "$tag" -R "$github" \
        --target "$(git -C "$root" rev-parse HEAD)" \
        --title "tlmb $version" \
        --notes "$notes" \
        "${assets[@]}"
fi

echo "released $tag: https://github.com/$github/releases/tag/$tag"
