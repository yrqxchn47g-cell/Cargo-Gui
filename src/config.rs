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
// Config
// ---------------------------------------------------------------------------

/// Persistent application settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Default project directory restored via "Als Start" / "Standard-Pfad laden".
    pub default_path: String,
    /// Active UI theme.
    pub theme: AppTheme,
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
