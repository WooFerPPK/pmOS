//! `/usr/bin/settings` — CLI preview of `/etc/preferences.toml` + About pane
//! + write subcommands (T184 partial + T192 partial).
//!
//! The tabbed graphical UI (T184..T192) is blocked on the toolkit
//! `Container` widget and T110's display-server protocol dispatch,
//! so this slice ships a debugging CLI. Subcommand dispatch is a
//! deliberate single hand-rolled `match` on `argv[1]` rather than a
//! `clap` dep — established by T192 (cbbebf6) when `about` was added,
//! and extended here by T184 to add `set-theme`. The two-arm match
//! grew to three arms without reaching the size where clap would
//! pay for itself.
//!
//! Invocations:
//!   settings                              # dump /etc/preferences.toml
//!   settings <path>                       # dump preferences from <path>
//!   settings about                        # print About (version + ABI + LICENSE + CREDITS)
//!   settings about --doc-root <dir>       # About with doc fixtures from <dir> (tests)
//!   settings set-theme <name>             # write theme.name to /etc/preferences.toml
//!   settings set-theme <name> --config <path>
//!                                         # write theme.name to <path> (test hook)
//!   settings set-wallpaper <value>        # write wallpaper.name to /etc/preferences.toml
//!   settings set-wallpaper <value> --config <path>
//!                                         # write wallpaper.name to <path> (test hook)
//!   settings set-keyboard <layout>        # write keyboard.layout to /etc/preferences.toml
//!   settings set-keyboard <layout> --config <path>
//!                                         # write keyboard.layout to <path> (test hook)
//!
//! Deferred T192 scope: `/proc/version` (procfs emits a placeholder
//! line today and is not yet a stable source) and `/proc/storage`
//! (blocked on T169). Both land in follow-up slices.
//!
//! Deferred T184 scope: valid-theme allow-list. `set-theme` accepts
//! any non-empty string; the GUI will enforce the allow-list when it
//! ships. Empty strings are rejected so the TOML round-trip cannot
//! silently erase the field. `set-wallpaper` writes the
//! `wallpaper.name` field — the only field on the `[wallpaper]`
//! section in the v1 schema (see `crates/preferences/src/lib.rs`
//! T183 schema). A future `set-wallpaper-color` slice can land if
//! the schema gains a colour field. `set-keyboard` writes the
//! `keyboard.layout` field — the only field on the `[keyboard]`
//! section in the v1 schema. Valid-layout allow-list deferred to
//! the GUI for the same reason as `set-theme`.

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

/// Default preferences path, used by both dump and `set-theme`.
const DEFAULT_CONFIG: &str = "/etc/preferences.toml";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("about") => run_about(&args[1..]),
        Some("set-theme") => run_set_theme(&args[1..]),
        Some("set-wallpaper") => run_set_wallpaper(&args[1..]),
        Some("set-keyboard") => run_set_keyboard(&args[1..]),
        _ => run_preferences(args.first().map(String::as_str).unwrap_or(DEFAULT_CONFIG)),
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

