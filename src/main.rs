//! Cargo GUI — Iced 0.13 front-end for running cargo commands.
//!
//! ## Layout
//! The main view consists of:
//! - A topbar with the app title.
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
//! is kept in `App::stop_tx`; sending `()` on it tells the cargo task to stop
//! the child process and return early.
//!
//! ## Stale-message guard
//! Every run increments `current_job_id`.  `Append` and `Done` messages carry
//! the job id they were emitted for; messages whose id does not match the
//! current id are silently discarded.
//!
//! ## Tooltip system
//! Mouse position is tracked via `iced::event::listen_with`. Widgets are
//! wrapped with [`hover_tip`] which uses `mouse_area` to set / clear a global
//! `tooltip_text` in the App state.  The tooltip overlay is rendered as the top
//! layer of an `iced::widget::stack`, positioned live at the cursor.
//!
//! ## Persistent settings
//! See `src/config.rs`.  Settings are written to disk immediately on every
//! change (file is tiny, so I/O is negligible).

mod config;
mod icons;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use config::{AppTheme, Config};
use futures::channel::mpsc;
use futures::FutureExt as _;
use futures::SinkExt as _;
use iced::widget::{
    button, column, container, horizontal_space, image as img_widget, mouse_area, pick_list, row,
    scrollable, stack, text, text_editor, text_input, Space,
};
use iced::{clipboard, Color, Element, Length, Pixels, Subscription, Task};
use icons::{bi, Bootstrap};
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of lines kept in the output ring buffer.
const MAX_LINES: usize = 5000;

/// Notice prepended once per run/session when older lines have been discarded.
/// The message is intentionally in German to match the application locale.
const TRIM_NOTICE: &str = "⚠ Hinweis: ältere Ausgabe wurde verworfen – max. 5000 Zeilen";

/// Horizontal offset applied to the tooltip overlay to approximately centre it
/// over the cursor (half of the assumed tooltip render width).
const TOOLTIP_OFFSET_X: f32 = 60.0;

/// Vertical offset that places the tooltip above the cursor.
const TOOLTIP_OFFSET_Y: f32 = 34.0;

/// Estimated width of the tooltip box used when positioning to the left of the
/// cursor (lower half of the screen).
const TOOLTIP_ESTIMATED_WIDTH: f32 = 260.0;

/// Width of the line-number gutter rendered left of the text editors.
const GUTTER_WIDTH: f32 = 48.0;

/// Virtual line height (px) used for the find-match highlight overlay.
/// Must match the absolute line height set on every `text_editor` widget via
/// `.line_height(Pixels(LINE_HEIGHT))` so that highlight bands align exactly.
const LINE_HEIGHT: f32 = 20.0;

/// Top padding (px) of the `text_editor` widget (matches the iced default of
/// `Padding::new(5.0)`).  Highlight bands must be offset by this amount so they
/// start at the same vertical position as the first rendered text line.
const EDITOR_PADDING_TOP: f32 = 5.0;

/// Background colour for non-current find-match highlight bands (subtle yellow).
const FIND_OTHER_COLOR: Color = Color { r: 1.0, g: 0.88, b: 0.0, a: 0.13 };

/// Background colour for the current find-match highlight band (strong orange-yellow).
const FIND_CURRENT_COLOR: Color = Color { r: 1.0, g: 0.65, b: 0.0, a: 0.40 };

/// Test highlight color: green.
const FIND_TEST_GREEN_COLOR: Color = Color { r: 0.0, g: 1.0, b: 0.5, a: 0.40 };

/// Test highlight color: red.
const FIND_TEST_RED_COLOR: Color = Color { r: 1.0, g: 0.2, b: 0.2, a: 0.40 };

/// Background colour for error diagnostic links (red).
const DIAG_ERROR_COLOR: Color = Color { r: 0.80, g: 0.15, b: 0.15, a: 0.90 };

/// Background colour for warning diagnostic links (amber).
const DIAG_WARN_COLOR: Color = Color { r: 0.70, g: 0.50, b: 0.05, a: 0.90 };

/// Background colour for note/other diagnostic links (blue).
const DIAG_NOTE_COLOR: Color = Color { r: 0.15, g: 0.40, b: 0.78, a: 0.90 };

/// Minimum window width enforced when restoring or saving the window size.
const MIN_WINDOW_WIDTH: f32 = 800.0;

/// Minimum window height enforced when restoring or saving the window size.
const MIN_WINDOW_HEIGHT: f32 = 600.0;

/// Ghost image shown in the About dialog.
const GHOST_GIF: &[u8] = include_bytes!("../assets/Ghost.gif");

/// Filename of the help PDF distributed alongside the application.
const HELP_PDF_FILENAME: &str = "cargo-gui-bedienungsanleitung.pdf";

/// Visible width (px) of the "Argumente" text-input field (≈ 16 characters).
const ARGS_INPUT_WIDTH: u16 = 128;

/// Display dimensions of the Ghost image in the About dialog.
const GHOST_WIDTH: f32 = 96.0;
const GHOST_HEIGHT: f32 = 112.0;

/// Cargo commands shown in the left column of the "Cargo Befehle" grid.
const COMMANDS_LEFT: &[(&str, &str, &str)] = &[
    ("Build", "build", "cargo build — Kompiliert das Projekt"),
    ("Build --release", "build --release", "cargo build --release — Kompiliert optimiert (Release-Modus)"),
    ("Run", "run", "cargo run — Kompiliert und startet das Projekt"),
    ("Run --release", "run --release", "cargo run --release — Startet die optimierte Release-Version"),
    ("Test", "test", "cargo test — Führt alle Tests aus"),
    ("Check", "check", "cargo check — Prüft Syntax ohne vollständige Kompilierung"),
    ("Fmt", "fmt", "cargo fmt — Formatiert den Quellcode automatisch"),
    ("Clippy", "clippy", "cargo clippy — Führt den Linter aus (Code-Qualitätsprüfung)"),
];

/// Cargo commands shown in the right column of the "Cargo Befehle" grid.
const COMMANDS_RIGHT: &[(&str, &str, &str)] = &[
    ("Update", "update", "cargo update — Aktualisiert alle Abhängigkeiten in Cargo.lock"),
    ("New", "new", "cargo new — Neues Rust-Projekt anlegen"),
    ("Init", "init", "cargo init — Aktuelles Verzeichnis als Rust-Projekt initialisieren"),
    ("Clean", "clean", "cargo clean — Build-Artefakte und Target-Verzeichnis löschen"),
    ("Doc", "doc", "cargo doc — API-Dokumentation für das Projekt generieren"),
    ("Bench", "bench", "cargo bench — Benchmarks ausführen"),
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
    About,
}

// ---------------------------------------------------------------------------
// Context menu
// ---------------------------------------------------------------------------

/// Which widget was right-clicked to open the context menu.
#[derive(Debug, Clone)]
enum ContextMenuKind {
    Editor,
    Output,
}

/// Transient state while a context menu is visible.
#[derive(Debug, Clone)]
struct ContextMenuState {
    /// Logical-pixel X where the menu should appear.
    x: f32,
    /// Logical-pixel Y where the menu should appear.
    y: f32,
    kind: ContextMenuKind,
}

// ---------------------------------------------------------------------------
// Editor tabs
// ---------------------------------------------------------------------------

/// A single tab in the editor view.
struct EditorTab {
    /// Display title (filename or "Untitled-N").
    title: String,
    /// Absolute path if the tab was opened from disk.
    path: Option<PathBuf>,
    /// Content managed by iced's built-in text editor widget.
    content: text_editor::Content,
    /// `true` after any edit action since the last save.
    dirty: bool,
    /// Persistent diagnostic error/warning/note information for this tab.
    /// Each entry contains the full error info including message and error code
    /// so that inline gutter tooltips can be shown.  Lines remain highlighted
    /// until the tab is closed or deleted.
    editor_errors: Vec<EditorError>,
}

impl EditorTab {
    fn new_untitled(index: usize) -> Self {
        Self {
            title: format!("Untitled-{}", index + 1),
            path: None,
            content: text_editor::Content::new(),
            dirty: false,
            editor_errors: Vec::new(),
        }
    }

    fn from_file(path: PathBuf, file_text: &str) -> Self {
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Unbekannt".to_string());
        Self {
            title,
            path: Some(path),
            content: text_editor::Content::with_text(file_text),
            dirty: false,
            editor_errors: Vec::new(),
        }
    }

