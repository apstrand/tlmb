//! Desktop notifications (§11).
//!
//! > Desktop notifications via the platform's native mechanism, filtered by a
//! > query (default `is:unread in:inbox NOT tag:muted`).
//!
//! Delivered by running the platform's own command rather than by binding to
//! D-Bus or Cocoa — the same choice as the keychain (§9), for the same reasons.
//! It keeps a platform dependency out of the static binary of §4, it is the
//! same thing a user can run by hand to see what we will do, and it makes the
//! whole feature testable by pointing `notify.command` at a script.
//!
//! macOS is the one exception. A notification carries the icon of the process
//! that posts it, and `osascript` posts as a generic script runner whatever it
//! is told to say — there is no argument that changes that. So a `mailbox`
//! running inside `Mailbox.app` posts through Cocoa in its own process, which
//! is the only way the banner comes up with the app's own icon. A `mailbox`
//! run from a terminal has no bundle and no icon of its own, and falls back to
//! `osascript` as before.
//!
//! Nothing here reaches the network. §2 is categorical about that, and a
//! notification is the one feature where it would be easy to forget.

use crate::error::Result;

/// One notification to raise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The headline — a sender, or a count.
    pub summary: String,
    /// The detail line, usually a subject.
    pub body: String,
}

impl Notification {
    /// What a new message looks like.
    ///
    /// Sender first, because that is what decides whether a notification is
    /// worth looking at. A message with no usable From falls back to the
    /// subject rather than showing an empty headline.
    pub fn for_message(message: &mailbox_api::MessageSummary) -> Notification {
        let sender = message
            .from
            .first()
            .map(|a| match &a.name {
                Some(n) if !n.trim().is_empty() => n.clone(),
                _ => a.address.clone(),
            })
            .unwrap_or_else(|| "(unknown sender)".to_string());
        let subject = if message.subject.trim().is_empty() {
            "(no subject)".to_string()
        } else {
            message.subject.clone()
        };
        Notification {
            summary: sender,
            body: subject,
        }
    }

    /// What a backlog looks like: one line, not two hundred popups.
    pub fn for_many(count: usize) -> Notification {
        Notification {
            summary: format!("{count} new messages"),
            body: String::new(),
        }
    }
}

/// How to raise a notification on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notifier {
    /// `notify-send SUMMARY BODY` — freedesktop, so Linux and the BSDs.
    NotifySend(String),
    /// `osascript -e 'display notification …'` — macOS from a terminal, where
    /// there is no app bundle and so no icon to post as.
    OsaScript(String),
    /// macOS, running inside `Mailbox.app`: post through Cocoa in this process
    /// so the banner carries the app's icon. Preferred over [`OsaScript`]
    /// wherever a bundle is found.
    MacApp,
    /// A command from the config. `{summary}` and `{body}` are substituted.
    Custom(String),
    /// Nothing suitable was found. Not an error: a headless machine has no
    /// desktop to notify, and saying so once is better than failing a sync.
    Unavailable,
}

impl Notifier {
    /// Picks a notifier: `MAILBOX_NOTIFIER`, else the configured command, else
    /// whatever the platform has.
    ///
    /// The environment comes first because it is the override for a context
    /// the config cannot know about — a test run, a CI job, a shared machine.
    /// `MAILBOX_NOTIFIER=none` raises nothing at all; anything else is a
    /// command, as `notify.command` is; empty is the same as unset, so a
    /// wrapper script can clear it without having to unset it. Without this a
    /// test suite that syncs new mail puts real popups on the desktop of
    /// whoever ran it, which is how it came to exist.
    pub fn discover(configured: Option<&str>) -> Notifier {
        discover_impl(configured)
    }
}

#[cfg(target_arch = "wasm32")]
fn discover_impl(configured: Option<&str>) -> Notifier {
    let _ = configured;
    Notifier::Unavailable
}

