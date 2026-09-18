#!/usr/bin/env bash
# Builds the desktop Flatpak, for trying here or for release.
#
#   packaging/flatpak/build.sh [--install | --run]
#       Build into a local, unsigned repository (.build/local-repo) and, with
#       --install, install from it for this user; --run also opens it.
#
#   packaging/flatpak/build.sh --release [--dry-run]
#       Build the release for the version in Cargo.toml and publish it:
#
#       - the update repository, signed, under MAILBOX_FLATPAK_SITE/repo on
#         the web host. This is what every installed copy checks, and what the
#         window's Install button pulls from (mailbox-ui's update.rs). GitHub
#         Releases cannot hold it: a Flatpak repository is a tree of files at
#         fixed paths, and release assets are a flat list.
#       - a GitHub release, v<version>, carrying tlmb.flatpakref and the
#         one-file tlmb-<version>.flatpak installer. Both point at the site's
#         repository, so whichever someone installs from updates itself.
#
#       The published site is fetched from MAILBOX_FLATPAK_PUBLISH with rsync
#       first and the new build is exported on top of it: that history is what
#       an update is a step along, and what the static deltas are cut from. It
#       goes back up objects first, summary next, deletions last, so a client
#       fetching while that runs never sees a summary without its objects.
#
#       --dry-run does all of it except the upload and the release, and leaves
#       the site in .build/site to serve and install from locally.
#
# Environment (defaults are this project's):
#   MAILBOX_FLATPAK_GPG_KEY      signing key fingerprint
#   MAILBOX_FLATPAK_GPG_HOMEDIR  GnuPG home holding it (default: GnuPG's own)
#   MAILBOX_FLATPAK_SITE         where the site is served
#   MAILBOX_FLATPAK_PUBLISH      [user@]host:path rsync publishes the site to
#   MAILBOX_FLATPAK_GITHUB       owner/repo the release is created in
#   MAILBOX_GOOGLE_CLIENT_ID …   OAuth2 client credentials, compiled in exactly
#                                as a `cargo build` would (README, "Building")

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
app_id=com.thelastmailbox.flat
branch=stable
work="$here/.build"

key="${MAILBOX_FLATPAK_GPG_KEY:-772EE1812AA4D782E519E62E4496C4F85CE95DB1}"
homedir="${MAILBOX_FLATPAK_GPG_HOMEDIR:-}"
site="${MAILBOX_FLATPAK_SITE:-https://tlmb-flat.2615.io}"
publish="${MAILBOX_FLATPAK_PUBLISH:-peter@wonko.nena.se:web/tlmb-flat}"
github="${MAILBOX_FLATPAK_GITHUB:-apstrand/tlmb}"
site="${site%/}"
publish="${publish%/}"
publish_host="${publish%%:*}"
publish_path="${publish#*:}"
url="$site/repo"
runtime_repo=https://dl.flathub.org/repo/flathub.flatpakrepo

install=0 run=0 release=0 dry_run=0
for arg in "$@"; do
    case "$arg" in
        --install) install=1 ;;
        --run) install=1 run=1 ;;
        --release) release=1 ;;
        --dry-run) dry_run=1 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done
if [[ $release == 1 && $install == 1 ]]; then
    echo "--release publishes; install the result from $site/tlmb.flatpakref" >&2
    exit 2
fi
if [[ $dry_run == 1 && $release == 0 ]]; then
    echo "--dry-run only means something with --release" >&2
    exit 2
fi

fail() { echo "$*" >&2; exit 1; }

if command -v flatpak-builder >/dev/null; then
    builder=(flatpak-builder)
elif flatpak info --user org.flatpak.Builder >/dev/null 2>&1 \
    || flatpak info --system org.flatpak.Builder >/dev/null 2>&1; then
    builder=(flatpak run org.flatpak.Builder)
else
    fail "needs flatpak-builder: flatpak install flathub org.flatpak.Builder"
fi

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
revision="$(git -C "$root" describe --always --dirty 2>/dev/null || echo unknown)"
tag="v$version"

gpg_args=() gpg_home_args=()
if [[ -n $homedir ]]; then
    gpg_home_args=(--homedir "$homedir")
fi

