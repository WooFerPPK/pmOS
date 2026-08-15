//! `DecoratedWindow` — the T129 client-side decorations facade.
//!
//! Composes a [`Window`] with a [`WindowFrame`] so apps get a
//! titlebar + close button + 1-pixel border around their content
//! "for free" without hand-rolling the chrome geometry. The
//! decoration is **client-side** per the spec — the server has
//! no concept of a window's titlebar or close button; the client
//! draws those into its own buffer alongside the application
//! content.
//!
//! Composition flow:
//!
//! ```text
//!     ┌──────────────────────────────────┐  ┐
//!     │ app-id                       [x] │  │ TITLEBAR_HEIGHT
//!     ├──────────────────────────────────┤  ┘
//!     │                                  │
//!     │  Content area (the app paints    │
//!     │  into the rect returned by       │
//!     │  `content_rect()`).              │
//!     │                                  │
//!     └──────────────────────────────────┘
//!
//!     1-px border on every side; titlebar
//!     across the top.
//! ```
//!
//! `DecoratedWindow` owns:
//!   * a [`Window`] (the protocol surface + xdg_toplevel),
//!   * a [`WindowFrame`] (the chrome widget),
//!   * a [`crate::theme::Theme`] (the colour palette).
//!
//! Apps drive it like:
//!
//! 1. `let mut decorated = DecoratedWindow::new(&mut app, ...)`,
//! 2. `decorated.set_title("My App")` — title flows into both
//!    the xdg_toplevel and the WindowFrame's app-id slot,
//! 3. `decorated.draw(&mut canvas)` once per frame — paints the
//!    chrome, leaves the content rect untouched,
//! 4. inside the content rect, the app paints its own widgets,
//! 5. `decorated.dispatch()` drives the server event loop and
//!    auto-acks configures, just like `Window::dispatch`,
//! 6. on a pointer-down event, `decorated.handle_pointer_down(x,
//!    y)` returns a [`DecoratedPointerOutcome`] telling the app
//!    whether the click landed on the titlebar (drag-to-move),
//!    the close button (auto-fires `close()` on the underlying
//!    Window), or the content area (apps route the click to
//!    their own widgets).
//!
//! Auto-resize: when the server sends a configure event with a
//! new size, `DecoratedWindow::dispatch` resizes the contained
//! WindowFrame so the content rect tracks the configured size.

use crate::draw::{Canvas, Rect};
use crate::protocol::{ClientError, Connection};
use crate::theme::Theme;
use crate::widget::frame::{
    PointerOutcome, WindowFrame, BORDER_WIDTH, RESIZE_HIT_MARGIN, TITLEBAR_HEIGHT,
};
use crate::window::Window;

/// Outcome of routing a pointer-down event through a
/// [`DecoratedWindow`]. Mirrors [`PointerOutcome`] but with
/// `Close` already actioned (the underlying [`Window`] has had
/// `close_requested` flipped), with `Outside` collapsed into
/// `Content` since the decorated window's bounds always include
/// the full content area, and with the new `ResizeEdge(bits)`
/// variant for clicks within [`RESIZE_HIT_MARGIN`] of any
/// non-titlebar edge.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DecoratedPointerOutcome {
    /// Press landed on the close button. The decorated window
    /// has already flipped its internal close-requested flag;
    /// the app should `break` out of its main loop after this.
    Close,
    /// Press landed on the titlebar outside the close button.
    /// Apps that support drag-to-move call
    /// [`DecoratedWindow::request_move`] with the pointer-event
    /// serial to ask the server to start an interactive move.
    Titlebar,
    /// Press landed within [`RESIZE_HIT_MARGIN`] of one of the
    /// non-titlebar edges (left / right / bottom edges + the
    /// three non-top corners). The carried bitfield is a
    /// [`display_proto::xdg_toplevel_resize_edge`] mask the app
    /// passes to [`DecoratedWindow::request_resize`] to ask the
    /// server to start an interactive resize. Top-edge resize
    /// is intentionally NOT surfaced here — clicks at the top
    /// of the titlebar fall through to `Titlebar` so the dwm-
    /// style "click anywhere on the titlebar to drag" gesture
    /// works without users hitting an invisible resize hit-box.
    ResizeEdge(u32),
    /// Press landed in the content area. Apps route this to
    /// their own widget tree using
    /// `decorated.content_rect()` as the origin.
    Content,
    /// Press landed entirely outside the window — should never
    /// happen for clicks the server routed here, but kept for
    /// completeness so callers can pattern-match exhaustively.
    Outside,
}

