//! One function per command. Each parses arguments into core calls, then
//! renders — nothing here decides behaviour.

use mailbox_api::{BodyFormat, MessageList, ShowResult, ThreadList};
use mailbox_core::clock::Clock;
use mailbox_core::error::{Error, Result};
use mailbox_core::history::History;
use mailbox_core::imap::sync::SyncOptions;
use mailbox_core::render::RenderOptions;
use mailbox_core::session::{FlagSpec, IdRef, Session, SessionOptions};
use mailbox_core::store::read::{ListOptions, Sort};
use mailbox_core::store::Written;
use mailbox_core::Tracer;
use mailbox_core::Workspace;

use crate::cli::{
    AccountsAction, AccountsArgs, AttachArgs, DebugAction, DebugArgs, FixtureAction, FlagArgs,
    FoldersArgs, Format, GlobalFlags, LsArgs, MoveArgs, NotifyArgs, SchemaArgs, SearchArgs,
    ShowArgs, SyncArgs, TargetArgs,
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
}

impl Ctx<'_> {
    pub(crate) fn session(&self) -> Result<Session> {
        Session::open(&self.opts, self.tracer.clone(), self.clock.clone())
    }

    /// Every configured account as one mailbox (§1.5), or just the one
    /// `--account` named.
    pub(crate) fn workspace(&self) -> Result<Workspace> {
        Workspace::open(&self.opts, self.tracer.clone(), self.clock.clone())
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

    /// §6.4: read-only by default in agent mode — `MAILBOX_AGENT=1` makes any
    /// state-changing command require an explicit `--yes`.
    ///
    /// Note what this deliberately does *not* do: gate on whether stdin is a
    /// terminal. §6.4's other rule — "no interactive prompts when stdin is not
    /// a TTY" — is about commands that *would have prompted*, and none of M1's
    /// do. Treating a non-TTY as a missing confirmation would break
    /// `mailbox sync` from cron, from a systemd timer, and from every script,
    /// which is most of the ways a sync is meant to be run.
    /// The `[notify]` section, read the same way `mailbox notify` reads it.
    pub(crate) fn notify_config(&self) -> Result<mailbox_core::config::Notify> {
        Ok(match &self.global.config {
            Some(path) => mailbox_core::config::Config::load(path)?,
            None => mailbox_core::config::Config::load_default()?,
        }
        .notify)
    }

    pub(crate) fn confirm(&self, what: &str) -> Result<()> {
        if self.global.yes || !self.agent_mode {
            return Ok(());
        }
        Err(Error::usage(format!(
            "agent mode makes `{what}` require an explicit --yes"
        ))
        .needs_confirmation("pass --yes"))
    }

    /// §6.4: an agent that uses `^` or a listing index is relying on state from
    /// a previous invocation that it cannot see. That is a warning rather than
    /// an error — it works, and a human's muscle memory should not break when
    /// `MAILBOX_AGENT` happens to be set in the environment.
    pub(crate) fn warn_if_positional(&self, id_ref: &IdRef) {
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
    let workspace = ctx.workspace()?;
    let sort = if args.reverse {
        Sort::DateAsc
    } else if args.by_date {
        Sort::DateDesc
    } else {
        Sort::UnreadFirst
    };
    let list = workspace.ls(args.query.as_deref(), &ctx.list_options(sort, args.total))?;

    if args.threads {
        return render_threads(ctx, &workspace, &list);
    }
    render_messages(ctx, &workspace, "ls", &list, args.full_ids)
}

pub fn search(ctx: &Ctx<'_>, args: &SearchArgs) -> Result<()> {
    let workspace = ctx.workspace()?;
    let query = args.query.join(" ");
    // Search orders by date: relevance is not a thing we can honestly rank
    // across a structured-plus-full-text query, and unread-first would bury the
    // match the user was looking for.
    let list = workspace.search(&query, &ctx.list_options(Sort::DateDesc, args.total))?;
    render_messages(ctx, &workspace, "search", &list, args.full_ids)
}

fn render_messages(
    ctx: &Ctx<'_>,
    workspace: &Workspace,
    command: &str,
    list: &MessageList,
    full_ids: bool,
) -> Result<()> {
    // Record what was printed, so `^` and `1..20` mean something next time
    // (§5.2). Done for structured output too: a script that pipes `--json` into
    // a human's terminal should not break the human's shorthand.
    let ids: Vec<String> = list.messages.iter().map(|m| m.id.clone()).collect();
    History::save(&workspace.history_scope(), &ids);

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

fn render_threads(ctx: &Ctx<'_>, workspace: &Workspace, list: &MessageList) -> Result<()> {
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
        // A thread belongs to the account that holds its messages, so it is
        // fetched from the store that owns this row rather than from a
        // notional shared one.
        if let Ok((i, _)) = workspace.locate(&IdRef::Id(m.id.clone())) {
            if let Ok(t) = workspace.sessions()[i].store().thread(&m.thread_id) {
                threads.push(t);
            }
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

/// `attach` (§6.1): list a message's parts, or write one to disk.
pub fn attach(ctx: &Ctx<'_>, args: &AttachArgs) -> Result<()> {
    let id_ref = IdRef::parse(&args.id);
    ctx.warn_if_positional(&id_ref);

    let (account, id) = {
        let workspace = ctx.workspace()?;
        let (index, id) = workspace.locate(&id_ref)?;
        (workspace.sessions()[index].account_id.clone(), id)
    };

    let mut opts = ctx.opts.clone();
    opts.account = Some(account);
    opts.read_only = args.offline;
    let mut session = Session::open(&opts, ctx.tracer.clone(), ctx.clock.clone())?;

    if !args.offline {
        session.ensure_body(&id)?;
    }
    let detail = session.store().message(&id, &Default::default())?;

    // Listing is the default: `attach <id>` should say what is there before
    // anything is written anywhere.
    if args.part.is_none() && !args.all {
        return list_attachments(ctx, &detail);
    }

    // An explicit part number is a direct request: it is extracted whether or
    // not it appears in the attachment list, because the list is for discovery
    // and a body part of a single-part message is legitimately part 1.
    // `--all` is the other way round — it means "the attachments", which is
    // exactly what the list is.
    let wanted: Vec<String> = match &args.part {
        Some(part) => vec![part.clone()],
        None => detail
            .attachments
            .iter()
            .filter(|a| a.is_attachment)
            .map(|a| a.part.clone())
            .collect(),
    };
    if wanted.is_empty() {
        return Err(Error::not_found("part", "any attachment"));
    }

    let mut written = Vec::new();
    for part in &wanted {
        let (extracted_name, mime_type, bytes) = session.attachment(&id, part)?;
        // The listed filename where there is one — it is what the sender
        // called the file — falling back to what the part itself carries.
        let name = detail
            .attachments
            .iter()
            .find(|a| &a.part == part)
            .and_then(|a| a.filename.clone())
            .unwrap_or(extracted_name);

        if !args.extract {
            // Without `--extract` a single named part goes to stdout, so
            // `mailbox attach 1 2 > invoice.pdf` works like every other
            // Unix tool.
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?;
            return Ok(());
        }

        // §9: a filename from a message is attacker-controlled. `destination`
        // strips paths, mangles extensions a desktop would run, and refuses to
        // overwrite; `write` clears the executable bit.
        let target = if args.out.is_dir() {
            mailbox_core::attach::destination(&args.out, &name)
        } else if wanted.len() == 1 {
            args.out.clone()
        } else {
            mailbox_core::attach::destination(&args.out, &name)
        };
        mailbox_core::attach::write(&target, &bytes)?;
        written.push(serde_json::json!({
            "part": part,
            "filename": name,
            "mime_type": mime_type,
            "bytes": bytes.len(),
            "path": target.display().to_string(),
        }));
    }

    if ctx.out.is_structured() {
        ctx.out
            .reply("attach", &serde_json::json!({ "extracted": written }))?;
    } else {
        for w in &written {
            ctx.out.print(format!(
                "{} {} ({})",
                ctx.out.paint("✓", GREEN),
                w["path"].as_str().unwrap_or_default(),
                format::human_size(w["bytes"].as_u64().unwrap_or(0)),
            ));
        }
    }
    Ok(())
}

fn list_attachments(ctx: &Ctx<'_>, detail: &mailbox_api::MessageDetail) -> Result<()> {
    if ctx.out.is_structured() {
        ctx.out.reply(
            "attach",
            &serde_json::json!({
                "id": detail.summary.id,
                "attachments": detail.attachments,
            }),
        )?;
        return Ok(());
    }

    if detail.attachments.is_empty() {
        ctx.out.print(ctx.out.dim("no attachments"));
        return Ok(());
    }
    use crate::table::{Column, Table};
    let mut table = Table::new(vec![
        Column::new("part").min(4),
        Column::new("filename").flexible().min(20),
        Column::new("type").min(20),
        Column::new("size").right().min(7),
    ]);
    for a in &detail.attachments {
        table.push(vec![
            a.part.clone(),
            a.filename
                .clone()
                .unwrap_or_else(|| ctx.out.dim("(inline)")),
            a.mime_type.clone(),
            format::human_size(a.size),
        ]);
    }
    ctx.out.print(table.render(ctx.out));
    Ok(())
}

/// `flag` (§6.1): `mailbox flag 1 2 +seen -flagged +tag:invoices`.
///
/// Ids and changes may be interleaved, because the sign already says which is
/// which and requiring an order would be a rule with no reason behind it.
pub fn flag(ctx: &Ctx<'_>, args: &FlagArgs) -> Result<()> {
    // `+seen`/`-flagged` force clap to stop parsing options for this
    // command's arguments, which means a global flag written *after* the
    // subcommand arrives here as text. Silently treating `--json` as a flag
    // named `-json` is the worst possible outcome — the caller's output format
    // is ignored and a nonsense flag is queued — so it is called out instead.
    if let Some(misplaced) = args.args.iter().find(|a| a.starts_with("--")) {
        return Err(Error::usage(format!(
            "`{misplaced}` has to come before the subcommand: \
             `mailbox {misplaced} flag …`. `flag` takes `+name` and `-name`, \
             so it cannot tell an option from a flag once it has started."
        )));
    }

    let (ids, changes): (Vec<String>, Vec<String>) = args
        .args
        .iter()
        .cloned()
        .partition(|a| !a.starts_with('+') && !a.starts_with('-'));

    if ids.is_empty() {
        return Err(Error::usage("no message given"));
    }
    let spec = FlagSpec::parse(&changes)?;

    ctx.confirm("flag")?;
    let refs: Vec<IdRef> = ids.iter().map(|s| IdRef::parse(s)).collect();
    for r in &refs {
        ctx.warn_if_positional(r);
    }

    let mut workspace = ctx.workspace()?;
    if ctx.global.dry_run {
        return report_dry_run(ctx, "flag", &workspace, &refs, None, Some(&spec));
    }
    let written = workspace.write_each(&refs, |session, ids| session.flag(ids, &spec))?;
    report_written(ctx, "flag", &written)
}

/// `mv` and `cp` (§6.1). The last argument is the destination.
pub fn mv(ctx: &Ctx<'_>, args: &MoveArgs, copy: bool) -> Result<()> {
    let (target, ids) = args
        .args
        .split_last()
        .map(|(t, ids)| (t.clone(), ids.to_vec()))
        .ok_or_else(|| Error::usage("give one or more messages and a destination folder"))?;
    if ids.is_empty() {
        return Err(Error::usage("no message given"));
    }

    let command = if copy { "cp" } else { "mv" };
    ctx.confirm(command)?;
    let refs: Vec<IdRef> = ids.iter().map(|s| IdRef::parse(s)).collect();
    for r in &refs {
        ctx.warn_if_positional(r);
    }

    let mut workspace = ctx.workspace()?;
    if ctx.global.dry_run {
        return report_dry_run(ctx, command, &workspace, &refs, Some(&target), None);
    }
    let written = workspace.write_each(&refs, |session, ids| {
        if copy {
            session.copy_to(ids, &target)
        } else {
            session.move_to(ids, &target)
        }
    })?;
    report_written(ctx, command, &written)
}

/// `rm`, `archive`, and `spam`: moves to whatever this server calls Trash,
/// Archive, and Junk (§6.1).
pub fn move_to_special(ctx: &Ctx<'_>, args: &TargetArgs, use_name: &str) -> Result<()> {
    let command = match use_name {
        "trash" => "rm",
        "junk" => "spam",
        other => other,
    };
    ctx.confirm(command)?;

    let refs: Vec<IdRef> = args.ids.iter().map(|s| IdRef::parse(s)).collect();
    for r in &refs {
        ctx.warn_if_positional(r);
    }

    let mut workspace = ctx.workspace()?;
    if ctx.global.dry_run {
        let target = workspace.primary()?.special_folder(use_name)?;
        return report_dry_run(ctx, command, &workspace, &refs, Some(&target), None);
    }
    // Resolved per account: `archive` is a different folder path on different
    // servers, and in a unified mailbox the messages may span both.
    let written = workspace.write_each(&refs, |session, ids| {
        let target = session.special_folder(use_name)?;
        session.move_to(ids, &target)
    })?;
    report_written(ctx, command, &written)
}

/// The result of a write, in both audiences' terms.
fn report_written(ctx: &Ctx<'_>, command: &str, written: &[Written]) -> Result<()> {
    let payload = serde_json::json!({
        "changed": written.iter().filter(|w| w.changed).count(),
        "messages": written
            .iter()
            .map(|w| serde_json::json!({
                "id": w.message_id,
                "folder": w.folder,
                "flags": w.flags,
                "changed": w.changed,
            }))
            .collect::<Vec<_>>(),
    });

    if ctx.out.is_structured() {
        ctx.out.reply(command, &payload)?;
        return Ok(());
    }
    let changed = written.iter().filter(|w| w.changed).count();
    if changed == 0 {
        ctx.out.print(ctx.out.dim("nothing changed"));
        return Ok(());
    }
    for w in written.iter().filter(|w| w.changed) {
        ctx.out.print(format!(
            "{} {} {}",
            ctx.out.paint("✓", GREEN),
            ctx.out.dim(format::short(&w.message_id)),
            w.folder
        ));
    }
    // The queue is the whole point of §5.3's optimistic write: the change is
    // already visible, and the server hears about it on the next sync.
    ctx.out
        .note("queued for the server — run `mailbox sync` to push it");
    Ok(())
}

/// §6.5: `--dry-run` shows exactly what would happen and touches nothing.
fn report_dry_run(
    ctx: &Ctx<'_>,
    command: &str,
    workspace: &Workspace,
    refs: &[IdRef],
    target: Option<&str>,
    spec: Option<&FlagSpec>,
) -> Result<()> {
    let mut messages = Vec::new();
    for r in refs {
        let (i, id) = workspace.locate(r)?;
        let summary = workspace.sessions()[i].store().summary(&id)?;
        messages.push(serde_json::json!({
            "id": id,
            "account": summary.account,
            "subject": summary.subject,
            "folder": summary.folder,
            "flags": summary.flags,
        }));
    }
    let payload = serde_json::json!({
        "dry_run": true,
        "would": command,
        "target": target,
        "add_flags": spec.map(|s| s.add_flags.clone()),
        "remove_flags": spec.map(|s| s.remove_flags.clone()),
        "add_tags": spec.map(|s| s.add_tags.clone()),
        "remove_tags": spec.map(|s| s.remove_tags.clone()),
        "messages": messages,
    });

    if ctx.out.is_structured() {
        ctx.out.reply(command, &payload)?;
    } else {
        for m in &messages {
            ctx.out.print(format!(
                "would {command} {} {}",
                ctx.out.dim(&m["id"].as_str().unwrap_or_default()[..7]),
                target.unwrap_or(""),
            ));
        }
    }
    Ok(())
}

pub fn show(ctx: &Ctx<'_>, args: &ShowArgs) -> Result<()> {
    let workspace = ctx.workspace()?;
    let id_ref = IdRef::parse(&args.id);
    ctx.warn_if_positional(&id_ref);

    // §5.1: "bodies for what you read". Opening a message older than the
    // prefetch window is the moment its body arrives — unless the caller said
    // not to go looking, or there is no network, in which case the headers are
    // still worth showing.
    if !args.offline {
        if let Ok((index, id)) = workspace.locate(&id_ref) {
            let account = workspace.sessions()[index].account_id.clone();
            let mut opts = ctx.opts.clone();
            opts.account = Some(account);
            opts.read_only = false;
            if let Ok(mut session) = Session::open(&opts, ctx.tracer.clone(), ctx.clock.clone()) {
                if let Err(e) = session.ensure_body(&id) {
                    ctx.out.note(format!("could not fetch the body: {e}"));
                }
            }
        }
    }

    // `--raw` bypasses rendering entirely: §13 promises the exact RFC5322
    // bytes, and anything that decoded and re-encoded them would not be exact.
    if args.raw || args.format == Format::Raw {
        let bytes = workspace.raw(&id_ref)?;
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

    let mut result = workspace.show(&id_ref, &render, args.thread)?;
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
        History::save(
            &workspace.history_scope(),
            std::slice::from_ref(&m.summary.id),
        );
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
    if args.links {
        let links = result
            .message
            .as_ref()
            .and_then(|m| m.body.as_ref())
            .map(|b| b.links.as_slice())
            .unwrap_or_default();
        if links.is_empty() {
            ctx.out.print(ctx.out.dim("no links in this message"));
            return;
        }
        for (i, url) in links.iter().enumerate() {
            ctx.out
                .print(format!("{} {}", ctx.out.dim(&format!("[{}]", i + 1)), url));
        }
        return;
    }
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

pub fn folders(ctx: &Ctx<'_>, args: &FoldersArgs) -> Result<()> {
    let session = ctx.workspace()?;
    // `--remote` asks the server. It reads nothing from the store and writes
    // nothing to it, so it is not gated by agent mode.
    let list = if args.remote {
        session.remote_folders()?
    } else {
        session.folders()?
    };

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
                let hint = if args.remote {
                    "the server lists no folders"
                } else {
                    "no folders yet — run `mailbox sync`, or `mailbox folders --remote` to ask the server"
                };
                ctx.out.print(ctx.out.dim(hint));
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
    let list = ctx.workspace()?.accounts()?;
    match ctx.out.mode {
        crate::output::Mode::Json => ctx.out.reply("accounts", &list)?,
        crate::output::Mode::Ndjson => ctx.out.ndjson_list("accounts", &list.accounts, None)?,
        crate::output::Mode::Human => {
            use crate::table::{Column, Table};
            let mut table = Table::new(vec![
                Column::new("account").min(10),
                Column::new("address").flexible().min(20),
                Column::new("login").min(10),
                Column::new("server").min(20),
                Column::new("credential").min(10),
            ]);
            for a in &list.accounts {
                table.push(vec![
                    a.id.clone(),
                    a.address.clone(),
                    // Repeating the address in every row would be noise; the
                    // column earns its width only when it says something.
                    if a.login_user == a.address {
                        ctx.out.dim("(address)")
                    } else {
                        a.login_user.clone()
                    },
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
    let mut opts = ctx.opts.clone();
    if let Some(a) = account {
        opts.account = Some(a.to_string());
    }
    let session = Session::open(&opts, ctx.tracer.clone(), ctx.clock.clone())?;
    let cfg = session.account_config()?.clone();

    // The same connect the sync path uses, so this tests what a sync will
    // actually do rather than an approximation of it.
    let mut client = session.connect()?;
    let folders = client.list_folders()?;
    let caps: Vec<String> = client.capabilities().iter().cloned().collect();
    let qresync = client.qresync();
    let _ = client.logout();

    let payload = serde_json::json!({
        "account": session.account_id,
        "address": cfg.address,
        "login_user": cfg.login_user(),
        "server": format!("{}:{}", cfg.imap_host.clone().unwrap_or_default(), cfg.port()),
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
            "{} {} at {}\n  logged in as: {}\n  {} folders\n  sync path: {}\n  capabilities: {}",
            ctx.out.paint("ok", GREEN),
            cfg.address,
            cfg.imap_host.clone().unwrap_or_default(),
            cfg.login_user(),
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

    let mut workspace = ctx.workspace()?;
    let cfg = workspace.primary()?.account_config()?.clone();

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
            "accounts": mailbox_core::workspace::account_ids(&workspace),
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
                mailbox_core::workspace::account_ids(&workspace).join(", ")
            ));
        }
        return Ok(());
    }

    // The per-account `offline` policy is applied inside `Session::sync`,
    // because a workspace syncs several accounts and they may differ.
    let opts = SyncOptions {
        full: args.full,
        folders: args.folder.clone(),
        since,
        prefetch_days: None,
        max_prefetch: 500,
    };

    if args.watch {
        return watch(ctx, &mut workspace, args, opts);
    }

    // §4.1: one sync engine per account, for a single pass as much as for a
    // loop. Held only for the pass, so this is a conflict with something
    // actually syncing rather than with something merely running.
    let _owners = own(&workspace)?;
    let result = workspace.sync(&opts)?;

    if ctx.out.is_structured() {
        ctx.out.reply("sync", &result)?;
    } else {
        if let Some(s) = &result.submitted {
            if s.attempted > 0 || s.uploaded > 0 {
                ctx.out.print(format!(
                    "outbox: {} sent, {} deferred, {} failed, {} filed",
                    s.sent, s.deferred, s.failed, s.uploaded
                ));
            }
        }
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

/// Becomes the sync engine for every account in scope (§4.1).
///
/// The lock lives as long as the returned value, so a caller holds it for
/// exactly the work it is doing — a pass, or a whole watch.
fn own(workspace: &mailbox_core::Workspace) -> Result<Vec<mailbox_core::owner::Owner>> {
    workspace
        .sessions()
        .iter()
        .map(|session| {
            mailbox_core::owner::Owner::acquire(session.store().dir(), &session.account_id)
        })
        .collect()
}

/// `sync --watch` (§5.3): sync, wait on `IDLE`, sync again.
///
/// This is the mode that makes the rest of the client behave like a mail
/// client rather than a mail viewer. Without it nothing arrives until someone
/// asks, the desktop notifications of §11 almost never fire, and a flag set on
/// a train sits in the queue until the next manual sync.
fn watch(
    ctx: &Ctx<'_>,
    workspace: &mut mailbox_core::Workspace,
    args: &SyncArgs,
    sync: SyncOptions,
) -> Result<()> {
    use mailbox_core::imap::watch::WatchOptions;

    // §4.1: exactly one process per account may be the sync engine, and this
    // one holds a connection for hours. Taken before the first pass so the
    // refusal is immediate rather than after a round trip.
    let mut owners = own(workspace)?;

    let notify = ctx.notify_config()?;
    let notifier = mailbox_core::notify::Notifier::discover(notify.command.as_deref());
    let structured = ctx.out.is_structured();

    if !structured {
        ctx.out.print(format!(
            "watching {} — Ctrl-C to stop",
            mailbox_core::workspace::account_ids(workspace).join(", ")
        ));
    }

    let options = WatchOptions {
        sync,
        idle_window: std::time::Duration::from_secs(args.idle_window.clamp(1, 29 * 60)),
        poll_interval: std::time::Duration::from_secs(args.poll.max(1)),
        max_passes: args.max_passes,
        ..Default::default()
    };

    let mut fatal: Option<Error> = None;
    mailbox_core::imap::watch::watch(workspace, &options, |workspace, wake, result| {
        match result {
            Ok(report) => {
                // Announced after the pass that brought them in, and marked so
                // a restart does not repeat itself (§11).
                if let Err(e) = announce(ctx, workspace, &notifier, &notify) {
                    ctx.out.warn(format!("could not announce new mail: {e}"));
                }
                if structured {
                    // One JSON document per pass, so a caller can follow this
                    // the way it follows `watch` (§6.1) — a stream of
                    // documents rather than one at the end that never comes.
                    let _ = ctx.out.ndjson_event(
                        "sync",
                        &serde_json::json!({
                            "wake": format!("{wake:?}").to_lowercase(),
                            "result": report,
                        }),
                    );
                } else {
                    report_pass(ctx, wake, report);
                }
                true
            }
            Err(e) => {
                if structured {
                    let _ = ctx
                        .out
                        .ndjson_event("sync", &serde_json::json!({ "error": e.to_response() }));
                } else {
                    ctx.out.warn(format!("sync failed: {e}"));
                }
                // A configuration mistake or a refused password retried every
                // fifteen minutes produces an authentication failure every
                // fifteen minutes and no mail. Only the network is worth
                // waiting out.
                if mailbox_core::imap::watch::is_transient(e) {
                    true
                } else {
                    fatal = Some(Error::Network(e.to_string()));
                    false
                }
            }
        }
    })?;

    drop(owners.drain(..));

    match fatal {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn report_pass(
    ctx: &Ctx<'_>,
    wake: mailbox_core::imap::watch::Wake,
    result: &mailbox_api::SyncResult,
) {
    use mailbox_core::imap::watch::Wake;
    let added: u32 = result.synced.iter().map(|r| r.messages_added).sum();
    let updated: u32 = result.synced.iter().map(|r| r.messages_updated).sum();
    let removed: u32 = result.synced.iter().map(|r| r.messages_removed).sum();

    // A loop that printed a line every nine minutes saying nothing happened
    // would be a loop nobody leaves running.
    if added == 0 && updated == 0 && removed == 0 && wake != Wake::Started {
        return;
    }
    ctx.out.print(format!(
        "{} +{added} ~{updated} -{removed}",
        ctx.out
            .dim(&mailbox_core::clock::format_rfc3339(ctx.clock.now())),
    ));
}

/// Raises a desktop notification for mail that arrived and has not been
/// announced (§11).
///
/// The query decides what is worth interrupting someone for, and defaults to
/// `is:unread in:inbox NOT tag:muted`. Marking happens whether or not the
/// notifier worked: a machine with no notification daemon should not
/// accumulate a backlog that all arrives at once when one appears.
fn announce(
    ctx: &Ctx<'_>,
    workspace: &mut mailbox_core::Workspace,
    notifier: &mailbox_core::notify::Notifier,
    config: &mailbox_core::config::Notify,
) -> Result<()> {
    // §6.4: an agent raises no desktop notifications. It is not sitting at the
    // machine, and something else is.
    if !config.enabled || ctx.agent_mode {
        return Ok(());
    }

    for session in workspace.sessions_mut() {
        let node = session.parse_query(&config.query)?;
        let waiting = session.store().unnotified(&node, config.max_per_sync)?;
        if waiting.is_empty() {
            continue;
        }

        // One notification per message up to a point, then one that says how
        // many — a hundred separate popups is not an announcement, it is a
        // denial of service with a friendly icon.
        if waiting.len() == 1 {
            let _ = notifier.send(&mailbox_core::notify::Notification::for_message(
                &waiting[0],
            ));
        } else {
            let _ = notifier.send(&mailbox_core::notify::Notification::for_many(waiting.len()));
        }

        let ids: Vec<String> = waiting.iter().map(|m| m.id.clone()).collect();
        session.store_mut().mark_notified(&ids)?;
    }
    Ok(())
}

pub fn status(ctx: &Ctx<'_>) -> Result<()> {
    let status = ctx.workspace()?.status()?;
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
        if a.queued_ops > 0 {
            ctx.out.print(format!(
                "  {} local {} not yet pushed — run `mailbox sync`",
                ctx.out.paint(&a.queued_ops.to_string(), YELLOW),
                if a.queued_ops == 1 {
                    "change"
                } else {
                    "changes"
                },
            ));
        }
        // §5.3: conflicts resolve to the server's flags, with a note here. The
        // note is the whole of the user's recourse, so it says what was lost
        // and why rather than just counting.
        for c in &a.conflicts {
            ctx.out.print(format!(
                "  {} {} {} {} — {}",
                ctx.out.paint("conflict", YELLOW),
                c.operation,
                ctx.out.dim(format::short(&c.message)),
                c.folder.clone().unwrap_or_default(),
                c.reason,
            ));
        }
    }
    Ok(())
}

/// `tui` (§6.1): launch the terminal interface.
pub fn tui(ctx: &Ctx<'_>) -> Result<()> {
    if ctx.out.is_structured() {
        return Err(Error::usage(
            "the TUI is an interactive interface; it has no --json form",
        ));
    }
    // Its own store handle, opened writable: the interface flags, moves, and
    // syncs, and the read-only fast path of §4.1 is for commands that only
    // list.
    let mut opts = ctx.opts.clone();
    opts.read_only = false;
    mailbox_tui::run(opts, ctx.tracer.clone(), ctx.clock.clone())
}

/// `notify` (§11): what the platform offers, and what is waiting.
pub fn notify(ctx: &Ctx<'_>, args: &NotifyArgs) -> Result<()> {
    // `--test` checks the platform mechanism and nothing else, so it reads the
    // config and stops there. Requiring a store would mean a user could not
    // check their notifications work until after their first sync — and could
    // not check at all while a store was waiting to be upgraded.
    let config = match &ctx.global.config {
        Some(path) => mailbox_core::config::Config::load(path)?,
        None => mailbox_core::config::Config::load_default()?,
    }
    .notify;
    let notifier = mailbox_core::notify::Notifier::discover(config.command.as_deref());

    if args.test {
        let sample = mailbox_core::notify::Notification {
            summary: "mailbox".into(),
            body: "Notifications are working.".into(),
        };
        notifier.send(&sample)?;
        let payload = serde_json::json!({
            "notifier": notifier.describe(),
            "available": notifier.is_available(),
            "sent": notifier.is_available(),
        });
        if ctx.out.is_structured() {
            ctx.out.reply("notify", &payload)?;
        } else if notifier.is_available() {
            ctx.out.print(format!(
                "sent a test notification via {}",
                notifier.describe()
            ));
        } else {
            ctx.out.print(ctx.out.dim(
                "no desktop notifier found — set notify.command in config.toml to use another",
            ));
        }
        return Ok(());
    }

    // What would be announced, without announcing it or marking anything.
    let workspace = ctx.workspace()?;
    let session = workspace.primary()?;
    let node = session.parse_query(&config.query)?;
    let mut waiting = Vec::new();
    for s in workspace.sessions() {
        let node = s
            .parse_query(&config.query)
            .unwrap_or_else(|_| node.clone());
        for m in s.store().unnotified(&node, config.max_per_sync)? {
            waiting.push(m);
        }
    }

    let payload = serde_json::json!({
        "enabled": config.enabled,
        "query": config.query,
        "notifier": notifier.describe(),
        "available": notifier.is_available(),
        "max_per_sync": config.max_per_sync,
        "pending": waiting,
    });

    if ctx.out.is_structured() {
        ctx.out.reply("notify", &payload)?;
        return Ok(());
    }

    ctx.out.print(format!(
        "{} via {}\n  query: {}",
        if config.enabled {
            ctx.out.paint("enabled", GREEN)
        } else {
            ctx.out.dim("disabled")
        },
        notifier.describe(),
        config.query,
    ));
    if waiting.is_empty() {
        ctx.out
            .print(ctx.out.dim("  nothing waiting to be announced"));
    } else {
        ctx.out.print(format!("  {} waiting:", waiting.len()));
        for m in &waiting {
            let n = mailbox_core::notify::Notification::for_message(m);
            ctx.out.print(format!("    {} — {}", n.summary, n.body));
        }
    }
    Ok(())
}

/// `mailbox schema <command>` (§6.4): the JSON Schema for a command's output.
pub fn schema(ctx: &Ctx<'_>, args: &SchemaArgs) -> Result<()> {
    use mailbox_api::*;

    let Some(command) = &args.command else {
        let commands = [
            "ls", "search", "show", "folders", "accounts", "sync", "status", "snooze", "watch",
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
        "send" | "compose" | "reply" => schemars::schema_for!(Reply<SendResult>),
        "outbox" => schemars::schema_for!(Reply<OutboxList>),
        "snooze" => schemars::schema_for!(Reply<SnoozeList>),
        "watch" => schemars::schema_for!(Reply<ChangeList>),
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
            let dump = ctx.workspace()?.store_dump(query.as_deref())?;
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
            let mut workspace = ctx.workspace()?;
            let mut n = 0;
            for session in workspace.sessions_mut() {
                n += session.store_mut().rebuild_threads(None)?;
            }
            let payload = serde_json::json!({ "rethreaded": n });
            if ctx.out.is_structured() {
                ctx.out.reply("debug rethread", &payload)?;
            } else {
                ctx.out.print(format!("rethreaded {n} messages"));
            }
            Ok(())
        }

        DebugAction::Generate {
            messages,
            seed,
            folders,
        } => {
            ctx.confirm("debug generate")?;
            let mut session = ctx.session()?;
            let report = session
                .store_mut()
                .generate_corpus(&mailbox_core::store::CorpusSpec {
                    messages: *messages,
                    seed: *seed,
                    folders: *folders,
                    ..Default::default()
                })?;
            let payload = serde_json::json!({
                "messages": report.messages,
                "threads": report.threads,
                "folders": report.folders,
                "elapsed_ms": report.elapsed_ms,
            });
            if ctx.out.is_structured() {
                ctx.out.reply("debug generate", &payload)?;
            } else {
                ctx.out.print(format!(
                    "generated {} messages in {} threads across {} folders in {} ms",
                    report.messages, report.threads, report.folders, report.elapsed_ms
                ));
            }
            Ok(())
        }

        DebugAction::Bench {
            samples,
            baseline,
            tolerance,
            save_baseline,
            budgets,
        } => bench(
            ctx,
            *samples,
            baseline.as_deref(),
            *tolerance,
            save_baseline.as_deref(),
            *budgets,
        ),

        DebugAction::Replay {
            transcript,
            strict,
            max_age_days,
        } => replay(ctx, transcript, *strict, *max_age_days),

        DebugAction::Fixture { action } => {
            if matches!(action, FixtureAction::Load { .. }) {
                ctx.confirm("debug fixture load")?;
            }
            fixture(ctx, action)
        }
    }
}

/// `debug bench` (§14.5): the §10 budgets, measured and compared.
fn bench(
    ctx: &Ctx<'_>,
    samples: u32,
    baseline: Option<&std::path::Path>,
    tolerance: f64,
    save_baseline: Option<&std::path::Path>,
    budgets: bool,
) -> Result<()> {
    let session = ctx.session()?;
    let report = mailbox_core::bench::run(session.store(), samples)?;

    if let Some(path) = save_baseline {
        std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
    }

    let mut regressions = Vec::new();
    if let Some(path) = baseline {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::usage(format!("{}: {e}", path.display())))?;
        let previous: mailbox_core::bench::BenchReport = serde_json::from_str(&text)
            .map_err(|e| Error::usage(format!("{}: {e}", path.display())))?;
        regressions = report.regressions(&previous, tolerance);
    }

    let over_budget: Vec<&mailbox_core::bench::Measurement> = report
        .measurements
        .iter()
        .filter(|m| !m.within_budget())
        .collect();

    if ctx.out.is_structured() {
        ctx.out.reply(
            "debug bench",
            &serde_json::json!({
                "messages": report.messages,
                "measurements": report.measurements,
                "regressions": regressions
                    .iter()
                    .map(|r| serde_json::json!({
                        "name": r.name,
                        "baseline_ms": r.baseline_ms,
                        "current_ms": r.current_ms,
                        "allowed_ms": r.allowed_ms,
                    }))
                    .collect::<Vec<_>>(),
                "over_budget": over_budget.iter().map(|m| &m.name).collect::<Vec<_>>(),
            }),
        )?;
    } else {
        ctx.out
            .print(format!("{} messages in the store", report.messages));
        use crate::table::{Column, Table};
        let mut table = Table::new(vec![
            Column::new("operation").flexible().min(20),
            Column::new("median ms").right().min(9),
            Column::new("worst ms").right().min(8),
            Column::new("budget").right().min(7),
            Column::new("rows").right().min(6),
        ]);
        for m in &report.measurements {
            let budget = match m.budget_ms {
                Some(b) if m.median_ms > b => ctx.out.paint(&format!("{b:.0}"), YELLOW),
                Some(b) => ctx.out.dim(&format!("{b:.0}")),
                None => ctx.out.dim("—"),
            };
            table.push(vec![
                m.name.clone(),
                format!("{:.2}", m.median_ms),
                format!("{:.2}", m.worst_ms),
                budget,
                m.rows.to_string(),
            ]);
        }
        ctx.out.print(table.render(ctx.out));
        for r in &regressions {
            ctx.out.warn(format!("regression: {r}"));
        }
    }

    // §14.5: gate on regression by default; absolute budgets are opt-in.
    if !regressions.is_empty() {
        return Err(Error::usage(format!(
            "{} measurement(s) regressed against the baseline",
            regressions.len()
        )));
    }
    if budgets && !over_budget.is_empty() {
        return Err(Error::usage(format!(
            "over the §10 budget: {}",
            over_budget
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

/// `debug replay` (§14.2's L1): a sync pass driven by a recording.
fn replay(
    ctx: &Ctx<'_>,
    path: &std::path::Path,
    strict: bool,
    max_age_days: Option<i64>,
) -> Result<()> {
    use mailbox_core::imap::transcript::Transcript;

    ctx.confirm("debug replay")?;
    let transcript = Transcript::load(path)?;

    // §14.2: a stale transcript is a test that passes about a server that no
    // longer exists. Refusing one is opt-in, because deciding what counts as
    // stale is the suite's business rather than this command's.
    let age = transcript.age_days(ctx.clock.now());
    if let Some(max) = max_age_days {
        if age > max {
            return Err(Error::usage(format!(
                "this transcript was recorded {age} days ago against {:?};                  re-record it or raise --max-age-days",
                transcript.server
            )));
        }
    }

    let mut session = ctx.session()?;
    let (mut client, divergence) =
        mailbox_core::imap::Client::replay(transcript.clone(), ctx.tracer.clone())?;

    let clock = ctx.clock.clone();
    // The options the recording was made with, so the replayed pass asks the
    // same questions. Falling back to the defaults keeps an older transcript,
    // recorded before the header carried them, replayable.
    let opts = transcript.sync_options.clone().unwrap_or_default();
    let result =
        mailbox_core::imap::sync::sync_once(&mut client, session.store_mut(), &opts, &clock);

    let diverged = divergence.lock().expect("divergence lock").clone();
    if strict {
        if let Some(what) = &diverged {
            return Err(Error::protocol(format!(
                "the engine diverged from the recording: {what}"
            )));
        }
    }

    let report = result?;
    let payload = serde_json::json!({
        "transcript": path.display().to_string(),
        "server": transcript.server,
        "recorded_at": transcript.recorded_at,
        "age_days": age,
        "commands": transcript.commands(),
        "diverged": diverged,
        "synced": [report],
    });

    if ctx.out.is_structured() {
        ctx.out.reply("debug replay", &payload)?;
    } else {
        ctx.out.print(format!(
            "replayed {} commands recorded {age} days ago against {}",
            transcript.commands(),
            transcript.server
        ));
    }
    Ok(())
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

/// `snooze` (§7): set a message aside until later.
///
/// Local by construction. Nothing is queued for the server and nothing is
/// pushed, because a snooze is not an IMAP concept — inventing one, as a
/// keyword or a hidden folder, would put a private decision into a mailbox
/// shared with every other client the user runs. The cost is that the message
/// reappears on *this* machine, which is why every listing here names the
/// device holding the timer rather than implying a sync (§5.4 makes it
/// portable in M8).
pub fn snooze(ctx: &Ctx<'_>, args: &crate::cli::SnoozeArgs) -> Result<()> {
    if args.list {
        let workspace = ctx.workspace()?;
        let snoozed = workspace.snoozed()?;
        return report_snoozed(ctx, &snoozed);
    }

    if args.args.is_empty() {
        return Err(Error::usage(
            "which message? `mailbox snooze <id> 2h`, or `--list` to see what is asleep",
        ));
    }

    // `--clear` takes messages and no time; otherwise the last argument is the
    // time, as `mv`'s last argument is the destination (§6.1).
    let (ids, until) = if args.clear {
        (args.args.as_slice(), None)
    } else {
        let (last, rest) = args.args.split_last().expect("checked non-empty");
        if rest.is_empty() {
            return Err(Error::usage(
                "snooze until when? `mailbox snooze <id> 2h`, `tomorrow`, or a date",
            ));
        }
        (rest, Some(last.clone()))
    };

    let refs: Vec<IdRef> = ids.iter().map(|s| IdRef::parse(s)).collect();
    for r in &refs {
        ctx.warn_if_positional(r);
    }

    let Some(until) = until else {
        ctx.confirm("snooze --clear")?;
        let mut workspace = ctx.workspace()?;
        if ctx.global.dry_run {
            return report_dry_run(ctx, "snooze", &workspace, &refs, None, None);
        }
        let woken = workspace.unsnooze(&refs)?;
        return report_snooze_change(ctx, "woke", &woken, None);
    };

    let until = mailbox_core::store::snooze::parse_until(&until, &ctx.clock)?;
    ctx.confirm("snooze")?;

    let mut workspace = ctx.workspace()?;
    if ctx.global.dry_run {
        return report_dry_run(ctx, "snooze", &workspace, &refs, None, None);
    }
    let ids = workspace.snooze(&refs, until)?;
    report_snooze_change(ctx, "snoozed", &ids, Some(until))
}

fn report_snooze_change(
    ctx: &Ctx<'_>,
    verb: &str,
    ids: &[String],
    until: Option<i64>,
) -> Result<()> {
    if ctx.out.is_structured() {
        let payload = serde_json::json!({
            "changed": ids.len(),
            "messages": ids,
            "until": until.map(mailbox_core::clock::format_rfc3339),
            "device": mailbox_core::paths::device_name(),
        });
        ctx.out.reply("snooze", &payload)?;
        return Ok(());
    }
    if ids.is_empty() {
        ctx.out.print(ctx.out.dim("nothing changed"));
        return Ok(());
    }
    for id in ids {
        let when = match until {
            Some(t) => format!(" until {}", mailbox_core::clock::format_rfc3339(t)),
            None => String::new(),
        };
        ctx.out.print(format!(
            "{} {} {verb}{when}",
            ctx.out.paint("✓", GREEN),
            ctx.out.dim(format::short(id)),
        ));
    }
    if until.is_some() {
        ctx.out.note(format!(
            "the timer is on {} — a snoozed message comes back on the device that snoozed it",
            mailbox_core::paths::device_name()
        ));
    }
    Ok(())
}

fn report_snoozed(ctx: &Ctx<'_>, snoozed: &[mailbox_api::Snoozed]) -> Result<()> {
    match ctx.out.mode {
        crate::output::Mode::Json => {
            ctx.out.reply(
                "snooze",
                &mailbox_api::SnoozeList {
                    snoozed: snoozed.to_vec(),
                },
            )?;
            Ok(())
        }
        crate::output::Mode::Ndjson => {
            ctx.out.ndjson_list("snooze", snoozed, None)?;
            Ok(())
        }
        crate::output::Mode::Human => {
            if snoozed.is_empty() {
                ctx.out.print(ctx.out.dim("nothing is snoozed"));
                return Ok(());
            }
            use crate::table::{Column, Table};
            let mut table = Table::new(vec![
                Column::new("id").min(8),
                Column::new("until").min(20),
                Column::new("subject").flexible().min(20),
                Column::new("device").min(8),
            ]);
            for s in snoozed {
                table.push(vec![
                    ctx.out.dim(format::short(&s.id)),
                    s.until.clone(),
                    s.subject.clone(),
                    ctx.out.dim(&s.device),
                ]);
            }
            maybe_page(ctx.out, &table.render(ctx.out));
            Ok(())
        }
    }
}

/// `serve` (§3): the web interface, on loopback, behind a one-time token.
pub fn serve(ctx: &Ctx<'_>, args: &crate::cli::ServeArgs) -> Result<()> {
    use mailbox_web::server::{make_token, Options, Server};

    if args.no_token && args.token.is_some() {
        return Err(Error::usage(
            "`--no-token` and `--token` say opposite things",
        ));
    }
    let token = if args.no_token {
        None
    } else {
        Some(match &args.token {
            Some(t) if t.trim().is_empty() => {
                return Err(Error::usage("an empty `--token` is not a token"))
            }
            Some(t) => t.clone(),
            None => make_token(&mailbox_core::Rng::from_env()),
        })
    };

    let server = Server::bind(
        Options {
            bind: args.bind,
            port: args.port,
            token,
            max_requests: args.max_requests,
        },
        ctx.opts.clone(),
        ctx.tracer.clone(),
        ctx.clock.clone(),
    )?;
    let url = server.url();

    if ctx.out.is_structured() {
        ctx.out.reply(
            "serve",
            &serde_json::json!({
                "url": url,
                "address": server.address().map(|a| a.to_string()).unwrap_or_default(),
            }),
        )?;
    } else {
        // The QR is for the phone case §3 names: a device that cannot copy and
        // paste from this terminal, and a token nobody should be typing.
        if !args.bind.is_loopback() {
            ctx.out.print(crate::qr::render(&url));
        }
        ctx.out.print(format!("  {url}"));
        ctx.out.print(
            ctx.out
                .dim("  the token is in that link — anything holding it can read this mail"),
        );
        if !args.dry_run_url {
            ctx.out.print(ctx.out.dim("  Ctrl-C to stop"));
        }
    }

    if args.dry_run_url {
        return Ok(());
    }
    server.run()
}