# Signing starts a gpg-agent for a non-default GnuPG home, which daemonizes
# and would otherwise outlive this script. Signing happens here on the host,
# not inside flatpak-builder: an agent started in the builder's sandbox has
# its socket in that sandbox's private /run, cannot be stopped from outside,
# and keeps the sandbox — and anything reading this script's output — alive.
stop_agent() {
    [[ -n $homedir ]] || return 0
    gpgconf "${gpg_home_args[@]}" --kill gpg-agent 2>/dev/null || true
}
trap stop_agent EXIT

# ---- before spending ten minutes on a build ----

if [[ $release == 1 ]]; then
    gpg "${gpg_home_args[@]}" --list-secret-keys "$key" >/dev/null 2>&1 \
        || fail "the signing key $key is not in this keyring"
    gpg_args=("--gpg-sign=$key")
    [[ -n $homedir ]] && gpg_args+=("--gpg-homedir=$homedir")

    grep -q "<release version=\"$version\"" "$here/$app_id.metainfo.xml" \
        || fail "add <release version=\"$version\" date=\"…\"> to $app_id.metainfo.xml first"

    # The published site is fetched even for a dry run: it is what this
    # build's static delta is cut against.
    [[ $publish == *:* ]] || fail "MAILBOX_FLATPAK_PUBLISH must be [user@]host:path, not '$publish'"
    for tool in rsync ssh; do
        command -v "$tool" >/dev/null || fail "needs $tool to fetch and publish the site"
    done

    if [[ $dry_run == 0 ]]; then
        [[ $revision != *-dirty ]] || fail "the working tree has uncommitted changes"
        command -v gh >/dev/null || fail "needs the GitHub CLI (gh) to create the release"
        # The macOS release runs on the same tag and the two share one GitHub
        # release, so an existing $tag is expected. What must not happen twice
        # is this version's own bundle, which is what says a version was
        # released and never quietly rebuilt.
        if gh release view "$tag" -R "$github" --json assets --jq '.assets[].name' 2>/dev/null \
            | grep -qx "tlmb-$version.flatpak"; then
            fail "$github's release $tag already has tlmb-$version.flatpak — bump the version in Cargo.toml"
        fi
    fi
fi

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

# ---- the repository this build goes into ----

if [[ $release == 1 ]]; then
    stage="$work/site"
    repo="$stage/repo"
    rm -rf "$stage"
    mkdir -p "$stage"
    echo "fetching the published site"
    # Made here so the first release has a directory to fetch from, and the
    # last one somewhere to upload to.
    ssh "$publish_host" "mkdir -p '$publish_path'"
    # OSTree's scratch space is not for publishing, and is not fetched back.
    rsync -a --exclude='/repo/tmp/' "$publish/" "$stage/"
    if [[ -f $repo/config ]]; then
        mkdir -p "$repo/tmp"
    else
        echo "nothing published yet: this is the first release"
        mkdir -p "$repo"
        ostree --repo="$repo" init --mode=archive-z2
    fi
else
    repo="$work/local-repo"
fi

# ---- build and export ----

echo "building $app_id $version ($revision)"
"${builder[@]}" \
    --user \
    --force-clean \
    --install-deps-from=flathub \
    --state-dir="$work/state" \
    --default-branch="$branch" \
    --repo="$repo" \
    --subject="tlmb $version ($revision)" \
    "$work/app" \
    "$here/$app_id.yml"