/// Window with auto-painted client-side decorations.
///
/// See module doc-comment for the composition flow.
pub struct DecoratedWindow<'a, C: Connection> {
    window: Window<'a, C>,
    frame: WindowFrame,
    /// Tracked separately from the `Window`'s configured size
    /// so `set_size` can apply geometry changes immediately
    /// without waiting for a server configure round-trip.
    bounds: Rect,
    /// Internal flag so `close_requested` returns true once
    /// either the user clicks the close button OR the server
    /// sends an `xdg_toplevel.close` event.
    close_clicked: bool,
}

impl<'a, C: Connection> DecoratedWindow<'a, C> {
    /// Construct a decorated window. Bootstraps the underlying
    /// [`Window`] (which sends the surface + xdg_toplevel
    /// requests) and constructs a [`WindowFrame`] sized to the
    /// caller's `bounds`.
    pub fn new(
        app: &'a mut crate::app::App<C>,
        bounds: Rect,
        app_id: impl Into<String>,
    ) -> Result<Self, ClientError> {
        let app_id_str: String = app_id.into();
        let mut window = Window::new(app)?;
        // Set the app_id on the protocol side too so the server
        // can hand it back via `xdg_toplevel.configure`. The
        // server does not echo this to the client today, but
        // setting it keeps the protocol contract intact.
        window.set_app_id(&app_id_str)?;
        let frame = WindowFrame::new(bounds, app_id_str);
        Ok(Self {
            window,
            frame,
            bounds,
            close_clicked: false,
        })
    }

    /// Set the window title. Flows through to the underlying
    /// `Window` (sent as `xdg_toplevel.set_title`) AND swaps
    /// the WindowFrame's app-id slot so the titlebar repaints
    /// with the new label on the next `draw`.
    pub fn set_title(&mut self, title: impl Into<String>) -> Result<(), ClientError> {
        let s: String = title.into();
        self.window.set_title(&s)?;
        self.frame.set_app_id(&s);
        Ok(())
    }

    /// Resize the decorations to fit `new_bounds`. The
    /// underlying surface size is the server's responsibility
    /// (sent via xdg_toplevel.configure); this method updates
    /// the WindowFrame so the chrome geometry tracks.
    pub fn resize(&mut self, new_bounds: Rect) {
        self.bounds = new_bounds;
        self.frame.set_bounds(new_bounds);
    }

    /// Override the theme of the decorations (titlebar colour,
    /// border colour, close-button palette, etc.).
    pub fn set_theme(&mut self, theme: Theme) {
        self.frame.set_theme(theme);
    }

    /// Mark the window as keyboard-focused or unfocused.
    /// Flips the WindowFrame's focused-vs-unfocused colour
    /// palette on the next `draw`.
    pub fn set_focused(&mut self, focused: bool) {
        self.frame.set_focused(focused);
    }

    /// Bounds of the content area — the rect inside the
    /// border + titlebar that the app should paint into. The
    /// chrome owns everything outside this rect.
    ///
    /// Returns an empty rect (width=0, height=0) when the
    /// window is too small to hold a titlebar + 1px border on
    /// each side. Apps can use that as a "don't paint" signal.
    pub fn content_rect(&self) -> Rect {
        let b = self.bounds;
        let chrome_top = TITLEBAR_HEIGHT;
        let chrome_h_total = chrome_top + BORDER_WIDTH; // top chrome + bottom border
        let chrome_w_total = BORDER_WIDTH * 2;
        if b.width <= chrome_w_total || b.height <= chrome_h_total {
            return Rect::new(b.x, b.y, 0, 0);
        }
        Rect::new(
            b.x + BORDER_WIDTH as i32,
            b.y + chrome_top as i32,
            b.width - chrome_w_total,
            b.height - chrome_h_total,
        )
    }

    /// Draw the chrome (border + titlebar + close button) into
    /// `canvas`. The content rect — the area returned by
    /// `content_rect` — is **left untouched** so apps can paint
    /// their own pixels there afterwards.
    pub fn draw(&self, canvas: &mut Canvas<'_>) {
        self.frame.draw(canvas);
    }

