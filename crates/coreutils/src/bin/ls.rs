//! POSIX-ish `ls`: list directory contents (or print a file's name)
//! one entry per line, alphabetically sorted. With multiple path
//! args, each section is prefixed by `<path>:` in GNU-ls style.
//! Per-path errors print `ls: <path>: <err>` to stderr, continue
//! with remaining paths, and flip the had-error flag (exit 1).
//!
//! Flags supported: `-l` (long-format: emits `<type-letter>
//! <size-bytes> <name>` per row, where `<type-letter>` is `d` for
//! directory / `l` for symlink / `-` for regular file, `<size-bytes>`
//! is `metadata.len()` for files and `0` for directories — no
//! recursive sum). `-l` is a per-row formatter toggle; multi-path /
//! file-arg / no-arg / partial-success / sort behaviour is unchanged.
//! `-a` (show-all: include directory entries whose name starts with
//! `.`; without `-a`, dotfiles are filtered out from directory
//! listings — POSIX hide-by-default). v1 simplification: `-a` does
//! NOT synthesise `.` / `..` entries, only real on-disk entries
//! whose name happens to start with `.`. `-a` and `-l` combine
//! freely (`ls -la` = long format including dotfiles). `-a` only
//! affects directory listings; a file argument whose own name
//! starts with `.` is always printed regardless of `-a` (the user
//! named it explicitly).
//! Flags may appear before or after path args (`ls /tmp -l /home`)
//! or after a `--` separator. Unknown flags exit 2 with `ls: unknown
//! flag: <flag>` to stderr (distinct from per-path exit 1, so the
//! caller can tell bad invocation from per-path failure).
//!
//! Explicitly deferred (each its own future single-slice follow-up):
//! mode bits (`rwxrwxrwx`), owner / group, mtime, alignment-padded
//! columns, `-1` / `-h` / `-R` / `-r` / `-A` flags, char / block /
//! socket / fifo type letters (only `d` / `l` / `-` are recognised
//! today; everything else also formats as `-`).

use std::env;
use std::fs::{self, Metadata};
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut long_format = false;
    let mut show_all = false;
    let mut paths: Vec<String> = Vec::new();
    let mut sep_seen = false;
    for arg in args {
        if !sep_seen && arg == "--" {
            sep_seen = true;
            continue;
        }
        if !sep_seen && arg.starts_with('-') && arg != "-" {
            if arg == "-l" {
                long_format = true;
            } else if arg == "-a" {
                show_all = true;
            } else if arg == "-la" || arg == "-al" {
                long_format = true;
                show_all = true;
            } else {
                let _ = writeln!(io::stderr(), "ls: unknown flag: {arg}");
                return ExitCode::from(2);
            }
        } else {
            paths.push(arg);
        }
    }
    if paths.is_empty() {
        paths.push(".".to_string());
    }

    let multi = paths.len() > 1;
    let mut had_error = false;
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for (i, path) in paths.iter().enumerate() {
        if multi {
            if i > 0 {
                let _ = writeln!(out);
            }
            let _ = writeln!(out, "{path}:");
        }
        if let Err(e) = list_one(&mut out, Path::new(path), long_format, show_all) {
            let _ = writeln!(io::stderr(), "ls: {path}: {e}");
            had_error = true;
        }
    }

    if had_error {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}

fn list_one<W: Write>(
    out: &mut W,
    path: &Path,
    long_format: bool,
    show_all: bool,
) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        let mut rows: Vec<(String, Metadata)> = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !show_all && name.starts_with('.') {
                continue;
            }
            let entry_meta = fs::symlink_metadata(entry.path())?;
            rows.push((name, entry_meta));
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, entry_meta) in rows {
            write_row(out, &name, &entry_meta, long_format)?;
        }
    } else {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        write_row(out, &name, &meta, long_format)?;
    }
    Ok(())
}

fn write_row<W: Write>(
    out: &mut W,
    name: &str,
    meta: &Metadata,
    long_format: bool,
) -> io::Result<()> {
    if long_format {
        let type_letter = type_letter(meta);
        let size = if meta.is_dir() { 0 } else { meta.len() };
        writeln!(out, "{type_letter} {size} {name}")
    } else {
        writeln!(out, "{name}")
    }
}

fn type_letter(meta: &Metadata) -> char {
    if meta.is_dir() {
        'd'
    } else if meta.file_type().is_symlink() {
        'l'
    } else {
        '-'
    }
}
