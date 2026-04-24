//! `/usr/bin/settings` — CLI preview of `/etc/preferences.toml` (T184 slice).
//!
//! The tabbed graphical UI (T184..T192) is blocked on the toolkit
//! `Container` widget and T110's display-server protocol dispatch,
//! so this slice ships a debugging CLI that reads the config file,
//! runs it through the `preferences` crate, and prints a fixed
//! six-line snapshot of the current settings. A future slice will
//! either gate this behind `--cli` or fork a `settings-cli` binary.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path: &str = args.first().map(String::as_str).unwrap_or("/etc/preferences.toml");

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

fn print_field(key: &str, value: Option<&str>) {
    match value {
        Some(v) => println!("{:<15} = \"{}\"", key, v),
        None => println!("{:<15} = (unset)", key),
    }
}
