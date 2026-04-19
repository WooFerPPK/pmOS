//! PID 1 — the first userland program the kernel spawns on boot.
//!
//! In v1 the responsibility is deliberately tiny: announce that
//! init is alive, spawn three fire-and-forget demo children via
//! the PMos extension `proc_spawn` syscall — first
//! `/bin/hello-std`, then `/bin/display-server`, then
//! `/bin/display-client-demo` — and exit cleanly so the dispatch
//! loop can pick every child up. A real OS init would loop, reap
//! children, and supervise long-lived services; PMos now has the
//! substrate for that (Worker-per-pid + per-pid SAB rings landed
//! in T230–T235), but the kernel-side `proc_wait` /
//! signal-delivery semantics (T075) are still partial, so this
//! slice's init stays a single-shot launcher. The looping /
//! supervision behaviour lands when `proc_wait` + the
//! user-visible `SignalChannel` fd are wired through.
//!
//! The second and third spawns — display-server and its sibling
//! display-client-demo — are the first pair in the workspace
//! whose raison d'être is to talk to each other over a PMos IPC
//! socket (display-server binds `/run/display`, display-client-demo
//! connects + writes a pixel payload). Init spawns both so the
//! client has a server to find; the order is chosen so
//! display-server reaches `display_bind` before
//! display-client-demo reaches `display_connect` under normal
//! Worker scheduling (spawn order matches pid map iteration
//! order, which is the dispatch loop's round-robin order).
//!
//! The crate is a `std` binary: we lean on `println!` for output
//! and on Rust's normal libc/WASI startup path for argv/environ
//! discovery + stdio. The only thing the crate owns itself is
//! the `extern "C"` import of `pmos_ext.proc_spawn`; everything
//! else a conventional Rust program uses Just Works because the
//! PMos WASI shim covers the std startup quartet
//! (args_sizes_get, args_get, environ_sizes_get, environ_get)
//! plus fd_fdstat_get, fd_prestat_get, fd_write, proc_exit.

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn proc_spawn(path_ptr: *const u8, path_len: u32, caps: u64) -> i32;
}

// Native-target stub: `cargo build --workspace` and `cargo test
// --workspace` compile bin crates for the host target too, so
// the symbol has to resolve even when nothing on the native side
// ever calls it. The wasm build is what matters in production.
#[cfg(not(target_arch = "wasm32"))]
unsafe fn proc_spawn(_path_ptr: *const u8, _path_len: u32, _caps: u64) -> i32 {
    0
}

fn main() {
    println!("init starting");

    const HELLO_STD: &[u8] = b"/bin/hello-std";
    let rc = unsafe {
        proc_spawn(HELLO_STD.as_ptr(), HELLO_STD.len() as u32, u64::MAX)
    };
    if rc < 0 {
        println!("init: proc_spawn /bin/hello-std failed errno={}", -rc);
    } else {
        println!("init spawned hello-std pid={}", rc);
    }

    const DISPLAY_SERVER: &[u8] = b"/bin/display-server";
    let rc = unsafe {
        proc_spawn(DISPLAY_SERVER.as_ptr(), DISPLAY_SERVER.len() as u32, u64::MAX)
    };
    if rc < 0 {
        println!("init: proc_spawn /bin/display-server failed errno={}", -rc);
    } else {
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
        println!("init spawned display-client-demo pid={}", rc);
    }

    println!("init exiting");
}
