//! Cargo GUI — Iced 0.13 front-end for running cargo commands.
//!
//! ## Layout
//! The main view consists of:
//! - A topbar with a menu/hamburger icon and the app title.
//! - A project directory row with path input + "Durchsuchen" (folder picker via rfd)
//!   and "Als Start" (set working directory) buttons.
//! - An arguments row with a label and text input.
//! - A "Neues Projekt" row for running `cargo new`.
//! - A "Cargo Befehle" 2-column grid of one-click cargo command buttons.
//! - An "Ausgabe" terminal panel (ring-buffered `text_editor`) + "Ausgabe löschen".
//! - A footer with Einstellungen / Editor / Hilfe / Beenden.
//!
//! ## Multi-view navigation
//! `App::current_view` selects between Main / Settings / Editor / Help.
//!
//! ## Output-buffer design
//! Output lines are stored in a ring buffer (`VecDeque<String>`) capped at
//! `MAX_LINES`.  A dirty flag (`output_dirty`) is set on every new line; the
//! `text_editor::Content` is rebuilt only when the `FlushOutput` tick fires
//! (every 100 ms), avoiding O(n²) work when thousands of lines arrive rapidly.
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
use iced::widget::{
    button, column, container, row, scrollable, text, text_editor, text_input, tooltip,
};
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

/// Cargo commands shown in the left column of the "Cargo Befehle" grid.
const COMMANDS_LEFT: &[(&str, &str)] = &[
    ("Build", "build"),
    ("Run", "run"),
    ("Test", "test"),
    ("Check", "check"),
    ("Fmt", "fmt"),
    ("Clippy", "clippy"),
];

/// Cargo commands shown in the right column of the "Cargo Befehle" grid.
const COMMANDS_RIGHT: &[(&str, &str)] = &[
    ("Update", "update"),
    ("New", "new"),
    ("Init", "init"),
    ("Clean", "clean"),
    ("Doc", "doc"),
    ("Bench", "bench"),
];

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// Top-level navigation state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum View {
    Main,
    Settings,
    Editor,
    Help,
}

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
    /// Active navigation view.
    current_view: View,

    // --- Main view state ---
    project_path: String,
    /// Cargo sub-command + arguments, e.g. `"build --release"`.
    cargo_args: String,
    /// Name for `cargo new <name>`.
    new_project_name: String,

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

    // --- Editor view state ---
    editor_content: text_editor::Content,

    // --- Settings view state ---
    /// User-defined default project path restored on "Als Start".
    default_path: String,
}

