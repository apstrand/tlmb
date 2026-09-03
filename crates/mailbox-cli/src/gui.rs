//! The desktop shell (§7): a window, and a backend that reads the store.
//!
//! Thin, as §4 requires. Everything the interface *does* is in `mailbox-ui`;
//! what is here is the half that cannot be shared with a browser — a native
//! window, and a worker thread that opens `meta.db`.
//!
//! The worker exists because §10 budgets a repaint at 16 ms and a store read at
//! rather more than that on a large mailbox. The interface submits and carries
//! on drawing; the answer arrives when it arrives. That is the same shape the
//! browser's `fetch` has, which is why one interface can sit on both.

use std::sync::mpsc::{Receiver, Sender};

use mailbox_core::error::Result;
use mailbox_core::render::RenderOptions;
use mailbox_core::session::{FlagSpec, IdRef, Session, SessionOptions};
use mailbox_core::store::read::{ListOptions, Sort};
use mailbox_core::{Clock, Tracer, Workspace};
use mailbox_ui::backend::{Backend, Request, Response, Seq};

use crate::commands::Ctx;

/// Runs the interface until the window closes.
pub fn run(ctx: &Ctx<'_>) -> Result<()> {
    let backend = Store::start(ctx.opts.clone(), ctx.tracer.clone(), ctx.clock.clone());
    let label = backend.label();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("mailbox")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native(
        "mailbox",
        options,
        Box::new(move |_cc| Ok(Box::new(Gui::new(backend)))),
    )
    .map_err(|e| mailbox_core::Error::usage(format!("could not open a window ({label}): {e}")))
}

struct Gui {
    app: mailbox_ui::App,
    backend: Store,
    started: bool,
}

impl Gui {
    fn new(backend: Store) -> Gui {
        Gui {
            app: mailbox_ui::App::new(),
            backend,
            started: false,
        }
    }
}

impl eframe::App for Gui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.started {
            self.started = true;
            self.app.start(&mut self.backend);
        }
        // Never blocks: whatever the worker has finished, and no waiting for
        // what it has not.
        if self.app.pump(&mut self.backend) {
            ctx.request_repaint();
        }
        mailbox_ui::view::draw(ctx, &mut self.app, &mut self.backend);

        // While work is outstanding, keep the frames coming so its result
        // appears without the user having to move the mouse.
        if self.app.in_flight > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }
}

/// The desktop backend: the local store, on a worker thread.
struct Store {
    to_worker: Sender<(Seq, Request)>,
    from_worker: Receiver<(Seq, Response)>,
    label: String,
}

impl Store {
    fn start(opts: SessionOptions, tracer: Tracer, clock: Clock) -> Store {
        let (to_worker, requests) = std::sync::mpsc::channel::<(Seq, Request)>();
        let (answers, from_worker) = std::sync::mpsc::channel::<(Seq, Response)>();

        let label = match &opts.account {
            Some(account) => format!("local store — {account}"),
            None => "local store".to_string(),
        };

        std::thread::spawn(move || {
            // One handle for the life of the window. Reads never contend with a
            // sync because this one is read-only (§4.1); the writes below open
            // their own.
            let mut workspace = match Workspace::open(&opts, tracer.clone(), clock.clone()) {
                Ok(w) => w,
                Err(e) => {
                    // Answer everything with the same failure rather than
                    // exiting silently: a window that never fills in and never
                    // says why is the worst outcome.
                    while let Ok((seq, request)) = requests.recv() {
                        let _ = answers.send((seq, failure(&request, &e.to_string())));
                    }
                    return;
                }
            };

            while let Ok((seq, request)) = requests.recv() {
                let response = serve(&mut workspace, &opts, &tracer, &clock, &request);
                if answers.send((seq, response)).is_err() {
                    break;
                }
            }
        });

        Store {
            to_worker,
            from_worker,
            label,
        }
    }
}

impl Backend for Store {
    fn send(&mut self, seq: Seq, request: Request) {
        let _ = self.to_worker.send((seq, request));
    }

    fn poll(&mut self) -> Option<(Seq, Response)> {
        self.from_worker.try_recv().ok()
    }

    fn label(&self) -> String {
        self.label.clone()
    }
}

