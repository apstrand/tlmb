//! Updating the desktop app from inside it.
//!
//! The Linux package is a Flatpak, and a Flatpak cannot replace its own files:
//! `/app` is read-only, and it should be. What it can do is ask the Flatpak
//! portal (`org.freedesktop.portal.Flatpak`) to watch the remote it was
//! installed from, to install what it finds there, and to start the new
//! version once it is on disk. That is the whole of "self update" here — the
//! same transaction `flatpak update` runs, started from the window, which is
//! why an update that asks for more sandbox permissions than the installed
//! version had is refused by the portal rather than granted by a button.
//!
//! [`Tracker`] is what the portal's reports mean, without a bus, so it can be
//! tested the way the rest of this crate is. [`portal`] is the D-Bus client
//! that feeds it. Outside a Flatpak the status is [`Status::Unsupported`] and
//! the page shows nothing: updating a `cargo install` or a distribution
//! package is that package manager's business, not this window's.

use serde::Serialize;

/// What the window says about updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// Not running as a Flatpak, or the portal is not there to ask.
    Unsupported,
    /// Watching, and nothing newer has been seen.
    Current,
    /// The remote has a newer build than this machine has installed.
    Available,
    /// The portal is downloading and deploying it.
    Installing { percent: u32 },
    /// A newer build is installed and this process is still the old one.
    Ready,
    /// The last attempt failed. Installing again is allowed.
    Failed { message: String },
}

/// One `Progress` signal from the portal's update monitor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub op: u32,
    pub n_ops: u32,
    /// Of the current operation, 0–100. The portal holds it at 99 until done.
    pub progress: u32,
    pub status: u32,
    pub error_message: Option<String>,
}

// The portal's `status` values (flatpak-portal.c, `PROGRESS_STATUS_*`).
const PROGRESS_RUNNING: u32 = 0;
const PROGRESS_EMPTY: u32 = 1;
const PROGRESS_DONE: u32 = 2;
const PROGRESS_ERROR: u32 = 3;

/// The commits the portal last reported that a later signal needs.
///
/// `running` is the build this process was started from and `remote` the
/// build the remote offers; whether they differ is what an empty install
/// transaction means.
#[derive(Debug, Clone, Default)]
struct Commits {
    running: String,
    remote: String,
}

/// What the portal has said so far, and what that adds up to.
#[derive(Debug, Clone)]
pub struct Tracker {
    status: Status,
    commits: Option<Commits>,
}

impl Tracker {
    pub fn unsupported() -> Tracker {
        Tracker {
            status: Status::Unsupported,
            commits: None,
        }
    }

