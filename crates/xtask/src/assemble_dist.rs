//! xtask assemble-dist — T026.
//!
//! Assembles `dist/` from the workspace's Rust build outputs, the
//! esbuild-produced JS bundle, and `web/index.html`. The complete output is
//! built in a clean staging directory, validated against its manifest, and
//! only then published over `dist/`.

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const REQUIRED_REPO_ARTIFACTS: &[(&str, &str)] = &[
    ("web/index.html", "index.html"),
    ("build/assets/bootstrap.js", "assets/bootstrap.js"),
    ("build/assets/kernel-worker.js", "assets/kernel-worker.js"),
    ("build/assets/user-worker.js", "assets/user-worker.js"),
    ("build/sw.js", "sw.js"),
];

const STATIC_DOCS: &[&str] = &["LICENSE.txt", "CREDITS.txt"];

pub fn run(_args: &[String]) -> std::result::Result<(), String> {
    run_inner().map_err(|e| format!("assemble-dist: {e}"))
}

fn run_inner() -> Result<()> {
    let repo_root = repo_root()?;
    let target_dir = crate::cargo_target::resolve(&repo_root)?;
    assemble_at(&repo_root, &target_dir)
}

fn assemble_at(repo_root: &Path, target_dir: &Path) -> Result<()> {
    let dist = repo_root.join("dist");
    let staging = repo_root.join("build/dist-staging");
    let backup = repo_root.join("build/dist-backup");

    println!("[xtask] assemble-dist: target = {}", dist.display());
    println!(
        "[xtask] assemble-dist: Cargo artifacts = {}",
        target_dir.display()
    );

    recover_interrupted_publish(&dist, &backup)?;
    remove_path(&staging)?;
    fs::create_dir_all(&staging)?;

    let assembled = assemble_staging(repo_root, target_dir, &staging);
    let entry_count = match assembled {
        Ok(count) => count,
        Err(error) => {
            let _ = remove_path(&staging);
            return Err(error);
        }
    };

    if let Err(error) = publish_staged_dist(&staging, &dist, &backup) {
        let _ = remove_path(&staging);
        return Err(error);
    }
    println!("[xtask] assemble-dist: wrote {entry_count} entries to dist/manifest.json");
    Ok(())
}

fn assemble_staging(repo_root: &Path, target_dir: &Path, staging: &Path) -> Result<usize> {
    let mut manifest_paths = Vec::new();

    for (source, destination) in REQUIRED_REPO_ARTIFACTS {
        copy_required(
            &repo_root.join(source),
            &staging.join(destination),
            staging,
            &mut manifest_paths,
        )?;
    }

    copy_required(
        &target_dir.join("wasm32-unknown-unknown/release/kernel.wasm"),
        &staging.join("assets/kernel.wasm"),
        staging,
        &mut manifest_paths,
    )?;

    for (crate_name, bin_name) in USERLAND_BINS {
        let source = target_dir
            .join("wasm32-wasip1/release")
            .join(format!("{bin_name}.wasm"));
        let destination = if *crate_name == "sample-app" {
            staging.join("assets").join(format!("{bin_name}.wasm"))
        } else {
            staging.join("assets/bin").join(format!("{bin_name}.wasm"))
        };
        copy_required(&source, &destination, staging, &mut manifest_paths)?;
    }

    let sample_package = crate::package::build_bundle_at(repo_root, target_dir, "sample-app")
        .map_err(std::io::Error::other)?;
    if sample_package.bytes.len() > abi::ext::host_file::MAX_IMPORT_BYTES {
        return Err(format!(
            "sample package {} is {} bytes; host import v1 accepts at most {}",
            sample_package.filename,
            sample_package.bytes.len(),
            abi::ext::host_file::MAX_IMPORT_BYTES,
        )
        .into());
    }
    let sample_package_path = staging.join("pkgs/staging").join(&sample_package.filename);
    if let Some(parent) = sample_package_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&sample_package_path, &sample_package.bytes)?;
    record_output(staging, &sample_package_path, &mut manifest_paths)?;

    for doc in STATIC_DOCS {
        copy_required(
            &repo_root.join("crates/kernel/assets").join(doc),
            &staging.join("assets").join(doc),
            staging,
            &mut manifest_paths,
        )?;
    }

    let headers = "/*\n  Cross-Origin-Opener-Policy: same-origin\n  Cross-Origin-Embedder-Policy: require-corp\n  Cross-Origin-Resource-Policy: same-origin\n  Cache-Control: no-store\n";
    let headers_path = staging.join("_headers");
    fs::write(&headers_path, headers)?;
    record_output(staging, &headers_path, &mut manifest_paths)?;

    manifest_paths.sort();
    let manifest_json = build_manifest_json(staging, &manifest_paths)?;
    fs::write(staging.join("manifest.json"), manifest_json)?;
    validate_manifest(staging, &manifest_paths)?;

    Ok(manifest_paths.len())
}

