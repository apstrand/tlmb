//! The §6.4 contract, over HTTP.
//!
//! Every route here answers with the same types `--json` prints, because §4 is
//! explicit that there is one contract and the CLI's output *is* it:
//!
//! > The CLI's `--json` output *is* this contract, so anything the GUI can do,
//! > a script can do.
//!
//! What is deliberately absent is a route that runs a command. Handing a
//! browser an `argv` endpoint would have been a dozen lines and would have made
//! every future subcommand — `pair send`, `|` to a shell — reachable from a
//! page. The routes are named one at a time, and adding one is a decision.

use std::sync::Mutex;

use mailbox_api::BodyFormat;
use mailbox_core::error::{Error, Result};
use mailbox_core::render::RenderOptions;
use mailbox_core::session::{FlagSpec, IdRef, Session, SessionOptions};
use mailbox_core::store::read::{ListOptions, Sort};
use mailbox_core::{Clock, Tracer, Workspace};

use crate::http::{Request, Response};

pub fn handle(
    path: &str,
    request: &Request,
    workspace: &Mutex<Workspace>,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
) -> Response {
    let result = match (request.method.as_str(), path) {
        ("GET", "ls") => list(request, workspace),
        ("GET", "search") => list(request, workspace),
        ("GET", "show") => show(request, workspace),
        ("GET", "folders") => reply(workspace.lock().unwrap().folders()),
        ("GET", "accounts") => reply(workspace.lock().unwrap().accounts()),
        ("GET", "status") => reply(workspace.lock().unwrap().status()),
        ("GET", "snoozed") => reply(
            workspace
                .lock()
                .unwrap()
                .snoozed()
                .map(|snoozed| mailbox_api::SnoozeList { snoozed }),
        ),
        ("POST", "flag") => flag(request, session, tracer, clock),
        ("POST", "move") => move_to(request, session, tracer, clock),
        ("POST", "snooze") => snooze(request, session, tracer, clock),
        ("POST", "sync") => sync(request, workspace, session, tracer, clock),
        ("GET", _) | ("POST", _) => Err(Error::not_found("route", path)),
        _ => Err(Error::usage(format!(
            "{} is not a method this serves",
            request.method
        ))),
    };

    match result {
        Ok(response) => response,
        // The same error shape the CLI writes to stderr (§6.5), so a caller
        // that knows one knows the other.
        Err(e) => Response::json(
            status_for(&e),
            &serde_json::to_value(e.to_response()).unwrap_or_default(),
        ),
    }
}

fn status_for(e: &Error) -> u16 {
    match e {
        Error::NotFound { .. } => 404,
        Error::Usage(_) | Error::Ambiguous { .. } => 400,
        Error::PolicyDenied { .. } | Error::NeedsConfirmation { .. } => 403,
        _ => 500,
    }
}

/// Serialises any contract type into the reply envelope.
fn reply<T: serde::Serialize>(value: Result<T>) -> Result<Response> {
    Ok(Response::json(200, &serde_json::to_value(value?)?))
}

fn list(request: &Request, workspace: &Mutex<Workspace>) -> Result<Response> {
    let query = request.query.get("q").cloned().unwrap_or_default();
    let opts = ListOptions {
        limit: request
            .query
            .get("limit")
            .and_then(|v| v.parse().ok())
            .unwrap_or(50)
            // A page is a page. Without a cap, one request can ask for the
            // whole mailbox and hold the store's lock while it builds it.
            .min(500),
        cursor: request.query.get("cursor").cloned(),
        sort: if query.trim().is_empty() {
            Sort::UnreadFirst
        } else {
            Sort::DateDesc
        },
        ..Default::default()
    };

    let workspace = workspace.lock().unwrap();
    let list = if query.trim().is_empty() {
        workspace.ls(None, &opts)?
    } else {
        workspace.search(&query, &opts)?
    };
    Ok(Response::json(200, &serde_json::to_value(list)?))
}