    /// Watching, with nothing reported yet.
    pub fn watching() -> Tracker {
        Tracker {
            status: Status::Current,
            commits: None,
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// An `UpdateAvailable` signal.
    ///
    /// The portal emits it whenever the installed or the remote commit moved
    /// since it last said — which includes a remote that went back to what is
    /// installed, or an install that caught up with it. So the signal's name
    /// is not the answer; the commits are.
    pub fn available(&mut self, running: &str, local: &str, remote: &str) {
        self.commits = Some(Commits {
            running: running.to_string(),
            remote: remote.to_string(),
        });
        // A check that lands mid-install says nothing the install will not.
        if matches!(self.status, Status::Installing { .. }) {
            return;
        }
        self.status = if remote != local {
            Status::Available
        } else if local != running {
            // Installed by someone else — `flatpak update`, a software centre —
            // while this window was open. Nothing to download; restart.
            Status::Ready
        } else {
            Status::Current
        };
    }

    /// The portal accepted an `Update` call.
    pub fn installing(&mut self) {
        self.status = Status::Installing { percent: 0 };
    }

    /// The call itself failed, before any progress was reported.
    pub fn failed(&mut self, message: impl Into<String>) {
        self.status = Status::Failed {
            message: message.into(),
        };
    }

    /// A `Progress` signal.
    pub fn progress(&mut self, p: &Progress) {
        self.status = match p.status {
            PROGRESS_RUNNING => {
                let percent = (p.op.min(p.n_ops) * 100 + p.progress.min(100))
                    .checked_div(p.n_ops)
                    .unwrap_or(0);
                Status::Installing {
                    percent: percent.min(99),
                }
            }
            PROGRESS_DONE => Status::Ready,
            // Nothing to install: the newest build is already on disk. Whether
            // that means "restart" depends on whether this process is it.
            PROGRESS_EMPTY => match &self.commits {
                Some(c) if c.running == c.remote => Status::Current,
                _ => Status::Ready,
            },
            PROGRESS_ERROR => Status::Failed {
                message: p
                    .error_message
                    .clone()
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "the update failed".into()),
            },
            _ => return,
        };
    }
}

/// Whether this process is inside a Flatpak sandbox.
///
/// `/.flatpak-info` is the documented marker: every sandbox has it and nothing
/// else does.
pub fn in_flatpak() -> bool {
    cfg!(target_os = "linux") && std::path::Path::new("/.flatpak-info").exists()
}

#[cfg(target_os = "linux")]
pub mod portal {
    //! The D-Bus side: `CreateUpdateMonitor`, its two signals, `Update`, and
    //! `Spawn` for the restart.
    //!
    //! Everything here runs on GLib's default main context, which on Linux is
    //! the GTK loop Tauri is already turning, so no thread is started for it.

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use gio::{DBusCallFlags, DBusConnection, DBusSignalFlags};
    use glib::{ToVariant, Variant, VariantTy};

    use super::{Progress, Tracker};

    const BUS_NAME: &str = "org.freedesktop.portal.Flatpak";
    const PATH: &str = "/org/freedesktop/portal/Flatpak";
    const INTERFACE: &str = "org.freedesktop.portal.Flatpak";
    const MONITOR_INTERFACE: &str = "org.freedesktop.portal.Flatpak.UpdateMonitor";

    /// `FLATPAK_SPAWN_FLAGS_LATEST_VERSION`: run what is installed now, not
    /// the commit the caller was started from.
    const SPAWN_LATEST_VERSION: u32 = 1 << 1;

    /// A live update monitor.
    #[derive(Clone)]
    pub struct Portal {
        connection: DBusConnection,
        monitor: Arc<Mutex<Option<String>>>,
        tracker: Arc<Mutex<Tracker>>,
    }

