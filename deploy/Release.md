# Releasing the Linux desktop app

The desktop app ships as the Flatpak `com.thelastmailbox.flat`. A release
has two parts, and the release process always publishes both:

| Part | Where | What it is for |
|---|---|---|
| Update repository | `gh-pages` branch, served at <https://apstrand.github.io/tlmb/repo> | The signed Flatpak (OSTree) repository. Every installed copy checks it, and the window's *Install* button pulls from it. |
| GitHub release `v<version>` | <https://github.com/apstrand/tlmb/releases> | `tlmb-<version>.flatpak`, a one-file installer, and `tlmb.flatpakref`. Both point at the update repository. |

GitHub Releases cannot hold the update repository itself. A Flatpak
repository is a tree of files at fixed paths, and release assets are a flat
list. That is why the repository lives on Pages.

Everything below is done by `packaging/flatpak/build.sh --release`. The
GitHub workflow runs that same command on a runner.

---

## One-time setup

### 1. Make the repository public

GitHub Pages and anonymous release downloads both need `apstrand/tlmb` to be
public. Until it is, `flatpak` cannot fetch anything from it.

### 2. Enable GitHub Pages

Settings → Pages → *Deploy from a branch* → `gh-pages`, folder `/ (root)`.

The branch does not exist until the first release pushes it. The release
tries to enable Pages through the API after that first push. If it can't, it
says so, and you set it as above once the branch is there.

### 3. The signing key

Releases are signed with
`772EE1812AA4D782E519E62E4496C4F85CE95DB1` (Peter Strand <peter@nena.se>).
The public key is embedded in `tlmb.flatpakref`, in the installer, and in
every installed copy's remote configuration.

> **Do not change the signing key casually.** Existing installs verify
> updates against the key they were installed with. A release signed with a
> different key fails verification for all of them, and they stop updating
> until each user reinstalls.

### 4. Repository secrets (GitHub releases only)

Settings → Secrets and variables → Actions → *New repository secret*:

| Secret | Value |
|---|---|
| `FLATPAK_GPG_PRIVATE_KEY` | output of `gpg --armor --export-secret-keys 772EE1812AA4D782E519E62E4496C4F85CE95DB1` |
| `FLATPAK_GPG_PASSPHRASE` | that key's passphrase |
| `MAILBOX_GOOGLE_CLIENT_ID`, `MAILBOX_GOOGLE_CLIENT_SECRET`, `MAILBOX_MICROSOFT_CLIENT_ID`, `MAILBOX_MICROSOFT_CLIENT_SECRET`, `MAILBOX_YAHOO_CLIENT_ID`, `MAILBOX_YAHOO_CLIENT_SECRET` | *optional* — OAuth2 client credentials compiled into the build; missing ones are left out |

The workflow's token needs no configuration. The workflow asks for
`contents: write` (push `gh-pages`, create the release) and `pages: write`
itself.

### 5. Tools (manual releases only)

On the machine that releases:

- `flatpak`, `ostree`, `git`, `gpg`, a Rust toolchain (`cargo vendor` runs on the host)
- a flatpak-builder: `flatpak install flathub org.flatpak.Builder`, or the distribution's `flatpak-builder` package
- the GitHub CLI, logged in with push access: `gh auth login`
- the signing key's secret part in your keyring
- push access to `git@github.com:apstrand/tlmb.git` over SSH

The GNOME runtime, SDK and Rust extension are installed by the build on first
use. That is a few GB, once.

---

## Preparing a release

These steps are the same whichever way you publish.

1. **Pick the version** and set it in the workspace `Cargo.toml`:

   ```toml
   [workspace.package]
   version = "0.2.0"
   ```

2. **Add a release entry** at the top of `<releases>` in
   `packaging/flatpak/com.thelastmailbox.flat.metainfo.xml`. Software centres
   show it, and the release refuses to run without one for the version:

   ```xml
   <releases>
     <release version="0.2.0" date="2026-10-01">
       <description>
         <p>What changed, in a sentence or two.</p>
       </description>
     </release>
     <release version="0.1.0" date="2026-09-13"/>
   </releases>
   ```

3. **Check the sandbox permissions have not grown.** Compare `finish-args` in
   `packaging/flatpak/com.thelastmailbox.flat.yml` with the last release:

   ```sh
   git diff v0.1.0 -- packaging/flatpak/com.thelastmailbox.flat.yml
   ```

   The Flatpak portal refuses a self-update that asks for more than the
   installed version has. Users see *Update failed: … requires new
   permissions* and have to run `flatpak update` themselves. If a new
   permission is really needed, say so in the release notes.

4. **Commit** the version bump (`Cargo.lock` changes with it) and push it to
   `master`.

5. **Optionally, rehearse** with a dry run. It builds, signs, fetches the
   published repository and stages the new site, then stops before pushing
   anything:

   ```sh
   packaging/flatpak/build.sh --release --dry-run
   ```

   The result is in `packaging/flatpak/.build/`: the site in `pages/`, the
   installer as `tlmb-<version>.flatpak`. The dry run does not check for a
   clean tree or an existing release.

---

## Releasing through GitHub

Push a tag named after the version:

```sh
git tag v0.2.0
git push origin v0.2.0
```

That runs `.github/workflows/flatpak-release.yml` (Actions → *flatpak-release*).
It:

