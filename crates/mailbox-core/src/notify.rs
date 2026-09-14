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
    /// `osascript -e 'display notification …'` — macOS.
    OsaScript(String),
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

        let status = match self {
            Notifier::Unavailable => return Ok(()),
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
                // break out of an AppleScript string literal.
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
            Notifier::Custom(c) => format!("custom: {c}"),
            Notifier::Unavailable => "none available".to_string(),
        }
    }
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
