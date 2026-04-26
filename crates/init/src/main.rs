//! PID 1 — the first userland program the kernel spawns on boot.
//!
//! v1 shape (T095 progress — the proc_wait supervision loop slice):
//! init announces itself, spawns four fire-and-forget demo children
//! via `pmos_ext.proc_spawn` — `/bin/hello-std`, `/bin/display-server`,
//! `/bin/display-client-demo` (twice) — then enters a blocking
//! `proc_wait` supervision loop. Each loop iteration reaps one
//! zombie child and prints `init reaped child pid={N}`. When both
//! display-client-demo children have been reaped, init signals
//! `/bin/display-server` with SIGTERM (`proc_kill(ds_pid, 15)`) so
//! the server's accept loop (which is otherwise parked waiting for
//! a third client that never comes) can exit cleanly. Once every
//! spawned pid has been reaped, init prints `init exiting` and
//! falls off the end of `main` — `__wasi_proc_exit(0)` finishes it.
//!
//! Under Playwright (concurrent Workers, `markRunning` semantics
//! active) the happy path fires: all four children overlap, parked
//! `proc_wait` wakes on each Zombie transition, SIGTERM lands on
//! display-server exactly once, display-server's signal-driven
//! exit prints `display-server fb blit ok`, every child gets
//! reaped. Under vitest `runAllSpawns` (strictly sequential,
//! spawned pids stay Ready), init's first `proc_wait` immediately
//! trips the `-ESRCH` early-exit because `park_on_wait` fails its
//! state transition on a Ready child — init prints
//! `init proc_wait returned errno=71; exiting with N children
//! unreaped` then `init exiting` and falls through. Both harnesses
//! produce a finite, observable termination.
//!
//! A real OS init would additionally parse `/etc/init.conf`,
//! respawn children per policy, load `/etc/preferences.toml` env
//! vars, and manage a shell-respawn cadence. Those remain deferred
//! within T095 (see tasks.md partial note). This slice's scope is
//! the supervision loop + graceful display-server shutdown — the
//! leg that closes the long-running-display-server accept-loop
//! arc.
//!
//! The crate is a `std` binary: we lean on `println!` for output
//! and on Rust's normal libc/WASI startup path for argv/environ
//! discovery + stdio. The only things the crate owns itself are
//! the `extern "C"` imports of `pmos_ext.proc_spawn`,
//! `pmos_ext.proc_wait`, and `pmos_ext.proc_kill`; everything
//! else a conventional Rust program uses Just Works because the
//! PMos WASI shim covers the std startup quartet
//! (args_sizes_get, args_get, environ_sizes_get, environ_get)
//! plus fd_fdstat_get, fd_prestat_get, fd_write, proc_exit.

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn proc_spawn(path_ptr: *const u8, path_len: u32, caps: u64) -> i32;
    fn proc_wait(target_pid: i32, options: i32, status_out_ptr: i32) -> i32;
    fn proc_kill(target_pid: i32, signum: i32) -> i32;
}

