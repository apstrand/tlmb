//! One function per command. Each parses arguments into core calls, then
//! renders — nothing here decides behaviour.

use mailbox_api::{BodyFormat, MessageList, ShowResult, ThreadList};
use mailbox_core::clock::Clock;
use mailbox_core::error::{Error, Result};
use mailbox_core::history::History;
use mailbox_core::imap::sync::SyncOptions;
use mailbox_core::render::RenderOptions;
use mailbox_core::session::{IdRef, Session, SessionOptions};
use mailbox_core::store::read::{ListOptions, Sort};
use mailbox_core::Tracer;

use crate::cli::{
    AccountsAction, AccountsArgs, DebugAction, DebugArgs, FixtureAction, Format, GlobalFlags,
    LsArgs, SchemaArgs, SearchArgs, ShowArgs, SyncArgs,
};
use crate::format;
use crate::output::{maybe_page, Out, GREEN, YELLOW};

pub struct Ctx<'a> {
    pub out: &'a Out,
    pub global: &'a GlobalFlags,
    pub opts: SessionOptions,
    pub tracer: Tracer,
    pub clock: Clock,
    pub agent_mode: bool,
    pub interactive: bool,
}

impl Ctx<'_> {
    fn session(&self) -> Result<Session> {
        Session::open(&self.opts, self.tracer.clone(), self.clock.clone())
    }

    fn list_options(&self, sort: Sort, with_total: bool) -> ListOptions {
        ListOptions {
            limit: self.global.limit.unwrap_or(50),
            cursor: self.global.cursor.clone(),
            sort,
            identities: Vec::new(),
            with_total,
        }
    }

    /// §6.4: read-only by default in agent mode — any state-changing command
    /// needs an explicit `--yes`. And §6.4 again: a command that would have
    /// prompted must not, when stdin is not a TTY; it fails with
    /// `needs_confirmation` and names the flag that supplies the answer.
    fn confirm(&self, what: &str) -> Result<()> {
        if self.global.yes {
            return Ok(());
        }
        if self.agent_mode {
            return Err(Error::usage(format!(
                "agent mode makes `{what}` require an explicit --yes"
            ))
            .needs_confirmation("pass --yes"));
        }
        if !self.interactive {
            return Err(Error::usage(format!(
                "`{what}` changes state and stdin is not a terminal"
            ))
            .needs_confirmation("pass --yes"));
        }
        Ok(())
    }

    /// §6.4: an agent that uses `^` or a listing index is relying on state from
    /// a previous invocation that it cannot see. That is a warning rather than
    /// an error — it works, and a human's muscle memory should not break when
    /// `MAILBOX_AGENT` happens to be set in the environment.
    fn warn_if_positional(&self, id_ref: &IdRef) {
        if !self.agent_mode {
            return;
        }
        if matches!(id_ref, IdRef::Previous | IdRef::Index(_)) {
            self.out.warn(
                "agent mode: `^` and listing indexes depend on a previous command's output; \
                 use the `id` from --json instead",
            );
        }
    }
}

pub fn ls(ctx: &Ctx<'_>, args: &LsArgs) -> Result<()> {
    let session = ctx.session()?;
    let sort = if args.reverse {
        Sort::DateAsc
    } else if args.by_date {
        Sort::DateDesc
    } else {
        Sort::UnreadFirst
    };
    let list = session.ls(args.query.as_deref(), ctx.list_options(sort, args.total))?;

    if args.threads {
        return render_threads(ctx, &session, &list);
    }
    render_messages(ctx, &session, "ls", &list, args.full_ids)
}

pub fn search(ctx: &Ctx<'_>, args: &SearchArgs) -> Result<()> {
    let session = ctx.session()?;
    let query = args.query.join(" ");
    // Search orders by date: relevance is not a thing we can honestly rank
    // across a structured-plus-full-text query, and unread-first would bury the
    // match the user was looking for.
    let list = session.search(&query, ctx.list_options(Sort::DateDesc, args.total))?;
    render_messages(ctx, &session, "search", &list, args.full_ids)
}

