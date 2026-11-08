//! The login keyring over D-Bus, for when `secret-tool` cannot reach it.
//!
//! Inside a Flatpak, libsecret does not talk to the Secret Service at all: it
//! sees `/.flatpak-info`, finds the Secret portal, and switches to a private
//! file keyring under `~/.var/app/<id>/data/keyrings` — whatever
//! `SECRET_BACKEND` says, since the sandbox check comes after it. So a
//! password `mailbox accounts add` put in the login keyring on the host was
//! invisible to the packaged window, every sync of that account failed to log
//! in, and a password typed into the window went where the host could not
//! see it. The manifest already grants `org.freedesktop.secrets`; this is the
//! client libsecret declines to be there.
//!
//! The protocol is the freedesktop Secret Service API with the `plain`
//! algorithm: the secret crosses the session bus in the clear, which is what
//! `secret-tool` does too, and the bus is this user's own.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gio::{DBusCallFlags, DBusConnection, DBusSignalFlags};
use glib::variant::ObjectPath;
use glib::{ToVariant, Variant, VariantTy};

const BUS_NAME: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const COLLECTION: &str = "org.freedesktop.Secret.Collection";
const ITEM: &str = "org.freedesktop.Secret.Item";
const SESSION: &str = "org.freedesktop.Secret.Session";
const PROMPT: &str = "org.freedesktop.Secret.Prompt";
const DEFAULT_COLLECTION: &str = "/org/freedesktop/secrets/aliases/default";
/// What `secret-tool store` files a password under, so either finds the other's.
const GENERIC_SCHEMA: &str = "org.freedesktop.Secret.Generic";

/// How long a call may take. Unlocking can show a dialog, which waits on a
/// person and has its own limit below.
const CALL_TIMEOUT_MS: i32 = 25_000;
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// The secret stored under `service`/`account`, if there is one.
pub fn lookup(service: &str, account: &str) -> Result<Option<String>, String> {
    let keyring = Keyring::open()?;
    let (unlocked, locked) = keyring.search(service, account)?;
    let mut items = unlocked;
    if items.is_empty() && !locked.is_empty() {
        items = keyring.unlock(&locked)?;
    }
    let Some(item) = items.into_iter().next() else {
        return Ok(None);
    };
    keyring.secret(&item)
}

/// Stores `secret` under `service`/`account` in the default collection,
/// replacing what was there.
pub fn store(service: &str, label: &str, account: &str, secret: &str) -> Result<(), String> {
    let keyring = Keyring::open()?;
    keyring.unlock(&[path(DEFAULT_COLLECTION)?])?;

    let properties = glib::VariantDict::new(None);
    properties.insert_value(&format!("{ITEM}.Label"), &label.to_variant());
    properties.insert_value(
        &format!("{ITEM}.Attributes"),
        &attributes(service, account, true).to_variant(),
    );
    let secret = (
        keyring.session.clone(),
        Vec::<u8>::new(),
        secret.as_bytes().to_vec(),
        "text/plain".to_string(),
    );
    let reply = keyring.call(
        DEFAULT_COLLECTION,
        COLLECTION,
        "CreateItem",
        Variant::tuple_from_iter([properties.end(), secret.to_variant(), true.to_variant()]),
        "(oo)",
    )?;
    let prompt = reply.child_value(1).str().unwrap_or("/").to_string();
    if prompt != "/" {
        keyring.prompt(&prompt)?;
    }
    Ok(())
}

/// Deletes everything stored under `service`/`account`.
pub fn clear(service: &str, account: &str) -> Result<(), String> {
    let keyring = Keyring::open()?;
    let (unlocked, locked) = keyring.search(service, account)?;
    let mut items = unlocked;
    if !locked.is_empty() {
        items.extend(keyring.unlock(&locked)?);
    }
    for item in items {
        let reply = keyring.call(
            item.as_str(),
            ITEM,
            "Delete",
            Variant::tuple_from_iter(std::iter::empty::<Variant>()),
            "(o)",
        )?;
        let prompt = reply.child_value(0).str().unwrap_or("/").to_string();
        if prompt != "/" {
            keyring.prompt(&prompt)?;
        }
    }
    Ok(())
}

fn attributes(
    service: &str,
    account: &str,
    with_schema: bool,
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    map.insert("service".to_string(), service.to_string());
    map.insert("account".to_string(), account.to_string());
    if with_schema {
        map.insert("xdg:schema".to_string(), GENERIC_SCHEMA.to_string());
    }
    map
}

fn path(p: &str) -> Result<ObjectPath, String> {
    ObjectPath::try_from(p.to_string()).map_err(|e| e.to_string())
}

/// A connection with an open `plain` session, closed on drop.
struct Keyring {
    connection: DBusConnection,
    session: ObjectPath,
}

impl Keyring {
    fn open() -> Result<Keyring, String> {
        let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
            .map_err(|e| format!("no session bus: {e}"))?;
        let reply = connection
            .call_sync(
                Some(BUS_NAME),
                SERVICE_PATH,
                SERVICE,
                "OpenSession",
                Some(&("plain", "".to_variant()).to_variant()),
                Some(VariantTy::new("(vo)").expect("a valid type string")),
                DBusCallFlags::NONE,
                CALL_TIMEOUT_MS,
                gio::Cancellable::NONE,
            )
            .map_err(|e| format!("the Secret Service did not answer: {e}"))?;
        let session = reply
            .child_value(1)
            .get::<ObjectPath>()
            .ok_or("the Secret Service opened no session")?;
        Ok(Keyring {
            connection,
            session,
        })
    }