const USERLAND_BINS: &[(&str, &str)] = &[
    ("init", "init"),
    ("init", "init-desktop"),
    ("display-server", "display-server"),
    ("display-client-demo", "display-client-demo"),
    ("shell", "shell"),
    ("alt-shell", "alt-shell"),
    ("sh", "sh"),
    ("term", "term"),
    ("files", "files"),
    ("edit", "edit"),
    ("settings", "settings"),
    ("sysmon", "sysmon"),
    ("pkginstall", "pkginstall"),
    ("pkginstall", "pkginstall-desktop-entry"),
    ("sample-app", "hello"),
    ("toolkit-free-client", "toolkit-free-client"),
    ("hello-wasi-min", "hello_wasi_min"),
    ("hello-wasi-spawner", "hello_wasi_spawner"),
    ("ipc-self-test", "ipc_self_test"),
    ("hello-framebuffer", "hello_framebuffer"),
    ("display-server-lite", "display_server_lite"),
    ("hello-wasi-bootstrap", "hello_wasi_bootstrap"),
    ("hello-fb-blit", "hello_fb_blit"),
    ("hello-input-echo", "hello_input_echo"),
    ("hello-sigchld", "hello_sigchld"),
    ("hello-kill-probe", "hello_kill_probe"),
    ("hello-pid", "hello_pid"),
    ("hello-self-probe", "hello_self_probe"),
    ("hello-self-kill", "hello_self_kill"),
    ("hello-ppid", "hello_ppid"),
    ("hello-caps", "hello_caps"),
    ("hello-raise", "hello_raise"),
    ("hello-wait-noop", "hello_wait_noop"),
    ("hello-cap-check", "hello_cap_check"),
    ("hello-random", "hello_random"),
    ("hello-fd-close-bad", "hello_fd_close_bad"),
    ("hello-fd-close-good", "hello_fd_close_good"),
    ("hello-yield-loop", "hello_yield_loop"),
    ("hello-cap-list", "hello_cap_list"),
    ("mem-adversary", "mem_adversary"),
    ("hello-std", "hello-std"),
    ("hello-clock", "hello-clock"),
    ("hello-toplevel", "hello-toplevel"),
    ("hello-trap", "hello_trap"),
    ("coreutils", "cat"),
    ("coreutils", "grep"),
    ("coreutils", "cp"),
    ("coreutils", "mkdir"),
    ("coreutils", "rm"),
    ("coreutils", "mv"),
    ("coreutils", "ls"),
    ("coreutils", "wc"),
    ("coreutils", "head"),
    ("coreutils", "tail"),
    ("coreutils", "sort"),
    ("coreutils", "uniq"),
    ("coreutils", "tr"),
    ("coreutils", "tee"),
];