1. fails straight away if the tag is not `v` + the `Cargo.toml` version;
2. installs Flatpak tooling on `ubuntu-24.04` (`packaging/flatpak/ci-setup.sh`);
3. imports the signing key from secrets into a throwaway GnuPG home and
   presets its passphrase (`packaging/flatpak/ci-import-key.sh`), because
   signing cannot ask for it;
4. runs `build.sh --release`, which builds, signs, pushes `gh-pages` with the
   job's token, and creates the GitHub release.

A release takes roughly as long as a cold release build plus the SDK
download. The job times out after 90 minutes. Tag pushes queue behind each
other rather than running in parallel, since two releases at once would
overwrite each other's `gh-pages`.

To retry a failed run, fix the cause and re-run the job from the Actions page.
If a release was already created before the failure, see
[A release that went wrong](#a-release-that-went-wrong).

---

## Releasing manually

From a clean checkout of the commit to release, already pushed to GitHub:

```sh
packaging/flatpak/build.sh --release
```

Before it builds anything, it checks that:

- the signing key is in your keyring;
- the metainfo has a `<release>` for the version;
- the working tree has no uncommitted changes;
- `gh` is installed, and `apstrand/tlmb` has no release `v<version>` yet.

Then it:

1. vendors the crates and builds the app;
2. clones `gh-pages` (or starts it, on the first release) and exports the
   build into its repository, on top of the previous release's history;
3. signs the commit and the repository summary, generates a static delta from
   the previous build, and prunes history older than five releases;
4. writes `tlmb.flatpakref` and `tlmb.flatpakrepo` at the site root, and the
   `tlmb-<version>.flatpak` bundle;
5. replaces `gh-pages` with a single commit and force-pushes it;
6. enables Pages if it is not yet;
7. creates the GitHub release `v<version>` at the current `HEAD` with the
   bundle and `tlmb.flatpakref` attached. The tag is created by this step if
   you have not pushed one.

GnuPG asks for the key's passphrase while signing.

The defaults can be overridden for a fork or a test:

| Variable | Default |
|---|---|
| `MAILBOX_FLATPAK_GPG_KEY` | `772EE1812AA4D782E519E62E4496C4F85CE95DB1` |
| `MAILBOX_FLATPAK_GPG_HOMEDIR` | GnuPG's own |
| `MAILBOX_FLATPAK_SITE` | `https://apstrand.github.io/tlmb` |
| `MAILBOX_FLATPAK_PAGES_GIT` | `git@github.com:apstrand/tlmb.git` |
| `MAILBOX_FLATPAK_GITHUB` | `apstrand/tlmb` |

---

## After a release

Pages can take a few minutes to serve the new files, and caches them for
about ten more.

- **Fresh install:**

  ```sh
  flatpak install --user https://apstrand.github.io/tlmb/tlmb.flatpakref
  flatpak run com.thelastmailbox.flat
  ```

- **Update from the terminal**, on a machine with the previous version:

  ```sh
  flatpak update --user com.thelastmailbox.flat
  flatpak info --user com.thelastmailbox.flat   # Version / Commit / Subject
  ```

- **Update from the window:** the portal checks every 30 minutes, not at
  launch. The status bar shows *A new version is available → Install*, then
  *Restart*. The first self-update of an install asks *Update Mailbox?* once.

The commit subject is `tlmb <version> (<git revision>)`, which is how to tell
builds apart in `flatpak info` and `flatpak remote-info --log`.

---

## A release that went wrong

Everything already published stays in the update repository. There is no
"unpublish" that reaches installed copies. Choose by how far it got:

- **Failed before the `gh-pages` push:** nothing is published. Fix the cause
  and run it again. Delete the tag first if the workflow created nothing and
  you need to move it (`git push --delete origin v0.2.0`).
- **`gh-pages` pushed, release not created:** the update is already live for
  installed copies. Re-running the job or `build.sh --release` works, but it
  publishes a second build of the same version on top, and installed copies
  are offered that as one more update. To avoid it, create the release by
  hand. After a manual run, use its `packaging/flatpak/.build` output. After
  a failed workflow, use a dry run of the same commit: the installer it
  builds works the same, and `tlmb.flatpakref` is identical.

  ```sh
  gh release create v0.2.0 -R apstrand/tlmb \
      packaging/flatpak/.build/tlmb-0.2.0.flatpak \
      packaging/flatpak/.build/pages/tlmb.flatpakref
  ```

- **A broken build shipped:** release a fixed version on top. The next
  version replaces it everywhere through the normal update path. A user who
  needs the previous build meanwhile can go back to it, since the repository
  keeps the last five:

  ```sh
  flatpak remote-info --user --log tlmb com.thelastmailbox.flat   # find the commit
  flatpak update --user --commit=<commit> com.thelastmailbox.flat
  ```

  Installed from the one-file bundle, the remote is called `flat-origin`
  instead of `tlmb`.

Do not delete or rewrite the update repository to "undo" a release. Installed
copies expect the history to continue from what they have.

---

## Trying a build without releasing

Nothing here touches `gh-pages` or GitHub:

```sh
packaging/flatpak/build.sh --run
```

This builds into a local unsigned repository (`packaging/flatpak/.build/local-repo`),
installs from it as remote `tlmb-local`, and opens the app. Running it again
makes the installed copy offer the new build as an update, exactly as a
release would. It refuses to overwrite an install that came from the real
release remote; uninstall that first.
