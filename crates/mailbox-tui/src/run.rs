//! The event loop: terminal setup, input, workers, and teardown.
//!
//! The one rule the loop enforces is §7's: nothing that could block runs on it.
//! A sync happens on a worker thread and arrives back as an [`Update`]; an
//! editor or a pipe gets the terminal handed to it and takes it back. What is
//! left on the loop is reading a key, changing state, and drawing — which is
//! what makes §10's 16 ms repaint budget a property of the structure.

use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use mailbox_core::clock::Clock;
use mailbox_core::error::{Error, Result};
use mailbox_core::imap::sync::SyncOptions;
use mailbox_core::render::RenderOptions;
use mailbox_core::session::{IdRef, Session, SessionOptions};
use mailbox_core::store::write::Written;
use mailbox_core::{Tracer, Workspace};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::{App, Effect, SuspendedTask, Update, View};
use crate::keys::{describe_key, Action, Keymap};

/// How long to wait for a key before looping anyway.
///
/// Short enough that a worker's result appears promptly, long enough that an
/// idle interface costs no measurable CPU — §10 budgets idle at about zero.
const TICK: Duration = Duration::from_millis(100);

/// Runs the interface until the user quits.
pub fn run(opts: SessionOptions, tracer: Tracer, clock: Clock) -> Result<()> {
    let mut workspace = Workspace::open(&opts, tracer.clone(), clock.clone())?;
    let config = workspace.primary()?.config.clone();
    let (keymap, unknown) = Keymap::from_config(&config);

    let mut app = App::new();
    app.reload(&workspace)?;
    if !unknown.is_empty() {
        // Said rather than swallowed: a typo in `[keys]` that silently does
        // nothing is a rebinding the user believes is live.
        app.status = format!("unknown key bindings ignored: {}", unknown.join(", "));
    }

    let mut terminal = enter()?;
    let (tx, rx) = mpsc::channel();
    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut workspace,
        &keymap,
        &opts,
        &tracer,
        &clock,
        &tx,
        &rx,
    );
    leave(&mut terminal)?;
    result
}

type Term = Terminal<CrosstermBackend<io::Stdout>>;

