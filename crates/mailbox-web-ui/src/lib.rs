//! The whole crate is wasm-only.
//!
//! Compiled for the host it is an empty library, so `cargo build` and
//! `cargo test` at the workspace root do not need the wasm target installed —
//! and `./build-web.sh` is the one command that does.
#![cfg(target_arch = "wasm32")]

//! **The web shell** (§3): the shared interface, in a browser.
//!
//! > The web interface is compiled into every build. […] It is the *same* UI
//! > the desktop GUI renders — one implementation, two shells.
//!
//! So there is almost nothing here. The interface is [`mailbox_ui`]; this
//! crate is the other half of what the desktop shell provides — a place to
//! draw, and a way to fetch — and no mail logic at all.
//!
//! # What is different about a browser
//!
//! No threads and no blocking. The desktop backend hands work to a worker
//! thread and collects it later; this one starts a `fetch` and collects it
//! later. The [`mailbox_ui::Backend`] contract was written so those are the
//! same shape, which is what lets one interface sit on both.
//!
//! # The token
//!
//! It arrives as an argument to [`start`], read from the page the server
//! rendered it into, and goes out on every request as `X-Mailbox-Token`. It is
//! never put in a URL, so it stays out of history and out of any `Referer`.

use std::cell::RefCell;
use std::rc::Rc;

use mailbox_ui::backend::{Backend, Request, Response, Seq};
use wasm_bindgen::prelude::*;

/// Starts the interface on a canvas. Called by the page's loader.
#[wasm_bindgen]
pub async fn start(canvas_id: String, token: String) -> Result<(), JsValue> {
    // A panic in a browser is otherwise a silent stop with a blank canvas.
    console_error_panic_hook();

    let backend = Http::new(token);
    eframe::WebRunner::new()
        .start(
            &canvas_id,
            eframe::WebOptions::default(),
            Box::new(move |_cc| Ok(Box::new(Web::new(backend)))),
        )
        .await
}

/// Reports a panic to the console instead of stopping silently.
fn console_error_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            web_sys::console::error_1(&JsValue::from_str(&info.to_string()));
            previous(info);
        }));
    });
}

struct Web {
    app: mailbox_ui::App,
    backend: Http,
    started: bool,
}

impl Web {
    fn new(backend: Http) -> Web {
        Web {
            app: mailbox_ui::App::new(),
            backend,
            started: false,
        }
    }
}

impl eframe::App for Web {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.started {
            self.started = true;
            self.app.start(&mut self.backend);
        }
        if self.app.pump(&mut self.backend) {
            ctx.request_repaint();
        }
        mailbox_ui::view::draw(ctx, &mut self.app, &mut self.backend);

        if self.app.in_flight > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }
}

/// The browser backend: `fetch` against the server that served this page.
struct Http {
    token: String,
    /// Filled by the futures as they finish, drained by the interface each
    /// frame. `Rc<RefCell<_>>` rather than a channel because a browser is
    /// single-threaded and this never crosses one.
    inbox: Rc<RefCell<Vec<(Seq, Response)>>>,
}