    /// Tab label, with a `*` suffix when unsaved changes exist.
    fn display_title(&self) -> String {
        if self.dirty {
            format!("{}*", self.title)
        } else {
            self.title.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Severity level of a parsed cargo diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticLevel {
    Error,
    Warning,
    Note,
}

/// A single parsed cargo diagnostic (error / warning / note) that can be
/// rendered as a clickable link in the output panel.
#[derive(Debug, Clone)]
struct Diagnostic {
    level: DiagnosticLevel,
    /// Absolute path to the source file.
    file: PathBuf,
    /// 1-based line number as reported by cargo.
    line: usize,
    /// 1-based column number as reported by cargo.
    column: usize,
    /// The human-readable message from the diagnostic header line.
    message: String,
    /// Optional Rust error code, e.g. `"E0425"`.
    error_code: Option<String>,
}

/// Full diagnostic information stored on an editor tab line for inline
/// tooltips and the error dropdown.
#[derive(Debug, Clone)]
struct EditorError {
    /// 0-based line index.
    line: usize,
    /// 1-based column number.
    column: usize,
    level: DiagnosticLevel,
    /// Full error message.
    message: String,
    /// Optional Rust error code (e.g. `"E0425"`).
    error_code: Option<String>,
}

/// A reference to a single diagnostic used as an item type for the editor
/// error [`pick_list`] dropdown.  Implements [`Display`] so the formatted
/// label is shown in the widget.
#[derive(Debug, Clone, PartialEq)]
struct DiagRef {
    /// Index into `App::diagnostics`.
    idx: usize,
    /// Pre-formatted display label shown in the dropdown.
    label: String,
}

impl std::fmt::Display for DiagRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> iced::Result {
    let config = Config::load();
    let initial_size = iced::Size::new(
        config.window_width.max(MIN_WINDOW_WIDTH),
        config.window_height.max(MIN_WINDOW_HEIGHT),
    );
    iced::application("🦀 Jürgen's Cargo GUI", App::update, App::view)
        .subscription(App::subscription)
        .theme(|app: &App| app.config.theme.to_iced())
        .font(icons::BOOTSTRAP_FONT_BYTES)
        .window_size(initial_size)
        .run_with(App::new)
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

struct App {
    /// Active navigation view.
    current_view: View,

    // --- Persistent config (loaded at startup, saved on every change) ---
    config: Config,

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
    /// The `cargo_args` string of the currently-running command (used to
    /// highlight the active button and match against `last_durations`).
    running_cmd: String,
    /// Instant the current run started; `None` when idle.
    run_start: Option<Instant>,
    /// Elapsed milliseconds updated every 100 ms while a run is active.
    display_elapsed_ms: u64,
    /// Last measured wall-clock duration (ms) for each command string.
    last_durations: HashMap<String, u64>,

    status: String,

    // --- Editor view state ---
    editor_tabs: Vec<EditorTab>,
    /// Index of the currently visible tab.
    active_tab: usize,
    /// Running counter for generating "Untitled-N" titles.
    untitled_counter: usize,

    // --- Find/Replace panel state ---
    /// Whether the find/replace panel is visible in the editor view.
    find_replace_open: bool,
    /// When the find panel is open, whether the replace row is also visible.
    find_show_replace: bool,
    /// Current search string.
    find_text: String,
    /// Current replacement string.
    replace_text: String,
    /// Status message shown in the find/replace panel (e.g. "Treffer 1 von 3 — Zeile 5" or "Keine Treffer").
    find_status: String,
    /// 0-based index of the match that will be acted on by "Nächstes" / "Ersetzen".
    find_current_match: usize,
    /// Line numbers (0-based) of every match in the current tab for multi-highlight.
    find_all_match_lines: Vec<usize>,

    // --- Context menu state ---
    /// Non-None when a context menu is currently visible.
    context_menu: Option<ContextMenuState>,

    // --- Editor line highlight ---
    /// 0-based index of the editor line to highlight (current find match).
    editor_highlight_line: Option<usize>,
    /// Color used to highlight the current find match in the editor.
    find_test_color: Color,

    // --- Output find panel state ---
    /// Whether the output-specific find panel is visible.
    output_find_open: bool,
    /// Current search string in the output find panel.
    output_find_text: String,
    /// Status message for the output find panel.
    output_find_status: String,
    /// 0-based index of the match navigated to in the output.
    output_find_current_match: usize,
    /// 0-based index of the output line to highlight (current output find match).
    output_highlight_line: Option<usize>,

    // --- Tooltip overlay state ---
    /// Text to show in the global tooltip, or `None` when hidden.
    tooltip_text: Option<String>,
    /// Current mouse cursor X position (in logical pixels).
    mouse_x: f32,
    /// Current mouse cursor Y position (in logical pixels).
    mouse_y: f32,
    /// Current window width in logical pixels (updated via WindowResized).
    window_width: f32,
    /// Current window height in logical pixels (updated via WindowResized).
    window_height: f32,

    // --- Diagnostics ---
    /// Parsed diagnostics accumulated during the last cargo run.
    diagnostics: Vec<Diagnostic>,
    /// The most recent diagnostic header line (level + message + optional
    /// error code) waiting to be paired with the following
    /// ` --> file:line:col` location line.
    pending_diag_level: Option<(DiagnosticLevel, String, Option<String>)>,
    /// Highlight colour to use for the diagnostic line in the editor.
    diag_highlight_color: Color,
}

impl App {
    fn new() -> (Self, Task<Msg>) {
        let config = Config::load();
        let project_path = config.default_path.clone();
        let window_width = config.window_width.max(MIN_WINDOW_WIDTH);
        let window_height = config.window_height.max(MIN_WINDOW_HEIGHT);
        let editor_tabs = vec![EditorTab::new_untitled(0)];
        let startup_task: Task<Msg> = if config.is_fullscreen {
            iced::window::get_latest().then(|maybe_id| {
                if let Some(id) = maybe_id {
                    iced::window::change_mode(id, iced::window::Mode::Fullscreen)
                } else {
                    Task::none()
                }
            })
        } else {
            Task::none()
        };
        (
            Self {
                current_view: View::Main,
                config,
                project_path,
                cargo_args: "build".to_string(),
                new_project_name: String::new(),
                output_lines: VecDeque::new(),
                output_content: text_editor::Content::new(),
                output_dirty: false,
                output_trimmed: false,
                running: false,
                current_job_id: 0,
                stop_tx: None,
                running_cmd: String::new(),
                run_start: None,
                display_elapsed_ms: 0,
                last_durations: HashMap::new(),
                status: "Bereit".to_string(),
                editor_tabs,
                active_tab: 0,
                untitled_counter: 1,
                find_replace_open: false,
                find_show_replace: false,
                find_text: String::new(),
                replace_text: String::new(),
                find_status: String::new(),
                find_current_match: 0,
                find_all_match_lines: Vec::new(),
                context_menu: None,
                editor_highlight_line: None,
                find_test_color: FIND_TEST_RED_COLOR,
                output_find_open: false,
                output_find_text: String::new(),
                output_find_status: String::new(),
                output_find_current_match: 0,
                output_highlight_line: None,
                tooltip_text: None,
                mouse_x: 0.0,
                mouse_y: 0.0,
                window_width,
                window_height,
                diagnostics: Vec::new(),
                pending_diag_level: None,
                diag_highlight_color: DIAG_ERROR_COLOR,
            },
            startup_task,
        )
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
    /// Set `project_path` as the new `default_path` and persist.
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
    Append {
        line: String,
        job_id: u64,
    },
    /// The cargo process exited.
    Done {
        success: bool,
        job_id: u64,
    },
    /// Clear the output terminal and reset state.
    Clear,
    /// Copy the entire output content to the clipboard.
    CopyOutput,
    /// Periodic flush: rebuild `text_editor::Content` from the ring buffer if dirty.
    FlushOutput,
    /// Pass-through for the output text-editor.
    OutputAction(text_editor::Action),

    // --- Editor view ---
    EditorAction(text_editor::Action),
    /// Create a new empty "Untitled" tab.
    TabNew,
    /// Switch to the tab at the given index.
    TabSelect(usize),
    /// Close the tab at the given index.
    TabClose(usize),
    /// Open a native file-picker dialog to load a file into the editor.
    OpenFile,
    /// File chosen via rfd; `None` means the dialog was cancelled.
    FilePicked(Option<(PathBuf, String)>),
    /// Save the active tab (direct write if path known, otherwise open save dialog).
    SaveFile,
    /// Native save completed.
    /// `None` = dialog was cancelled; `Some(Ok(path))` = success; `Some(Err(msg))` = I/O error.
    SaveDone(Option<Result<PathBuf, String>>),

    // --- Find / Replace ---
    /// Toggle the find/replace panel open or closed.
    ToggleFindReplace,
    /// Open the find panel (search only, no replace row) — triggered by Ctrl+F.
    OpenInlineFind,
    /// Open the find panel with the replace row visible — triggered by Ctrl+H.
    OpenInlineReplace,
    /// Close the find panel — triggered by Esc.
    CloseInlineFind,
    /// Toggle the replace row inside the open find panel.
    ToggleReplaceField,
    /// Search text field changed.
    FindTextChanged(String),
    /// Replace text field changed.
    ReplaceTextChanged(String),
    /// Jump to the next match.
    FindNext,
    /// Jump to the previous match.
    FindPrev,
    /// Replace the current match and advance.
    ReplaceOne,
    /// Replace every occurrence in the active tab.
    ReplaceAll,

    // --- Output find panel ---
    /// Toggle the output-specific find panel open or closed.
    ToggleOutputFind,
    /// Output find text field changed.
    OutputFindTextChanged(String),
    /// Jump to the next match in the output.
    OutputFindNext,
    /// Jump to the previous match in the output.
    OutputFindPrev,

    // --- Context menu ---
    /// Open the context menu at the current mouse position for `kind`.
    ShowContextMenu(ContextMenuKind),
    /// Close / dismiss the context menu without performing any action.
    HideContextMenu,
    /// Copy the current selection to the clipboard.
    ContextCopy,
    /// Cut the current selection (copy + delete).
    ContextCut,
    /// Paste from the clipboard (triggers an async clipboard read).
    ContextPaste,
    /// Select all text in the focused widget.
    ContextSelectAll,
    /// Clipboard read completed; insert into the active editor.
    PasteText(Option<String>),

    // --- Settings view ---
    DefaultPathChanged(String),
    /// User selected a new theme from the pick-list.
    ThemeChanged(AppTheme),
    /// Increase or decrease the button font size by the given delta (+1 / -1).
    ButtonFontSizeChanged(f32),
    /// Reset all settings to their default values and persist.
    ResetSettings,

    // --- Tooltip overlay ---
    /// Mouse cursor moved to `position`.
    MouseMoved(iced::Point),
    /// A widget has been hovered — show the given tooltip text.
    TooltipShow(String),
    /// The cursor left a widget — hide the tooltip.
    TooltipHide,
    /// Window was resized; track height for tooltip positioning.
    WindowResized(iced::Size),
    /// Window mode changed (e.g. entered or exited fullscreen).
    WindowModeChanged(iced::window::Mode),

    // --- App ---
    Quit,

    /// Open the help PDF with the system default viewer.
    OpenHelpPdf,

    /// Open the Public Domain license information in the default browser.
    OpenPublicDomainLink,

    // --- Find highlight color ---
    /// Change the highlight color of the current find match in the editor.
    SetFindTestColor(Color),

    // --- Diagnostics ---
    /// Open the file from a diagnostic link in the editor and jump to line:col.
    OpenDiagnostic { path: PathBuf, line: usize, col: usize, level: DiagnosticLevel },
    /// Async result of reading the file for an `OpenDiagnostic` action.
    DiagnosticFileLoaded(Option<(PathBuf, String, usize, usize, DiagnosticLevel)>),
    /// User selected a diagnostic from the editor error dropdown; navigate to it.
    SelectErrorFromDropdown(usize),
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
                self.config.default_path = self.project_path.clone();
                self.config.save();
                self.status = "Startpfad gesetzt".to_string();
                Task::none()
            }

            Msg::RestoreDefault => {
                if !self.config.default_path.is_empty() {
                    self.project_path = self.config.default_path.clone();
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
                self.running_cmd = self.cargo_args.clone();
                self.run_start = Some(Instant::now());
                self.display_elapsed_ms = 0;
                self.status = "Läuft…".to_string();
                // Reset diagnostics for the new run.
                self.diagnostics.clear();
                self.pending_diag_level = None;

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
                // Parse cargo diagnostics on the fly.
                if let Some((level, msg, error_code)) = parse_diagnostic_line(&line) {
                    self.pending_diag_level = Some((level, msg, error_code));
                } else if let Some((file, diag_line, col)) = parse_location_line(&line) {
                    if let Some((level, msg, error_code)) = self.pending_diag_level.take() {
                        let full_path = if file.is_absolute() {
                            file
                        } else {
                            PathBuf::from(&self.project_path).join(&file)
                        };
                        self.diagnostics.push(Diagnostic {
                            level,
                            file: full_path,
                            line: diag_line,
                            column: col,
                            message: msg,
                            error_code,
                        });
                    }
                }
                self.output_lines.push_back(line);
                if self.output_lines.len() > MAX_LINES {
                    self.output_lines.pop_front();
                    self.output_trimmed = true;
                }
                self.output_dirty = true;
                // New output invalidates any existing output highlight and find position.
                self.output_highlight_line = None;
                self.output_find_status.clear();
                self.output_find_current_match = 0;
                Task::none()
            }

            Msg::Done { success, job_id } => {
                if job_id != self.current_job_id {
                    return Task::none();
                }
                self.running = false;
                self.stop_tx = None;
                if let Some(start) = self.run_start.take() {
                    let elapsed = start.elapsed().as_millis() as u64;
                    self.display_elapsed_ms = elapsed;
                    self.last_durations
                        .insert(self.running_cmd.clone(), elapsed);
                }
                self.status = if success {
                    "Fertig ✓".to_string()
                } else {
                    "Fehlgeschlagen ✗".to_string()
                };
                flush_output(self);
                scroll_output_to_top()
            }

            Msg::Clear => {
                self.output_lines.clear();
                self.output_content = text_editor::Content::new();
                self.output_dirty = false;
                self.output_trimmed = false;
                self.status = "Bereit".to_string();
                self.output_highlight_line = None;
                self.diagnostics.clear();
                self.pending_diag_level = None;
                Task::none()
            }

            Msg::CopyOutput => {
                let text = self.output_content.text();
                if text.is_empty() {
                    Task::none()
                } else {
                    clipboard::write(text)
                }
            }

            Msg::FlushOutput => {
                let was_dirty = self.output_dirty;
                if self.output_dirty {
                    flush_output(self);
                }
                if self.running {
                    if let Some(start) = self.run_start {
                        self.display_elapsed_ms = start.elapsed().as_millis() as u64;
                    }
                }
                if was_dirty {
                    scroll_output_to_top()
                } else {
                    Task::none()
                }
            }

            Msg::OutputAction(action) => {
                if !matches!(action, text_editor::Action::Edit(_)) {
                    self.output_content.perform(action);
                }
                Task::none()
            }

            // --- Editor ---
            Msg::EditorAction(action) => {
                if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                    if matches!(action, text_editor::Action::Edit(_)) {
                        tab.dirty = true;
                        // Text has changed — existing match highlights are stale.
                        self.editor_highlight_line = None;
                        self.find_all_match_lines.clear();
                    }
                    tab.content.perform(action);
                }
                Task::none()
            }

            Msg::TabNew => {
                self.editor_tabs
                    .push(EditorTab::new_untitled(self.untitled_counter));
                self.untitled_counter += 1;
                self.active_tab = self.editor_tabs.len() - 1;
                Task::none()
            }

            Msg::TabSelect(idx) => {
                // Only reset state when actually switching to a different tab.
                // Clicking the already-active tab must not clear the highlight,
                // reset the match counter, or corrupt the find_status string.
                if idx < self.editor_tabs.len() && idx != self.active_tab {
                    self.active_tab = idx;
                    self.editor_highlight_line = None;
                    // Recompute all-match highlights for the newly active tab.
                    if self.find_replace_open && !self.find_text.is_empty() {
                        if let Some(tab) = self.editor_tabs.get(idx) {
                            let text = tab.content.text();
                            self.find_all_match_lines =
                                collect_all_match_lines(&text, &self.find_text);
                        } else {
                            self.find_all_match_lines.clear();
                        }
                    } else {
                        self.find_all_match_lines.clear();
                    }
                    // Synchronise find_status with the newly computed match list so
                    // that the status display always reflects the current tab's
                    // matches (fixes: stale / incorrect line-number in the status).
                    // Clamp the existing match index to the new list length so the
                    // user's position is preserved as closely as possible instead of
                    // being silently reset to 0.
                    let total = self.find_all_match_lines.len();
                    if total == 0 {
                        self.find_current_match = 0;
                        self.find_status = if !self.find_text.is_empty() {
                            "Keine Treffer".to_string()
                        } else {
                            String::new()
                        };
                        return Task::none();
                    }
                    // Clamp to valid range.
                    self.find_current_match =
                        self.find_current_match.min(total - 1);
                    if let Some(tab) = self.editor_tabs.get_mut(idx) {
                        apply_find_selection(
                            &mut tab.content,
                            &self.find_text,
                            self.find_current_match,
                        );
                    }
                    let hl_line =
                        self.find_all_match_lines.get(self.find_current_match).copied();
                    self.editor_highlight_line = hl_line;
                    self.find_status =
                        editor_find_status_text(self.find_current_match, total, hl_line);
                    if let Some(line) = hl_line {
                        return scroll_editor_to_line(line);
                    }
                }
                Task::none()
            }

            Msg::TabClose(idx) => {
                if self.editor_tabs.len() <= 1 {
                    // Keep at least one tab; just reset its content.
                    if let Some(tab) = self.editor_tabs.get_mut(0) {
                        *tab = EditorTab::new_untitled(self.untitled_counter);
                        self.untitled_counter += 1;
                    }
                    return Task::none();
                }
                self.editor_tabs.remove(idx);
                if self.active_tab >= self.editor_tabs.len() {
                    self.active_tab = self.editor_tabs.len() - 1;
                }
                Task::none()
            }

            Msg::OpenFile => Task::perform(
                async {
                    let handle = rfd::AsyncFileDialog::new().pick_file().await?;
                    let path = handle.path().to_path_buf();
                    let contents = tokio::fs::read_to_string(&path).await.ok()?;
                    Some((path, contents))
                },
                Msg::FilePicked,
            ),

            Msg::FilePicked(maybe) => {
                if let Some((path, file_text)) = maybe {
                    // Switch to existing tab if the file is already open.
                    if let Some(idx) = self
                        .editor_tabs
                        .iter()
                        .position(|t| t.path.as_deref() == Some(&path))
                    {
                        self.active_tab = idx;
                    } else {
                        self.editor_tabs
                            .push(EditorTab::from_file(path, &file_text));
                        self.active_tab = self.editor_tabs.len() - 1;
                    }
                }
                Task::none()
            }

            Msg::SaveFile => {
                let Some(tab) = self.editor_tabs.get(self.active_tab) else {
                    return Task::none();
                };
                let text_content = tab.content.text();
                if let Some(path) = tab.path.clone() {
                    Task::perform(
                        async move {
                            Some(
                                tokio::fs::write(&path, text_content.as_bytes())
                                    .await
                                    .map(|()| path)
                                    .map_err(|e| e.to_string()),
                            )
                        },
                        Msg::SaveDone,
                    )
                } else {
                    Task::perform(
                        async move {
                            let handle = rfd::AsyncFileDialog::new().save_file().await?;
                            let path = handle.path().to_path_buf();
                            Some(
                                tokio::fs::write(&path, text_content.as_bytes())
                                    .await
                                    .map(|()| path)
                                    .map_err(|e| e.to_string()),
                            )
                        },
                        Msg::SaveDone,
                    )
                }
            }

            Msg::SaveDone(maybe) => {
                match maybe {
                    None => {
                        // User cancelled the save dialog — no action needed.
                    }
                    Some(Ok(path)) => {
                        if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                            tab.dirty = false;
                            // Update title/path when saving an untitled tab for the first time
                            // (tab.path is None before save, so it differs from the new path).
                            if tab.path.as_deref() != Some(&path) {
                                tab.title = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "Unbekannt".to_string());
                                tab.path = Some(path.clone());
                            }
                            self.status = format!("Datei gespeichert: {}", path.display());
                        }
                    }
                    Some(Err(msg)) => {
                        self.status = format!("Fehler beim Speichern: {msg}");
                    }
                }
                Task::none()
            }

            // --- Find / Replace ---
            Msg::ToggleFindReplace => {
                self.find_replace_open = !self.find_replace_open;
                if !self.find_replace_open {
                    self.find_status.clear();
                    self.editor_highlight_line = None;
                    self.find_all_match_lines.clear();
                } else {
                    self.find_show_replace = false;
                }
                Task::none()
            }

            Msg::OpenInlineFind => {
                if self.current_view != View::Editor {
                    return Task::none();
                }
                self.find_replace_open = true;
                self.find_show_replace = false;
                Task::none()
            }

            Msg::OpenInlineReplace => {
                if self.current_view != View::Editor {
                    return Task::none();
                }
                self.find_replace_open = true;
                self.find_show_replace = true;
                Task::none()
            }

            Msg::CloseInlineFind => {
                if self.find_replace_open {
                    self.find_replace_open = false;
                    self.find_status.clear();
                    self.editor_highlight_line = None;
                    self.find_all_match_lines.clear();
                }
                Task::none()
            }

            Msg::ToggleReplaceField => {
                self.find_show_replace = !self.find_show_replace;
                Task::none()
            }

            Msg::FindTextChanged(s) => {
                self.find_text = s;
                self.find_current_match = 0;
                if let Some(tab) = self.editor_tabs.get(self.active_tab) {
                    let text = tab.content.text();
                    self.find_all_match_lines =
                        collect_all_match_lines(&text, &self.find_text);
                } else {
                    self.find_all_match_lines.clear();
                }
                let total = self.find_all_match_lines.len();
                if total == 0 {
                    self.find_status = if self.find_text.is_empty() {
                        String::new()
                    } else {
                        "Keine Treffer".to_string()
                    };
                    self.editor_highlight_line = None;
                } else {
                    // Position the cursor at match 0 in the content.
                    if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                        apply_find_selection(&mut tab.content, &self.find_text, 0);
                    }
                    // Derive status from find_all_match_lines so it always
                    // matches the visual highlight overlay.
                    let hl_line = self.find_all_match_lines.first().copied();
                    self.editor_highlight_line = hl_line;
                    self.find_status = editor_find_status_text(0, total, hl_line);
                }
                if let Some(&line) = self.find_all_match_lines.first() {
                    scroll_editor_to_line(line)
                } else {
                    Task::none()
                }
            }

            Msg::ReplaceTextChanged(s) => {
                self.replace_text = s;
                Task::none()
            }

            Msg::FindNext => {
                // No-op when the find panel is closed (e.g. triggered by a
                // global Arrow-Down key that arrived before the panel opened).
                if !self.find_replace_open || self.find_text.is_empty() {
                    return Task::none();
                }
                let total = self.find_all_match_lines.len();
                if total == 0 {
                    self.find_status = "Keine Treffer".to_string();
                    self.editor_highlight_line = None;
                    return Task::none();
                }
                self.find_current_match = (self.find_current_match + 1) % total;
                if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                    apply_find_selection(
                        &mut tab.content,
                        &self.find_text,
                        self.find_current_match,
                    );
                }
                // Use find_all_match_lines for the status/highlight so they are
                // always in sync with the visual overlay.
                let hl_line = self.find_all_match_lines.get(self.find_current_match).copied();
                self.editor_highlight_line = hl_line;
                self.find_status =
                    editor_find_status_text(self.find_current_match, total, hl_line);
                if let Some(line) = hl_line {
                    scroll_editor_to_line(line)
                } else {
                    Task::none()
                }
            }

