//! `send`, `compose`, `reply`, and `outbox` (§6.1, §5.5, §6.5).
//!
//! Sending is the one thing this binary does that cannot be undone, so the
//! shape here is deliberate: build and check first, hand to the outbox second,
//! and let a separate flush put it on the wire. Nothing in this file talks to
//! an SMTP server.

use std::io::Read;

use mailbox_api::{Address, OutboxList, SendResult};
use mailbox_core::compose::{Attachment, Draft};
use mailbox_core::error::{Error, Result};
use mailbox_core::send::{ReplyMode, SendRequest};
use mailbox_core::session::IdRef;
use mailbox_core::store::OutboxState;

use crate::cli::{ComposeArgs, OutboxAction, OutboxArgs, ReplyArgs, SendArgs};
use crate::commands::Ctx;
use crate::output::{GREEN, YELLOW};

pub fn send(ctx: &Ctx<'_>, args: &SendArgs) -> Result<()> {
    ctx.confirm("send")?;
    let mut session = ctx.session()?;

    let draft = if args.stdin {
        // §13: `mailbox send --stdin` takes exact RFC5322 bytes, which is what
        // makes `sendmail_command = "mailbox send --stdin"` and `git
        // send-email` work.
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes)?;
        draft_from_rfc5322(&bytes, args)?
    } else {
        let body = match &args.body {
            Some(b) => b.clone(),
            None => read_stdin_text()?,
        };
        Draft {
            from: parse_one(args.from.as_deref())?,
            to: parse_addresses(&args.to)?,
            cc: parse_addresses(&args.cc)?,
            bcc: parse_addresses(&args.bcc)?,
            reply_to: parse_addresses(args.reply_to.as_slice())?,
            subject: args.subject.clone().unwrap_or_default(),
            body,
            attachments: load_attachments(&args.attach)?,
            ..Default::default()
        }
    };

    let result = session.enqueue_send(&SendRequest {
        draft,
        idempotency_key: args.idempotency_key.clone(),
        delay_seconds: args.delay.unwrap_or(0),
        dry_run: ctx.global.dry_run,
        confirmed: ctx.global.yes,
        draft_only: args.draft,
    })?;
    report_send(ctx, "send", &result)
}

pub fn compose(ctx: &Ctx<'_>, args: &ComposeArgs) -> Result<()> {
    ctx.confirm("compose")?;
    let mut session = ctx.session()?;

    let template = format!(
        "To: {}\nSubject: {}\n\n",
        args.to.join(", "),
        args.subject.clone().unwrap_or_default()
    );
    let edited = edit_in_editor(&session.config.general.editor, &template)?;
    let mut draft = draft_from_edited(&edited)?;
    draft.from = parse_one(args.from.as_deref())?;
    draft.attachments = load_attachments(&args.attach)?;

    if draft.body.trim().is_empty() && draft.attachments.is_empty() {
        // An empty buffer is how every editor-driven tool says "cancel".
        ctx.out.print(ctx.out.dim("empty message — nothing queued"));
        return Ok(());
    }

    let result = session.enqueue_send(&SendRequest {
        draft,
        idempotency_key: None,
        delay_seconds: args.delay.unwrap_or(0),
        dry_run: ctx.global.dry_run,
        confirmed: ctx.global.yes,
        draft_only: args.draft,
    })?;
    report_send(ctx, "compose", &result)
}

