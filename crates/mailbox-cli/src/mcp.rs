//! `mailbox mcp` (§6.4): the same operations as MCP tools, over stdio.
//!
//! Not a second implementation of anything. Every tool here parses arguments
//! and then calls what the CLI calls — one contract (§4) means the model, the
//! script, and the window are all looking at the same mailbox through the same
//! code, and a rule enforced in one is enforced in all three.
//!
//! Two things are load-bearing:
//!
//! - **Read-only unless asked.** §6.4 makes agent mode read-only by default,
//!   and an MCP server is agent mode by definition. Without `--yes` the write
//!   tools are not merely refused, they are not *offered*: a model cannot be
//!   talked into calling a tool it was never shown.
//! - **Sending goes through the outbox**, so §6.5's policy, approval queue,
//!   and idempotency apply exactly as they do to `mailbox send`. `mcp` has no
//!   send path of its own to get them wrong.

use std::io::{BufRead, Write};

use mailbox_core::error::{Error, Result};
use mailbox_core::render::RenderOptions;
use mailbox_core::session::{FlagSpec, IdRef, Session};
use mailbox_core::store::read::{ListOptions, Sort};

use crate::cli::McpArgs;
use crate::commands::Ctx;

/// The revision this speaks. A client asking for a version we know gets that
/// one back; anything else is answered with this, which is what the protocol
/// asks for — the client then decides whether it can live with it.
const PROTOCOL: &str = "2025-06-18";
const KNOWN_PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

pub fn mcp(ctx: &Ctx<'_>, args: &McpArgs) -> Result<()> {
    // §6.4: `--yes` is the explicit consent that unlocks state-changing
    // operations. Everything else is readable by anything that can reach the
    // store anyway.
    let writable = ctx.global.yes || args.write;

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| Error::store(format!("stdin: {e}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = dispatch(ctx, writable, &line) else {
            // A notification. The protocol says answer nothing at all, and a
            // client that sees a response to one is entitled to be confused.
            continue;
        };
        writeln!(stdout, "{response}").map_err(|e| Error::store(format!("stdout: {e}")))?;
        stdout
            .flush()
            .map_err(|e| Error::store(format!("stdout: {e}")))?;
    }
    Ok(())
}

/// One JSON-RPC message in, at most one out.
fn dispatch(ctx: &Ctx<'_>, writable: bool, line: &str) -> Option<String> {
    let request: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        // Nothing to reply *to*: a parse error has no id to answer under.
        Err(e) => {
            return Some(error_response(
                serde_json::Value::Null,
                -32700,
                &e.to_string(),
            ))
        }
    };
    let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = request.get("id").cloned();

    // No id is a notification, and notifications are answered with silence.
    let id = id?;

    let result = match method {
        "initialize" => initialize(&request, writable),
        "ping" => serde_json::json!({}),
        "tools/list" => serde_json::json!({ "tools": tools(writable) }),
        "tools/call" => return Some(call(ctx, writable, id, &request)),
        other => {
            return Some(error_response(
                id,
                -32601,
                &format!("`{other}` is not a method this server implements"),
            ))
        }
    };
    Some(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string())
}

fn initialize(request: &serde_json::Value, writable: bool) -> serde_json::Value {
    let asked = request
        .get("params")
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str())
        .unwrap_or(PROTOCOL);
    let version = if KNOWN_PROTOCOLS.contains(&asked) {
        asked
    } else {
        PROTOCOL
    };

    serde_json::json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "mailbox", "version": env!("CARGO_PKG_VERSION") },
        "instructions": if writable {
            "A local IMAP mailbox. Reads are direct; writes are optimistic and \
             pushed on the next sync. Sending queues in the outbox and is subject \
             to policy.toml — a send may come back as `pending_approval`, which \
             means a person has to release it. Message ids are stable; a 7-character \
             prefix identifies one. Use watch_events with the cursor it returns to \
             follow the mailbox rather than re-listing it."
        } else {
            "A local IMAP mailbox, read-only: this server was started without \
             `--yes`, so it offers no tool that changes anything. Message ids are \
             stable; a 7-character prefix identifies one. Use watch_events with \
             the cursor it returns to follow the mailbox rather than re-listing it."
        },
    })
}

