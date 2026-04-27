//! T205: pkginstall fixture-bundle integration tests.
//!
//! Build a real `.pmpkg.tar` in-memory, drop it into a tempdir,
//! run the install logic, and assert: bundle is extracted under
//! `<opt-root>/<name>/`, desktop entry is written, and the
//! desktop entry has the expected fields.

use pkg::build_tar;

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
    let bundle = build_tar(&[
        ("manifest.toml", manifest_toml().as_bytes()),
        ("bin/hello.wasm", b"\0asm\x01\0\0\0"),
    ]);
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
    let bundle = build_tar(&[
        ("manifest.toml", manifest_toml().as_bytes()),
        ("bin/hello.wasm", b"\0asm\x01\0\0\0"),
    ]);
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
