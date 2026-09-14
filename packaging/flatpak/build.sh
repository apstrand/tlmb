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
#       - the update repository, signed, on GitHub Pages under MAILBOX_FLATPAK_SITE/repo.
#         This is what every installed copy checks, and what the window's
#         Install button pulls from (mailbox-ui's update.rs). GitHub Releases
#         cannot hold it: a Flatpak repository is a tree of files at fixed
#         paths, and release assets are a flat list.
#       - a GitHub release, v<version>, carrying tlmb.flatpakref and the
#         one-file tlmb-<version>.flatpak installer. Both point at the Pages
#         repository, so whichever someone installs from updates itself.
#
#       The previous release's repository is fetched from the gh-pages branch
#       first and the new build is committed on top of it: that history is
#       what an update is a step along, and what the static deltas are cut
#       from. gh-pages is replaced by a single commit each time, so its git
#       history does not grow with every binary ever shipped.
#
#       --dry-run does all of it except the push and the release, and leaves
#       the site in .build/pages to serve and install from locally.
#
# Environment (defaults are this project's):
#   MAILBOX_FLATPAK_GPG_KEY      signing key fingerprint
#   MAILBOX_FLATPAK_GPG_HOMEDIR  GnuPG home holding it (default: GnuPG's own)
#   MAILBOX_FLATPAK_SITE         where the Pages site is served
#   MAILBOX_FLATPAK_PAGES_GIT    the git remote whose gh-pages branch it is
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
site="${MAILBOX_FLATPAK_SITE:-https://apstrand.github.io/tlmb}"
pages_git="${MAILBOX_FLATPAK_PAGES_GIT:-git@github.com:apstrand/tlmb.git}"
github="${MAILBOX_FLATPAK_GITHUB:-apstrand/tlmb}"
url="$site/repo"
runtime_repo=https://dl.flathub.org/repo/flathub.flatpakrepo

install=0 run=0 release=0 dry_run=0
for arg in "$@"; do
    case "$arg" in
        --install) install=1 ;;
        --run) install=1 run=1 ;;
        --release) release=1 ;;
        --dry-run) dry_run=1 ;;
        -h|--help) sed -n '2,36p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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

    if [[ $dry_run == 0 ]]; then
        [[ $revision != *-dirty ]] || fail "the working tree has uncommitted changes"
        command -v gh >/dev/null || fail "needs the GitHub CLI (gh) to create the release"
        if gh release view "$tag" -R "$github" >/dev/null 2>&1; then
            fail "$github already has a release $tag — bump the version in Cargo.toml"
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
    pages="$work/pages"
    repo="$pages/repo"
    rm -rf "$pages"
    if git ls-remote --exit-code --heads "$pages_git" gh-pages >/dev/null 2>&1; then
        echo "fetching the published repository"
        git clone --quiet --depth 1 --branch gh-pages "$pages_git" "$pages"
    else
        echo "no gh-pages branch yet: this is the first release"
        git init --quiet "$pages"
    fi
    if [[ -f $repo/config ]]; then
        # Git keeps no empty directories, and OSTree wants these to exist.
        mkdir -p "$repo"/{objects,tmp,extensions,state,refs/heads,refs/mirrors,refs/remotes}
    else
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
# pruning keeps the repository — and the Pages site — from growing forever.
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

cat >"$pages/tlmb.flatpakrepo" <<EOF
[Flatpak Repo]
Title=Mailbox
Url=$url
Homepage=https://github.com/$github
Comment=Releases of the desktop app
GPGKey=$gpg_key_b64
EOF

cat >"$pages/tlmb.flatpakref" <<EOF
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

# Pages runs Jekyll unless told not to, and Jekyll drops what it does not
# recognise. OSTree's scratch space is not for publishing.
touch "$pages/.nojekyll"
printf 'repo/tmp/\n' >"$pages/.gitignore"

bundle="$work/tlmb-$version.flatpak"
flatpak build-bundle \
    --repo-url="$url" \
    --runtime-repo="$runtime_repo" \
    --gpg-keys="$work/key.gpg" \
    "$repo" "$bundle" "$app_id" "$branch"

(
    cd "$pages"
    git checkout --quiet --orphan "publish-$version"
    git add -A
    # A CI runner has no git identity of its own.
    git -c user.name="$(git -C "$root" config user.name || echo "tlmb release")" \
        -c user.email="$(git -C "$root" config user.email || echo "tlmb-release@users.noreply.github.com")" \
        commit --quiet -m "tlmb $version ($revision)"
)

notes="Install: flatpak install --user $site/tlmb.flatpakref

or download tlmb-$version.flatpak below and open it. Either way the app
updates itself from $url."

if [[ $dry_run == 1 ]]; then
    cat <<EOF

dry run — nothing was pushed. The site is in $pages; to install from it
here, build with MAILBOX_FLATPAK_SITE set to where you serve that directory.
A real run would now:
  git -C $pages push --force $pages_git HEAD:gh-pages
  gh release create $tag -R $github --target $(git -C "$root" rev-parse HEAD) \\
      $bundle $pages/tlmb.flatpakref
EOF
    exit 0
fi

echo "publishing the repository to gh-pages"
git -C "$pages" push --quiet --force "$pages_git" HEAD:gh-pages

# The first push is the one that needs Pages turned on. Best effort: it only
# works once the repository is public, or on a plan that serves private Pages.
if ! gh api "repos/$github/pages" >/dev/null 2>&1; then
    gh api -X POST "repos/$github/pages" -f 'source[branch]=gh-pages' -f 'source[path]=/' >/dev/null 2>&1 \
        || echo "could not enable GitHub Pages for $github — turn it on for gh-pages in the repository settings" >&2
fi

echo "creating release $tag"
gh release create "$tag" -R "$github" \
    --target "$(git -C "$root" rev-parse HEAD)" \
    --title "tlmb $version" \
    --notes "$notes" \
    "$bundle" "$pages/tlmb.flatpakref"

echo "released $tag: $site/tlmb.flatpakref"
