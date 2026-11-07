//! Saving a message or an attachment from the window.
//!
//! The page cannot do this itself. A webview's own downloads land wherever
//! the engine decides, or nowhere: WKWebView drops a download nobody asked
//! for, and a Flatpak cannot write to `~/Downloads` at all without a
//! permission that would make the portal refuse every later self-update (see
//! the Flatpak manifest). So the page asks the shell, the shell asks the
//! system's own save dialog where the file should go, and then writes it.
//!
//! On Linux the dialog is the desktop portal's `FileChooser`, which is also
//! what makes the write possible in a sandbox: the file the person picks is
//! handed back through the document portal, writable, and nothing else is.
//! Elsewhere it is the platform dialog, through Tauri's dialog plugin.

use std::path::{Path, PathBuf};

use mailbox_core::session::{IdRef, SaveFile, SessionOptions};
use mailbox_core::workspace::Workspace;
use mailbox_core::{Clock, Tracer};
use serde::Serialize;
use tauri::AppHandle;

/// What the window's store is, for the commands that read from it.
pub struct Source {
    pub opts: SessionOptions,
    pub tracer: Tracer,
    pub clock: Clock,
}

/// What the page is told once the file is on disk.
#[derive(Debug, Serialize)]
pub struct Saved {
    pub path: String,
}

/// Which bytes to save.
pub enum What {
    Message { id: String },
    Attachment { id: String, part: String },
}

/// Loads the bytes, asks where they go, and writes them.
///
/// `Ok(None)` is the dialog cancelled, which is not an error.
pub async fn save(app: AppHandle, source: &Source, what: What) -> Result<Option<Saved>, String> {
    let (opts, tracer, clock) = (
        source.opts.clone(),
        source.tracer.clone(),
        source.clock.clone(),
    );
    // Off the async runtime: opening a workspace reads SQLite, and fetching a
    // body the store does not have yet is a network round trip.
    let file = tauri::async_runtime::spawn_blocking(move || load(&opts, tracer, clock, &what))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;

    let Some(target) = choose(&app, &file.filename).await? else {
        return Ok(None);
    };
    write(&target, &file.bytes)
        .map_err(|e| format!("could not write {}: {e}", target.display()))?;
    Ok(Some(Saved {
        path: target.display().to_string(),
    }))
}

fn load(
    opts: &SessionOptions,
    tracer: Tracer,
    clock: Clock,
    what: &What,
) -> mailbox_core::Result<SaveFile> {
    let mut opts = opts.clone();
    // Writable, because the body may have to be fetched and stored first.
    opts.read_only = false;
    let mut workspace = Workspace::open(&opts, tracer, clock)?;
    match what {
        What::Message { id } => workspace.message_file(&IdRef::parse(id)),
        What::Attachment { id, part } => workspace.attachment_file(&IdRef::parse(id), part),
    }
}

/// Writes the file the person chose.
///
/// Overwriting is fine here, unlike `attach --extract`: the dialog already
/// asked. The executable bit is cleared as §9 asks, but only as well as the
/// filesystem allows — the document portal's does not take a `chmod`, and a
/// file it creates is never executable to begin with.
fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode();
            if mode & 0o111 != 0 {
                let _ =
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o111));
            }
        }
    }
    Ok(())
}