/// `set-theme <name> [--config <path>]` — read-modify-write
/// `theme.name` on the preferences file, preserving every other
/// field.
///
/// Read-modify-write rather than a naive overwrite so the other
/// five preference fields survive untouched. We hand-serialise
/// because the `preferences` crate is intentionally parse-only and
/// `#![no_std]`; widening it to emit TOML would pull serialisation
/// concerns into init's `no_std + alloc` universe for no gain
/// today (only `settings` writes the file in v1). The canonical
/// emitter here is the inverse of the crate's parser: quoted
/// plain-string values, five sections in a fixed order, blank
/// lines between sections.
fn run_set_theme(rest: &[String]) -> ExitCode {
    let mut name: Option<&str> = None;
    let mut config_path: &str = DEFAULT_CONFIG;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--config" => {
                let Some(next) = rest.get(i + 1) else {
                    eprintln!("settings: set-theme: --config requires a path argument");
                    return ExitCode::from(1);
                };
                config_path = next.as_str();
                i += 2;
            }
            other => {
                if name.is_none() {
                    name = Some(other);
                    i += 1;
                } else {
                    eprintln!("settings: set-theme: unexpected argument {:?}", other);
                    return ExitCode::from(1);
                }
            }
        }
    }

    let Some(name) = name else {
        eprintln!("settings: set-theme: usage: set-theme <name>");
        return ExitCode::from(1);
    };

    if name.is_empty() {
        eprintln!("settings: set-theme: theme name must be non-empty");
        return ExitCode::from(1);
    }

    if name.contains('"') || name.contains('\n') {
        eprintln!(
            "settings: set-theme: theme name must not contain quotes or newlines"
        );
        return ExitCode::from(1);
    }

    let mut prefs = match std::fs::read(config_path) {
        Ok(bytes) => match preferences::Preferences::parse(&bytes) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "settings: set-theme: failed to parse {}: {:?}",
                    config_path, e
                );
                return ExitCode::from(1);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            preferences::Preferences::empty()
        }
        Err(e) => {
            eprintln!(
                "settings: set-theme: failed to open {}: {}",
                config_path, e
            );
            return ExitCode::from(1);
        }
    };

    prefs.theme_name = Some(name.to_string());

    let serialised = serialise_preferences(&prefs);

    if let Err(e) = std::fs::write(config_path, serialised.as_bytes()) {
        eprintln!(
            "settings: set-theme: failed to write {}: {}",
            config_path, e
        );
        return ExitCode::from(1);
    }

    // Mode 0o644 is left to the kernel VFS default; std::fs on the
    // host cannot set Unix permissions portably and the kernel
    // ignores host permission bits.

    ExitCode::from(0)
}

/// `set-wallpaper <value> [--config <path>]` — read-modify-write
/// `wallpaper.name` on the preferences file, preserving every other
/// field.
///
/// `wallpaper.name` is the only field on the `[wallpaper]` section
/// in the v1 schema (see `crates/preferences/src/lib.rs`), so
/// `set-wallpaper <value>` writes that field directly — no
/// disambiguation needed today. If the schema later grows a
/// `wallpaper.color`, that will land as a separate
/// `set-wallpaper-color` subcommand to keep argv parsing flat.
fn run_set_wallpaper(rest: &[String]) -> ExitCode {
    let mut value: Option<&str> = None;
    let mut config_path: &str = DEFAULT_CONFIG;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--config" => {
                let Some(next) = rest.get(i + 1) else {
                    eprintln!("settings: set-wallpaper: --config requires a path argument");
                    return ExitCode::from(1);
                };
                config_path = next.as_str();
                i += 2;
            }
            other => {
                if value.is_none() {
                    value = Some(other);
                    i += 1;
                } else {
                    eprintln!("settings: set-wallpaper: unexpected argument {:?}", other);
                    return ExitCode::from(1);
                }
            }
        }
    }

    let Some(value) = value else {
        eprintln!("settings: set-wallpaper: usage: set-wallpaper <value>");
        return ExitCode::from(1);
    };

    if value.is_empty() {
        eprintln!("settings: set-wallpaper: wallpaper value must be non-empty");
        return ExitCode::from(1);
    }

    if value.contains('"') || value.contains('\n') {
        eprintln!(
            "settings: set-wallpaper: wallpaper value must not contain quotes or newlines"
        );
        return ExitCode::from(1);
    }

    let mut prefs = match std::fs::read(config_path) {
        Ok(bytes) => match preferences::Preferences::parse(&bytes) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "settings: set-wallpaper: failed to parse {}: {:?}",
                    config_path, e
                );
                return ExitCode::from(1);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            preferences::Preferences::empty()
        }
        Err(e) => {
            eprintln!(
                "settings: set-wallpaper: failed to open {}: {}",
                config_path, e
            );
            return ExitCode::from(1);
        }
    };

    prefs.wallpaper_name = Some(value.to_string());

    let serialised = serialise_preferences(&prefs);

    if let Err(e) = std::fs::write(config_path, serialised.as_bytes()) {
        eprintln!(
            "settings: set-wallpaper: failed to write {}: {}",
            config_path, e
        );
        return ExitCode::from(1);
    }

    ExitCode::from(0)
}