// Native-target stubs: `cargo build --workspace` and `cargo test
// --workspace` compile bin crates for the host target too, so
// the symbols have to resolve even when nothing on the native side
// ever calls them. The wasm build is what matters in production.
#[cfg(not(target_arch = "wasm32"))]
unsafe fn proc_spawn(_path_ptr: *const u8, _path_len: u32, _caps: u64) -> i32 {
    0
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn proc_wait(_target_pid: i32, _options: i32, _status_out_ptr: i32) -> i32 {
    0
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn proc_kill(_target_pid: i32, _signum: i32) -> i32 {
    0
}

// Errno constants (abi::errno, positive form). PMos ext syscalls
// surface errors as negative errno; we compare against the negated
// forms inline.
const ECHILD: i32 = 9;
const ESRCH: i32 = 71;

// wait target: -1 means "any child" (same as POSIX waitpid).
const WAIT_ANY: i32 = -1;
// SIGTERM.
const SIGTERM: i32 = 15;

fn main() {
    println!("init starting");

    // T095: read /etc/init.conf if present. The parser lives in
    // `init::conf` and is exhaustively tested in `crates/init/
    // tests/init.rs`. The current demo-spawn flow below ignores
    // the parsed config (the v1 boot path runs a hardcoded set
    // of demo children that the Playwright + vitest harnesses
    // pin); the read happens for two reasons: (1) it exercises
    // the parser end-to-end on the wasm32-wasip1 target so a
    // regression in `read_to_string` / TOML decoding surfaces in
    // the boot log, (2) future shell-respawn work can switch
    // the spawn loop to config-driven without touching the
    // read path.
    match std::fs::read_to_string("/etc/init.conf") {
        Ok(text) => match init::conf::InitConfig::parse(&text) {
            Ok(cfg) => {
                println!(
                    "init: /etc/init.conf parsed (boot.shell={}, boot.display_server={}, autostart={})",
                    cfg.boot.shell,
                    cfg.boot.display_server,
                    cfg.boot.autostart.len(),
                );
            }
            Err(err) => {
                println!(
                    "init: /etc/init.conf parse failed: {} (using built-in defaults)",
                    err,
                );
            }
        },
        Err(_) => {
            // Common in v1 — `/etc` is on tmpfs and nothing
            // populates it at boot. Silent fall-through to the
            // demo flow below.
        }
    }

    // Track spawned pids so the reap loop can distinguish which
    // child just exited and drive the delayed SIGTERM to
    // display-server.
    let mut hello_std_pid: i32 = -1;
    let mut ds_pid: i32 = -1;
    let mut dc1_pid: i32 = -1;
    let mut dc2_pid: i32 = -1;

    const HELLO_STD: &[u8] = b"/bin/hello-std";
    let rc = unsafe {
        proc_spawn(HELLO_STD.as_ptr(), HELLO_STD.len() as u32, u64::MAX)
    };
    if rc < 0 {
        println!("init: proc_spawn /bin/hello-std failed errno={}", -rc);
    } else {
        hello_std_pid = rc;
        println!("init spawned hello-std pid={}", rc);
    }

    const DISPLAY_SERVER: &[u8] = b"/bin/display-server";
    let rc = unsafe {
        proc_spawn(DISPLAY_SERVER.as_ptr(), DISPLAY_SERVER.len() as u32, u64::MAX)
    };
    if rc < 0 {
        println!("init: proc_spawn /bin/display-server failed errno={}", -rc);
    } else {
        ds_pid = rc;
        println!("init spawned display-server pid={}", rc);
    }

    const DISPLAY_CLIENT_DEMO: &[u8] = b"/bin/display-client-demo";
    let rc = unsafe {
        proc_spawn(
            DISPLAY_CLIENT_DEMO.as_ptr(),
            DISPLAY_CLIENT_DEMO.len() as u32,
            u64::MAX,
        )
    };
    if rc < 0 {
        println!(
            "init: proc_spawn /bin/display-client-demo failed errno={}",
            -rc
        );
    } else {
        dc1_pid = rc;
        println!("init spawned display-client-demo pid={}", rc);
    }

    let rc = unsafe {
        proc_spawn(
            DISPLAY_CLIENT_DEMO.as_ptr(),
            DISPLAY_CLIENT_DEMO.len() as u32,
            u64::MAX,
        )
    };
    if rc < 0 {
        println!(
            "init: proc_spawn /bin/display-client-demo failed errno={}",
            -rc
        );
    } else {
        dc2_pid = rc;
        println!("init spawned display-client-demo pid={}", rc);
    }

    // Count pids we actually got and need to reap. A proc_spawn
    // failure above means the child never existed, so don't wait
    // for it.
    let mut remaining: u32 = 0;
    if hello_std_pid > 0 { remaining += 1; }
    if ds_pid > 0 { remaining += 1; }
    if dc1_pid > 0 { remaining += 1; }
    if dc2_pid > 0 { remaining += 1; }

    // Count of display-client-demo pids still alive. When this
    // drops to zero AND ds_pid is still live AND sigterm hasn't
    // fired yet, signal display-server so its accept loop exits.
    let mut clients_remaining: u32 = 0;
    if dc1_pid > 0 { clients_remaining += 1; }
    if dc2_pid > 0 { clients_remaining += 1; }
    let mut sigterm_sent = false;

    let mut status_out: i64 = 0;

    loop {
        if remaining == 0 {
            break;
        }
        let status_ptr = &mut status_out as *mut i64 as i32;
        let rc = unsafe { proc_wait(WAIT_ANY, 0, status_ptr) };
        if rc < 0 {
            // Two-shape early-exit: -ECHILD (no more children —
            // shouldn't happen before remaining == 0 under Playwright
            // but can race) or -ESRCH (vitest runAllSpawns quirk —
            // spawned pids stay Ready, park_on_wait fails the state
            // transition). Either way, bail cleanly rather than spin.
            if rc == -ECHILD || rc == -ESRCH {
                println!(
                    "init proc_wait returned errno={}; exiting with {} children unreaped",
                    -rc, remaining
                );
                break;
            }
            // Any other error is unexpected — log it and bail too.
            println!("init: proc_wait unexpected errno={}", -rc);
            break;
        }

        let reaped_pid = rc;
        println!("init reaped child pid={}", reaped_pid);
        remaining = remaining.saturating_sub(1);

        if reaped_pid == dc1_pid || reaped_pid == dc2_pid {
            clients_remaining = clients_remaining.saturating_sub(1);
        }

        // Both clients done + server still in the unreaped set +
        // SIGTERM not yet sent → signal display-server to exit.
        if clients_remaining == 0
            && !sigterm_sent
            && ds_pid > 0
            && reaped_pid != ds_pid
        {
            let krc = unsafe { proc_kill(ds_pid, SIGTERM) };
            if krc < 0 {
                // -ESRCH means display-server already exited (e.g.
                // under the bounded outer-loop legacy path). Log
                // and continue reaping either way.
                println!(
                    "init: proc_kill(ds, SIGTERM) failed errno={}",
                    -krc
                );
            } else {
                println!("init sent SIGTERM to display-server pid={}", ds_pid);
            }
            sigterm_sent = true;
        }
    }

    println!("init exiting");
}