    impl Portal {
        /// Asks the portal for an update monitor and starts listening to it.
        ///
        /// `changed` is called on the main loop whenever the status moves.
        /// Returns `None` when there is no session bus; a portal that turns
        /// out to be missing is reported through the tracker instead, since
        /// that answer only arrives asynchronously.
        pub fn start(
            tracker: Arc<Mutex<Tracker>>,
            changed: impl Fn() + Clone + 'static,
        ) -> Option<Portal> {
            let connection =
                gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
            let portal = Portal {
                connection,
                monitor: Arc::new(Mutex::new(None)),
                tracker,
            };

            // Subscribed before the monitor exists and filtered on the path
            // in the handler: the path is only known once the call returns,
            // and the portal does not check for thirty minutes anyway.
            let this = portal.clone();
            let notify = changed.clone();
            portal.connection.signal_subscribe(
                None,
                Some(MONITOR_INTERFACE),
                Some("UpdateAvailable"),
                None,
                None,
                DBusSignalFlags::NONE,
                move |_, _, path, _, _, params| {
                    if !this.is_ours(path) {
                        return;
                    }
                    let info = vardict(params);
                    let get = |key: &str| info.get(key).cloned().unwrap_or_default();
                    this.lock().available(
                        &get("running-commit"),
                        &get("local-commit"),
                        &get("remote-commit"),
                    );
                    notify();
                },
            );

            let this = portal.clone();
            let notify = changed.clone();
            portal.connection.signal_subscribe(
                None,
                Some(MONITOR_INTERFACE),
                Some("Progress"),
                None,
                None,
                DBusSignalFlags::NONE,
                move |_, _, path, _, _, params| {
                    if !this.is_ours(path) {
                        return;
                    }
                    this.lock().progress(&progress(params));
                    notify();
                },
            );

            let this = portal.clone();
            portal.connection.call(
                Some(BUS_NAME),
                PATH,
                INTERFACE,
                "CreateUpdateMonitor",
                Some(&Variant::tuple_from_iter([empty_vardict()])),
                Some(VariantTy::new("(o)").expect("a valid type string")),
                DBusCallFlags::NONE,
                -1,
                gio::Cancellable::NONE,
                move |result| match result {
                    Ok(reply) => {
                        let path = reply.child_value(0).str().map(str::to_string);
                        *this.monitor.lock().unwrap_or_else(|e| e.into_inner()) = path;
                    }
                    Err(_) => {
                        // No portal, or one too old for update monitors. The
                        // page hides the control rather than offer a button
                        // that cannot work.
                        *this.lock() = Tracker::unsupported();
                        changed();
                    }
                },
            );

            Some(portal)
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Tracker> {
            self.tracker.lock().unwrap_or_else(|e| e.into_inner())
        }

        fn is_ours(&self, path: &str) -> bool {
            self.monitor
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref()
                == Some(path)
        }

        /// Asks the portal to install the update. Progress arrives as signals.
        pub fn install(&self, changed: impl Fn() + 'static) {
            let Some(monitor) = self
                .monitor
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            else {
                self.lock().failed("the update monitor is not ready yet");
                changed();
                return;
            };
            self.lock().installing();
            changed();

            let this = self.clone();
            self.connection.call(
                Some(BUS_NAME),
                &monitor,
                MONITOR_INTERFACE,
                "Update",
                // No parent window handle: the portal only uses it to parent
                // a polkit dialog, which a per-user installation never shows.
                Some(&Variant::tuple_from_iter([
                    "".to_variant(),
                    empty_vardict(),
                ])),
                None,
                DBusCallFlags::NONE,
                -1,
                gio::Cancellable::NONE,
                move |result| {
                    if let Err(e) = result {
                        this.lock().failed(dbus_message(&e));
                        changed();
                    }
                },
            );
        }

        /// Starts the newest installed build of this app with `argv`.
        ///
        /// Blocking, and short: the portal replies once `flatpak run` has been
        /// forked, which is the moment it is safe for the caller to exit.
        pub fn spawn_latest(&self, argv: &[&str]) -> Result<(), String> {
            let cwd = std::env::current_dir()
                .map(|d| d.into_os_string().into_encoded_bytes())
                .unwrap_or_else(|_| b"/".to_vec());
            let params = spawn_params(&cwd, argv);
            self.connection
                .call_sync(
                    Some(BUS_NAME),
                    PATH,
                    INTERFACE,
                    "Spawn",
                    Some(&params),
                    Some(VariantTy::new("(u)").expect("a valid type string")),
                    DBusCallFlags::NONE,
                    10_000,
                    gio::Cancellable::NONE,
                )
                .map(|_| ())
                .map_err(|e| dbus_message(&e))
        }
    }

    /// `Spawn(ay cwd, aay argv, a{uh} fds, a{ss} envs, u flags, a{sv} options)`.
    fn spawn_params(cwd: &[u8], argv: &[&str]) -> Variant {
        // Not `array_from_iter_with_type(VariantTy::BYTE_STRING, …)`: glib-sys
        // 0.18 spells an array of byte strings "ayy", and the call quietly
        // builds an `ay` that the portal rejects. The test below holds this.
        let argv: Vec<Vec<u8>> = argv
            .iter()
            .map(|a| a.bytes().chain([0]).collect())
            .collect();
        let argv = argv.to_variant();
        let fds = Variant::array_from_iter_with_type(
            VariantTy::new("{uh}").expect("a valid type string"),
            std::iter::empty::<Variant>(),
        );
        let envs: HashMap<String, String> = HashMap::new();
        Variant::tuple_from_iter([
            byte_string(cwd),
            argv,
            fds,
            envs.to_variant(),
            SPAWN_LATEST_VERSION.to_variant(),
            empty_vardict(),
        ])
    }

