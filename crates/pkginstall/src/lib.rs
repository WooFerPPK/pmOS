//! `pkginstall` library — install logic shared between the
//! `pkginstall` and `pkginstall-desktop-entry` binaries
//! (T198 / T199), and used directly by tests (T205 / T206).

use std::path::{Path, PathBuf};

/// Install a `.pmpkg.tar` bundle: validate, extract under
/// `<opt_root>/<name>/`, write `<app_dir>/<name>.desktop`.
pub fn install(
    bundle_path: &str,
    opt_root: &str,
    app_dir: &str,
    upgrade: bool,
) -> Result<String, String> {
    let bytes =
        std::fs::read(bundle_path).map_err(|e| format!("read {bundle_path}: {e}"))?;
    let manifest = pkg::validate_bundle(&bytes).map_err(|e| e.to_string())?;
    let install_dir = PathBuf::from(opt_root).join(&manifest.name);
    if install_dir.exists() && !upgrade {
        return Err(format!(
            "already installed: {} (pass --upgrade to replace)",
            install_dir.display()
        ));
    }
    if install_dir.exists() {
        std::fs::remove_dir_all(&install_dir)
            .map_err(|e| format!("remove {}: {}", install_dir.display(), e))?;
    }
    let entries = pkg::parse_tar(&bytes).map_err(|e| e.to_string())?;
    for (name, data) in &entries {
        let dst = install_dir.join(name);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
        }
        std::fs::write(&dst, data).map_err(|e| format!("write {}: {}", dst.display(), e))?;
    }
    let desktop = build_desktop_entry(&manifest, &install_dir);
    let desktop_path =
        PathBuf::from(app_dir).join(format!("{}.desktop", manifest.name));
    if let Some(parent) = desktop_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
    }
    std::fs::write(&desktop_path, desktop)
        .map_err(|e| format!("write {}: {}", desktop_path.display(), e))?;
    Ok(manifest.name)
}

/// Render a `.desktop` file body for the given manifest +
/// install directory.
pub fn build_desktop_entry(m: &pkg::Manifest, install_dir: &Path) -> String {
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
