//! `hello-framebuffer` — a no_std wasm32-wasip1 binary that
//! opens `/dev/fb0` via WASI `path_open` and writes a small
//! block of raw RGBA pixel bytes to it.
//!
//! Used by `user-wasm-runtime.test.ts` to prove the
//! **kernel → TS framebuffer driver** path end-to-end:
//!
//!   1. User wasm calls `path_open("/dev/fb0")` via the WASI
//!      shim; the shim dispatches PMos `PATH_OPEN`; the kernel
//!      resolves the VFS path, finds it in devfs,
//!      `DeviceDispatcher::check_open` enforces the
//!      `Cap::DisplayServer` requirement, installs an
//!      `FdObject::CharDevice(DEV_FB0)` fd.
//!   2. User wasm calls `fd_write(fd, pixels)` via the WASI
//!      shim; shim dispatches `FD_WRITE`; the kernel's
//!      `Kernel::fd_write` sees `FdObject::CharDevice(DEV_FB0)`
//!      and routes to `DeviceDispatcher::write(DEV_FB0, buf)`,
//!      which calls `framebuffer_write`, which forwards the
//!      whole buffer to `platform::current().driver_call(
//!      DevId::Framebuffer, DEV_FB0, buf)`.
//!   3. The `WasmPlatform::driver_call` calls
//!      `pmos_host_driver_call(Framebuffer, ...)` as a host
//!      import; the `KernelWasmHost` closure reads the bytes
//!      out of the kernel's linear memory and forwards them
//!      to `options.onFramebufferWrite(bytes)`.
//!   4. The vitest harness's callback captures the bytes and
//!      asserts they match exactly what the user wrote.
//!
//! Every arrow in that chain is real production code — no
//! mocks, no shortcuts. This is the first slice where user
//! wasm writes bytes that the TS host recognizes as "pixels
//! destined for the framebuffer".
//!
//! ## Exit codes (for test diagnostics)
//!
//!   * 0  = success
//!   * 10 = `path_open("/dev/fb0")` failed (missing cap,
//!          missing device, bad path resolution)
//!   * 11 = `fd_write(fb_fd, pixels)` failed
//!   * 101 = panic

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn path_open(
        dirfd: i32,
        dirflags: i32,
        path_ptr: *const u8,
        path_len: i32,
        oflags: i32,
        fs_rights_base: i64,
        fs_rights_inheriting: i64,
        fdflags: i32,
        fd_out_ptr: *mut u32,
    ) -> i32;
    fn fd_write(
        fd: i32,
        iovs_ptr: *const Ciovec,
        iovs_len: i32,
        nwritten_ptr: *mut u32,
    ) -> i32;
    fn proc_exit(rval: i32) -> !;
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Ciovec {
    buf: *const u8,
    buf_len: u32,
}

/// Four RGBA pixels: red, green, blue, white. 16 bytes total.
/// The vitest asserts the callback receives exactly these
/// bytes in this order.
#[cfg(target_arch = "wasm32")]
const PIXELS: [u8; 16] = [
    0xff, 0x00, 0x00, 0xff, // red
    0x00, 0xff, 0x00, 0xff, // green
    0x00, 0x00, 0xff, 0xff, // blue
    0xff, 0xff, 0xff, 0xff, // white
];

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn _start() {
    const FB_PATH: &[u8] = b"/dev/fb0";

    unsafe {
        // 1. Open /dev/fb0. The caller's cap set must include
        //    DisplayServer; the vitest grants CAPSET_ALL so
        //    this succeeds.
        let mut fb_fd: u32 = 0;
        let rc = path_open(
            0, // dirfd (unused)
            0, // dirflags
            FB_PATH.as_ptr(),
            FB_PATH.len() as i32,
            0, // oflags
            0, // fs_rights_base
            0, // fs_rights_inheriting
            0, // fdflags
            &mut fb_fd,
        );
        if rc != 0 {
            proc_exit(10);
        }

        // 2. Write 16 bytes of RGBA pixel data to /dev/fb0.
        //    The kernel's device dispatch forwards the bytes
        //    to the framebuffer driver via driver_call, which
        //    becomes a pmos_host_driver_call host import on
        //    the WASM side, which the TS host routes to the
        //    test's onFramebufferWrite callback.
        let iov = Ciovec {
            buf: PIXELS.as_ptr(),
            buf_len: PIXELS.len() as u32,
        };
        let mut nwritten: u32 = 0;
        let rc = fd_write(fb_fd as i32, &iov, 1, &mut nwritten);
        if rc != 0 || nwritten != PIXELS.len() as u32 {
            proc_exit(11);
        }

        proc_exit(0);
    }
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { proc_exit(101) }
}
