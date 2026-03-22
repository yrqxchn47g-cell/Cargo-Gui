//! Bootstrap-icon helpers for Cargo GUI.
//!
//! The Bootstrap Icons font is bundled by `iced_fonts` and must be loaded at
//! application start via:
//!
//! ```rust
//! iced::application(...)
//!     .font(icons::BOOTSTRAP_FONT_BYTES)
//! ```
//!
//! Every widget that renders an icon character must have its font set to
//! [`BOOTSTRAP_FONT`].  The [`bi`] helper builds such a `Text` widget in
//! one call.

pub use iced_fonts::{Bootstrap, BOOTSTRAP_FONT, BOOTSTRAP_FONT_BYTES};

/// Build a [`Text`](iced::widget::Text) widget that renders a single
/// Bootstrap icon at the font's default size.
///
/// Combine with other [`Text`](iced::widget::Text) widgets inside a
/// [`row!`](iced::widget::row) for icon + label buttons:
///
/// ```rust
/// button(row![bi(Bootstrap::Search), text(" Suchen").size(fs)])
/// ```
pub fn bi(icon: Bootstrap) -> iced::widget::Text<'static> {
    use iced_fonts::bootstrap::icon_to_char;
    iced::widget::text(icon_to_char(icon).to_string())
        .font(BOOTSTRAP_FONT)
        .line_height(1.0)
}
