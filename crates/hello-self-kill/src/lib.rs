//! Browser lifecycle fixture: announces itself, requests SIGKILL
//! against its own pid, then spins without making another syscall.
//! Only the kernel-to-main forced-termination route can stop its
//! dedicated Worker and release the host pid map.

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn proc_self() -> i32;
    fn proc_kill(target_pid: i32, signum: i32) -> i32;
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
    const LINE: &[u8] = b"process SIGKILL armed\n";
    let iov = Ciovec {
        buf: LINE.as_ptr(),
        buf_len: LINE.len() as u32,
    };
    let mut written = 0;
    unsafe {
        let _ = fd_write(1, &iov, 1, &mut written);
        let pid = proc_self();
        let _ = proc_kill(pid, 9);
    }

    // Returning would let UserWasmRuntime issue a normal proc_exit,
    // masking a missing host-side SIGKILL teardown. A pure compute
    // loop makes the Worker live forever unless main terminates it.
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