/// Asks where to save `filename`. `None` is a cancel.
#[cfg(target_os = "linux")]
async fn choose(_app: &AppHandle, filename: &str) -> Result<Option<PathBuf>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let name = filename.to_string();
    // The portal's reply arrives as a signal on GLib's main context, which is
    // the GTK loop Tauri is turning; the call has to be made from there too.
    glib::MainContext::default().invoke(move || portal::save_file(&name, tx));
    let answer = tauri::async_runtime::spawn_blocking(move || rx.recv())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|_| "the save dialog went away".to_string())?;
    match answer {
        portal::Answer::Chosen(path) => Ok(Some(path)),
        portal::Answer::Cancelled => Ok(None),
        // No portal, or none with a file chooser: a desktop outside a
        // sandbox, set up without one. Save where a browser would rather
        // than not at all, and never over anything already there.
        portal::Answer::Unavailable => Ok(Some(mailbox_core::attach::destination(
            &mailbox_core::paths::downloads_dir(),
            filename,
        ))),
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
async fn choose(app: &AppHandle, filename: &str) -> Result<Option<PathBuf>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog()
        .file()
        .set_file_name(filename)
        .set_directory(mailbox_core::paths::downloads_dir())
        .save_file(move |path| {
            let _ = tx.send(path);
        });
    let path = tauri::async_runtime::spawn_blocking(move || rx.recv())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|_| "the save dialog went away".to_string())?;
    match path {
        Some(path) => path.into_path().map(Some).map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn choose(_app: &AppHandle, filename: &str) -> Result<Option<PathBuf>, String> {
    Ok(Some(mailbox_core::attach::destination(
        &mailbox_core::paths::downloads_dir(),
        filename,
    )))
}

#[cfg(target_os = "linux")]
mod portal {
    //! `org.freedesktop.portal.FileChooser.SaveFile`, and its one reply.

    use std::path::PathBuf;
    use std::sync::mpsc::Sender;
    use std::sync::{Arc, Mutex};

    use gio::prelude::*;
    use gio::{DBusCallFlags, DBusConnection, DBusSignalFlags};
    use glib::{ToVariant, Variant, VariantTy};

    const BUS_NAME: &str = "org.freedesktop.portal.Desktop";
    const PATH: &str = "/org/freedesktop/portal/desktop";
    const INTERFACE: &str = "org.freedesktop.portal.FileChooser";
    const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

    pub enum Answer {
        Chosen(PathBuf),
        Cancelled,
        Unavailable,
    }

    /// Opens the dialog and sends exactly one [`Answer`].
    pub fn save_file(filename: &str, tx: Sender<Answer>) {
        let Ok(connection) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
        else {
            let _ = tx.send(Answer::Unavailable);
            return;
        };
        // Whichever of the reply and a failed call comes first answers; the
        // other finds the sender gone.
        let tx = Arc::new(Mutex::new(Some(tx)));
        let answer = {
            let tx = tx.clone();
            move |a: Answer| {
                if let Some(tx) = tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = tx.send(a);
                }
            }
        };

        // The request's object path is predictable from a token we choose,
        // and subscribing to it before the call is what keeps a fast reply
        // from arriving before anyone is listening (the portal docs' advice).
        let token = format!("mailbox_save_{}", std::process::id() ^ rand_suffix());
        let Some(sender) = connection.unique_name() else {
            answer(Answer::Unavailable);
            return;
        };
        let request_path = format!(
            "{PATH}/request/{}/{token}",
            sender.trim_start_matches(':').replace('.', "_")
        );

        let subscription: Arc<Mutex<Option<gio::SignalSubscriptionId>>> =
            Arc::new(Mutex::new(None));
        let on_response = {
            let answer = answer.clone();
            let connection = connection.clone();
            let subscription = subscription.clone();
            move |_: &DBusConnection, _: &str, _: &str, _: &str, _: &str, params: &Variant| {
                if let Some(id) = subscription
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                {
                    connection.signal_unsubscribe(id);
                }
                answer(read_response(params));
            }
        };
        let id = connection.signal_subscribe(
            Some(BUS_NAME),
            Some(REQUEST_INTERFACE),
            Some("Response"),
            Some(&request_path),
            None,
            DBusSignalFlags::NONE,
            on_response,
        );
        *subscription.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);

        let options = glib::VariantDict::new(None);
        options.insert_value("handle_token", &token.to_variant());
        options.insert_value("current_name", &filename.to_variant());
        options.insert_value("modal", &true.to_variant());
        let params = Variant::tuple_from_iter([
            // No parent window handle: Tauri does not expose one portably,
            // and the dialog works without it — it is only not attached.
            "".to_variant(),
            "Save".to_variant(),
            options.end(),
        ]);
        let caller = connection.clone();
        caller.call(
            Some(BUS_NAME),
            PATH,
            INTERFACE,
            "SaveFile",
            Some(&params),
            Some(VariantTy::new("(o)").expect("a valid type string")),
            DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            move |result| {
                if result.is_err() {
                    if let Some(id) = subscription
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                    {
                        connection.signal_unsubscribe(id);
                    }
                    answer(Answer::Unavailable);
                }
            },
        );
    }

    /// `Response(u response, a{sv} results)`: 0 is chosen, 1 cancelled, 2
    /// anything else that ended the dialog.
    fn read_response(params: &Variant) -> Answer {
        let code = params.child_value(0).get::<u32>().unwrap_or(2);
        if code != 0 {
            return Answer::Cancelled;
        }
        let results = glib::VariantDict::new(Some(&params.child_value(1)));
        let uri = results
            .lookup_value("uris", Some(VariantTy::STRING_ARRAY))
            .and_then(|v| v.get::<Vec<String>>())
            .and_then(|uris| uris.into_iter().next());
        match uri.and_then(|uri| gio::File::for_uri(&uri).path()) {
            Some(path) => Answer::Chosen(path),
            None => Answer::Cancelled,
        }
    }

    /// Enough to keep two dialogs from this process apart; not a secret.
    fn rand_suffix() -> u32 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn response(code: u32, uris: &[&str]) -> Variant {
            let results = glib::VariantDict::new(None);
            let uris: Vec<String> = uris.iter().map(|s| s.to_string()).collect();
            results.insert_value("uris", &uris.to_variant());
            Variant::tuple_from_iter([code.to_variant(), results.end()])
        }

        #[test]
        fn a_chosen_file_comes_back_as_a_path() {
            match read_response(&response(0, &["file:///run/user/1000/doc/ab12/Lunch.eml"])) {
                Answer::Chosen(path) => {
                    assert_eq!(path, PathBuf::from("/run/user/1000/doc/ab12/Lunch.eml"))
                }
                _ => panic!("not chosen"),
            }
        }

        #[test]
        fn a_cancel_or_an_odd_answer_saves_nothing() {
            assert!(matches!(
                read_response(&response(1, &[])),
                Answer::Cancelled
            ));
            assert!(matches!(
                read_response(&response(0, &[])),
                Answer::Cancelled
            ));
            assert!(matches!(
                read_response(&response(0, &["https://example.org/x"])),
                Answer::Cancelled
            ));
        }
    }
}
