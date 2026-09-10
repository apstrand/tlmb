//! UI evidence in a KVM guest (§14): TUI, web, GUI, never a real mail server.
//!
//! Skipped unless `MAILBOX_E2E=1`. A VM is not a PR-sized machine, so this is
//! not a gate — the same reason L4 is nightly. When it does run, it requires a
//! guest already provisioned by `tests/e2e/mailbox-e2e`.

use std::path::PathBuf;
use std::process::Command;

fn e2e() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("tests/e2e/mailbox-e2e")
}

fn enabled() -> bool {
    std::env::var("MAILBOX_E2E").is_ok_and(|v| v == "1")
}

#[test]
fn the_e2e_driver_is_in_the_tree() {
    let path = e2e();
    assert!(path.is_file(), "missing {}", path.display());
}

#[test]
fn the_guest_is_driven_when_asked() {
    if !enabled() {
        eprintln!("e2e SKIPPED: set MAILBOX_E2E=1 after `mailbox-e2e provision`");
        return;
    }
    let status = Command::new(e2e())
        .args(["run", "all"])
        .status()
        .expect("mailbox-e2e");
    assert!(status.success(), "mailbox-e2e run all failed: {status}");
}
