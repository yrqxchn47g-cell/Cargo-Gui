//! Persistent application configuration.
//!
//! Configuration is stored as TOML in the OS-specific config directory:
//! - **Linux**:   `~/.config/cargo-gui/config.toml`
//! - **macOS**:   `~/Library/Application Support/cargo-gui/config.toml`
//! - **Windows**: `%APPDATA%\cargo-gui\config.toml`
//!
//! The file is written on every change (fields are small, so write latency is
//! negligible). On first launch, or if the file cannot be read/parsed, the
//! application falls back to [`Config::default()`].

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// AppTheme
// ---------------------------------------------------------------------------

/// All theme variants offered in the settings UI.
///
/// | Variant              | Style        | Notes                          |
/// |----------------------|--------------|--------------------------------|
/// | `Light`              | Light        | Iced built-in                  |
/// | `Dark`               | Dark         | Iced built-in                  |
/// | `Dracula`            | Dark         | Purple accent                  |
/// | `Nord`               | Dark-bluish  | Arctic palette                 |
/// | `SolarizedLight`     | Light        | Warm tones                     |
/// | `SolarizedDark`      | Dark         | Warm tones                     |
/// | `GruvboxLight`       | Light        | Retro warm                     |
/// | `GruvboxDark`        | Dark         | Retro warm                     |
/// | `CatppuccinLatte`    | Light        | Pastel                         |
/// | `CatppuccinFrappe`   | Mid-dark     | Pastel                         |
/// | `CatppuccinMacchiato`| Dark         | Pastel                         |
/// | `CatppuccinMocha`    | Dark         | Pastel                         |
/// | `TokyoNight`         | Dark         | Neon-blue                      |
/// | `TokyoNightStorm`    | Dark         | Neon-blue variant              |
/// | `TokyoNightLight`    | Light        | Neon-blue light                |
/// | `KanagawaWave`       | Dark         | Japanese ink                   |
/// | `KanagawaDragon`     | Dark         | Japanese ink variant           |
/// | `KanagawaLotus`      | Light        | Japanese ink light             |
/// | `Moonfly`            | Dark         | Deep ocean                     |
/// | `Nightfly`           | Dark         | Deep blue                      |
/// | `Oxocarbon`          | Dark         | IBM Carbon                     |
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AppTheme {
    #[default]
    Light,
    Dark,
    Dracula,
    Nord,
    SolarizedLight,
    SolarizedDark,
    GruvboxLight,
    GruvboxDark,
    CatppuccinLatte,
    CatppuccinFrappe,
    CatppuccinMacchiato,
    CatppuccinMocha,
    TokyoNight,
    TokyoNightStorm,
    TokyoNightLight,
    KanagawaWave,
    KanagawaDragon,
    KanagawaLotus,
    Moonfly,
    Nightfly,
    Oxocarbon,
}

impl AppTheme {
    /// All variants in display order (used for the settings pick-list).
    pub const ALL: &'static [AppTheme] = &[
        AppTheme::Light,
        AppTheme::Dark,
        AppTheme::Dracula,
        AppTheme::Nord,
        AppTheme::SolarizedLight,
        AppTheme::SolarizedDark,
        AppTheme::GruvboxLight,
        AppTheme::GruvboxDark,
        AppTheme::CatppuccinLatte,
        AppTheme::CatppuccinFrappe,
        AppTheme::CatppuccinMacchiato,
        AppTheme::CatppuccinMocha,
        AppTheme::TokyoNight,
        AppTheme::TokyoNightStorm,
        AppTheme::TokyoNightLight,
        AppTheme::KanagawaWave,
        AppTheme::KanagawaDragon,
        AppTheme::KanagawaLotus,
        AppTheme::Moonfly,
        AppTheme::Nightfly,
        AppTheme::Oxocarbon,
    ];

    /// Convert to the corresponding [`iced::Theme`].
    pub fn to_iced(&self) -> iced::Theme {
        match self {
            AppTheme::Light => iced::Theme::Light,
            AppTheme::Dark => iced::Theme::Dark,
            AppTheme::Dracula => iced::Theme::Dracula,
            AppTheme::Nord => iced::Theme::Nord,
            AppTheme::SolarizedLight => iced::Theme::SolarizedLight,
            AppTheme::SolarizedDark => iced::Theme::SolarizedDark,
            AppTheme::GruvboxLight => iced::Theme::GruvboxLight,
            AppTheme::GruvboxDark => iced::Theme::GruvboxDark,
            AppTheme::CatppuccinLatte => iced::Theme::CatppuccinLatte,
            AppTheme::CatppuccinFrappe => iced::Theme::CatppuccinFrappe,
            AppTheme::CatppuccinMacchiato => iced::Theme::CatppuccinMacchiato,
            AppTheme::CatppuccinMocha => iced::Theme::CatppuccinMocha,
            AppTheme::TokyoNight => iced::Theme::TokyoNight,
            AppTheme::TokyoNightStorm => iced::Theme::TokyoNightStorm,
            AppTheme::TokyoNightLight => iced::Theme::TokyoNightLight,
            AppTheme::KanagawaWave => iced::Theme::KanagawaWave,
            AppTheme::KanagawaDragon => iced::Theme::KanagawaDragon,
            AppTheme::KanagawaLotus => iced::Theme::KanagawaLotus,
            AppTheme::Moonfly => iced::Theme::Moonfly,
            AppTheme::Nightfly => iced::Theme::Nightfly,
            AppTheme::Oxocarbon => iced::Theme::Oxocarbon,
        }
    }
}

