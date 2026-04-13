// toolkit-free-client — Principle VII conformance fixture.
//
// Hand-written client that speaks the display-server wire protocol
// directly with NO toolkit linked in. Calls display_connect,
// gets_registry, binds compositor/shm/xdg_wm_base, allocates a SAB
// pool, creates a surface + xdg_surface + xdg_toplevel, attaches a
// buffer filled with a solid colour, commits, handles close.
//
// Run in display-server isolation tests and Playwright integration
// tests. If this binary stops producing a window, the project has
// drifted from Principle VII.
//
// Populated in Phase 2 T113.

fn main() {
    // stub
}
