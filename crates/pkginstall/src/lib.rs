//! `pkginstall` library — install logic shared between the
//! `pkginstall` and `pkginstall-desktop-entry` binaries
//! (T198 / T199), and used directly by tests (T205 / T206).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Install a `.pmpkg.tar` bundle: validate, extract under
/// `<opt_root>/<name>/`, write `<app_dir>/<name>.desktop`.
pub fn install(
    bundle_path: &str,
    opt_root: &str,
    app_dir: &str,
    upgrade: bool,
) -> Result<String, String> {
    let bytes = fs::read(bundle_path).map_err(|e| format!("read {bundle_path}: {e}"))?;
    install_bytes(
        &bytes,
        Path::new(opt_root),
        Path::new(app_dir),
        upgrade,
        PublishGate::Normal,
    )
}

#[derive(Clone, Copy)]
enum PublishGate {
    Normal,
    #[cfg(test)]
    FailBeforeDesktopEntry,
}

struct TransactionPaths {
    install: PathBuf,
    install_stage: PathBuf,
    install_backup: PathBuf,
    desktop: PathBuf,
    desktop_stage: PathBuf,
    desktop_backup: PathBuf,
}

impl TransactionPaths {
    fn new(opt_root: &Path, app_dir: &Path, name: &str) -> Self {
        Self {
            install: opt_root.join(name),
            install_stage: opt_root.join(format!(".{name}.pmos-stage")),
            install_backup: opt_root.join(format!(".{name}.pmos-backup")),
            desktop: app_dir.join(format!("{name}.desktop")),
            desktop_stage: app_dir.join(format!(".{name}.desktop.pmos-stage")),
            desktop_backup: app_dir.join(format!(".{name}.desktop.pmos-backup")),
        }
    }

    fn ensure_transaction_paths_available(&self) -> Result<(), String> {
        for path in [
            &self.install_stage,
            &self.install_backup,
            &self.desktop_stage,
            &self.desktop_backup,
        ] {
            if path.exists() {
                return Err(format!(
                    "stale install transaction at {}; remove it before retrying",
                    path.display()
                ));
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct PublishState {
    old_install_backed_up: bool,
    new_install_published: bool,
    old_desktop_backed_up: bool,
    new_desktop_published: bool,
}

fn install_bytes(
    bytes: &[u8],
    opt_root: &Path,
    app_dir: &Path,
    upgrade: bool,
    gate: PublishGate,
) -> Result<String, String> {
    let manifest = pkg::validate_bundle(bytes).map_err(|e| e.to_string())?;
    pkg::validate_install_capabilities(&manifest).map_err(|error| error.to_string())?;
    let entries = pkg::parse_tar(bytes).map_err(|e| e.to_string())?;
    let paths = TransactionPaths::new(opt_root, app_dir, &manifest.name);

    if paths.install.exists() && !upgrade {
        return Err(format!(
            "already installed: {} (pass --upgrade to replace)",
            paths.install.display()
        ));
    }
    paths.ensure_transaction_paths_available()?;
    fs::create_dir_all(opt_root)
        .map_err(|error| format!("mkdir {}: {error}", opt_root.display()))?;
    fs::create_dir_all(app_dir).map_err(|error| format!("mkdir {}: {error}", app_dir.display()))?;

    fs::create_dir(&paths.install_stage)
        .map_err(|error| format!("mkdir {}: {error}", paths.install_stage.display()))?;
    let prepared = prepare_install_stage(&paths.install_stage, &entries, &manifest.binary)
        .and_then(|()| {
            let desktop = build_desktop_entry(&manifest, &paths.install);
            write_file_synced(&paths.desktop_stage, desktop.as_bytes())
        });
    if let Err(error) = prepared {
        let cleanup =
            cleanup_path(&paths.install_stage).and_then(|()| cleanup_path(&paths.desktop_stage));
        return Err(with_cleanup_error(error, cleanup));
    }

    let mut state = PublishState::default();
    let published = (|| {
        if paths.install.exists() {
            fs::rename(&paths.install, &paths.install_backup).map_err(|error| {
                format!(
                    "backup {} to {}: {error}",
                    paths.install.display(),
                    paths.install_backup.display()
                )
            })?;
            state.old_install_backed_up = true;
        }
        fs::rename(&paths.install_stage, &paths.install).map_err(|error| {
            format!(
                "publish {} to {}: {error}",
                paths.install_stage.display(),
                paths.install.display()
            )
        })?;
        state.new_install_published = true;

        if paths.desktop.exists() {
            fs::rename(&paths.desktop, &paths.desktop_backup).map_err(|error| {
                format!(
                    "backup {} to {}: {error}",
                    paths.desktop.display(),
                    paths.desktop_backup.display()
                )
            })?;
            state.old_desktop_backed_up = true;
        }

        #[cfg(test)]
        if matches!(gate, PublishGate::FailBeforeDesktopEntry) {
            return Err("injected failure before desktop entry publication".to_string());
        }
        #[cfg(not(test))]
        let _ = gate;

        fs::rename(&paths.desktop_stage, &paths.desktop).map_err(|error| {
            format!(
                "publish {} to {}: {error}",
                paths.desktop_stage.display(),
                paths.desktop.display()
            )
        })?;
        state.new_desktop_published = true;
        Ok(())
    })();

    if let Err(error) = published {
        return Err(with_cleanup_error(error, rollback(&paths, &state)));
    }

    cleanup_path(&paths.install_backup)?;
    cleanup_path(&paths.desktop_backup)?;
    Ok(manifest.name)
}

fn prepare_install_stage(
    stage: &Path,
    entries: &[(String, Vec<u8>)],
    executable: &str,
) -> Result<(), String> {
    for (name, data) in entries {
        let destination = stage.join(name);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
        }
        write_file_synced(&destination, data)?;
        set_file_mode(&destination, if name == executable { 0o755 } else { 0o644 })?;
    }
    Ok(())
}

fn write_file_synced(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))
}

#[cfg(target_arch = "wasm32")]
fn set_file_mode(path: &Path, mode: u32) -> Result<(), String> {
    #[link(wasm_import_module = "pmos_ext")]
    extern "C" {
        fn fs_chmod(path_ptr: *const u8, path_len: i32, mode: i32) -> i32;
    }

    let path = path
        .to_str()
        .ok_or_else(|| "installed path is not UTF-8".to_string())?;
    let path_len = i32::try_from(path.len()).map_err(|_| "installed path is too long")?;
    let result = unsafe { fs_chmod(path.as_ptr(), path_len, mode as i32) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!("chmod {path} to {mode:#05o}: errno {}", -result))
    }
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("chmod {} to {mode:#05o}: {error}", path.display()))
}

