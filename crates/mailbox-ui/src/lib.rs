//! **The graphical interface** (§7).
//!
//! `mailbox gui` opens a native window; `mailbox serve` hands a browser the
//! same page. Both are the DOM client in [`mailbox_web`], talking to the §6.4
//! contract. The window is Tauri — a webview on Linux (WebKitGTK), macOS
//! (WKWebView), and Windows (WebView2) — because a mail client is a document:
//! selection, find-in-page, and the sanitized HTML of §9 as HTML, which is
//! what the canvas was wrong for (§16).
//!
//! [`app`] is the same rules without a window, so they stay testable the way
//! §15 asks: microseconds, a fake backend, no screenshot suite. The production
//! window does not drive that state machine; it loads the page.

pub mod activity;
pub mod app;
pub mod backend;
/// The window's command names and their ACL permissions.
pub mod commands;
pub mod save;
pub mod update;

pub use app::{App, Focus};
pub use backend::{Backend, Request, Response, Seq};

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};

use mailbox_core::error::{Error, Result};
use mailbox_core::session::SessionOptions;
use mailbox_core::{Clock, Tracer};
use mailbox_web::server::{make_token, Options, Server};
use tauri::ipc::CapabilityBuilder;
use tauri::webview::{NewWindowResponse, WebviewWindowBuilder};
use tauri::{AppHandle, Manager, WebviewUrl};

/// Opens the window and runs until it closes.
pub fn run(opts: SessionOptions, tracer: Tracer, clock: Clock) -> Result<()> {
    let source = save::Source {
        opts: opts.clone(),
        tracer: tracer.clone(),
        clock: clock.clone(),
    };
    let server = Server::bind(
        Options {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            token: Some(make_token(&mailbox_core::Rng::from_env())),
            max_requests: None,
        },
        opts,
        tracer,
        clock,
    )?;

    let url = server.url();
    let port = server
        .address()
        .map_err(|e| Error::usage(format!("could not read the GUI server address: {e}")))?
        .port();

    // Bound before the thread starts, so the first navigation is not a race
    // against `listen`. The process exit when the window closes is what
    // stops it — there is no graceful drain, and there does not need to be:
    // the store is already consistent after every request.
    std::thread::spawn(move || {
        let _ = server.run();
    });

    open_window(&url, port, source)
}

fn open_window(url: &str, port: u16, source: save::Source) -> Result<()> {
    prepare_webview_env();

    let parsed: tauri::Url = url
        .parse()
        .map_err(|e| Error::usage(format!("the GUI URL {url:?} did not parse: {e}")))?;

    let webview_dir = mailbox_core::paths::state_dir().join("webview");
    let _ = std::fs::create_dir_all(&webview_dir);

    let builder = tauri::Builder::default();

    // The updater reads a signed manifest published beside the release; it is
    // what stands in for the Flatpak portal where there is no portal. Not
    // registered on Linux, where a Flatpak cannot replace its own files.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    // The save dialog, for `save::choose`. Only its Rust side is used: the
    // page is granted none of its commands, and asks `save_*` instead.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let builder = builder.plugin(tauri_plugin_dialog::init());

    builder
        .invoke_handler(tauri::generate_handler![
            update_status,
            update_check,
            update_install,
            update_restart,
            save_message,
            save_attachment,
            shell_log
        ])
        .setup(move |app| {
            // The page is served from loopback, which Tauri counts as remote
            // content and refuses every command to. This grants the update
            // commands to exactly this process's server — not to any port, and
            // not to anything a message links to.
            let mut shell = CapabilityBuilder::new("shell-updates")
                .remote(format!("http://127.0.0.1:{port}"))
                .window("main");
            for command in commands::COMMANDS {
                shell = shell.permission(commands::allow(command));
            }
            app.add_capability(shell)?;
            // Before the updater, which logs from its first check.
            app.manage(activity::Activity::default());
            app.manage(Updates::start(app.handle()));
            app.manage(source);

            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(parsed))
                .title("mailbox")
                .inner_size(1100.0, 720.0)
                .min_inner_size(640.0, 400.0)
                .resizable(true)
                .enable_clipboard_access()
                .accept_first_mouse(true)
                .background_color(tauri::webview::Color(0x1c, 0x1c, 0x1e, 0xff))
                .initialization_script(SHELL_SCRIPT)
                .data_directory(webview_dir)
                .on_navigation(move |dest| {
                    if is_app_url(dest, port) {
                        return true;
                    }
                    // A top-level jump off the mailbox is a link the reader
                    // followed. Open it with the platform handler and stay put
                    // — this window is the mail, not a browser.
                    let _ = mailbox_core::open::open_url(dest.as_str());
                    false
                })
                .on_new_window(move |dest, _features| {
                    let _ = mailbox_core::open::open_url(dest.as_str());
                    NewWindowResponse::Deny
                })
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .map_err(|e| {
            Error::usage(format!(
                "could not open a window: {e}\n\
                 if there is no display, `mailbox serve` is the same interface in a browser"
            ))
        })
}

/// WebKitGTK on Wayland (NVIDIA, some compositors) fails at first paint with
/// `Error 71 (Protocol error) dispatching to Wayland display` unless the
/// dmabuf renderer is off. The user can override by setting the variable
/// themselves — including to `0` to turn the workaround off.
fn prepare_webview_env() {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
        // A compositor pairs a window with its desktop entry, and so its icon
        // and name in a dock, by the Wayland app_id — which GTK takes from the
        // program name. In a Flatpak the entry is named after the app id.
        // Tauri's own `enableGTKAppId` would do this too, but it also makes
        // the process single-instance, and restarting into an update starts
        // the new process while the old one is still closing.
        if let Ok(id) = std::env::var("FLATPAK_ID") {
            glib::set_prgname(Some(id.as_str()));
        }
    }
}

