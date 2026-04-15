//! Global allocator for the wasm32-unknown-unknown target.
//!
//! The kernel is `#![no_std]` with `extern crate alloc`. It needs
//! `Box`, `Vec`, `BTreeMap`, and `String` — which all require a
//! global allocator. `wasm32-unknown-unknown` doesn't provide one
//! out of the box (unlike `wasm32-wasip1`), so we install a small
//! bump allocator that uses the WASM linear memory's `grow`
//! instruction to obtain pages.
//!
//! Under `native-platform` (host-target `cargo test`), the crate
//! uses `std` and the system allocator, so this file is a no-op.
//!
//! This is a **Phase 1 bump allocator**. It never frees. That is
//! acceptable for the kernel in v1 because:
//!
//! * The kernel's steady-state allocation pressure is low (process
//!   table entries, fd table entries, VFS cache) — dominated by
//!   per-process structures that outlive the kernel tick boundary.
//! * Per-request arena allocations go through a stack-style
//!   scratch allocator built on top of this, introduced when T073
//!   needs it for syscall dispatch.
//! * A better allocator (linked-list, or dlmalloc) can drop in
//!   behind `#[global_allocator]` with zero kernel changes —
//!   this file is deliberately isolated.

#![allow(dead_code)]

// The bump allocator + `#[global_allocator]` declaration only
// compiles on the wasm32 target, because it uses
// `core::arch::wasm32::memory_grow`. On every other target
// (including `cargo check` on a native host without the
// `native-platform` feature), this module is empty and no global
// allocator is installed; the kernel library still type-checks but
// no final binary would link. That is fine — the only *binary*
// target for the kernel is wasm32, so this cfg gate is equivalent
// to "the allocator exists iff it is needed".
#[cfg(all(not(feature = "native-platform"), target_arch = "wasm32"))]
mod bump {
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// A page-granular bump allocator. Thread-unsafe by construction
    /// (the kernel runs in a single Worker and does not spawn
    /// threads), but protected against accidental concurrent use by
    /// an atomic cursor.
    pub struct BumpAllocator {
        cursor: AtomicUsize,
        /// End of the currently-mapped arena in bytes. The allocator
        /// grows the WASM memory when `cursor` reaches this value.
        end: UnsafeCell<usize>,
    }

    unsafe impl Sync for BumpAllocator {}

    impl BumpAllocator {
        pub const fn new() -> Self {
            BumpAllocator {
                cursor: AtomicUsize::new(0),
                end: UnsafeCell::new(0),
            }
        }
    }

    // Linker-defined symbol marking the first byte above the
    // kernel's data + bss sections. The heap has to start here —
    // *not* at address 0 — or allocations overlap the static
    // data the linker placed in low linear memory (including
    // `#[no_mangle] static mut` scratch buffers used by
    // `crate::wasm_entry`). Accessing the symbol's address is
    // the standard no_std way to find the start of free memory
    // on wasm32-unknown-unknown; the Rust/LLD toolchain emits
    // `__heap_base` for every cdylib.
    extern "C" {
        static __heap_base: u8;
    }

    impl BumpAllocator {
        /// Lazy first-touch initialisation. Called at the top of
        /// every `alloc` but only does work the first time
        /// `cursor` is observed as zero. Sets `cursor` to
        /// `__heap_base` (the first free byte after the data
        /// section) and `end` to the current memory size in
        /// bytes. Subsequent calls short-circuit.
        ///
        /// Correct under the single-threaded assumption already
        /// in the struct docs: the kernel runs in one Worker.
        /// The `compare_exchange` is a belt-and-braces guard —
        /// even if two calls were to race here, only one wins
        /// the CAS and the loser's observation of a nonzero
        /// cursor on the next load is fine.
        unsafe fn lazy_init(&self) {
            if self.cursor.load(Ordering::Relaxed) != 0 {
                return;
            }
            let base = &__heap_base as *const u8 as usize;
            let _ = self.cursor.compare_exchange(
                0,
                base,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            let end_ptr = self.end.get();
            if *end_ptr == 0 {
                let mem_pages = core::arch::wasm32::memory_size(0);
                *end_ptr = mem_pages * PAGE_SIZE;
            }
        }
    }

    unsafe impl GlobalAlloc for BumpAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            self.lazy_init();

            let align = layout.align().max(1);
            let size = layout.size();

            loop {
                let current = self.cursor.load(Ordering::Relaxed);
                let aligned = (current + align - 1) & !(align - 1);
                let new_cursor = match aligned.checked_add(size) {
                    Some(v) => v,
                    None => return core::ptr::null_mut(),
                };

                // Grow WASM memory if the allocation would overrun.
                let end_ptr = self.end.get();
                let end_val = *end_ptr;
                if new_cursor > end_val {
                    let extra_bytes = new_cursor - end_val;
                    let extra_pages = (extra_bytes + PAGE_SIZE - 1) / PAGE_SIZE;
                    let prev_pages = core::arch::wasm32::memory_grow(0, extra_pages);
                    if prev_pages == usize::MAX {
                        return core::ptr::null_mut();
                    }
                    *end_ptr = (prev_pages + extra_pages) * PAGE_SIZE;
                }

                match self.cursor.compare_exchange(
                    current,
                    new_cursor,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return aligned as *mut u8,
                    Err(_) => continue,
                }
            }
        }

        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
            // Bump allocator: no deallocation. See module docs.
        }
    }

    const PAGE_SIZE: usize = 64 * 1024;

    #[global_allocator]
    static GLOBAL: BumpAllocator = BumpAllocator::new();
}