#[cfg(not(target_arch = "wasm32"))]
fn discover_impl(configured: Option<&str>) -> Notifier {
    match std::env::var("MAILBOX_NOTIFIER") {
        Ok(v) if v.trim() == "none" => return Notifier::Unavailable,
        Ok(v) if !v.trim().is_empty() => return Notifier::Custom(v),
        _ => {}
    }
    if let Some(command) = configured.filter(|c| !c.trim().is_empty()) {
        return Notifier::Custom(command.to_string());
    }
    // Inside our own bundle, post as the app so the banner is the app's icon;
    // this is the common case, since the desktop app is where a notification
    // has a desktop to appear on.
    #[cfg(target_os = "macos")]
    if enclosing_bundle_id().is_some() {
        return Notifier::MacApp;
    }
    for (program, build) in [
        (
            "notify-send",
            Notifier::NotifySend as fn(String) -> Notifier,
        ),
        ("osascript", Notifier::OsaScript as fn(String) -> Notifier),
    ] {
        if let Some(path) = which(program) {
            return build(path);
        }
    }
    Notifier::Unavailable
}

impl Notifier {
    pub fn is_available(&self) -> bool {
        !matches!(self, Notifier::Unavailable)
    }

    /// Raises one notification.
    ///
    /// Arguments are passed as arguments, never interpolated into a shell
    /// string — a subject line is attacker-controlled, and `$(rm -rf ~)` is a
    /// perfectly legal thing to put in one.
    pub fn send(&self, notification: &Notification) -> Result<()> {
        use std::process::Command;

        // Posting through Cocoa does not run a command and has no exit status
        // to fold in below; it is handled here and returns on its own.
        #[cfg(target_os = "macos")]
        if matches!(self, Notifier::MacApp) {
            deliver_as_app(&notification.summary, &notification.body);
            return Ok(());
        }

        let status = match self {
            Notifier::Unavailable => return Ok(()),
            // Only ever built on macOS, and handled above where it is.
            Notifier::MacApp => return Ok(()),
            Notifier::NotifySend(path) => Command::new(path)
                .arg("--app-name=mailbox")
                .arg("--category=email.arrived")
                .arg("--")
                .arg(&notification.summary)
                .arg(&notification.body)
                .status(),
            Notifier::OsaScript(path) => {
                // AppleScript has no argv, so the text has to be escaped into
                // the script. Quotes and backslashes are the whole of what can
                // break out of an AppleScript string literal. This is the
                // terminal fallback, with no bundle to post as, so the banner
                // carries osascript's own icon — `MacApp` is what the app uses.
                let script = format!(
                    "display notification {} with title \"mailbox\" subtitle {}",
                    applescript_string(&notification.body),
                    applescript_string(&notification.summary),
                );
                Command::new(path).arg("-e").arg(script).status()
            }
            Notifier::Custom(command) => {
                // A configured command is the user's own, so a shell is
                // appropriate — but the message text still goes in as
                // arguments, not spliced into the string.
                Command::new("sh")
                    .arg("-c")
                    .arg(format!("{command} \"$1\" \"$2\""))
                    .arg("sh")
                    .arg(&notification.summary)
                    .arg(&notification.body)
                    .status()
            }
        };

        // A notifier that fails is not a reason to fail a sync. The mail
        // arrived; the popup did not.
        match status {
            Ok(_) => Ok(()),
            Err(e) => Err(crate::Error::usage(format!(
                "could not raise a notification: {e}"
            ))),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Notifier::NotifySend(p) => format!("notify-send ({p})"),
            Notifier::OsaScript(p) => format!("osascript ({p})"),
            Notifier::MacApp => "macOS app".to_string(),
            Notifier::Custom(c) => format!("custom: {c}"),
            Notifier::Unavailable => "none available".to_string(),
        }
    }
}