fn enter() -> Result<Term> {
    enable_raw_mode().map_err(|e| Error::usage(format!("this is not a terminal: {e}")))?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

fn leave(terminal: &mut Term) -> Result<()> {
    // Best-effort throughout: a failure here must not stop the others, or a
    // panic partway through leaves the user with a broken terminal.
    let _ = disable_raw_mode();
    let _ = terminal.backend_mut().execute(LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn event_loop(
    terminal: &mut Term,
    app: &mut App,
    workspace: &mut Workspace,
    keymap: &Keymap,
    opts: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    tx: &Sender<Update>,
    rx: &Receiver<Update>,
) -> Result<()> {
    // §10 budgets idle CPU at about zero, so the loop draws when something
    // changed rather than on every tick. Without this an idle interface
    // repaints ten times a second forever.
    let mut needs_redraw = true;

    loop {
        if needs_redraw {
            let mut links = Vec::new();
            terminal.draw(|frame| links = crate::ui::draw(frame, app, keymap))?;
            // After the frame, never during it: see [`crate::markdown::LinkSpan`].
            mark_hyperlinks(&links)?;
            needs_redraw = false;
        }

        // Anything a worker finished. Drained without blocking, so a busy
        // worker never holds up a keystroke.
        while let Ok(update) = rx.try_recv() {
            needs_redraw = true;
            match update {
                Update::SyncFinished(outcome) => {
                    app.syncing = false;
                    app.status = match outcome {
                        Ok(summary) => summary,
                        Err(e) => format!("sync failed: {e}"),
                    };
                    app.dirty = true;
                }
                Update::Status(text) => app.status = text,
            }
        }

        if app.dirty {
            app.reload(workspace)?;
            needs_redraw = true;
        }

        if !event::poll(TICK)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => {
                let started = Instant::now();
                if handle_key(app, workspace, keymap, opts, tracer, clock, tx, key)? {
                    return Ok(());
                }
                needs_redraw = true;
                // §10 budgets a keystroke at 16 ms. Recorded rather than
                // enforced: the trace is where a slow key shows up, and failing
                // the user's keystroke would be a worse answer than being slow.
                let elapsed = started.elapsed();
                if elapsed > Duration::from_millis(16) {
                    tracer.emit(
                        "tui.slow_key",
                        serde_json::json!({
                            "key": describe_key(&key),
                            "ms": elapsed.as_millis()
                        }),
                    );
                }
            }
            // A resize changes the layout, so it has to repaint.
            Event::Resize(_, _) => needs_redraw = true,
            _ => {}
        }
    }
}

/// Returns true when the interface should exit.
#[allow(clippy::too_many_arguments)]
fn handle_key(
    app: &mut App,
    workspace: &mut Workspace,
    keymap: &Keymap,
    opts: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    tx: &Sender<Update>,
    key: crossterm::event::KeyEvent,
) -> Result<bool> {
    // A prompt eats printable keys, so `/` then `d` searches for "d" rather
    // than deleting something. Getting this backwards is how a search box
    // destroys mail.
    if app.prompt.is_some() {
        match key.code {
            KeyCode::Esc => {
                app.cancel_prompt();
                app.dirty = true;
                return Ok(false);
            }
            KeyCode::Enter => {
                let effect = app.submit_prompt();
                return apply_effect(app, workspace, opts, tracer, clock, tx, effect);
            }
            KeyCode::Backspace => {
                app.backspace_prompt();
                refresh_live_search(app);
                return Ok(false);
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.type_into_prompt(c);
                refresh_live_search(app);
                return Ok(false);
            }
            _ => return Ok(false),
        }
    }

    // The help view closes on anything, so it never becomes a place to be stuck.
    if app.view == View::Help {
        app.view = View::List;
        return Ok(false);
    }

    let Some(action) = keymap.action(&describe_key(&key)) else {
        return Ok(false);
    };

    // The store writes are performed here, where the session lives.
    let performed = match action {
        Action::ToggleRead => Some(toggle_flag(app, opts, tracer, clock, "seen")?),
        Action::ToggleFlagged => Some(toggle_flag(app, opts, tracer, clock, "flagged")?),
        Action::Archive => Some(move_to(app, opts, tracer, clock, "archive")?),
        Action::Delete => Some(move_to(app, opts, tracer, clock, "trash")?),
        Action::Spam => Some(move_to(app, opts, tracer, clock, "junk")?),
        _ => None,
    };
    if let Some(status) = performed {
        app.status = status;
        app.dirty = true;
        // Moving a message out of the current list should leave the cursor
        // where it was, pointing at whatever moved up into that row.
        return Ok(false);
    }

    let effect = app.handle(action);
    if action == Action::Open || action == Action::Thread {
        load_open_message(app, workspace)?;
    }
    apply_effect(app, workspace, opts, tracer, clock, tx, effect)
}

/// Filtering as you type (§7).
///
/// The query is only adopted when it parses: a half-typed `from:` is not an
/// error to show someone mid-word, it is a query that is not finished yet.
fn refresh_live_search(app: &mut App) {
    let Some(text) = app.live_search().map(str::to_string) else {
        return;
    };
    if text.trim().is_empty() {
        app.query.clear();
        app.dirty = true;
        return;
    }
    // A half-typed word should match what it is the start of: `postm` finds
    // `postmortem`. Only a trailing bare word gets this — a structured term
    // like `from:ali` already substring-matches, and a quoted phrase means
    // exactly itself.
    let query = as_prefix_search(&text);
    if mailbox_core::query::parse(&query, &Clock::from_env()).is_ok() {
        app.query = query;
        app.dirty = true;
    }
}

/// Turns the last bare word of a query into a prefix term.
fn as_prefix_search(text: &str) -> String {
    let trimmed = text.trim_end();
    let Some(last) = trimmed.rsplit(' ').next() else {
        return text.to_string();
    };
    // Leave anything that is already a term, a phrase, or an operator alone.
    let plain = !last.is_empty()
        && !last.contains(':')
        && !last.contains('"')
        && !last.ends_with('*')
        && !last.starts_with('-')
        && !matches!(last, "AND" | "OR" | "NOT")
        && last.chars().all(|c| c.is_alphanumeric());
    if plain {
        format!("{trimmed}*")
    } else {
        text.to_string()
    }
}

fn load_open_message(app: &mut App, workspace: &Workspace) -> Result<()> {
    let Some(id) = app.current_id() else {
        return Ok(());
    };
    // Read-only and from the store: opening a message must paint now, and the
    // body fetch that §5.1 might want is not worth blocking a keystroke on.
    let detail = workspace.show(
        &IdRef::Id(id),
        &RenderOptions {
            strip_quotes: false,
            // Markdown, not flat text: most mail is HTML, and Markdown is what
            // the core makes of it while keeping the structure — headings,
            // lists, emphasis, links — that the flat form throws away. The
            // renderer turns the markers into styles (§7), so nobody sees a
            // `**`.
            format: mailbox_api::BodyFormat::Md,
            ..Default::default()
        },
        false,
    )?;
    app.open = detail.message;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_effect(
    app: &mut App,
    workspace: &mut Workspace,
    opts: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    tx: &Sender<Update>,
    effect: Effect,
) -> Result<bool> {
    match effect {
        Effect::None => Ok(false),
        Effect::Quit => Ok(true),
        Effect::StartSync => {
            if app.syncing {
                return Ok(false);
            }
            app.syncing = true;
            app.status = String::new();
            start_sync(opts.clone(), tracer.clone(), clock.clone(), tx.clone());
            Ok(false)
        }
        // Not suspended: the opener detaches and its output is discarded, so
        // the interface never loses the terminal for it.
        Effect::OpenUrl(url) => {
            app.status = match mailbox_core::open::open_url(&url) {
                Ok(()) => format!("opened {url}"),
                Err(e) => format!("{e}"),
            };
            Ok(false)
        }
        Effect::Copy(text) => {
            // Straight at the terminal: it owns the clipboard, and under tmux
            // or ssh it is the only thing that can reach the right one.
            // "sent to", not "copied": OSC 52 has no reply, and a terminal
            // may be configured to refuse it. Claiming success we cannot
            // observe is how someone loses a URL they thought they had.
            app.status = match mailbox_core::clipboard::copy(&mut io::stdout(), &text) {
                Ok(()) => format!("sent to the clipboard: {text}"),
                Err(e) => format!("could not copy: {e}"),
            };
            Ok(false)
        }
        Effect::Write(write) => {
            app.status = perform_write(opts, tracer, clock, &write)?;
            app.dirty = true;
            Ok(false)
        }
        Effect::Suspend(task) => {
            run_suspended(app, opts, tracer, clock, task)?;
            app.dirty = true;
            let _ = workspace;
            Ok(false)
        }
    }
}

/// Syncs on a worker thread.
///
/// §7 forbids modal "please wait" states, so the sync gets its own thread and
/// its own store handle; the interface keeps painting from the store it
/// already has, and picks up the result when it arrives.
fn start_sync(opts: SessionOptions, tracer: Tracer, clock: Clock, tx: Sender<Update>) {
    std::thread::spawn(move || {
        let mut writable = opts;
        writable.read_only = false;
        let outcome = Workspace::open(&writable, tracer, clock)
            .and_then(|mut w| w.sync(&SyncOptions::default()))
            .map(|result| {
                let added: u32 = result.synced.iter().map(|r| r.messages_added).sum();
                let removed: u32 = result.synced.iter().map(|r| r.messages_removed).sum();
                match (added, removed) {
                    (0, 0) => "up to date".to_string(),
                    (a, 0) => format!("{a} new"),
                    (a, r) => format!("{a} new, {r} gone"),
                }
            })
            .map_err(|e| e.to_string());
        let _ = tx.send(Update::SyncFinished(outcome));
    });
}

/// Hands the terminal to an editor or a command, and takes it back.
fn run_suspended(
    app: &mut App,
    opts: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    task: SuspendedTask,
) -> Result<()> {
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);

    let outcome = suspended_task(opts, tracer, clock, &task);

    // The terminal comes back whatever happened, so a failing editor cannot
    // strand the user in raw mode.
    let _ = enable_raw_mode();
    let _ = io::stdout().execute(EnterAlternateScreen);

    app.status = match outcome {
        Ok(status) => status,
        Err(e) => format!("failed: {e}"),
    };
    Ok(())
}

fn suspended_task(
    opts: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    task: &SuspendedTask,
) -> Result<String> {
    // The CLI already knows how to compose, reply, and forward — including the
    // editor handling and the outbox — so the TUI runs it rather than growing a
    // second copy of that logic (§4: if a frontend needs logic the core lacks,
    // it belongs in the core; here it is already in the binary).
    let exe = std::env::current_exe()
        .map_err(|e| Error::usage(format!("cannot find my own binary: {e}")))?;

    match task {
        SuspendedTask::Pipe { id, command } => {
            let mut writable = opts.clone();
            writable.read_only = false;
            let session = Session::open(&writable, tracer.clone(), clock.clone())?;
            let raw = session.raw(&IdRef::Id(id.clone()))?;

            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| Error::usage(format!("could not run `{command}`: {e}")))?;
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(&raw);
            }
            let status = child.wait()?;
            Ok(format!("piped to `{command}` ({status})"))
        }
        SuspendedTask::Compose => {
            let status = std::process::Command::new(&exe).arg("compose").status()?;
            Ok(if status.success() {
                "composed".into()
            } else {
                "compose cancelled".into()
            })
        }
        SuspendedTask::Reply { id, all } => {
            let mut command = std::process::Command::new(&exe);
            command.arg("reply").arg(id);
            if *all {
                command.arg("--all");
            }
            let status = command.status()?;
            Ok(if status.success() {
                "reply queued".into()
            } else {
                "reply cancelled".into()
            })
        }
        SuspendedTask::Forward { id } => {
            let status = std::process::Command::new(&exe)
                .arg("reply")
                .arg(id)
                .arg("--forward")
                .status()?;
            Ok(if status.success() {
                "forward queued".into()
            } else {
                "forward cancelled".into()
            })
        }
    }
}