#[cfg(not(any(target_arch = "wasm32", unix)))]
fn set_file_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Err("pkginstall requires filesystem mode support on this target".to_string())
}

fn rollback(paths: &TransactionPaths, state: &PublishState) -> Result<(), String> {
    let mut failures = Vec::new();
    if state.new_desktop_published {
        collect_cleanup_error(&mut failures, cleanup_path(&paths.desktop));
    } else {
        collect_cleanup_error(&mut failures, cleanup_path(&paths.desktop_stage));
    }
    if state.old_desktop_backed_up {
        collect_cleanup_error(
            &mut failures,
            fs::rename(&paths.desktop_backup, &paths.desktop)
                .map_err(|error| format!("restore {}: {error}", paths.desktop.display())),
        );
    }

    if state.new_install_published {
        collect_cleanup_error(&mut failures, cleanup_path(&paths.install));
    } else {
        collect_cleanup_error(&mut failures, cleanup_path(&paths.install_stage));
    }
    if state.old_install_backed_up {
        collect_cleanup_error(
            &mut failures,
            fs::rename(&paths.install_backup, &paths.install)
                .map_err(|error| format!("restore {}: {error}", paths.install.display())),
        );
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn cleanup_path(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("inspect {}: {error}", path.display())),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(|error| format!("remove {}: {error}", path.display()))
    } else {
        fs::remove_file(path).map_err(|error| format!("remove {}: {error}", path.display()))
    }
}

fn collect_cleanup_error(failures: &mut Vec<String>, result: Result<(), String>) {
    if let Err(error) = result {
        failures.push(error);
    }
}

fn with_cleanup_error(error: String, cleanup: Result<(), String>) -> String {
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => format!("{error}; rollback failed: {cleanup_error}"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "pkginstall-publish-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&directory).expect("create tempdir");
        directory
    }

    fn bundle(version: &str, display_name: &str, wasm: &[u8]) -> Vec<u8> {
        let source = format!(
            r#"[package]
name = "hello"
version = "{version}"
display_name = "{display_name}"
author = "PMos"
summary = "Transactional fixture."

[exec]
binary = "bin/hello.wasm"

[capabilities]
required = ["DISPLAY_CLIENT"]
"#
        );
        let manifest = format!(
            "{source}\n[integrity]\nsha256 = {{ \"bin/hello.wasm\" = \"{}\" }}\n",
            pkg::sha256_hex(wasm)
        );
        pkg::build_tar(&[
            ("manifest.toml", manifest.as_bytes()),
            ("bin/hello.wasm", wasm),
        ])
    }

    #[test]
    fn publish_failure_restores_previous_binary_and_desktop_entry() {
        let root = tempdir();
        let opt_root = root.join("opt");
        let app_dir = root.join("apps");
        let old = bundle("1.0.0", "Old Hello", b"\0asm\x01\0\0\0");
        install_bytes(&old, &opt_root, &app_dir, false, PublishGate::Normal)
            .expect("initial install");
        let binary = opt_root.join("hello/bin/hello.wasm");
        let desktop = app_dir.join("hello.desktop");
        let old_binary = fs::read(&binary).expect("old binary");
        let old_desktop = fs::read(&desktop).expect("old desktop");

        let upgrade = bundle("2.0.0", "New Hello", b"\0asm\x02\0\0\0");
        let error = install_bytes(
            &upgrade,
            &opt_root,
            &app_dir,
            true,
            PublishGate::FailBeforeDesktopEntry,
        )
        .expect_err("injected publication failure");
        assert!(error.contains("injected failure"), "{error}");
        assert_eq!(fs::read(&binary).expect("restored binary"), old_binary);
        assert_eq!(fs::read(&desktop).expect("restored desktop"), old_desktop);
        assert!(!opt_root.join(".hello.pmos-stage").exists());
        assert!(!opt_root.join(".hello.pmos-backup").exists());
        assert!(!app_dir.join(".hello.desktop.pmos-stage").exists());
        assert!(!app_dir.join(".hello.desktop.pmos-backup").exists());

        let _ = fs::remove_dir_all(root);
    }
}