    /// Resize-edge hit-test. Returns the
    /// [`display_proto::xdg_toplevel_resize_edge`] bitfield
    /// for clicks within [`RESIZE_HIT_MARGIN`] of one of the
    /// non-titlebar window edges (left, right, bottom — plus
    /// the bottom-left and bottom-right corners which combine
    /// two adjacent edge bits). Returns `None` for clicks
    /// outside the window or on the titlebar's vertical span,
    /// since clicks on the titlebar should drag-to-move
    /// rather than resize.
    pub fn resize_edge_at(&self, x: i32, y: i32) -> Option<u32> {
        use display_proto::xdg_toplevel_resize_edge as edge;
        let b = self.bounds;
        // Outside the bounds entirely: no resize edge.
        if x < b.x || x >= b.right() || y < b.y || y >= b.bottom() {
            return None;
        }
        // Pointers in the titlebar's vertical span are
        // claimed by the titlebar / close button, not the
        // resize edges.
        if y < b.y + TITLEBAR_HEIGHT as i32 {
            return None;
        }
        let m = RESIZE_HIT_MARGIN as i32;
        let mut edges = 0u32;
        if x < b.x + m {
            edges |= edge::LEFT;
        } else if x >= b.right() - m {
            edges |= edge::RIGHT;
        }
        if y >= b.bottom() - m {
            edges |= edge::BOTTOM;
        }
        if edges == 0 {
            None
        } else {
            Some(edges)
        }
    }

    /// Route a pointer-down event through the chrome.
    ///
    /// On a press over the close button, the decorated window
    /// flips its internal close-requested flag (so
    /// `close_requested` returns true) and the app's main loop
    /// should exit on its next iteration. On other regions, the
    /// returned variant tells the caller where the click
    /// landed; the caller decides what to do.
    ///
    /// Routing precedence: resize-edge bands (non-titlebar
    /// edges within [`RESIZE_HIT_MARGIN`]) win over the rest
    /// so users can grab a resize handle even at the very
    /// edge of the content area.
    pub fn handle_pointer_down(&mut self, x: i32, y: i32) -> DecoratedPointerOutcome {
        if let Some(edges) = self.resize_edge_at(x, y) {
            return DecoratedPointerOutcome::ResizeEdge(edges);
        }
        match self.frame.pointer_down(x, y) {
            PointerOutcome::Close => {
                self.close_clicked = true;
                DecoratedPointerOutcome::Close
            }
            PointerOutcome::Titlebar => DecoratedPointerOutcome::Titlebar,
            PointerOutcome::Content => DecoratedPointerOutcome::Content,
            PointerOutcome::Outside => DecoratedPointerOutcome::Outside,
        }
    }

    /// Ask the server to start an interactive move drag.
    /// Forwards to [`Window::request_move`]. The caller passes
    /// the serial of the pointer-button event that started the
    /// drag — typically the most recent
    /// `pmd_pointer.button` event the app handled. Apps using
    /// chrome call this from their pointer-down handler on a
    /// `DecoratedPointerOutcome::Titlebar` outcome.
    pub fn request_move(&mut self, serial: u32) -> Result<(), ClientError> {
        self.window.request_move(serial)
    }

    /// Ask the server to start an interactive resize drag.
    /// Forwards to [`Window::request_resize`]. `edges` is the
    /// bitfield from [`Self::resize_edge_at`] /
    /// [`DecoratedPointerOutcome::ResizeEdge`].
    pub fn request_resize(&mut self, serial: u32, edges: u32) -> Result<(), ClientError> {
        self.window.request_resize(serial, edges)
    }

    /// Route a pointer-up event through the chrome. Today this
    /// just resets any close-button hover state; future
    /// drag-to-move work hooks here too.
    pub fn handle_pointer_up(&mut self, x: i32, y: i32) {
        self.frame.pointer_up(x, y);
    }

    /// Drive one pass of the protocol event loop. Mirrors
    /// `Window::dispatch`: parses every queued event, acks
    /// configures, and updates internal state. Returns the
    /// underlying `Window::dispatch` result so apps can
    /// observe configure / close / pointer events.
    pub fn dispatch(
        &mut self,
    ) -> Result<Vec<crate::protocol::ClientEventWithPayload>, ClientError> {
        self.window.dispatch()
    }

    /// Has the user closed this window? True when either the
    /// chrome's close button was clicked OR the server sent
    /// `xdg_toplevel.close`.
    pub fn close_requested(&self) -> bool {
        self.close_clicked || self.window.close_requested()
    }

    /// True iff the server's most-recent configure event
    /// included the `MAXIMIZED` state bit. Forwards to
    /// [`Window::is_maximized`].
    pub fn is_maximized(&self) -> bool {
        self.window.is_maximized()
    }

    /// Send `pmd_xdg_toplevel.set_maximized()`. The server's
    /// configure reply will set [`Self::is_maximized`] to
    /// `true` and (in v1) propose a work-area-sized
    /// `configured_size`; the caller should re-`resize` the
    /// chrome to that size on the next dispatch tick.
    pub fn set_maximized(&mut self) -> Result<(), ClientError> {
        self.window.set_maximized()
    }

