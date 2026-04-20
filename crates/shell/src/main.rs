//! PMos desktop shell — binary entry point.
//!
//! v1 shape (T121 paint-wallpaper slice): the shell is a
//! toolkit app that connects to `/run/display`, creates a
//! top-level window, paints a solid wallpaper colour into
//! it, and enters an event loop that exits when the server
//! requests window close. The event-loop body lives in
//! [`shell::run_shell`]; tests drive it through a
//! mock-server scaffold (`tests/paint_wallpaper.rs`).
//! Taskbar, launcher, and click handling are explicitly
//! deferred — see T121 partial note in `tasks.md`.
//!
//! End-to-end wiring against the bundled display-server is
//! blocked on T110 protocol dispatch (today display-server
//! relays raw pixels rather than parsing wire-framed
//! messages). When that lands, `main` will open
//! `/run/display` via the kernel's `display_connect`
//! extension syscall, wrap the returned fd in a Connection,
//! and call [`shell::run_shell`] against it. For now the
//! binary is inert so `just build` and the existing
//! userland integration tests don't regress.

use shell::{run_shell, ShellExit};
use toolkit::{ClientError, MemoryConnection};

// Anchor references to the library's paint-wallpaper entry
// points so `just build` doesn't DCE the symbols out of
// the WASM binary. Even though `fn main` is an inert
// println! for this slice, keeping the wallpaper-paint
// code path anchored in the WASM output catches any
// downstream link breakage early.
#[used]
static _KEEP_RUN_SHELL: fn(MemoryConnection, u32) -> Result<ShellExit, ClientError> =
    run_shell::<MemoryConnection>;

fn main() {
    println!("shell: toolkit paint-wallpaper slice — T121 partial");
}