/// The update state the page asks for, and the portal behind it.
struct Updates {
    tracker: Arc<Mutex<update::Tracker>>,
    #[cfg(target_os = "linux")]
    portal: Option<update::portal::Portal>,
}

impl Updates {
    fn start(app: &AppHandle) -> Updates {
        // Where there is no portal there may still be a signed manifest.
        // Checked once as the window opens, and again whenever the about box
        // asks — never on a timer, and never before someone has opened the
        // program (§2).
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let tracker = Arc::new(Mutex::new(update::Tracker::watching()));
            let updates = Updates {
                tracker: tracker.clone(),
                #[cfg(target_os = "linux")]
                portal: None,
            };
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                let pushed = handle.clone();
                activity::log(&handle, activity::Level::Info, "checking for updates");
                update::store::check(handle.clone(), tracker, move || push_update_status(&pushed))
                    .await;
                log_update_status(&handle);
            });
            return updates;
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        if !update::in_flatpak() {
            activity::log(
                app,
                activity::Level::Info,
                "not running as a Flatpak: updates come from whatever installed this build",
            );
            return Updates {
                tracker: Arc::new(Mutex::new(update::Tracker::unsupported())),
                #[cfg(target_os = "linux")]
                portal: None,
            };
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let tracker = Arc::new(Mutex::new(update::Tracker::watching()));
            #[cfg(target_os = "linux")]
            {
                let handle = app.clone();
                let portal = update::portal::Portal::start(tracker.clone(), move || {
                    push_update_status(&handle)
                });
                if portal.is_none() {
                    activity::log(
                        app,
                        activity::Level::Error,
                        "the Flatpak portal did not answer: this window cannot update itself",
                    );
                    *tracker.lock().unwrap_or_else(|e| e.into_inner()) =
                        update::Tracker::unsupported();
                }
                // Once as the window opens, as the manifest is on macOS: the
                // portal may not say anything for half an hour.
                let handle = app.clone();
                tauri::async_runtime::spawn(check_repo(handle, tracker.clone(), false));
                Updates { tracker, portal }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = app;
                Updates { tracker }
            }
        }
    }

    fn status(&self) -> update::Status {
        self.tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status()
            .clone()
    }
}

/// Tells the page the update status moved. The page also asks on load, so a
/// change that lands before it is listening is not lost.
fn push_update_status(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let status = app.state::<Updates>().status();
    let Ok(json) = serde_json::to_string(&status) else {
        return;
    };
    let _ = window.eval(format!(
        "window.dispatchEvent(new CustomEvent('mailbox-update', {{ detail: {json} }}))"
    ));
}

#[tauri::command]
fn update_status(updates: tauri::State<'_, Updates>) -> update::Status {
    updates.status()
}