impl std::fmt::Display for AppTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            AppTheme::Light => "Hell (Light)",
            AppTheme::Dark => "Dunkel (Dark)",
            AppTheme::Dracula => "Dracula",
            AppTheme::Nord => "Nord",
            AppTheme::SolarizedLight => "Solarized Light",
            AppTheme::SolarizedDark => "Solarized Dark",
            AppTheme::GruvboxLight => "Gruvbox Light",
            AppTheme::GruvboxDark => "Gruvbox Dark",
            AppTheme::CatppuccinLatte => "Catppuccin Latte",
            AppTheme::CatppuccinFrappe => "Catppuccin Frappé",
            AppTheme::CatppuccinMacchiato => "Catppuccin Macchiato",
            AppTheme::CatppuccinMocha => "Catppuccin Mocha",
            AppTheme::TokyoNight => "Tokyo Night",
            AppTheme::TokyoNightStorm => "Tokyo Night Storm",
            AppTheme::TokyoNightLight => "Tokyo Night Light",
            AppTheme::KanagawaWave => "Kanagawa Wave",
            AppTheme::KanagawaDragon => "Kanagawa Dragon",
            AppTheme::KanagawaLotus => "Kanagawa Lotus",
            AppTheme::Moonfly => "Moonfly",
            AppTheme::Nightfly => "Nightfly",
            AppTheme::Oxocarbon => "Oxocarbon",
        };
        write!(f, "{label}")
    }
}

// ---------------------------------------------------------------------------
// HighlightColor
// ---------------------------------------------------------------------------

/// The three highlight-colour variants offered in the editor toolbar.
///
/// The selected variant is persisted in the config file so the application
/// restores the user's choice on the next start.
///
/// | Variant  | Display label | Usage                                 |
/// |----------|---------------|---------------------------------------|
/// | `Yellow` | Gelb          | Default – warm, easy on the eyes      |
/// | `Green`  | Grün          | Alternative colour option             |
/// | `Red`    | Rot           | High-contrast accent                  |
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HighlightColor {
    /// Gelb – the application default.
    Yellow,
    /// Grün.
    Green,
    /// Rot.
    Red,
}

impl Default for HighlightColor {
    /// Returns [`HighlightColor::Yellow`] as the application default.
    fn default() -> Self {
        Self::Yellow
    }
}

/// Returns [`HighlightColor::Yellow`] – used as the `serde` default for the
/// config field so that existing config files without the key still start with
/// the yellow highlight colour selected.
fn default_highlight_color() -> HighlightColor {
    HighlightColor::Yellow
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Default button font size used when no value is stored in the config file.
fn default_button_font_size() -> f32 {
    13.0
}

/// Default window width used when no value is stored in the config file.
fn default_window_width() -> f32 {
    1280.0
}

/// Default window height used when no value is stored in the config file.
fn default_window_height() -> f32 {
    800.0
}

/// Default fullscreen state used when no value is stored in the config file.
fn default_is_fullscreen() -> bool {
    false
}

/// Persistent application settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Default project directory restored via "Als Start" / "Standard-Pfad laden".
    pub default_path: String,
    /// Active UI theme.
    pub theme: AppTheme,
    /// Font size (pt) applied to the main action buttons in the GUI.
    ///
    /// Range: 10 – 24.  Defaults to 13.0 when not set in the config file.
    #[serde(default = "default_button_font_size")]
    pub button_font_size: f32,
    /// Window width in logical pixels, persisted across restarts.
    ///
    /// Minimum enforced value: 800.
    #[serde(default = "default_window_width")]
    pub window_width: f32,
    /// Window height in logical pixels, persisted across restarts.
    ///
    /// Minimum enforced value: 600.
    #[serde(default = "default_window_height")]
    pub window_height: f32,
    /// Whether the window was in fullscreen mode when the application was last closed.
    #[serde(default = "default_is_fullscreen")]
    pub is_fullscreen: bool,
    /// Zuletzt gewählte Highlight-Farbe im Editor-Tab.
    ///
    /// Wird beim Starten der Anwendung wiederhergestellt. Wenn der Wert in der
    /// Config-Datei fehlt (ältere Installation), wird [`HighlightColor::Yellow`]
    /// als Standard verwendet.
    #[serde(default = "default_highlight_color")]
    pub highlight_color: HighlightColor,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_path: String::new(),
            theme: AppTheme::default(),
            button_font_size: default_button_font_size(),
            window_width: default_window_width(),
            window_height: default_window_height(),
            is_fullscreen: default_is_fullscreen(),
            highlight_color: default_highlight_color(),
        }
    }
}

impl Config {
    /// Absolute path to the config file on the current platform.
    ///
    /// Returns `None` when the platform provides no suitable config directory.
    pub fn config_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("io.github", "cargo-gui", "cargo-gui")
            .map(|dirs| dirs.config_dir().join("config.toml"))
    }

    /// Load config from disk.  Falls back to [`Config::default()`] on any
    /// error (file not found, parse error, …).
    pub fn load() -> Self {
        Self::config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist the current config to disk.  Errors are silently ignored so
    /// that a read-only filesystem never crashes the application.
    pub fn save(&self) {
        let Some(path) = Self::config_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = toml::to_string_pretty(self) {
            let _ = std::fs::write(path, s);
        }
    }
}