fn show(request: &Request, workspace: &Mutex<Workspace>) -> Result<Response> {
    let id = request
        .query
        .get("id")
        .ok_or_else(|| Error::usage("show needs an `id`"))?;
    let format = match request.query.get("format").map(String::as_str) {
        Some("text") => BodyFormat::Text,
        Some("html") => BodyFormat::Html,
        // Markdown by default, as the TUI takes it: the structure of HTML mail
        // survives, and the shared UI styles it.
        None | Some("md") => BodyFormat::Md,
        Some(other) => {
            return Err(Error::usage(format!(
                "`{other}` is not a body format; try text, md, or html"
            )))
        }
    };
    let as_thread = request.query.get("thread").is_some_and(|v| v == "1");

    let render = RenderOptions {
        format,
        ..Default::default()
    };
    let workspace = workspace.lock().unwrap();
    let result = workspace.show(&IdRef::parse(id), &render, as_thread)?;
    Ok(Response::json(200, &serde_json::to_value(result)?))
}

/// Opens a writable session for one operation.
///
/// Per request rather than held: the server's own handle is read-only, so
/// reads never contend with a write, and a write that fails leaves nothing
/// half-open (§4.1).
fn writable(session: &SessionOptions, tracer: &Tracer, clock: &Clock) -> Result<Session> {
    let mut opts = session.clone();
    opts.read_only = false;
    Session::open(&opts, tracer.clone(), clock.clone())
}

fn body(request: &Request) -> Result<serde_json::Value> {
    serde_json::from_slice(&request.body)
        .map_err(|e| Error::usage(format!("the request body is not JSON: {e}")))
}

fn ids(value: &serde_json::Value) -> Result<Vec<IdRef>> {
    let list = value
        .get("ids")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::usage("give `ids` as an array of message ids"))?;
    if list.is_empty() {
        return Err(Error::usage("no message given"));
    }
    Ok(list
        .iter()
        .filter_map(|v| v.as_str())
        .map(IdRef::parse)
        .collect())
}

fn strings(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn flag(
    request: &Request,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
) -> Result<Response> {
    let body = body(request)?;
    let refs = ids(&body)?;
    let spec = FlagSpec {
        add_flags: strings(&body, "add"),
        remove_flags: strings(&body, "remove"),
        add_tags: strings(&body, "add_tags"),
        remove_tags: strings(&body, "remove_tags"),
    };

    let mut session = writable(session, tracer, clock)?;
    let written = session.flag(&refs, &spec)?;
    Ok(Response::json(
        200,
        &serde_json::json!({
            "changed": written.iter().filter(|w| w.changed).count(),
            "messages": written.iter().map(|w| serde_json::json!({
                "id": w.message_id,
                "folder": w.folder,
                "flags": w.flags,
                "changed": w.changed,
            })).collect::<Vec<_>>(),
        }),
    ))
}

fn move_to(
    request: &Request,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
) -> Result<Response> {
    let body = body(request)?;
    let refs = ids(&body)?;
    let folder = body
        .get("folder")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::usage("give a `folder` to move to"))?;

    let mut session = writable(session, tracer, clock)?;
    let written = session.move_to(&refs, folder)?;
    Ok(Response::json(
        200,
        &serde_json::json!({
            "changed": written.iter().filter(|w| w.changed).count(),
            "folder": folder,
        }),
    ))
}

fn snooze(
    request: &Request,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
) -> Result<Response> {
    let body = body(request)?;
    let refs = ids(&body)?;

    let mut session = writable(session, tracer, clock)?;
    match body.get("until").and_then(|v| v.as_str()) {
        Some(until) => {
            let when = mailbox_core::store::snooze::parse_until(until, clock)?;
            let ids = session.snooze(&refs, when)?;
            Ok(Response::json(
                200,
                &serde_json::json!({
                    "snoozed": ids,
                    "until": mailbox_core::clock::format_rfc3339(when),
                    "device": mailbox_core::paths::device_name(),
                }),
            ))
        }
        // No time means wake it, which is what the picker's "unsnooze" does.
        None => Ok(Response::json(
            200,
            &serde_json::json!({ "woke": session.unsnooze(&refs)? }),
        )),
    }
}

fn sync(
    request: &Request,
    workspace: &Mutex<Workspace>,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
) -> Result<Response> {
    let body = if request.body.is_empty() {
        serde_json::Value::Null
    } else {
        body(request)?
    };
    let opts = mailbox_core::imap::sync::SyncOptions {
        full: body.get("full").and_then(|v| v.as_bool()).unwrap_or(false),
        ..Default::default()
    };

    let mut opts_session = session.clone();
    opts_session.read_only = false;
    let mut writable = Workspace::open(&opts_session, tracer.clone(), clock.clone())?;
    let result = writable.sync(&opts)?;
    // The reading handle has an older view of the store until it reopens; the
    // caller asked for a sync and then reads, so refresh it now rather than
    // leaving the next `ls` to show what was there before.
    *workspace.lock().unwrap() = Workspace::open(session, tracer.clone(), clock.clone())?;
    Ok(Response::json(200, &serde_json::to_value(result)?))
}

