//! Browser lifecycle fixture: announces itself, then executes the
//! wasm `unreachable` instruction without calling `proc_exit`.
//! The user Worker must report the trap and the host must reconcile
//! the kernel process through the out-of-band exit path.

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
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
    const LINE: &[u8] = b"process trap armed\n";
    let iov = Ciovec {
        buf: LINE.as_ptr(),
        buf_len: LINE.len() as u32,
    };
    let mut written = 0;
    unsafe {
        let _ = fd_write(1, &iov, 1, &mut written);
        core::arch::wasm32::unreachable();
    }
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
