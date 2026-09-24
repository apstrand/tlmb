//! Which build this is (§4).
//!
//! [`INFO`] is stamped in at compile time by `build.rs` and never changes
//! afterwards, so every frontend can say the same thing about itself: the CLI
//! in `--version`, the terminal in its help, the window in its status bar.
//!
//! The point is answering "which build is that?" from a screenshot. A version
//! number alone cannot: `0.1.6` is every build between two releases, and the
//! interesting ones are always the builds in between.
//!
//! Anything git could not be asked for is `None` rather than a guess. A build
//! from a tarball genuinely does not know its commit, and inventing one would
//! make a bug report worse, not better.

/// What this binary was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    /// The crate version, always present.
    pub version: &'static str,
    /// The full commit hash, if it was built from a checkout.
    pub commit: Option<&'static str>,
    /// The commit's committer date, RFC 3339.
    pub commit_date: Option<&'static str>,
    /// A tag pointing at exactly that commit, if there is one.
    pub tag: Option<&'static str>,
    /// The working tree had uncommitted changes.
    pub dirty: bool,
    /// Built as an official release, rather than from somebody's checkout.
    pub released: bool,
}

/// This build.
pub const INFO: Info = Info {
    version: env!("CARGO_PKG_VERSION"),
    commit: some(env!("MAILBOX_BUILD_COMMIT")),
    commit_date: some(env!("MAILBOX_BUILD_COMMIT_DATE")),
    tag: some(env!("MAILBOX_BUILD_TAG")),
    dirty: !env!("MAILBOX_BUILD_DIRTY").is_empty(),
    released: !env!("MAILBOX_BUILD_RELEASE").is_empty(),
};

/// `build.rs` writes an empty string for what it could not find out; here
/// that becomes `None`, so nothing downstream has to know the convention.
const fn some(value: &'static str) -> Option<&'static str> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

impl Info {
    /// The commit, short enough to read and long enough to find.
    ///
    /// Twelve hex digits: `git` itself moved off seven as repositories grew,
    /// and a status bar that shows an ambiguous prefix has wasted the space.
    pub fn short_commit(&self) -> Option<&'static str> {
        self.commit.map(|c| &c[..c.len().min(12)])
    }

    /// Just the day of the commit — a status bar has no use for the clock.
    pub fn short_date(&self) -> Option<&'static str> {
        self.commit_date.map(|d| match d.split_once('T') {
            Some((day, _)) => day,
            None => d,
        })
    }

    /// One line, for a status bar or a window corner.
    ///
    /// `0.1.6` for a release, and for anything else the commit and its date,
    /// because between releases the version number says nothing:
    /// `0.1.6+a1b2c3d4e5f6 (2026-09-20)`, `-dirty` when the tree was not clean.
    pub fn short(&self) -> String {
        let mut s = String::from(self.version);
        if self.released {
            return s;
        }
        if let Some(commit) = self.short_commit() {
            s.push('+');
            s.push_str(commit);
        }
        if self.dirty {
            s.push_str("-dirty");
        }
        if let Some(date) = self.short_date() {
            s.push_str(&format!(" ({date})"));
        }
        s
    }

    /// Every field, one per line, for `--version` and bug reports.
    ///
    /// The program's name is not in it: clap prints its own in front of
    /// `--version`, and a caller that wants one can say so.
    pub fn long(&self) -> String {
        let mut lines = vec![self.version.to_string()];
        if let Some(commit) = self.commit {
            let dirty = if self.dirty { " (modified)" } else { "" };
            lines.push(format!("commit:  {commit}{dirty}"));
        }
        if let Some(date) = self.commit_date {
            lines.push(format!("date:    {date}"));
        }
        if let Some(tag) = self.tag {
            lines.push(format!("tag:     {tag}"));
        }
        lines.push(format!(
            "build:   {}",
            if self.released {
                "release"
            } else {
                "development"
            }
        ));
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> Info {
        Info {
            version: "0.1.6",
            commit: Some("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678"),
            commit_date: Some("2026-09-20T11:22:33+02:00"),
            tag: None,
            dirty: false,
            released: false,
        }
    }

    /// The number alone is what a screenshot of a development build cannot
    /// be identified by, so the commit has to survive into the short form.
    #[test]
    fn a_development_build_names_its_commit_and_day() {
        assert_eq!(info().short(), "0.1.6+a1b2c3d4e5f6 (2026-09-20)");
    }

    /// A release is the one case where the version *is* the whole answer,
    /// and a status bar full of hex helps nobody.
    #[test]
    fn a_release_is_just_its_version() {
        let released = Info {
            tag: Some("v0.1.6"),
            released: true,
            ..info()
        };
        assert_eq!(released.short(), "0.1.6");
    }

    /// Built from a tree with edits in it, the commit describes something
    /// that was never compiled. Saying so is the point.
    #[test]
    fn an_edited_tree_is_marked() {
        let dirty = Info {
            dirty: true,
            ..info()
        };
        assert_eq!(dirty.short(), "0.1.6+a1b2c3d4e5f6-dirty (2026-09-20)");
    }

    /// A tarball build knows its version and nothing else, and has to render
    /// without inventing the rest.
    #[test]
    fn a_build_without_git_still_renders() {
        let bare = Info {
            commit: None,
            commit_date: None,
            ..info()
        };
        assert_eq!(bare.short(), "0.1.6");
        assert_eq!(bare.long(), "0.1.6\nbuild:   development");
    }

    #[test]
    fn the_long_form_lists_what_it_has() {
        let tagged = Info {
            tag: Some("v0.1.6"),
            released: true,
            ..info()
        };
        assert_eq!(
            tagged.long(),
            "0.1.6\n\
             commit:  a1b2c3d4e5f60718293a4b5c6d7e8f9012345678\n\
             date:    2026-09-20T11:22:33+02:00\n\
             tag:     v0.1.6\n\
             build:   release"
        );
    }

    /// The compiled-in value is the one every frontend shows; if `build.rs`
    /// stops setting it, this is where that is noticed.
    #[test]
    fn this_build_describes_itself() {
        assert_eq!(INFO.version, env!("CARGO_PKG_VERSION"));
        assert!(INFO.short().starts_with(env!("CARGO_PKG_VERSION")));
        assert!(INFO.long().starts_with(env!("CARGO_PKG_VERSION")));
    }
}