fn render_messages(
    ctx: &Ctx<'_>,
    session: &Session,
    command: &str,
    list: &MessageList,
    full_ids: bool,
) -> Result<()> {
    // Record what was printed, so `^` and `1..20` mean something next time
    // (§5.2). Done for structured output too: a script that pipes `--json` into
    // a human's terminal should not break the human's shorthand.
    let ids: Vec<String> = list.messages.iter().map(|m| m.id.clone()).collect();
    History::save(&session.account_id, &ids);

    match ctx.out.mode {
        crate::output::Mode::Json => ctx.out.reply(command, list)?,
        crate::output::Mode::Ndjson => {
            ctx.out
                .ndjson_list(command, &list.messages, list.cursor.as_deref())?
        }
        crate::output::Mode::Human => {
            if list.messages.is_empty() {
                ctx.out.print(ctx.out.dim("no messages"));
                return Ok(());
            }
            maybe_page(
                ctx.out,
                &format::message_table(ctx.out, &list.messages, full_ids),
            );
            if let Some(total) = list.total {
                ctx.out
                    .note(format!("{} of {total} shown", list.messages.len()));
            }
            if list.cursor.is_some() {
                ctx.out.note("more results: pass --cursor to continue");
            }
        }
    }
    Ok(())
}

fn render_threads(ctx: &Ctx<'_>, session: &Session, list: &MessageList) -> Result<()> {
    // Group the page's messages into their threads, preserving the page's
    // order. Deliberately *not* a second query: the page is the unit the cursor
    // paginates, and expanding it server-side would make the cursor lie.
    let mut seen: Vec<String> = Vec::new();
    let mut threads = Vec::new();
    for m in &list.messages {
        if seen.contains(&m.thread_id) {
            continue;
        }
        seen.push(m.thread_id.clone());
        if let Ok(t) = session.store().thread(&m.thread_id) {
            threads.push(t);
        }
    }

    let payload = ThreadList {
        threads,
        cursor: list.cursor.clone(),
        total: list.total,
    };
    match ctx.out.mode {
        crate::output::Mode::Json => ctx.out.reply("ls", &payload)?,
        crate::output::Mode::Ndjson => {
            ctx.out
                .ndjson_list("ls", &payload.threads, payload.cursor.as_deref())?
        }
        crate::output::Mode::Human => {
            if payload.threads.is_empty() {
                ctx.out.print(ctx.out.dim("no messages"));
                return Ok(());
            }
            maybe_page(ctx.out, &format::thread_summary(ctx.out, &payload.threads));
        }
    }
    Ok(())
}

pub fn show(ctx: &Ctx<'_>, args: &ShowArgs) -> Result<()> {
    let session = ctx.session()?;
    let id_ref = IdRef::parse(&args.id);
    ctx.warn_if_positional(&id_ref);

    // `--raw` bypasses rendering entirely: §13 promises the exact RFC5322
    // bytes, and anything that decoded and re-encoded them would not be exact.
    if args.raw || args.format == Format::Raw {
        let bytes = session.raw(&id_ref)?;
        use std::io::Write;
        std::io::stdout().write_all(&bytes)?;
        return Ok(());
    }

    let render = RenderOptions {
        format: match args.format {
            Format::Text => BodyFormat::Text,
            Format::Md => BodyFormat::Md,
            Format::Html => BodyFormat::Html,
            Format::Raw => BodyFormat::Raw,
        },
        text_only: args.text_only,
        strip_quotes: args.strip_quotes,
        max_body_bytes: args.max_body_bytes,
    };

    let mut result = session.show(&id_ref, &render, args.thread)?;
    if args.no_attachments {
        if let Some(m) = &mut result.message {
            m.attachments.clear();
        }
    }
    if args.headers_only {
        if let Some(m) = &mut result.message {
            m.body = None;
        }
    }

    // The message just shown becomes `^`.
    if let Some(m) = &result.message {
        History::save(&session.account_id, std::slice::from_ref(&m.summary.id));
    }

    match ctx.out.mode {
        crate::output::Mode::Json | crate::output::Mode::Ndjson => {
            ctx.out.reply("show", &result)?
        }
        crate::output::Mode::Human => render_show_human(ctx, &result, args),
    }
    Ok(())
}

fn render_show_human(ctx: &Ctx<'_>, result: &ShowResult, args: &ShowArgs) {
    if let Some(t) = &result.thread {
        maybe_page(ctx.out, &format::thread_detail(ctx.out, t));
        return;
    }
    if let Some(m) = &result.message {
        maybe_page(
            ctx.out,
            &format::message_detail(ctx.out, m, !args.no_attachments),
        );
    }
}

