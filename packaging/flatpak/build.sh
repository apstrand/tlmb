#!/usr/bin/env bash
# Builds the desktop Flatpak and publishes it into a repository.
#
# The repository is the release channel. It is plain static files: upload the
# directory anywhere HTTPS is served, and every installed copy finds the new
# build there and offers to install it from inside the window (mailbox-ui's
# update.rs). Keep one repository for the life of the app and export every
# release into it — its history is what an update is a step along.
#
#   packaging/flatpak/build.sh [--install] [--bundle] [--run]
#
#   --install   install (or update) the result for this user, from the repo
#   --bundle    also write mailbox.flatpak: a one-file installer that sets up
#               the repository at MAILBOX_FLATPAK_URL, so it updates too
#   --run       run the installed app afterwards
#
# Environment:
#   MAILBOX_FLATPAK_REPO         the repository (default: .build/repo here)
#   MAILBOX_FLATPAK_URL          where that repository will be served; written
#                                into the .flatpakref, .flatpakrepo and bundle
#   MAILBOX_FLATPAK_GPG_KEY      key id to sign commits and the summary with
#   MAILBOX_FLATPAK_GPG_HOMEDIR  the GnuPG home holding that key
#   MAILBOX_GOOGLE_CLIENT_ID …   OAuth2 client credentials, compiled in exactly
#                                as a `cargo build` would (README, "Building")
#
# Unsigned repositories work, but have to be added with --no-gpg-verify, and
# then nothing but TLS vouches for what an update installs. Sign releases.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
app_id=io.github.pstrand.Mailbox
branch=stable
work="$here/.build"
repo="${MAILBOX_FLATPAK_REPO:-$work/repo}"
url="${MAILBOX_FLATPAK_URL:-}"
key="${MAILBOX_FLATPAK_GPG_KEY:-}"
homedir="${MAILBOX_FLATPAK_GPG_HOMEDIR:-}"

install=0 bundle=0 run=0
for arg in "$@"; do
    case "$arg" in
        --install) install=1 ;;
        --bundle) bundle=1 ;;
        --run) install=1 run=1 ;;
        -h|--help) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

if [[ $bundle == 1 && -z $url ]]; then
    echo "--bundle needs MAILBOX_FLATPAK_URL: a bundle without it cannot update" >&2
    exit 2
fi

if command -v flatpak-builder >/dev/null; then
    builder=(flatpak-builder)
elif flatpak info --user org.flatpak.Builder >/dev/null 2>&1 \
    || flatpak info --system org.flatpak.Builder >/dev/null 2>&1; then
    builder=(flatpak run org.flatpak.Builder)
else
    echo "needs flatpak-builder: flatpak install flathub org.flatpak.Builder" >&2
    exit 1
fi

gpg_args=()
if [[ -n $key ]]; then
    gpg_args+=("--gpg-sign=$key")
    [[ -n $homedir ]] && gpg_args+=("--gpg-homedir=$homedir")
fi

# Signing starts a gpg-agent for a non-default GnuPG home, which daemonizes
# and would otherwise outlive this script. Signing happens here on the host,
# not inside flatpak-builder: an agent started in the builder's sandbox has
# its socket in that sandbox's private /run, cannot be stopped from outside,
# and keeps the sandbox — and anything reading this script's output — alive.
stop_agent() {
    [[ -n $key ]] || return 0
    local args=()
    [[ -n $homedir ]] && args=(--homedir "$homedir")
    gpgconf "${args[@]}" --kill gpg-agent 2>/dev/null || true
}
trap stop_agent EXIT

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
revision="$(git -C "$root" describe --always --dirty 2>/dev/null || echo unknown)"

mkdir -p "$work"

# ---- sources the sandbox cannot fetch ----

echo "vendoring crates"
(cd "$root" && cargo vendor --locked --versioned-dirs --quiet "$work/vendor" >/dev/null)
cat >"$work/cargo-config.toml" <<'EOF'
# Relative to the parent of this file's directory: /run/build/mailbox.
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
EOF

# Values single-quoted for `sh`, so a secret containing `$` or a quote arrives
# as itself.
: >"$work/oauth.env"
chmod 600 "$work/oauth.env"
for var in MAILBOX_GOOGLE_CLIENT_ID MAILBOX_GOOGLE_CLIENT_SECRET \
    MAILBOX_MICROSOFT_CLIENT_ID MAILBOX_MICROSOFT_CLIENT_SECRET \
    MAILBOX_YAHOO_CLIENT_ID MAILBOX_YAHOO_CLIENT_SECRET; do
    if [[ -n ${!var:-} ]]; then
        printf "%s='%s'\n" "$var" "${!var//\'/\'\\\'\'}" >>"$work/oauth.env"
    fi
done

# ---- build and export ----

echo "building $app_id $version ($revision)"
"${builder[@]}" \
    --user \
    --force-clean \
    --install-deps-from=flathub \
    --state-dir="$work/state" \
    --default-branch="$branch" \
    --repo="$repo" \
    --subject="mailbox $version ($revision)" \
    "$work/app" \
    "$here/$app_id.yml"

if [[ -n $key ]]; then
    flatpak build-sign "${gpg_args[@]}" "$repo" "$app_id" "$branch"
fi

# Static deltas are what keep an update from being a download of every file
# that changed; pruning keeps the repository from growing without bound.
flatpak build-update-repo \
    --title=Mailbox \
    --default-branch="$branch" \
    --generate-static-deltas \
    --prune --prune-depth=10 \
    "${gpg_args[@]}" \
    "$repo"

# ---- the files a user installs from ----

gpg_line=""
if [[ -n $key ]]; then
    gpg_home_args=()
    [[ -n $homedir ]] && gpg_home_args=(--homedir "$homedir")
    gpg "${gpg_home_args[@]}" --export "$key" >"$work/key.gpg"
    gpg_line="GPGKey=$(base64 -w0 "$work/key.gpg")"
fi

if [[ -n $url ]]; then
    cat >"$repo/mailbox.flatpakrepo" <<EOF
[Flatpak Repo]
Title=Mailbox
Url=$url
Homepage=https://github.com/pstrand/mailbox
Comment=The Mailbox desktop app
$gpg_line
EOF
    cat >"$repo/mailbox.flatpakref" <<EOF
[Flatpak Ref]
Name=$app_id
Branch=$branch
Title=Mailbox
Url=$url
IsRuntime=false
SuggestRemoteName=mailbox
RuntimeRepo=https://dl.flathub.org/repo/flathub.flatpakrepo
$gpg_line
EOF
    echo "wrote $repo/mailbox.flatpakref — upload $repo to $url"
fi

if [[ $bundle == 1 ]]; then
    bundle_args=(--repo-url="$url" --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo)
    [[ -n $key ]] && bundle_args+=(--gpg-keys="$work/key.gpg")
    flatpak build-bundle "${bundle_args[@]}" "$repo" "$work/mailbox.flatpak" "$app_id" "$branch"
    echo "wrote $work/mailbox.flatpak"
fi

# ---- trying it here ----

if [[ $install == 1 ]]; then
    remote=mailbox-local
    if ! flatpak remotes --user --columns=name | grep -qx "$remote"; then
        verify=(--no-gpg-verify)
        [[ -n $key ]] && verify=(--gpg-import="$work/key.gpg")
        flatpak remote-add --user "${verify[@]}" "$remote" "file://$repo"
    fi
    # An install from a remote, not from the build directory: the origin is
    # what the in-app updater watches.
    flatpak install --user --noninteractive --or-update "$remote" "$app_id//$branch"
fi

if [[ $run == 1 ]]; then
    exec flatpak run "$app_id"
fi
