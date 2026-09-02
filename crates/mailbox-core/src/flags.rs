//! Flags, in the one spelling the rest of the system uses.
//!
//! IMAP says `\Seen`; Gmail says `\Seen` but files junk with a `$Junk` keyword;
//! the CLI says `is:unread`. Normalizing at the edge — once, on the way in —
//! means every query, every golden file, and every JSON document agrees, and
//! nothing downstream has to remember which dialect it is holding.

/// The canonical spelling of the system flags.
pub const SEEN: &str = "seen";
pub const FLAGGED: &str = "flagged";
pub const ANSWERED: &str = "answered";
pub const DRAFT: &str = "draft";
pub const DELETED: &str = "deleted";
pub const SPAM: &str = "spam";
pub const RECENT: &str = "recent";

/// Turns one IMAP flag or keyword into our spelling.
///
/// Returns `None` for flags that are not worth carrying: `\Recent` is
/// per-session and meaningless once stored, and `\*` is a permitted-flags
/// marker rather than a flag.
pub fn normalize(imap_flag: &str) -> Option<String> {
    let f = imap_flag.trim();
    if f.is_empty() || f == "\\*" {
        return None;
    }
    let lower = f.to_ascii_lowercase();
    Some(match lower.as_str() {
        "\\seen" => SEEN.into(),
        "\\flagged" => FLAGGED.into(),
        "\\answered" => ANSWERED.into(),
        "\\draft" => DRAFT.into(),
        "\\deleted" => DELETED.into(),
        "\\recent" => return None,
        // Junk is spelled at least four ways in the wild, and users type
        // `is:spam` for all of them.
        "$junk" | "junk" | "$spam" | "\\junk" => SPAM.into(),
        "$notjunk" | "nonjunk" | "$nonspam" => return None,
        _ => {
            // A server keyword. Strip the `$` sigil and lowercase, so a
            // keyword round-trips predictably; anything with a space in it is
            // not a legal IMAP atom and is dropped rather than corrupting the
            // space-delimited encoding below.
            let k = lower.trim_start_matches(['\\', '$']).to_string();
            if k.is_empty() || k.contains(char::is_whitespace) {
                return None;
            }
            k
        }
    })
}

/// Packs flags for storage: space-delimited *and* space-padded.
///
/// The padding is what makes `instr(flags, ' seen ')` correct — without it,
/// `unseen` contains `seen` and every `is:read` query is wrong.
pub fn pack(flags: &[String]) -> String {
    if flags.is_empty() {
        return " ".to_string();
    }
    let mut sorted: Vec<&str> = flags.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    format!(" {} ", sorted.join(" "))
}

/// Packs straight from the wire, normalizing on the way.
pub fn pack_imap(imap_flags: &[String]) -> String {
    let normalized: Vec<String> = imap_flags.iter().filter_map(|f| normalize(f)).collect();
    pack(&normalized)
}

/// Unpacks storage form back to a sorted list, for the JSON contract.
pub fn unpack(packed: &str) -> Vec<String> {
    packed.split_whitespace().map(str::to_string).collect()
}

pub fn contains(packed: &str, flag: &str) -> bool {
    packed.contains(&format!(" {flag} "))
}
