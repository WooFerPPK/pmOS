//! Toolkit theme: colour palette consumed by every widget
//! that draws chrome around a window.
//!
//! A [`Theme`] is a plain bag of colours. The toolkit itself
//! only knows about the light theme today; the dark-theme
//! entry is populated opportunistically and is not yet wired
//! into any runtime picker. Theme switching from the
//! settings app (US9 / T184) writes the theme *name* into
//! `/etc/preferences.toml` and any client that cares reloads
//! by constructing a new [`Theme`] from the stored name.
//!
//! This module intentionally has no dependency on
//! `display_proto` or any protocol message — themes are a
//! pure client concern. A future slice may add a
//! `watch_theme()` helper using `fs_watch` on the
//! preferences file, but today a client that wants to react
//! to theme changes just reconstructs its widgets.

use crate::draw::Color;

/// A complete palette for window chrome and default widget
/// backgrounds.
///
/// Field names follow the convention *subject*_*state*, so
/// `titlebar_active` is the titlebar fill for a focused
/// window and `titlebar_inactive` is the titlebar fill for
/// an unfocused window. The toolkit treats "active" as a
/// synonym for "has keyboard focus."
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Short machine-readable identifier, e.g. `"light"` or
    /// `"dark"`. Used by the settings app when writing to
    /// `/etc/preferences.toml`.
    pub name: &'static str,

    /// Default window content background — the colour a
    /// freshly-created [`crate::draw::Canvas`] should be
    /// cleared to before widgets paint over it.
    pub window_background: Color,

    /// Titlebar fill for a focused window.
    pub titlebar_active: Color,
    /// Titlebar fill for an unfocused window.
    pub titlebar_inactive: Color,

    /// App-id text colour on an active titlebar.
    pub titlebar_text_active: Color,
    /// App-id text colour on an inactive titlebar.
    pub titlebar_text_inactive: Color,

    /// 1-pixel window border for a focused window.
    pub border_active: Color,
    /// 1-pixel window border for an unfocused window.
    pub border_inactive: Color,

    /// Default close-button fill. The close button is drawn
    /// as a filled square on the titlebar; this is the
    /// resting colour.
    pub close_button: Color,
    /// Close-button fill when the pointer is hovering over
    /// it. The hover-state setter on [`crate::widget::frame::WindowFrame`]
    /// swaps the fill between [`Self::close_button`] and
    /// this value. Input routing that drives the setter is
    /// the display server's job — today tests toggle it
    /// directly.
    pub close_button_hover: Color,
    /// Colour of the "X" glyph drawn on top of the close
    /// button.
    pub close_button_glyph: Color,
}

impl Theme {
    /// The bundled light theme. Neutral greys with a subtle
    /// blue accent for focused chrome. Chosen so that black
    /// body text on [`Self::window_background`] stays
    /// legible without forcing a specific content palette
    /// on the app.
    pub const LIGHT: Theme = Theme {
        name: "light",
        window_background: Color::rgb(0xf2, 0xf2, 0xf2),
        titlebar_active: Color::rgb(0xd8, 0xdc, 0xe4),
        titlebar_inactive: Color::rgb(0xec, 0xec, 0xec),
        titlebar_text_active: Color::rgb(0x1a, 0x1a, 0x1a),
        titlebar_text_inactive: Color::rgb(0x6a, 0x6a, 0x6a),
        border_active: Color::rgb(0x3b, 0x5b, 0x8d),
        border_inactive: Color::rgb(0xa8, 0xa8, 0xa8),
        close_button: Color::rgb(0xd8, 0xdc, 0xe4),
        close_button_hover: Color::rgb(0xe8, 0x4a, 0x4a),
        close_button_glyph: Color::rgb(0x1a, 0x1a, 0x1a),
    };

    /// Placeholder dark theme. The titlebar / border /
    /// text entries are filled in, but the hover colour
    /// and glyph tint are intentionally borrowed from
    /// [`Self::LIGHT`] until the settings app lands and
    /// someone actually picks dark-mode colours. Treat the
    /// values here as provisional.
    pub const DARK: Theme = Theme {
        name: "dark",
        window_background: Color::rgb(0x1c, 0x1f, 0x26),
        titlebar_active: Color::rgb(0x2b, 0x31, 0x3d),
        titlebar_inactive: Color::rgb(0x22, 0x26, 0x2e),
        titlebar_text_active: Color::rgb(0xec, 0xec, 0xec),
        titlebar_text_inactive: Color::rgb(0x7a, 0x7a, 0x7a),
        border_active: Color::rgb(0x5b, 0x7c, 0xc2),
        border_inactive: Color::rgb(0x3a, 0x3e, 0x46),
        close_button: Color::rgb(0x2b, 0x31, 0x3d),
        close_button_hover: Color::rgb(0xe8, 0x4a, 0x4a),
        close_button_glyph: Color::rgb(0xec, 0xec, 0xec),
    };
}

impl Default for Theme {
    fn default() -> Self {
        Theme::LIGHT
    }
}
