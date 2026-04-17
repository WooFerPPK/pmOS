//! `hello-wasi-spawner` — a no_std wasm32-wasip1 binary that
//! spawns `/usr/bin/hello` via the PMos extension `proc_spawn`
//! syscall, then exits.
//!
//! Used by the `user-wasm-runtime.test.ts` composition test to
//! prove that the test-harness `runAllSpawns` helper is reentrant
//! under real wasm execution: a running parent wasm calls
//! `PROC_SPAWN` mid-run, the host's `onSpawnProcess` callback
//! captures the child, the parent returns from `_start`,
//! `runAllSpawns` pops the child and runs it, the child writes
//! its line and exits. The test asserts both the parent's and
//! the child's output appear in the capture in order.
//!
//! ## Import namespaces
//!
//! Two non-standard import modules are declared:
//!
//!   * `wasi_snapshot_preview1` — for `fd_write` + `proc_exit`,
//!     same as `hello-wasi-min`.
//!   * `pmos_ext` — a PMos-native extension namespace for the
//!     syscalls WASI doesn't cover. In this slice it exposes one
//!     function, `proc_spawn`, signed as
//!     `(path_ptr, path_len, caps: u64) -> i32`. Returns the new
//!     pid on success, negative errno on failure. The WASI shim
//!     on the TS side translates this into a `PROC_SPAWN` opcode
//!     with an `encodeSpawnManifest`-produced args + heap
//!     payload.
//!
//! A future slice will replace `pmos_ext` with a proper PMos
//! runtime crate (`crates/pmos-rt/` or similar) that every
//! userland binary links against. For now the import declarations
//! are inline in each test binary.

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(
        fd: i32,
        iovs_ptr: *const Ciovec,
        iovs_len: i32,
        nwritten_ptr: *mut u32,
    ) -> i32;
    fn proc_exit(rval: i32) -> !;
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    /// Spawn a child process running the binary at `path`.
    /// Returns the new pid on success, negative errno on failure.
    /// `caps` is the child's capability bitset — the kernel
    /// enforces that it's a subset of the caller's own caps.
    fn proc_spawn(path_ptr: *const u8, path_len: u32, caps: u64) -> i32;
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Ciovec {
    buf: *const u8,
    buf_len: u32,
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn _start() {
    // 1. Announce that the spawner is running before it does any
    //    spawning, so the test observes parent-before-child
    //    ordering in the console output capture.
    const MSG: &[u8] = b"spawner alive\n";
    let iov = Ciovec {
        buf: MSG.as_ptr(),
        buf_len: MSG.len() as u32,
    };
    let mut nwritten: u32 = 0;
    unsafe {
        let _ = fd_write(1, &iov, 1, &mut nwritten);
    }

    // 2. Spawn `/usr/bin/hello` with CapSet::ALL. The kernel
    //    enforces the subset rule; since this spawner was
    //    registered with CAPSET_ALL, every cap is permitted.
    const HELLO_PATH: &[u8] = b"/usr/bin/hello";
    let caps: u64 = u64::MAX;
    let rc = unsafe {
        proc_spawn(HELLO_PATH.as_ptr(), HELLO_PATH.len() as u32, caps)
    };
    if rc < 0 {
        // Propagate the errno to the test harness via proc_exit's
        // code. rc is already negative, which means we exit with
        // that value and `runAllSpawns` records a non-zero exit
        // in its history — pointing at where the failure was.
        unsafe { proc_exit(rc) }
    }

    // 3. Exit cleanly. The child has NOT run yet — it's sitting
    //    in the test harness's `captures` array waiting for
    //    this `run()` call to return so `runAllSpawns` can pop
    //    it and start its own runtime. That ordering is the
    //    whole point of the reentrancy test: the child runs
    //    after the parent exits, not during.
    unsafe { proc_exit(0) }
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { proc_exit(101) }
}