pub fn reply(ctx: &Ctx<'_>, args: &ReplyArgs) -> Result<()> {
    ctx.confirm("reply")?;
    let id_ref = IdRef::parse(&args.id);
    ctx.warn_if_positional(&id_ref);

    let mut session = ctx.session()?;
    let mode = if args.forward {
        ReplyMode::Forward
    } else if args.all {
        ReplyMode::All
    } else {
        ReplyMode::Sender
    };

    let mut draft = session.reply_draft(&id_ref, mode)?;
    if !args.to.is_empty() {
        draft.to = parse_addresses(&args.to)?;
    }
    if mode == ReplyMode::Forward && draft.to.is_empty() {
        return Err(Error::usage("a forward needs `--to`"));
    }

    // The quoted original is already in `draft.body`; whatever the user writes
    // goes above it, which is where a reply belongs.
    draft.body = match &args.body {
        Some(b) => format!("{b}{}", draft.body),
        None if std::io::IsTerminal::is_terminal(&std::io::stdin()) => {
            edit_in_editor(&session.config.general.editor, &draft.body)?
        }
        None => format!("{}{}", read_stdin_text()?, draft.body),
    };
    draft.attachments = load_attachments(&args.attach)?;

    let result = session.enqueue_send(&SendRequest {
        draft,
        idempotency_key: args.idempotency_key.clone(),
        delay_seconds: args.delay.unwrap_or(0),
        dry_run: ctx.global.dry_run,
        confirmed: ctx.global.yes,
        draft_only: args.draft,
    })?;
    report_send(ctx, "reply", &result)
}

pub fn outbox(ctx: &Ctx<'_>, args: &OutboxArgs) -> Result<()> {
    match args
        .action
        .as_ref()
        .unwrap_or(&OutboxAction::List { state: None })
    {
        OutboxAction::List { state } => list(ctx, state.as_deref()),
        OutboxAction::Approve { id } => transition(ctx, *id, OutboxState::Queued, "approved"),
        OutboxAction::Retry { id } => transition(ctx, *id, OutboxState::Queued, "requeued"),
        OutboxAction::Cancel { id } => transition(ctx, *id, OutboxState::Cancelled, "cancelled"),
        OutboxAction::Flush => flush(ctx),
    }
}

fn list(ctx: &Ctx<'_>, state: Option<&str>) -> Result<()> {
    let session = ctx.session()?;
    let filter = state.map(|s| match s {
        "draft" => Ok(OutboxState::Draft),
        "pending_approval" => Ok(OutboxState::PendingApproval),
        "queued" => Ok(OutboxState::Queued),
        "sent" => Ok(OutboxState::Sent),
        "failed" => Ok(OutboxState::Failed),
        "cancelled" => Ok(OutboxState::Cancelled),
        other => Err(Error::usage(format!("`{other}` is not an outbox state"))),
    });
    let filter = match filter {
        Some(r) => Some(r?),
        None => None,
    };

    let entries = session.store().outbox_entries(filter)?;
    let messages: Vec<_> = entries
        .iter()
        .map(|e| mailbox_core::send::outbox_item(session.store(), e))
        .collect();

    if ctx.out.is_structured() {
        ctx.out.reply("outbox", &OutboxList { messages })?;
        return Ok(());
    }
    if messages.is_empty() {
        ctx.out.print(ctx.out.dim("the outbox is empty"));
        return Ok(());
    }

    use crate::table::{Column, Table};
    let mut table = Table::new(vec![
        Column::new("id").right().min(3),
        Column::new("state").min(16),
        Column::new("to").min(20),
        Column::new("subject").flexible().min(20),
    ]);
    for m in &messages {
        let state = match m.state.as_str() {
            "sent" => ctx.out.paint(&m.state, GREEN),
            "failed" | "pending_approval" => ctx.out.paint(&m.state, YELLOW),
            _ => m.state.clone(),
        };
        table.push(vec![
            m.id.to_string(),
            state,
            m.recipients.join(", "),
            m.subject.clone(),
        ]);
    }
    ctx.out.print(table.render(ctx.out));

    for m in messages.iter().filter(|m| m.last_error.is_some()) {
        ctx.out.print(format!(
            "  {} {} — {}",
            ctx.out.paint("!", YELLOW),
            m.id,
            m.last_error.clone().unwrap_or_default()
        ));
    }
    Ok(())
}

