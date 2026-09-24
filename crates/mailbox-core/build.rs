//! What the build knows about itself.
//!
//! Two jobs. First, telling cargo when the built-in OAuth2 credentials (§9,
//! §16) have changed: `option_env!` reads the environment at compile time, and
//! cargo does not know that on its own — without these lines a rebuild after
//! exporting a client id keeps the old binary, which is the sort of thing that
//! costs an afternoon.
//!
//! Second, recording which commit this binary came from, for `build::INFO`.
//! A bug report says "0.1.6" either way; it is the commit and its date that
//! say whether the reporter is running last Tuesday's build or last year's.
//! Git is asked here because a running binary has no repository to ask.
//!
//! A build from a source tarball has no `.git` — the Flatpak builds that way —
//! so every value can be supplied through the environment instead, and a
//! missing one is empty rather than fatal. A mail client that will not compile
//! outside a git checkout is a mail client nobody can package.

use std::path::Path;
use std::process::Command;

fn main() {
    for provider in ["GOOGLE", "MICROSOFT", "YAHOO"] {
        println!("cargo:rerun-if-env-changed=MAILBOX_{provider}_CLIENT_ID");
        println!("cargo:rerun-if-env-changed=MAILBOX_{provider}_CLIENT_SECRET");
    }
    build_info();
}

/// The four `MAILBOX_BUILD_*` values `src/build.rs` reads with `env!`.
///
/// Always set, possibly to nothing: an empty string is "not known", which
/// keeps the reading side free of `option_env!` and its two spellings.
fn build_info() {
    for var in [
        "MAILBOX_BUILD_COMMIT",
        "MAILBOX_BUILD_COMMIT_DATE",
        "MAILBOX_BUILD_TAG",
        "MAILBOX_BUILD_RELEASE",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    let commit = env_or("MAILBOX_BUILD_COMMIT", || git(&["rev-parse", "HEAD"]));
    let date = env_or("MAILBOX_BUILD_COMMIT_DATE", || {
        // The committer date, not the author date: a rebased or cherry-picked
        // commit was *built* from the tree it landed in, and its author date
        // can be months older than the binary.
        git(&["log", "-1", "--format=%cI", "HEAD"])
    });
    let tag = env_or("MAILBOX_BUILD_TAG", || {
        // `--exact-match` on purpose. `git describe`'s usual output names the
        // nearest tag and how far past it you are, and "v0.1.5-27-gabc1234"
        // in a status bar reads as a version that was never released.
        git(&["describe", "--tags", "--exact-match", "HEAD"])
    });

    // Uncommitted work means the commit above does not describe this binary,
    // and saying so is the whole point of recording it.
    let dirty = match std::env::var("MAILBOX_BUILD_COMMIT") {
        // A supplied commit came from a packager, who has no working tree.
        Ok(v) if !v.is_empty() => false,
        _ => git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty()),
    };

    // An official release is one a packager declares, or one built from a tag
    // that names this very version. Anything else is somebody's build of some
    // commit, however tidy the tree was.
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let released = match std::env::var("MAILBOX_BUILD_RELEASE") {
        Ok(v) if !v.is_empty() => v != "0",
        _ => {
            !dirty
                && tag
                    .as_deref()
                    .is_some_and(|t| t.trim_start_matches('v') == version)
        }
    };

    emit("MAILBOX_BUILD_COMMIT", commit.as_deref().unwrap_or(""));
    emit("MAILBOX_BUILD_COMMIT_DATE", date.as_deref().unwrap_or(""));
    emit("MAILBOX_BUILD_TAG", tag.as_deref().unwrap_or(""));
    emit("MAILBOX_BUILD_DIRTY", if dirty { "1" } else { "" });
    emit("MAILBOX_BUILD_RELEASE", if released { "1" } else { "" });

    watch_git();
}

fn emit(key: &str, value: &str) {
    println!("cargo:rustc-env={key}={value}");
}

/// The environment wins when it has something to say; git answers otherwise.
fn env_or(key: &str, from_git: impl Fn() -> Option<String>) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => from_git(),
    }
}

/// `git` in the source tree, or `None` for every way that can fail: no git on
/// `PATH`, no repository, a repository with no commits, a tag that isn't there.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", env!("CARGO_MANIFEST_DIR")])
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    Some(text.trim().to_string())
}

/// Rebuild when `HEAD` moves.
///
/// Without this the commit is baked in at the first build and stays there
/// through every checkout after it — a stamp that lies is worse than none.
/// The ref file is watched as well as `HEAD`, since committing on a branch
/// leaves `HEAD` itself untouched.
fn watch_git() {
    let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    let git_dir = Path::new(&git_dir);
    let head = git_dir.join("HEAD");
    if !head.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());

    // A packed ref has no file of its own; `packed-refs` is the file that
    // changes when it moves.
    if let Some(reference) = std::fs::read_to_string(&head)
        .ok()
        .and_then(|h| h.strip_prefix("ref: ").map(|r| r.trim().to_string()))
    {
        let path = git_dir.join(&reference);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    let packed = git_dir.join("packed-refs");
    if packed.exists() {
        println!("cargo:rerun-if-changed={}", packed.display());
    }
}
