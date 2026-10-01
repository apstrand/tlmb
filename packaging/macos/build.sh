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
#   APPLE_API_KEY_BASE64     base64 of the .p8 holding it — the form a CI
#                            secret can be, and what the release workflow sets
#   APPLE_API_KEY_PATH       the .p8 itself, if you have it on disk; takes
#                            precedence over APPLE_API_KEY_BASE64
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

# notarytool reads the key from a file, and a secret is a string. So the
# base64 form becomes a file here rather than in the caller: one place that
# knows it is a key, mode 600 before anything is written to it, under TMPDIR
# instead of the work directory so it is never in the tree being built, and
# removed however this exits.
key_file=""
remove_key_file() { [[ -n $key_file ]] && rm -f "$key_file"; return 0; }
trap remove_key_file EXIT

if [[ -z ${APPLE_API_KEY_PATH:-} && -n ${APPLE_API_KEY_BASE64:-} ]]; then
    key_file="$(mktemp "${TMPDIR:-/tmp}/mailbox-authkey.XXXXXXXX")"
    chmod 600 "$key_file"
    # -D, not --decode: the long option is recent, and this is the one every
    # macOS has had.
    base64 -D <<<"$APPLE_API_KEY_BASE64" >"$key_file" \
        || fail "APPLE_API_KEY_BASE64 is not base64"
    [[ -s $key_file ]] || fail "APPLE_API_KEY_BASE64 decoded to nothing"
    export APPLE_API_KEY_PATH="$key_file"
fi

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
    fail "a release needs APPLE_API_KEY, APPLE_API_ISSUER and one of APPLE_API_KEY_BASE64 or APPLE_API_KEY_PATH to notarize"
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

# The build stamps its own commit in (mailbox-core's `build.rs`); what it
# cannot work out for itself is whether this is the build that gets published.
export MAILBOX_BUILD_RELEASE=$([[ $release == 1 && $dry_run == 0 ]] && echo 1 || echo 0)

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

# `icon-macos.png`, not the master square: Finder draws the pixels it is given,
# so the rounded shape and the margin around it have to be in the file or the
# app sits in the Dock as a square tile beside everything else's squircle.
# `make-icon.sh` builds it from the master, and the master stays square because
# iOS masks for itself and must not be handed a pre-rounded icon.
#
# macOS wants every size in one .icns, and iconutil is the only thing that
# writes one. @2x and the plain size below it are the same pixels under two
# names, which is what a Retina display and a non-Retina one each ask for.
icon="$root/crates/mailbox-ui/icons/icon-macos.png"
iconset="$work/mailbox.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z $size $size "$icon" \
        --out "$iconset/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) "$icon" \
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

# ---- what the in-app updater reads ----
#
# Tauri's updater fetches `latest.json`, compares its version with the running
# one, and will not unpack the archive unless the detached signature verifies
# against the public key compiled into the app. That signature is the whole
# reason downloading a replacement for yourself is a reasonable thing to do,
# so a build without the key publishes no manifest at all rather than one
# nothing can check.
#
# The archive is made after notarization so it carries the stapled ticket: a
# copy that lands on another machine has to launch without asking Apple.

# Appended to rather than collected and expanded: macOS ships bash 3.2, where
# `"${empty[@]}"` under `set -u` is an unbound variable rather than nothing at
# all. That killed the 0.1.11 release after it had signed and notarized, so
# nothing reached the release page.
assets=("$zip_path")
[[ $dmg == 1 ]] && assets+=("$dmg_path")

if [[ -n ${TAURI_SIGNING_PRIVATE_KEY:-} ]]; then
    tarball="$work/Mailbox-$version-universal.app.tar.gz"
    echo "packing $(basename "$tarball")"
    # -C so the archive holds `Mailbox.app`, not the path it was built at.
    tar -czf "$tarball" -C "$(dirname "$app")" "$(basename "$app")"

    echo "signing the update archive"
    # The file is positional — there is no `--file`. The key and its password
    # are read from the environment, so neither reaches a command line, and
    # the signature is written beside the archive as `<archive>.sig`.
    npx --yes @tauri-apps/cli@2 signer sign "$tarball"
    signature=$(cat "$tarball.sig")

    cat >"$work/latest.json" <<JSON
{
  "version": "$version",
  "pub_date": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "notes": "See https://github.com/$github/releases/tag/$tag",
  "platforms": {
    "darwin-universal": {
      "signature": "$signature",
      "url": "https://github.com/$github/releases/download/$tag/$(basename "$tarball")"
    },
    "darwin-aarch64": {
      "signature": "$signature",
      "url": "https://github.com/$github/releases/download/$tag/$(basename "$tarball")"
    },
    "darwin-x86_64": {
      "signature": "$signature",
      "url": "https://github.com/$github/releases/download/$tag/$(basename "$tarball")"
    }
  }
}
JSON
    assets+=("$tarball" "$work/latest.json")
else
    echo "no TAURI_SIGNING_PRIVATE_KEY — publishing without an update manifest"
fi

# ---- the release ----

notes="Download Mailbox-$version-universal.dmg below, open it, and drag
Mailbox to Applications. Universal: Apple silicon and Intel."

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