fn transition(ctx: &Ctx<'_>, id: i64, state: OutboxState, verb: &str) -> Result<()> {
    ctx.confirm(&format!("outbox {verb}"))?;
    let mut session = ctx.session()?;
    session.store_mut().set_outbox_state(id, state)?;
    let entry = session.store().outbox_entry(id)?;
    let item = mailbox_core::send::outbox_item(session.store(), &entry);

    if ctx.out.is_structured() {
        ctx.out.reply("outbox", &item)?;
    } else {
        ctx.out
            .print(format!("{} {id} {verb}", ctx.out.paint("✓", GREEN)));
    }
    Ok(())
}

fn flush(ctx: &Ctx<'_>) -> Result<()> {
    ctx.confirm("outbox flush")?;
    let mut session = ctx.session()?;
    let report = session.flush_outbox()?;

    if ctx.out.is_structured() {
        ctx.out.reply("outbox flush", &report)?;
    } else {
        ctx.out.print(format!(
            "{} sent, {} deferred, {} failed, {} filed",
            report.sent, report.deferred, report.failed, report.uploaded
        ));
    }
    Ok(())
}

fn report_send(ctx: &Ctx<'_>, command: &str, result: &SendResult) -> Result<()> {
    if ctx.out.is_structured() {
        ctx.out.reply(command, result)?;
        return Ok(());
    }

    if let Some(raw) = &result.raw {
        // §6.5: `--dry-run` renders the exact bytes that would be transmitted.
        ctx.out.print(raw);
        ctx.out.note(format!(
            "{} bytes to {} — nothing was sent",
            result.bytes,
            result.recipients.join(", ")
        ));
        return Ok(());
    }

    if result.duplicate {
        ctx.out.print(format!(
            "{} already queued as {} — the idempotency key matched",
            ctx.out.dim("·"),
            result.id
        ));
        return Ok(());
    }

    match result.status.as_str() {
        "pending_approval" => ctx.out.print(format!(
            "{} held for approval as {} — release it with `mailbox outbox approve {}`",
            ctx.out.paint("⏸", YELLOW),
            result.id,
            result.id
        )),
        "draft" => ctx.out.print(format!(
            "{} saved as draft {}",
            ctx.out.paint("✎", GREEN),
            result.id
        )),
        _ => {
            ctx.out.print(format!(
                "{} queued as {} for {}",
                ctx.out.paint("✓", GREEN),
                result.id,
                result.recipients.join(", ")
            ));
            match &result.send_after {
                Some(when) => ctx.out.note(format!(
                    "holding until {when} — `mailbox outbox cancel {}` to stop it",
                    result.id
                )),
                None => ctx
                    .out
                    .note("run `mailbox sync` or `mailbox outbox flush` to send it"),
            }
        }
    }
    Ok(())
}

// ---- input helpers ----

fn parse_addresses(list: &[String]) -> Result<Vec<Address>> {
    let mut out = Vec::new();
    for item in list {
        for part in split_address_list(item) {
            if let Some(a) = parse_address(&part)? {
                out.push(a);
            }
        }
    }
    Ok(out)
}

fn parse_one(value: Option<&str>) -> Result<Option<Address>> {
    match value {
        Some(v) => parse_address(v),
        None => Ok(None),
    }
}

/// Splits on commas that are not inside a quoted display name.
fn split_address_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ',' if !quoted => {
                out.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// `alice@example.com` or `Alice <alice@example.com>`.
fn parse_address(s: &str) -> Result<Option<Address>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let (name, addr) = match (s.rfind('<'), s.rfind('>')) {
        (Some(open), Some(close)) if close > open => (
            s[..open].trim().trim_matches('"').to_string(),
            s[open + 1..close].trim(),
        ),
        _ => (String::new(), s),
    };
    if !addr.contains('@') || addr.contains(' ') {
        return Err(Error::usage(format!("`{s}` is not an email address")));
    }
    Ok(Some(Address::new(
        Some(name).filter(|n| !n.is_empty()),
        addr.to_string(),
    )))
}