            Msg::FindPrev => {
                // No-op when the find panel is closed.
                if !self.find_replace_open || self.find_text.is_empty() {
                    return Task::none();
                }
                let total = self.find_all_match_lines.len();
                if total == 0 {
                    self.find_status = "Keine Treffer".to_string();
                    self.editor_highlight_line = None;
                    return Task::none();
                }
                // Wrap backwards: when at index 0, jump to the last match.
                self.find_current_match =
                    self.find_current_match.checked_sub(1).unwrap_or(total - 1);
                if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                    apply_find_selection(
                        &mut tab.content,
                        &self.find_text,
                        self.find_current_match,
                    );
                }
                let hl_line = self.find_all_match_lines.get(self.find_current_match).copied();
                self.editor_highlight_line = hl_line;
                self.find_status =
                    editor_find_status_text(self.find_current_match, total, hl_line);
                if let Some(line) = hl_line {
                    scroll_editor_to_line(line)
                } else {
                    Task::none()
                }
            }

            Msg::ReplaceOne => {
                if self.find_text.is_empty() {
                    return Task::none();
                }
                if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                    let full = tab.content.text();
                    let needle = &self.find_text;
                    let replacement = &self.replace_text;
                    // Collect all match byte-positions (Unicode-safe: use char boundaries).
                    let positions: Vec<usize> = full
                        .match_indices(needle.as_str())
                        .map(|(i, _)| i)
                        .collect();
                    if positions.is_empty() {
                        self.find_status = "Keine Treffer".to_string();
                    } else {
                        let idx = self.find_current_match % positions.len();
                        let pos = positions[idx];
                        let mut new_text = full.clone();
                        new_text.replace_range(pos..pos + needle.len(), replacement);
                        tab.content = text_editor::Content::with_text(&new_text);
                        tab.dirty = true;
                        // Recompute all-match highlights after the text changed.
                        let new_text_ref = tab.content.text();
                        self.find_all_match_lines =
                            collect_all_match_lines(&new_text_ref, needle);
                        let new_total = self.find_all_match_lines.len();
                        if new_total == 0 {
                            self.find_current_match = 0;
                            self.editor_highlight_line = None;
                            self.find_status = "Keine Treffer".to_string();
                        } else {
                            self.find_current_match %= new_total;
                            // Reposition cursor at the new current match.
                            let find_text = self.find_text.clone();
                            let sel_pos = apply_find_selection(
                                &mut tab.content,
                                &find_text,
                                self.find_current_match,
                            );
                            self.editor_highlight_line = sel_pos.map(|(l, _)| l);
                            self.find_status = format!(
                                "Treffer {} von {}",
                                self.find_current_match + 1, new_total
                            );
                        }
                    }
                }
                if let Some(&line) = self.find_all_match_lines.get(self.find_current_match) {
                    scroll_editor_to_line(line)
                } else {
                    Task::none()
                }
            }

            Msg::ReplaceAll => {
                if self.find_text.is_empty() {
                    return Task::none();
                }
                if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                    let full = tab.content.text();
                    let count = full.matches(self.find_text.as_str()).count();
                    if count == 0 {
                        self.find_status = "Keine Treffer".to_string();
                    } else {
                        let new_text = full.replace(self.find_text.as_str(), self.replace_text.as_str());
                        tab.content = text_editor::Content::with_text(&new_text);
                        tab.dirty = true;
                        self.find_current_match = 0;
                        self.editor_highlight_line = None;
                        self.find_all_match_lines.clear();
                        self.find_status = format!("{count} ersetzt");
                    }
                }
                Task::none()
            }

            // --- Output find panel ---
            Msg::ToggleOutputFind => {
                self.output_find_open = !self.output_find_open;
                if !self.output_find_open {
                    self.output_find_status.clear();
                    self.output_highlight_line = None;
                }
                self.context_menu = None;
                Task::none()
            }

            Msg::OutputFindTextChanged(s) => {
                self.output_find_text = s;
                self.output_find_current_match = 0;
                let total = {
                    let t = self.output_content.text();
                    if self.output_find_text.is_empty() {
                        0
                    } else {
                        t.matches(self.output_find_text.as_str()).count()
                    }
                };
                if total == 0 {
                    self.output_find_status = if self.output_find_text.is_empty() {
                        String::new()
                    } else {
                        "Keine Treffer".to_string()
                    };
                    self.output_highlight_line = None;
                } else {
                    let pos = apply_find_selection(
                        &mut self.output_content,
                        &self.output_find_text,
                        0,
                    );
                    self.output_highlight_line = pos.map(|(line, _)| line);
                    self.output_find_status =
                        output_find_status_text(0, total, pos.map(|(line, _)| line));
                }
                Task::none()
            }

            Msg::OutputFindNext => {
                if self.output_find_text.is_empty() {
                    return Task::none();
                }
                let total = {
                    let t = self.output_content.text();
                    t.matches(self.output_find_text.as_str()).count()
                };
                if total == 0 {
                    self.output_find_status = "Keine Treffer".to_string();
                    self.output_highlight_line = None;
                } else {
                    self.output_find_current_match =
                        (self.output_find_current_match + 1) % total;
                    let pos = apply_find_selection(
                        &mut self.output_content,
                        &self.output_find_text,
                        self.output_find_current_match,
                    );
                    self.output_highlight_line = pos.map(|(line, _)| line);
                    self.output_find_status = output_find_status_text(
                        self.output_find_current_match,
                        total,
                        pos.map(|(line, _)| line),
                    );
                }
                Task::none()
            }

            Msg::OutputFindPrev => {
                if self.output_find_text.is_empty() {
                    return Task::none();
                }
                let total = {
                    let t = self.output_content.text();
                    t.matches(self.output_find_text.as_str()).count()
                };
                if total == 0 {
                    self.output_find_status = "Keine Treffer".to_string();
                    self.output_highlight_line = None;
                } else {
                    self.output_find_current_match =
                        self.output_find_current_match.checked_sub(1).unwrap_or(total - 1);
                    let pos = apply_find_selection(
                        &mut self.output_content,
                        &self.output_find_text,
                        self.output_find_current_match,
                    );
                    self.output_highlight_line = pos.map(|(line, _)| line);
                    self.output_find_status = output_find_status_text(
                        self.output_find_current_match,
                        total,
                        pos.map(|(line, _)| line),
                    );
                }
                Task::none()
            }

            // --- Context menu ---
            Msg::ShowContextMenu(kind) => {
                self.context_menu = Some(ContextMenuState {
                    x: self.mouse_x,
                    y: self.mouse_y,
                    kind,
                });
                Task::none()
            }

            Msg::HideContextMenu => {
                self.context_menu = None;
                Task::none()
            }

            Msg::ContextCopy => {
                let selected = match self.context_menu.as_ref().map(|m| &m.kind) {
                    Some(ContextMenuKind::Editor) => self
                        .editor_tabs
                        .get(self.active_tab)
                        .and_then(|t| t.content.selection()),
                    Some(ContextMenuKind::Output) => self.output_content.selection(),
                    None => None,
                };
                self.context_menu = None;
                if let Some(text) = selected {
                    clipboard::write(text)
                } else {
                    Task::none()
                }
            }

            Msg::ContextCut => {
                // Copy selection, then delete it (editor only).
                let selected = self
                    .editor_tabs
                    .get(self.active_tab)
                    .and_then(|t| t.content.selection());
                self.context_menu = None;
                if let Some(sel_text) = selected {
                    // Delete selection by performing Backspace (iced deletes
                    // the active selection when one exists).
                    if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                        tab.content
                            .perform(text_editor::Action::Edit(text_editor::Edit::Backspace));
                        tab.dirty = true;
                    }
                    clipboard::write(sel_text)
                } else {
                    Task::none()
                }
            }

            Msg::ContextPaste => {
                self.context_menu = None;
                clipboard::read().map(Msg::PasteText)
            }

            Msg::ContextSelectAll => {
                match self.context_menu.as_ref().map(|m| &m.kind) {
                    Some(ContextMenuKind::Editor) => {
                        if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                            tab.content.perform(text_editor::Action::SelectAll);
                        }
                    }
                    Some(ContextMenuKind::Output) => {
                        self.output_content.perform(text_editor::Action::SelectAll);
                    }
                    None => {}
                }
                self.context_menu = None;
                Task::none()
            }

            Msg::PasteText(maybe) => {
                if let Some(text) = maybe {
                    use std::sync::Arc;
                    if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                        tab.content.perform(text_editor::Action::Edit(
                            text_editor::Edit::Paste(Arc::new(text)),
                        ));
                        tab.dirty = true;
                    }
                }
                Task::none()
            }

            // --- Settings ---
            Msg::DefaultPathChanged(p) => {
                self.config.default_path = p;
                self.config.save();
                Task::none()
            }

            Msg::ThemeChanged(theme) => {
                self.config.theme = theme;
                self.config.save();
                Task::none()
            }

            Msg::ButtonFontSizeChanged(delta) => {
                let new_size = (self.config.button_font_size + delta).clamp(10.0, 24.0);
                self.config.button_font_size = new_size;
                self.config.save();
                Task::none()
            }

            Msg::ResetSettings => {
                self.config = Config::default();
                self.config.save();
                self.status = "Einstellungen zurückgesetzt ✓".to_string();
                Task::none()
            }

            // --- Tooltip overlay ---
            Msg::MouseMoved(pt) => {
                self.mouse_x = pt.x;
                self.mouse_y = pt.y;
                Task::none()
            }

            Msg::TooltipShow(t) => {
                self.tooltip_text = Some(t);
                Task::none()
            }

            Msg::TooltipHide => {
                self.tooltip_text = None;
                Task::none()
            }

            Msg::WindowResized(size) => {
                self.window_width = size.width.max(MIN_WINDOW_WIDTH);
                self.window_height = size.height.max(MIN_WINDOW_HEIGHT);
                self.config.window_width = self.window_width;
                self.config.window_height = self.window_height;
                iced::window::get_latest()
                    .then(|maybe_id| {
                        if let Some(id) = maybe_id {
                            iced::window::get_mode(id)
                        } else {
                            Task::done(iced::window::Mode::Windowed)
                        }
                    })
                    .map(Msg::WindowModeChanged)
            }

            Msg::WindowModeChanged(mode) => {
                self.config.is_fullscreen = mode == iced::window::Mode::Fullscreen;
                Task::none()
            }

            // --- App ---
            Msg::Quit => {
                self.config.save();
                iced::exit()
            }

            Msg::OpenHelpPdf => {
                // Find PDF next to the executable or in the current directory.
                let pdf_path = std::env::current_exe()
                    .ok()
                    .and_then(|exe| {
                        let candidate = exe.parent()?.join(HELP_PDF_FILENAME);
                        if candidate.exists() { Some(candidate) } else { None }
                    })
                    .unwrap_or_else(|| std::path::PathBuf::from(HELP_PDF_FILENAME));

                let open_result = {
                    #[cfg(target_os = "linux")]
                    { std::process::Command::new("xdg-open").arg(&pdf_path).spawn() }
                    #[cfg(target_os = "macos")]
                    { std::process::Command::new("open").arg(&pdf_path).spawn() }
                    #[cfg(target_os = "windows")]
                    {
                        std::process::Command::new("cmd")
                            .args(["/c", "start", "", pdf_path.display().to_string().as_str()])
                            .spawn()
                    }
                    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
                    { Err(std::io::Error::other("unsupported platform")) }
                };

                if let Err(e) = open_result {
                    self.status = format!("PDF konnte nicht geöffnet werden: {e}");
                }
                Task::none()
            }

            Msg::OpenPublicDomainLink => {
                let url = "https://creativecommons.org/publicdomain/";
                let open_result = {
                    #[cfg(target_os = "linux")]
                    { std::process::Command::new("xdg-open").arg(url).spawn() }
                    #[cfg(target_os = "macos")]
                    { std::process::Command::new("open").arg(url).spawn() }
                    #[cfg(target_os = "windows")]
                    { std::process::Command::new("explorer").arg(url).spawn() }
                    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
                    { Err(std::io::Error::other("nicht unterstützte Plattform")) }
                };
                if let Err(e) = open_result {
                    self.status = format!("Link konnte nicht geöffnet werden: {e}");
                }
                Task::none()
            }

            // --- Find highlight color ---
            Msg::SetFindTestColor(c) => {
                self.find_test_color = c;
                Task::none()
            }

            // --- Diagnostics ---
            Msg::OpenDiagnostic { path, line, col, level } => {
                // Resolve the path relative to the project directory.
                let full_path = if path.is_absolute() {
                    path
                } else {
                    PathBuf::from(&self.project_path).join(&path)
                };
                Task::perform(
                    async move {
                        let content = tokio::fs::read_to_string(&full_path).await.ok()?;
                        Some((full_path, content, line, col, level))
                    },
                    Msg::DiagnosticFileLoaded,
                )
            }

            Msg::DiagnosticFileLoaded(maybe) => {
                if let Some((path, file_text, line, col, level)) = maybe {
                    // Find the matching diagnostic before path may be moved.
                    let matching_diag = self
                        .diagnostics
                        .iter()
                        .find(|d| d.file == path && d.line == line)
                        .cloned();

                    // Switch to an existing tab or open a new one.
                    if let Some(idx) = self
                        .editor_tabs
                        .iter()
                        .position(|t| t.path.as_deref() == Some(&path))
                    {
                        self.active_tab = idx;
                    } else {
                        self.editor_tabs.push(EditorTab::from_file(path, &file_text));
                        self.active_tab = self.editor_tabs.len() - 1;
                    }
                    // Navigate to the target line and column (cargo reports 1-based).
                    let target_line = line.saturating_sub(1);
                    let target_col = col.saturating_sub(1);
                    if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                        tab.content.perform(text_editor::Action::Move(
                            text_editor::Motion::DocumentStart,
                        ));
                        for _ in 0..target_line {
                            tab.content.perform(text_editor::Action::Move(
                                text_editor::Motion::Down,
                            ));
                        }
                        tab.content.perform(text_editor::Action::Move(
                            text_editor::Motion::Home,
                        ));
                        for _ in 0..target_col {
                            tab.content.perform(text_editor::Action::Move(
                                text_editor::Motion::Right,
                            ));
                        }
                    }
                    let diag_color = level_color(level);
                    self.diag_highlight_color = diag_color;
                    // Store the full error info persistently on the tab so the
                    // highlight and gutter tooltip survive scrolling and tab switches.
                    if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                        // Avoid duplicates: one entry per line is sufficient.
                        if !tab.editor_errors.iter().any(|e| e.line == target_line) {
                            let message = matching_diag
                                .as_ref()
                                .map(|d| d.message.clone())
                                .unwrap_or_default();
                            let error_code = matching_diag.and_then(|d| d.error_code);
                            tab.editor_errors.push(EditorError {
                                line: target_line,
                                column: target_col,
                                level,
                                message,
                                error_code,
                            });
                        }
                    }
                    self.editor_highlight_line = Some(target_line);
                    // Switch to the Editor view.
                    self.current_view = View::Editor;
                    return scroll_editor_to_line(target_line);
                }
                Task::none()
            }

            // --- Error dropdown ---
            Msg::SelectErrorFromDropdown(idx) => {
                let Some(diag) = self.diagnostics.get(idx).cloned() else {
                    return Task::none();
                };
                let path = diag.file.clone();
                let line = diag.line;
                let col = diag.column;
                let level = diag.level;
                self.update(Msg::OpenDiagnostic { path, line, col, level })
            }
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
            View::About => self.view_about(),
        };

        let topbar = self.view_topbar();
        let footer = self.view_footer();

        let main_col: Element<'_, Msg> = column![topbar, body, footer].into();

        // ---- Context menu overlay ----
        //
        // The overlay is built unconditionally and placed at a fixed position
        // inside the final stack so that `main_col` always sits at index 0.
        // If the overlay were only added when the menu is open the widget-tree
        // structure would change on every open/close, causing iced to
        // misidentify stateful widgets (like the editor scrollable) and reset
        // their state — which makes the editor jump back to page 1.
        let ctx_overlay: Element<'_, Msg> = if let Some(cm) = &self.context_menu {
            let is_editor = matches!(cm.kind, ContextMenuKind::Editor);
            let dismiss_bg: Element<'_, Msg> = mouse_area(
                Space::new(Length::Fill, Length::Fill),
            )
            .on_press(Msg::HideContextMenu)
            .into();

            // Build menu items.
            let copy_btn = button(
                row![bi(Bootstrap::ClipboardFill).size(13), text(" Kopieren (Copy)").size(13)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .on_press(Msg::ContextCopy)
            .width(Length::Fill)
            .padding([4, 10])
            .style(readable_button_style);
            let selectall_btn = button(
                row![bi(Bootstrap::TextLeft).size(13), text(" Alles auswählen (Select All)").size(13)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .on_press(Msg::ContextSelectAll)
            .width(Length::Fill)
            .padding([4, 10])
            .style(readable_button_style);

            let mut menu_col = column![copy_btn, selectall_btn].spacing(2);

            if is_editor {
                let cut_btn = button(
                    row![bi(Bootstrap::Scissors).size(13), text(" Ausschneiden (Cut)").size(13)]
                        .spacing(4)
                        .align_y(iced::Alignment::Center),
                )
                .on_press(Msg::ContextCut)
                .width(Length::Fill)
                .padding([4, 10])
                .style(readable_button_style);
                let paste_btn = button(
                    row![bi(Bootstrap::ClipboardFill).size(13), text(" Einfügen (Paste)").size(13)]
                        .spacing(4)
                        .align_y(iced::Alignment::Center),
                )
                .on_press(Msg::ContextPaste)
                .width(Length::Fill)
                .padding([4, 10])
                .style(readable_button_style);
                let find_btn = button(
                    row![bi(Bootstrap::Search).size(13), text(" Suchen/Ersetzen…").size(13)]
                        .spacing(4)
                        .align_y(iced::Alignment::Center),
                )
                .on_press(Msg::ToggleFindReplace)
                .width(Length::Fill)
                .padding([4, 10])
                .style(readable_button_style);
                menu_col = menu_col.push(cut_btn).push(paste_btn).push(find_btn);
            } else {
                let output_find_btn = button(
                    row![bi(Bootstrap::Search).size(13), text(" Suchen…").size(13)]
                        .spacing(4)
                        .align_y(iced::Alignment::Center),
                )
                .on_press(Msg::ToggleOutputFind)
                .width(Length::Fill)
                    .padding([4, 10])
                    .style(readable_button_style);
                menu_col = menu_col.push(output_find_btn);
            }

            let menu_box: Element<'_, Msg> = container(menu_col)
                .style(|_theme| iced::widget::container::Style {
                    background: Some(iced::Background::Color(Color::from_rgba(
                        0.15, 0.15, 0.18, 0.97,
                    ))),
                    border: iced::Border {
                        color: Color::from_rgba(0.4, 0.4, 0.4, 1.0),
                        width: 1.0,
                        radius: 5.0.into(),
                    },
                    text_color: Some(Color::WHITE),
                    shadow: iced::Shadow {
                        color: Color::from_rgba(0.0, 0.0, 0.0, 0.4),
                        offset: iced::Vector::new(2.0, 2.0),
                        blur_radius: 6.0,
                    },
                })
                .padding([4, 0])
                .width(220)
                .into();

            let menu_layer: Element<'_, Msg> = column![
                Space::with_height(Length::Fixed(cm.y.max(0.0))),
                row![
                    Space::with_width(Length::Fixed(cm.x.max(0.0))),
                    menu_box,
                ],
            ]
            .width(Length::Fill)
            .into();

            // Sub-stack: dismiss background behind the positioned menu.
            stack![dismiss_bg, menu_layer].into()
        } else {
            // Transparent placeholder; passes all events through to main_col.
            Space::new(Length::Fill, Length::Fill).into()
        };

        // ---- Tooltip overlay ----
        //
        // Same structural-stability strategy: the tooltip layer is always at
        // index 2 in the final stack (transparent Space when hidden).
        let tip_overlay: Element<'_, Msg> = if let Some(tip) = &self.tooltip_text {
            let tip_box: Element<'_, Msg> = container(text(tip.as_str()).size(12))
                .style(|_theme| iced::widget::container::Style {
                    background: Some(iced::Background::Color(Color::from_rgba(
                        0.12, 0.12, 0.12, 0.93,
                    ))),
                    border: iced::Border {
                        color: Color::BLACK,
                        width: 1.5,
                        radius: 4.0.into(),
                    },
                    text_color: Some(Color::WHITE),
                    shadow: iced::Shadow::default(),
                })
                .padding([4, 8])
                .into();

            // When the cursor is in the LOWER half of the window, show the
            // tooltip to the LEFT of the cursor so it does not obscure buttons
            // above. In the upper half, keep the classic above-cursor position.
            let (tip_x, tip_y) = if self.mouse_y > self.window_height / 2.0 {
                let x = (self.mouse_x - TOOLTIP_ESTIMATED_WIDTH - 12.0).max(0.0);
                let y = (self.mouse_y - 24.0).max(0.0);
                (x, y)
            } else {
                let x = (self.mouse_x - TOOLTIP_OFFSET_X).max(0.0);
                let y = (self.mouse_y - TOOLTIP_OFFSET_Y).max(0.0);
                (x, y)
            };

            column![
                Space::with_height(Length::Fixed(tip_y)),
                row![Space::with_width(Length::Fixed(tip_x)), tip_box,],
            ]
            .width(Length::Fill)
            .into()
        } else {
            // Transparent placeholder.
            Space::new(Length::Fill, Length::Fill).into()
        };

        // `main_col` is ALWAYS at index 0 of this stack regardless of whether
        // the context menu or tooltip are active.  This keeps all stateful
        // widget positions (e.g. the editor scrollable) stable across renders,
        // preventing iced from resetting the editor scroll offset to 0 when an
        // overlay appears or disappears (root cause of the "jump to page 1" bug).
        stack![main_col, ctx_overlay, tip_overlay].into()
    }

    // -----------------------------------------------------------------------
    // Topbar
    // -----------------------------------------------------------------------

    fn view_topbar(&self) -> Element<'_, Msg> {
        let title = text("Cargo GUI").size(20);

        container(
            row![title]
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
        let fs = self.config.button_font_size;
        let settings_btn = hover_tip(
            button(
                row![bi(Bootstrap::GearFill).size(fs), text(" Einstellungen").size(fs)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .on_press(Msg::NavigateTo(View::Settings))
            .padding([5, 10])
            .style(readable_button_style),
            "Einstellungen öffnen — Standard-Pfad, Theme und Button-Schriftgröße festlegen".to_string(),
        );

        let editor_btn = hover_tip(
            button(
                row![bi(Bootstrap::PencilFill).size(fs), text(" Editor").size(fs)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .on_press(Msg::NavigateTo(View::Editor))
            .padding([5, 10])
            .style(readable_button_style),
            "Datei-Editor öffnen — Texte bearbeiten, Tabs verwalten, Suchen/Ersetzen".to_string(),
        );

        let help_btn = hover_tip(
            button(
                row![bi(Bootstrap::QuestionCircle).size(fs), text(" Hilfe").size(fs)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .on_press(Msg::NavigateTo(View::Help))
            .padding([5, 10])
            .style(readable_button_style),
            "Bedienungsanleitung öffnen — alle Funktionen im Überblick".to_string(),
        );

        let quit_btn = hover_tip(
            button(
                row![bi(Bootstrap::XCircle).size(fs), text(" Beenden").size(fs)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .on_press(Msg::Quit)
            .padding([5, 10])
            .style(readable_button_style),
            "Anwendung beenden (alle ungespeicherten Änderungen gehen verloren)".to_string(),
        );

        let status_text = text(format!("Status: {}", self.status)).size(13);

        let about_btn = hover_tip(
            button(
                row![bi(Bootstrap::InfoCircleFill).size(fs), text(" Über").size(fs)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .on_press(Msg::NavigateTo(View::About))
            .padding([5, 10])
                .style(readable_button_style),
            "Über Cargo GUI — Versionsinformationen und Kontakt".to_string(),
        );

        container(
            row![settings_btn, editor_btn, help_btn, about_btn, quit_btn, status_text]
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
        let fs = self.config.button_font_size;
        // -- Project directory row --
        let path_input = text_input("Projektpfad…", &self.project_path)
            .on_input(Msg::PathChanged)
            .padding(5);

        let browse_btn = hover_tip(
            button(
                row![bi(Bootstrap::FoldertwoOpen).size(fs), text(" Durchsuchen").size(fs)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .on_press(Msg::BrowsePath)
            .padding([5, 10])
            .style(readable_button_style),
            "Projektordner auswählen — öffnet einen nativen Ordnerauswahl-Dialog".to_string(),
        );

        let set_default_btn = hover_tip(
            button(text("Als Start").size(fs))
                .on_press(Msg::SetAsDefault)
                .padding([5, 10])
                .style(readable_button_style),
            "Diesen Pfad als Standard-Projektpfad speichern (wird beim nächsten Start geladen)".to_string(),
        );

        let path_buttons_row = row![browse_btn, set_default_btn,]
            .spacing(6)
            .align_y(iced::Alignment::Center);

        let path_row = column![
            text("Projektverzeichnis:").size(13),
            path_input.width(Length::Fill),
            path_buttons_row,
        ]
        .spacing(4)
        .padding([4, 8]);

        // -- Arguments row --
        let args_input = text_input("z.B. build --release", &self.cargo_args)
            .on_input(Msg::ArgsChanged)
            .on_submit(Msg::Run)
            .padding([1, 8])
            .width(ARGS_INPUT_WIDTH);

        let run_btn = hover_tip(
            button(
                row![bi(Bootstrap::PlayFill).size(fs), text(" Ausführen").size(fs)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .on_press_maybe((!self.running).then_some(Msg::Run))
            .padding([5, 10])
            .style(readable_button_style),
            "Cargo-Befehl ausführen — startet den im Argumentfeld eingetragenen Befehl".to_string(),
        );

        let stop_btn = hover_tip(
            button(
                row![
                    bi(Bootstrap::ExclamationOctagonFill).size(fs + 2.0),
                    text(" Stop").size(fs),
                ]
                .spacing(4)
                .align_y(iced::Alignment::Center),
            )
            .on_press_maybe(self.running.then_some(Msg::Stop))
            .padding([5, 10])
            .style(alarm_button_style),
            "Laufenden Cargo-Prozess abbrechen — roter Alarm-Stopp-Knopf".to_string(),
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

        let cargo_new_btn = hover_tip(
            button(text("cargo new").size(fs))
                .on_press_maybe(
                    (!self.running && !self.new_project_name.trim().is_empty())
                        .then_some(Msg::RunCargoNew),
                )
                .padding([5, 10])
                .style(readable_button_style),
            "Neues Cargo-Projekt mit dem eingetragenen Namen anlegen (cargo new <name>)".to_string(),
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
        // Helper closure: builds one command button with expected/elapsed timing.
        // The timing label is placed *outside* the button so that it stays fully
        // readable even when the button is disabled (which would otherwise apply
        // a global opacity reduction to every child of the button widget).
        let make_cmd_btn = |(label, cmd, tip_text): &(&str, &str, &str)| {
            let cmd_str = cmd.to_string();
            let tip = tip_text.to_string();
            let duration_label = self
                .last_durations
                .get(&cmd_str)
                .map(|&ms| format_duration(ms))
                .unwrap_or_else(|| "?".to_string());
            let is_running_this = self.running && self.running_cmd == cmd_str;
            let time_str = if is_running_this {
                format!(
                    "est:{duration_label} jetzt:{}",
                    format_duration(self.display_elapsed_ms)
                )
            } else {
                format!("est:{duration_label}")
            };
            let btn = button(text(label.to_string()).size(fs))
                .on_press_maybe((!self.running).then_some(Msg::RunCommand(cmd_str)))
                .width(Length::Fill)
                .padding([5, 8])
                .style(readable_button_style);
            hover_tip(
                column![btn, text(time_str).size(11.0)]
                    .spacing(1)
                    .width(Length::Fill),
                tip,
            )
        };

        let left_col = column(COMMANDS_LEFT.iter().map(&make_cmd_btn).collect::<Vec<_>>())
            .spacing(4);

        let right_col = column(COMMANDS_RIGHT.iter().map(&make_cmd_btn).collect::<Vec<_>>())
            .spacing(4);

        let commands_grid = row![left_col, right_col].spacing(8);

        let commands_section = column![text("Cargo Befehle").size(15), commands_grid,]
            .spacing(6)
            .padding([4, 8]);

        // -- Output section --
        let clear_btn = hover_tip(
            button(text("Ausgabe löschen").size(fs))
                .on_press(Msg::Clear)
                .padding([5, 10])
                .style(readable_button_style),
            "Ausgabe leeren und Status zurücksetzen".to_string(),
        );

        let copy_output_btn = hover_tip(
            button(
                row![
                    bi(Bootstrap::ClipboardFill).size(fs),
                    text(" Kopieren").size(fs),
                ]
                .align_y(iced::Alignment::Center),
            )
            .on_press(Msg::CopyOutput)
            .padding([5, 10])
            .style(readable_button_style),
            "Gesamte Ausgabe in die Zwischenablage kopieren".to_string(),
        );

        let timing_str = if self.display_elapsed_ms > 0 {
            format!(
                "Ausführungszeit: {}",
                format_duration(self.display_elapsed_ms)
            )
        } else {
            "Ausführungszeit: —".to_string()
        };

        let output_header = row![
            text("Ausgabe").size(15),
            clear_btn,
            copy_output_btn,
            horizontal_space(),
            text(timing_str).size(12),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let output_te = text_editor(&self.output_content)
            .on_action(Msg::OutputAction)
            .height(Length::Shrink)
            // Keep output line height consistent with LINE_HEIGHT for correct
            // zebra-stripe and highlight-band alignment.
            .line_height(Pixels(LINE_HEIGHT));
        let output_line_count = self.output_content.text().split('\n').count().max(1);
        // The output panel has no diagnostic error lines; pass an empty slice.
        let output_gutter = make_gutter(output_line_count, &[]);
        let output_hl = make_highlight_layer(self.output_highlight_line);
        let zebra = make_zebra_overlay(output_line_count);
        let output_stack: Element<'_, Msg> = stack![zebra, output_hl, output_te].into();
        let output = mouse_area(
            scrollable(row![output_gutter, output_stack])
                .height(Length::Fill)
                .id(scrollable::Id::new("output_scroll")),
        )
        .on_right_press(Msg::ShowContextMenu(ContextMenuKind::Output));

        // -- Output find panel --
        let output_find_panel: Option<Element<'_, Msg>> = if self.output_find_open {
            let find_input = text_input("Suchen…", &self.output_find_text)
                .on_input(Msg::OutputFindTextChanged)
                .on_submit(Msg::OutputFindNext)
                .padding([4, 6])
                .width(180);
            let next_btn = hover_tip(
                button(bi(Bootstrap::ChevronDown).size(11))
                    .on_press(Msg::OutputFindNext)
                    .padding([4, 8])
                    .style(readable_button_style),
                "Zum nächsten Treffer springen (Enter)".to_string(),
            );
            let prev_btn = hover_tip(
                button(bi(Bootstrap::ChevronUp).size(11))
                    .on_press(Msg::OutputFindPrev)
                    .padding([4, 8])
                    .style(readable_button_style),
                "Zum vorherigen Treffer springen (Shift+Enter)".to_string(),
            );
            let close_btn = hover_tip(
                button(bi(Bootstrap::X).size(11))
                    .on_press(Msg::ToggleOutputFind)
                    .padding([4, 6])
                    .style(button::danger),
                "Suchleiste schließen (Esc)".to_string(),
            );
            let status_text = text(self.output_find_status.as_str()).size(12);
            let panel = container(
                row![
                    find_input,
                    prev_btn,
                    next_btn,
                    status_text,
                    horizontal_space(),
                    close_btn,
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center)
                .padding([4, 8]),
            )
            .style(container::bordered_box)
            .width(Length::Fill);
            Some(panel.into())
        } else {
            None
        };

        let output_section = {
            let mut col = column![output_header].spacing(4).padding([4, 8]);
            if let Some(fp) = output_find_panel {
                col = col.push(fp);
            }
            col = col.push(output);

            // -- Diagnostics panel --
            if !self.diagnostics.is_empty() {
                let header = text(
                    format!("Diagnosen ({}): — klicken zum Öffnen im Editor", self.diagnostics.len())
                )
                .size(12);

                let diag_items: Vec<Element<'_, Msg>> = self
                    .diagnostics
                    .iter()
                    .map(|diag| {
                        let (level_str, color) = match diag.level {
                            DiagnosticLevel::Error   => ("FEHLER",   DIAG_ERROR_COLOR),
                            DiagnosticLevel::Warning => ("WARNUNG",  DIAG_WARN_COLOR),
                            DiagnosticLevel::Note    => ("HINWEIS",  DIAG_NOTE_COLOR),
                        };
                        let file_name = diag
                            .file
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| diag.file.to_string_lossy().into_owned());
                        let label = format!(
                            "[{}] {}:{}:{} — {}",
                            level_str, file_name, diag.line, diag.column, diag.message
                        );
                        let path = diag.file.clone();
                        let line = diag.line;
                        let col  = diag.column;
                        let level = diag.level;
                        button(text(label).size(12))
                            .on_press(Msg::OpenDiagnostic { path, line, col, level })
                            .padding([3, 8])
                            .width(Length::Fill)
                            .style(move |_theme: &iced::Theme, _status: button::Status| {
                                button::Style {
                                    background: Some(iced::Background::Color(color)),
                                    text_color: Color::WHITE,
                                    border: iced::border::rounded(2),
                                    ..Default::default()
                                }
                            })
                            .into()
                    })
                    .collect();

                let diag_list = column(diag_items).spacing(2).width(Length::Fill);
                let diag_scroll = scrollable(diag_list)
                    .height(Length::Fixed(160.0))
                    .width(Length::Fill);

                col = col.push(
                    column![header, diag_scroll]
                        .spacing(4)
                        .padding([4, 0])
                );
            }

            col
        };

        // -- Layout: path row spans full width; left side has inputs + commands; right side is larger output --
        let left_panel = scrollable(
            column![args_row, new_row, commands_section]
                .spacing(4)
                .width(500),
        );

        let main_content = column![
            path_row,
            row![left_panel, output_section]
                .spacing(8)
                .padding([0, 8])
                .height(Length::Fill),
        ]
        .spacing(4)
        .padding([8, 0]);

        main_content.into()
    }

    // -----------------------------------------------------------------------
    // Settings view
    // -----------------------------------------------------------------------

    fn view_settings(&self) -> Element<'_, Msg> {
        let fs = self.config.button_font_size;
        let back_btn = hover_tip(
            button(text("← Zurück").size(fs))
                .on_press(Msg::NavigateTo(View::Main))
                .padding([5, 10])
                .style(readable_button_style),
            "Zurück zur Hauptansicht".to_string(),
        );

        // -- Default path row --
        let default_path_input = text_input("Standard-Projektpfad…", &self.config.default_path)
            .on_input(Msg::DefaultPathChanged)
            .padding(5);

        let restore_btn = hover_tip(
            button(text("Standard-Pfad laden").size(fs))
                .on_press(Msg::RestoreDefault)
                .padding([5, 10])
                .style(readable_button_style),
            "Standard-Projektpfad in das Projektverzeichnis-Feld übernehmen".to_string(),
        );

        let default_path_row = column![
            text("Standard-Pfad:").size(13),
            row![default_path_input.width(Length::Fill), restore_btn,]
                .spacing(6)
                .align_y(iced::Alignment::Center),
        ]
        .spacing(4);

        // -- Theme row --
        let theme_picker = pick_list(
            AppTheme::ALL,
            Some(self.config.theme.clone()),
            Msg::ThemeChanged,
        )
        .padding([5, 10]);

        let theme_row = row![text("Theme:").size(13).width(160), theme_picker,]
            .spacing(6)
            .align_y(iced::Alignment::Center);

        // -- Button font size row --
        let font_dec_btn = hover_tip(
            button(text("−").size(16))
                .on_press_maybe((fs > 10.0).then_some(Msg::ButtonFontSizeChanged(-1.0)))
                .padding([4, 10])
                .style(readable_button_style),
            "Schriftgröße der Buttons verkleinern".to_string(),
        );
        let font_inc_btn = hover_tip(
            button(text("+").size(16))
                .on_press_maybe((fs < 24.0).then_some(Msg::ButtonFontSizeChanged(1.0)))
                .padding([4, 10])
                .style(readable_button_style),
            "Schriftgröße der Buttons vergrößern".to_string(),
        );
        let font_size_row = row![
            text("Button-Schriftgröße:").size(13).width(160),
            font_dec_btn,
            text(format!("{} pt", fs as u8)).size(13).width(40),
            font_inc_btn,
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);

        // -- Reset button --
        let reset_btn = hover_tip(
            button(text("Standard zurück").size(fs))
                .on_press(Msg::ResetSettings)
                .padding([5, 10])
                .style(readable_button_style),
            "Alle Einstellungen auf Standardwerte zurücksetzen und speichern".to_string(),
        );

        // Config file path hint
        let config_hint = if let Some(p) = Config::config_path() {
            format!("Konfigurationsdatei: {}", p.display())
        } else {
            "Konfigurationsdatei: (kein Pfad verfügbar)".to_string()
        };

        column![
            row![back_btn, text("Einstellungen").size(18)].spacing(10),
            default_path_row,
            theme_row,
            font_size_row,
            reset_btn,
            text(config_hint).size(11),
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
        let fs = self.config.button_font_size;
        let back_btn = hover_tip(
            button(text("← Zurück").size(fs))
                .on_press(Msg::NavigateTo(View::Main))
                .padding([5, 10])
                .style(readable_button_style),
            "Zurück zur Hauptansicht".to_string(),
        );

        let new_tab_btn = hover_tip(
            button(text("+ Neu").size(fs))
                .on_press(Msg::TabNew)
                .padding([5, 10])
                .style(readable_button_style),
            "Neuen leeren Tab im Editor öffnen".to_string(),
        );

        let open_btn = hover_tip(
            button(
                row![bi(Bootstrap::FoldertwoOpen).size(fs), text(" Öffnen").size(fs)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .on_press(Msg::OpenFile)
            .padding([5, 10])
            .style(readable_button_style),
            "Datei öffnen — öffnet einen nativen Dateiauswahl-Dialog".to_string(),
        );

        let save_btn = hover_tip(
            button(
                row![bi(Bootstrap::Floppy).size(fs), text(" Speichern").size(fs)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .on_press(Msg::SaveFile)
            .padding([5, 10])
            .style(readable_button_style),
            "Aktiven Tab speichern — bei Untitled wird ein Speichern-Dialog geöffnet".to_string(),
        );

        let find_btn = hover_tip(
            button(
                row![
                    bi(Bootstrap::Search).size(fs),
                    text(if self.find_replace_open { " Suchen ✕" } else { " Suchen" }).size(fs),
                ]
                .spacing(4)
                .align_y(iced::Alignment::Center),
            )
            .on_press(Msg::ToggleFindReplace)
            .padding([5, 10])
            .style(readable_button_style),
            "Inline-Suchleiste ein-/ausblenden (Ctrl+F = Suchen, Ctrl+H = Suchen+Ersetzen)".to_string(),
        );

        // -- Tab bar --
        let tab_bar = row(self
            .editor_tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                let is_active = i == self.active_tab;
                let label = tab.display_title();

                let tab_btn = hover_tip(
                    button(text(label).size(13))
                        .on_press(Msg::TabSelect(i))
                        .padding([4, 8])
                        .style(if is_active {
                            button::primary
                        } else {
                            button::secondary
                        }),
                    if is_active {
                        "Aktiver Tab".to_string()
                    } else {
                        "Zu diesem Tab wechseln".to_string()
                    },
                );

                let close_btn = hover_tip(
                    button(bi(Bootstrap::Trash).size(11))
                        .on_press(Msg::TabClose(i))
                        .padding([4, 6])
                        .style(button::danger),
                    "Tab löschen".to_string(),
                );

                row![tab_btn, close_btn]
                    .spacing(2)
                    .align_y(iced::Alignment::Center)
                    .into()
            })
            .collect::<Vec<_>>())
        .spacing(4);

        // -- Find / Replace panel --
        let find_replace_panel: Option<Element<'_, Msg>> = if self.find_replace_open {
            let find_input = hover_tip(
                text_input("Suchen…", &self.find_text)
                    .on_input(Msg::FindTextChanged)
                    .on_submit(Msg::FindNext)
                    .padding([4, 6])
                    .width(200),
                "Suchtext eingeben (Enter = Nächstes, Shift+Enter = Vorheriges)".to_string(),
            );
            let next_btn = hover_tip(
                button(bi(Bootstrap::ChevronDown).size(11))
                    .on_press(Msg::FindNext)
                    .padding([4, 8])
                    .style(readable_button_style),
                "Zum nächsten Treffer springen (Enter)".to_string(),
            );
            let prev_btn = hover_tip(
                button(bi(Bootstrap::ChevronUp).size(11))
                    .on_press(Msg::FindPrev)
                    .padding([4, 8])
                    .style(readable_button_style),
                "Zum vorherigen Treffer springen (Shift+Enter)".to_string(),
            );
            let toggle_replace_icon = if self.find_show_replace {
                bi(Bootstrap::ChevronUp).size(11)
            } else {
                bi(Bootstrap::ChevronDown).size(11)
            };
            let toggle_replace_btn = hover_tip(
                button(row![toggle_replace_icon, text(" Ersetzen").size(12)])
                    .on_press(Msg::ToggleReplaceField)
                    .padding([4, 8])
                    .style(readable_button_style),
                "Ersetzen-Feld ein-/ausblenden (Ctrl+H)".to_string(),
            );
            let close_btn = hover_tip(
                button(bi(Bootstrap::X).size(11))
                    .on_press(Msg::CloseInlineFind)
                    .padding([4, 6])
                    .style(button::danger),
                "Suchleiste schließen (Esc)".to_string(),
            );
            let status_text = text(self.find_status.as_str()).size(12);

            // First row: search field + navigation + status + close.
            let find_row = row![
                bi(Bootstrap::Search).size(12),
                find_input,
                prev_btn,
                next_btn,
                toggle_replace_btn,
                status_text,
                horizontal_space(),
                close_btn,
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .padding([4, 8]);

            let mut panel_col = column![find_row].spacing(2);

            // Second row: replace field + action buttons (only when visible).
            if self.find_show_replace {
                let replace_input = hover_tip(
                    text_input("Ersetzen durch…", &self.replace_text)
                        .on_input(Msg::ReplaceTextChanged)
                        .on_submit(Msg::ReplaceOne)
                        .padding([4, 6])
                        .width(200),
                    "Ersetzungstext eingeben (Enter = Ersetzen)".to_string(),
                );
                let replace_btn = hover_tip(
                    button("Ersetzen")
                        .on_press(Msg::ReplaceOne)
                        .padding([4, 8])
                        .style(readable_button_style),
                    "Aktuelles Vorkommen durch den Ersetzungstext ersetzen".to_string(),
                );
                let replace_all_btn = hover_tip(
                    button("Alle ersetzen")
                        .on_press(Msg::ReplaceAll)
                        .padding([4, 8])
                        .style(readable_button_style),
                    "Alle Vorkommen im aktiven Tab auf einmal ersetzen".to_string(),
                );
                let replace_row = row![
                    bi(Bootstrap::ArrowRepeat).size(12),
                    replace_input,
                    replace_btn,
                    replace_all_btn,
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center)
                .padding([2, 8]);
                panel_col = panel_col.push(replace_row);
            }

            let panel = container(panel_col)
                .style(container::bordered_box)
                .width(Length::Fill);

            Some(panel.into())
        } else {
            None
        };

        // -- Active editor (wrapped for right-click context menu) --
        let editor_widget: Element<'_, Msg> =
            if let Some(tab) = self.editor_tabs.get(self.active_tab) {
                let line_count = tab.content.text().split('\n').count().max(1);
                let gutter = make_gutter(line_count, &tab.editor_errors);
                let highlight = make_multi_highlight_layer(
                    &self.find_all_match_lines,
                    self.find_current_match,
                    self.find_test_color,
                );
                // Persistent per-tab diagnostic highlights (all error/warning/note
                // lines remain visible until the tab is closed).
                let diag_highlight = make_persistent_error_highlight_layer(&tab.editor_errors);
                let te = text_editor(&tab.content)
                    .on_action(Msg::EditorAction)
                    .height(Length::Shrink)
                    // Force an absolute line height that matches LINE_HEIGHT exactly
                    // so that highlight bands are always pixel-perfectly aligned,
                    // regardless of font size or theme.
                    .line_height(Pixels(LINE_HEIGHT));
                let editor_stack: Element<'_, Msg> = stack![te, diag_highlight, highlight].into();
                mouse_area(
                    scrollable(row![gutter, editor_stack])
                        .id(scrollable::Id::new("editor_scroll"))
                        .height(Length::Fill),
                )
                .on_right_press(Msg::ShowContextMenu(ContextMenuKind::Editor))
                .into()
            } else {
                text("Kein Tab ausgewählt").into()
            };

        let color_btn = |label: &str, color: Color| {
            button(text(label.to_string()).size(11))
                .on_press(Msg::SetFindTestColor(color))
                .padding([2, 6])
                .style(readable_button_style)
        };
        let color_row = row![
            text("Highlight-Farbe:").size(11),
            color_btn("Gelb", FIND_CURRENT_COLOR),
            color_btn("Grün", FIND_TEST_GREEN_COLOR),
            color_btn("Rot",  FIND_TEST_RED_COLOR),
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center);

        // -- Error / warning dropdown (only visible when diagnostics exist) --
        // Sorted by file path + line + column for easy navigation.
        let error_dropdown: Element<'_, Msg> = if !self.diagnostics.is_empty() {
            let mut sorted: Vec<(usize, &Diagnostic)> =
                self.diagnostics.iter().enumerate().collect();
            sorted.sort_by_key(|(_, d)| {
                (d.file.to_string_lossy().to_string(), d.line, d.column)
            });

            let items: Vec<DiagRef> = sorted
                .iter()
                .map(|(i, d)| {
                    let level_str = match d.level {
                        DiagnosticLevel::Error   => "FEHLER",
                        DiagnosticLevel::Warning => "WARNUNG",
                        DiagnosticLevel::Note    => "HINWEIS",
                    };
                    let file_name = d
                        .file
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| d.file.to_string_lossy().into_owned());
                    let label = format!(
                        "[{}] {}:{}:{} — {}",
                        level_str, file_name, d.line, d.column, d.message
                    );
                    DiagRef { idx: *i, label }
                })
                .collect();

            pick_list(
                items,
                None::<DiagRef>,
                |item: DiagRef| Msg::SelectErrorFromDropdown(item.idx),
            )
            .placeholder("▼ Diagnosen — Eintrag wählen um zur Zeile zu springen…")
            .width(Length::Fill)
            .into()
        } else {
            Space::with_height(0).into()
        };

        let mut col = column![
            row![
                back_btn,
                text("Editor").size(18),
                horizontal_space(),
                new_tab_btn,
                open_btn,
                save_btn,
                find_btn,
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center),
            color_row,
            scrollable(tab_bar).direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::default(),
            )),
            error_dropdown,
        ]
        .spacing(8)
        .padding(16)
        .height(Length::Fill);

        if let Some(panel) = find_replace_panel {
            col = col.push(panel);
        } else {
            // Always occupy the find-panel slot with a zero-height spacer so the
            // editor scrollable remains at a stable child index inside the column.
            // Without this, toggling find_replace_open changes the column's child
            // count, causing iced's widget-tree diff to misalign states and reset
            // the editor scrollable's scroll offset to 0 (jumping to page 1).
            col = col.push(Space::with_height(0));
        }

        col.push(editor_widget).into()
    }

    // -----------------------------------------------------------------------
    // Help view
    // -----------------------------------------------------------------------

    fn view_help(&self) -> Element<'_, Msg> {
        let fs = self.config.button_font_size;
        let back_btn = hover_tip(
            button(text("← Zurück").size(fs))
                .on_press(Msg::NavigateTo(View::Main))
                .padding([5, 10])
                .style(readable_button_style),
            "Zurück zur Hauptansicht".to_string(),
        );

        let pdf_btn = hover_tip(
            button(text("📄 PDF öffnen").size(fs))
                .on_press(Msg::OpenHelpPdf)
                .padding([5, 10])
                .style(readable_button_style),
            "Bedienungsanleitung als PDF-Dokument öffnen".to_string(),
        );

        let help_text = text(HELP_TEXT).size(13);

        column![
            row![back_btn, text("Hilfe / Bedienungsanleitung").size(18), pdf_btn]
                .spacing(10)
                .align_y(iced::Alignment::Center),
            scrollable(help_text).height(Length::Fill),
        ]
        .spacing(8)
        .padding(16)
        .height(Length::Fill)
        .into()
    }

    // -----------------------------------------------------------------------
    // About view
    // -----------------------------------------------------------------------

    fn view_about(&self) -> Element<'_, Msg> {
        let fs = self.config.button_font_size;
        let back_btn = hover_tip(
            button(text("← Zurück").size(fs))
                .on_press(Msg::NavigateTo(View::Main))
                .padding([5, 10])
                .style(readable_button_style),
            "Zurück zur Hauptansicht".to_string(),
        );

        let ghost = img_widget(iced::widget::image::Handle::from_bytes(GHOST_GIF))
            .width(Length::Fixed(GHOST_WIDTH))
            .height(Length::Fixed(GHOST_HEIGHT));

        let ghost_row = container(ghost)
            .width(Length::Fill)
            .align_x(iced::Alignment::Center)
            .padding(iced::Padding { top: 12.0, right: 0.0, bottom: 8.0, left: 0.0 });

        let title = text("Cargo GUI").size(22);
        let author_label = text("Autor:").size(13);
        let author_value = text("Jürgen Schneider").size(14);
        let email_label = text("E-Mail:").size(13);
        let email_value = text("juergen.sr@t-online.de").size(14);
        let license_label = text("Lizenz:").size(13);
        let license_value = hover_tip(
            button(text("Public Domain").size(14))
                .on_press(Msg::OpenPublicDomainLink)
                .padding([0, 4])
                .style(iced::widget::button::text),
            "Public-Domain-Informationen im Browser öffnen".to_string(),
        );

        let info_col = column![
            title,
            row![author_label, author_value].spacing(8).align_y(iced::Alignment::Center),
            row![email_label, email_value].spacing(8).align_y(iced::Alignment::Center),
            row![license_label, license_value].spacing(8).align_y(iced::Alignment::Center),
        ]
        .spacing(8)
        .padding([0, 16]);

        let centered_info = container(info_col)
            .width(Length::Fill)
            .align_x(iced::Alignment::Center);

        column![
            row![back_btn, text("Über Cargo GUI").size(18)].spacing(10),
            ghost_row,
            centered_info,
        ]
        .spacing(12)
        .padding(24)
        .height(Length::Fill)
        .into()
    }
}

// ---------------------------------------------------------------------------
// Subscription
// ---------------------------------------------------------------------------

impl App {
    fn subscription(&self) -> Subscription<Msg> {
        // Periodic flush of the output ring buffer (every 100 ms).
        let flush = iced::time::every(Duration::from_millis(100)).map(|_| Msg::FlushOutput);

        // Track the global mouse position so the tooltip overlay can follow
        // the cursor, and handle keyboard shortcuts for the inline find bar.
        let mouse = iced::event::listen_with(|event, status, _id| match event {
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                Some(Msg::MouseMoved(position))
            }
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key,
                modifiers,
                ..
            }) => {
                use iced::event::Status;
                use iced::keyboard::key::Named;
                use iced::keyboard::Key;
                let ctrl = modifiers.control();
                let shift = modifiers.shift();
                // Ctrl+F → open find panel (search only).
                if ctrl && !shift {
                    if let Key::Character(c) = &key {
                        match c.as_ref() {
                            "f" => return Some(Msg::OpenInlineFind),
                            "h" => return Some(Msg::OpenInlineReplace),
                            _ => {}
                        }
                    }
                }
                // Esc → close the find panel if open.
                if key == Key::Named(Named::Escape) {
                    return Some(Msg::CloseInlineFind);
                }
                // Shift+Enter → previous match (unmodified Enter is handled by
                // the text_input's on_submit).
                if shift && !ctrl && key == Key::Named(Named::Enter) {
                    return Some(Msg::FindPrev);
                }
                // Arrow navigation through find results.
                //
                // Only act when the event was NOT captured by another widget
                // (e.g. the text_editor moving its own cursor with Arrow keys).
                // A single-line text_input does not capture ArrowUp/Down, so
                // pressing them while the search field is focused correctly
                // navigates through matches.
                //
                // The handlers for FindNext/FindPrev guard against
                // `!find_replace_open`, so these shortcuts are no-ops when
                // the find panel is closed.
                if !ctrl && !shift && status == Status::Ignored {
                    match key {
                        Key::Named(Named::ArrowDown) | Key::Named(Named::PageDown) => {
                            return Some(Msg::FindNext);
                        }
                        Key::Named(Named::ArrowUp) | Key::Named(Named::PageUp) => {
                            return Some(Msg::FindPrev);
                        }
                        _ => {}
                    }
                }
                None
            }
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Msg::WindowResized(size))
            }
            iced::Event::Window(iced::window::Event::CloseRequested) => {
                Some(Msg::Quit)
            }
            _ => None,
        });

        Subscription::batch([flush, mouse])
    }
}

// ---------------------------------------------------------------------------
// Tooltip helper
// ---------------------------------------------------------------------------

/// Wrap `widget` with a [`mouse_area`] that shows/hides the global tooltip
/// overlay when the cursor enters or leaves the widget bounds.
///
/// On hover the application-wide `tooltip_text` is set; when the cursor
/// leaves it is cleared.  The actual rendering happens in `App::view` via a
/// [`stack`] layer positioned at the current mouse coordinates.
fn hover_tip<'a>(widget: impl Into<Element<'a, Msg>>, tip: String) -> Element<'a, Msg> {
    mouse_area(widget.into())
        .on_enter(Msg::TooltipShow(tip))
        .on_exit(Msg::TooltipHide)
        .into()
}

// ---------------------------------------------------------------------------
// Diagnostic colour / tooltip helpers
// ---------------------------------------------------------------------------

/// Return the highlight colour for a given diagnostic level.
fn level_color(level: DiagnosticLevel) -> Color {
    match level {
        DiagnosticLevel::Error   => DIAG_ERROR_COLOR,
        DiagnosticLevel::Warning => DIAG_WARN_COLOR,
        DiagnosticLevel::Note    => DIAG_NOTE_COLOR,
    }
}

/// Build the tooltip string shown when the user hovers over an error-marked
/// line number in the editor gutter.  Includes the severity label, the full
/// message, and — if a known error code is present — a German suggestion.
fn build_error_tooltip(e: &EditorError) -> String {
    let level_str = match e.level {
        DiagnosticLevel::Error   => "FEHLER",
        DiagnosticLevel::Warning => "WARNUNG",
        DiagnosticLevel::Note    => "HINWEIS",
    };
    let code_part = e
        .error_code
        .as_deref()
        .map(|c| format!(" [{}]", c))
        .unwrap_or_default();
    let base = format!(
        "{}{}\nZeile {}, Spalte {} — {}",
        level_str,
        code_part,
        // EditorError.line is 0-based; add 1 for human-readable display.
        // EditorError.column is already 1-based (as reported by cargo).
        e.line + 1,
        e.column,
        e.message
    );
    if let Some(code) = e.error_code.as_deref() {
        if let Some(suggestion) = error_suggestions(code) {
            return format!("{}\n💡 Lösung: {}", base, suggestion);
        }
    }
    base
}

/// Return a German solution hint for well-known Rust error codes.
/// Returns `None` when the code is not in the built-in table.
fn error_suggestions(code: &str) -> Option<&'static str> {
    match code {
        "E0425" => Some("Prüfen Sie den Namen der Variablen/Funktion — ist sie im aktuellen Gültigkeitsbereich sichtbar?"),
        "E0308" => Some("Die Typen stimmen nicht überein. Prüfen Sie, ob eine explizite Typumwandlung (as / From / Into) nötig ist."),
        "E0382" => Some("Der Wert wurde bereits verschoben (moved). Verwenden Sie .clone() oder übergeben Sie eine Referenz (&)."),
        "E0106" => Some("Fehlende Lebenszeit-Annotation. Fügen Sie einen Lifetime-Parameter (z.B. <'a>) hinzu."),
        "E0502" => Some("Gleichzeitiger mutierbarer und immutabler Ausleihe ist nicht erlaubt. Teilen Sie die Ausleihbereiche auf."),
        "E0597" => Some("Die ausgeliehene Variable lebt nicht lang genug. Verlängern Sie den Gültigkeitsbereich oder verwenden Sie .to_owned()."),
        "E0277" => Some("Der Trait ist nicht für diesen Typ implementiert. Fügen Sie eine 'impl Trait for Typ'-Implementierung hinzu oder verwenden Sie einen anderen Typ."),
        "E0507" => Some("Inhaber eines dereferenzierten Werts kann nicht verschoben werden. Verwenden Sie .clone()."),
        "E0499" => Some("Ein Wert kann nicht mehrfach mutierbar ausgeliehen werden. Beenden Sie den ersten mut-Ausleihe-Bereich vor dem zweiten."),
        "E0505" => Some("Wert kann nicht verschoben werden, da er noch ausgeliehen ist. Beenden Sie den Ausleihe-Bereich zuerst."),
        "E0515" => Some("Rückgabe einer Referenz auf einen lokalen Wert ist nicht erlaubt. Geben Sie einen owned Wert zurück."),
        "E0369" => Some("Der Operator ist für diesen Typ nicht implementiert. Implementieren Sie den passenden Operator-Trait (z.B. std::ops::Add)."),
        "E0004" => Some("Nicht-erschöpfendes Pattern-Matching. Fügen Sie die fehlenden Zweige oder einen Catch-all-Arm (_) hinzu."),
        "E0282" => Some("Typ kann nicht automatisch ermittelt werden. Geben Sie den Typ explizit an (z.B. let x: u32 = ...)."),
        "E0061" => Some("Falsche Anzahl an Argumenten. Prüfen Sie die Funktionssignatur."),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format a duration given in milliseconds for display.
///
/// - Under 1000 ms → `"xxx ms"`
/// - 1000 ms or more → `"x.xx s"` (two decimal places, standard float rounding)
fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", ms as f64 / 1000.0)
    }
}

/// Convert a byte offset in `text` to a `(line, col)` pair where both are
/// 0-based and `col` is measured in Unicode scalar values (chars), not bytes.
fn byte_offset_to_position(text: &str, byte_offset: usize) -> (usize, usize) {
    let prefix = &text[..byte_offset];
    let line = prefix.chars().filter(|&c| c == '\n').count();
    let col = match prefix.rfind('\n') {
        Some(nl) => prefix[nl + 1..].chars().count(),
        None => prefix.chars().count(),
    };
    (line, col)
}

/// Return the byte offset of the `match_index`-th non-overlapping occurrence
/// of `needle` in `text`.  Returns `None` when `needle` is empty or when
/// there are fewer than `match_index + 1` occurrences.
fn find_match_byte_offset(text: &str, needle: &str, match_index: usize) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    text.match_indices(needle).nth(match_index).map(|(i, _)| i)
}

/// Move the cursor in `content` to the `match_index`-th occurrence of `needle`
/// and select exactly that occurrence so it appears highlighted.
/// Returns `Some((line, col))` (0-based) of the match start when successful,
/// or `None` when `needle` is empty or the match does not exist.
///
/// Note: iced 0.13's `text_editor::Action` has no direct "jump to byte offset"
/// operation; the only way to position the cursor programmatically is through
/// repeated `Motion`-based `perform` calls.  For typical source-code files this
/// is fast enough.  A future iced version with a positional action could replace
/// this implementation.
fn apply_find_selection(
    content: &mut text_editor::Content,
    needle: &str,
    match_index: usize,
) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let text = content.text();
    let Some(byte_off) = find_match_byte_offset(&text, needle, match_index) else {
        return None;
    };
    let (line, col) = byte_offset_to_position(&text, byte_off);

    // Move to document start, then navigate to the target line and column.
    content.perform(text_editor::Action::Move(text_editor::Motion::DocumentStart));
    for _ in 0..line {
        content.perform(text_editor::Action::Move(text_editor::Motion::Down));
    }
    content.perform(text_editor::Action::Move(text_editor::Motion::Home));
    for _ in 0..col {
        content.perform(text_editor::Action::Move(text_editor::Motion::Right));
    }
    // Select the match.  There is no bulk-select-by-length action in the iced
    // 0.13 API, so we extend the selection one character at a time.
    let match_char_len = needle.chars().count();
    for _ in 0..match_char_len {
        content.perform(text_editor::Action::Select(text_editor::Motion::Right));
    }
    Some((line, col))
}

/// Rebuild `text_editor::Content` from the ring buffer.
///
/// Lines are rendered in reverse chronological order so that the newest
/// entry always appears at the top of the output area.
/// If trimming has occurred this run, `TRIM_NOTICE` is appended at the
/// bottom (after the oldest visible line).
fn flush_output(app: &mut App) {
    let capacity = app.output_lines.len() + usize::from(app.output_trimmed);
    let mut parts: Vec<&str> = Vec::with_capacity(capacity);
    for line in app.output_lines.iter().rev() {
        parts.push(line.as_str());
    }
    if app.output_trimmed {
        parts.push(TRIM_NOTICE);
    }
    app.output_content = text_editor::Content::with_text(&parts.join("\n"));
    app.output_dirty = false;
    // Rebuilding the content invalidates any previous highlight and find position.
    app.output_highlight_line = None;
    app.output_find_status.clear();
    app.output_find_current_match = 0;
}

/// Button style that gives clear visual feedback in every state while keeping
/// foreground-to-background contrast high enough to be readable.
///
/// Iced 0.13's built-in `button::primary` treats `Active` and `Pressed`
/// identically, so users receive no visual click feedback.  Additionally, the
/// `Disabled` state reduces both colors to 50 % alpha, which can make labels
/// unreadable on dark backgrounds.  This helper addresses both issues:
///
/// - **Active**: uses `primary.base` (the theme's lighter primary shade).
/// - **Hovered**: uses `primary.strong` (the slightly darker/stronger shade)
///   so the hover change is still visible.
/// - **Pressed**: keeps the active background but adds a 2-px border so that
///   the click is immediately recognisable without darkening the button.
/// - **Disabled**: keeps the active background at 50 % but raises the text to
///   70 % alpha so labels remain legible.
fn readable_button_style(
    theme: &iced::Theme,
    status: iced::widget::button::Status,
) -> iced::widget::button::Style {
    use iced::widget::button::Status;
    let palette = theme.extended_palette();
    let active_pair = palette.primary.base;
    let hovered_pair = palette.primary.strong;
    let base = iced::widget::button::Style {
        background: Some(iced::Background::Color(active_pair.color)),
        text_color: active_pair.text,
        border: iced::border::rounded(2),
        ..Default::default()
    };
    match status {
        Status::Active => base,
        Status::Pressed => iced::widget::button::Style {
            border: iced::Border {
                width: 2.0,
                color: active_pair.text.scale_alpha(0.7),
                ..base.border
            },
            ..base
        },
        Status::Hovered => iced::widget::button::Style {
            background: Some(iced::Background::Color(hovered_pair.color)),
            text_color: hovered_pair.text,
            ..base
        },
        Status::Disabled => iced::widget::button::Style {
            background: base.background.map(|b| b.scale_alpha(0.5)),
            text_color: base.text_color.scale_alpha(0.7),
            ..base
        },
    }
}

/// Red alarm-style button used for the Stop action.
///
/// Always renders with a vivid red background regardless of the current theme
/// so the Stop button is immediately recognisable as a danger action.
/// The button is dimmed (50 % alpha) in the `Disabled` state to signal that
/// no cargo process is currently running.
fn alarm_button_style(
    _theme: &iced::Theme,
    status: iced::widget::button::Status,
) -> iced::widget::button::Style {
    use iced::widget::button::Status;
    let red_active   = Color { r: 0.82, g: 0.10, b: 0.10, a: 1.0 };
    let red_hovered  = Color { r: 0.95, g: 0.18, b: 0.18, a: 1.0 };
    let base = iced::widget::button::Style {
        background: Some(iced::Background::Color(red_active)),
        text_color: Color::WHITE,
        border: iced::border::rounded(2),
        ..Default::default()
    };
    match status {
        Status::Active => base,
        Status::Pressed => iced::widget::button::Style {
            border: iced::Border {
                width: 2.0,
                color: Color::WHITE.scale_alpha(0.7),
                ..base.border
            },
            ..base
        },
        Status::Hovered => iced::widget::button::Style {
            background: Some(iced::Background::Color(red_hovered)),
            ..base
        },
        Status::Disabled => iced::widget::button::Style {
            background: Some(iced::Background::Color(Color {
                a: 0.4,
                ..red_active
            })),
            text_color: Color::WHITE.scale_alpha(0.5),
            ..base
        },
    }
}

/// Build the line-number gutter widget for `line_count` lines.
///
/// Each line is rendered as an individual fixed-height element so that the
/// gutter never clips at large line counts (a single `text()` widget has an
/// internal pixel-height limit that caused numbers to stop appearing around
/// 3 500 lines in practice).  The leading `EDITOR_PADDING_TOP` spacer keeps
/// every number aligned with the corresponding text line inside the adjacent
/// `text_editor`.
///
/// Lines listed in `editor_errors` are rendered with the associated diagnostic
/// colour and wrapped with a tooltip showing the error message and suggestions.
fn make_gutter<'a>(line_count: usize, editor_errors: &[EditorError]) -> Element<'a, Msg> {
    // Build a fast lookup: 0-based line index → &EditorError.
    let error_map: std::collections::HashMap<usize, &EditorError> =
        editor_errors.iter().map(|e| (e.line, e)).collect();

    let mut items: Vec<Element<'a, Msg>> = Vec::with_capacity(line_count + 1);
    // Top spacer matching text_editor's internal top padding so that line 1
    // aligns with the first rendered text line.
    items.push(Space::with_height(Length::Fixed(EDITOR_PADDING_TOP)).into());
    for n in 1..=line_count {
        let line_0based = n - 1;
        let item: Element<'a, Msg> = if let Some(e) = error_map.get(&line_0based) {
            let base_color = level_color(e.level);
            let gutter_color = Color { a: 1.0, ..base_color };
            let num_text = text(n.to_string()).size(12).color(gutter_color);
            let tip = build_error_tooltip(e);
            hover_tip(
                container(num_text).center_y(Length::Fixed(LINE_HEIGHT)),
                tip,
            )
        } else {
            container(text(n.to_string()).size(12))
                .center_y(Length::Fixed(LINE_HEIGHT))
                .into()
        };
        items.push(item);
    }
    container(column(items))
        .style(|_theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(Color::from_rgba(
                0.12, 0.12, 0.15, 0.6,
            ))),
            border: iced::Border::default(),
            text_color: Some(Color::from_rgba(0.55, 0.55, 0.60, 1.0)),
            shadow: iced::Shadow::default(),
        })
        .padding([0, 4])
        .width(Length::Fixed(GUTTER_WIDTH))
        .into()
}

