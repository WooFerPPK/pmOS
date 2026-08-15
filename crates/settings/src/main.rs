//! `/usr/bin/settings` — tabbed graphical preference editor plus CLI/About
//! diagnostics (T184 partial + T192 partial).
//!
//! A desktop launch with no arguments opens the graphical UI. Explicit
//! subcommands retain the host-testable diagnostic and scripting surface.
//! Dispatch remains a small hand-rolled `match` rather than adding a command
//! parser dependency to this system utility.
//!
//! Invocations:
//!   settings                              # dump /etc/preferences.toml
//!   settings <path>                       # dump preferences from <path>
//!   settings about                        # print About (version + ABI + LICENSE + CREDITS + Storage)
//!   settings about --doc-root <dir>       # About with doc fixtures from <dir> (tests)
//!   settings about --proc-storage <path>  # About with /proc/storage redirected (tests)
//!   settings about --human                # Storage byte values formatted as B / KB / MB / GB
//!   settings set-theme <name>             # write theme.name to /etc/preferences.toml
//!   settings set-theme <name> --config <path>
//!                                         # write theme.name to <path> (test hook)
//!   settings set-wallpaper <value>        # write wallpaper.name to /etc/preferences.toml
//!   settings set-wallpaper <value> --config <path>
//!                                         # write wallpaper.name to <path> (test hook)
//!   settings set-keyboard <layout>        # write keyboard.layout to /etc/preferences.toml
//!   settings set-keyboard <layout> --config <path>
//!                                         # write keyboard.layout to <path> (test hook)
//!   settings set-timezone <name>          # write timezone.iana to /etc/preferences.toml
//!   settings set-timezone <name> --config <path>
//!                                         # write timezone.iana to <path> (test hook)
//!
//! Deferred T192 scope: `/proc/version` (procfs emits a placeholder
//! line today and is not yet a stable source). `/proc/storage`
//! reading landed as a T192 follow-up after T169's
//! `KernelProcFsSource` bridge wired the live block-driver counters
//! into procfs — `about` now reads it as a regular file and emits a
//! Storage section. A missing `/proc/storage` is silently elided
//! (many test environments will not mount procfs and the existing
//! `about_prints_*` tests would otherwise break); a malformed body
//! warns to stderr and skips the section without failing the
//! command. The `--human` flag formats `quota` / `used` as
//! base-1024 B / KB / MB / GB with one decimal place above 1 KB
//! (`files` stays as a raw integer count either way).
//!
//! CLI setters deliberately accept forward-compatible values; the graphical
//! UI enforces its bundled allow-lists. Empty strings are rejected so the TOML round-trip cannot
//! silently erase the field. `set-wallpaper` writes the
//! `wallpaper.name` field — the only field on the `[wallpaper]`
//! section in the v1 schema (see `crates/preferences/src/lib.rs`
//! T183 schema). A future `set-wallpaper-color` slice can land if
//! the schema gains a colour field. `set-keyboard` writes the
//! `keyboard.layout` field — the only field on the `[keyboard]`
//! section in the v1 schema. Valid-layout allow-list deferred to
//! the GUI for the same reason as `set-theme`. `set-timezone` writes
//! the `timezone.iana` field — the only field on the `[timezone]`
//! section in the v1 schema. Valid-IANA-zone allow-list deferred to
//! the GUI for the same reason as `set-theme`.

use std::process::ExitCode;

#[cfg(any(target_arch = "wasm32", test))]
mod gui;

/// PMos userland-visible version string (T192 partial).
///
/// Hardcoded for v1 because `/proc/version` is not yet a stable
/// source — kernel procfs emits "PMos 0.1.0 (native-test)" as a
/// placeholder. Bump in lockstep with the workspace version when
/// the first real release ships.
const PMOS_VERSION: &str = "v0.1.0-alpha";

/// Default directory for bundled docs, mounted by mkfs at T193.
const DEFAULT_DOC_ROOT: &str = "/usr/share/doc/pmos/";

/// Default preferences path shared with init and the desktop shell.
const DEFAULT_CONFIG: &str = preferences::DEFAULT_PATH;

/// Default `/proc/storage` path. Populated by the kernel's procfs
/// module (T169) once the block-driver counter source is installed.
/// Tests redirect this via the `--proc-storage` flag on `about`
/// because their hosts do not mount procfs.
const DEFAULT_PROC_STORAGE: &str = "/proc/storage";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    #[cfg(target_arch = "wasm32")]
    {
        // The desktop-launched `settings` binary boots straight
        // into the GUI when no args are passed. The shell or a
        // human running `settings <subcommand>` from a terminal
        // takes the CLI dispatch below.
        if args.is_empty() {
            return run_gui_wasm();
        }
        if args.first().map(String::as_str) == Some("gui") {
            return run_gui_wasm();
        }
    }

    match args.first().map(String::as_str) {
        Some("about") => run_about(&args[1..]),
        Some("set-theme") => run_set_theme(&args[1..]),
        Some("set-wallpaper") => run_set_wallpaper(&args[1..]),
        Some("set-keyboard") => run_set_keyboard(&args[1..]),
        Some("set-timezone") => run_set_timezone(&args[1..]),
        Some("gui") => {
            #[cfg(not(target_arch = "wasm32"))]
            {
                eprintln!("settings: gui subcommand is only available in the wasm build");
                ExitCode::from(2)
            }
            #[cfg(target_arch = "wasm32")]
            {
                run_gui_wasm()
            }
        }
        _ => run_preferences(args.first().map(String::as_str).unwrap_or(DEFAULT_CONFIG)),
    }
}

