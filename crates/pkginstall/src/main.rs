//! `/usr/bin/pkginstall` (T198) — install a `.pmpkg.tar` bundle.
//!
//! Validates the bundle against `pkg::validate_bundle`, extracts
//! it under `/opt/<name>/`, and writes
//! `/usr/share/applications/<name>.desktop` per
//! `contracts/package-manifest.md §3`.

use std::process::ExitCode;

const DEFAULT_OPT_ROOT: &str = "/opt";
const DEFAULT_APP_DIR: &str = "/usr/share/applications";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut bundle_path: Option<String> = None;
    let mut opt_root = DEFAULT_OPT_ROOT.to_string();
    let mut app_dir = DEFAULT_APP_DIR.to_string();
    let mut upgrade = false;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--opt-root" => {
                opt_root = iter.next().unwrap_or_else(|| {
                    eprintln!("pkginstall: --opt-root requires a value");
                    std::process::exit(2);
                });
            }
            "--app-dir" => {
                app_dir = iter.next().unwrap_or_else(|| {
                    eprintln!("pkginstall: --app-dir requires a value");
                    std::process::exit(2);
                });
            }
            "--upgrade" => upgrade = true,
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other if !other.starts_with('-') && bundle_path.is_none() => {
                bundle_path = Some(other.to_string());
            }
            other => {
                eprintln!("pkginstall: unrecognised arg {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(bundle) = bundle_path else {
        print_usage();
        return ExitCode::from(2);
    };
    match pkginstall::install(&bundle, &opt_root, &app_dir, upgrade) {
        Ok(name) => {
            println!("pkginstall: installed {name}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("pkginstall: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: pkginstall [--opt-root DIR] [--app-dir DIR] [--upgrade] <bundle.pmpkg.tar>"
    );
}