fn copy_required(
    source: &Path,
    destination: &Path,
    output_root: &Path,
    manifest: &mut Vec<String>,
) -> Result<()> {
    if !source.is_file() {
        return Err(format!("required artifact missing: {}", source.display()).into());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination).map_err(|error| {
        format!(
            "copy {} -> {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    record_output(output_root, destination, manifest)
}

fn record_output(output_root: &Path, path: &Path, manifest: &mut Vec<String>) -> Result<()> {
    manifest.push(relative_output_path(output_root, path)?);
    Ok(())
}

fn relative_output_path(output_root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(output_root).map_err(|_| {
        format!(
            "output {} is outside staging root {}",
            path.display(),
            output_root.display()
        )
    })?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| format!("non-UTF-8 output path: {}", path.display()))?,
            ),
            _ => return Err(format!("invalid output path: {}", path.display()).into()),
        }
    }
    Ok(parts.join("/"))
}

fn validate_manifest(output_root: &Path, manifest_paths: &[String]) -> Result<()> {
    if !manifest_paths.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err("manifest paths must be sorted and unique".into());
    }

    let expected: BTreeSet<String> = manifest_paths.iter().cloned().collect();
    let mut actual_paths = collect_output_files(output_root)?;
    if !actual_paths.remove("manifest.json") {
        return Err("staged output is missing manifest.json".into());
    }

    if actual_paths != expected {
        let missing = expected
            .difference(&actual_paths)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let unlisted = actual_paths
            .difference(&expected)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "manifest/output mismatch (missing: [{missing}]; unlisted: [{unlisted}])"
        )
        .into());
    }

    let actual_manifest = fs::read_to_string(output_root.join("manifest.json"))?;
    let expected_manifest = build_manifest_json(output_root, manifest_paths)?;
    if actual_manifest != expected_manifest {
        return Err("manifest.json does not match the validated output inventory".into());
    }
    Ok(())
}

fn collect_output_files(output_root: &Path) -> Result<BTreeSet<String>> {
    if !output_root.is_dir() {
        return Err(format!("output directory missing: {}", output_root.display()).into());
    }

    fn visit(root: &Path, directory: &Path, files: &mut BTreeSet<String>) -> Result<()> {
        let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() {
                visit(root, &path, files)?;
            } else if file_type.is_file() {
                files.insert(relative_output_path(root, &path)?);
            } else {
                return Err(format!("unsupported output entry: {}", path.display()).into());
            }
        }
        Ok(())
    }

    let mut files = BTreeSet::new();
    visit(output_root, output_root, &mut files)?;
    Ok(files)
}

fn recover_interrupted_publish(dist: &Path, backup: &Path) -> Result<()> {
    let dist_exists = path_exists(dist)?;
    let backup_exists = path_exists(backup)?;
    match (dist_exists, backup_exists) {
        (false, true) => fs::rename(backup, dist).map_err(|error| {
            format!(
                "restore interrupted distribution {} -> {}: {error}",
                backup.display(),
                dist.display()
            )
            .into()
        }),
        (true, true) => remove_path(backup),
        _ => Ok(()),
    }
}