if [[ ${#gpg_args[@]} -gt 0 ]]; then
    flatpak build-sign "${gpg_args[@]}" "$repo" "$app_id" "$branch"
fi

# Static deltas keep an update from being a download of every changed file;
# pruning keeps the repository — and the site — from growing forever.
flatpak build-update-repo \
    --title="Mailbox" \
    --default-branch="$branch" \
    --generate-static-deltas \
    --prune --prune-depth=5 \
    "${gpg_args[@]}" \
    "$repo"

# ---- trying it here ----

if [[ $release == 0 ]]; then
    if [[ $install == 1 ]]; then
        remote=tlmb-local
        origin="$(flatpak info --user --show-origin "$app_id" 2>/dev/null || true)"
        if [[ -n $origin && $origin != "$remote" ]]; then
            fail "$app_id is installed from '$origin'; flatpak uninstall --user $app_id to try a local build"
        fi
        if ! flatpak remotes --user --columns=name | grep -qx "$remote"; then
            flatpak remote-add --user --no-gpg-verify "$remote" "file://$repo"
        fi
        # From a remote, not from the build directory: the origin is what the
        # in-app updater watches, so a local install can update from the next
        # local build.
        flatpak install --user --noninteractive --or-update "$remote" "$app_id//$branch"
    fi
    [[ $run == 1 ]] && exec flatpak run "$app_id"
    exit 0
fi

# ---- the release ----

gpg "${gpg_home_args[@]}" --export "$key" >"$work/key.gpg"
gpg_key_b64="$(base64 -w0 "$work/key.gpg")"

cat >"$stage/tlmb.flatpakrepo" <<EOF
[Flatpak Repo]
Title=Mailbox
Url=$url
Homepage=https://github.com/$github
Comment=Releases of the desktop app
GPGKey=$gpg_key_b64
EOF

cat >"$stage/tlmb.flatpakref" <<EOF
[Flatpak Ref]
Name=$app_id
Branch=$branch
Title=Mailbox
Url=$url
IsRuntime=false
SuggestRemoteName=tlmb
RuntimeRepo=$runtime_repo
GPGKey=$gpg_key_b64
EOF

# The site root is otherwise a directory of OSTree internals. Overwritten on
# every release, so it always names the version the repository actually holds.
cat >"$stage/index.html" <<EOF
<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Mailbox for Linux</title>
<style>
  :root { color-scheme: light dark; }
  body { margin: 0 auto; padding: 3rem 1.25rem; max-width: 42rem;
         font: 16px/1.6 system-ui, sans-serif; }
  code, pre { font-family: ui-monospace, monospace; }
  pre { padding: .75rem 1rem; overflow-x: auto; border-radius: .4rem;
        background: rgba(127, 127, 127, .15); }
  a { color: inherit; }
</style>
<h1>Mailbox for Linux</h1>
<p>Version $version, as a Flatpak. Installing it also subscribes this machine
to the update repository at <code>$url</code>, so it keeps itself current.</p>
<pre><code>flatpak install --user $site/tlmb.flatpakref</code></pre>
<p>Or download
<a href="https://github.com/$github/releases/latest">the single-file
installer</a> and open it. Either way, updates come from here.</p>
<p><a href="https://github.com/$github">Source</a></p>
</html>
EOF

bundle="$work/tlmb-$version.flatpak"
flatpak build-bundle \
    --repo-url="$url" \
    --runtime-repo="$runtime_repo" \
    --gpg-keys="$work/key.gpg" \
    "$repo" "$bundle" "$app_id" "$branch"

notes="Install: flatpak install --user $site/tlmb.flatpakref

or download tlmb-$version.flatpak below and open it. Either way the app
updates itself from $url."

if [[ $dry_run == 1 ]]; then
    cat <<EOF

dry run — nothing was published. The site is in $stage; to install from it
here, build with MAILBOX_FLATPAK_SITE set to where you serve that directory.
A real run would now:
  rsync -a --exclude='/repo/tmp/' --exclude='/repo/summary*' $stage/ $publish/
  rsync -a $stage/repo/summary* $publish/repo/
  rsync -a --delete --exclude='/repo/tmp/' $stage/ $publish/
  gh release create $tag -R $github --target $(git -C "$root" rev-parse HEAD) \\
      $bundle $stage/tlmb.flatpakref
  (or, if the macOS release already made $tag, upload them into it)
EOF
    exit 0
fi

# Objects first, then the summary that names them, then the deletions pruning
# left behind: a client fetching while this runs sees a whole repository, the
# old one or the new one, never a summary pointing at objects not there yet.
echo "publishing the repository to $publish"
rsync -a --exclude='/repo/tmp/' --exclude='/repo/summary*' "$stage/" "$publish/"
rsync -a "$stage"/repo/summary* "$publish/repo/"
rsync -a --delete --exclude='/repo/tmp/' "$stage/" "$publish/"

if gh release view "$tag" -R "$github" >/dev/null 2>&1; then
    echo "adding the Linux assets to release $tag"
    gh release upload "$tag" -R "$github" --clobber "$bundle" "$stage/tlmb.flatpakref"
else
    echo "creating release $tag"
    gh release create "$tag" -R "$github" \
        --target "$(git -C "$root" rev-parse HEAD)" \
        --title "tlmb $version" \
        --notes "$notes" \
        "$bundle" "$stage/tlmb.flatpakref"
fi

echo "released $tag: $site/tlmb.flatpakref"
