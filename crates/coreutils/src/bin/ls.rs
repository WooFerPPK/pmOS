//! POSIX-ish `ls`: list directory contents (or print a file's name)
//! one entry per line, alphabetically sorted. With multiple path
//! args, each section is prefixed by `<path>:` in GNU-ls style. No
//! flags in this slice (`-l` / `-a` / `-1` / `-h` / `-R` are deferred
//! follow-ups); per-path errors print `ls: <path>: <err>` to stderr,
//! continue with remaining paths, and flip the had-error flag.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let paths: Vec<String> = if args.is_empty() {
        vec![".".to_string()]
    } else {
        args
    };

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
        if let Err(e) = list_one(&mut out, Path::new(path)) {
            let _ = writeln!(io::stderr(), "ls: {path}: {e}");
            had_error = true;
        }
    }

    if had_error { ExitCode::from(1) } else { ExitCode::from(0) }
}

fn list_one<W: Write>(out: &mut W, path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        let mut names: Vec<String> = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        for name in names {
            writeln!(out, "{name}")?;
        }
    } else {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        writeln!(out, "{name}")?;
    }
    Ok(())
}