/// Shapes an error as an answer to the question that was asked.
fn failure(request: &Request, message: &str) -> Response {
    match request {
        Request::List { .. } => Response::List(Err(message.to_string())),
        Request::Open { .. } => Response::Opened(Err(message.to_string())),
        Request::Folders => Response::Folders(Err(message.to_string())),
        Request::Sync => Response::Synced(Err(message.to_string())),
        _ => Response::Changed(Err(message.to_string())),
    }
}

fn serve(
    workspace: &mut Workspace,
    opts: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    request: &Request,
) -> Response {
    let mut answer = || -> Result<Response> {
        Ok(match request {
            Request::List { query, limit } => {
                let list_opts = ListOptions {
                    limit: *limit,
                    sort: if query.trim().is_empty() {
                        Sort::UnreadFirst
                    } else {
                        Sort::DateDesc
                    },
                    ..Default::default()
                };
                let list = if query.trim().is_empty() {
                    workspace.ls(None, &list_opts)?
                } else {
                    workspace.search(query, &list_opts)?
                };
                Response::List(Ok(Box::new(list)))
            }

            Request::Open { id } => {
                // The flattened text form, with its numbered links: the styled
                // Markdown the terminal draws needs a parser this interface does
                // not share yet, and showing half of one would be worse.
                let render = RenderOptions {
                    format: mailbox_api::BodyFormat::Text,
                    ..Default::default()
                };
                Response::Opened(Ok(Box::new(workspace.show(
                    &IdRef::parse(id),
                    &render,
                    false,
                )?)))
            }

            Request::Folders => Response::Folders(Ok(Box::new(workspace.folders()?))),

            Request::Flag { ids, add, remove } => {
                let mut session = writable(opts, tracer, clock)?;
                let spec = FlagSpec {
                    add_flags: add.clone(),
                    remove_flags: remove.clone(),
                    ..Default::default()
                };
                let written = session.flag(&refs(ids), &spec)?;
                Response::Changed(Ok(describe(written.len(), "changed")))
            }

            Request::Move { ids, folder } => {
                let mut session = writable(opts, tracer, clock)?;
                let written = session.move_to(&refs(ids), folder)?;
                Response::Changed(Ok(format!(
                    "{} to {folder}",
                    describe(written.len(), "moved")
                )))
            }

            Request::Snooze { ids, until } => {
                let mut session = writable(opts, tracer, clock)?;
                match until {
                    Some(until) => {
                        let when = mailbox_core::store::snooze::parse_until(until, clock)?;
                        let done = session.snooze(&refs(ids), when)?;
                        Response::Changed(Ok(format!(
                            "{} until {} — on {}",
                            describe(done.len(), "snoozed"),
                            mailbox_core::clock::format_rfc3339(when),
                            mailbox_core::paths::device_name(),
                        )))
                    }
                    None => {
                        let woke = session.unsnooze(&refs(ids))?;
                        Response::Changed(Ok(describe(woke.len(), "woken")))
                    }
                }
            }

            Request::Sync => {
                let mut writable_opts = opts.clone();
                writable_opts.read_only = false;
                let mut writer = Workspace::open(&writable_opts, tracer.clone(), clock.clone())?;
                let result = writer.sync(&Default::default())?;
                // The reading handle predates the sync, so it would show the
                // mailbox as it was. Reopen before answering.
                *workspace = Workspace::open(opts, tracer.clone(), clock.clone())?;
                Response::Synced(Ok(Box::new(result)))
            }
        })
    };

    match answer() {
        Ok(response) => response,
        Err(e) => failure(request, &e.to_string()),
    }
}

fn writable(opts: &SessionOptions, tracer: &Tracer, clock: &Clock) -> Result<Session> {
    let mut opts = opts.clone();
    opts.read_only = false;
    Session::open(&opts, tracer.clone(), clock.clone())
}

fn refs(ids: &[String]) -> Vec<IdRef> {
    ids.iter().map(|id| IdRef::parse(id)).collect()
}

fn describe(count: usize, verb: &str) -> String {
    format!(
        "{count} message{} {verb}",
        if count == 1 { "" } else { "s" }
    )
}
