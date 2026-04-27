//! `xtask package <crate>` — T203.
//!
//! Build a `.pmpkg.tar` for a crate's bundled wasm. The crate must
//! have:
//!  * a built `wasm32-wasip1` artefact at
//!    `target/wasm32-wasip1/release/<bin>.wasm` (or `<bin>` =
//!    crate-name; we accept both),
//!  * a `pkg.toml` next to its `Cargo.toml` describing the
//!    manifest fields (or a default-derived manifest from the crate
//!    name + version).
//!
//! Output lands at `dist/pkgs/<name>-<version>.pmpkg.tar`.

use std::fs;
use std::path::{Path, PathBuf};

pub fn run(args: &[String]) -> Result<(), String> {
    let crate_name = args
        .first()
        .ok_or_else(|| "usage: xtask package <crate>".to_string())?;
    let workspace_root = workspace_root();
    let crate_dir = workspace_root.join("crates").join(crate_name);
    if !crate_dir.exists() {
        return Err(format!("crate not found: {}", crate_dir.display()));
    }
    let manifest_text = build_manifest(crate_name, &crate_dir)?;
    let bin_name = derive_bin_name(crate_name, &crate_dir);
    let wasm_path = workspace_root
        .join("target/wasm32-wasip1/release")
        .join(format!("{bin_name}.wasm"));
    if !wasm_path.exists() {
        // Try debug profile as fallback.
        let alt = workspace_root
            .join("target/wasm32-wasip1/debug")
            .join(format!("{bin_name}.wasm"));
        if alt.exists() {
            return package_from(&manifest_text, &alt, crate_name, &workspace_root);
        }
        return Err(format!(
            "wasm artefact not built: {} (run `cargo build --release --target wasm32-wasip1 -p {}`)",
            wasm_path.display(),
            crate_name
        ));
    }
    package_from(&manifest_text, &wasm_path, crate_name, &workspace_root)
}

fn package_from(
    manifest_text: &str,
    wasm_path: &Path,
    _crate_name: &str,
    workspace_root: &Path,
) -> Result<(), String> {
    let manifest = pkg::parse_manifest(manifest_text.as_bytes())
        .map_err(|e| format!("manifest: {e}"))?;
    let wasm_bytes = fs::read(wasm_path)
        .map_err(|e| format!("read {}: {}", wasm_path.display(), e))?;
    let entries: Vec<(&str, &[u8])> = vec![
        ("manifest.toml", manifest_text.as_bytes()),
        (manifest.binary.as_str(), wasm_bytes.as_slice()),
    ];
    let tar = pkg::build_tar(&entries);
    let dist_pkgs = workspace_root.join("dist/pkgs");
    fs::create_dir_all(&dist_pkgs)
        .map_err(|e| format!("mkdir {}: {}", dist_pkgs.display(), e))?;
    let out = dist_pkgs.join(format!("{}-{}.pmpkg.tar", manifest.name, manifest.version));
    fs::write(&out, &tar).map_err(|e| format!("write {}: {}", out.display(), e))?;
    println!(
        "[xtask] package: wrote {} ({} bytes)",
        out.display(),
        tar.len()
    );
    Ok(())
}

fn build_manifest(crate_name: &str, crate_dir: &Path) -> Result<String, String> {
    // Prefer an explicit pkg.toml shipped next to Cargo.toml.
    let pkg_toml = crate_dir.join("pkg.toml");
    if pkg_toml.exists() {
        return fs::read_to_string(&pkg_toml)
            .map_err(|e| format!("read {}: {}", pkg_toml.display(), e));
    }
    // Fallback: derive a minimal manifest that satisfies the
    // required fields.
    let bin_name = derive_bin_name(crate_name, crate_dir);
    let version = workspace_version(crate_dir);
    Ok(format!(
        r#"[package]
name = "{name}"
version = "{version}"
display_name = "{title}"
author = "PMos"
summary = "Bundled {name}."

[exec]
binary = "bin/{bin}.wasm"

[capabilities]
required = ["DISPLAY_CLIENT"]
"#,
        name = crate_name,
        version = version,
        title = capitalize(crate_name),
        bin = bin_name,
    ))
}

/// Look up the bin name from `[[bin]] name = ...` in Cargo.toml,
/// else fall back to the crate name.
fn derive_bin_name(crate_name: &str, crate_dir: &Path) -> String {
    let cargo_toml = crate_dir.join("Cargo.toml");
    if let Ok(text) = fs::read_to_string(&cargo_toml) {
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("name") {
                if let Some(after_eq) = rest.split('=').nth(1) {
                    let name = after_eq.trim().trim_matches('"');
                    if !name.is_empty() && name != crate_name {
                        return name.to_string();
                    }
                }
            }
        }
    }
    crate_name.to_string()
}

fn workspace_version(crate_dir: &Path) -> String {
    let workspace_toml = crate_dir.join("../../Cargo.toml");
    if let Ok(text) = fs::read_to_string(&workspace_toml) {
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("version = ") {
                return rest.trim().trim_matches('"').to_string();
            }
        }
    }
    "0.1.0".to_string()
}

fn workspace_root() -> PathBuf {
    // `cargo run -p xtask` puts CARGO_MANIFEST_DIR at the xtask
    // crate root; the workspace root is two levels up.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .unwrap_or(manifest_dir)
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
