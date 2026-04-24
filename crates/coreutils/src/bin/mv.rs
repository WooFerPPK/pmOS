//! T146 partial — POSIX-ish `mv`: move a single source to a single
//! destination. Primary path is `std::fs::rename(src, dst)` which is
//! atomic on a single filesystem. If the rename trips
//! `ErrorKind::CrossesDevices` (src and dst on different mounts,
//! where an in-place rename is impossible), we fall back to
//! `std::fs::copy(src, dst)` followed by `std::fs::remove_file(src)`.
//!
//! Any other rename error (src missing, dst parent missing,
//! permission denied, etc.) is reported to stderr as
//! `mv: <src> -> <dst>: <error>` and the process exits 1.
//!
//! Cross-device fallback caveat: on most test hosts
//! `std::env::temp_dir()` is a single mount, so the `CrossesDevices`
//! arm is not directly exercised by integration tests. The logic is
//! straightforward — copy the bytes, then unlink the source — and if
//! `remove_file` fails after a successful copy we still exit 1 but
//! leave `dst` in place rather than trying to roll back (a partial
//! move is preferable to data loss).
//!
//! No `-i` / `-f` / `-n` / `-v` flags — single-slice simplicity;
//! overwriting `dst` is the POSIX default. Pattern precedent:
//! `crates/coreutils/src/bin/{cp,mkdir}.rs`.

use std::env;
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let [src, dst] = match args.as_slice() {
        [a, b] => [a, b],
        _ => {
            let _ = writeln!(io::stderr(), "mv: usage: mv <src> <dst>");
            return ExitCode::from(1);
        }
    };

    match fs::rename(src, dst) {
        Ok(()) => ExitCode::from(0),
        Err(e) if e.kind() == ErrorKind::CrossesDevices => {
            if let Err(ce) = fs::copy(src, dst) {
                let _ = writeln!(io::stderr(), "mv: {src} -> {dst}: {ce}");
                return ExitCode::from(1);
            }
            if let Err(re) = fs::remove_file(src) {
                let _ = writeln!(io::stderr(), "mv: {src} -> {dst}: {re}");
                return ExitCode::from(1);
            }
            ExitCode::from(0)
        }
        Err(e) => {
            let _ = writeln!(io::stderr(), "mv: {src} -> {dst}: {e}");
            ExitCode::from(1)
        }
    }
}
