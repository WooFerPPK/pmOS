//! T205: pkginstall fixture-bundle integration tests.
//!
//! Build a real `.pmpkg.tar` in-memory, drop it into a tempdir,
//! run the install logic, and assert: bundle is extracted under
//! `<opt-root>/<name>/`, desktop entry is written, and the
//! desktop entry has the expected fields.

use pkg::{build_tar, sha256_hex};

fn manifest_toml() -> &'static str {
    r#"
[package]
name = "hello"
version = "0.1.0"
display_name = "Hello"
author = "PMos"
summary = "Sample app."

[exec]
binary = "bin/hello.wasm"

[ui]
mime_types = ["application/x-hello"]
categories = ["Utility"]

[capabilities]
required = ["DISPLAY_CLIENT"]
"#
}

fn packaged_manifest(source: &str, wasm: &[u8]) -> String {
    format!(
        "{}\n[integrity]\nsha256 = {{ \"bin/hello.wasm\" = \"{}\" }}\n",
        source,
        sha256_hex(wasm)
    )
}

fn bundle(source: &str, wasm: &[u8]) -> Vec<u8> {
    let manifest = packaged_manifest(source, wasm);
    build_tar(&[
        ("manifest.toml", manifest.as_bytes()),
        ("bin/hello.wasm", wasm),
    ])
}

