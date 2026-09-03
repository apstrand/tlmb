//! `mailbox` — the reference interface (§6).
//!
//! Two rules shape this binary:
//!
//! 1. It is thin. Everything it does is a call into `mailbox-core`; what lives
//!    here is argument parsing and rendering (§4).
//! 2. A human and an agent are equally first-class (§6.4). `--json` is not a
//!    debugging aid — it is the API, it exists on every command, and the test
//!    suite consumes it.

mod cli;
mod commands;
mod format;
mod gui;
mod output;
mod pairing;
mod qr;
mod sending;
mod table;

use std::process::ExitCode;

use clap::Parser;
use mailbox_api::{ErrorCode, ErrorResponse};
use mailbox_core::clock::Clock;
use mailbox_core::session::SessionOptions;
use mailbox_core::Tracer;

use cli::{Cli, Command};
use output::Out;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => return usage_exit(e),
    };
    let out = Out::new(&cli.global);

    match run(&cli, &out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            out.error(&err.to_response());
            ExitCode::from(err.exit_code() as u8)
        }
    }
}

/// clap's own errors are usage errors (§6.6 exit code 2), except `--help` and
/// `--version`, which are successes that happen to abort parsing.
fn usage_exit(e: clap::Error) -> ExitCode {
    use clap::error::ErrorKind;
    if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
        let _ = e.print();
        return ExitCode::SUCCESS;
    }
    // A JSON caller gets a structured error even when the arguments never
    // parsed — otherwise the one failure an agent is most likely to hit is the
    // one it cannot read.
    let wants_json = std::env::args().any(|a| a == "--json" || a == "--ndjson");
    if wants_json {
        let resp = ErrorResponse::new(ErrorCode::Usage, e.to_string().trim().to_string())
            .with_hint("run `mailbox --help` for the accepted arguments");
        eprintln!("{}", serde_json::to_string(&resp).unwrap_or_default());
    } else {
        let _ = e.print();
    }
    ExitCode::from(ErrorCode::Usage.exit_code() as u8)
}

fn run(cli: &Cli, out: &Out) -> mailbox_core::Result<()> {
    let clock = Clock::from_env();
    let tracer = build_tracer(cli, &clock)?;

    // §6.4: `MAILBOX_AGENT=1` or `--profile agent` makes any state-changing
    // command require an explicit `--yes`.
    let agent_mode = cli.global.profile.as_deref() == Some("agent")
        || std::env::var("MAILBOX_AGENT").is_ok_and(|v| v == "1");

    let opts = SessionOptions {
        account: cli.global.account.clone(),
        config_path: cli.global.config.clone(),
        // Reads skip ownership entirely (§4.1). Only the commands that write
        // ask for a writable store.
        read_only: !needs_write(&cli.command),
        agent_mode,
        timeout_ms: cli.global.timeout,
    };

    let ctx = commands::Ctx {
        out,
        global: &cli.global,
        opts,
        tracer,
        clock,
        agent_mode,
    };

    match &cli.command {
        Command::Ls(args) => commands::ls(&ctx, args),
        Command::Show(args) => commands::show(&ctx, args),
        Command::Search(args) => commands::search(&ctx, args),
        Command::Flag(args) => commands::flag(&ctx, args),
        Command::Attach(args) => commands::attach(&ctx, args),
        Command::Snooze(args) => commands::snooze(&ctx, args),
        Command::Serve(args) => commands::serve(&ctx, args),
        Command::Gui => gui::run(&ctx),
        Command::Send(args) => sending::send(&ctx, args),
        Command::Compose(args) => sending::compose(&ctx, args),
        Command::Reply(args) => sending::reply(&ctx, args),
        Command::Outbox(args) => sending::outbox(&ctx, args),
        Command::Mv(args) => commands::mv(&ctx, args, false),
        Command::Cp(args) => commands::mv(&ctx, args, true),
        Command::Rm(args) => commands::move_to_special(&ctx, args, "trash"),
        Command::Archive(args) => commands::move_to_special(&ctx, args, "archive"),
        Command::Spam(args) => commands::move_to_special(&ctx, args, "junk"),
        Command::Folders(args) => commands::folders(&ctx, args),
        Command::Accounts(args) => commands::accounts(&ctx, args),
        Command::Sync(args) => commands::sync(&ctx, args),
        Command::Status => commands::status(&ctx),
        Command::Notify(args) => commands::notify(&ctx, args),
        Command::Tui => commands::tui(&ctx),
        Command::Pair(args) => pairing::pair(&ctx, args),
        Command::Schema(args) => commands::schema(&ctx, args),
        Command::Debug(args) => commands::debug(&ctx, args),
    }
}

/// Whether a command needs the store open for writing.
///
/// Getting this wrong in the safe direction costs nothing; getting it wrong in
/// the other direction turns a read into an ownership contest (§4.1), so the
/// default here is "read-only unless we know better".
fn needs_write(cmd: &Command) -> bool {
    match cmd {
        // Every write goes through the store, so these need it writable even
        // though none of them touches the network (§5.3).
        Command::Sync(_)
        | Command::Flag(_)
        | Command::Mv(_)
        | Command::Cp(_)
        | Command::Rm(_)
        | Command::Archive(_)
        | Command::Spam(_)
        | Command::Send(_)
        | Command::Compose(_)
        | Command::Reply(_) => true,
        // The TUI writes: flags, moves, and a sync it starts itself.
        Command::Tui => true,
        // The GUI's reading handle is read-only; its writes open their own
        // session on the worker thread (§4.1).
        Command::Gui => false,
        // Setting a message aside is a local write; listing what is asleep is
        // not.
        Command::Snooze(args) => !args.list,
        // The server reads; each write it performs opens its own session
        // (§4.1), so the handle it holds stays read-only.
        Command::Serve(_) => false,
        // Receiving a pairing rewrites the config, and may create a store.
        Command::Pair(_) => true,
        // `show` and `attach` fetch a body they do not have (§5.1), which is a
        // write to the blob store and the message row.
        Command::Show(args) => !args.offline,
        Command::Attach(args) => !args.offline,
        // Listing is a read; everything else the outbox does is a write.
        Command::Outbox(args) => {
            !matches!(args.action, None | Some(cli::OutboxAction::List { .. }))
        }
        Command::Debug(args) => matches!(
            args.action,
            cli::DebugAction::Fixture { .. }
                | cli::DebugAction::Rethread
                | cli::DebugAction::Replay { .. }
                | cli::DebugAction::Generate { .. }
        ),
        _ => false,
    }
}

fn build_tracer(cli: &Cli, clock: &Clock) -> mailbox_core::Result<Tracer> {
    let Some(path) = &cli.global.trace else {
        return Ok(Tracer::disabled());
    };
    // `sync --once --until <event>` (§6.7): the tracer holds the stop
    // condition, because it is the thing that sees every event.
    let until = match &cli.command {
        Command::Sync(args) => args.until.clone(),
        _ => None,
    };
    Ok(Tracer::to_file(
        path,
        clock.clone(),
        cli.global.trace_bodies,
        until,
    )?)
}