/// Marks up the links the frame just drew, if the terminal wants them.
fn mark_hyperlinks(links: &[crate::markdown::LinkSpan]) -> Result<()> {
    if links.is_empty() || !crate::markdown::hyperlinks_wanted() {
        return Ok(());
    }
    crate::markdown::write_hyperlinks(&mut io::stdout(), links)?;
    Ok(())
}

/// Performs a write the state machine asked for.
///
/// The session is opened here rather than held, for the same reason the flag
/// and move keys open one: the interface's own handle is read-only (§4.1), and
/// a write that borrowed it would make every repaint contend with a sync.
fn perform_write(
    opts: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    write: &crate::app::Write,
) -> Result<String> {
    use crate::app::Write;

    let mut writable = opts.clone();
    writable.read_only = false;
    let mut session = Session::open(&writable, tracer.clone(), clock.clone())?;
    let target = [IdRef::Id(match write {
        Write::Tag { id, .. } | Write::Snooze { id, .. } => id.clone(),
    })];

    match write {
        Write::Tag { tag, .. } => {
            let spec = mailbox_core::session::FlagSpec {
                add_tags: vec![tag.to_ascii_lowercase()],
                ..Default::default()
            };
            session.flag(&target, &spec)?;
            Ok(format!("tagged {tag}"))
        }
        Write::Snooze { until, .. } => {
            // The time is parsed here because the grammar lives in the core and
            // its answer depends on the injected clock. A time that does not
            // parse is the user's typo, not a failure: it goes to the status
            // line and the message stays where it is.
            match mailbox_core::store::snooze::parse_until(until, clock) {
                Ok(when) => {
                    session.snooze(&target, when)?;
                    Ok(format!(
                        "snoozed until {} — on {}",
                        mailbox_core::clock::format_rfc3339(when),
                        mailbox_core::paths::device_name()
                    ))
                }
                Err(e) => Ok(format!("{e}")),
            }
        }
    }
}

