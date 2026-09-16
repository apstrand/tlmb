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
        no_keychain();
        let dir = tempdir::TempDir::new();
        let path = dir.path().join("config.toml");
        Fx { _dir: dir, path }
    }

    fn config(&self) -> Config {
        Config::load(&self.path).expect("load the config back")
    }
}

/// Keeps the suite out of the developer's own keyring (§14).
///
/// `accounts::add` files a password where the platform can find it again, and
/// this file adds accounts with passwords. Without this, running the tests
/// leaves `hunter2` in a real keychain under a real-looking account name.
fn no_keychain() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::set_var("MAILBOX_KEYCHAIN", "none"));
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
fn a_conflicting_short_id_falls_back_to_the_email_address() {
    let fx = Fx::new();
    accounts::add(&fx.path, &spec("peter@gmail.com")).expect("first");

    let second = accounts::add(&fx.path, &spec("peter@googlemail.com")).expect("fallback id");
    assert_eq!(second.id, "peter@googlemail.com");
    assert!(fx.config().accounts.contains_key("peter@googlemail.com"));

    let named = accounts::add(
        &fx.path,
        &NewAccount {
            id: "Peter+Personal@Example.COM".into(),
            ..spec("other@gmail.com")
        },
    )
    .expect("a named one is fine");
    assert_eq!(named.id, "peter+personal@example.com");
    assert_eq!(fx.config().accounts.len(), 3);
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

/// A provider that does not exist is always refused; a missing client id
/// depends on what the build ships (§16), so the two are checked apart.
#[test]
fn an_oauth_provider_that_does_not_exist_is_refused() {
    let fx = Fx::new();
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
}

#[test]
fn an_oauth_account_with_a_client_id_is_always_fine() {
    let fx = Fx::new();
    accounts::add(
        &fx.path,
        &NewAccount {
            oauth_provider: Some("google".into()),
            oauth_client_id: Some("abc.apps.googleusercontent.com".into()),
            ..spec("peter@gmail.com")
        },
    )
    .expect("provider and client id both given");
}

/// Without one, the answer depends on the build: a release that ships a client
/// accepts the account, and a build that ships none refuses it and says where
/// to register one. What is pinned here is that the two agree.
#[test]
fn an_oauth_account_without_a_client_id_follows_the_build() {
    let fx = Fx::new();
    let result = accounts::add(
        &fx.path,
        &NewAccount {
            oauth_provider: Some("google".into()),
            ..spec("peter@gmail.com")
        },
    );

    let ships = mailbox_core::oauth::has_builtin("google");
    match (ships, result) {
        (true, Ok(_)) => {
            let account = fx.config().account("peter").expect("added").clone();
            assert!(
                account.oauth_client_id.is_none(),
                "the built-in client belongs to the build, not to the account"
            );
        }
        (false, Err(error)) => {
            assert!(error.to_string().contains("oauth_client_id"), "{error}");
            assert!(
                error.to_string().contains("console.cloud.google.com"),
                "and where to register one: {error}"
            );
        }
        (true, Err(e)) => panic!("this build ships a Google client, but the add failed: {e}"),
        (false, Ok(_)) => panic!("this build ships no Google client, but the add succeeded"),
    }
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

// ---- §16: whose OAuth2 client is used ----

mod oauth_client {
    use mailbox_core::oauth::{self, BuiltinClient};

    const SHIPPED: BuiltinClient = BuiltinClient {
        id: "shipped.apps.googleusercontent.com",
        secret: Some("GOCSPX-shipped"),
    };

    #[test]
    fn an_account_that_registered_its_own_uses_it() {
        let client = oauth::choose_client(Some("mine"), Some("my-secret"), Some(&SHIPPED))
            .expect("a client");
        assert_eq!(client.id, "mine");
        assert_eq!(client.secret.as_deref(), Some("my-secret"));
        assert!(
            !client.builtin,
            "it is this user's registration, and their quota"
        );
    }

    #[test]
    fn an_account_with_none_falls_back_to_what_the_build_ships() {
        let client = oauth::choose_client(None, None, Some(&SHIPPED)).expect("a client");
        assert_eq!(client.id, SHIPPED.id);
        assert_eq!(client.secret.as_deref(), Some("GOCSPX-shipped"));
        assert!(client.builtin);
    }

    /// The mismatch that produces a baffling `invalid_client` from the
    /// provider: one application's id sent with another's secret.
    #[test]
    fn a_different_configured_id_never_borrows_the_builtin_secret() {
        let client = oauth::choose_client(Some("mine"), None, Some(&SHIPPED)).expect("a client");
        assert_eq!(client.id, "mine");
        assert_eq!(
            client.secret, None,
            "a different application's secret is worse than none"
        );
    }

    /// But the *same* id is the same application, and its secret applies.
    ///
    /// This is what a config written before the build shipped credentials
    /// looks like: `oauth_client_id` set by hand to the id the build now
    /// carries. Sending it without the secret gets `invalid_request —
    /// client_secret is missing` from Google, which says nothing about the
    /// config line that caused it.
    #[test]
    fn a_configured_id_that_matches_the_build_uses_the_builds_secret() {
        let client =
            oauth::choose_client(Some(SHIPPED.id), None, Some(&SHIPPED)).expect("a client");
        assert_eq!(client.id, SHIPPED.id);
        assert_eq!(
            client.secret.as_deref(),
            Some("GOCSPX-shipped"),
            "the same application has the same secret"
        );
    }

    /// And an explicitly configured secret still wins over the build's.
    #[test]
    fn a_configured_secret_is_never_overridden() {
        let client =
            oauth::choose_client(Some(SHIPPED.id), Some("mine"), Some(&SHIPPED)).expect("a client");
        assert_eq!(client.secret.as_deref(), Some("mine"));
    }

    #[test]
    fn a_build_that_ships_nothing_has_nothing_to_fall_back_to() {
        assert!(oauth::choose_client(None, None, None).is_none());
        // …but a configured id still works, which is the source-build case.
        let client = oauth::choose_client(Some("mine"), None, None).expect("a client");
        assert_eq!(client.id, "mine");
    }

    #[test]
    fn an_empty_configured_id_is_not_an_id() {
        let client = oauth::choose_client(Some(""), None, Some(&SHIPPED)).expect("a client");
        assert!(
            client.builtin,
            "an empty string in the config should not defeat the fallback"
        );
    }

    /// Microsoft's public clients authenticate without a secret, and the type
    /// has to be able to say so rather than sending an empty one.
    #[test]
    fn a_provider_can_ship_an_id_and_no_secret() {
        const PUBLIC: BuiltinClient = BuiltinClient {
            id: "public-client",
            secret: None,
        };
        let client = oauth::choose_client(None, None, Some(&PUBLIC)).expect("a client");
        assert_eq!(client.id, "public-client");
        assert_eq!(client.secret, None);
    }
}

// ---- trying an account before committing to it (§9) ----

/// What the web interface's setup form does before it writes anything.
///
/// The point of the split is that this can fail without leaving an account
/// behind: someone adding mail from a browser has no `accounts edit` to
/// correct a typo with.
#[test]
fn verify_logs_in_to_a_server_that_answers() {
    no_keychain();
    let server = mailbox_l3::ImapServer::start(vec![mailbox_l3::Message::simple(
        1,
        "First",
        "one@example.com",
    )])
    .expect("start the L3 IMAP server");

    let cfg = plaintext_loopback(server.port());
    let found = accounts::verify(
        "test",
        &cfg,
        &mailbox_core::imap::Credential::Password("hunter2".into()),
        mailbox_core::Tracer::disabled(),
        std::time::Duration::from_secs(5),
    )
    .expect("the server answered and took the login");

    // What the Test button reports. "It worked" is the least useful true
    // thing to say: the folder count is how someone knows the login reached a
    // mailbox and not merely a server, and the sync path answers "why is my
    // mail slow" before it is asked (§5.3).
    assert_eq!(found.server, format!("127.0.0.1:{}", server.port()));
    assert_eq!(found.login_user, "peter@example.com");
    assert!(found.folders > 0, "no folders came back: {found:?}");
    assert!(
        !found.capabilities.is_empty(),
        "no capabilities came back: {found:?}"
    );
    assert!(
        ["qresync", "condstore", "full-listing"].contains(&found.sync_path.as_str()),
        "`{}` is not one of §5.3's paths",
        found.sync_path
    );
}

/// A port with nothing behind it is the common shape of a mistyped host, and
/// it has to come back as an error rather than as a written account.
#[test]
fn verify_fails_when_nothing_is_listening() {
    no_keychain();
    // Bound and dropped, so the port is one nothing can have taken since.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    };

    let error = accounts::verify(
        "test",
        &plaintext_loopback(port),
        &mailbox_core::imap::Credential::Password("hunter2".into()),
        mailbox_core::Tracer::disabled(),
        std::time::Duration::from_secs(5),
    )
    .expect_err("nothing is listening there");
    assert!(
        matches!(
            error,
            mailbox_core::error::Error::Network(_) | mailbox_core::error::Error::Timeout(_)
        ),
        "a closed port is a network error, not {error:?}"
    );
}

/// Loopback and plaintext, which §9 allows for exactly this host.
fn plaintext_loopback(port: u16) -> mailbox_core::config::AccountConfig {
    mailbox_core::config::AccountConfig {
        address: "peter@example.com".into(),
        imap_host: Some("127.0.0.1".into()),
        imap_port: Some(port),
        imap_security: Some("none".into()),
        require_tls: false,
        ..Default::default()
    }
}

/// With no keychain to file it in, `add` says so rather than implying the
/// password is safe for next time.
#[test]
fn a_password_with_nowhere_durable_to_go_is_reported_as_such() {
    let fx = Fx::new();
    let added = accounts::add(
        &fx.path,
        &NewAccount {
            password: Some("hunter2".into()),
            ..spec("peter@gmail.com")
        },
    )
    .expect("add");

    let said = added.credential.as_deref().unwrap_or_default();
    assert!(
        said.contains("this session only"),
        "the caller cannot tell the user the password will not survive a restart: {said:?}"
    );
    assert!(
        said.contains(mailbox_core::credentials::keychain_name()),
        "the message does not name the place it failed to reach: {said:?}"
    );
}
