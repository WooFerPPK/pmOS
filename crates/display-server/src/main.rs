// PMos display server binary.
//
// Userland program that holds DISPLAY_SERVER capability (the
// only process allowed to open /dev/fb0 and /dev/input/*).
// Listens on /run/display and speaks the wire protocol from
// `contracts/display-protocol.md`. Software compositor in v1.
//
// The actual protocol handling lives in the `display_server`
// library (crate root); this binary is a thin wrapper that
// wires the library into the kernel's IPC subsystem via WASI.
//
// Populated in Phase 2 T098..T113. Current stub: constructs an
// empty `Server` and exits. The real integration with
// /run/display + the kernel's `ipc_recv` / `ipc_send` loops
// lands when the WASM kernel is wired up.

use display_server::Server;

fn main() {
    let server = Server::new();
    // Stub: prove the library + binary hybrid links cleanly
    // by printing a diagnostic line. A later slice replaces
    // this with the real `/run/display` accept loop.
    println!(
        "[display-server] stub: {} clients connected",
        server.client_count(),
    );
}
