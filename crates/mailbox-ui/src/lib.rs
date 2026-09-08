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

pub mod app;
pub mod backend;

pub use app::{App, Focus};
pub use backend::{Backend, Request, Response, Seq};

use std::net::{IpAddr, Ipv4Addr};

use mailbox_core::error::{Error, Result};
use mailbox_core::session::SessionOptions;
use mailbox_core::{Clock, Tracer};
use mailbox_web::server::{make_token, Options, Server};
use tauri::webview::{NewWindowResponse, WebviewWindowBuilder};
use tauri::WebviewUrl;

/// Opens the window and runs until it closes.
pub fn run(opts: SessionOptions, tracer: Tracer, clock: Clock) -> Result<()> {
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

    open_window(&url, port)
}

fn open_window(url: &str, port: u16) -> Result<()> {
    prepare_webview_env();

    let parsed: tauri::Url = url
        .parse()
        .map_err(|e| Error::usage(format!("the GUI URL {url:?} did not parse: {e}")))?;

    let webview_dir = mailbox_core::paths::state_dir().join("webview");
    let _ = std::fs::create_dir_all(&webview_dir);

    tauri::Builder::default()
        .setup(move |app| {
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
                    if is_app_url(&dest, port) {
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
