//! What the shell did by itself, for the page's activity log.
//!
//! The page logs what it asks for. Some things happen without it asking —
//! the update check as the window opens, and what the repository answered —
//! and some happen before the page is loaded to hear them. The shell keeps
//! those here, sends each one to the page as it happens, and hands over the
//! backlog when the page asks (`shell_log`).

use std::collections::VecDeque;
use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Manager};

/// Plenty for one session: the shell logs a handful of lines a day.
const LIMIT: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// Milliseconds since the epoch, as the page's `Date` takes it.
    pub at: u64,
    pub level: Level,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Error,
}

#[derive(Default)]
pub struct Activity {
    entries: Mutex<VecDeque<Entry>>,
}

impl Activity {
    /// Records a line and returns it.
    pub fn record(&self, level: Level, text: impl Into<String>) -> Entry {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let entry = Entry {
            at,
            level,
            text: text.into(),
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() == LIMIT {
            entries.pop_front();
        }
        entries.push_back(entry.clone());
        entry
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

/// Records a line and tells the page, if it is there to hear it.
pub fn log(app: &AppHandle, level: Level, text: impl Into<String>) {
    let entry = app.state::<Activity>().record(level, text);
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let Ok(json) = serde_json::to_string(&entry) else {
        return;
    };
    let _ = window.eval(format!(
        "window.dispatchEvent(new CustomEvent('mailbox-log', {{ detail: {json} }}))"
    ));
}

/// The first 10 characters of a commit, which is what a person compares.
pub fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_oldest_lines_fall_off_and_the_rest_stay_in_order() {
        let activity = Activity::default();
        for i in 0..LIMIT + 5 {
            activity.record(Level::Info, format!("line {i}"));
        }
        let entries = activity.entries();
        assert_eq!(entries.len(), LIMIT);
        assert_eq!(entries[0].text, "line 5");
        assert_eq!(entries[LIMIT - 1].text, format!("line {}", LIMIT + 4));
    }

    #[test]
    fn an_entry_reads_as_the_page_expects() {
        let entry = Activity::default().record(Level::Error, "no network");
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["level"], "error");
        assert_eq!(json["text"], "no network");
        assert!(json["at"].as_u64().unwrap() > 0);
    }

    #[test]
    fn a_commit_is_shortened_but_a_short_one_is_left_alone() {
        assert_eq!(short("0123456789abcdef"), "0123456789");
        assert_eq!(short("abc"), "abc");
    }
}
