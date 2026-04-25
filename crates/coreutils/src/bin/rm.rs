//! T146 follow-up — POSIX-ish `rm`: remove each named file with
//! `std::fs::remove_file`. With `-r` (or `-R`), a directory target
//! is removed recursively via `std::fs::remove_dir_all`. Without
//! `-r`, a directory target is a failure (POSIX `rm` without `-r`
//! refuses directories). A missing path is a failure. Multi-path
//! invocations follow the same partial-success shape as
//! cat/grep/cp/mkdir: each path is attempted, the per-path error
//! goes to stderr as `rm: <path>: <error>`, and the exit code is 1
//! if any path failed, 0 otherwise. Force (`-f`), interactive
//! (`-i`), and verbose (`-v`) are follow-up slices.
//!
//! Symlinks under `-r`: `remove_dir_all` follows POSIX semantics for
//! the entry it's invoked on (a symlink-to-dir at the top is removed
//! as a symlink, contents under it are not touched), matching the
//! standard library default; preserving more granular symlink
//! behaviour is a future flag follow-up.
//!
//! Flag parsing mirrors `cp -r` (commit `b2623ff`) and `mkdir -p`
//! (commit `bbf6b58`): single-pass arg split; `-r` and `-R` toggle
//! recursion; everything after `--` is forced into paths regardless
//! of leading `-`. Unknown flags write `rm: unknown flag: <flag>`
//! to stderr and exit 2 (distinct from per-path exit 1).
//!
//! Pattern precedent: `crates/coreutils/src/bin/{cat,grep,cp,mkdir,mv}.rs`.
//! Argv parse is hand-rolled (no `clap`), `std::fs` only (no WASI).

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut recursive = false;
    let mut paths: Vec<String> = Vec::new();
    let mut sep_seen = false;
    for arg in args {
        if !sep_seen && arg == "--" {
            sep_seen = true;
            continue;
        }
        if !sep_seen && arg.starts_with('-') && arg != "-" {
            if arg == "-r" || arg == "-R" {
                recursive = true;
            } else {
                let _ = writeln!(io::stderr(), "rm: unknown flag: {arg}");
                return ExitCode::from(2);
            }
        } else {
            paths.push(arg);
        }
    }

    if paths.is_empty() {
        let _ = writeln!(io::stderr(), "rm: usage: rm [-r] <path>...");
        return ExitCode::from(1);
    }

    let mut had_error = false;
    for path in &paths {
        let res = if recursive && is_dir(Path::new(path)) {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        if let Err(e) = res {
            let _ = writeln!(io::stderr(), "rm: {path}: {e}");
            had_error = true;
        }
    }

    if had_error {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}

fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.is_dir())
        .unwrap_or(false)
}
