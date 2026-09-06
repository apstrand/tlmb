//! L0: adding, editing, and removing accounts (§9).
//!
//! These rules used to live in the iOS shell, which meant the CLI had none.
//! They are here now, and so are the refusals: an account that cannot connect,
//! or that quietly turns off TLS, is worse than one that was never added.

use mailbox_core::accounts::{self, NewAccount};
use mailbox_core::config::Config;

mod tempdir {
    use std::path::{Path, PathBuf};
    pub struct TempDir(PathBuf);
    impl TempDir {
        pub fn new() -> TempDir {
            let mut seed = [0u8; 8];
            getrandom::fill(&mut seed).expect("entropy");
            let path =
                std::env::temp_dir().join(format!("mailbox-accounts-{}", u64::from_le_bytes(seed)));
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

struct Fx {
    _dir: tempdir::TempDir,
    path: std::path::PathBuf,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempdir::TempDir::new();
        let path = dir.path().join("config.toml");
        Fx { _dir: dir, path }
    }

    fn config(&self) -> Config {
        Config::load(&self.path).expect("load the config back")
    }
}

fn spec(address: &str) -> NewAccount {
    NewAccount {
        address: address.to_string(),
        ..Default::default()
    }
}

// ---- adding ----

#[test]
fn a_known_provider_needs_only_an_address() {
    let fx = Fx::new();
    let added = accounts::add(&fx.path, &spec("Peter@Gmail.com")).expect("add");

    assert_eq!(added.id, "peter", "the local part, lowercased");
    assert!(added.discovered);

    let cfg = fx.config();
    let account = cfg.account("peter").expect("the account");
    assert_eq!(account.imap_host.as_deref(), Some("imap.gmail.com"));
    assert_eq!(account.smtp_host.as_deref(), Some("smtp.gmail.com"));
    assert_eq!(account.imap_port, Some(993));
    assert!(account.require_tls, "TLS is on unless someone turns it off");
}

#[test]
fn an_unknown_domain_asks_for_a_host_rather_than_guessing() {
    let fx = Fx::new();
    let error = accounts::add(&fx.path, &spec("peter@example.org"))
        .expect_err("nothing is known about example.org");
    assert!(
        error.to_string().contains("--imap-host"),
        "and it should say what is missing: {error}"
    );
    assert!(
        !fx.path.exists(),
        "a refused add must not leave a config behind"
    );
}

#[test]
fn what_the_caller_gives_beats_the_table() {
    let fx = Fx::new();
    let added = accounts::add(
        &fx.path,
        &NewAccount {
            imap_host: Some("imap.internal.example".into()),
            imap_port: Some(1993),
            ..spec("peter@gmail.com")
        },
    )
    .expect("add");
    assert!(!added.discovered, "the caller supplied the host");

    let cfg = fx.config();
    let account = cfg.account("peter").unwrap();
    assert_eq!(account.imap_host.as_deref(), Some("imap.internal.example"));
    assert_eq!(account.imap_port, Some(1993));
    // Still filled in from the table, because the caller said nothing about it.
    assert_eq!(account.smtp_host.as_deref(), Some("smtp.gmail.com"));
}

#[test]
fn a_second_account_for_the_same_address_needs_its_own_id() {
    let fx = Fx::new();
    accounts::add(&fx.path, &spec("peter@gmail.com")).expect("first");

    let error = accounts::add(&fx.path, &spec("peter@gmail.com")).expect_err("collides");
    assert!(error.to_string().contains("already exists"), "{error}");

    accounts::add(
        &fx.path,
        &NewAccount {
            id: "personal".into(),
            ..spec("peter@gmail.com")
        },
    )
    .expect("a named one is fine");
    assert_eq!(fx.config().accounts.len(), 2);
}

#[test]
fn adding_never_writes_a_password_to_the_config() {
    let fx = Fx::new();
    accounts::add(
        &fx.path,
        &NewAccount {
            password: Some("hunter2".into()),
            ..spec("peter@gmail.com")
        },
    )
    .expect("add");

    let text = std::fs::read_to_string(&fx.path).expect("read the config");
    assert!(
        !text.contains("hunter2"),
        "§9: the secret never lands in the config file:\n{text}"
    );
}

#[test]
fn an_id_that_could_not_be_used_is_refused() {
    let fx = Fx::new();
    let error = accounts::add(
        &fx.path,
        &NewAccount {
            id: "not/an id".into(),
            ..spec("peter@gmail.com")
        },
    )
    .expect_err("an id ends up in a path and an environment variable");
    assert!(error.to_string().contains("usable account id"), "{error}");
}

#[test]
fn an_address_without_an_at_sign_is_not_an_address() {
    let fx = Fx::new();
    assert!(accounts::add(&fx.path, &spec("peter")).is_err());
    assert!(accounts::add(&fx.path, &spec("")).is_err());
}

// ---- §9's TLS rule ----

#[test]
fn tls_can_only_be_turned_off_for_loopback() {
    let fx = Fx::new();
    let error = accounts::add(
        &fx.path,
        &NewAccount {
            imap_host: Some("imap.example.org".into()),
            require_tls: Some(false),
            ..spec("peter@example.org")
        },
    )
    .expect_err("§9 refuses plaintext to a real host");
    assert!(error.to_string().contains("loopback"), "{error}");

    accounts::add(
        &fx.path,
        &NewAccount {
            imap_host: Some("127.0.0.1".into()),
            imap_security: Some("none".into()),
            require_tls: Some(false),
            ..spec("peter@example.org")
        },
    )
    .expect("a loopback server is the one case §9 allows");
}

#[test]
fn a_security_mode_that_does_not_exist_is_refused() {
    let fx = Fx::new();
    let error = accounts::add(
        &fx.path,
        &NewAccount {
            imap_host: Some("imap.example.org".into()),
            imap_security: Some("maybe".into()),
            ..spec("peter@example.org")
        },
    )
    .expect_err("`maybe` is not a security mode");
    assert!(error.to_string().contains("starttls"), "{error}");
}

// ---- OAuth2 ----

#[test]
fn an_oauth_account_needs_a_client_id_and_a_provider_that_exists() {
    let fx = Fx::new();
    let error = accounts::add(
        &fx.path,
        &NewAccount {
            oauth_provider: Some("google".into()),
            ..spec("peter@gmail.com")
        },
    )
    .expect_err("no client id");
    assert!(error.to_string().contains("oauth_client_id"), "{error}");

    let error = accounts::add(
        &fx.path,
        &NewAccount {
            oauth_provider: Some("nonesuch".into()),
            oauth_client_id: Some("abc".into()),
            ..spec("peter@gmail.com")
        },
    )
    .expect_err("no such provider");
    assert!(
        error.to_string().contains("google, microsoft, yahoo"),
        "{error}"
    );

    accounts::add(
        &fx.path,
        &NewAccount {
            oauth_provider: Some("google".into()),
            oauth_client_id: Some("abc.apps.googleusercontent.com".into()),
            ..spec("peter@gmail.com")
        },
    )
    .expect("both given");
}

// ---- editing and removing ----

#[test]
fn editing_changes_only_what_was_given() {
    let fx = Fx::new();
    accounts::add(&fx.path, &spec("peter@gmail.com")).expect("add");

    // Something the edit form does not know about, set by hand.
    let mut config = fx.config();
    config
        .accounts
        .get_mut("peter")
        .unwrap()
        .identities
        .push("p.strand@example.com".into());
    config.save(&fx.path).unwrap();

    accounts::edit(
        &fx.path,
        "peter",
        &NewAccount {
            imap_user: Some("peter.strand".into()),
            ..Default::default()
        },
    )
    .expect("edit");

    let account = fx.config().account("peter").expect("still there").clone();
    assert_eq!(account.imap_user.as_deref(), Some("peter.strand"));
    assert_eq!(account.address, "peter@gmail.com", "untouched");
    assert_eq!(
        account.identities,
        vec!["p.strand@example.com".to_string()],
        "a field the caller never mentioned must survive the edit"
    );
}

#[test]
fn editing_an_account_that_is_not_there_says_so() {
    let fx = Fx::new();
    accounts::add(&fx.path, &spec("peter@gmail.com")).expect("add");
    let error = accounts::edit(&fx.path, "nope", &NewAccount::default()).expect_err("no such");
    assert!(error.to_string().contains("nope"), "{error}");
}

#[test]
fn an_edit_that_would_break_the_account_is_refused_before_it_is_written() {
    let fx = Fx::new();
    accounts::add(&fx.path, &spec("peter@gmail.com")).expect("add");

    let error = accounts::edit(
        &fx.path,
        "peter",
        &NewAccount {
            require_tls: Some(false),
            ..Default::default()
        },
    )
    .expect_err("§9 again");
    assert!(error.to_string().contains("loopback"), "{error}");

    assert!(
        fx.config().account("peter").unwrap().require_tls,
        "the rejected edit must not have been saved"
    );
}

#[test]
fn removing_takes_the_account_and_leaves_the_mail() {
    let fx = Fx::new();
    accounts::add(&fx.path, &spec("peter@gmail.com")).expect("add");

    let store = accounts::remove(&fx.path, "peter").expect("remove");
    assert!(fx.config().accounts.is_empty());
    assert!(
        store.ends_with("peter"),
        "the store is named so it can be deleted deliberately: {}",
        store.display()
    );

    let error = accounts::remove(&fx.path, "peter").expect_err("already gone");
    assert!(error.to_string().contains("peter"), "{error}");
}