#[cfg(target_arch = "wasm32")]
fn run_gui_wasm() -> ExitCode {
    extern crate alloc;
    let conn = match toolkit::wasi::FdConnection::connect() {
        Ok(connection) => connection,
        Err(errno) => return ExitCode::from(errno as u8),
    };
    match gui::run(conn, DEFAULT_CONFIG) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
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
    let mut proc_storage: &str = DEFAULT_PROC_STORAGE;
    let mut human = false;
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
            "--proc-storage" => {
                let Some(next) = rest.get(i + 1) else {
                    eprintln!("settings: about: --proc-storage requires a path argument");
                    return ExitCode::from(1);
                };
                proc_storage = next.as_str();
                i += 2;
            }
            "--human" => {
                human = true;
                i += 1;
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

    let storage = read_proc_storage(proc_storage);

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

    if let Some((quota, used, files)) = storage {
        println!();
        println!("Storage:");
        if human {
            println!("  quota: {}", format_bytes_human(quota));
            println!("  used:  {}", format_bytes_human(used));
        } else {
            println!("  quota: {}", quota);
            println!("  used:  {}", used);
        }
        println!("  files: {}", files);
    }

    ExitCode::from(0)
}

/// Format a byte count using base-1024 thresholds.
///
/// Values at or above 1 GiB render as `<x.x> GB`, at or above 1 MiB as
/// `<x.x> MB`, at or above 1 KiB as `<x.x> KB`, otherwise `<n> B`. The suffixes
/// keep the familiar KB/MB/GB letters even though the multiplier
/// is binary — matches the convention the existing `du`/`df`
/// userland uses for quick human reads. One decimal place is
/// enough at the About-pane glance fidelity; tests that need
/// exact byte counts continue to use the default (no `--human`)
/// path.
fn format_bytes_human(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    if n >= GIB {
        format!("{:.1} GB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.1} MB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KB", n as f64 / KIB as f64)
    } else {
        format!("{} B", n)
    }
}

/// Read `/proc/storage` and return parsed `(quota, used, files)`.
///
/// File format (per T169's `format_storage_snapshot` in
/// `crates/kernel/src/fs/procfs.rs`): `<quota> <used> <files>\n` —
/// three space-separated u64 values on a single line.
///
/// Returns `None` (and silently elides the Storage section) if the
/// file is missing — many test environments will not mount procfs
/// and the existing `about_prints_*` tests must keep passing. A
/// malformed body warns to stderr and also returns `None` so the
/// command exits cleanly. Any other I/O error (permission denied,
/// etc.) is also reported to stderr without failing the command —
/// the About pane is informational, not a hard procfs probe.
fn read_proc_storage(path: &str) -> Option<(u64, u64, u64)> {
    let body = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("settings: about: failed to read {}: {}", path, e);
            return None;
        }
    };

    let mut tokens = body.split_ascii_whitespace();
    let quota_tok = tokens.next();
    let used_tok = tokens.next();
    let files_tok = tokens.next();

    let (Some(quota_tok), Some(used_tok), Some(files_tok)) = (quota_tok, used_tok, files_tok)
    else {
        eprintln!(
            "settings: failed to parse {}: expected three whitespace-separated u64 fields",
            path
        );
        return None;
    };

    let quota = match quota_tok.parse::<u64>() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("settings: failed to parse {}: quota: {}", path, e);
            return None;
        }
    };
    let used = match used_tok.parse::<u64>() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("settings: failed to parse {}: used: {}", path, e);
            return None;
        }
    };
    let files = match files_tok.parse::<u64>() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("settings: failed to parse {}: files: {}", path, e);
            return None;
        }
    };

    Some((quota, used, files))
}