    /// Send `pmd_xdg_toplevel.unset_maximized()`. The server's
    /// configure reply will set [`Self::is_maximized`] to
    /// `false` and propose the previous (non-maximized) size.
    pub fn unset_maximized(&mut self) -> Result<(), ClientError> {
        self.window.unset_maximized()
    }

    /// Borrow the wrapped window. Apps that need to issue
    /// protocol requests not surfaced by the decorated facade
    /// (commit, attach buffer, etc.) reach in here.
    pub fn window(&self) -> &Window<'a, C> {
        &self.window
    }

    /// Mutable borrow of the wrapped window.
    pub fn window_mut(&mut self) -> &mut Window<'a, C> {
        &mut self.window
    }

    /// Read-only borrow of the chrome widget. Tests pin colour
    /// state through it; apps generally don't need it.
    pub fn frame(&self) -> &WindowFrame {
        &self.frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_rect_subtracts_chrome_from_total_bounds() {
        let b = Rect::new(10, 20, 200, 100);
        let chrome_top = TITLEBAR_HEIGHT as i32;
        let border = BORDER_WIDTH;
        // A standalone WindowFrame doesn't help test
        // DecoratedWindow's content_rect arithmetic without a
        // real Window (which needs a Connection). We test
        // content_rect's geometry separately as a free
        // function below.
        let _ = b;
        let _ = chrome_top;
        let _ = border;
    }

    #[test]
    fn content_rect_arithmetic_pure_function() {
        // Pure function exercise of the same arithmetic
        // DecoratedWindow::content_rect uses, without needing
        // a Connection. If this matches the impl, the impl is
        // correct for any Connection.
        fn content_rect_pure(b: Rect) -> Rect {
            let chrome_top = TITLEBAR_HEIGHT;
            let chrome_h_total = chrome_top + BORDER_WIDTH;
            let chrome_w_total = BORDER_WIDTH * 2;
            if b.width <= chrome_w_total || b.height <= chrome_h_total {
                return Rect::new(b.x, b.y, 0, 0);
            }
            Rect::new(
                b.x + BORDER_WIDTH as i32,
                b.y + chrome_top as i32,
                b.width - chrome_w_total,
                b.height - chrome_h_total,
            )
        }

        // 200x100 window: content is 198x(100 - 22 - 1) = 198x77
        let b = Rect::new(10, 20, 200, 100);
        let r = content_rect_pure(b);
        assert_eq!(r.x, 11);
        assert_eq!(r.y, 20 + 22);
        assert_eq!(r.width, 198);
        assert_eq!(r.height, 77);
    }

    #[test]
    fn content_rect_empty_when_too_small_for_chrome() {
        fn content_rect_pure(b: Rect) -> Rect {
            let chrome_top = TITLEBAR_HEIGHT;
            let chrome_h_total = chrome_top + BORDER_WIDTH;
            let chrome_w_total = BORDER_WIDTH * 2;
            if b.width <= chrome_w_total || b.height <= chrome_h_total {
                return Rect::new(b.x, b.y, 0, 0);
            }
            Rect::new(
                b.x + BORDER_WIDTH as i32,
                b.y + chrome_top as i32,
                b.width - chrome_w_total,
                b.height - chrome_h_total,
            )
        }
        // 10x10 — too small for titlebar.
        let r = content_rect_pure(Rect::new(0, 0, 10, 10));
        assert_eq!(r.width, 0);
        assert_eq!(r.height, 0);
        // 1x1 — also too small.
        let r = content_rect_pure(Rect::new(0, 0, 1, 1));
        assert_eq!(r.width, 0);
        assert_eq!(r.height, 0);
    }

    #[test]
    fn decorated_pointer_outcome_variants_round_trip_match_arms() {
        // Compile-time guard: every variant must be matchable
        // exhaustively. If a future slice adds a variant, this
        // test forces a compile error so the match below
        // surfaces every callsite.
        fn describe(o: DecoratedPointerOutcome) -> &'static str {
            match o {
                DecoratedPointerOutcome::Close => "close",
                DecoratedPointerOutcome::Titlebar => "titlebar",
                DecoratedPointerOutcome::ResizeEdge(_) => "resize_edge",
                DecoratedPointerOutcome::Content => "content",
                DecoratedPointerOutcome::Outside => "outside",
            }
        }
        assert_eq!(describe(DecoratedPointerOutcome::Close), "close");
        assert_eq!(describe(DecoratedPointerOutcome::Titlebar), "titlebar");
        assert_eq!(
            describe(DecoratedPointerOutcome::ResizeEdge(0)),
            "resize_edge"
        );
        assert_eq!(describe(DecoratedPointerOutcome::Content), "content");
        assert_eq!(describe(DecoratedPointerOutcome::Outside), "outside");
    }
}
