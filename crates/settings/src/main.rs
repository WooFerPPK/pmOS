//! `/usr/bin/settings` — CLI preview of `/etc/preferences.toml` + About pane
//! (T184 partial + T192 partial).
//!
//! The tabbed graphical UI (T184..T192) is blocked on the toolkit
//! `Container` widget and T110's display-server protocol dispatch,
//! so this slice ships a debugging CLI. Subcommand dispatch is a
//! deliberate single hand-rolled `match` on `argv[1]` rather than a
//! `clap` dep — the About pane is the only subcommand today and the
//! preferences-dump is the bare default, so a two-arm match stays
//! smaller than the dependency cost.
//!
//! Invocations:
//!   settings                              # dump /etc/preferences.toml
//!   settings <path>                       # dump preferences from <path>
//!   settings about                        # print About (version + ABI + LICENSE + CREDITS)
//!   settings about --doc-root <dir>       # About with doc fixtures from <dir> (tests)
//!
//! Deferred T192 scope: `/proc/version` (procfs emits a placeholder
//! line today and is not yet a stable source) and `/proc/storage`
//! (blocked on T169). Both land in follow-up slices.

use std::process::ExitCode;

/// PMos userland-visible version string (T192 partial).
///
/// Hardcoded for v1 because `/proc/version` is not yet a stable
/// source — kernel procfs emits "PMos 0.1.0 (native-test)" as a
/// placeholder. Bump in lockstep with the workspace version when
/// the first real release ships.
const PMOS_VERSION: &str = "v0.1.0-alpha";

/// Default directory for bundled docs, mounted by mkfs at T193.
const DEFAULT_DOC_ROOT: &str = "/usr/share/doc/pmos/";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("about") => run_about(&args[1..]),
        _ => run_preferences(args.first().map(String::as_str).unwrap_or("/etc/preferences.toml")),
    }
}

fn run_preferences(path: &str) -> ExitCode {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("settings: failed to open {}: {}", path, e);
            return ExitCode::from(1);
        }
    };

    let prefs = match preferences::Preferences::parse(&bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("settings: failed to parse {}: {:?}", path, e);
            return ExitCode::from(1);
        }
    };

    print_field("theme.name", prefs.theme_name.as_deref());
    print_field("theme.fit", prefs.theme_fit.as_deref());
    print_field("wallpaper.name", prefs.wallpaper_name.as_deref());
    print_field("keyboard.layout", prefs.keyboard_layout.as_deref());
    print_field("timezone.iana", prefs.timezone_iana.as_deref());
    print_field("terminal.font", prefs.terminal_font.as_deref());

    ExitCode::from(0)
}

fn run_about(rest: &[String]) -> ExitCode {
    let mut doc_root: &str = DEFAULT_DOC_ROOT;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--doc-root" => {
                let Some(next) = rest.get(i + 1) else {
                    eprintln!("settings: about: --doc-root requires a directory argument");
                    return ExitCode::from(1);
                };
                doc_root = next.as_str();
                i += 2;
            }
            other => {
                eprintln!("settings: about: unknown argument {:?}", other);
                return ExitCode::from(1);
            }
        }
    }

    let license_path = join_doc(doc_root, "LICENSE.txt");
    let credits_path = join_doc(doc_root, "CREDITS.txt");

    let license = match std::fs::read_to_string(&license_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("settings: about: failed to read {}: {}", license_path, e);
            return ExitCode::from(1);
        }
    };
    let credits = match std::fs::read_to_string(&credits_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("settings: about: failed to read {}: {}", credits_path, e);
            return ExitCode::from(1);
        }
    };

    println!("PMos {}", PMOS_VERSION);
    println!(
        "Kernel ABI version: {}.{}",
        abi::version::ABI_MAJOR,
        abi::version::ABI_MINOR
    );
    println!();
    println!("License:");
    print!("{}", license);
    if !license.ends_with('\n') {
        println!();
    }
    println!();
    println!("Credits:");
    print!("{}", credits);
    if !credits.ends_with('\n') {
        println!();
    }

    ExitCode::from(0)
}

fn join_doc(root: &str, file: &str) -> String {
    if root.ends_with('/') {
        format!("{}{}", root, file)
    } else {
        format!("{}/{}", root, file)
    }
}

fn print_field(key: &str, value: Option<&str>) {
    match value {
        Some(v) => println!("{:<15} = \"{}\"", key, v),
        None => println!("{:<15} = (unset)", key),
    }
}