impl Default for App {
    fn default() -> Self {
        Self {
            current_view: View::Main,
            project_path: String::new(),
            cargo_args: "build".to_string(),
            new_project_name: String::new(),
            output_lines: VecDeque::new(),
            output_content: text_editor::Content::new(),
            output_dirty: false,
            output_trimmed: false,
            running: false,
            current_job_id: 0,
            stop_tx: None,
            status: "Bereit".to_string(),
            editor_content: text_editor::Content::new(),
            default_path: String::new(),
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
    // --- Navigation ---
    NavigateTo(View),

    // --- Main view ---
    PathChanged(String),
    ArgsChanged(String),
    /// Open a native folder-picker dialog (rfd) to choose the project path.
    BrowsePath,
    /// Folder chosen via rfd dialog; `None` means the dialog was cancelled.
    PathPicked(Option<String>),
    /// Set `project_path` as the new `default_path`.
    SetAsDefault,
    /// Restore `project_path` from `default_path`.
    RestoreDefault,

    /// Name typed into the "cargo new" input.
    NewProjectNameChanged(String),
    /// Run `cargo new <new_project_name>` in `project_path`.
    RunCargoNew,

    /// One-click: set `cargo_args` to `cmd` then immediately run.
    RunCommand(String),

    /// Start a new cargo run (using `cargo_args`).
    Run,
    /// Request cancellation of the running cargo process.
    Stop,
    /// One output line from the cargo process.
    Append { line: String, job_id: u64 },
    /// The cargo process exited.
    Done { success: bool, job_id: u64 },
    /// Clear the output terminal and reset state.
    Clear,
    /// Periodic flush: rebuild `text_editor::Content` from the ring buffer if dirty.
    FlushOutput,
    /// Pass-through for the output text-editor.
    OutputAction(text_editor::Action),

    // --- Editor view ---
    EditorAction(text_editor::Action),

    // --- Settings view ---
    DefaultPathChanged(String),
    /// Confirm and persist the default path with a status notification.
    SaveSettings,

    // --- App ---
    Quit,
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

impl App {
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            // --- Navigation ---
            Msg::NavigateTo(view) => {
                self.current_view = view;
                Task::none()
            }

            // --- Path / project ---
            Msg::PathChanged(p) => {
                self.project_path = p;
                Task::none()
            }

            Msg::ArgsChanged(a) => {
                self.cargo_args = a;
                Task::none()
            }

            Msg::BrowsePath => Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .pick_folder()
                        .await
                        .map(|h| h.path().to_string_lossy().into_owned())
                },
                Msg::PathPicked,
            ),

            Msg::PathPicked(maybe) => {
                if let Some(p) = maybe {
                    self.project_path = p;
                }
                Task::none()
            }

            Msg::SetAsDefault => {
                self.default_path = self.project_path.clone();
                self.status = "Startpfad gesetzt".to_string();
                Task::none()
            }

            Msg::RestoreDefault => {
                if !self.default_path.is_empty() {
                    self.project_path = self.default_path.clone();
                }
                Task::none()
            }

            Msg::NewProjectNameChanged(n) => {
                self.new_project_name = n;
                Task::none()
            }

            Msg::RunCargoNew => {
                if self.running || self.new_project_name.trim().is_empty() {
                    return Task::none();
                }
                let new_args = format!("new {}", self.new_project_name.trim());
                self.cargo_args = new_args;
                self.update(Msg::Run)
            }

            Msg::RunCommand(cmd) => {
                if self.running {
                    return Task::none();
                }
                self.cargo_args = cmd;
                self.update(Msg::Run)
            }

            // --- Run / Stop ---
            Msg::Run => {
                if self.running {
                    return Task::none();
                }
                self.current_job_id += 1;
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

            // --- Output streaming ---
            Msg::Append { line, job_id } => {
                if job_id != self.current_job_id {
                    return Task::none();
                }
                self.output_lines.push_back(line);
                if self.output_lines.len() > MAX_LINES {
                    self.output_lines.pop_front();
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
                if !matches!(action, text_editor::Action::Edit(_)) {
                    self.output_content.perform(action);
                }
                Task::none()
            }

            // --- Editor ---
            Msg::EditorAction(action) => {
                self.editor_content.perform(action);
                Task::none()
            }

            // --- Settings ---
            Msg::DefaultPathChanged(p) => {
                self.default_path = p;
                Task::none()
            }

            Msg::SaveSettings => {
                self.status = "Einstellungen gespeichert ✓".to_string();
                Task::none()
            }

            // --- App ---
            Msg::Quit => iced::exit(),
        }
    }
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

impl App {
    fn view(&self) -> Element<'_, Msg> {
        let body: Element<'_, Msg> = match self.current_view {
            View::Main => self.view_main(),
            View::Settings => self.view_settings(),
            View::Editor => self.view_editor(),
            View::Help => self.view_help(),
        };

        let topbar = self.view_topbar();
        let footer = self.view_footer();

        column![topbar, body, footer].into()
    }

    // -----------------------------------------------------------------------
    // Topbar
    // -----------------------------------------------------------------------

    fn view_topbar(&self) -> Element<'_, Msg> {
        let menu_btn = tooltip(
            button("☰").padding([4, 10]),
            "Menü",
            tooltip::Position::Bottom,
        );

        let title = text("Cargo GUI").size(20);