fn publish_staged_dist(staging: &Path, dist: &Path, backup: &Path) -> Result<()> {
    let had_dist = path_exists(dist)?;
    if had_dist {
        fs::rename(dist, backup).map_err(|error| {
            format!(
                "move existing distribution {} -> {}: {error}",
                dist.display(),
                backup.display()
            )
        })?;
    }

    if let Err(publish_error) = fs::rename(staging, dist) {
        if had_dist {
            if let Err(restore_error) = fs::rename(backup, dist) {
                return Err(format!(
                    "publish staged distribution: {publish_error}; restore previous distribution: {restore_error}"
                )
                .into());
            }
        }
        return Err(format!("publish staged distribution: {publish_error}").into());
    }

    if had_dist {
        remove_path(backup)?;
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn remove_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn build_manifest_json(output_root: &Path, paths: &[String]) -> Result<String> {
    let mut hashes = Vec::with_capacity(paths.len());
    let mut release_hasher = Sha256::new();
    for path in paths {
        let bytes = fs::read(output_root.join(path))?;
        let hash = hex_digest(Sha256::digest(&bytes));
        release_hasher.update(path.as_bytes());
        release_hasher.update([0]);
        release_hasher.update(hash.as_bytes());
        release_hasher.update(b"\n");
        hashes.push(hash);
    }
    let release = hex_digest(release_hasher.finalize());
    let asset_paths = paths
        .iter()
        .filter(|path| path.as_str() != "_headers")
        .collect::<Vec<_>>();
    let deployment_paths = paths
        .iter()
        .filter(|path| path.as_str() == "_headers")
        .collect::<Vec<_>>();

    let mut output = String::from("{\n  \"version\": 40,\n  \"release\": \"");
    output.push_str(&release);
    output.push_str("\",\n  \"assets\": [\n");
    for (index, path) in asset_paths.iter().enumerate() {
        output.push_str("    ");
        push_json_string(&mut output, path);
        if index + 1 < asset_paths.len() {
            output.push(',');
        }
        output.push('\n');
    }
    output.push_str("  ],\n  \"deployment\": [\n");
    for (index, path) in deployment_paths.iter().enumerate() {
        output.push_str("    ");
        push_json_string(&mut output, path);
        if index + 1 < deployment_paths.len() {
            output.push(',');
        }
        output.push('\n');
    }
    output.push_str("  ],\n  \"integrity\": {\n");
    for (index, (path, hash)) in paths.iter().zip(hashes.iter()).enumerate() {
        output.push_str("    ");
        push_json_string(&mut output, path);
        output.push_str(": \"");
        output.push_str(hash);
        output.push('"');
        if index + 1 < paths.len() {
            output.push(',');
        }
        output.push('\n');
    }
    output.push_str("  }\n}\n");
    Ok(output)
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{001f}' => {
                output.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

fn repo_root() -> Result<PathBuf> {
    let start = std::env::current_dir()?;
    let mut directory: &Path = &start;
    loop {
        let candidate = directory.join("Cargo.toml");
        if candidate.exists() {
            let text = fs::read_to_string(&candidate)?;
            if text.contains("[workspace]") {
                return Ok(directory.to_path_buf());
            }
        }
        match directory.parent() {
            Some(parent) => directory = parent,
            None => return Err("could not find workspace root".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestRepo {
        root: PathBuf,
    }

    impl TestRepo {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "pmos-xtask-assemble-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("create test repository");
            Self { root }
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent");
        }
        fs::write(path, bytes).expect("write fixture");
    }

    fn write_unless_omitted(path: &Path, bytes: &[u8], omitted: Option<&Path>) {
        if omitted != Some(path) {
            write_file(path, bytes);
        }
    }

    fn seed_required_inputs(
        root: &Path,
        target_dir: &Path,
        wasm_bytes: &[u8],
        omitted: Option<&Path>,
    ) {
        for (source, _) in REQUIRED_REPO_ARTIFACTS {
            let source = root.join(source);
            write_unless_omitted(&source, b"required", omitted);
        }

        let kernel = target_dir.join("wasm32-unknown-unknown/release/kernel.wasm");
        write_unless_omitted(&kernel, wasm_bytes, omitted);
        for (_, bin_name) in USERLAND_BINS {
            let source = target_dir
                .join("wasm32-wasip1/release")
                .join(format!("{bin_name}.wasm"));
            write_unless_omitted(&source, wasm_bytes, omitted);
        }

        write_file(
            &root.join("crates/sample-app/pkg.toml"),
            br#"[package]
name = "hello"
version = "0.1.0"
display_name = "Hello"
author = "PMos"
summary = "Fixture."

[exec]
binary = "bin/hello.wasm"

[capabilities]
required = ["DISPLAY_CLIENT"]
"#,
        );
        write_file(
            &root.join("crates/sample-app/Cargo.toml"),
            br#"[package]
name = "sample-app"

[[bin]]
name = "hello"
path = "src/main.rs"
"#,
        );

        for doc in STATIC_DOCS {
            write_file(&root.join("crates/kernel/assets").join(doc), b"document");
        }
    }

    #[test]
    fn missing_required_artifact_preserves_existing_dist() {
        for relative_missing in [
            "build/assets/bootstrap.js",
            "target/wasm32-unknown-unknown/release/kernel.wasm",
            "target/wasm32-wasip1/release/alt-shell.wasm",
            "target/wasm32-wasip1/release/toolkit-free-client.wasm",
            "target/wasm32-wasip1/release/cat.wasm",
        ] {
            let repo = TestRepo::new();
            let target_dir = repo.root.join("target");
            let missing = repo.root.join(relative_missing);
            seed_required_inputs(&repo.root, &target_dir, b"\0asmfixture", Some(&missing));
            write_file(&repo.root.join("dist/keep.txt"), b"previous release");

            let error = assemble_at(&repo.root, &target_dir)
                .expect_err("assembly must reject missing input");

            assert!(
                error.to_string().contains("required artifact missing: "),
                "unexpected error: {error}"
            );
            assert!(
                error.to_string().contains(&missing.display().to_string()),
                "error did not identify {}: {error}",
                missing.display()
            );
            assert_eq!(
                fs::read(repo.root.join("dist/keep.txt")).expect("read previous release"),
                b"previous release"
            );
            assert!(!repo.root.join("build/dist-staging").exists());
        }
    }

    #[test]
    fn successful_assembly_replaces_stale_dist_and_matches_manifest() {
        let repo = TestRepo::new();
        let target_dir = repo.root.join("target");
        seed_required_inputs(&repo.root, &target_dir, b"\0asmfixture", None);
        write_file(&repo.root.join("dist/stale.txt"), b"stale");

        assemble_at(&repo.root, &target_dir).expect("assemble complete distribution");

        let dist = repo.root.join("dist");
        assert!(!dist.join("stale.txt").exists());
        assert!(!repo.root.join("build/dist-staging").exists());
        assert!(!repo.root.join("build/dist-backup").exists());
        assert!(dist.join("assets/bin/alt-shell.wasm").is_file());
        assert!(dist.join("assets/bin/toolkit-free-client.wasm").is_file());
        // Wallpapers, fonts, keymaps, and zoneinfo are compile-embedded and
        // seeded into the guest VFS by the kernel. Shipping a second static
        // HTTP tree adds megabytes to every cold/offline cache without a
        // runtime consumer.
        for duplicate in ["wallpapers", "fonts", "keymaps", "zoneinfo"] {
            assert!(!dist.join("assets").join(duplicate).exists());
        }
        let sample_package = dist.join("pkgs/staging/hello-0.1.0.pmpkg.tar");
        assert!(sample_package.is_file());
        let sample_bytes = fs::read(sample_package).expect("read staged sample package");
        assert!(sample_bytes.len() <= abi::ext::host_file::MAX_IMPORT_BYTES);
        let sample_manifest =
            pkg::validate_bundle(&sample_bytes).expect("validate staged sample package");
        assert_eq!(
            sample_manifest.integrity_sha256.keys().collect::<Vec<_>>(),
            ["bin/hello.wasm"]
        );
        for (_, bin_name) in USERLAND_BINS
            .iter()
            .filter(|(crate_name, _)| *crate_name == "coreutils")
        {
            assert!(
                dist.join("assets/bin")
                    .join(format!("{bin_name}.wasm"))
                    .is_file(),
                "missing coreutils binary {bin_name}"
            );
        }

        let mut output_files = collect_output_files(&dist).expect("inventory output");
        assert!(output_files.remove("manifest.json"));
        let manifest_paths = output_files.into_iter().collect::<Vec<_>>();
        let manifest = fs::read_to_string(dist.join("manifest.json")).expect("read manifest");
        assert_eq!(
            manifest,
            build_manifest_json(&dist, &manifest_paths).expect("build expected manifest")
        );
        validate_manifest(&dist, &manifest_paths).expect("validate manifest");
    }

    #[test]
    fn alternate_target_is_authoritative_for_dist_and_sample_package() {
        const STALE_WASM: &[u8] = b"\0asmstale-default";
        const CURRENT_WASM: &[u8] = b"\0asmcurrent-alternate";

        let repo = TestRepo::new();
        let default_target = repo.root.join("target");
        let alternate_target = repo.root.join("alternate-target");
        seed_required_inputs(&repo.root, &default_target, STALE_WASM, None);
        seed_required_inputs(&repo.root, &alternate_target, CURRENT_WASM, None);

        assemble_at(&repo.root, &alternate_target).expect("assemble from alternate target");

        let dist = repo.root.join("dist");
        for output in [
            "assets/kernel.wasm",
            "assets/bin/shell.wasm",
            "assets/hello.wasm",
        ] {
            assert_eq!(
                fs::read(dist.join(output)).expect("read assembled WASM"),
                CURRENT_WASM,
                "{output} came from the default Cargo target directory"
            );
        }
        let package =
            fs::read(dist.join("pkgs/staging/hello-0.1.0.pmpkg.tar")).expect("read sample package");
        let packaged_wasm = pkg::parse_tar(&package)
            .expect("parse sample package")
            .into_iter()
            .find_map(|(path, bytes)| (path == "bin/hello.wasm").then_some(bytes))
            .expect("sample package binary");
        assert_eq!(packaged_wasm, CURRENT_WASM);
        assert_ne!(packaged_wasm, STALE_WASM);
    }

    #[test]
    fn alternate_target_missing_artifact_never_falls_back_to_stale_default() {
        let repo = TestRepo::new();
        let default_target = repo.root.join("target");
        let alternate_target = repo.root.join("alternate-target");
        let missing = alternate_target.join("wasm32-wasip1/release/shell.wasm");
        seed_required_inputs(&repo.root, &default_target, b"\0asmstale-default", None);
        seed_required_inputs(
            &repo.root,
            &alternate_target,
            b"\0asmcurrent-alternate",
            Some(&missing),
        );
        write_file(&repo.root.join("dist/keep.txt"), b"previous release");

        let error = assemble_at(&repo.root, &alternate_target)
            .expect_err("missing alternate artifact must fail closed");

        assert!(error.to_string().contains(&missing.display().to_string()));
        assert_eq!(
            fs::read(repo.root.join("dist/keep.txt")).expect("read previous release"),
            b"previous release"
        );
        assert!(!repo.root.join("build/dist-staging").exists());
    }

    #[test]
    fn manifest_validation_rejects_unlisted_output() {
        let repo = TestRepo::new();
        let output = repo.root.join("stage");
        write_file(&output.join("extra.txt"), b"not declared");
        write_file(
            &output.join("manifest.json"),
            build_manifest_json(&output, &[])
                .expect("build fixture manifest")
                .as_bytes(),
        );

        let error = validate_manifest(&output, &[]).expect_err("extra file must fail validation");

        assert!(error.to_string().contains("unlisted: [extra.txt]"));
    }

    #[test]
    fn manifest_release_and_integrity_change_with_asset_bytes() {
        let repo = TestRepo::new();
        let output = repo.root.join("stage");
        write_file(&output.join("asset.bin"), b"first");
        let paths = vec!["asset.bin".to_string()];
        let first = build_manifest_json(&output, &paths).expect("build first manifest");

        write_file(&output.join("asset.bin"), b"second");
        let second = build_manifest_json(&output, &paths).expect("build second manifest");

        assert_ne!(first, second);
        assert!(first.contains("\"release\": \""));
        assert!(first.contains("\"integrity\":"));
    }
}