/// `set-theme <name> [--config <path>]` — read-modify-write
/// `theme.name` on the preferences file, preserving every other field. The
/// shared preferences crate serializes the complete snapshot and Settings
/// atomically renames it into place.
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
        eprintln!("settings: set-theme: theme name must not contain quotes or newlines");
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => preferences::Preferences::empty(),
        Err(e) => {
            eprintln!("settings: set-theme: failed to open {}: {}", config_path, e);
            return ExitCode::from(1);
        }
    };

    prefs.theme_name = Some(name.to_string());

    if let Err(e) = write_preferences(config_path, &prefs) {
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
        eprintln!("settings: set-wallpaper: wallpaper value must not contain quotes or newlines");
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => preferences::Preferences::empty(),
        Err(e) => {
            eprintln!(
                "settings: set-wallpaper: failed to open {}: {}",
                config_path, e
            );
            return ExitCode::from(1);
        }
    };

    prefs.wallpaper_name = Some(value.to_string());

    if let Err(e) = write_preferences(config_path, &prefs) {
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
        eprintln!("settings: set-keyboard: keyboard layout must not contain quotes or newlines");
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => preferences::Preferences::empty(),
        Err(e) => {
            eprintln!(
                "settings: set-keyboard: failed to open {}: {}",
                config_path, e
            );
            return ExitCode::from(1);
        }
    };

    prefs.keyboard_layout = Some(layout.to_string());

    if let Err(e) = write_preferences(config_path, &prefs) {
        eprintln!(
            "settings: set-keyboard: failed to write {}: {}",
            config_path, e
        );
        return ExitCode::from(1);
    }

    ExitCode::from(0)
}

/// `set-timezone <name> [--config <path>]` — read-modify-write
/// `timezone.iana` on the preferences file, preserving every other
/// field.
///
/// `timezone.iana` is the only field on the `[timezone]` section
/// in the v1 schema (see `crates/preferences/src/lib.rs`), so
/// `set-timezone <name>` writes that field directly — no
/// disambiguation needed today. Mirrors `set-keyboard` /
/// `set-wallpaper` / `set-theme` byte-for-byte: same `--config`
/// test hook, same validation (non-empty, no embedded `"` or `\n`),
/// same exit codes, same stderr shape.
fn run_set_timezone(rest: &[String]) -> ExitCode {
    let mut name: Option<&str> = None;
    let mut config_path: &str = DEFAULT_CONFIG;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--config" => {
                let Some(next) = rest.get(i + 1) else {
                    eprintln!("settings: set-timezone: --config requires a path argument");
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
                    eprintln!("settings: set-timezone: unexpected argument {:?}", other);
                    return ExitCode::from(1);
                }
            }
        }
    }

    let Some(name) = name else {
        eprintln!("settings: set-timezone: usage: set-timezone <name>");
        return ExitCode::from(1);
    };

    if name.is_empty() {
        eprintln!("settings: set-timezone: timezone name must be non-empty");
        return ExitCode::from(1);
    }

    if name.contains('"') || name.contains('\n') {
        eprintln!("settings: set-timezone: timezone name must not contain quotes or newlines");
        return ExitCode::from(1);
    }

    let mut prefs = match std::fs::read(config_path) {
        Ok(bytes) => match preferences::Preferences::parse(&bytes) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "settings: set-timezone: failed to parse {}: {:?}",
                    config_path, e
                );
                return ExitCode::from(1);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => preferences::Preferences::empty(),
        Err(e) => {
            eprintln!(
                "settings: set-timezone: failed to open {}: {}",
                config_path, e
            );
            return ExitCode::from(1);
        }
    };

    prefs.timezone_iana = Some(name.to_string());

    if let Err(e) = write_preferences(config_path, &prefs) {
        eprintln!(
            "settings: set-timezone: failed to write {}: {}",
            config_path, e
        );
        return ExitCode::from(1);
    }

    ExitCode::from(0)
}

/// Persist a complete preference snapshot without exposing readers to a
/// truncated intermediate file. The temporary file lives beside the target,
/// so the final rename stays within one VFS directory and is atomic.
pub(crate) fn write_preferences(
    path: &str,
    prefs: &preferences::Preferences,
) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind, Write};
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    let serialised = prefs
        .to_toml()
        .map_err(|error| Error::new(ErrorKind::InvalidInput, format!("{error:?}")))?;
    let target = Path::new(path);
    let parent = target.parent().filter(|dir| !dir.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }

    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "preference path has no file name"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut temporary_file = None;
    for attempt in 0..32_u32 {
        let temporary_name = format!(".{file_name}.{nonce}.{attempt}.tmp");
        let temporary = match parent {
            Some(parent) => parent.join(temporary_name),
            None => temporary_name.into(),
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => {
                temporary_file = Some((temporary, file));
                break;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let Some((temporary, mut file)) = temporary_file else {
        return Err(Error::new(
            ErrorKind::AlreadyExists,
            "could not allocate a unique preferences temporary file",
        ));
    };
    if let Err(error) = file
        .write_all(serialised.as_bytes())
        .and_then(|()| file.sync_all())
    {
        drop(file);
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);

    match std::fs::rename(&temporary, target) {
        Ok(()) => {
            // A successful Apply means durable, not merely visible through
            // this kernel instance. `sync_all` stays on the standard WASI
            // surface and flushes the OPFS mount after the atomic rename, so
            // an immediate browser reload observes the committed snapshot.
            std::fs::File::open(target)?.sync_all()
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
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