/// Posts a notification through `NSUserNotificationCenter`, as this process.
///
/// This is what makes the banner the app's: the center attributes it to the
/// running application, which inside `Mailbox.app` is us, icon and all. It is
/// the deprecated API rather than `UNUserNotificationCenter` because the newer
/// one needs an authorization round trip and a delegate held for the life of
/// the app, where this is one self-contained call — and it is still delivered.
///
/// `NSUserNotificationCenter` reaches into AppKit state and has to run on the
/// main thread, but a sync runs on a background one; the work is handed to the
/// main queue, which the window's run loop is already turning. Nothing waits
/// on it — a banner that is a beat late, or that a headless process never
/// draws, is not worth blocking a sync for.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // NSUserNotification*; see above for why it is the choice.
fn deliver_as_app(summary: &str, body: &str) {
    use objc2_foundation::{NSString, NSUserNotification, NSUserNotificationCenter};

    let summary = summary.to_owned();
    let body = body.to_owned();
    dispatch2::DispatchQueue::main().exec_async(move || {
        let note = NSUserNotification::new();
        note.setTitle(Some(&NSString::from_str("mailbox")));
        note.setSubtitle(Some(&NSString::from_str(&summary)));
        note.setInformativeText(Some(&NSString::from_str(&body)));
        NSUserNotificationCenter::defaultUserNotificationCenter().deliverNotification(&note);
    });
}

/// The bundle identifier of the `.app` this process is running inside.
///
/// `None` anywhere else, which includes every Linux machine and a `mailbox`
/// run from a terminal on macOS — a bare binary has no bundle, and no icon to
/// be drawn with.
#[cfg(target_os = "macos")]
fn enclosing_bundle_id() -> Option<String> {
    // …/Mailbox.app/Contents/MacOS/mailbox → …/Mailbox.app/Contents/Info.plist
    let exe = std::env::current_exe().ok()?;
    let contents = exe.parent()?.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    if contents.parent()?.extension()? != "app" {
        return None;
    }
    let plist = std::fs::read_to_string(contents.join("Info.plist")).ok()?;
    bundle_id_from_plist(&plist)
}

#[cfg(not(target_os = "macos"))]
fn enclosing_bundle_id() -> Option<String> {
    None
}

/// The `CFBundleIdentifier` out of an `Info.plist`.
///
/// Scanned rather than parsed: this is one string out of a document we ship
/// ourselves, and a plist parser is a dependency to carry on every platform
/// for the sake of one lookup on one of them.
///
/// Compiled everywhere the tests run, so the shipped `Info.plist` is checked
/// on the machines that build it as well as on the one platform that uses it.
#[cfg(any(target_os = "macos", test))]
fn bundle_id_from_plist(plist: &str) -> Option<String> {
    let after = plist.split_once("<key>CFBundleIdentifier</key>")?.1;
    let open = after.find("<string>")? + "<string>".len();
    let close = after[open..].find("</string>")? + open;
    let id = after[open..close].trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Escapes a string for an AppleScript literal.
fn applescript_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Finds a program on `PATH`.
///
/// Hand-rolled rather than shelling out to `which`, which would be a process
/// per lookup on a path this runs after every sync.
#[cfg(not(target_arch = "wasm32"))]
fn which(program: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .map(|p| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// macOS draws the icon of whichever application sent the notification, so
    /// the identifier out of our own `Info.plist` is what turns a generic
    /// script icon into the app's.
    #[test]
    fn the_bundle_identifier_is_read_out_of_a_plist() {
        let plist = include_str!("../../../packaging/macos/Info.plist");
        assert_eq!(
            bundle_id_from_plist(plist).as_deref(),
            Some("com.thelastmailbox.mailbox"),
            "the shipped Info.plist no longer yields an identifier"
        );
    }

    /// A plist without one is the ordinary case on every other platform, and
    /// has to come back as "no bundle" rather than as a panic or an empty
    /// `tell application id ""`.
    #[test]
    fn a_plist_without_an_identifier_yields_nothing() {
        for plist in [
            "",
            "<plist><dict></dict></plist>",
            "<key>CFBundleIdentifier</key>",
            "<key>CFBundleIdentifier</key><string></string>",
            "<key>CFBundleIdentifier</key><string>unterminated",
        ] {
            assert_eq!(bundle_id_from_plist(plist), None, "{plist:?}");
        }
    }

    /// The text of a subject line reaches AppleScript as a literal, whatever
    /// is in it: a quote or a backslash is the whole of what can break out.
    #[test]
    fn applescript_literals_are_escaped() {
        assert_eq!(applescript_string(r#"a "b" c"#), r#""a \"b\" c""#);
        assert_eq!(applescript_string(r"back\slash"), r#""back\\slash""#);
    }
}