impl Http {
    fn new(token: String) -> Http {
        Http {
            token,
            inbox: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

impl Backend for Http {
    fn send(&mut self, seq: Seq, request: Request) {
        let token = self.token.clone();
        let inbox = self.inbox.clone();

        wasm_bindgen_futures::spawn_local(async move {
            let response = match call(&token, &request).await {
                Ok(value) => shape(&request, Ok(value)),
                Err(e) => shape(&request, Err(describe(&e))),
            };
            inbox.borrow_mut().push((seq, response));
        });
    }

    fn poll(&mut self) -> Option<(Seq, Response)> {
        let mut inbox = self.inbox.borrow_mut();
        if inbox.is_empty() {
            None
        } else {
            Some(inbox.remove(0))
        }
    }

    fn label(&self) -> String {
        web_sys::window()
            .and_then(|w| w.location().host().ok())
            .unwrap_or_else(|| "this device".into())
    }
}

/// Turns a request into a call on the §6.4 routes, and back into JSON.
async fn call(token: &str, request: &Request) -> Result<serde_json::Value, JsValue> {
    let (method, path, body) = route(request);

    let init = web_sys::RequestInit::new();
    init.set_method(method);
    if let Some(body) = &body {
        init.set_body(&JsValue::from_str(body));
    }

    let js_request = web_sys::Request::new_with_str_and_init(&path, &init)?;
    // The header the server demands, and that a cross-origin page cannot send.
    js_request.headers().set("X-Mailbox-Token", token)?;
    if body.is_some() {
        js_request
            .headers()
            .set("Content-Type", "application/json")?;
    }

    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&js_request))
        .await?
        .dyn_into::<web_sys::Response>()?;

    let text = wasm_bindgen_futures::JsFuture::from(response.text()?)
        .await?
        .as_string()
        .unwrap_or_default();
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| JsValue::from_str(&format!("the server did not answer with JSON: {e}")))?;

    if !response.ok() {
        // §6.5's error shape, which the server answers with for every failure.
        let message = value["error"]["message"]
            .as_str()
            .unwrap_or("the request failed")
            .to_string();
        return Err(JsValue::from_str(&message));
    }
    Ok(value)
}

/// Which route answers a request. Named here so the browser and the desktop
/// disagree in exactly one place, and it is this one.
fn route(request: &Request) -> (&'static str, String, Option<String>) {
    match request {
        Request::List { query, limit } => (
            "GET",
            format!("/api/ls?q={}&limit={limit}", encode(query)),
            None,
        ),
        // The flattened text form, as the desktop shell asks for.
        Request::Open { id } => (
            "GET",
            format!("/api/show?id={}&format=text", encode(id)),
            None,
        ),
        Request::Folders => ("GET", "/api/folders".into(), None),
        Request::Flag { ids, add, remove } => (
            "POST",
            "/api/flag".into(),
            Some(serde_json::json!({ "ids": ids, "add": add, "remove": remove }).to_string()),
        ),
        Request::Move { ids, folder } => (
            "POST",
            "/api/move".into(),
            Some(serde_json::json!({ "ids": ids, "folder": folder }).to_string()),
        ),
        Request::Snooze { ids, until } => (
            "POST",
            "/api/snooze".into(),
            Some(serde_json::json!({ "ids": ids, "until": until }).to_string()),
        ),
        Request::Sync => ("POST", "/api/sync".into(), Some("{}".into())),
    }
}

/// Percent-encodes a query parameter.
///
/// Written out rather than pulled in: the set of characters that must not
/// appear in a query string is small and fixed, and a crate for it would be a
/// dependency in a wasm bundle that has to be downloaded.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Shapes a JSON answer, or a failure, as the response to the question asked.
fn shape(request: &Request, result: Result<serde_json::Value, String>) -> Response {
    fn parse<T: serde::de::DeserializeOwned>(
        value: Result<serde_json::Value, String>,
    ) -> Result<Box<T>, String> {
        serde_json::from_value(value?)
            .map(Box::new)
            .map_err(|e| format!("unexpected answer: {e}"))
    }

    match request {
        Request::List { .. } => Response::List(parse(result)),
        Request::Open { .. } => Response::Opened(parse(result)),
        Request::Folders => Response::Folders(parse(result)),
        Request::Sync => Response::Synced(parse(result)),
        Request::Flag { .. } | Request::Move { .. } | Request::Snooze { .. } => {
            Response::Changed(result.map(
                |value| match value.get("changed").and_then(|c| c.as_u64()) {
                    Some(n) => format!("{n} message{} changed", if n == 1 { "" } else { "s" }),
                    None => "done".to_string(),
                },
            ))
        }
    }
}

fn describe(error: &JsValue) -> String {
    error
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(error, &JsValue::from_str("message"))
                .ok()?
                .as_string()
        })
        .unwrap_or_else(|| "the request failed".to_string())
}