pub fn folders(ctx: &Ctx<'_>) -> Result<()> {
    let session = ctx.session()?;
    let list = session.folders()?;

    match ctx.out.mode {
        crate::output::Mode::Json => ctx.out.reply("folders", &list)?,
        crate::output::Mode::Ndjson => ctx.out.ndjson_list("folders", &list.folders, None)?,
        crate::output::Mode::Human => {
            use crate::table::{Column, Table};
            let mut table = Table::new(vec![
                Column::new("folder").flexible().min(20),
                Column::new("use").min(7),
                Column::new("unread").right().min(6),
                Column::new("total").right().min(6),
            ]);
            for f in &list.folders {
                table.push(vec![
                    f.path.clone(),
                    ctx.out.dim(f.special_use.as_deref().unwrap_or("")),
                    if f.unread > 0 {
                        ctx.out.paint(&f.unread.to_string(), GREEN)
                    } else {
                        ctx.out.dim("0")
                    },
                    f.total.to_string(),
                ]);
            }
            if table.is_empty() {
                ctx.out
                    .print(ctx.out.dim("no folders yet — run `mailbox sync`"));
            } else {
                maybe_page(ctx.out, &table.render(ctx.out));
            }
        }
    }
    Ok(())
}

pub fn accounts(ctx: &Ctx<'_>, args: &AccountsArgs) -> Result<()> {
    match &args.action {
        Some(AccountsAction::Test { account }) => accounts_test(ctx, account.as_deref()),
        _ => accounts_list(ctx),
    }
}

fn accounts_list(ctx: &Ctx<'_>) -> Result<()> {
    let session = ctx.session()?;
    let list = session.accounts()?;
    match ctx.out.mode {
        crate::output::Mode::Json => ctx.out.reply("accounts", &list)?,
        crate::output::Mode::Ndjson => ctx.out.ndjson_list("accounts", &list.accounts, None)?,
        crate::output::Mode::Human => {
            use crate::table::{Column, Table};
            let mut table = Table::new(vec![
                Column::new("account").min(10),
                Column::new("address").flexible().min(20),
                Column::new("server").min(20),
                Column::new("credential").min(10),
            ]);
            for a in &list.accounts {
                table.push(vec![
                    a.id.clone(),
                    a.address.clone(),
                    format!("{}:{}", a.imap_host, a.imap_port),
                    if a.has_credential {
                        ctx.out.paint("ok", GREEN)
                    } else {
                        ctx.out.paint("missing", YELLOW)
                    },
                ]);
            }
            if table.is_empty() {
                ctx.out.print(ctx.out.dim("no accounts configured"));
            } else {
                ctx.out.print(table.render(ctx.out));
            }
        }
    }
    Ok(())
}

/// Connects, authenticates, and reports what the server offers.
///
/// It says which of §5.3's fast paths will be available, because "why is my
/// sync slow" almost always answers to "this server has no CONDSTORE".
fn accounts_test(ctx: &Ctx<'_>, account: Option<&str>) -> Result<()> {
    use mailbox_core::imap::{Client, ConnectionConfig};

    let mut opts = ctx.opts.clone();
    if let Some(a) = account {
        opts.account = Some(a.to_string());
    }
    let session = Session::open(&opts, ctx.tracer.clone(), ctx.clock.clone())?;
    let cfg = session.account_config()?.clone();
    let host = cfg
        .imap_host
        .clone()
        .ok_or_else(|| Error::usage("this account has no imap_host configured"))?;
    let credential = mailbox_core::credentials::resolve(&session.account_id, &cfg)?;

    let conn_cfg = ConnectionConfig {
        host: host.clone(),
        port: cfg.port(),
        security: cfg.security(),
        require_tls: cfg.require_tls,
        timeout: std::time::Duration::from_millis(ctx.global.timeout.unwrap_or(30_000)),
    };
    let mut client = Client::connect(&conn_cfg, ctx.tracer.clone())?;
    client.login(cfg.login_user(), &credential)?;
    let folders = client.list_folders()?;
    let caps: Vec<String> = client.capabilities().iter().cloned().collect();
    let qresync = client.qresync();
    let _ = client.logout();

    let payload = serde_json::json!({
        "account": session.account_id,
        "address": cfg.address,
        "server": format!("{host}:{}", cfg.port()),
        "authenticated": true,
        "capabilities": caps,
        "folders": folders.len(),
        "sync_path": if qresync {
            "qresync"
        } else if caps.iter().any(|c| c == "CONDSTORE") {
            "condstore"
        } else {
            "full-listing"
        },
    });

    if ctx.out.is_structured() {
        ctx.out.reply("accounts test", &payload)?;
    } else {
        ctx.out.print(format!(
            "{} {} at {}\n  {} folders\n  sync path: {}\n  capabilities: {}",
            ctx.out.paint("ok", GREEN),
            cfg.address,
            conn_cfg.host,
            folders.len(),
            payload["sync_path"].as_str().unwrap_or(""),
            caps.join(" "),
        ));
    }
    Ok(())
}

