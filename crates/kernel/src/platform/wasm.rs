//! WASM runtime `Platform` implementation.
//!
//! Active when the kernel crate is built for `wasm32-unknown-unknown`
//! **without** the `native-platform` feature. Every method here is
//! backed by a host function imported from the kernel Worker's
//! JavaScript side (`web/src/kernel-worker.ts`).
//!
//! The real function bodies call `extern "C"` imports that the JS
//! runtime wires up when instantiating the kernel WASM. Host-side
//! wiring lands in T091 (web/src/kernel-worker.ts). The stubs here
//! exist so the kernel crate compiles under the wasm32 target
//! before the runtime glue is written.

use core::panic::PanicInfo;

use abi::ext::Pid;

use super::{DevId, DriverError, DriverResult, Platform};

#[cfg(all(target_arch = "wasm32", not(feature = "native-platform")))]
#[link(wasm_import_module = "env")]
extern "C" {
    fn pmos_host_now_ns() -> u64;
    fn pmos_host_now_realtime_ns() -> u64;
    fn pmos_host_driver_call(
        dev: u32,
        op: u32,
        args_ptr: *const u8,
        args_len: u32,
        result: *mut u32,
    ) -> i32;
    fn pmos_host_random_bytes(out_ptr: *mut u8, out_len: u32);
    fn pmos_host_halt(reason_ptr: *const u8, reason_len: u32) -> !;
    fn pmos_host_panic(message_ptr: *const u8, message_len: u32);
    fn pmos_host_spawn_process(
        pid: i32,
        path_ptr: *const u8,
        path_len: u32,
        executable_ptr: *const u8,
        executable_len: u32,
    ) -> i32;
    fn pmos_host_terminate_process(pid: i32) -> i32;
    fn pmos_host_file_picker() -> i32;
    fn pmos_host_download_file(
        name_ptr: *const u8,
        name_len: u32,
        mime_ptr: *const u8,
        mime_len: u32,
        bytes_ptr: *const u8,
        bytes_len: u32,
    ) -> i32;
}

/// The singleton WasmPlatform. Zero-sized.
pub struct WasmPlatform;

impl Platform for WasmPlatform {
    #[allow(unreachable_code)]
    fn now_ns(&self) -> u64 {
        #[cfg(all(target_arch = "wasm32", not(feature = "native-platform")))]
        unsafe {
            return pmos_host_now_ns();
        }
        // Unreachable in real wasm32 builds; present so the crate
        // type-checks when feature-flipped inconsistently.
        0
    }

    #[allow(unreachable_code)]
    fn now_realtime_ns(&self) -> u64 {
        #[cfg(all(target_arch = "wasm32", not(feature = "native-platform")))]
        unsafe {
            return pmos_host_now_realtime_ns();
        }
        0
    }

    #[allow(unreachable_code, unused_variables)]
    fn driver_call(&self, dev: DevId, op: u32, args: &[u8]) -> DriverResult<u32> {
        #[cfg(all(target_arch = "wasm32", not(feature = "native-platform")))]
        unsafe {
            let mut result: u32 = 0;
            let rc = pmos_host_driver_call(
                dev as u32,
                op,
                args.as_ptr(),
                args.len() as u32,
                &mut result as *mut u32,
            );
            return if rc == 0 {
                Ok(result)
            } else if rc < 0 {
                Err(DriverError::Errno(-rc))
            } else {
                Err(DriverError::Transport)
            };
        }
        Err(DriverError::NotReady)
    }

    #[allow(unused_variables)]
    fn random_bytes(&self, out: &mut [u8]) {
        #[cfg(all(target_arch = "wasm32", not(feature = "native-platform")))]
        unsafe {
            pmos_host_random_bytes(out.as_mut_ptr(), out.len() as u32);
        }
    }

    #[allow(unreachable_code, unused_variables)]
    fn halt(&self, reason: &str) -> ! {
        #[cfg(all(target_arch = "wasm32", not(feature = "native-platform")))]
        unsafe {
            pmos_host_halt(reason.as_ptr(), reason.len() as u32);
        }
        // Unreachable on real wasm builds. Required so the return
        // type `!` is satisfied if the cfg is flipped.
        loop {
            core::hint::spin_loop();
        }
    }

    #[allow(unused_variables)]
    fn on_panic(&self, info: &PanicInfo) {
        // The panic message may contain non-UTF-8 from unexpected
        // sources; we format into a small fixed buffer to avoid
        // pulling in `alloc::format!` on the hot path. For now
        // we only report the location.
        #[cfg(all(target_arch = "wasm32", not(feature = "native-platform")))]
        unsafe {
            if let Some(loc) = info.location() {
                let file = loc.file().as_bytes();
                pmos_host_panic(file.as_ptr(), file.len() as u32);
            } else {
                let empty: &[u8] = b"kernel panic";
                pmos_host_panic(empty.as_ptr(), empty.len() as u32);
            }
        }
    }

    #[allow(unreachable_code, unused_variables)]
    fn spawn_process(&self, pid: Pid, path: &str, executable: Option<&[u8]>) -> DriverResult<()> {
        #[cfg(all(target_arch = "wasm32", not(feature = "native-platform")))]
        unsafe {
            let (executable_ptr, executable_len) = executable
                .map(|bytes| (bytes.as_ptr(), bytes.len() as u32))
                .unwrap_or((core::ptr::null(), 0));
            let rc = pmos_host_spawn_process(
                pid,
                path.as_ptr(),
                path.len() as u32,
                executable_ptr,
                executable_len,
            );
            return if rc == 0 {
                Ok(())
            } else if rc < 0 {
                Err(DriverError::Errno(-rc))
            } else {
                Err(DriverError::Transport)
            };
        }
        Err(DriverError::NotReady)
    }

    #[allow(unreachable_code, unused_variables)]
    fn terminate_process(&self, pid: Pid) -> DriverResult<()> {
        #[cfg(all(target_arch = "wasm32", not(feature = "native-platform")))]
        unsafe {
            let rc = pmos_host_terminate_process(pid);
            return if rc == 0 {
                Ok(())
            } else if rc < 0 {
                Err(DriverError::Errno(-rc))
            } else {
                Err(DriverError::Transport)
            };
        }
        Err(DriverError::NotReady)
    }

    #[allow(unreachable_code)]
    fn request_host_file_picker(&self) -> DriverResult<()> {
        #[cfg(all(target_arch = "wasm32", not(feature = "native-platform")))]
        unsafe {
            return host_result(pmos_host_file_picker());
        }
        Err(DriverError::NotReady)
    }

    #[allow(unreachable_code, unused_variables)]
    fn download_host_file(&self, name: &str, mime: &str, bytes: &[u8]) -> DriverResult<()> {
        #[cfg(all(target_arch = "wasm32", not(feature = "native-platform")))]
        unsafe {
            return host_result(pmos_host_download_file(
                name.as_ptr(),
                name.len() as u32,
                mime.as_ptr(),
                mime.len() as u32,
                bytes.as_ptr(),
                bytes.len() as u32,
            ));
        }
        Err(DriverError::NotReady)
    }
}

#[inline]
fn host_result(rc: i32) -> DriverResult<()> {
    if rc == 0 {
        Ok(())
    } else if rc < 0 {
        Err(DriverError::Errno(-rc))
    } else {
        Err(DriverError::Transport)
    }
}