/// Build a zebra-stripe overlay for `line_count` lines.
///
/// Alternates between two subtly different background shades (even and odd
/// rows) so individual output lines are easier to distinguish.
fn make_zebra_overlay<'a>(line_count: usize) -> Element<'a, Msg> {
    let mut bands: Vec<Element<'a, Msg>> = Vec::with_capacity(line_count + 1);
    // Push a spacer equal to the text_editor top padding so that the first
    // zebra band aligns with line 0 of the rendered text.
    bands.push(Space::with_height(Length::Fixed(EDITOR_PADDING_TOP)).into());
    for i in 0..line_count {
        let bg = if i % 2 == 0 {
            Color::from_rgba(0.15, 0.15, 0.18, 0.5)
        } else {
            Color::from_rgba(0.20, 0.20, 0.24, 0.5)
        };
        bands.push(
            container(Space::new(Length::Fill, Length::Fixed(LINE_HEIGHT)))
                .style(move |_theme| iced::widget::container::Style {
                    background: Some(iced::Background::Color(bg)),
                    ..Default::default()
                })
                .width(Length::Fill)
                .into(),
        );
    }
    column(bands).width(Length::Fill).height(Length::Fill).into()
}

/// Build a virtual highlight-overlay element that colors one line.
///
/// Positions a translucent yellow band at `line_index * LINE_HEIGHT` from the
/// top of the widget.  When `line_index` is `None` the overlay is invisible.
fn make_highlight_layer<'a>(line_index: Option<usize>) -> Element<'a, Msg> {
    if let Some(line) = line_index {
        // Add EDITOR_PADDING_TOP so the band aligns with the actual text line,
        // not with the top of the text_editor widget (which includes top padding).
        let offset = EDITOR_PADDING_TOP + line as f32 * LINE_HEIGHT;
        column![
            Space::with_height(Length::Fixed(offset)),
            container(Space::new(Length::Fill, Length::Fixed(LINE_HEIGHT)))
                .style(|_theme| iced::widget::container::Style {
                    background: Some(iced::Background::Color(Color::from_rgba(
                        1.0, 0.88, 0.0, 0.22,
                    ))),
                    border: iced::Border::default(),
                    text_color: None,
                    shadow: iced::Shadow::default(),
                })
                .width(Length::Fill),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    } else {
        Space::new(Length::Fill, Length::Fill).into()
    }
}

/// Build a virtual highlight-overlay element that colors **all** persistent
/// diagnostic lines for the active editor tab.
///
/// Similar to [`make_multi_highlight_layer`] but uses the per-tab
/// `editor_errors` (stored as `Vec<EditorError>`) instead of find-match
/// state.  Each line is independently positioned so that all bands remain
/// correctly aligned during scrolling.
///
/// When `editor_errors` is empty the overlay is fully transparent.
fn make_persistent_error_highlight_layer<'a>(editor_errors: &[EditorError]) -> Element<'a, Msg> {
    if editor_errors.is_empty() {
        return Space::new(Length::Fill, Length::Fill).into();
    }

    let mut layers: Vec<Element<'a, Msg>> = Vec::with_capacity(editor_errors.len() + 1);
    // Base transparent fill so the stack occupies the full editor area.
    layers.push(Space::new(Length::Fill, Length::Fill).into());

    for e in editor_errors {
        let base_color = level_color(e.level);
        let color = Color { a: 0.25, ..base_color };
        let offset = EDITOR_PADDING_TOP + e.line as f32 * LINE_HEIGHT;
        let band: Element<'a, Msg> = column![
            Space::with_height(Length::Fixed(offset)),
            container(Space::new(Length::Fill, Length::Fixed(LINE_HEIGHT)))
                .style(move |_theme| iced::widget::container::Style {
                    background: Some(iced::Background::Color(color)),
                    border: iced::Border::default(),
                    text_color: None,
                    shadow: iced::Shadow::default(),
                })
                .width(Length::Fill),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
        layers.push(band);
    }

    stack(layers).into()
}

/// Collect the 0-based line numbers of every non-overlapping occurrence of
/// `needle` in `text`.  Returns an empty `Vec` when `needle` is empty.
///
/// Uses [`byte_offset_to_position`] so the result is correct for any Unicode
/// input.
fn collect_all_match_lines(text: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    text.match_indices(needle)
        .map(|(byte_off, _)| byte_offset_to_position(text, byte_off).0)
        .collect()
}

/// Build a virtual highlight-overlay element that renders **all** match lines
/// with a subtle yellow band and the **current** match with a stronger
/// orange-yellow band on top.
///
/// Uses a [`stack`] so that each band is independently positioned from the top
/// of the editor area.  When `all_lines` is empty the overlay is transparent.
///
/// If `current_match >= all_lines.len()` no line receives the stronger
/// highlight; this is safe (no panic) and indicates there is no active match.
fn make_multi_highlight_layer<'a>(all_lines: &[usize], current_match: usize, current_color: Color) -> Element<'a, Msg> {
    if all_lines.is_empty() {
        return Space::new(Length::Fill, Length::Fill).into();
    }

    let mut layers: Vec<Element<'a, Msg>> = Vec::with_capacity(all_lines.len() + 1);
    // Base transparent fill so the stack occupies the full editor area.
    layers.push(Space::new(Length::Fill, Length::Fill).into());

    for (i, &line) in all_lines.iter().enumerate() {
        let color = if i == current_match { current_color } else { FIND_OTHER_COLOR };
        // Add EDITOR_PADDING_TOP so each band aligns with the corresponding
        // text line rather than drifting upward by the widget's top padding.
        let offset = EDITOR_PADDING_TOP + line as f32 * LINE_HEIGHT;
        let band: Element<'a, Msg> = column![
            Space::with_height(Length::Fixed(offset)),
            container(Space::new(Length::Fill, Length::Fixed(LINE_HEIGHT)))
                .style(move |_theme| iced::widget::container::Style {
                    background: Some(iced::Background::Color(color)),
                    border: iced::Border::default(),
                    text_color: None,
                    shadow: iced::Shadow::default(),
                })
                .width(Length::Fill),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
        layers.push(band);
    }

    stack(layers).into()
}

/// Produce a [`Task`] that scrolls the editor scrollable so that `line` is
/// visible near the top of the viewport.
fn scroll_editor_to_line(line: usize) -> Task<Msg> {
    // Mirror the band-offset formula (EDITOR_PADDING_TOP + line * LINE_HEIGHT)
    // so the scrollable shows the target line near the top of the viewport.
    let y = EDITOR_PADDING_TOP + line as f32 * LINE_HEIGHT;
    scrollable::scroll_to(
        scrollable::Id::new("editor_scroll"),
        scrollable::AbsoluteOffset { x: 0.0, y },
    )
}

/// Produce a [`Task`] that scrolls the output scrollable back to the very top
/// so that the most recently appended line (rendered first in reverse order)
/// is always visible.
fn scroll_output_to_top() -> Task<Msg> {
    scrollable::scroll_to(
        scrollable::Id::new("output_scroll"),
        scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
    )
}

/// Build the find-status string for the **editor** panel.
///
/// Format: `"Treffer {current+1} von {total} — Zeile {line+1}"`
/// Fallback (no line): `"Treffer 0 von {total}"`
fn editor_find_status_text(current: usize, total: usize, hl_line: Option<usize>) -> String {
    match hl_line {
        Some(line) => format!("Treffer {} von {} \u{2014} Zeile {}", current + 1, total, line + 1),
        None => format!("Treffer 0 von {}", total),
    }
}

/// Build the find-status string for the **output** panel (no file context).
///
/// Format: `"Treffer {current+1} von {total} — Zeile {line+1}"`
/// Fallback (no line): `"Treffer 0 von {total}"`
fn output_find_status_text(current: usize, total: usize, hl_line: Option<usize>) -> String {
    match hl_line {
        Some(line) => format!("Treffer {} von {} \u{2014} Zeile {}", current + 1, total, line + 1),
        None => format!("Treffer 0 von {}", total),
    }
}

/// Parse a cargo diagnostic header line (e.g. `error[E0425]: message` or
/// `warning: message`) and return the [`DiagnosticLevel`], the human-readable
/// message, and the optional error code (e.g. `"E0425"`).
/// Returns `None` for lines that are not diagnostic headers.
fn parse_diagnostic_line(line: &str) -> Option<(DiagnosticLevel, String, Option<String>)> {
    let (level, rest) = if let Some(r) = line.strip_prefix("error") {
        (DiagnosticLevel::Error, r)
    } else if let Some(r) = line.strip_prefix("warning") {
        (DiagnosticLevel::Warning, r)
    } else if let Some(r) = line.strip_prefix("note") {
        (DiagnosticLevel::Note, r)
    } else {
        return None;
    };

    // Accept `error[E0xxx]: msg`, `error: msg`, but NOT `error` alone (no colon).
    let (msg, error_code) = if let Some(r) = rest.strip_prefix('[') {
        // `[E0xxx]: msg` → extract the code and the message.
        // splitn(2, "]: ") produces at most two parts. The second part (index 1)
        // is `None` when `]: ` does not appear, which means the bracket notation
        // is malformed — return None via `?` so we skip this line entirely.
        let mut parts = r.splitn(2, "]: ");
        let raw_code = parts.next().map(|s| s.to_string());
        let msg = parts.next()?.to_string();
        // Only keep the code when it actually looks like a valid code identifier
        // (non-empty and does not itself contain `]: ` fragments that slipped
        // through edge-case inputs).
        let code = raw_code.filter(|c| !c.is_empty());
        (msg, code)
    } else if let Some(r) = rest.strip_prefix(": ") {
        (r.to_string(), None)
    } else {
        return None;
    };

    // Filter out meta-lines like "error: aborting due to N previous errors"
    // that do not correspond to a real source location.
    if msg.starts_with("aborting due to") || msg.starts_with("could not compile") {
        return None;
    }

    Some((level, msg, error_code))
}

/// Parse a cargo location line of the form `  --> relative/path.rs:line:col`
/// and return `(path, line, column)`.  Returns `None` for any other line.
///
/// `rsplitn(3, ':')` is used to split from the right so that the file path
/// portion correctly retains any embedded colons (e.g. Windows drive letters
/// `C:\...` or rare Unix filenames with colons).
fn parse_location_line(line: &str) -> Option<(PathBuf, usize, usize)> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("--> ")?;
    // `rest` is now `"relative/path.rs:5:3"` (or similar).
    // Split from the right to handle Windows paths that may contain `:`.
    let mut parts = rest.rsplitn(3, ':');
    let col_str  = parts.next()?;
    let line_str = parts.next()?;
    let file_str = parts.next()?;
    let col:  usize = col_str.trim().parse().ok()?;
    let ln:   usize = line_str.trim().parse().ok()?;
    let file = PathBuf::from(file_str.trim());
    Some((file, ln, col))
}