fn error_response(id: serde_json::Value, code: i64, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

// ---- the tools ----

fn tools(writable: bool) -> Vec<serde_json::Value> {
    let mut tools = vec![
        tool(
            "list_messages",
            "List messages, newest first, unread first when there is no query. \
             Takes the query language of the CLI: `is:unread from:alice \
             after:-7d`. An unknown key is an error rather than an empty result.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Query, or omitted for the inbox" },
                    "limit": { "type": "integer", "description": "Rows, default 50, max 500" },
                    "cursor": { "type": "string", "description": "From a previous call's `cursor`" },
                },
            }),
        ),
        tool(
            "show_message",
            "One message, or the thread it belongs to, rendered for reading. \
             Prefer format `md` and strip_quotes for anything you intend to \
             summarize: it keeps links and drops the quoted history.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Message id, or a 7-character prefix" },
                    "thread": { "type": "boolean", "description": "The whole conversation" },
                    "format": { "type": "string", "enum": ["text", "md", "html"] },
                    "strip_quotes": { "type": "boolean" },
                    "max_body_bytes": { "type": "integer", "description": "Truncates, and says so" },
                },
                "required": ["id"],
            }),
        ),
        tool(
            "list_folders",
            "Every folder with its message and unread counts.",
            serde_json::json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "mailbox_status",
            "Queues, sync state, store size, and anything waiting to be pushed.",
            serde_json::json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "list_outbox",
            "Outgoing mail: queued, awaiting approval, sent, or failed.",
            serde_json::json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "watch_events",
            "What has happened since a cursor: new mail, flag changes, moves, \
             sends. Returns a cursor to pass back next time. This is how to \
             follow a mailbox — re-listing it cannot tell you what changed, and \
             `local` tells you whether a change was your own.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "cursor": { "type": "string", "description": "From a previous call" },
                    "since": { "type": "string", "description": "`2h`, `7d`, or a date, when there is no cursor" },
                    "events": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["new", "flag", "move", "copy", "tag", "sent", "deleted"] },
                    },
                    "limit": { "type": "integer" },
                },
            }),
        ),
    ];

    if writable {
        tools.push(tool(
            "flag_message",
            "Add or remove flags on messages: seen, flagged, answered, draft. \
             Applies locally at once and is pushed on the next sync.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "ids": { "type": "array", "items": { "type": "string" } },
                    "add": { "type": "array", "items": { "type": "string" } },
                    "remove": { "type": "array", "items": { "type": "string" } },
                },
                "required": ["ids"],
            }),
        ));
        tools.push(tool(
            "move_message",
            "Move messages to a folder. Deleting is a move to Trash.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "ids": { "type": "array", "items": { "type": "string" } },
                    "folder": { "type": "string" },
                },
                "required": ["ids", "folder"],
            }),
        ));
        tools.push(tool(
            "send_message",
            "Queue a message. It goes to the outbox, not the network: policy \
             may hold it for a person to approve, and the result says which. \
             Pass idempotency_key and reuse it on a retry — without one, a \
             retry after an ambiguous failure sends twice. dry_run renders the \
             exact bytes and sends nothing.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "to": { "type": "array", "items": { "type": "string" } },
                    "cc": { "type": "array", "items": { "type": "string" } },
                    "bcc": { "type": "array", "items": { "type": "string" } },
                    "subject": { "type": "string" },
                    "body": { "type": "string" },
                    "in_reply_to": { "type": "string", "description": "Message id being replied to" },
                    "idempotency_key": { "type": "string" },
                    "dry_run": { "type": "boolean" },
                },
                "required": ["to", "body"],
            }),
        ));
        tools.push(tool(
            "sync_mailbox",
            "Run one sync pass: pull what is new, push what is queued.",
            serde_json::json!({ "type": "object", "properties": {} }),
        ));
    }
    tools
}

