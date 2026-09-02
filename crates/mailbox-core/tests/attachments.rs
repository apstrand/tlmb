//! L0: attachment extraction and the safety rules of §9.
//!
//! > Attachments are never auto-opened; `attach --extract` writes with the
//! > executable bit cleared and mangles dangerous extensions.
//!
//! A filename in a message is attacker-controlled — it can carry a path, a
//! traversal, a NUL, or an extension a desktop will run. These are the tests
//! for treating it as hostile.

use mailbox_core::attach::{destination, safe_filename, write};
use mailbox_core::mime;

mod tempdir {
    use std::path::{Path, PathBuf};
    pub struct TempDir(PathBuf);
    impl TempDir {
        pub fn new() -> TempDir {
            let mut seed = [0u8; 8];
            getrandom::fill(&mut seed).expect("entropy");
            let path =
                std::env::temp_dir().join(format!("mailbox-attach-{}", u64::from_le_bytes(seed)));
            std::fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// The one that matters most: a filename cannot name a location.
#[test]
fn a_filename_cannot_escape_the_target_directory() {
    for hostile in [
        "../../../etc/passwd",
        "..\\..\\windows\\system32\\evil.dll",
        "/etc/shadow",
        "./../../.ssh/authorized_keys",
        "subdir/nested.txt",
    ] {
        let safe = safe_filename(hostile);
        assert!(
            !safe.contains('/') && !safe.contains('\\'),
            "{hostile:?} became {safe:?}, which still names a path"
        );
        assert!(
            !safe.starts_with('.'),
            "{hostile:?} became {safe:?}, which is still relative"
        );
    }
}

/// A destination is always inside the directory the user named, whatever the
/// message called the file.
#[test]
fn the_destination_stays_inside_the_target_directory() {
    let dir = tempdir::TempDir::new();
    for hostile in ["../../../etc/passwd", "/etc/shadow", "ok.txt"] {
        let path = destination(dir.path(), hostile);
        assert_eq!(
            path.parent(),
            Some(dir.path()),
            "{hostile:?} resolved outside the target directory: {path:?}"
        );
    }
}

/// §9: extensions a double-click would execute get a suffix. The name stays
/// recognisable, so a user can still find and deliberately rename it.
#[test]
fn dangerous_extensions_are_mangled() {
    for (raw, expected) in [
        ("payroll.exe", "payroll.exe.bin"),
        ("setup.MSI", "setup.MSI.bin"),
        ("run.sh", "run.sh.bin"),
        ("script.js", "script.js.bin"),
        ("thing.desktop", "thing.desktop.bin"),
        ("lib.so", "lib.so.bin"),
    ] {
        assert_eq!(safe_filename(raw), expected, "{raw}");
    }

    // And ordinary files are left alone: mangling everything would be as
    // useless as mangling nothing.
    for harmless in [
        "invoice.pdf",
        "photo.jpg",
        "notes.txt",
        "archive.zip",
        "data.csv",
    ] {
        assert_eq!(safe_filename(harmless), harmless);
    }
}

#[test]
fn control_characters_and_empty_names_are_handled() {
    assert!(!safe_filename("bad\0name.txt").contains('\0'));
    assert!(!safe_filename("two\nlines.txt").contains('\n'));
    assert_eq!(safe_filename(""), "attachment");
    assert_eq!(safe_filename("   "), "attachment");
    assert_eq!(safe_filename("..."), "attachment");
    // Long names are truncated to something every filesystem accepts.
    assert!(safe_filename(&"a".repeat(500)).len() <= 210);
}

/// A message must not be able to overwrite a file by choosing its name.
#[test]
fn an_existing_file_is_never_overwritten() {
    let dir = tempdir::TempDir::new();
    std::fs::write(dir.path().join("invoice.pdf"), b"the original").unwrap();

    let path = destination(dir.path(), "invoice.pdf");
    assert_eq!(path.file_name().unwrap(), "invoice-1.pdf");

    std::fs::write(&path, b"second").unwrap();
    assert_eq!(
        destination(dir.path(), "invoice.pdf").file_name().unwrap(),
        "invoice-2.pdf"
    );
    // The original is untouched.
    assert_eq!(
        std::fs::read(dir.path().join("invoice.pdf")).unwrap(),
        b"the original"
    );
}

/// §9: written with the executable bit cleared.
#[cfg(unix)]
#[test]
fn extracted_files_are_not_executable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir::TempDir::new();
    let path = dir.path().join("thing.bin");
    write(&path, b"#!/bin/sh\necho hello").expect("write");

    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0, "the file is executable: {mode:o}");
    assert_eq!(mode & 0o077, 0, "the file is readable by others: {mode:o}");
}

// ---- extraction ----

const WITH_ATTACHMENTS: &str = "\
From: Alice <alice@example.com>\r
Subject: Two files\r
MIME-Version: 1.0\r
Content-Type: multipart/mixed; boundary=\"B\"\r
\r
--B\r
Content-Type: text/plain; charset=utf-8\r
\r
The body.\r
--B\r
Content-Type: application/pdf; name=\"invoice.pdf\"\r
Content-Disposition: attachment; filename=\"invoice.pdf\"\r
Content-Transfer-Encoding: base64\r
\r
JVBERi0xLjQgZmFrZQ==\r
--B\r
Content-Type: text/csv; name=\"data.csv\"\r
Content-Disposition: attachment; filename=\"data.csv\"\r
\r
a,b,c\r
1,2,3\r
--B--\r
";

#[test]
fn a_part_is_extracted_decoded() {
    let raw = WITH_ATTACHMENTS.as_bytes();

    let (name, mime_type, bytes) = mime::extract_part(raw, "2").expect("part 2");
    assert_eq!(name, "invoice.pdf");
    assert_eq!(mime_type, "application/pdf");
    // Decoded, not the base64 it travelled in: what a caller wants is the file.
    assert_eq!(bytes, b"%PDF-1.4 fake");

    let (name, mime_type, bytes) = mime::extract_part(raw, "3").expect("part 3");
    assert_eq!(name, "data.csv");
    assert_eq!(mime_type, "text/csv");
    assert!(String::from_utf8_lossy(&bytes).contains("1,2,3"));
}

/// The part numbers `attach` takes are the ones the contract hands out, so the
/// same string works against a cached message and a live `FETCH BODY[2]`.
#[test]
fn part_numbers_match_what_the_contract_reports() {
    let raw = WITH_ATTACHMENTS.as_bytes();
    let parsed = mime::parse(raw);

    for attachment in parsed.attachments.iter().filter(|a| a.is_attachment) {
        let extracted = mime::extract_part(raw, &attachment.part);
        assert!(
            extracted.is_some(),
            "part {} is listed but cannot be extracted",
            attachment.part
        );
        let (_, mime_type, _) = extracted.unwrap();
        assert_eq!(mime_type, attachment.mime_type);
    }
}

#[test]
fn a_missing_part_is_not_invented() {
    let raw = WITH_ATTACHMENTS.as_bytes();
    assert!(mime::extract_part(raw, "9").is_none());
    assert!(mime::extract_part(raw, "").is_none());
    assert!(mime::extract_part(raw, "2.1.1").is_none());
}

/// A single-part message is part 1 in its entirety (RFC 3501 §6.4.5).
#[test]
fn a_single_part_message_is_part_one() {
    let raw = b"From: a@b.c\r\nSubject: Plain\r\n\r\nJust text.\r\n";
    let (_, mime_type, bytes) = mime::extract_part(raw, "1").expect("part 1");
    assert_eq!(mime_type, "text/plain");
    assert_eq!(String::from_utf8_lossy(&bytes).trim(), "Just text.");
}

/// §14.4: a malformed message may render badly; it may never crash.
#[test]
fn extraction_never_panics_on_broken_input() {
    for broken in [
        &b""[..],
        b"garbage",
        b"Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n--B\r\n",
        b"Content-Type: multipart/mixed; boundary=\"\"\r\n\r\n--\r\n",
        b"\x00\xff\xfe",
    ] {
        for part in ["1", "2", "2.1", "", "..", "999999999999999999999"] {
            let _ = mime::extract_part(broken, part);
        }
        let _ = mime::part_list(broken);
    }
}
