//! `pkginstall-desktop-entry` (T199) — write a `.desktop` file
//! from an already-extracted bundle's `manifest.toml`.
//!
//! Used when a user has unpacked a bundle by hand
//! (e.g. `tar -xf foo.pmpkg.tar -C /opt/foo`) and just needs
//! the launcher integration to land.

use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_APP_DIR: &str = "/usr/share/applications";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut manifest_path: Option<String> = None;
    let mut app_dir = DEFAULT_APP_DIR.to_string();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--app-dir" => {
                app_dir = iter.next().unwrap_or_else(|| {
                    eprintln!("pkginstall-desktop-entry: --app-dir requires a value");
                    std::process::exit(2);
                });
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: pkginstall-desktop-entry [--app-dir DIR] <path/to/manifest.toml>"
                );
                return ExitCode::SUCCESS;
            }
            other if !other.starts_with('-') && manifest_path.is_none() => {
                manifest_path = Some(other.to_string());
            }
            other => {
                eprintln!("pkginstall-desktop-entry: unrecognised arg {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(manifest_path) = manifest_path else {
        eprintln!(
            "usage: pkginstall-desktop-entry [--app-dir DIR] <path/to/manifest.toml>"
        );
        return ExitCode::from(2);
    };
    match run(&manifest_path, &app_dir) {
        Ok(name) => {
            println!("pkginstall-desktop-entry: wrote entry for {name}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("pkginstall-desktop-entry: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(manifest_path: &str, app_dir: &str) -> Result<String, String> {
    let bytes = std::fs::read(manifest_path)
        .map_err(|e| format!("read {manifest_path}: {e}"))?;
    let manifest = pkg::parse_manifest(&bytes).map_err(|e| e.to_string())?;
    let install_dir = PathBuf::from(manifest_path)
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let desktop = build_entry(&manifest, &install_dir);
    let dest = PathBuf::from(app_dir).join(format!("{}.desktop", manifest.name));
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
    }
    std::fs::write(&dest, desktop).map_err(|e| format!("write {}: {}", dest.display(), e))?;
    Ok(manifest.name)
}

fn build_entry(m: &pkg::Manifest, install_dir: &std::path::Path) -> String {
    let mut s = String::new();
    s.push_str("[Desktop Entry]\n");
    s.push_str("Type=Application\n");
    s.push_str(&format!("Name={}\n", m.display_name));
    s.push_str(&format!("Exec={}\n", install_dir.join(&m.binary).display()));
    if let Some(icon) = &m.icon {
        s.push_str(&format!("Icon={}\n", install_dir.join(icon).display()));
    }
    s.push_str(&format!("Summary={}\n", m.summary));
    if !m.mime_types.is_empty() {
        s.push_str(&format!("MimeType={};\n", m.mime_types.join(";")));
    }
    if !m.categories.is_empty() {
        s.push_str(&format!("Categories={};\n", m.categories.join(";")));
    }
    s.push_str(&format!("X-PMos-Caps={}\n", m.caps_required.join(";")));
    s
}