pub fn sync(ctx: &Ctx<'_>, args: &SyncArgs) -> Result<()> {
    // Syncing writes to the store and talks to the network, so agent mode
    // gates it (§6.4).
    ctx.confirm("sync")?;

    let mut session = ctx.session()?;
    let cfg = session.account_config()?.clone();

    let since = match &args.since {
        Some(s) => Some(
            mailbox_core::query::parse::parse_date(s, &ctx.clock)
                .ok_or_else(|| Error::usage(format!("`--since {s}` is not a date")))?,
        ),
        None => None,
    };

    if ctx.global.dry_run {
        // §6.5: `--dry-run` exits 0 without touching the network.
        let payload = serde_json::json!({
            "account": session.account_id,
            "would_sync": true,
            "server": format!("{}:{}", cfg.imap_host.clone().unwrap_or_default(), cfg.port()),
            "full": args.full,
            "folders": args.folder,
            "since": since,
        });
        if ctx.out.is_structured() {
            ctx.out.reply("sync", &payload)?;
        } else {
            ctx.out.print(format!(
                "would sync {} (no network touched)",
                session.account_id
            ));
        }
        return Ok(());
    }

    let opts = SyncOptions {
        full: args.full,
        folders: args.folder.clone(),
        since,
        prefetch_days: if args.full { None } else { cfg.prefetch_days() },
        max_prefetch: 500,
    };

    let result = session.sync(&opts)?;

    if ctx.out.is_structured() {
        ctx.out.reply("sync", &result)?;
    } else {
        for r in &result.synced {
            ctx.out.print(format!(
                "{}: {} folders, +{} messages, ~{} updated, -{} removed, {} bodies in {} ms",
                r.account,
                r.folders_examined,
                r.messages_added,
                r.messages_updated,
                r.messages_removed,
                r.bodies_fetched,
                r.duration_ms
            ));
            for folder in &r.uidvalidity_resets {
                ctx.out
                    .warn(format!("{folder}: UIDVALIDITY changed, folder rescanned"));
            }
        }
    }
    Ok(())
}

pub fn status(ctx: &Ctx<'_>) -> Result<()> {
    let session = ctx.session()?;
    let status = session.status()?;
    if ctx.out.is_structured() {
        ctx.out.reply("status", &status)?;
        return Ok(());
    }
    for a in &status.accounts {
        ctx.out.print(format!(
            "{} ({})\n  {} messages, {} unread, {} folders\n  last sync: {}\n  store: {}",
            ctx.out.bold(&a.account),
            a.address,
            a.messages,
            a.unread,
            a.folders,
            a.last_sync.clone().unwrap_or_else(|| "never".into()),
            format::human_size(status.store_bytes),
        ));
    }
    Ok(())
}

/// `mailbox schema <command>` (§6.4): the JSON Schema for a command's output.
pub fn schema(ctx: &Ctx<'_>, args: &SchemaArgs) -> Result<()> {
    use mailbox_api::*;

    let Some(command) = &args.command else {
        let commands = [
            "ls", "search", "show", "folders", "accounts", "sync", "status",
        ];
        if ctx.out.is_structured() {
            ctx.out
                .reply("schema", &serde_json::json!({ "commands": commands }))?;
        } else {
            for c in commands {
                ctx.out.print(c);
            }
        }
        return Ok(());
    };

    let schema = match command.as_str() {
        "ls" | "search" => schemars::schema_for!(Reply<MessageList>),
        "show" => schemars::schema_for!(Reply<ShowResult>),
        "folders" => schemars::schema_for!(Reply<FolderList>),
        "accounts" => schemars::schema_for!(Reply<AccountList>),
        "sync" => schemars::schema_for!(Reply<SyncResult>),
        "status" => schemars::schema_for!(Reply<Status>),
        "error" => schemars::schema_for!(ErrorResponse),
        other => {
            return Err(Error::not_found("schema for command", other));
        }
    };
    println!("{}", serde_json::to_string_pretty(&schema)?);
    Ok(())
}