fn load_attachments(paths: &[std::path::PathBuf]) -> Result<Vec<Attachment>> {
    paths
        .iter()
        .map(|p| {
            let content = std::fs::read(p)
                .map_err(|e| Error::usage(format!("cannot attach {}: {e}", p.display())))?;
            let filename = p
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "attachment".into());
            Ok(Attachment {
                mime_type: guess_mime(&filename),
                filename,
                content,
                inline: false,
                content_id: None,
            })
        })
        .collect()
}

/// Extension-based content type, with a deliberately short table.
///
/// Guessing wrong here is harmless — every client sniffs anyway — and a long
/// table is a long table to maintain.
fn guess_mime(filename: &str) -> String {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn read_stdin_text() -> Result<String> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    Ok(s)
}

/// Splits an edited buffer into headers and body, the way every
/// `$EDITOR`-driven mail client has since `mail(1)`.
fn draft_from_edited(text: &str) -> Result<Draft> {
    let (head, body) = text.split_once("\n\n").unwrap_or((text, ""));
    let mut draft = Draft {
        body: body.to_string(),
        ..Default::default()
    };
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "to" => draft.to = parse_addresses(&[value.to_string()])?,
            "cc" => draft.cc = parse_addresses(&[value.to_string()])?,
            "bcc" => draft.bcc = parse_addresses(&[value.to_string()])?,
            "from" => draft.from = parse_address(value)?,
            "reply-to" => draft.reply_to = parse_addresses(&[value.to_string()])?,
            "subject" => draft.subject = value.to_string(),
            _ => {}
        }
    }
    Ok(draft)
}

/// Parses complete RFC5322 bytes into a draft, so `--stdin` goes through the
/// same policy and outbox path as everything else (§13).
fn draft_from_rfc5322(bytes: &[u8], args: &SendArgs) -> Result<Draft> {
    let parsed = mailbox_core::mime::parse(bytes);
    let mut draft = Draft {
        from: parsed
            .from
            .first()
            .cloned()
            .or(parse_one(args.from.as_deref())?),
        to: parsed.to.clone(),
        cc: parsed.cc.clone(),
        bcc: parsed.bcc.clone(),
        reply_to: parsed.reply_to.clone(),
        subject: parsed.subject.clone(),
        body: parsed.text.clone().unwrap_or_default(),
        html: parsed.html.clone(),
        in_reply_to: parsed.in_reply_to.clone().map(|m| format!("<{m}>")),
        references: parsed.references.iter().map(|r| format!("<{r}>")).collect(),
        message_id: parsed.message_id.clone().map(|m| format!("<{m}>")),
        date: parsed.date,
        ..Default::default()
    };
    // Recipients named on the command line are added to whatever the headers
    // said — that is how `sendmail -t` and its callers expect to behave.
    draft.to.extend(parse_addresses(&args.to)?);
    draft.cc.extend(parse_addresses(&args.cc)?);
    draft.bcc.extend(parse_addresses(&args.bcc)?);
    if draft.to.is_empty() && draft.cc.is_empty() && draft.bcc.is_empty() {
        return Err(Error::usage(
            "the message on stdin names no recipient; pass --to",
        ));
    }
    Ok(draft)
}

/// Opens `$EDITOR` on a temporary file and returns what came back.
fn edit_in_editor(configured: &str, template: &str) -> Result<String> {
    let editor = match configured {
        "$EDITOR" | "" => std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| "vi".to_string()),
        other => other.to_string(),
    };

    let path = std::env::temp_dir().join(format!("mailbox-compose-{}.eml", std::process::id()));
    std::fs::write(&path, template)?;

    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\"",))
        .arg("sh")
        .arg(&path)
        .status()
        .map_err(|e| Error::usage(format!("could not run editor `{editor}`: {e}")))?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(Error::usage(format!(
            "editor `{editor}` exited with {status}"
        )));
    }

    let text = std::fs::read_to_string(&path)?;
    // The draft is in the store from here on; leaving a copy in /tmp is a copy
    // of the user's mail nobody asked for.
    let _ = std::fs::remove_file(&path);
    Ok(text)
}