#[tauri::command]
fn update_install(app: AppHandle, updates: tauri::State<'_, Updates>) {
    #[cfg(target_os = "linux")]
    if let Some(portal) = &updates.portal {
        portal.install(move || push_update_status(&app));
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let tracker = updates.tracker.clone();
        let handle = app.clone();
        tauri::async_runtime::spawn(async move {
            let pushed = handle.clone();
            update::store::install(handle.clone(), tracker, move || push_update_status(&pushed))
                .await;
        });
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let _ = (app, updates);
}

/// Asks again, for the about box's "Check for updates", and returns once
/// the answer is in, so the page can say what it found.
///
/// Where updates come from a signed manifest, this fetches it. In a Flatpak
/// it reads the repository's summary: the portal watches too, but cannot be
/// asked (see `update::repo`).
#[tauri::command]
async fn update_check(app: AppHandle) {
    let tracker = app.state::<Updates>().tracker.clone();
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let pushed = app.clone();
        activity::log(&app, activity::Level::Info, "checking for updates");
        update::store::check(app.clone(), tracker, move || push_update_status(&pushed)).await;
        log_update_status(&app);
    }
    #[cfg(target_os = "linux")]
    if update::in_flatpak() {
        check_repo(app, tracker, true).await;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let _ = (app, tracker);
}

/// Compares the Flatpak repository's build with the running one.
///
/// A build with no repository to ask (a local one) and a sandbox without
/// `/.flatpak-info` leave the status as the portal has it.
#[cfg(target_os = "linux")]
async fn check_repo(app: AppHandle, tracker: Arc<Mutex<update::Tracker>>, asked: bool) {
    use activity::{log, short, Level};
    let Some(url) = update::repo::url() else {
        log(
            &app,
            Level::Info,
            "update check skipped: this build has no repository to ask (a local build)",
        );
        return;
    };
    let Some(running) = update::repo::running() else {
        log(
            &app,
            Level::Error,
            "update check skipped: /.flatpak-info does not say which build this is",
        );
        return;
    };
    log(
        &app,
        Level::Info,
        format!("checking for updates: {url} for {}", running.ref_),
    );
    let ref_ = running.ref_.clone();
    let found = tauri::async_runtime::spawn_blocking(move || update::repo::fetch(url, &ref_))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
    match &found {
        Ok(remote) if *remote == running.commit => log(
            &app,
            Level::Info,
            format!(
                "update check: the repository has {}, the build running — up to date",
                short(remote)
            ),
        ),
        Ok(remote) => log(
            &app,
            Level::Info,
            format!(
                "update check: the repository has {}, this is {} — a newer version is available",
                short(remote),
                short(&running.commit)
            ),
        ),
        Err(e) => log(&app, Level::Error, format!("update check failed: {e}")),
    }
    {
        let mut t = tracker.lock().unwrap_or_else(|e| e.into_inner());
        match found {
            Ok(remote) => t.checked(&running.commit, &remote),
            // Offline as the window opens is not worth a red line in the
            // status bar; offline when someone pressed the button is.
            Err(e) if asked && matches!(t.status(), update::Status::Current) => t.failed(e),
            Err(_) => {}
        }
    }
    push_update_status(&app);
}

/// Logs where the manifest check left the update status.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn log_update_status(app: &AppHandle) {
    let (level, text) = match app.state::<Updates>().status() {
        update::Status::Current => (
            activity::Level::Info,
            "update check: up to date".to_string(),
        ),
        update::Status::Available => (
            activity::Level::Info,
            "update check: a newer version is available".to_string(),
        ),
        update::Status::Failed { message } => (
            activity::Level::Error,
            format!("update check failed: {message}"),
        ),
        other => (activity::Level::Info, format!("update check: {other:?}")),
    };
    activity::log(app, level, text);
}

/// What the shell logged by itself, for the page to show on load.
#[tauri::command]
fn shell_log(activity: tauri::State<'_, activity::Activity>) -> Vec<activity::Entry> {
    activity.entries()
}

/// Saves a message as an `.eml` file, wherever the save dialog says.
#[tauri::command]
async fn save_message(
    app: AppHandle,
    id: String,
) -> std::result::Result<Option<save::Saved>, String> {
    let source = app.state::<save::Source>();
    save::save(app.clone(), &source, save::What::Message { id }).await
}

/// Saves one attachment, wherever the save dialog says.
#[tauri::command]
async fn save_attachment(
    app: AppHandle,
    id: String,
    part: String,
) -> std::result::Result<Option<save::Saved>, String> {
    let source = app.state::<save::Source>();
    save::save(app.clone(), &source, save::What::Attachment { id, part }).await
}

/// Starts the newest installed build and closes this one.
///
/// The empty command is the portal's way of saying "the app's own command",
/// so this does not need to know what the package calls its launcher.
#[tauri::command]
fn update_restart(
    app: AppHandle,
    updates: tauri::State<'_, Updates>,
) -> std::result::Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        if let Some(portal) = &updates.portal {
            portal.spawn_latest(&[""])?;
            app.exit(0);
            return Ok(());
        }
        let _ = (&app, &updates);
        Err("this build does not update itself".into())
    }

    // The updater wrote the new build in place; restarting is all that is
    // left, and Tauri knows how to relaunch its own bundle.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let _ = updates;
        app.restart();
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (app, updates);
        Err("this build does not update itself".into())
    }
}

/// Marks the page as the desktop shell so the status line can say "local
/// store" rather than the ephemeral loopback address.
const SHELL_SCRIPT: &str = "window.__MAILBOX_SHELL__ = 'gui';";

/// Whether this URL is the in-process server, including the framed body.
fn is_app_url(url: &tauri::Url, port: u16) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        // `about:blank` is how a webview starts; blocking it blanks the window.
        return url.scheme() == "about";
    }
    let host_ok = matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"));
    host_ok && url.port().unwrap_or(0) == port
}

#[cfg(test)]
mod tests {
    use super::is_app_url;

    #[test]
    fn the_loopback_server_is_ours_and_nothing_else_is() {
        let page: tauri::Url = "http://127.0.0.1:4321/?mailbox_token=x".parse().unwrap();
        assert!(is_app_url(&page, 4321));
        assert!(
            !is_app_url(&page, 9),
            "a different port is not this process"
        );

        let body: tauri::Url = "http://127.0.0.1:4321/body?id=abc".parse().unwrap();
        assert!(
            is_app_url(&body, 4321),
            "the framed body is the same origin"
        );

        let remote: tauri::Url = "https://evil.example/".parse().unwrap();
        assert!(!is_app_url(&remote, 4321));

        let blank: tauri::Url = "about:blank".parse().unwrap();
        assert!(is_app_url(&blank, 4321));
    }
}