/// `set-keyboard <layout> [--config <path>]` — read-modify-write
/// `keyboard.layout` on the preferences file, preserving every other
/// field.
///
/// `keyboard.layout` is the only field on the `[keyboard]` section
/// in the v1 schema (see `crates/preferences/src/lib.rs`), so
/// `set-keyboard <layout>` writes that field directly — no
/// disambiguation needed today. Mirrors `set-wallpaper` /
/// `set-theme` byte-for-byte: same `--config` test hook, same
/// validation (non-empty, no embedded `"` or `\n`), same exit
/// codes, same stderr shape.
fn run_set_keyboard(rest: &[String]) -> ExitCode {
    let mut layout: Option<&str> = None;
    let mut config_path: &str = DEFAULT_CONFIG;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--config" => {
                let Some(next) = rest.get(i + 1) else {
                    eprintln!("settings: set-keyboard: --config requires a path argument");
                    return ExitCode::from(1);
                };
                config_path = next.as_str();
                i += 2;
            }
            other => {
                if layout.is_none() {
                    layout = Some(other);
                    i += 1;
                } else {
                    eprintln!("settings: set-keyboard: unexpected argument {:?}", other);
                    return ExitCode::from(1);
                }
            }
        }
    }

    let Some(layout) = layout else {
        eprintln!("settings: set-keyboard: usage: set-keyboard <layout>");
        return ExitCode::from(1);
    };

    if layout.is_empty() {
        eprintln!("settings: set-keyboard: keyboard layout must be non-empty");
        return ExitCode::from(1);
    }

    if layout.contains('"') || layout.contains('\n') {
        eprintln!(
            "settings: set-keyboard: keyboard layout must not contain quotes or newlines"
        );
        return ExitCode::from(1);
    }

    let mut prefs = match std::fs::read(config_path) {
        Ok(bytes) => match preferences::Preferences::parse(&bytes) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "settings: set-keyboard: failed to parse {}: {:?}",
                    config_path, e
                );
                return ExitCode::from(1);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            preferences::Preferences::empty()
        }
        Err(e) => {
            eprintln!(
                "settings: set-keyboard: failed to open {}: {}",
                config_path, e
            );
            return ExitCode::from(1);
        }
    };

    prefs.keyboard_layout = Some(layout.to_string());

    let serialised = serialise_preferences(&prefs);

    if let Err(e) = std::fs::write(config_path, serialised.as_bytes()) {
        eprintln!(
            "settings: set-keyboard: failed to write {}: {}",
            config_path, e
        );
        return ExitCode::from(1);
    }

    ExitCode::from(0)
}

/// Emit a TOML document that round-trips through `preferences::Preferences::parse`.
///
/// Sections are emitted in the canonical order used throughout the
/// spec: theme, wallpaper, keyboard, timezone, terminal. Empty
/// sections (all fields `None`) are skipped so the resulting file
/// stays minimal. Values are plain ASCII-quoted strings; callers
/// must reject names that contain embedded `"` or newlines before
/// calling here.
fn serialise_preferences(prefs: &preferences::Preferences) -> String {
    let mut out = String::new();

    let theme_pairs: &[(&str, Option<&str>)] = &[
        ("name", prefs.theme_name.as_deref()),
        ("fit", prefs.theme_fit.as_deref()),
    ];
    emit_section(&mut out, "theme", theme_pairs);

    let wallpaper_pairs: &[(&str, Option<&str>)] =
        &[("name", prefs.wallpaper_name.as_deref())];
    emit_section(&mut out, "wallpaper", wallpaper_pairs);

    let keyboard_pairs: &[(&str, Option<&str>)] =
        &[("layout", prefs.keyboard_layout.as_deref())];
    emit_section(&mut out, "keyboard", keyboard_pairs);

    let timezone_pairs: &[(&str, Option<&str>)] =
        &[("iana", prefs.timezone_iana.as_deref())];
    emit_section(&mut out, "timezone", timezone_pairs);

    let terminal_pairs: &[(&str, Option<&str>)] =
        &[("font", prefs.terminal_font.as_deref())];
    emit_section(&mut out, "terminal", terminal_pairs);

    out
}

fn emit_section(out: &mut String, name: &str, pairs: &[(&str, Option<&str>)]) {
    if pairs.iter().all(|(_, v)| v.is_none()) {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push('[');
    out.push_str(name);
    out.push_str("]\n");
    for (key, value) in pairs {
        if let Some(v) = value {
            out.push_str(key);
            out.push_str(" = \"");
            out.push_str(v);
            out.push_str("\"\n");
        }
    }
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