    /// A NUL-terminated `ay`, which is how D-Bus carries paths and argv.
    fn byte_string(bytes: &[u8]) -> Variant {
        let mut owned = bytes.to_vec();
        owned.push(0);
        Variant::array_from_fixed_array(&owned)
    }

    fn empty_vardict() -> Variant {
        glib::VariantDict::new(None).end()
    }

    /// The string values of an `(a{sv})` signal body.
    fn vardict(params: &Variant) -> HashMap<String, String> {
        let dict = glib::VariantDict::new(Some(&params.child_value(0)));
        let mut out = HashMap::new();
        for key in ["running-commit", "local-commit", "remote-commit"] {
            if let Some(value) = dict
                .lookup_value(key, Some(VariantTy::STRING))
                .and_then(|v| v.get::<String>())
            {
                out.insert(key.to_string(), value);
            }
        }
        out
    }

    fn progress(params: &Variant) -> Progress {
        let dict = glib::VariantDict::new(Some(&params.child_value(0)));
        let uint = |key: &str| {
            dict.lookup_value(key, Some(VariantTy::UINT32))
                .and_then(|v| v.get::<u32>())
                .unwrap_or(0)
        };
        Progress {
            op: uint("op"),
            n_ops: uint("n_ops"),
            progress: uint("progress"),
            status: uint("status"),
            error_message: dict
                .lookup_value("error_message", Some(VariantTy::STRING))
                .and_then(|v| v.get::<String>()),
        }
    }

    /// A D-Bus error without GDBus's `GDBus.Error:org.foo.Bar:` prefix, which
    /// is for logs rather than for a status line.
    fn dbus_message(error: &glib::Error) -> String {
        strip_remote_error(error.message()).to_string()
    }

