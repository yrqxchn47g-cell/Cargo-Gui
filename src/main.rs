//! Cargo GUI — Iced 0.13 front-end for running cargo commands.
//!
//! ## Output-buffer design
//! Output lines are stored in a ring buffer (`VecDeque<String>`) capped at
//! `MAX_LINES`.  A dirty flag (`output_dirty`) is set on every new line; the
//! `text_editor::Content` is rebuilt only when the `FlushOutput` tick fires
//! (every 100 ms), avoiding O(n²) work when thousands of lines arrive rapidly.
//!
//! When the buffer is full the oldest line is discarded.  The *first* time this
//! happens in a given run a German-language notice (`TRIM_NOTICE`) is prepended
//! to the display (not stored in the buffer) so the cap is never exceeded.
//!
//! ## Cancellation design
//! A `tokio::sync::oneshot` channel is created for each run.  The `Sender` half
//! is kept in `App::stop_tx`; sending `()` on it tells the cargo task to kill
//! the child process and return early.
//!
//! ## Stale-message guard
//! Every run increments `current_job_id`.  `Append` and `Done` messages carry
//! the job id they were emitted for; messages whose id does not match the
//! current id are silently discarded.

use std::collections::VecDeque;
use std::time::Duration;

use futures::channel::mpsc;
use futures::FutureExt as _;
use futures::SinkExt as _;
use iced::widget::{button, column, row, text, text_editor, text_input};
use iced::{Element, Length, Subscription, Task};
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of lines kept in the output ring buffer.
const MAX_LINES: usize = 5000;

