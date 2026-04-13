//! PMos window toolkit.
//!
//! Library statically linked into apps. Wraps the display server
//! wire protocol with windows, widgets, layout, drawing, and an
//! event loop integrated with frame callbacks.
//!
//! Populated in Phase 2 T114..T120.

pub mod app;
pub mod window;
pub mod widget;
pub mod layout;
pub mod draw;
pub mod theme;
