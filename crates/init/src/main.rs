//! PID 1 — the first userland program the kernel spawns on boot.
//!
//! In v1 the responsibility is deliberately tiny: announce that
//! init is alive, spawn `/bin/hello-std` as a demo child via the
//! PMos extension `proc_spawn` syscall, and exit cleanly so the
//! drain loop can pick the child up. A real OS init would loop,
//! reap children, and supervise long-lived services; PMos's
//! sequential in-process `drainPendingSpawns` can't support a
//! looping parent yet, so this slice's init is a single-shot
//! launcher. The looping/supervision behaviour lands when the
//! Worker-per-pid seam is cut.
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

    println!("init exiting");
}
