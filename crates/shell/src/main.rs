// /usr/bin/shell — the desktop shell binary.
//
// Ordinary userland program holding Shell + DisplayClient +
// ProcEnumerate + KeymapAdmin capabilities (see
// `abi::cap::initial::DESKTOP_SHELL`). Draws the wallpaper,
// taskbar, launcher, and window chrome for every open app,
// and is the single most replaceable layer in the system
// (Principle II).
//
// The library layer in `lib.rs` is the real protocol state
// machine. This binary is the thin runtime driver: in
// production it will open `/run/display` via the kernel's
// `display_connect` extension syscall, feed bytes through
// a Session, and render windows. Until that syscall is
// bridged to userland via WASI (T071/T072 follow-ups), the
// binary is a dry-run diagnostic: it constructs a Session
// over an in-memory connection, calls `start()`, and
// prints the outbound request bytes.

use shell::Session;
use toolkit::protocol::MemoryConnection;

fn main() {
    let mut session = Session::new(MemoryConnection::new());
    match session.start() {
        Ok(()) => {
            let bytes = session.drain_outbound();
            println!(
                "[shell] dry-run: session::start sent {} bytes on the wire",
                bytes.len(),
            );
        }
        Err(e) => {
            eprintln!("[shell] dry-run: session::start failed: {e:?}");
        }
    }
}