    fn call(
        &self,
        object: &str,
        interface: &str,
        method: &str,
        params: Variant,
        reply: &str,
    ) -> Result<Variant, String> {
        self.connection
            .call_sync(
                Some(BUS_NAME),
                object,
                interface,
                method,
                Some(&params),
                Some(VariantTy::new(reply).map_err(|e| e.to_string())?),
                DBusCallFlags::NONE,
                CALL_TIMEOUT_MS,
                gio::Cancellable::NONE,
            )
            .map_err(|e| format!("{method}: {e}"))
    }

    /// Items matching the attributes: those already unlocked, then the rest.
    fn search(
        &self,
        service: &str,
        account: &str,
    ) -> Result<(Vec<ObjectPath>, Vec<ObjectPath>), String> {
        let reply = self.call(
            SERVICE_PATH,
            SERVICE,
            "SearchItems",
            Variant::tuple_from_iter([attributes(service, account, false).to_variant()]),
            "(aoao)",
        )?;
        let paths = |i| {
            reply
                .child_value(i)
                .get::<Vec<ObjectPath>>()
                .unwrap_or_default()
        };
        Ok((paths(0), paths(1)))
    }

    /// Unlocks `objects`, asking the person if the keyring wants that.
    fn unlock(&self, objects: &[ObjectPath]) -> Result<Vec<ObjectPath>, String> {
        let reply = self.call(
            SERVICE_PATH,
            SERVICE,
            "Unlock",
            Variant::tuple_from_iter([objects.to_vec().to_variant()]),
            "(aoo)",
        )?;
        let mut unlocked = reply
            .child_value(0)
            .get::<Vec<ObjectPath>>()
            .unwrap_or_default();
        let prompt = reply.child_value(1).str().unwrap_or("/").to_string();
        if prompt != "/" {
            if let Some(result) = self.prompt(&prompt)? {
                unlocked.extend(
                    result
                        .as_variant()
                        .and_then(|v| v.get::<Vec<ObjectPath>>())
                        .unwrap_or_default(),
                );
            }
        }
        Ok(unlocked)
    }

    fn secret(&self, item: &ObjectPath) -> Result<Option<String>, String> {
        let reply = self.call(
            item.as_str(),
            ITEM,
            "GetSecret",
            Variant::tuple_from_iter([self.session.to_variant()]),
            "((oayays))",
        )?;
        let value = reply.child_value(0).child_value(2);
        let bytes = value.fixed_array::<u8>().map_err(|e| e.to_string())?;
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|_| "the stored secret is not text".to_string())?;
        // `secret-tool lookup` prints the first line; match it, so a secret
        // stored with a trailing newline reads the same both ways.
        let line = text.lines().next().unwrap_or("").to_string();
        Ok((!line.is_empty()).then_some(line))
    }

    /// Shows a prompt and waits for it. `None` is the person dismissing it.
    ///
    /// The `Completed` signal is delivered to the thread-default main context
    /// at the time of subscribing, so this runs one of its own: the caller is
    /// a worker thread, not the window's GTK loop.
    fn prompt(&self, prompt: &str) -> Result<Option<Variant>, String> {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let done: Arc<Mutex<Option<(bool, Variant)>>> = Arc::new(Mutex::new(None));
                let seen = done.clone();
                let id = self.connection.signal_subscribe(
                    Some(BUS_NAME),
                    Some(PROMPT),
                    Some("Completed"),
                    Some(prompt),
                    None,
                    DBusSignalFlags::NONE,
                    move |_, _, _, _, _, params| {
                        let dismissed = params.child_value(0).get::<bool>().unwrap_or(true);
                        *seen.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some((dismissed, params.child_value(1)));
                    },
                );
                let started = self.call(prompt, PROMPT, "Prompt", ("",).to_variant(), "()");
                let deadline = Instant::now() + PROMPT_TIMEOUT;
                let outcome = started.and_then(|_| loop {
                    while context.iteration(false) {}
                    if let Some(answer) = done.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        break Ok(answer);
                    }
                    if Instant::now() > deadline {
                        break Err("the keyring prompt was not answered".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                });
                self.connection.signal_unsubscribe(id);
                outcome.map(|(dismissed, result)| (!dismissed).then_some(result))
            })
            .map_err(|e| e.to_string())?
    }
}

impl Drop for Keyring {
    fn drop(&mut self) {
        let _ = self.connection.call_sync(
            Some(BUS_NAME),
            self.session.as_str(),
            SESSION,
            "Close",
            None,
            None,
            DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            gio::Cancellable::NONE,
        );
    }
}

/// Against the real login keyring, so not run by default: the suite stays off
/// the desktop (§14). `MAILBOX_KEYRING_TEST=<account>` names an existing
/// `mailbox` entry to read; the round trip uses a throwaway service.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "uses the login keyring"]
    fn reads_writes_and_clears_the_login_keyring() {
        if let Ok(account) = std::env::var("MAILBOX_KEYRING_TEST") {
            let found = lookup("mailbox", &account).expect("lookup");
            assert!(found.is_some_and(|s| !s.is_empty()), "{account} not found");
        }
        let (service, account) = ("mailbox-selftest", "roundtrip");
        store(service, "mailbox self-test", account, "s3cret\u{e5}").expect("store");
        assert_eq!(
            lookup(service, account).unwrap().as_deref(),
            Some("s3cret\u{e5}")
        );
        store(service, "mailbox self-test", account, "replaced").expect("replace");
        assert_eq!(
            lookup(service, account).unwrap().as_deref(),
            Some("replaced")
        );
        clear(service, account).expect("clear");
        assert_eq!(lookup(service, account).unwrap(), None);
    }
}
