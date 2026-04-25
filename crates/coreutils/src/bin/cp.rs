//! POSIX-ish `cp`: copy a file, or with `-r` / `-R`, copy a
//! directory tree. Without `-r`, behaviour is the original
//! single-file shape — read `src` wholesale, write to `dst`,
//! overwriting any existing regular file (POSIX default); a
//! directory `src` errors out unchanged. With `-r` (or `-R`),
//! a directory `src` is walked via `std::fs::read_dir` and each
//! entry is mirrored under `dst`: subdirectories are created via
//! `fs::create_dir`, regular files via `fs::copy`. If `dst`
//! already exists as a directory when `src` is a directory, the
//! copy lands at `dst/<src-basename>/...` (GNU cp's "if dst is an
//! existing directory, copy src into dst" semantics — chosen
//! deliberately so re-running `cp -r src/ dst/` doesn't merge into
//! dst's root). If `dst` exists as a regular file when `src` is a
//! directory, error out without clobbering. File overwrites inside
//! the tree are unconditional in this slice (no `-n` / `-i`).
//!
//! Symlinks: `fs::copy` follows symlinks and copies the target's
//! bytes — preserving symlinks-as-symlinks is a future flag follow-up.
//!
//! Flag parsing mirrors `ls -l` (commit `c3d1aa7`): single-pass
//! arg split; `-r` and `-R` toggle recursion; everything after `--`
//! is forced into paths regardless of leading `-`. Unknown flags
//! write `cp: unknown flag: <flag>` to stderr and exit 2 (distinct
//! from per-path exit 1).
//!
//! Explicitly deferred (each its own future single-slice
//! follow-up): `-n` (no-clobber), `-i` (interactive), `-v`
//! (verbose), `-p` (preserve metadata), preserving symlinks as
//! symlinks, multi-source variadic invocations (`cp a b c dst/`).

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
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
                let _ = writeln!(io::stderr(), "cp: unknown flag: {arg}");
                return ExitCode::from(2);
            }
        } else {
            paths.push(arg);
        }
    }

    let [src, dst] = match paths.as_slice() {
        [a, b] => [a.clone(), b.clone()],
        _ => {
            let _ = writeln!(io::stderr(), "cp: usage: cp <src> <dst> (or `cp -r <src> <dst>` for recursive)");
            return ExitCode::from(1);
        }
    };

    let src_path = Path::new(&src);
    let dst_path = Path::new(&dst);

    let src_meta = match fs::symlink_metadata(src_path) {
        Ok(m) => m,
        Err(e) => {
            let _ = writeln!(io::stderr(), "cp: {src}: {e}");
            return ExitCode::from(1);
        }
    };

    if src_meta.is_dir() {
        if !recursive {
            let _ = writeln!(io::stderr(), "cp: {src}: is a directory");
            return ExitCode::from(1);
        }
        let target = match resolve_dir_target(src_path, dst_path) {
            Ok(t) => t,
            Err(e) => {
                let _ = writeln!(io::stderr(), "cp: {dst}: {e}");
                return ExitCode::from(1);
            }
        };
        if let Err(e) = copy_tree(src_path, &target) {
            let _ = writeln!(io::stderr(), "cp: {e}");
            return ExitCode::from(1);
        }
        return ExitCode::from(0);
    }

    let bytes = match fs::read(src_path) {
        Ok(b) => b,
        Err(e) => {
            let _ = writeln!(io::stderr(), "cp: {src}: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = fs::write(dst_path, &bytes) {
        let _ = writeln!(io::stderr(), "cp: {dst}: {e}");
        return ExitCode::from(1);
    }
    ExitCode::from(0)
}

fn resolve_dir_target(src: &Path, dst: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(dst) {
        Ok(m) if m.is_dir() => {
            let name = src.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "src has no basename")
            })?;
            Ok(dst.join(name))
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination exists and is not a directory",
        )),
        Err(ref e) if e.kind() == io::ErrorKind::NotFound => Ok(dst.to_path_buf()),
        Err(e) => Err(e),
    }
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dst) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{}: destination is not a directory", dst.display()),
            ));
        }
        Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(dst).map_err(|e| {
                io::Error::new(e.kind(), format!("{}: {e}", dst.display()))
            })?;
        }
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", dst.display()))),
    }

    for entry in fs::read_dir(src).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", src.display())))? {
        let entry = entry.map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", src.display())))?;
        let entry_meta = fs::symlink_metadata(entry.path())
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", entry.path().display())))?;
        let entry_dst = dst.join(entry.file_name());
        if entry_meta.is_dir() {
            copy_tree(&entry.path(), &entry_dst)?;
        } else {
            fs::copy(entry.path(), &entry_dst).map_err(|e| {
                io::Error::new(e.kind(), format!("{}: {e}", entry_dst.display()))
            })?;
        }
    }
    Ok(())
}
