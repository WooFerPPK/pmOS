//! xtask assemble-dist — T026.
//!
//! Assembles `dist/` from the workspace's Rust build outputs, the
//! esbuild-produced JS bundle, and `web/index.html`. Writes the
//! `_headers` file with the COOP/COEP headers required for
//! SharedArrayBuffer availability. Writes `dist/manifest.json`
//! listing every asset so the service worker (T087) can precache it.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub fn run(_args: &[String]) -> std::result::Result<(), String> {
    run_inner().map_err(|e| format!("assemble-dist: {e}"))
}

fn run_inner() -> Result<()> {
    let repo_root = repo_root()?;
    let dist = repo_root.join("dist");
    let dist_assets = dist.join("assets");
    let dist_bin = dist_assets.join("bin");

    println!("[xtask] assemble-dist: target = {}", dist.display());

    fs::create_dir_all(&dist)?;
    fs::create_dir_all(&dist_assets)?;
    fs::create_dir_all(&dist_bin)?;

    let mut manifest_paths: Vec<String> = Vec::new();

    // 1. index.html at dist root
    copy(&repo_root.join("web/index.html"), &dist.join("index.html"), &mut manifest_paths)?;

    // 2. Bundled JS and service worker from the build/ dir (produced
    //    by `just build` via esbuild). Missing files are OK in Phase 1
    //    — the skeleton bootstrap.js is produced by esbuild as part
    //    of `just build`; if it doesn't exist, warn but continue so
    //    `cargo run -p xtask -- assemble-dist` works even before
    //    esbuild has been run.
    copy_optional(
        &repo_root.join("build/assets/bootstrap.js"),
        &dist_assets.join("bootstrap.js"),
        &mut manifest_paths,
    );
    copy_optional(
        &repo_root.join("build/sw.js"),
        &dist.join("sw.js"),
        &mut manifest_paths,
    );

    // 3. Kernel WASM (wasm32-unknown-unknown target).
    copy_optional(
        &repo_root.join("target/wasm32-unknown-unknown/release/kernel.wasm"),
        &dist_assets.join("kernel.wasm"),
        &mut manifest_paths,
    );

    // 4. Userland WASM binaries (wasm32-wasip1 target). Each lives at
    //    target/wasm32-wasip1/release/<name>.wasm. Copy the ones we
    //    know about; missing is OK before Phase 2.
    for (crate_name, bin_name) in USERLAND_BINS {
        let src = repo_root
            .join("target/wasm32-wasip1/release")
            .join(format!("{bin_name}.wasm"));
        let dst = if *crate_name == "sample-app" {
            // sample-app's binary is "hello", placed outside bin/ for clarity
            dist_assets.join(format!("{bin_name}.wasm"))
        } else {
            dist_bin.join(format!("{bin_name}.wasm"))
        };
        copy_optional(&src, &dst, &mut manifest_paths);
    }

    // 5. _headers file (Cloudflare Pages / Netlify format). The full
    //    set the demo path established: COOP/COEP for cross-origin
    //    isolation (required for SAB + Atomics.wait), plus a
    //    per-resource CORP so every asset explicitly opts into being
    //    embedded, plus Cache-Control: no-store so the dev server
    //    never serves stale bytes during iteration. Production
    //    deployments can swap Cache-Control for a long-lived immutable
    //    policy once asset fingerprints land.
    let headers = "/*\n  Cross-Origin-Opener-Policy: same-origin\n  Cross-Origin-Embedder-Policy: require-corp\n  Cross-Origin-Resource-Policy: same-origin\n  Cache-Control: no-store\n";
    fs::write(dist.join("_headers"), headers)?;
    manifest_paths.push("_headers".to_string());

    // 6. manifest.json — relative paths of every asset, for the
    //    service worker to precache (T087).
    manifest_paths.sort();
    let manifest_json = build_manifest_json(&manifest_paths);
    fs::write(dist.join("manifest.json"), manifest_json)?;

    println!(
        "[xtask] assemble-dist: wrote {} entries to dist/manifest.json",
        manifest_paths.len()
    );
    Ok(())
}

const USERLAND_BINS: &[(&str, &str)] = &[
    ("init", "init"),
    ("display-server", "display-server"),
    ("shell", "shell"),
    ("sh", "sh"),
    ("term", "term"),
    ("files", "files"),
    ("edit", "edit"),
    ("settings", "settings"),
    ("sysmon", "sysmon"),
    ("sample-app", "hello"),
    ("toolkit-free-client", "toolkit-free-client"),
    // cdylib: crate name has `-` but the compiled artefact uses `_`
    // per Rust's wasm output convention, so the filename carried
    // across in `dist/assets/bin/` stays `hello_wasi_min.wasm`.
    ("hello-wasi-min", "hello_wasi_min"),
    ("hello-wasi-spawner", "hello_wasi_spawner"),
    ("ipc-self-test", "ipc_self_test"),
    ("hello-framebuffer", "hello_framebuffer"),
];

fn copy(src: &Path, dst: &Path, manifest: &mut Vec<String>) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src, dst)
        .map_err(|e| format!("copy {} -> {}: {}", src.display(), dst.display(), e))?;
    manifest.push(dst_relative(dst));
    Ok(())
}

fn copy_optional(src: &Path, dst: &Path, manifest: &mut Vec<String>) {
    if !src.exists() {
        println!("[xtask] assemble-dist: skip (not built yet): {}", src.display());
        return;
    }
    if let Err(e) = copy(src, dst, manifest) {
        eprintln!("[xtask] assemble-dist: warning: {e}");
    }
}

fn dst_relative(dst: &Path) -> String {
    // Strip the dist/ prefix for manifest entries.
    let s = dst.to_string_lossy();
    if let Some(idx) = s.find("/dist/") {
        s[idx + "/dist/".len()..].to_string()
    } else if let Some(idx) = s.rfind("dist/") {
        s[idx + "dist/".len()..].to_string()
    } else {
        s.into_owned()
    }
}

fn build_manifest_json(paths: &[String]) -> String {
    let mut s = String::from("{\n  \"version\": 0,\n  \"assets\": [\n");
    for (i, p) in paths.iter().enumerate() {
        s.push_str("    \"");
        for c in p.chars() {
            if c == '"' || c == '\\' {
                s.push('\\');
            }
            s.push(c);
        }
        s.push('"');
        if i + 1 < paths.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str("  ]\n}\n");
    s
}

fn repo_root() -> Result<PathBuf> {
    // Walk up from CWD until we find a Cargo.toml that has [workspace].
    let start = std::env::current_dir()?;
    let mut dir: &Path = &start;
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.exists() {
            let text = fs::read_to_string(&candidate)?;
            if text.contains("[workspace]") {
                return Ok(dir.to_path_buf());
            }
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return Err("could not find workspace root".into()),
        }
    }
}