    fn strip_remote_error(message: &str) -> &str {
        message
            .strip_prefix("GDBus.Error:")
            .and_then(|rest| rest.split_once(": "))
            .map_or(message, |(_, text)| text)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_progress_signal_body_reads_back() {
            let dict = glib::VariantDict::new(None);
            dict.insert_value("op", &1u32.to_variant());
            dict.insert_value("n_ops", &2u32.to_variant());
            dict.insert_value("progress", &40u32.to_variant());
            dict.insert_value("status", &0u32.to_variant());
            let body = Variant::tuple_from_iter([dict.end()]);
            let p = progress(&body);
            assert_eq!((p.op, p.n_ops, p.progress, p.status), (1, 2, 40, 0));
            assert_eq!(p.error_message, None);
        }

        #[test]
        fn an_update_available_body_reads_back() {
            let dict = glib::VariantDict::new(None);
            dict.insert_value("running-commit", &"aaa".to_variant());
            dict.insert_value("local-commit", &"aaa".to_variant());
            dict.insert_value("remote-commit", &"bbb".to_variant());
            let info = vardict(&Variant::tuple_from_iter([dict.end()]));
            assert_eq!(info["remote-commit"], "bbb");
            assert_eq!(info["running-commit"], "aaa");
        }

        #[test]
        fn a_remote_error_is_said_without_its_dbus_name() {
            assert_eq!(
                strip_remote_error(
                    "GDBus.Error:org.freedesktop.DBus.Error.NotSupported: requires new permissions"
                ),
                "requires new permissions"
            );
            assert_eq!(strip_remote_error("timed out"), "timed out");
        }

        #[test]
        fn spawn_is_called_with_the_portals_signature() {
            let params = spawn_params(b"/", &[""]);
            assert_eq!(params.type_().as_str(), "(ayaaya{uh}a{ss}ua{sv})");
        }

        #[test]
        fn argv_goes_over_the_bus_nul_terminated() {
            let v = byte_string(b"mailbox");
            assert_eq!(v.type_().as_str(), "ay");
            assert_eq!(v.fixed_array::<u8>().unwrap(), b"mailbox\0");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_that_everything_matches_is_not_an_update() {
        let mut t = Tracker::watching();
        t.available("aaa", "aaa", "aaa");
        assert_eq!(t.status(), &Status::Current);
    }

    #[test]
    fn a_newer_remote_is_available() {
        let mut t = Tracker::watching();
        t.available("aaa", "aaa", "bbb");
        assert_eq!(t.status(), &Status::Available);
    }

    #[test]
    fn an_update_installed_elsewhere_only_wants_a_restart() {
        let mut t = Tracker::watching();
        t.available("aaa", "bbb", "bbb");
        assert_eq!(t.status(), &Status::Ready);
    }

    #[test]
    fn a_newer_remote_beats_a_pending_restart() {
        let mut t = Tracker::watching();
        t.available("aaa", "bbb", "ccc");
        assert_eq!(t.status(), &Status::Available);
    }

    #[test]
    fn progress_counts_across_operations_and_stops_short_of_done() {
        let mut t = Tracker::watching();
        t.installing();
        let running = |op, n_ops, progress| Progress {
            op,
            n_ops,
            progress,
            status: PROGRESS_RUNNING,
            error_message: None,
        };
        t.progress(&running(0, 2, 50));
        assert_eq!(t.status(), &Status::Installing { percent: 25 });
        t.progress(&running(1, 2, 99));
        assert_eq!(t.status(), &Status::Installing { percent: 99 });
        t.progress(&Progress {
            status: PROGRESS_DONE,
            ..Default::default()
        });
        assert_eq!(t.status(), &Status::Ready);
    }

    #[test]
    fn a_check_mid_install_does_not_interrupt_it() {
        let mut t = Tracker::watching();
        t.available("aaa", "aaa", "bbb");
        t.installing();
        t.available("aaa", "aaa", "bbb");
        assert_eq!(t.status(), &Status::Installing { percent: 0 });
    }

    #[test]
    fn an_empty_transaction_means_the_new_build_is_already_here() {
        let mut t = Tracker::watching();
        t.available("aaa", "aaa", "bbb");
        t.installing();
        t.progress(&Progress {
            status: PROGRESS_EMPTY,
            ..Default::default()
        });
        assert_eq!(t.status(), &Status::Ready);

        let mut t = Tracker::watching();
        t.available("bbb", "bbb", "bbb");
        t.installing();
        t.progress(&Progress {
            status: PROGRESS_EMPTY,
            ..Default::default()
        });
        assert_eq!(t.status(), &Status::Current, "this process is the newest");
    }

    #[test]
    fn a_failure_says_why_and_can_be_retried() {
        let mut t = Tracker::watching();
        t.installing();
        t.progress(&Progress {
            status: PROGRESS_ERROR,
            error_message: Some(
                "Self update not supported, new version requires new permissions".into(),
            ),
            ..Default::default()
        });
        assert_eq!(
            t.status(),
            &Status::Failed {
                message: "Self update not supported, new version requires new permissions".into()
            }
        );
        t.installing();
        assert_eq!(t.status(), &Status::Installing { percent: 0 });
    }

    #[test]
    fn the_page_gets_a_tagged_shape() {
        let json = serde_json::to_string(&Status::Installing { percent: 40 }).unwrap();
        assert_eq!(json, r#"{"state":"installing","percent":40}"#);
        let json = serde_json::to_string(&Status::Unsupported).unwrap();
        assert_eq!(json, r#"{"state":"unsupported"}"#);
    }
}
