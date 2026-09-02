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
mod output;
mod table;

use std::io::IsTerminal;
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

    // §6.4 again: no interactive prompts when stdin is not a TTY. Checked once,
    // here, so no command has to remember to.
    let interactive = std::io::stdin().is_terminal();

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
        interactive,
    };

    match &cli.command {
        Command::Ls(args) => commands::ls(&ctx, args),
        Command::Show(args) => commands::show(&ctx, args),
        Command::Search(args) => commands::search(&ctx, args),
        Command::Folders => commands::folders(&ctx),
        Command::Accounts(args) => commands::accounts(&ctx, args),
        Command::Sync(args) => commands::sync(&ctx, args),
        Command::Status => commands::status(&ctx),
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
        Command::Sync(_) => true,
        Command::Debug(args) => matches!(
            args.action,
            cli::DebugAction::Fixture { .. } | cli::DebugAction::Rethread
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