fn write_tempdir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn install_extracts_bundle_and_writes_desktop_entry() {
    let bundle = bundle(manifest_toml(), b"\0asm\x01\0\0\0");
    let tmp = write_tempdir("pkginstall-test");
    let bundle_path = tmp.join("hello-0.1.0.pmpkg.tar");
    std::fs::write(&bundle_path, &bundle).unwrap();
    let opt_root = tmp.join("opt");
    let app_dir = tmp.join("apps");

    let name = pkginstall::install(
        bundle_path.to_str().unwrap(),
        opt_root.to_str().unwrap(),
        app_dir.to_str().unwrap(),
        false,
    )
    .unwrap();
    assert_eq!(name, "hello");

    // Bundle extracted.
    let installed_wasm = opt_root.join("hello/bin/hello.wasm");
    assert!(installed_wasm.exists(), "wasm file extracted");
    assert_eq!(std::fs::read(&installed_wasm).unwrap(), b"\0asm\x01\0\0\0");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            std::fs::metadata(&installed_wasm)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "manifest executable is installed with an execute bit"
        );
    }

    // Desktop entry written.
    let entry = std::fs::read_to_string(app_dir.join("hello.desktop")).unwrap();
    assert!(entry.contains("Type=Application"));
    assert!(entry.contains("Name=Hello"));
    assert!(entry.contains(installed_wasm.to_str().unwrap()));
    assert!(entry.contains("X-PMos-Caps=DISPLAY_CLIENT"));
    assert!(entry.contains("MimeType=application/x-hello;"));
    assert!(entry.contains("Categories=Utility;"));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn install_refuses_to_overwrite_without_upgrade_flag() {
    let bundle = bundle(manifest_toml(), b"\0asm\x01\0\0\0");
    let tmp = write_tempdir("pkginstall-test");
    let bundle_path = tmp.join("hello.pmpkg.tar");
    std::fs::write(&bundle_path, &bundle).unwrap();
    let opt_root = tmp.join("opt");
    let app_dir = tmp.join("apps");

    pkginstall::install(
        bundle_path.to_str().unwrap(),
        opt_root.to_str().unwrap(),
        app_dir.to_str().unwrap(),
        false,
    )
    .unwrap();
    let err = pkginstall::install(
        bundle_path.to_str().unwrap(),
        opt_root.to_str().unwrap(),
        app_dir.to_str().unwrap(),
        false,
    )
    .unwrap_err();
    assert!(err.contains("already installed"));

    // --upgrade succeeds.
    pkginstall::install(
        bundle_path.to_str().unwrap(),
        opt_root.to_str().unwrap(),
        app_dir.to_str().unwrap(),
        true,
    )
    .unwrap();

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn failed_integrity_upgrade_preserves_live_install_and_desktop_entry() {
    let original = bundle(manifest_toml(), b"\0asm\x01\0\0\0");
    let tmp = write_tempdir("pkginstall-integrity-rollback");
    let bundle_path = tmp.join("hello.pmpkg.tar");
    std::fs::write(&bundle_path, &original).unwrap();
    let opt_root = tmp.join("opt");
    let app_dir = tmp.join("apps");
    pkginstall::install(
        bundle_path.to_str().unwrap(),
        opt_root.to_str().unwrap(),
        app_dir.to_str().unwrap(),
        false,
    )
    .unwrap();
    let installed = opt_root.join("hello/bin/hello.wasm");
    let desktop = app_dir.join("hello.desktop");
    let before_binary = std::fs::read(&installed).unwrap();
    let before_desktop = std::fs::read(&desktop).unwrap();

    let manifest = packaged_manifest(manifest_toml(), b"\0asm\x02\0\0\0");
    let tampered = build_tar(&[
        ("manifest.toml", manifest.as_bytes()),
        ("bin/hello.wasm", b"\0asm\x03\0\0\0"),
    ]);
    std::fs::write(&bundle_path, tampered).unwrap();
    let error = pkginstall::install(
        bundle_path.to_str().unwrap(),
        opt_root.to_str().unwrap(),
        app_dir.to_str().unwrap(),
        true,
    )
    .unwrap_err();
    assert!(error.contains("SHA-256 mismatch"), "{error}");
    assert_eq!(std::fs::read(installed).unwrap(), before_binary);
    assert_eq!(std::fs::read(desktop).unwrap(), before_desktop);

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn privileged_required_cap_is_rejected_before_any_installation_mutation() {
    let source = manifest_toml().replace("DISPLAY_CLIENT", "HOST_TRANSFER");
    let bundle = bundle(&source, b"\0asm\x01\0\0\0");
    let tmp = write_tempdir("pkginstall-cap-confinement");
    let bundle_path = tmp.join("hello.pmpkg.tar");
    std::fs::write(&bundle_path, bundle).unwrap();
    let opt_root = tmp.join("opt");
    let app_dir = tmp.join("apps");

    let error = pkginstall::install(
        bundle_path.to_str().unwrap(),
        opt_root.to_str().unwrap(),
        app_dir.to_str().unwrap(),
        false,
    )
    .unwrap_err();
    assert!(error.contains("may not require capability HOST_TRANSFER"));
    assert!(!opt_root.join("hello").exists());
    assert!(!app_dir.join("hello.desktop").exists());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn optional_privileged_cap_is_not_emitted_into_desktop_entry() {
    let source = manifest_toml().replace(
        "required = [\"DISPLAY_CLIENT\"]",
        "required = [\"DISPLAY_CLIENT\"]\noptional = [\"HOST_TRANSFER\"]",
    );
    let bundle = bundle(&source, b"\0asm\x01\0\0\0");
    let tmp = write_tempdir("pkginstall-optional-cap");
    let bundle_path = tmp.join("hello.pmpkg.tar");
    std::fs::write(&bundle_path, bundle).unwrap();
    let opt_root = tmp.join("opt");
    let app_dir = tmp.join("apps");

    pkginstall::install(
        bundle_path.to_str().unwrap(),
        opt_root.to_str().unwrap(),
        app_dir.to_str().unwrap(),
        false,
    )
    .unwrap();
    let desktop = std::fs::read_to_string(app_dir.join("hello.desktop")).unwrap();
    assert!(desktop.contains("X-PMos-Caps=DISPLAY_CLIENT\n"));
    assert!(!desktop.contains("HOST_TRANSFER"));

    let _ = std::fs::remove_dir_all(&tmp);
}