pub fn debug(ctx: &Ctx<'_>, args: &DebugArgs) -> Result<()> {
    match &args.action {
        DebugAction::StoreDump { query } => {
            let session = ctx.session()?;
            let dump = session.store_dump(query.as_deref())?;
            // Always JSON: this is a golden-file oracle (§14.3), and a
            // human-readable variant would just be a second thing to keep
            // stable.
            println!("{}", serde_json::to_string_pretty(&dump)?);
            Ok(())
        }

        DebugAction::Clock => {
            let now = ctx.clock.now();
            let source = if std::env::var("MAILBOX_CLOCK").is_ok() {
                "MAILBOX_CLOCK"
            } else {
                "system"
            };
            let payload = serde_json::json!({
                "now": now,
                "rfc3339": mailbox_core::clock::format_rfc3339(now),
                "source": source,
                "seed": std::env::var("MAILBOX_SEED").ok(),
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
            Ok(())
        }

        DebugAction::Query { query } => {
            let node = mailbox_core::query::parse(query, &ctx.clock)?;
            // The AST as text: the query-corpus tests (§14.2, L0) assert on
            // this, and Debug is a stable enough rendering for that as long as
            // the enum is.
            println!("{node:#?}");
            Ok(())
        }

        DebugAction::Rethread => {
            let mut session = ctx.session()?;
            let n = session.store_mut().rebuild_threads(None)?;
            let payload = serde_json::json!({ "rethreaded": n });
            if ctx.out.is_structured() {
                ctx.out.reply("debug rethread", &payload)?;
            } else {
                ctx.out.print(format!("rethreaded {n} messages"));
            }
            Ok(())
        }

        DebugAction::Fixture { action } => {
            if matches!(action, FixtureAction::Load { .. }) {
                ctx.confirm("debug fixture load")?;
            }
            fixture(ctx, action)
        }
    }
}

fn fixture(ctx: &Ctx<'_>, action: &FixtureAction) -> Result<()> {
    use mailbox_core::fixture::Fixture;

    match action {
        FixtureAction::Load { path } => {
            let fixture = Fixture::load(path)?;
            let mut session = ctx.session()?;
            let ids = fixture.seed_store(session.store_mut())?;
            let payload = serde_json::json!({ "loaded": ids.len(), "ids": ids });
            if ctx.out.is_structured() {
                ctx.out.reply("debug fixture load", &payload)?;
            } else {
                ctx.out.print(format!("loaded {} messages", ids.len()));
                for (name, id) in &ids {
                    ctx.out
                        .print(format!("  {} {}", ctx.out.dim(&id[..7]), name));
                }
            }
            Ok(())
        }

        FixtureAction::Render { path, name } => {
            use std::io::Write;
            let fixture = Fixture::load(path)?;
            let mut stdout = std::io::stdout().lock();
            match name {
                Some(n) => {
                    let rendered = fixture.render(fixture.get(n)?)?;
                    stdout.write_all(&rendered.bytes)?;
                }
                None => {
                    // NDJSON, so the L2 seeder can stream it: one object per
                    // message carrying the folder, flags, and bytes it needs
                    // for an APPEND.
                    use base64::Engine;
                    for r in fixture.render_all()? {
                        let line = serde_json::json!({
                            "name": r.name,
                            "folder": r.folder,
                            "flags": r.flags,
                            "internaldate": r.internaldate,
                            "message_id": r.message_id,
                            "size": r.size(),
                            "bytes_base64": base64::engine::general_purpose::STANDARD.encode(&r.bytes),
                        });
                        writeln!(stdout, "{line}")?;
                    }
                }
            }
            Ok(())
        }
    }
}