fn toggle_flag(
    app: &App,
    opts: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    flag: &str,
) -> Result<String> {
    let Some(target) = app.target() else {
        return Ok(String::new());
    };
    let has = app
        .current()
        .map(|m| m.flags.iter().any(|f| f == flag))
        .unwrap_or(false);

    let mut writable = opts.clone();
    writable.read_only = false;
    let mut session = Session::open(&writable, tracer.clone(), clock.clone())?;

    let spec = if has {
        mailbox_core::session::FlagSpec {
            remove_flags: vec![flag.to_string()],
            ..Default::default()
        }
    } else {
        mailbox_core::session::FlagSpec {
            add_flags: vec![flag.to_string()],
            ..Default::default()
        }
    };
    let written: Vec<Written> = session.flag(std::slice::from_ref(&target), &spec)?;
    Ok(if written.iter().any(|w| w.changed) {
        format!("{}{flag}", if has { "-" } else { "+" })
    } else {
        String::new()
    })
}

fn move_to(
    app: &App,
    opts: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    special_use: &str,
) -> Result<String> {
    let Some(target) = app.target() else {
        return Ok(String::new());
    };
    let mut writable = opts.clone();
    writable.read_only = false;
    let mut session = Session::open(&writable, tracer.clone(), clock.clone())?;
    let folder = session.special_folder(special_use)?;
    session.move_to(std::slice::from_ref(&target), &folder)?;
    Ok(format!("moved to {folder}"))
}