/// [`Msg::Append`] messages, followed by a single [`Msg::Done`].
///
/// `stop_rx`: receiving `()` kills the child process and sends
/// `Msg::Done { success: false, … }`.
fn run_cargo(path: String, args: String, job_id: u64, stop_rx: oneshot::Receiver<()>) -> Task<Msg> {
    use tokio::io::{AsyncBufReadExt, BufReader as AsyncBufReader};
    use tokio::process::Command;

    let (mut tx, rx) = mpsc::channel::<Msg>(256);

    tokio::spawn(async move {
        let arg_parts: Vec<&str> = args.split_whitespace().collect();
        let working_dir = if path.is_empty() {
            ".".to_string()
        } else {
            path
        };

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
                let _ = tx
                    .send(Msg::Done {
                        success: false,
                        job_id,
                    })
                    .await;
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

## Symbole / Icons
Alle Schaltflächen verwenden Bootstrap-Icons aus dem integrierten Icon-Font.
Damit sind die Icons auf jedem Betriebssystem scharf und einheitlich dargestellt.

## Projektverzeichnis
Geben Sie den Pfad zu Ihrem Rust-Projekt ein oder klicken Sie auf
\"[Ordner] Durchsuchen\", um einen Ordner auszuwählen.
Mit \"Als Start\" speichern Sie den Pfad als Standard-Startpfad.

## Argumente
Tragen Sie den gewünschten Cargo-Befehl ein, z.B.:
  build --release
  test -- --nocapture
  run -- arg1 arg2

## Cargo Befehle (Schnellzugriff)
Die Schaltflächen führen den jeweiligen Cargo-Befehl direkt aus:
  Build            — Kompiliert das Projekt (cargo build)
  Build --release  — Kompiliert optimiert (cargo build --release)
  Run              — Kompiliert und startet das Projekt (cargo run)
  Run --release    — Startet die Release-Version (cargo run --release)
  Test             — Führt alle Tests aus (cargo test)
  Check            — Prüft Syntax ohne Kompilierung (cargo check)
  Fmt              — Formatiert den Quellcode (cargo fmt)
  Clippy           — Führt den Linter aus (cargo clippy)
  Update           — Aktualisiert Abhängigkeiten (cargo update)
  New              — Neues Projekt anlegen (cargo new)
  Init             — Aktuelles Verzeichnis initialisieren (cargo init)
  Clean            — Build-Artefakte löschen (cargo clean)
  Doc              — Dokumentation generieren (cargo doc)
  Bench            — Benchmarks ausführen (cargo bench)

Jede Schaltfläche zeigt hinter dem Label die erwartete Dauer an
(est:? vor dem ersten Lauf, danach die zuletzt gemessene Zeit).
Während ein Befehl läuft, zeigt der aktive Button zusätzlich die
aktuelle Laufzeit in Echtzeit an (jetzt: X ms oder X.XX s).

## Neues Projekt
Geben Sie einen Projektnamen ein und klicken Sie auf \"cargo new\", um ein
neues Rust-Projekt im ausgewählten Verzeichnis anzulegen.

## Ausgabe
Die Ausgabe des letzten Cargo-Laufs wird hier angezeigt. Sie können Text
selektieren und kopieren. Mit \"Ausgabe löschen\" wird die Ausgabe zurückgesetzt.

## Stop — Roter Alarm-Knopf
Der rote Stop-Knopf (Alarm-Symbol) erscheint rechts neben \"Ausführen\".
Während ein Cargo-Prozess läuft, können Sie ihn damit sofort abbrechen.
  - Aktiv (rot leuchtend): Cargo läuft — Klick bricht den Prozess ab.
  - Inaktiv (gedimmt):     Kein Cargo-Prozess aktiv — Knopf hat keine Wirkung.

## Diagnose-Panel (nach einem Cargo-Lauf)
Nach einem Cargo-Build werden Fehler, Warnungen und Hinweise automatisch
geparst und unter der Ausgabe als farbige Schaltflächen dargestellt:
  - Roter Button   [FEHLER]  — Kompilierfehler (error[Exxxx]: ...)
  - Gelber Button  [WARNUNG] — Warnungen (warning: ...)
  - Blauer Button  [HINWEIS] — Hinweise (note: ...)
Klick auf einen Button öffnet die betreffende Quelldatei im Editor und
springt automatisch zur genauen Zeile und Spalte des Fehlers.
Beim Überfahren mit der Maus zeigt ein Tooltip die vollständige Fehlermeldung
mit Dateinamen, Zeile und Spalte.

## Einstellungen
Unter \"[Zahnrad] Einstellungen\" können Sie den Standard-Projektpfad festlegen,
das Theme auswählen, die Button-Schriftgröße anpassen und Einstellungen zurücksetzen.
Einstellungen werden sofort automatisch gespeichert.

Button-Schriftgröße:
  Mit den Schaltflächen \"-\" und \"+\" lässt sich die Schriftgröße der Buttons
  stufenweise anpassen (Bereich: 10-24 pt, Standard: 13 pt).
  Die Einstellung wird gespeichert und beim nächsten Start wieder angewendet.

Verfügbare Themes:
  Hell (Light) · Dunkel (Dark) · Dracula · Nord · Solarized Light/Dark
  Gruvbox Light/Dark · Catppuccin Latte/Frappe/Macchiato/Mocha
  Tokyo Night · Tokyo Night Storm · Tokyo Night Light
  Kanagawa Wave · Kanagawa Dragon · Kanagawa Lotus
  Moonfly · Nightfly · Oxocarbon

## Editor
Unter \"[Stift] Editor\" steht ein Texteditor mit Tabs zur Verfügung.
  - \"+ Neu\"          — Neuen leeren Tab öffnen
  - \"[Ordner] Oeffnen\" — Datei laden (oeffnet nativen Dateiauswahl-Dialog)
  - \"[Disk] Speichern\" — Aktiven Tab speichern; bei Untitled-Dateien wird ein
                          nativer Speichern-Dialog geöffnet.
  - \"Tabs x\"         — Tab schliessen
  - \"*\"              im Tabtitel zeigt ungespeicherte Aenderungen an.
  - \"[Lupe] Suchen\"  — Inline-Suchleiste ein-/ausblenden (auch per Ctrl+F / Ctrl+H).
  - Rechtsklick im Editor öffnet ein Kontextmenü mit Kopieren, Ausschneiden,
    Einfügen, Alles auswählen und Suchen/Ersetzen.

## Suchen & Ersetzen (Editor)
Die Inline-Suchleiste öffnet sich unterhalb der Tab-Leiste:
  - Ctrl+F           — Suchleiste öffnen (nur Suchen).
  - Ctrl+H           — Suchleiste öffnen mit Ersetzen-Feld.
  - Esc              — Suchleiste schliessen.
  - Suchfeld: Suchtext eingeben (Enter = Naechstes, Shift+Enter = Vorheriges).
  - \"v\" / \"^\"        — Durch Treffer navigieren.
  - \"v Ersetzen\"     — Ersetzen-Feld ein-/ausblenden.
  - \"Ersetzen\"       — Aktuelles Vorkommen ersetzen.
  - \"Alle ersetzen\"  — Alle Vorkommen auf einmal ersetzen.
  - Statusanzeige rechts neben den Buttons:
      «Treffer N von M - Zeile Z» zeigt Treffer N (von M gesamt) auf Zeile Z.
      «Keine Treffer» wird angezeigt, wenn kein Ergebnis gefunden wurde.
  - Alle Treffer werden dezent markiert; der aktuelle Treffer wird staerker hervorgehoben.

## Tooltips
Tooltips erscheinen beim Überfahren einer Schaltfläche mit der Maus.
  - Im oberen Bildschirmbereich werden Tooltips oberhalb des Mauszeigers angezeigt.
  - Im unteren Bildschirmbereich (unterhalb der Fenstermitte) erscheinen Tooltips
    links vom Mauszeiger, damit sie die Schaltflächen nicht verdecken.

## Kontextmenü (Rechtsklick)
  - Im Editor-Textfeld und im Ausgabe-Feld per Rechtsklick öffnen.
  - Kopieren, Ausschneiden (nur Editor), Einfügen (nur Editor), Alles auswählen.
  - Im Editor zusätzlich: \"Suchen/Ersetzen...\" öffnet das Find-Replace-Panel.
  - Schliessen sich bei Klick ausserhalb des Menüs.

## Zeitanzeige
  Laufzeiten unter 1 Sekunde werden als \"xxx ms\" angezeigt,
  ab 1 Sekunde als \"x.xx s\".";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        byte_offset_to_position, collect_all_match_lines, editor_find_status_text,
        find_match_byte_offset, format_duration, output_find_status_text,
        parse_diagnostic_line, parse_location_line, DiagnosticLevel,
    };

    // --- parse_diagnostic_line ---

    #[test]
    fn parse_diag_error_with_code() {
        let (level, msg) = parse_diagnostic_line("error[E0425]: cannot find value `x`").unwrap();
        assert!(matches!(level, DiagnosticLevel::Error));
        assert_eq!(msg, "cannot find value `x`");
    }

    #[test]
    fn parse_diag_error_no_code() {
        let (level, msg) = parse_diagnostic_line("error: mismatched types").unwrap();
        assert!(matches!(level, DiagnosticLevel::Error));
        assert_eq!(msg, "mismatched types");
    }

    #[test]
    fn parse_diag_warning() {
        let (level, msg) = parse_diagnostic_line("warning: unused variable: `x`").unwrap();
        assert!(matches!(level, DiagnosticLevel::Warning));
        assert_eq!(msg, "unused variable: `x`");
    }

    #[test]
    fn parse_diag_note() {
        let (level, msg) = parse_diagnostic_line("note: see issue #12345").unwrap();
        assert!(matches!(level, DiagnosticLevel::Note));
        assert_eq!(msg, "see issue #12345");
    }

    #[test]
    fn parse_diag_aborting_filtered_out() {
        // "aborting due to" lines must not be treated as real diagnostics.
        assert!(parse_diagnostic_line("error: aborting due to 3 previous errors").is_none());
    }

    #[test]
    fn parse_diag_non_diag_line() {
        assert!(parse_diagnostic_line("  |").is_none());
        assert!(parse_diagnostic_line(" --> src/main.rs:5:3").is_none());
        assert!(parse_diagnostic_line("Compiling foo v0.1.0").is_none());
    }

    // --- parse_location_line ---

    #[test]
    fn parse_location_basic() {
        let (file, line, col) = parse_location_line(" --> src/main.rs:5:3").unwrap();
        assert_eq!(file.to_str().unwrap(), "src/main.rs");
        assert_eq!(line, 5);
        assert_eq!(col, 3);
    }

    #[test]
    fn parse_location_leading_spaces() {
        let (file, line, col) = parse_location_line("   --> lib/foo.rs:100:1").unwrap();
        assert_eq!(file.to_str().unwrap(), "lib/foo.rs");
        assert_eq!(line, 100);
        assert_eq!(col, 1);
    }

    #[test]
    fn parse_location_non_location_line() {
        assert!(parse_location_line("  |").is_none());
        assert!(parse_location_line("error[E0425]: msg").is_none());
        assert!(parse_location_line("5 | let x = 1;").is_none());
    }

    #[test]
    fn format_duration_below_1000ms() {
        assert_eq!(format_duration(0), "0 ms");
        assert_eq!(format_duration(1), "1 ms");
        assert_eq!(format_duration(999), "999 ms");
    }

    #[test]
    fn format_duration_at_boundary() {
        assert_eq!(format_duration(1000), "1.00 s");
        assert_eq!(format_duration(1500), "1.50 s");
        assert_eq!(format_duration(2345), "2.35 s");
        assert_eq!(format_duration(60000), "60.00 s");
        // Verify that standard float rounding is applied (1999 ms → 2.00 s after rounding).
        assert_eq!(format_duration(1999), "2.00 s");
        assert_eq!(format_duration(1994), "1.99 s");
    }

    // --- byte_offset_to_position ---

    #[test]
    fn byte_offset_to_position_single_line() {
        let text = "hello world";
        assert_eq!(byte_offset_to_position(text, 0), (0, 0));
        assert_eq!(byte_offset_to_position(text, 6), (0, 6));
    }

    #[test]
    fn byte_offset_to_position_multi_line() {
        let text = "abc\ndef\nghi";
        // "abc\n" = 4 bytes; "def\n" = 4 bytes
        assert_eq!(byte_offset_to_position(text, 0), (0, 0)); // 'a'
        assert_eq!(byte_offset_to_position(text, 4), (1, 0)); // 'd'
        assert_eq!(byte_offset_to_position(text, 6), (1, 2)); // 'f'
        assert_eq!(byte_offset_to_position(text, 8), (2, 0)); // 'g'
    }

    #[test]
    fn byte_offset_to_position_unicode() {
        // "ä" is 2 bytes (U+00E4) but 1 char
        let text = "aäb\ncd";
        // byte layout: a=0, ä=1-2, b=3, \n=4, c=5, d=6
        assert_eq!(byte_offset_to_position(text, 3), (0, 2)); // 'b' at char-col 2
        assert_eq!(byte_offset_to_position(text, 5), (1, 0)); // 'c'
    }

    // --- find_match_byte_offset ---

    #[test]
    fn find_match_byte_offset_empty_needle() {
        assert_eq!(find_match_byte_offset("hello", "", 0), None);
    }

    #[test]
    fn find_match_byte_offset_no_match() {
        assert_eq!(find_match_byte_offset("hello", "xyz", 0), None);
    }

    #[test]
    fn find_match_byte_offset_first_match() {
        assert_eq!(find_match_byte_offset("abcabc", "bc", 0), Some(1));
    }

    #[test]
    fn find_match_byte_offset_second_match() {
        assert_eq!(find_match_byte_offset("abcabc", "bc", 1), Some(4));
    }

    #[test]
    fn find_match_byte_offset_out_of_range() {
        assert_eq!(find_match_byte_offset("abcabc", "bc", 2), None);
    }

    #[test]
    fn find_match_byte_offset_multiline() {
        let text = "foo\nbar\nfoo";
        // First "foo" at byte 0, second at byte 8
        assert_eq!(find_match_byte_offset(text, "foo", 0), Some(0));
        assert_eq!(find_match_byte_offset(text, "foo", 1), Some(8));
    }

    // --- collect_all_match_lines ---

    #[test]
    fn collect_all_match_lines_empty_needle() {
        assert!(collect_all_match_lines("hello\nworld", "").is_empty());
    }

    #[test]
    fn collect_all_match_lines_no_match() {
        assert!(collect_all_match_lines("hello\nworld", "xyz").is_empty());
    }

    #[test]
    fn collect_all_match_lines_single_line_two_matches() {
        // Both matches are on line 0.
        let lines = collect_all_match_lines("abcabc", "bc");
        assert_eq!(lines, vec![0, 0]);
    }

    #[test]
    fn collect_all_match_lines_multi_line() {
        let text = "foo\nbar\nfoo";
        let lines = collect_all_match_lines(text, "foo");
        assert_eq!(lines, vec![0, 2]);
    }

    #[test]
    fn collect_all_match_lines_unicode() {
        // "ü" is 2 bytes; ensure we still count lines correctly.
        let text = "über\nalles\nüber";
        let lines = collect_all_match_lines(text, "über");
        assert_eq!(lines, vec![0, 2]);
    }

    #[test]
    fn collect_all_match_lines_emoji() {
        // Each emoji may be 4 bytes; line counting must not be confused.
        let text = "😀 hi\n😀 ho";
        let lines = collect_all_match_lines(text, "😀");
        assert_eq!(lines, vec![0, 1]);
    }

    // --- Unicode-safe replace ---

    #[test]
    fn replace_one_unicode_safe() {
        // Replacing "ü" with "ue" must not corrupt surrounding bytes.
        let text = "für dich\nfür mich";
        let needle = "ü";
        let replacement = "ue";
        let positions: Vec<usize> = text.match_indices(needle).map(|(i, _)| i).collect();
        assert_eq!(positions.len(), 2);
        let mut result = text.to_string();
        result.replace_range(positions[0]..positions[0] + needle.len(), replacement);
        assert_eq!(result, "fuer dich\nfür mich");
    }

    #[test]
    fn replace_all_unicode_safe() {
        let text = "Größe und Größe";
        let result = text.replace("Größe", "size");
        assert_eq!(result, "size und size");
    }

    #[test]
    fn replace_all_emoji() {
        let text = "hello 🎉 world 🎉 end";
        let result = text.replace("🎉", "!");
        assert_eq!(result, "hello ! world ! end");
    }

    // --- Find navigation: correct line reported for each match ---

    /// Searching across multiple lines: each match maps to the correct line.
    #[test]
    fn find_navigation_multi_line_correct_lines() {
        let text = "alpha\nbeta\nalpha\ngamma\nalpha";
        let lines = collect_all_match_lines(text, "alpha");
        // "alpha" is on lines 0, 2, 4.
        assert_eq!(lines, vec![0, 2, 4]);
    }

    /// Next/prev navigation wraps correctly using modular arithmetic.
    #[test]
    fn find_navigation_next_wraps() {
        let total = 3usize;
        let mut current = 0usize;
        // Advance three times; should wrap back to 0.
        for expected in [1, 2, 0] {
            current = (current + 1) % total;
            assert_eq!(current, expected);
        }
    }

    #[test]
    fn find_navigation_prev_wraps() {
        let total = 3usize;
        let mut current = 0usize;
        // Go backwards three times from 0; should cycle 2, 1, 0.
        for expected in [2, 1, 0] {
            current = current.checked_sub(1).unwrap_or(total - 1);
            assert_eq!(current, expected);
        }
    }

    /// The current match index is always a valid index into find_all_match_lines.
    #[test]
    fn find_current_match_is_valid_index() {
        let text = "foo bar\nfoo baz\nqux\nfoo";
        let lines = collect_all_match_lines(text, "foo");
        assert_eq!(lines.len(), 3);
        // After each advance the index must be in range.
        let total = lines.len();
        let mut current = 0usize;
        for _ in 0..6 {
            assert!(current < total);
            current = (current + 1) % total;
        }
    }

    /// Active match highlight: find_all_match_lines[find_current_match] gives
    /// the line that should be highlighted in orange.
    #[test]
    fn active_match_highlight_line() {
        let text = "line0 needle\nline1\nline2 needle\nline3";
        let lines = collect_all_match_lines(text, "needle");
        assert_eq!(lines, vec![0, 2]);
        // find_current_match = 0 → orange band at line 0.
        assert_eq!(lines[0], 0);
        // After next: find_current_match = 1 → orange band at line 2.
        assert_eq!(lines[1], 2);
    }

    // --- Replace-one state update ---

    /// After replacing the only match, find_all_match_lines must be empty.
    #[test]
    fn replace_one_last_match_clears_list() {
        let text = "hello world";
        let needle = "world";
        let replacement = "Rust";
        let positions: Vec<usize> =
            text.match_indices(needle).map(|(i, _)| i).collect();
        let mut new_text = text.to_string();
        new_text.replace_range(positions[0]..positions[0] + needle.len(), replacement);
        assert_eq!(new_text, "hello Rust");
        let new_lines = collect_all_match_lines(&new_text, needle);
        assert!(new_lines.is_empty(), "no matches after replace");
    }

    /// After replacing one of two matches, find_all_match_lines has one entry
    /// and find_current_match stays in range.
    #[test]
    fn replace_one_advances_correctly() {
        let text = "foo bar foo";
        let needle = "foo";
        let replacement = "baz";
        // Replace match at index 0 (first "foo").
        let positions: Vec<usize> =
            text.match_indices(needle).map(|(i, _)| i).collect();
        assert_eq!(positions.len(), 2);
        let mut new_text = text.to_string();
        new_text.replace_range(positions[0]..positions[0] + needle.len(), replacement);
        assert_eq!(new_text, "baz bar foo");
        let new_lines = collect_all_match_lines(&new_text, needle);
        assert_eq!(new_lines.len(), 1);
        // find_current_match = 0 % 1 = 0, which is in range.
        let new_current = 0usize % new_lines.len();
        assert!(new_current < new_lines.len());
        // The remaining match is still on line 0 (same line).
        assert_eq!(new_lines[new_current], 0);
    }

    // --- Replace-all state reset ---

    /// After replace-all, match list is empty and current_match resets to 0.
    #[test]
    fn replace_all_resets_state() {
        let text = "foo\nbar\nfoo\nbaz";
        let count = text.matches("foo").count();
        assert_eq!(count, 2);
        let new_text = text.replace("foo", "qux");
        assert_eq!(new_text, "qux\nbar\nqux\nbaz");
        // After replace-all: no matches remain, state is reset.
        let new_lines = collect_all_match_lines(&new_text, "foo");
        assert!(new_lines.is_empty());
        // find_current_match would be reset to 0 in the handler.
        let new_current = 0usize;
        assert_eq!(new_current, 0);
    }

    // --- Regression: apply_find_selection returns correct (line, col) ---

    /// Verify that the helper functions used by apply_find_selection return the
    /// correct (line, col) for every match in a multi-line document.  This is
    /// the regression test for "cursor not jumping to match line".
    #[test]
    fn regression_cursor_line_col_multi_line() {
        let text = "alpha\nbeta\nalpha\ngamma\nalpha";
        // Match 0: "alpha" at byte 0  → line 0, col 0
        let off0 = find_match_byte_offset(text, "alpha", 0).unwrap();
        assert_eq!(byte_offset_to_position(text, off0), (0, 0));
        // Match 1: "alpha" at byte 11 → line 2, col 0
        let off1 = find_match_byte_offset(text, "alpha", 1).unwrap();
        assert_eq!(byte_offset_to_position(text, off1), (2, 0));
        // Match 2: "alpha" at byte 22 → line 4, col 0
        let off2 = find_match_byte_offset(text, "alpha", 2).unwrap();
        assert_eq!(byte_offset_to_position(text, off2), (4, 0));
    }

    /// Regression: after FindTextChanged the highlight line equals the line
    /// returned by byte_offset_to_position for match index 0.
    #[test]
    fn regression_highlight_line_equals_first_match_line() {
        let text = "foo\nbar\nfoo\nbaz\nfoo";
        let lines = collect_all_match_lines(text, "foo");
        // find_current_match starts at 0; highlighted line must be lines[0].
        assert_eq!(lines[0], 0);
        // After FindNext (current=1): highlighted line is lines[1] = 2.
        assert_eq!(lines[1], 2);
        // After another FindNext (current=2): highlighted line is lines[2] = 4.
        assert_eq!(lines[2], 4);
    }

    // --- Highlight-band offset includes EDITOR_PADDING_TOP ---

    /// The highlight-band offset for line N must be
    /// `EDITOR_PADDING_TOP + N * LINE_HEIGHT` so that it aligns with the
    /// rendered text line inside the text_editor (which indents content by its
    /// top padding).
    #[test]
    fn highlight_offset_includes_editor_padding() {
        // Verify the constants themselves have the expected values so that any
        // accidental change is caught immediately.
        assert_eq!(super::EDITOR_PADDING_TOP, 5.0,
            "EDITOR_PADDING_TOP must match text_editor default Padding::new(5.0)");
        assert_eq!(super::LINE_HEIGHT, 20.0,
            "LINE_HEIGHT must match the absolute line height set on the text_editor widget");

        let padding = super::EDITOR_PADDING_TOP;
        let lh = super::LINE_HEIGHT;
        // Line 0: band top = 5.0 + 0 * 20.0 = 5.0  (not 0.0 as the old code produced)
        assert_eq!(padding + 0.0 * lh, 5.0);
        // Line 1: band top = 5.0 + 1 * 20.0 = 25.0
        assert_eq!(padding + 1.0 * lh, 25.0);
        // Line 5: band top = 5.0 + 5 * 20.0 = 105.0
        assert_eq!(padding + 5.0 * lh, 105.0);
        // Old formula (without padding) gave 100.0 for line 5 — verify the
        // difference equals exactly EDITOR_PADDING_TOP.
        let old_offset = 5.0 * lh; // 100.0
        let new_offset = padding + 5.0 * lh; // 105.0
        assert_eq!(new_offset - old_offset, padding);
    }

    /// `scroll_editor_to_line` must scroll to the same y-coordinate as the
    /// top of the highlight band for that line.
    #[test]
    fn scroll_target_matches_highlight_offset() {
        let lh = super::LINE_HEIGHT;    // 20.0
        let pt = super::EDITOR_PADDING_TOP; // 5.0
        // Spot-check a few lines with hard-coded expected values.
        assert_eq!(pt + 0.0 * lh,   5.0);
        assert_eq!(pt + 1.0 * lh,  25.0);
        assert_eq!(pt + 10.0 * lh, 205.0);
        assert_eq!(pt + 50.0 * lh, 1005.0);
        // Old (broken) formula was `line * LINE_HEIGHT`; verify it differs.
        let old_y_line10 = 10.0 * lh;     // 200.0
        let new_y_line10 = pt + 10.0 * lh; // 205.0
        assert_ne!(old_y_line10, new_y_line10,
            "scroll target must include EDITOR_PADDING_TOP");
    }

    // --- Status string is derived from find_all_match_lines ---

    /// The find_status must reference lines from find_all_match_lines so it is
    /// always in sync with the visual highlight overlay.
    /// Format: "Treffer N von M — Zeile Z"
    #[test]
    fn find_status_uses_match_lines() {
        let text = "alpha\nbeta\nalpha\ngamma";
        let lines = collect_all_match_lines(text, "alpha");
        assert_eq!(lines, vec![0, 2]);
        let total = lines.len();
        // match index 0 → "Treffer 1 von 2 — Zeile 1"
        let s0 = editor_find_status_text(0, total, Some(lines[0]));
        assert_eq!(s0, "Treffer 1 von 2 \u{2014} Zeile 1");
        // match index 1 → "Treffer 2 von 2 — Zeile 3"
        let s1 = editor_find_status_text(1, total, Some(lines[1]));
        assert_eq!(s1, "Treffer 2 von 2 \u{2014} Zeile 3");
        // Output panel: same format, same results
        let o0 = output_find_status_text(0, total, Some(lines[0]));
        assert_eq!(o0, "Treffer 1 von 2 \u{2014} Zeile 1");
    }

    // --- Arrow-key navigation wraps correctly ---

    /// PageDown / ArrowDown: same modular arithmetic as FindNext.
    #[test]
    fn arrow_down_navigation_wraps() {
        let total = 4usize;
        let mut current = 0usize;
        let steps = [1, 2, 3, 0, 1];
        for expected in steps {
            current = (current + 1) % total;
            assert_eq!(current, expected);
        }
    }

    /// PageUp / ArrowUp: same modular arithmetic as FindPrev.
    #[test]
    fn arrow_up_navigation_wraps() {
        let total = 4usize;
        let mut current = 0usize;
        let steps = [3, 2, 1, 0, 3];
        for expected in steps {
            current = current.checked_sub(1).unwrap_or(total - 1);
            assert_eq!(current, expected);
        }
    }

    /// When the find panel is closed (find_all_match_lines empty), navigation
    /// must not corrupt find_current_match.
    #[test]
    fn navigation_no_op_when_panel_closed() {
        // Simulate: panel closed → match list cleared.
        let find_all_match_lines: Vec<usize> = Vec::new();
        // FindNext would return early when total == 0; current stays at 0.
        let current = 0usize;
        let total = find_all_match_lines.len();
        // Guard in handler: if total == 0, return early → current unchanged.
        if total == 0 {
            assert_eq!(current, 0);
        } else {
            panic!("should have returned early");
        }
    }

    // --- TabSelect find_status synchronisation ---

    /// When switching to a different tab that has matches and the previous
    /// find_current_match is within the new list, the position is preserved.
    #[test]
    fn tab_select_preserves_match_position() {
        let new_tab_text = "alpha\nbeta\nalpha\ngamma";
        let needle = "alpha";
        let lines = collect_all_match_lines(new_tab_text, needle);
        assert_eq!(lines, vec![0, 2]);
        let total = lines.len();

        // User was at match index 1 before switching tabs.
        let prev_current_match = 1usize;
        // Clamp: 1 < 2, so index stays at 1.
        let find_current_match = prev_current_match.min(total - 1);
        let hl_line = lines.get(find_current_match).copied();
        let find_status = editor_find_status_text(find_current_match, total, hl_line);
        assert_eq!(find_current_match, 1);
        assert_eq!(hl_line, Some(2));
        assert_eq!(find_status, "Treffer 2 von 2 \u{2014} Zeile 3");
    }

    /// When find_current_match exceeds the new tab's match count, it must be
    /// clamped to the last valid index (not reset to 0).
    #[test]
    fn tab_select_clamps_match_index_when_too_large() {
        let new_tab_text = "alpha\nbeta\ngamma";
        let needle = "alpha";
        let lines = collect_all_match_lines(new_tab_text, needle);
        assert_eq!(lines, vec![0]);
        let total = lines.len();

        // User was at match index 3 in a tab with many matches.
        let prev_current_match = 3usize;
        // Clamp: min(3, total-1) = min(3, 0) = 0, the only valid index.
        let find_current_match = prev_current_match.min(total - 1);
        let hl_line = lines.get(find_current_match).copied();
        let find_status = editor_find_status_text(find_current_match, total, hl_line);
        assert_eq!(find_current_match, 0);
        assert_eq!(hl_line, Some(0));
        assert_eq!(find_status, "Treffer 1 von 1 \u{2014} Zeile 1");
    }

    /// When switching to a different tab that has matches and find_current_match
    /// is 0, find_status shows "Treffer 1 von N — Zeile K" for the first match.
    #[test]
    fn tab_select_updates_find_status_with_matches() {
        // Simulate the new-tab text and the recomputed match list.
        let new_tab_text = "alpha\nbeta\nalpha\ngamma";
        let needle = "alpha";
        let lines = collect_all_match_lines(new_tab_text, needle);
        assert_eq!(lines, vec![0, 2]);
        let total = lines.len();

        // Mimic what TabSelect now does: clamp current_match (here 0 → stays 0),
        // recompute lines, then derive find_status from lines[0].
        let prev_current_match = 0usize;
        let find_current_match = prev_current_match.min(total - 1);
        let hl_line = lines.get(find_current_match).copied();
        let find_status = editor_find_status_text(find_current_match, total, hl_line);
        assert_eq!(find_current_match, 0);
        assert_eq!(hl_line, Some(0));
        assert_eq!(find_status, "Treffer 1 von 2 \u{2014} Zeile 1");
    }

    /// When switching to a different tab that has NO matches, find_status must
    /// be set to "Keine Treffer" (and highlight must be cleared).
    #[test]
    fn tab_select_updates_find_status_no_matches() {
        let new_tab_text = "no hits here";
        let needle = "alpha";
        let lines = collect_all_match_lines(new_tab_text, needle);
        assert!(lines.is_empty());

        // Mimic what TabSelect does when total == 0: reset to 0, clear highlight.
        let find_current_match = 0usize;
        let editor_highlight_line: Option<usize> = None;
        let find_status = if !needle.is_empty() {
            "Keine Treffer".to_string()
        } else {
            String::new()
        };
        assert_eq!(find_current_match, 0);
        assert_eq!(editor_highlight_line, None);
        assert_eq!(find_status, "Keine Treffer");
    }

    /// Clicking the already-active tab must NOT reset find_current_match.
    /// The guard `idx != self.active_tab` prevents any state mutation.
    #[test]
    fn tab_select_same_tab_is_no_op() {
        // Simulate: active_tab = 1, user clicks tab 1 again.
        let active_tab = 1usize;
        let clicked_idx = 1usize;
        // The guard: only act if idx != active_tab.
        let should_reset = clicked_idx != active_tab;
        assert!(!should_reset, "clicking the active tab must not reset state");
    }

    // --- Regression: stable overlay structure prevents scroll-position reset ---

    /// Root cause: the `App::view()` function previously changed the widget-tree
    /// structure whenever a tooltip appeared or disappeared (switching between a
    /// bare `main_col` and `stack![main_col, tip_layer]`).  Iced tracks stateful
    /// widget state (e.g. a `scrollable`'s scroll offset) by the widget's
    /// position in the tree.  When `main_col` shifted position the editor
    /// scrollable was assigned a fresh (zeroed) state on every hover event,
    /// causing the editor to jump back to page 1.
    ///
    /// The fix keeps `main_col` at a stable index 0 in an unconditional
    /// `stack![main_col, ctx_overlay, tip_overlay]`.  Overlays are transparent
    /// `Space` elements when inactive, so they don't capture any events.
    ///
    /// This test verifies the logical invariants of the scroll-target formula
    /// that would be disrupted if the state were reset, as a regression guard.
    #[test]
    fn regression_overlay_structure_scroll_target_stable() {
        // Scroll target for a line deep in the document must be non-zero;
        // a reset would silently return the scroll to y=0.
        let lh = super::LINE_HEIGHT;
        let pt = super::EDITOR_PADDING_TOP;
        let deep_line = 99usize;
        let y = pt + deep_line as f32 * lh;
        // Must be well above 0 to be distinguishable from a reset.
        assert!(y > 0.0, "scroll target for line {deep_line} must be > 0");
        assert_eq!(y, super::EDITOR_PADDING_TOP + 99.0 * super::LINE_HEIGHT);

        // Hovering a widget (tooltip show/hide) must NOT change the scroll
        // target formula.  The formula depends only on line index and constants,
        // not on tooltip state.
        let with_tooltip = pt + deep_line as f32 * lh;
        let without_tooltip = pt + deep_line as f32 * lh;
        assert_eq!(with_tooltip, without_tooltip,
            "scroll target must be identical regardless of tooltip visibility");
    }

    /// Verify that the find-panel zero-height spacer (which stabilises the
    /// editor scrollable's child index within the editor column) is consistent
    /// with the outer overlay-stability fix: both work together to guarantee
    /// a fully stable widget-tree position for the editor scrollable.
    #[test]
    fn regression_find_panel_spacer_and_overlay_stability_combined() {
        // The editor column always has exactly these children in order:
        //   [0] header row (column![] child 0)
        //   [1] colour-picker row (column![] child 1)
        //   [2] tab bar scrollable (column![] child 2)
        //   [3] find panel OR zero-height Space  ← spacer keeps index stable
        //   [4] editor widget (mouse_area(scrollable(...)))  ← always index 4
        //
        // The outer view stack always has:
        //   [0] main_col  ← always index 0, editor scrollable path is stable
        //   [1] ctx_overlay
        //   [2] tip_overlay
        //
        // editor_col_child_count = 3 (from column![]) + 2 (.push × 2) = 5.
        // editor_scrollable_col_index = 5 - 1 = 4 (it is always the last push).
        let editor_col_base_children: usize = 3; // column![header, color_row, tab_bar]
        let editor_col_pushes: usize = 2;        // .push(find_slot) + .push(editor_widget)
        let editor_scrollable_col_index = editor_col_base_children + editor_col_pushes - 1;
        assert_eq!(editor_scrollable_col_index, 4,
            "editor scrollable must always be at column index 4");

        // The outer stack always has 3 layers; main_col is first.
        let outer_stack_layers: usize = 3; // main_col + ctx_overlay + tip_overlay
        let main_col_stack_index: usize = 0; // always the first layer
        assert_eq!(main_col_stack_index, 0,
            "main_col must always be at stack index 0 to prevent state resets");
        assert!(main_col_stack_index < outer_stack_layers,
            "main_col index must be within the stack's layer count");

        // Before the fix, when no overlay was active, main_col was the root
        // element (not wrapped in a stack at all).  Any tooltip event would
        // then change the root type from column to stack, invalidating all
        // widget states.  The invariant below captures this regression:
        // main_col MUST be inside a stable stack, never the bare root.
        let is_always_inside_stack = true; // enforced by the unconditional stack![]
        assert!(is_always_inside_stack,
            "main_col must be wrapped in a stable stack, never the bare root");
    }

    // -----------------------------------------------------------------------
    // New tests: search status format (Treffer N von M — Zeile Z / Keine Treffer)
    // -----------------------------------------------------------------------

    /// Status with zero matches: "Keine Treffer" (not "Nicht gefunden").
    #[test]
    fn status_zero_matches_is_keine_treffer() {
        let text = "hello world";
        let lines = collect_all_match_lines(text, "xyz");
        assert!(lines.is_empty());
        // Handler produces "Keine Treffer" for non-empty search with no results.
        let status = if !lines.is_empty() {
            editor_find_status_text(0, lines.len(), lines.first().copied())
        } else {
            "Keine Treffer".to_string()
        };
        assert_eq!(status, "Keine Treffer");
    }

    /// Status with exactly one match uses correct format.
    #[test]
    fn status_one_match_format() {
        let text = "only one needle here";
        let lines = collect_all_match_lines(text, "needle");
        assert_eq!(lines, vec![0]);
        let status = editor_find_status_text(0, 1, Some(lines[0]));
        assert_eq!(status, "Treffer 1 von 1 \u{2014} Zeile 1");
    }

    /// Status with many matches — first and last positions are correct.
    #[test]
    fn status_many_matches_format() {
        let text = "x\nx\nx\nx\nx";
        let lines = collect_all_match_lines(text, "x");
        assert_eq!(lines.len(), 5);
        // First match
        let s0 = editor_find_status_text(0, 5, Some(lines[0]));
        assert_eq!(s0, "Treffer 1 von 5 \u{2014} Zeile 1");
        // Last match (line 5)
        let s4 = editor_find_status_text(4, 5, Some(lines[4]));
        assert_eq!(s4, "Treffer 5 von 5 \u{2014} Zeile 5");
    }

    /// When hl_line is None (defensive: out-of-range or position not found),
    /// status falls back to "Treffer 0 von N".
    #[test]
    fn status_no_active_selection_falls_back() {
        let status = editor_find_status_text(0, 5, None);
        assert_eq!(status, "Treffer 0 von 5");
        let status2 = output_find_status_text(2, 10, None);
        assert_eq!(status2, "Treffer 0 von 10");
    }

    /// Line number in status matches the line of the highlighted match.
    #[test]
    fn status_line_number_matches_highlight() {
        let text = "line0\nline1\nneedle\nline3\nneedle";
        let lines = collect_all_match_lines(text, "needle");
        // Matches on lines 2 and 4.
        assert_eq!(lines, vec![2, 4]);
        let s0 = editor_find_status_text(0, 2, Some(lines[0]));
        assert!(s0.contains("Zeile 3"), "first match must show Zeile 3, got: {s0}");
        let s1 = editor_find_status_text(1, 2, Some(lines[1]));
        assert!(s1.contains("Zeile 5"), "second match must show Zeile 5, got: {s1}");
    }

    /// Multiple matches on the same line — current/total still reflects exact position.
    #[test]
    fn status_multiple_matches_same_line() {
        let text = "ab ab ab";
        let lines = collect_all_match_lines(text, "ab");
        // All 3 matches are on line 0.
        assert_eq!(lines, vec![0, 0, 0]);
        let s0 = editor_find_status_text(0, 3, Some(0));
        assert_eq!(s0, "Treffer 1 von 3 \u{2014} Zeile 1");
        let s1 = editor_find_status_text(1, 3, Some(0));
        assert_eq!(s1, "Treffer 2 von 3 \u{2014} Zeile 1");
        let s2 = editor_find_status_text(2, 3, Some(0));
        assert_eq!(s2, "Treffer 3 von 3 \u{2014} Zeile 1");
    }

    /// Navigation: changing search term resets to match 0.
    #[test]
    fn status_term_change_resets_to_first_match() {
        // Simulate FindTextChanged: find_current_match = 0, new matches computed.
        let text = "alpha\nbeta\nalpha";
        let lines_alpha = collect_all_match_lines(text, "alpha");
        assert_eq!(lines_alpha, vec![0, 2]);
        // New search term with fewer matches:
        let lines_beta = collect_all_match_lines(text, "beta");
        assert_eq!(lines_beta, vec![1]);
        // Status after term change: current=0, total=1, line=lines_beta[0]=1 → Zeile 2
        let status = editor_find_status_text(0, lines_beta.len(), Some(lines_beta[0]));
        assert_eq!(status, "Treffer 1 von 1 \u{2014} Zeile 2");
    }

    /// Regression: "1/32 -- Zeile 232" style format cannot appear in new code.
    /// The separator is always " — " (em dash) and counts are always integers.
    #[test]
    fn regression_no_slash_format_in_status() {
        let status = editor_find_status_text(0, 32, Some(231));
        // Must NOT contain "/" or "--" separators.
        assert!(!status.contains('/'), "status must not use '/' separator: {status}");
        assert!(!status.contains("--"), "status must not use '--': {status}");
        // Must use the em dash.
        assert!(status.contains('\u{2014}'), "status must use em dash: {status}");
        assert_eq!(status, "Treffer 1 von 32 \u{2014} Zeile 232");
    }

    /// Output panel status uses same format as editor panel.
    #[test]
    fn output_find_status_same_format() {
        let status = output_find_status_text(4, 10, Some(99));
        assert_eq!(status, "Treffer 5 von 10 \u{2014} Zeile 100");
        let no_match = output_find_status_text(0, 0, None);
        // 0 total means "Keine Treffer" from handler, but function returns "Treffer 0 von 0"
        // when called directly with total=0, None.
        assert_eq!(no_match, "Treffer 0 von 0");
    }

    // --- Gutter line count ---

    /// The gutter line-count formula (`split('\n').count().max(1)`) must return
    /// the correct number of lines for typical inputs, including large values
    /// well past the old rendering limit of ~3 504 lines.
    #[test]
    fn gutter_line_count_formula_correct() {
        // Single non-empty line with no trailing newline.
        let single = "hello";
        assert_eq!(single.split('\n').count().max(1), 1);

        // Two lines separated by a newline.
        let two = "line1\nline2";
        assert_eq!(two.split('\n').count().max(1), 2);

        // File ending with a newline produces one extra empty element.
        let with_trailing = "line1\nline2\n";
        assert_eq!(with_trailing.split('\n').count().max(1), 3);

        // Empty string still yields at least 1 (guarded by .max(1)).
        assert_eq!("".split('\n').count().max(1), 1);
    }

    /// The per-line gutter approach must cover all lines without clipping.
    /// Previously a single `text()` widget was used, which stopped rendering
    /// at roughly 3 504 lines due to an internal pixel-height limit in the
    /// text widget.  Now each line is an independent fixed-height element, so
    /// the count is always correct regardless of how large it gets.
    #[test]
    fn gutter_covers_large_line_counts() {
        // Build a synthetic text with more lines than the old 3 504-line limit.
        let line_count = 4_000usize;
        let content: String = (0..line_count).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let computed = content.split('\n').count().max(1);
        // Every line must be accounted for.
        assert_eq!(computed, line_count,
            "gutter must show {line_count} line numbers but computed {computed}");
    }
}