/// A message body, as a document of its own, for the interface to frame.
///
/// Its own URL rather than a `srcdoc`, and that is the whole point. A `srcdoc`
/// frame inherits the embedding page's content policy, so the *page's*
/// `img-src` decides whether a message's images can load — which meant the
/// reader could say "load images" and be quietly overruled by a header written
/// for a different purpose. A framed document with a real URL gets exactly the
/// policy sent with it, and the `sandbox` attribute still gives it a unique
/// origin, so nothing here can reach the page around it (§9).
///
/// This is a page rather than an API route, and is authenticated like one: a
/// frame navigation cannot set a header, so it carries the cookie. That is
/// safe here in a way it would not be for a write — no other origin can frame
/// it (`frame-ancestors 'self'`) or read it (no CORS), and it changes nothing.
pub fn body_document(request: &Request, workspace: &Mutex<Workspace>) -> Response {
    let Some(id) = request.query.get("id") else {
        return Response::text(400, "which message?");
    };
    // The reader's decision, arriving as they made it. Off unless asked for.
    let images = request.query.get("images").is_some_and(|v| v == "1");

    let render = RenderOptions {
        format: BodyFormat::Html,
        ..Default::default()
    };
    let shown = match workspace
        .lock()
        .unwrap()
        .show(&IdRef::parse(id), &render, false)
    {
        Ok(shown) => shown,
        Err(e) => {
            return document(
                &format!("<p><em>{}</em></p>", escape(&e.to_string())),
                false,
            )
        }
    };

    let content = shown
        .message
        .and_then(|m| m.body)
        .map(|b| b.content)
        .unwrap_or_else(|| "<p><em>The body has not been downloaded yet.</em></p>".to_string());

    // The sanitizer disarmed every remote reference into `data-blocked-src`
    // (§9). Re-arming them is this one substitution, made where the disarming
    // happened rather than by string surgery in a browser.
    let content = if images {
        content.replace("data-blocked-src=", "src=")
    } else {
        // A placeholder instead of the browser's broken-image icon, which says
        // "this message is damaged" when what happened is that we declined to
        // fetch something. Substituted for the missing `src`, not alongside a
        // real one — an `<img>` with two `src` attributes uses the first, which
        // is how "load images" managed to load nothing.
        content.replace("<img ", &format!("<img src=\"{PLACEHOLDER}\" "))
    };

    document(&content, images)
}

/// A grey dashed rectangle, inline, so nothing is fetched to draw it.
const PLACEHOLDER: &str = "data:image/svg+xml;charset=utf-8,\
    %3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='48'%20height='32'%3E\
    %3Crect%20width='48'%20height='32'%20rx='3'%20fill='none'%20stroke='%239a9aa0'\
    %20stroke-dasharray='3%202'/%3E%3C/svg%3E";

fn document(content: &str, images: bool) -> Response {
    let img = if images {
        "img-src data: https: http:"
    } else {
        "img-src data:"
    };
    let page = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <style>{STYLE}</style></head><body>{content}</body></html>"
    );

    Response::new(200, "text/html; charset=utf-8", page.into_bytes())
        .framed()
        .with(
            "Content-Security-Policy",
            &format!(
                "default-src 'none'; {img}; style-src 'unsafe-inline'; \
                 frame-ancestors 'self'; base-uri 'none'; form-action 'none'"
            ),
        )
}

/// The reading typography. Inline because the frame has a unique origin and
/// cannot fetch a stylesheet from anywhere, including from us.
const STYLE: &str = "\
:root { color-scheme: light dark; }
body { margin: 1rem 1.1rem; font: 14px/1.6 -apple-system, BlinkMacSystemFont, \
'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif; overflow-wrap: anywhere; }
img, table { max-width: 100%; }
img[data-blocked-src] { opacity: .55; }
blockquote { margin: .6em 0 .6em .4em; padding-left: .8em; \
border-left: 2px solid currentColor; opacity: .65; }
pre { white-space: pre-wrap; font: 13px/1.5 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
a { color: #0a66d0; }
@media (prefers-color-scheme: dark) { a { color: #64a6f5; } }
";

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
