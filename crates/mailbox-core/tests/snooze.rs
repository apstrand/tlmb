//! Snooze (§7), in the local-only form the spec asks for.
//!
//! > **Snooze** is a low-priority feature and starts local-only: a timer plus a
//! > local state record, which means a snoozed message reappears on the device
//! > that snoozed it, whenever that device is next running. This is an honest
//! > limitation, so the UI says which device holds the snooze rather than
//! > implying it is synced.
//!
//! The two properties worth holding onto are here: waking needs nobody to be
//! running at the time, and nothing about a snooze reaches the server.

use mailbox_core::store::snooze::parse_until;
use mailbox_core::Clock;

fn clock_at(rfc3339: &str) -> Clock {
    Clock::fixed(mailbox_core::clock::parse_timestamp(rfc3339).expect("a timestamp"))
}

#[test]
fn a_bare_duration_means_from_now() {
    let clock = clock_at("2026-09-03T10:00:00Z");
    let now = clock.now();

    // `before:`/`after:` demand a sign because `7d` there could mean either
    // direction. A snooze only points forwards, so both forms are accepted.
    assert_eq!(parse_until("2h", &clock).unwrap(), now + 7_200);
    assert_eq!(parse_until("+2h", &clock).unwrap(), now + 7_200);
    assert_eq!(parse_until("3d", &clock).unwrap(), now + 3 * 86_400);
}

#[test]
fn absolute_and_named_times_are_understood() {
    let clock = clock_at("2026-09-03T10:00:00Z");

    let tomorrow = parse_until("tomorrow", &clock).unwrap();
    assert_eq!(
        mailbox_core::clock::format_rfc3339(tomorrow),
        "2026-09-04T00:00:00Z"
    );

    let dated = parse_until("2026-09-10", &clock).unwrap();
    assert_eq!(
        mailbox_core::clock::format_rfc3339(dated),
        "2026-09-10T00:00:00Z"
    );
}

/// "Snooze until yesterday" is a typo every time — most often a `-` where a
/// `+` was meant — and doing nothing silently is the least helpful reading.
#[test]
fn a_time_in_the_past_is_refused() {
    let clock = clock_at("2026-09-03T10:00:00Z");

    for input in ["yesterday", "-2h", "2020-01-01"] {
        let error = parse_until(input, &clock).unwrap_err().to_string();
        assert!(error.contains("past"), "{input}: {error}");
    }
}

#[test]
fn nonsense_says_what_would_have_worked() {
    let clock = clock_at("2026-09-03T10:00:00Z");
    let error = parse_until("wobble", &clock).unwrap_err().to_string();

    assert!(error.contains("2h"), "{error}");
    assert!(error.contains("tomorrow"), "{error}");
}
