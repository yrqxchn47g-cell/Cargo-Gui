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

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use config::{AppTheme, Config};
use futures::channel::mpsc;
use futures::FutureExt as _;
use futures::SinkExt as _;
use iced::widget::{
    button, column, container, horizontal_space, mouse_area, pick_list, row, scrollable, stack,
    text, text_editor, text_input, Space,
};
use iced::{clipboard, Color, Element, Length, Subscription, Task};
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

/// Cargo commands shown in the left column of the "Cargo Befehle" grid.
const COMMANDS_LEFT: &[(&str, &str)] = &[
    ("Build", "build"),
    ("Build --release", "build --release"),
    ("Run", "run"),
    ("Run --release", "run --release"),
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
}

impl EditorTab {
    fn new_untitled(index: usize) -> Self {
        Self {
            title: format!("Untitled-{}", index + 1),
            path: None,
            content: text_editor::Content::new(),
            dirty: false,
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
// Entry point
// ---------------------------------------------------------------------------

fn main() -> iced::Result {
    iced::application("Cargo GUI", App::update, App::view)
        .subscription(App::subscription)
        .theme(|app: &App| app.config.theme.to_iced())
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
    /// Current search string.
    find_text: String,
    /// Current replacement string.
    replace_text: String,
    /// Status message shown in the find/replace panel (e.g. "3 Treffer" or "Nicht gefunden").
    find_status: String,
    /// 0-based index of the match that will be acted on by "Nächstes" / "Ersetzen".
    find_current_match: usize,

    // --- Context menu state ---
    /// Non-None when a context menu is currently visible.
    context_menu: Option<ContextMenuState>,

    // --- Tooltip overlay state ---
    /// Text to show in the global tooltip, or `None` when hidden.
    tooltip_text: Option<String>,
    /// Current mouse cursor X position (in logical pixels).
    mouse_x: f32,
    /// Current mouse cursor Y position (in logical pixels).
    mouse_y: f32,
}

impl App {
    fn new() -> (Self, Task<Msg>) {
        let config = Config::load();
        let project_path = config.default_path.clone();
        let editor_tabs = vec![EditorTab::new_untitled(0)];
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
                find_text: String::new(),
                replace_text: String::new(),
                find_status: String::new(),
                find_current_match: 0,
                context_menu: None,
                tooltip_text: None,
                mouse_x: 0.0,
                mouse_y: 0.0,
            },
            Task::none(),
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

    // --- Find / Replace ---
    /// Toggle the find/replace panel open or closed.
    ToggleFindReplace,
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
    /// Reset all settings to their default values and persist.
    ResetSettings,

    // --- Tooltip overlay ---
    /// Mouse cursor moved to `position`.
    MouseMoved(iced::Point),
    /// A widget has been hovered — show the given tooltip text.
    TooltipShow(String),
    /// The cursor left a widget — hide the tooltip.
    TooltipHide,

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
                if self.running {
                    if let Some(start) = self.run_start {
                        self.display_elapsed_ms = start.elapsed().as_millis() as u64;
                    }
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
                if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                    if matches!(action, text_editor::Action::Edit(_)) {
                        tab.dirty = true;
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
                if idx < self.editor_tabs.len() {
                    self.active_tab = idx;
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

            // --- Find / Replace ---
            Msg::ToggleFindReplace => {
                self.find_replace_open = !self.find_replace_open;
                if !self.find_replace_open {
                    self.find_status.clear();
                }
                Task::none()
            }

            Msg::FindTextChanged(s) => {
                self.find_text = s;
                self.find_current_match = 0;
                self.find_status = count_matches_status(
                    self.editor_tabs.get(self.active_tab),
                    &self.find_text,
                );
                // Immediately jump to and select the first occurrence so the
                // user can see it while still typing.
                let total = count_matches(
                    self.editor_tabs.get(self.active_tab),
                    &self.find_text,
                );
                if total > 0 {
                    if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                        apply_find_selection(&mut tab.content, &self.find_text, 0);
                    }
                }
                Task::none()
            }

            Msg::ReplaceTextChanged(s) => {
                self.replace_text = s;
                Task::none()
            }

            Msg::FindNext => {
                if self.find_text.is_empty() {
                    return Task::none();
                }
                let total = count_matches(
                    self.editor_tabs.get(self.active_tab),
                    &self.find_text,
                );
                if total == 0 {
                    self.find_status = "Nicht gefunden".to_string();
                } else {
                    self.find_current_match =
                        (self.find_current_match + 1) % total;
                    self.find_status =
                        format!("{}/{}", self.find_current_match + 1, total);
                    if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                        apply_find_selection(
                            &mut tab.content,
                            &self.find_text,
                            self.find_current_match,
                        );
                    }
                }
                Task::none()
            }

            Msg::FindPrev => {
                if self.find_text.is_empty() {
                    return Task::none();
                }
                let total = count_matches(
                    self.editor_tabs.get(self.active_tab),
                    &self.find_text,
                );
                if total == 0 {
                    self.find_status = "Nicht gefunden".to_string();
                } else {
                    // Wrap backwards: when at index 0, jump to the last match.
                    self.find_current_match =
                        self.find_current_match.checked_sub(1).unwrap_or(total - 1);
                    self.find_status =
                        format!("{}/{}", self.find_current_match + 1, total);
                    if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                        apply_find_selection(
                            &mut tab.content,
                            &self.find_text,
                            self.find_current_match,
                        );
                    }
                }
                Task::none()
            }

            Msg::ReplaceOne => {
                if self.find_text.is_empty() {
                    return Task::none();
                }
                if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                    let full = tab.content.text();
                    let needle = &self.find_text;
                    let replacement = &self.replace_text;
                    // Collect all match byte-positions.
                    let positions: Vec<usize> = full
                        .match_indices(needle.as_str())
                        .map(|(i, _)| i)
                        .collect();
                    if positions.is_empty() {
                        self.find_status = "Nicht gefunden".to_string();
                    } else {
                        let idx = self.find_current_match % positions.len();
                        let pos = positions[idx];
                        let mut new_text = full.clone();
                        new_text.replace_range(pos..pos + needle.len(), replacement);
                        tab.content = text_editor::Content::with_text(&new_text);
                        tab.dirty = true;
                        // Advance to next match (count may have changed).
                        let new_total = count_matches(Some(tab), needle);
                        if new_total == 0 {
                            self.find_current_match = 0;
                            self.find_status = "Nicht gefunden".to_string();
                        } else {
                            self.find_current_match =
                                self.find_current_match % new_total;
                            self.find_status =
                                format!("{}/{}", self.find_current_match + 1, new_total);
                        }
                    }
                }
                Task::none()
            }

            Msg::ReplaceAll => {
                if self.find_text.is_empty() {
                    return Task::none();
                }
                if let Some(tab) = self.editor_tabs.get_mut(self.active_tab) {
                    let full = tab.content.text();
                    let count = full.matches(self.find_text.as_str()).count();
                    if count == 0 {
                        self.find_status = "Nicht gefunden".to_string();
                    } else {
                        let new_text = full.replace(self.find_text.as_str(), self.replace_text.as_str());
                        tab.content = text_editor::Content::with_text(&new_text);
                        tab.dirty = true;
                        self.find_current_match = 0;
                        self.find_status = format!("{count} ersetzt");
                    }
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

        let main_col: Element<'_, Msg> = column![topbar, body, footer].into();

        // ---- Context menu overlay ----
        let with_ctx: Element<'_, Msg> = if let Some(cm) = &self.context_menu {
            let is_editor = matches!(cm.kind, ContextMenuKind::Editor);
            let dismiss_bg: Element<'_, Msg> = mouse_area(
                Space::new(Length::Fill, Length::Fill),
            )
            .on_press(Msg::HideContextMenu)
            .into();

            // Build menu items.
            let copy_btn = button("📋 Kopieren (Copy)")
                .on_press(Msg::ContextCopy)
                .width(Length::Fill)
                .padding([4, 10]);
            let selectall_btn = button("☰ Alles auswählen (Select All)")
                .on_press(Msg::ContextSelectAll)
                .width(Length::Fill)
                .padding([4, 10]);

            let mut menu_col = column![copy_btn, selectall_btn].spacing(2);

            if is_editor {
                let cut_btn = button("✂ Ausschneiden (Cut)")
                    .on_press(Msg::ContextCut)
                    .width(Length::Fill)
                    .padding([4, 10]);
                let paste_btn = button("📄 Einfügen (Paste)")
                    .on_press(Msg::ContextPaste)
                    .width(Length::Fill)
                    .padding([4, 10]);
                let find_btn = button("🔍 Suchen/Ersetzen…")
                    .on_press(Msg::ToggleFindReplace)
                    .width(Length::Fill)
                    .padding([4, 10]);
                menu_col = menu_col.push(cut_btn).push(paste_btn).push(find_btn);
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

            stack![main_col, dismiss_bg, menu_layer].into()
        } else {
            main_col
        };

        // ---- Tooltip overlay ----
        if let Some(tip) = &self.tooltip_text {
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

            // Position the tooltip above and horizontally centred over the
            // cursor.  See `TOOLTIP_OFFSET_X` / `TOOLTIP_OFFSET_Y` for the
            // exact values.
            let tip_x = (self.mouse_x - TOOLTIP_OFFSET_X).max(0.0);
            let tip_y = (self.mouse_y - TOOLTIP_OFFSET_Y).max(0.0);

            let tip_layer: Element<'_, Msg> = column![
                Space::with_height(Length::Fixed(tip_y)),
                row![Space::with_width(Length::Fixed(tip_x)), tip_box,],
            ]
            .width(Length::Fill)
            .into();

            stack![with_ctx, tip_layer].into()
        } else {
            with_ctx
        }
    }

    // -----------------------------------------------------------------------
    // Topbar
    // -----------------------------------------------------------------------

    fn view_topbar(&self) -> Element<'_, Msg> {
        let menu_btn = hover_tip(button("☰").padding([4, 10]), "Menü".to_string());

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
        let settings_btn = hover_tip(
            button("⚙ Einstellungen")
                .on_press(Msg::NavigateTo(View::Settings))
                .padding([5, 10]),
            "Einstellungen öffnen".to_string(),
        );

        let editor_btn = hover_tip(
            button("✏ Editor")
                .on_press(Msg::NavigateTo(View::Editor))
                .padding([5, 10]),
            "Datei-Editor öffnen".to_string(),
        );

        let help_btn = hover_tip(
            button("? Hilfe")
                .on_press(Msg::NavigateTo(View::Help))
                .padding([5, 10]),
            "Bedienungsanleitung öffnen".to_string(),
        );

        let quit_btn = hover_tip(
            button("✕ Beenden").on_press(Msg::Quit).padding([5, 10]),
            "Anwendung beenden".to_string(),
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

        let browse_btn = hover_tip(
            button("📂 Durchsuchen")
                .on_press(Msg::BrowsePath)
                .padding([5, 10]),
            "Projektordner auswählen".to_string(),
        );

        let set_default_btn = hover_tip(
            button("Als Start")
                .on_press(Msg::SetAsDefault)
                .padding([5, 10]),
            "Diesen Pfad als Standardpfad speichern".to_string(),
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
            .padding(5);

        let run_btn = hover_tip(
            button("▶ Ausführen")
                .on_press_maybe((!self.running).then_some(Msg::Run))
                .padding([5, 10]),
            "Cargo-Befehl ausführen".to_string(),
        );

        let stop_btn = hover_tip(
            button("■ Stop")
                .on_press_maybe(self.running.then_some(Msg::Stop))
                .padding([5, 10]),
            "Laufenden Prozess abbrechen".to_string(),
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
            button("cargo new")
                .on_press_maybe(
                    (!self.running && !self.new_project_name.trim().is_empty())
                        .then_some(Msg::RunCargoNew),
                )
                .padding([5, 10]),
            "Neues Cargo-Projekt anlegen".to_string(),
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
        let make_cmd_btn = |(label, cmd): &(&str, &str)| {
            let cmd_str = cmd.to_string();
            let tip = format!("cargo {cmd}");
            let duration_label = self
                .last_durations
                .get(&cmd_str)
                .map(|&ms| format_duration(ms))
                .unwrap_or_else(|| "?".to_string());
            let btn_label = if self.running && self.running_cmd == cmd_str {
                format!(
                    "{label}\nest:{duration_label} jetzt:{}",
                    format_duration(self.display_elapsed_ms)
                )
            } else {
                format!("{label} (est:{duration_label})")
            };
            hover_tip(
                button(text(btn_label).size(11))
                    .on_press_maybe((!self.running).then_some(Msg::RunCommand(cmd_str)))
                    .width(Length::Fill)
                    .padding([5, 8]),
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
            button("Ausgabe löschen")
                .on_press(Msg::Clear)
                .padding([5, 10]),
            "Ausgabe leeren und Status zurücksetzen".to_string(),
        );

        let output_header = row![text("Ausgabe").size(15), clear_btn,]
            .spacing(8)
            .align_y(iced::Alignment::Center);

        let output = mouse_area(
            text_editor(&self.output_content)
                .on_action(Msg::OutputAction)
                .height(Length::Fill),
        )
        .on_right_press(Msg::ShowContextMenu(ContextMenuKind::Output));

        let output_section = column![output_header, output].spacing(4).padding([4, 8]);

        // -- Layout: path row spans full width; left side has inputs + commands; right side is larger output --
        let left_panel = scrollable(
            column![args_row, new_row, commands_section]
                .spacing(4)
                .width(420),
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
        let back_btn = button("← Zurück")
            .on_press(Msg::NavigateTo(View::Main))
            .padding([5, 10]);

        // -- Default path row --
        let default_path_input = text_input("Standard-Projektpfad…", &self.config.default_path)
            .on_input(Msg::DefaultPathChanged)
            .padding(5);

        let restore_btn = hover_tip(
            button("Standard-Pfad laden")
                .on_press(Msg::RestoreDefault)
                .padding([5, 10]),
            "Standard-Pfad in das Projektverzeichnis-Feld laden".to_string(),
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

        // -- Reset button --
        let reset_btn = hover_tip(
            button("Standard zurück")
                .on_press(Msg::ResetSettings)
                .padding([5, 10]),
            "Alle Einstellungen auf Standardwerte zurücksetzen".to_string(),
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
        let back_btn = hover_tip(
            button("← Zurück")
                .on_press(Msg::NavigateTo(View::Main))
                .padding([5, 10]),
            "Zurück zur Hauptansicht".to_string(),
        );

        let new_tab_btn = hover_tip(
            button("+ Neu").on_press(Msg::TabNew).padding([5, 10]),
            "Neuen leeren Tab öffnen".to_string(),
        );

        let open_btn = hover_tip(
            button("📂 Öffnen").on_press(Msg::OpenFile).padding([5, 10]),
            "Datei öffnen".to_string(),
        );

        let find_replace_toggle_label = if self.find_replace_open {
            "🔍 Suchen ✕"
        } else {
            "🔍 Suchen"
        };
        let find_btn = hover_tip(
            button(find_replace_toggle_label)
                .on_press(Msg::ToggleFindReplace)
                .padding([5, 10]),
            "Suchen & Ersetzen-Panel ein-/ausblenden".to_string(),
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
                    button(text("✕").size(11))
                        .on_press(Msg::TabClose(i))
                        .padding([4, 6])
                        .style(button::danger),
                    "Tab schließen".to_string(),
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
                    .width(180),
                "Suchtext eingeben".to_string(),
            );
            let replace_input = hover_tip(
                text_input("Ersetzen durch…", &self.replace_text)
                    .on_input(Msg::ReplaceTextChanged)
                    .on_submit(Msg::ReplaceOne)
                    .padding([4, 6])
                    .width(180),
                "Ersetzungstext eingeben".to_string(),
            );
            let next_btn = hover_tip(
                button("▼ Nächstes").on_press(Msg::FindNext).padding([4, 8]),
                "Nächstes Vorkommen suchen".to_string(),
            );
            let prev_btn = hover_tip(
                button("▲ Vorheriges").on_press(Msg::FindPrev).padding([4, 8]),
                "Vorheriges Vorkommen suchen".to_string(),
            );
            let replace_btn = hover_tip(
                button("Ersetzen").on_press(Msg::ReplaceOne).padding([4, 8]),
                "Aktuelles Vorkommen ersetzen".to_string(),
            );
            let replace_all_btn = hover_tip(
                button("Alle ersetzen")
                    .on_press(Msg::ReplaceAll)
                    .padding([4, 8]),
                "Alle Vorkommen ersetzen".to_string(),
            );
            let close_btn = hover_tip(
                button("✕")
                    .on_press(Msg::ToggleFindReplace)
                    .padding([4, 6])
                    .style(button::danger),
                "Suchen/Ersetzen-Panel schließen".to_string(),
            );
            let status_text = text(self.find_status.as_str()).size(12);

            let panel = container(
                row![
                    text("Suchen:").size(12),
                    find_input,
                    text("Ersetzen:").size(12),
                    replace_input,
                    prev_btn,
                    next_btn,
                    replace_btn,
                    replace_all_btn,
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

        // -- Active editor (wrapped for right-click context menu) --
        let editor_widget: Element<'_, Msg> =
            if let Some(tab) = self.editor_tabs.get(self.active_tab) {
                let te = text_editor(&tab.content)
                    .on_action(Msg::EditorAction)
                    .height(Length::Fill);
                mouse_area(te)
                    .on_right_press(Msg::ShowContextMenu(ContextMenuKind::Editor))
                    .into()
            } else {
                text("Kein Tab ausgewählt").into()
            };

        let mut col = column![
            row![
                back_btn,
                text("Editor").size(18),
                horizontal_space(),
                find_btn,
                new_tab_btn,
                open_btn
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center),
            scrollable(tab_bar).direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::default(),
            )),
        ]
        .spacing(8)
        .padding(16)
        .height(Length::Fill);

        if let Some(panel) = find_replace_panel {
            col = col.push(panel);
        }

        col.push(editor_widget).into()
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
        // Periodic flush of the output ring buffer (every 100 ms).
        let flush = iced::time::every(Duration::from_millis(100)).map(|_| Msg::FlushOutput);

        // Track the global mouse position so the tooltip overlay can follow
        // the cursor.
        let mouse = iced::event::listen_with(|event, _status, _id| match event {
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                Some(Msg::MouseMoved(position))
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

/// Count the number of non-overlapping occurrences of `needle` in the active
/// tab's text.  Returns 0 when `needle` is empty or no tab is given.
fn count_matches(tab: Option<&EditorTab>, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    tab.map(|t| t.content.text().matches(needle).count())
        .unwrap_or(0)
}

/// Build the status string shown in the find/replace panel.
fn count_matches_status(tab: Option<&EditorTab>, needle: &str) -> String {
    if needle.is_empty() {
        return String::new();
    }
    let n = count_matches(tab, needle);
    if n == 0 {
        "Nicht gefunden".to_string()
    } else {
        format!("{n} Treffer")
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
/// Does nothing when `needle` is empty or the match does not exist.
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
) {
    if needle.is_empty() {
        return;
    }
    let text = content.text();
    let Some(byte_off) = find_match_byte_offset(&text, needle, match_index) else {
        return;
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
}

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

## Stop
Während ein Cargo-Prozess läuft, können Sie ihn mit \"■ Stop\" abbrechen.

## Einstellungen
Unter \"⚙ Einstellungen\" können Sie den Standard-Projektpfad festlegen,
das Theme auswählen und Einstellungen zurücksetzen.
Einstellungen werden sofort automatisch gespeichert.

Verfügbare Themes:
  Hell (Light) · Dunkel (Dark) · Dracula · Nord · Solarized Light/Dark
  Gruvbox Light/Dark · Catppuccin Latte/Frappé/Macchiato/Mocha
  Tokyo Night · Tokyo Night Storm · Tokyo Night Light
  Kanagawa Wave · Kanagawa Dragon · Kanagawa Lotus
  Moonfly · Nightfly · Oxocarbon

## Editor
Unter \"✏ Editor\" steht ein Texteditor mit Tabs zur Verfügung.
  - \"+ Neu\"      — Neuen leeren Tab öffnen
  - \"📂 Öffnen\"  — Datei laden (öffnet nativen Dateiauswahl-Dialog)
  - \"✕\"          — Tab schließen
  - \"*\"          im Tabtitel zeigt ungespeicherte Änderungen an.
  - \"🔍 Suchen\"  — Suchen & Ersetzen-Panel ein-/ausblenden.
  - Rechtsklick im Editor öffnet ein Kontextmenü mit Kopieren, Ausschneiden,
    Einfügen, Alles auswählen und Suchen/Ersetzen.

## Suchen & Ersetzen (Editor)
Das Panel öffnet sich unterhalb der Tab-Leiste:
  - Suchfeld: Suchtext eingeben (Enter = Nächstes).
  - Ersetzen-Feld: Ersetzungstext eingeben.
  - \"▼ Nächstes\" / \"▲ Vorheriges\" — Durch Treffer navigieren.
  - \"Ersetzen\" — Aktuelles Vorkommen ersetzen.
  - \"Alle ersetzen\" — Alle Vorkommen auf einmal ersetzen.
  - Statusanzeige rechts neben den Buttons (Trefferanzahl oder \"Nicht gefunden\").

## Kontextmenü (Rechtsklick)
  - Im Editor-Textfeld und im Ausgabe-Feld per Rechtsklick öffnen.
  - Kopieren, Ausschneiden (nur Editor), Einfügen (nur Editor), Alles auswählen.
  - Im Editor zusätzlich: \"Suchen/Ersetzen…\" öffnet das Find-Replace-Panel.
  - Schließt sich bei Klick außerhalb des Menüs.

## Zeitanzeige
  Laufzeiten unter 1 Sekunde werden als \"xxx ms\" angezeigt,
  ab 1 Sekunde als \"x.xx s\".";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{byte_offset_to_position, find_match_byte_offset, format_duration};

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
}
