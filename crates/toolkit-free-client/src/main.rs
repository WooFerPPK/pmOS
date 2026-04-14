// toolkit-free-client — Principle VII conformance fixture.
//
// Hand-written client that speaks the display-server wire
// protocol directly with NO toolkit linked in. The library
// module (`lib.rs`) implements the protocol helpers; this
// binary is a thin driver that constructs a `FreeClient`,
// walks the display→registry→compositor→surface→commit
// sequence in memory, and prints the resulting byte-queue
// length as a sanity check.
//
// The real integration — opening /run/display via the kernel
// IPC extension syscalls and feeding the byte queue into the
// socket — lands in a follow-up slice once the kernel's
// display_connect is wired up. Until then this binary is a
// self-contained dry run.
//
// Run by the crate's isolation tests AND by browser
// integration tests (T109+). If this binary stops producing a
// recognisable request sequence, the project has drifted from
// Principle VII.

use toolkit_free_client::FreeClient;

fn main() {
    let mut c = FreeClient::new();

    // Walk the minimal bind-compositor-create-surface-commit
    // sequence. We use fake global names / versions since we
    // aren't talking to a real registry yet.
    let registry = c.get_registry().expect("allocate registry id");
    let compositor = c
        .registry_bind(registry, 1, "pmd_compositor", 1)
        .expect("bind compositor");
    let surface = c
        .compositor_create_surface(compositor)
        .expect("create surface");
    c.surface_commit(surface).expect("commit surface");

    let wire_bytes = c.drain_outbound();
    println!(
        "[toolkit-free-client] wrote {} wire bytes without a toolkit",
        wire_bytes.len(),
    );
}
