//! xtask gen-sab-layout — T037.
//!
//! Generates `web/src/shared/sab-layout.ts` from the constants
//! defined in the `abi` crate, so that the TypeScript bootstrap
//! and drivers read from exactly the same layout the Rust kernel
//! emits.
//!
//! Drift between the two halves of the SAB contract is the most
//! dangerous silent failure mode in this project: a one-byte
//! offset mismatch means every syscall writes its arguments into
//! the wrong field. So this generator takes no parameters and
//! has no "maybe update" mode — it always rewrites the file
//! atomically, and CI runs it with `--check` to fail loudly if
//! the checked-in copy is stale.
//!
//! The generator depends on `abi` as an ordinary Cargo dep — the
//! same crate the kernel and userland link against — so there is
//! no second source for the constants. When abi's numbers
//! change, this command produces updated TS on the next run.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use abi::ring::{
    HEAP_SCRATCH_BYTES, OFF_HEADER_FLAGS, OFF_HEAP_SCRATCH, OFF_KERNEL_BLOCK_COUNT,
    OFF_KERNEL_WAIT_SLOT, OFF_REQ_HEAD, OFF_REQ_RING, OFF_REQ_TAIL, OFF_RES_HEAD, OFF_RES_RING,
    OFF_RES_TAIL, OFF_USER_BLOCK_COUNT, OFF_USER_WAIT_SLOT, REQ_RING_BYTES, REQ_SLOT_COUNT,
    RES_RING_BYTES, RES_SLOT_COUNT, SAB_SIZE, SLOT_SIZE, STATUS_IDLE, STATUS_READY,
    STATUS_REQUESTED, STATUS_SERVICING,
};
use abi::version::{ABI_MAJOR, ABI_MINOR};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub fn run(args: &[String]) -> std::result::Result<(), String> {
    run_inner(args).map_err(|e| format!("gen-sab-layout: {e}"))
}

fn run_inner(args: &[String]) -> Result<()> {
    let check_only = args.iter().any(|a| a == "--check");

    let repo_root = repo_root()?;
    let target = repo_root.join("web/src/shared/sab-layout.ts");

    let content = render();

    if check_only {
        let existing = match fs::read_to_string(&target) {
            Ok(s) => s,
            Err(e) => return Err(format!("cannot read {}: {e}", target.display()).into()),
        };
        if existing == content {
            println!("[xtask] gen-sab-layout: {} is up to date", target.display());
            Ok(())
        } else {
            Err(format!(
                "{} is stale — run `cargo run -p xtask -- gen-sab-layout` and commit the result",
                target.display()
            )
            .into())
        }
    } else {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, content)?;
        println!("[xtask] gen-sab-layout: wrote {}", target.display());
        Ok(())
    }
}

fn render() -> String {
    let mut s = String::new();
    s.push_str(HEADER);
    s.push('\n');

    push_const(&mut s, "ABI_MAJOR", ABI_MAJOR as u64);
    push_const(&mut s, "ABI_MINOR", ABI_MINOR as u64);
    s.push_str("export const ABI_VERSION: readonly [number, number] = [ABI_MAJOR, ABI_MINOR];\n\n");

    s.push_str("// --- Region offsets ---------------------------------------------------\n");
    push_const(&mut s, "SAB_SIZE", SAB_SIZE as u64);
    push_const(&mut s, "OFF_REQ_HEAD", OFF_REQ_HEAD as u64);
    push_const(&mut s, "OFF_REQ_TAIL", OFF_REQ_TAIL as u64);
    push_const(&mut s, "OFF_RES_HEAD", OFF_RES_HEAD as u64);
    push_const(&mut s, "OFF_RES_TAIL", OFF_RES_TAIL as u64);
    push_const(&mut s, "OFF_USER_WAIT_SLOT", OFF_USER_WAIT_SLOT as u64);
    push_const(&mut s, "OFF_KERNEL_WAIT_SLOT", OFF_KERNEL_WAIT_SLOT as u64);
    push_const(&mut s, "OFF_USER_BLOCK_COUNT", OFF_USER_BLOCK_COUNT as u64);
    push_const(&mut s, "OFF_KERNEL_BLOCK_COUNT", OFF_KERNEL_BLOCK_COUNT as u64);
    push_const(&mut s, "OFF_HEADER_FLAGS", OFF_HEADER_FLAGS as u64);
    push_const(&mut s, "OFF_REQ_RING", OFF_REQ_RING as u64);
    push_const(&mut s, "REQ_RING_BYTES", REQ_RING_BYTES as u64);
    push_const(&mut s, "OFF_RES_RING", OFF_RES_RING as u64);
    push_const(&mut s, "RES_RING_BYTES", RES_RING_BYTES as u64);
    push_const(&mut s, "OFF_HEAP_SCRATCH", OFF_HEAP_SCRATCH as u64);
    push_const(&mut s, "HEAP_SCRATCH_BYTES", HEAP_SCRATCH_BYTES as u64);
    s.push('\n');

    s.push_str("// --- Slot geometry ----------------------------------------------------\n");
    push_const(&mut s, "SLOT_SIZE", SLOT_SIZE as u64);
    push_const(&mut s, "REQ_SLOT_COUNT", REQ_SLOT_COUNT as u64);
    push_const(&mut s, "RES_SLOT_COUNT", RES_SLOT_COUNT as u64);
    s.push('\n');

    s.push_str("// --- Magic wait-slot status values ------------------------------------\n");
    push_const(&mut s, "STATUS_IDLE", STATUS_IDLE as u64);
    push_const(&mut s, "STATUS_REQUESTED", STATUS_REQUESTED as u64);
    push_const(&mut s, "STATUS_SERVICING", STATUS_SERVICING as u64);
    push_const(&mut s, "STATUS_READY", STATUS_READY as u64);
    s.push('\n');

    s.push_str(FOOTER);
    s
}

fn push_const(s: &mut String, name: &str, value: u64) {
    s.push_str(&format!("export const {name} = 0x{value:X};\n"));
}

const HEADER: &str = r#"// AUTOGENERATED by `cargo run -p xtask -- gen-sab-layout`.
// Do not edit by hand. Source of truth is the `abi` crate
// (crates/abi/src/ring.rs and crates/abi/src/version.rs).
//
// This file is the TypeScript mirror of the SAB ring layout
// shared between the kernel Worker (Rust WASM) and every
// user-process Worker. A Vitest test in
// `web/tests/unit/sab-layout.test.ts` asserts that the values
// here match what the `abi` crate would emit; CI additionally
// runs `cargo run -p xtask -- gen-sab-layout --check` to fail
// loudly if this file is stale.

/* eslint-disable */
"#;

const FOOTER: &str = r#"// --- Typed helpers for the TypeScript driver layer --------------------

/** A typed wrapper that exposes the SAB's u32 fields as AtomicInt32Array slots. */
export function sabHeader(sab: SharedArrayBuffer): Int32Array {
  return new Int32Array(sab, 0, OFF_HEAP_SCRATCH / 4);
}

/** The heap scratch region as a Uint8Array for arbitrary payloads. */
export function sabHeapScratch(sab: SharedArrayBuffer): Uint8Array {
  return new Uint8Array(sab, OFF_HEAP_SCRATCH, HEAP_SCRATCH_BYTES);
}
"#;

fn repo_root() -> Result<PathBuf> {
    let start = std::env::current_dir()?;
    let mut dir: &Path = &start;
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.exists() {
            let text = fs::read_to_string(&candidate)?;
            if text.contains("[workspace]") {
                return Ok(dir.to_path_buf());
            }
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return Err("could not find workspace root".into()),
        }
    }
}