        container(
            row![menu_btn, title]
                .spacing(10)
                .align_y(iced::Alignment::Center)
                .padding([6, 10]),
        )
        .style(container::bordered_box)
        .width(Length::Fill)
        .into()
    }

    // -----------------------------------------------------------------------
    // Footer
    // -----------------------------------------------------------------------

    fn view_footer(&self) -> Element<'_, Msg> {
        let settings_btn = tooltip(
            button("⚙ Einstellungen")
                .on_press(Msg::NavigateTo(View::Settings))
                .padding([5, 10]),
            "Einstellungen öffnen",
            tooltip::Position::Top,
        );

        let editor_btn = tooltip(
            button("✏ Editor")
                .on_press(Msg::NavigateTo(View::Editor))
                .padding([5, 10]),
            "Datei-Editor öffnen",
            tooltip::Position::Top,
        );

        let help_btn = tooltip(
            button("? Hilfe")
                .on_press(Msg::NavigateTo(View::Help))
                .padding([5, 10]),
            "Bedienungsanleitung öffnen",
            tooltip::Position::Top,
        );

        let quit_btn = tooltip(
            button("✕ Beenden")
                .on_press(Msg::Quit)
                .padding([5, 10]),
            "Anwendung beenden",
            tooltip::Position::Top,
        );

        let status_text = text(format!("Status: {}", self.status)).size(13);

        container(
            row![settings_btn, editor_btn, help_btn, quit_btn, status_text]
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .padding([6, 10]),
        )
        .style(container::bordered_box)
        .width(Length::Fill)
        .into()
    }

    // -----------------------------------------------------------------------
    // Main view
    // -----------------------------------------------------------------------

    fn view_main(&self) -> Element<'_, Msg> {
        // -- Project directory row --
        let path_input = text_input("Projektpfad…", &self.project_path)
            .on_input(Msg::PathChanged)
            .padding(5);

        let browse_btn = tooltip(
            button("📂 Durchsuchen")
                .on_press(Msg::BrowsePath)
                .padding([5, 10]),
            "Projektordner auswählen",
            tooltip::Position::Bottom,
        );

        let set_default_btn = tooltip(
            button("Als Start")
                .on_press(Msg::SetAsDefault)
                .padding([5, 10]),
            "Diesen Pfad als Standardpfad speichern",
            tooltip::Position::Bottom,
        );

        let path_row = row![
            text("Projektverzeichnis:").size(13).width(150),
            path_input,
            browse_btn,
            set_default_btn,
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .padding([4, 8]);

        // -- Arguments row --
        let args_input = text_input("z.B. build --release", &self.cargo_args)
            .on_input(Msg::ArgsChanged)
            .on_submit(Msg::Run)
            .padding(5);

        let run_btn = tooltip(
            button("▶ Ausführen")
                .on_press_maybe((!self.running).then_some(Msg::Run))
                .padding([5, 10]),
            "Cargo-Befehl ausführen",
            tooltip::Position::Bottom,
        );

        let stop_btn = tooltip(
            button("■ Stop")
                .on_press_maybe(self.running.then_some(Msg::Stop))
                .padding([5, 10]),
            "Laufenden Prozess abbrechen",
            tooltip::Position::Bottom,
        );

        let args_row = row![
            text("Argumente:").size(13).width(150),
            args_input,
            run_btn,
            stop_btn,
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .padding([4, 8]);

        // -- New project row --
        let new_name_input = text_input("Projektname…", &self.new_project_name)
            .on_input(Msg::NewProjectNameChanged)
            .on_submit(Msg::RunCargoNew)
            .padding(5);

        let cargo_new_btn = tooltip(
            button("cargo new")
                .on_press_maybe(
                    (!self.running && !self.new_project_name.trim().is_empty())
                        .then_some(Msg::RunCargoNew),
                )
                .padding([5, 10]),
            "Neues Cargo-Projekt anlegen",
            tooltip::Position::Bottom,
        );

        let new_row = row![
            text("Neues Projekt:").size(13).width(150),
            new_name_input,
            cargo_new_btn,
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .padding([4, 8]);

        // -- Cargo Befehle grid (2 columns) --
        let left_col = column(
            COMMANDS_LEFT
                .iter()
                .map(|(label, cmd)| {
                    let cmd_str = cmd.to_string();
                    tooltip(
                        button(*label)
                            .on_press_maybe(
                                (!self.running).then_some(Msg::RunCommand(cmd_str)),
                            )
                            .width(110)
                            .padding([5, 8]),
                        text(format!("cargo {cmd}")),
                        tooltip::Position::Right,
                    )
                    .into()
                })
                .collect::<Vec<_>>(),
        )
        .spacing(4);

        let right_col = column(
            COMMANDS_RIGHT
                .iter()
                .map(|(label, cmd)| {
                    let cmd_str = cmd.to_string();
                    tooltip(
                        button(*label)
                            .on_press_maybe(
                                (!self.running).then_some(Msg::RunCommand(cmd_str)),
                            )
                            .width(110)
                            .padding([5, 8]),
                        text(format!("cargo {cmd}")),
                        tooltip::Position::Right,
                    )
                    .into()
                })
                .collect::<Vec<_>>(),
        )
        .spacing(4);

        let commands_grid = row![left_col, right_col].spacing(8);

        let commands_section = column![
            text("Cargo Befehle").size(15),
            commands_grid,
        ]
        .spacing(6)
        .padding([4, 8]);

        // -- Output section --
        let clear_btn = tooltip(
            button("Ausgabe löschen")
                .on_press(Msg::Clear)
                .padding([5, 10]),
            "Ausgabe leeren und Status zurücksetzen",
            tooltip::Position::Bottom,
        );

        let output_header = row![
            text("Ausgabe").size(15),
            clear_btn,
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let output = text_editor(&self.output_content)
            .on_action(Msg::OutputAction)
            .height(Length::Fill);

        let output_section = column![output_header, output]
            .spacing(4)
            .padding([4, 8]);

        // -- Layout: left side has inputs + commands; right side is larger output --
        let left_panel = scrollable(
            column![path_row, args_row, new_row, commands_section]
                .spacing(4)
                .width(420),
        );

        let main_content = row![left_panel, output_section]
            .spacing(8)
            .padding(8)
            .height(Length::Fill);

        main_content.into()
    }

    // -----------------------------------------------------------------------
    // Settings view
    // -----------------------------------------------------------------------

    fn view_settings(&self) -> Element<'_, Msg> {
        let back_btn = button("← Zurück")
            .on_press(Msg::NavigateTo(View::Main))
            .padding([5, 10]);

        let default_path_input =
            text_input("Standard-Projektpfad…", &self.default_path)
                .on_input(Msg::DefaultPathChanged)
                .padding(5);

        let save_btn = tooltip(
            button("Speichern")
                .on_press(Msg::SaveSettings)
                .padding([5, 10]),
            "Einstellungen übernehmen",
            tooltip::Position::Bottom,
        );

        let default_path_row = row![
            text("Standard-Pfad:").size(13).width(160),
            default_path_input,
            save_btn,
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);

        let restore_btn = tooltip(
            button("Standard-Pfad laden")
                .on_press(Msg::RestoreDefault)
                .padding([5, 10]),
            "Standard-Pfad in das Projektverzeichnis-Feld laden",
            tooltip::Position::Bottom,
        );

        column![
            row![back_btn, text("Einstellungen").size(18)].spacing(10),
            default_path_row,
            restore_btn,
        ]
        .spacing(12)
        .padding(16)
        .height(Length::Fill)
        .into()
    }

    // -----------------------------------------------------------------------
    // Editor view
    // -----------------------------------------------------------------------

    fn view_editor(&self) -> Element<'_, Msg> {
        let back_btn = button("← Zurück")
            .on_press(Msg::NavigateTo(View::Main))
            .padding([5, 10]);

        let editor = text_editor(&self.editor_content)
            .on_action(Msg::EditorAction)
            .height(Length::Fill);

        column![
            row![back_btn, text("Editor").size(18)].spacing(10),
            editor,
        ]
        .spacing(8)
        .padding(16)
        .height(Length::Fill)
        .into()
    }

    // -----------------------------------------------------------------------
    // Help view
    // -----------------------------------------------------------------------

    fn view_help(&self) -> Element<'_, Msg> {
        let back_btn = button("← Zurück")
            .on_press(Msg::NavigateTo(View::Main))
            .padding([5, 10]);

        let help_text = text(HELP_TEXT).size(13);

        column![
            row![back_btn, text("Hilfe / Bedienungsanleitung").size(18)].spacing(10),
            scrollable(help_text).height(Length::Fill),
        ]
        .spacing(8)
        .padding(16)
        .height(Length::Fill)
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

    let (mut tx, rx) = mpsc::channel::<Msg>(256);

    tokio::spawn(async move {
        let arg_parts: Vec<&str> = args.split_whitespace().collect();
        let working_dir = if path.is_empty() { ".".to_string() } else { path };

        let mut child = match Command::new("cargo")
            .args(&arg_parts)
            .current_dir(&working_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
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

        let mut stop_rx = stop_rx.fuse();

        loop {
            if stdout_done && stderr_done {
                break;
            }

            tokio::select! {
                Ok(()) = &mut stop_rx => {
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

    Task::stream(rx)
}

// ---------------------------------------------------------------------------
// Help text
// ---------------------------------------------------------------------------

const HELP_TEXT: &str = "\
Cargo GUI — Bedienungsanleitung
================================

Cargo GUI ist eine grafische Benutzeroberfläche für den Rust-Paketmanager Cargo.

## Projektverzeichnis
Geben Sie den Pfad zu Ihrem Rust-Projekt ein oder klicken Sie auf \"📂 Durchsuchen\",
um einen Ordner auszuwählen. Mit \"Als Start\" speichern Sie den Pfad als Standard.

## Argumente
Tragen Sie den gewünschten Cargo-Befehl ein, z.B.:
  build --release
  test -- --nocapture
  run -- arg1 arg2

## Cargo Befehle (Schnellzugriff)
Die Schaltflächen führen den jeweiligen Cargo-Befehl direkt aus:
  Build    — Kompiliert das Projekt (cargo build)
  Run      — Kompiliert und startet das Projekt (cargo run)
  Test     — Führt alle Tests aus (cargo test)
  Check    — Prüft Syntax ohne Kompilierung (cargo check)
  Fmt      — Formatiert den Quellcode (cargo fmt)
  Clippy   — Führt den Linter aus (cargo clippy)
  Update   — Aktualisiert Abhängigkeiten (cargo update)
  New      — Neues Projekt anlegen (cargo new)
  Init     — Aktuelles Verzeichnis initialisieren (cargo init)
  Clean    — Build-Artefakte löschen (cargo clean)
  Doc      — Dokumentation generieren (cargo doc)
  Bench    — Benchmarks ausführen (cargo bench)

## Neues Projekt
Geben Sie einen Projektnamen ein und klicken Sie auf \"cargo new\", um ein
neues Rust-Projekt im ausgewählten Verzeichnis anzulegen.

## Ausgabe
Die Ausgabe des letzten Cargo-Laufs wird hier angezeigt. Sie können Text
selektieren und kopieren. Mit \"Ausgabe löschen\" wird die Ausgabe zurückgesetzt.

## Stop
Während ein Cargo-Prozess läuft, können Sie ihn mit \"■ Stop\" abbrechen.

## Einstellungen
Unter \"⚙ Einstellungen\" können Sie den Standard-Projektpfad festlegen.

## Editor
Unter \"✏ Editor\" steht ein einfacher Texteditor zur Verfügung.
";