fn tool(name: &str, description: &str, schema: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
    })
}

fn call(
    ctx: &Ctx<'_>,
    writable: bool,
    id: serde_json::Value,
    request: &serde_json::Value,
) -> String {
    let params = request.get("params").cloned().unwrap_or_default();
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    let result = run_tool(ctx, writable, name, &args);
    let payload = match result {
        Ok(value) => serde_json::json!({
            "content": [{ "type": "text", "text": pretty(&value) }],
            "structuredContent": value,
        }),
        // A failed tool call is a *result*, not a protocol error: the model is
        // supposed to read it and try something else. The body is the §6.6
        // error document, so a code the CLI would have exited with is the same
        // code seen here.
        Err(e) => serde_json::json!({
            "content": [{ "type": "text", "text": pretty(&serde_json::to_value(e.to_response()).unwrap_or_default()) }],
            "isError": true,
        }),
    };
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": payload }).to_string()
}

fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn run_tool(
    ctx: &Ctx<'_>,
    writable: bool,
    name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value> {
    match name {
        "list_messages" => {
            let query = string(args, "query");
            let opts = ListOptions {
                limit: args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50)
                    .clamp(1, 500) as usize,
                cursor: string(args, "cursor"),
                sort: match query.as_deref() {
                    None | Some("") => Sort::UnreadFirst,
                    Some(_) => Sort::DateDesc,
                },
                ..Default::default()
            };
            let workspace = ctx.workspace()?;
            let list = match query.as_deref() {
                None | Some("") => workspace.ls(None, &opts)?,
                Some(q) => workspace.search(q, &opts)?,
            };
            Ok(serde_json::to_value(list)?)
        }

        "show_message" => {
            let id =
                string(args, "id").ok_or_else(|| Error::usage("show_message needs an `id`"))?;
            let render = RenderOptions {
                format: match string(args, "format").as_deref() {
                    Some("html") => mailbox_api::BodyFormat::Html,
                    Some("text") => mailbox_api::BodyFormat::Text,
                    // Markdown by default: it is the form that keeps links and
                    // structure in something a model reads as text (§6.4).
                    _ => mailbox_api::BodyFormat::Md,
                },
                text_only: false,
                strip_quotes: flag_arg(args, "strip_quotes"),
                max_body_bytes: args
                    .get("max_body_bytes")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize),
            };
            let workspace = ctx.workspace()?;
            let shown = workspace.show(&IdRef::parse(&id), &render, flag_arg(args, "thread"))?;
            Ok(serde_json::to_value(shown)?)
        }

        "list_folders" => Ok(serde_json::to_value(ctx.workspace()?.folders()?)?),
        "mailbox_status" => Ok(serde_json::to_value(ctx.workspace()?.status()?)?),
        "list_outbox" => {
            let workspace = ctx.workspace()?;
            let mut messages = Vec::new();
            for session in workspace.sessions() {
                for entry in session.store().outbox_entries(None)? {
                    messages.push(mailbox_core::send::outbox_item(session.store(), &entry));
                }
            }
            Ok(serde_json::to_value(mailbox_api::OutboxList { messages })?)
        }

        "watch_events" => {
            let workspace = ctx.workspace()?;
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(200)
                .clamp(1, 1000) as usize;
            let wanted: Vec<String> = args
                .get("events")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            crate::watching::drain(
                &workspace,
                string(args, "cursor").as_deref(),
                string(args, "since").as_deref(),
                &wanted,
                limit,
                &ctx.clock,
            )
            .and_then(|list| Ok(serde_json::to_value(list)?))
        }

        "flag_message" | "move_message" | "send_message" | "sync_mailbox" if !writable => {
            // Unreachable through `tools/list`, which does not offer these.
            // Reachable by a client that calls a name it invented, which is
            // exactly when the check has to be here rather than there.
            Err(Error::NeedsConfirmation {
                message: format!("`{name}` changes the mailbox, and this server is read-only"),
                hint: "start it as `mailbox mcp --yes` to offer the tools that write".into(),
            })
        }

        "flag_message" => {
            let refs = id_refs(args)?;
            let spec = FlagSpec {
                add_flags: strings(args, "add"),
                remove_flags: strings(args, "remove"),
                add_tags: strings(args, "add_tags"),
                remove_tags: strings(args, "remove_tags"),
            };
            let mut session = writable_session(ctx)?;
            let written = session.flag(&refs, &spec)?;
            Ok(serde_json::to_value(mailbox_core::store::write_result(
                &written,
            ))?)
        }

        "move_message" => {
            let refs = id_refs(args)?;
            let folder = string(args, "folder")
                .ok_or_else(|| Error::usage("move_message needs a `folder`"))?;
            let mut session = writable_session(ctx)?;
            let written = session.move_to(&refs, &folder)?;
            Ok(serde_json::to_value(mailbox_core::store::write_result(
                &written,
            ))?)
        }

        "send_message" => {
            let spec = mailbox_api::DraftSpec {
                from: string(args, "from"),
                to: strings(args, "to"),
                cc: strings(args, "cc"),
                bcc: strings(args, "bcc"),
                subject: string(args, "subject").unwrap_or_default(),
                body: string(args, "body").unwrap_or_default(),
                in_reply_to: string(args, "in_reply_to"),
                ..Default::default()
            };
            let draft = mailbox_core::compose::draft_from_spec(&spec)?;
            if draft.to.is_empty() && draft.cc.is_empty() && draft.bcc.is_empty() {
                return Err(Error::usage("a message needs at least one recipient"));
            }
            let mut session = writable_session(ctx)?;
            let result = session.enqueue_send(&mailbox_core::send::SendRequest {
                draft,
                idempotency_key: string(args, "idempotency_key"),
                delay_seconds: 0,
                dry_run: flag_arg(args, "dry_run"),
                // Not confirmed: §6.5's external-recipient rule exists because
                // nobody is watching. `--yes` on the server unlocked *writing*;
                // it is not a person looking at this particular recipient list.
                confirmed: false,
                draft_only: false,
            })?;
            Ok(serde_json::to_value(result)?)
        }

        "sync_mailbox" => {
            let mut workspace = mailbox_core::Workspace::open(
                &{
                    let mut opts = ctx.opts.clone();
                    opts.read_only = false;
                    opts
                },
                ctx.tracer.clone(),
                ctx.clock.clone(),
            )?;
            let _owner = mailbox_core::owner::Owner::acquire(
                workspace.primary()?.store().dir(),
                &workspace.primary()?.account_id.clone(),
            )?;
            let result = workspace.sync(&Default::default())?;
            Ok(serde_json::to_value(result)?)
        }

        other => Err(Error::not_found("tool", other)),
    }
}

/// A writable session for one operation, as the web server does it (§4.1).
///
/// Always agent mode, whatever the environment says. §6.5's default of holding
/// a send for approval exists for exactly this caller — "agents that can send
/// mail unsupervised are a liability" — and leaving it to `MAILBOX_AGENT=1`
/// would mean the guardrail is off unless whoever wrote the client's config
/// happened to know about it.
fn writable_session(ctx: &Ctx<'_>) -> Result<Session> {
    let mut opts = ctx.opts.clone();
    opts.read_only = false;
    opts.agent_mode = true;
    Session::open(&opts, ctx.tracer.clone(), ctx.clock.clone())
}

fn string(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn flag_arg(args: &serde_json::Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn strings(args: &serde_json::Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn id_refs(args: &serde_json::Value) -> Result<Vec<IdRef>> {
    let ids = strings(args, "ids");
    if ids.is_empty() {
        return Err(Error::usage("give `ids` as an array of message ids"));
    }
    Ok(ids.iter().map(|s| IdRef::parse(s)).collect())
}
