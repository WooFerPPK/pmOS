//! `xtask push-sample` — T204.
//!
//! After `xtask package sample-app` lands a bundle at
//! `dist/pkgs/hello-0.1.0.pmpkg.tar`, this subcommand stages it
//! into the location the test harness reads at boot (so that
//! Playwright can drive the install flow without any out-of-band
//! data transfer).
//!
//! v1 staging path: `dist/pkgs/staging/hello-0.1.0.pmpkg.tar`. The
//! Playwright fixture (T207) reads this byte-for-byte and feeds
//! it into the page via `setInputFiles` against the file-manager's
//! drag-drop input — the same path a human would use.

use std::fs;
use std::path::PathBuf;

pub fn run(_args: &[String]) -> Result<(), String> {
    let workspace = workspace_root();
    let pkgs = workspace.join("dist/pkgs");
    let staging = pkgs.join("staging");
    fs::create_dir_all(&staging)
        .map_err(|e| format!("mkdir {}: {}", staging.display(), e))?;
    let mut copied = 0u32;
    for entry in fs::read_dir(&pkgs)
        .map_err(|e| format!("read_dir {}: {}", pkgs.display(), e))?
    {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path
            .extension()
            .and_then(|s| s.to_str())
            == Some("tar")
            && path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|n| n.ends_with(".pmpkg.tar"))
                .unwrap_or(false)
        {
            let dest = staging.join(path.file_name().unwrap());
            fs::copy(&path, &dest).map_err(|e| {
                format!("cp {} -> {}: {}", path.display(), dest.display(), e)
            })?;
            copied += 1;
            println!("[xtask] push-sample: staged {}", dest.display());
        }
    }
    if copied == 0 {
        return Err(format!(
            "no .pmpkg.tar bundles in {} — run `xtask package sample-app` first",
            pkgs.display()
        ));
    }
    Ok(())
}

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .map(PathBuf::from)
        .unwrap_or(manifest_dir)
}