/// Notice prepended once per run/session when older lines have been discarded.
/// The message is intentionally in German to match the application locale.
const TRIM_NOTICE: &str =
    "⚠ Hinweis: ältere Ausgabe wurde verworfen – max. 5000 Zeilen";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> iced::Result {
    iced::application("Cargo GUI", App::update, App::view)
        .subscription(App::subscription)
        .run_with(App::new)
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

struct App {
    project_path: String,
    /// Cargo sub-command + arguments, e.g. `"build --release"`.
    cargo_args: String,

    /// Ring buffer of terminal output lines, capped at `MAX_LINES`.
    output_lines: VecDeque<String>,
    /// Rebuilt from `output_lines` on each `FlushOutput` tick (every 100 ms).
    output_content: text_editor::Content,
    /// Set whenever `output_lines` gains a new entry; cleared after flush.
    output_dirty: bool,
    /// True once the trim notice has been shown for the current run.
    output_trimmed: bool,

    running: bool,
    /// Incremented on each new run; stale `Append`/`Done` messages are dropped.
    current_job_id: u64,
    /// Send `()` here to request cancellation of the running process.
    stop_tx: Option<oneshot::Sender<()>>,

    status: String,
}

impl Default for App {
    fn default() -> Self {
        Self {
            project_path: String::new(),
            cargo_args: "build".to_string(),
            output_lines: VecDeque::new(),
            output_content: text_editor::Content::new(),
            output_dirty: false,
            output_trimmed: false,
            running: false,
            current_job_id: 0,
            stop_tx: None,
            status: "Bereit".to_string(),
        }
    }
}

impl App {
    fn new() -> (Self, Task<Msg>) {
        (Self::default(), Task::none())
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Msg {
    PathChanged(String),
    ArgsChanged(String),
    /// Start a new cargo run.
    Run,
    /// Request cancellation of the running cargo process.
    Stop,
    /// One output line from the cargo process (monochrome; no RGB processing).
    Append { line: String, job_id: u64 },
    /// The cargo process exited.
    Done { success: bool, job_id: u64 },
    /// Clear the output terminal and reset state.
    Clear,
    /// Periodic flush: rebuild `text_editor::Content` from the ring buffer if dirty.
    FlushOutput,
    /// Pass-through for text-editor interactions (selection / cursor movement).
    OutputAction(text_editor::Action),
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

impl App {
    fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::PathChanged(p) => {
                self.project_path = p;
                Task::none()
            }

            Msg::ArgsChanged(a) => {
                self.cargo_args = a;
                Task::none()
            }

            Msg::Run => {
                if self.running {
                    return Task::none();
                }
                // Increment job id so any in-flight messages from the previous
                // run are ignored.
                self.current_job_id += 1;
                // Reset the trim notice flag for this new run.
                self.output_trimmed = false;
                self.running = true;
                self.status = "Läuft…".to_string();

                let (stop_tx, stop_rx) = oneshot::channel::<()>();
                self.stop_tx = Some(stop_tx);

                run_cargo(
                    self.project_path.clone(),
                    self.cargo_args.clone(),
                    self.current_job_id,
                    stop_rx,
                )
            }

            Msg::Stop => {
                if let Some(tx) = self.stop_tx.take() {
                    let _ = tx.send(());
                }
                self.status = "Abbrechen…".to_string();
                Task::none()
            }

            Msg::Append { line, job_id } => {
                // Discard messages that belong to a previous run.
                if job_id != self.current_job_id {
                    return Task::none();
                }
                // Push to ring buffer; trim oldest entry when over cap.
                self.output_lines.push_back(line);
                if self.output_lines.len() > MAX_LINES {
                    self.output_lines.pop_front();
                    // On the first trim, flag so flush() prepends the notice.
                    self.output_trimmed = true;
                }
                self.output_dirty = true;
                Task::none()
            }

            Msg::Done { success, job_id } => {
                if job_id != self.current_job_id {
                    return Task::none();
                }
                self.running = false;
                self.stop_tx = None;
                self.status = if success {
                    "Fertig ✓".to_string()
                } else {
                    "Fehlgeschlagen ✗".to_string()
                };
                // Flush any remaining buffered lines immediately on completion.
                flush_output(self);
                Task::none()
            }

            Msg::Clear => {
                self.output_lines.clear();
                self.output_content = text_editor::Content::new();
                self.output_dirty = false;
                self.output_trimmed = false;
                self.status = "Bereit".to_string();
                Task::none()
            }

            Msg::FlushOutput => {
                if self.output_dirty {
                    flush_output(self);
                }
                Task::none()
            }

            Msg::OutputAction(action) => {
                // Allow selection and cursor movement but not editing.
                if !matches!(action, text_editor::Action::Edit(_)) {
                    self.output_content.perform(action);
                }
                Task::none()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

impl App {
    fn view(&self) -> Element<'_, Msg> {
        let path_input = text_input("Projektpfad…", &self.project_path)
            .on_input(Msg::PathChanged)
            .padding(5);

        let args_input = text_input("cargo-Befehl…", &self.cargo_args)
            .on_input(Msg::ArgsChanged)
            .width(180)
            .padding(5);

        // Run button — disabled while a process is running.
        let run_btn = button("▶ Ausführen")
            .on_press_maybe((!self.running).then_some(Msg::Run))
            .padding([5, 10]);

        // Stop button — enabled only while a process is running.
        let stop_btn = button("■ Stop")
            .on_press_maybe(self.running.then_some(Msg::Stop))
            .padding([5, 10]);

        let clear_btn = button("Löschen")
            .on_press(Msg::Clear)
            .padding([5, 10]);

        let controls = row![path_input, args_input, run_btn, stop_btn, clear_btn]
            .spacing(8)
            .padding(8);

        // Terminal output — kept as text_editor for selection/copy support.
        let output = text_editor(&self.output_content)
            .on_action(Msg::OutputAction)
            .height(Length::Fill);

        let status_bar = row![text(format!("Status: {}", self.status)).size(13)].padding(4);

        column![controls, output, status_bar]
            .spacing(4)
            .padding(8)
            .into()
    }
}

// ---------------------------------------------------------------------------
// Subscription
// ---------------------------------------------------------------------------

impl App {
    fn subscription(&self) -> Subscription<Msg> {
        // Periodic flush: rebuild the terminal display from the ring buffer
        // every 100 ms.  New lines only set `output_dirty`; this tick does
        // the (potentially expensive) `Content` rebuild at most 10 times/s.
        iced::time::every(Duration::from_millis(100)).map(|_| Msg::FlushOutput)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rebuild `text_editor::Content` from the ring buffer.
///
/// If trimming has occurred this run, `TRIM_NOTICE` is prepended to the
/// displayed text (it is not stored in the ring buffer, so it does not count
/// against `MAX_LINES`).
fn flush_output(app: &mut App) {
    let capacity = app.output_lines.len() + usize::from(app.output_trimmed);
    let mut parts: Vec<&str> = Vec::with_capacity(capacity);
    if app.output_trimmed {
        parts.push(TRIM_NOTICE);
    }
    for line in &app.output_lines {
        parts.push(line.as_str());
    }
    app.output_content = text_editor::Content::with_text(&parts.join("\n"));
    app.output_dirty = false;
}

/// Spawn a cargo process and return a [`Task`] that streams its output as
/// [`Msg::Append`] messages, followed by a single [`Msg::Done`].
///
/// `stop_rx`: receiving `()` kills the child process and sends
/// `Msg::Done { success: false, … }`.
fn run_cargo(
    path: String,
    args: String,
    job_id: u64,
    stop_rx: oneshot::Receiver<()>,
) -> Task<Msg> {
    use tokio::io::{AsyncBufReadExt, BufReader as AsyncBufReader};
    use tokio::process::Command;

    // Channel that forwards messages produced by the async task back to iced.
    let (mut tx, rx) = mpsc::channel::<Msg>(256);

    tokio::spawn(async move {
        let arg_parts: Vec<&str> = args.split_whitespace().collect();
        let working_dir = if path.is_empty() { ".".to_string() } else { path };

        let mut child = match Command::new("cargo")
            .args(&arg_parts)
            .current_dir(&working_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Kill the child automatically if this task is dropped.
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx
                    .send(Msg::Append {
                        line: format!("Fehler beim Starten von cargo: {e}"),
                        job_id,
                    })
                    .await;
                let _ = tx.send(Msg::Done { success: false, job_id }).await;
                return;
            }
        };

        let mut stdout_lines =
            AsyncBufReader::new(child.stdout.take().expect("stdout piped")).lines();
        let mut stderr_lines =
            AsyncBufReader::new(child.stderr.take().expect("stderr piped")).lines();

        let mut stdout_done = false;
        let mut stderr_done = false;

        // Fuse the cancellation future so that after it resolves once (whether
        // via an explicit send() or a sender drop) it will not be polled again.
        // A sender drop without send() is NOT treated as a cancellation request.
        let mut stop_rx = stop_rx.fuse();

        loop {
            if stdout_done && stderr_done {
                break;
            }

            tokio::select! {
                // Match only the success case (explicit Stop request).
                Ok(()) = &mut stop_rx => {
                    // Kill the child process on explicit stop.
                    let _ = child.kill().await;
                    let _ = tx
                        .send(Msg::Append {
                            line: "⚠ Prozess abgebrochen.".to_string(),
                            job_id,
                        })
                        .await;
                    let _ = tx.send(Msg::Done { success: false, job_id }).await;
                    return;
                }

                maybe = stdout_lines.next_line(), if !stdout_done => {
                    match maybe {
                        Ok(Some(line)) => {
                            let _ = tx.send(Msg::Append { line, job_id }).await;
                        }
                        _ => stdout_done = true,
                    }
                }

                maybe = stderr_lines.next_line(), if !stderr_done => {
                    match maybe {
                        Ok(Some(line)) => {
                            let _ = tx.send(Msg::Append { line, job_id }).await;
                        }
                        _ => stderr_done = true,
                    }
                }
            }
        }

        let success = child.wait().await.map(|s| s.success()).unwrap_or(false);
        let _ = tx.send(Msg::Done { success, job_id }).await;
    });

    // Stream the mpsc receiver as a Task so iced processes each message.
    Task::stream(rx)
}
