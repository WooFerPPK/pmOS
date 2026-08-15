//! T206: malformed-bundle rejection tests.

use pkg::{build_tar, sha256_hex, validate_bundle, PkgError};

fn good_manifest() -> &'static str {
    r#"
[package]
name = "good"
version = "1.0.0"
display_name = "Good"
author = "X"
summary = "ok."

[exec]
binary = "bin/good.wasm"

[capabilities]
required = []
"#
}

fn good_wasm() -> &'static [u8] {
    b"\0asm\x01\0\0\0"
}

fn manifest_with_integrity(manifest: &str, path: &str, bytes: &[u8]) -> String {
    format!(
        "{}\n[integrity]\nsha256 = {{ \"{}\" = \"{}\" }}\n",
        manifest,
        path,
        sha256_hex(bytes)
    )
}

fn good_packaged_manifest() -> String {
    manifest_with_integrity(good_manifest(), "bin/good.wasm", good_wasm())
}

#[test]
fn rejects_absolute_path_inside_archive() {
    let manifest = good_packaged_manifest();
    let tar = build_tar(&[
        ("/etc/passwd", b"x"),
        ("manifest.toml", manifest.as_bytes()),
        ("bin/good.wasm", good_wasm()),
    ]);
    assert!(matches!(
        validate_bundle(&tar),
        Err(PkgError::InvalidPath(_))
    ));
}

#[test]
fn rejects_dotdot_segment() {
    let manifest = good_packaged_manifest();
    let tar = build_tar(&[
        ("../sneaky", b"x"),
        ("manifest.toml", manifest.as_bytes()),
        ("bin/good.wasm", good_wasm()),
    ]);
    assert!(matches!(
        validate_bundle(&tar),
        Err(PkgError::InvalidPath(_))
    ));
}

#[test]
fn rejects_bad_wasm_magic() {
    let wasm = b"NOTWASM";
    let manifest = manifest_with_integrity(good_manifest(), "bin/good.wasm", wasm);
    let tar = build_tar(&[
        ("manifest.toml", manifest.as_bytes()),
        ("bin/good.wasm", wasm),
    ]);
    assert!(matches!(validate_bundle(&tar), Err(PkgError::BadWasmMagic)));
}

#[test]
fn rejects_missing_required_field() {
    let bad_manifest = r#"
[package]
name = "missing"
version = "1.0.0"
author = "X"
summary = "no display_name."

[exec]
binary = "bin/x.wasm"

[capabilities]
required = []
"#;
    let manifest = manifest_with_integrity(bad_manifest, "bin/x.wasm", good_wasm());
    let tar = build_tar(&[
        ("manifest.toml", manifest.as_bytes()),
        ("bin/x.wasm", good_wasm()),
    ]);
    assert!(matches!(
        validate_bundle(&tar),
        Err(PkgError::MissingField(_))
    ));
}

#[test]
fn rejects_invalid_name() {
    let bad = good_manifest().replace("good", "BAD!NAME");
    let manifest = manifest_with_integrity(&bad, "bin/good.wasm", good_wasm());
    let tar = build_tar(&[
        ("manifest.toml", manifest.as_bytes()),
        ("bin/good.wasm", good_wasm()),
    ]);
    let err = validate_bundle(&tar).unwrap_err();
    assert!(matches!(err, PkgError::InvalidName(_)), "got {err:?}");
}

#[test]
fn rejects_unknown_cap() {
    let bad = good_manifest().replace("required = []", "required = [\"NOT_A_CAP\"]");
    let manifest = manifest_with_integrity(&bad, "bin/good.wasm", good_wasm());
    let tar = build_tar(&[
        ("manifest.toml", manifest.as_bytes()),
        ("bin/good.wasm", good_wasm()),
    ]);
    assert!(matches!(
        validate_bundle(&tar),
        Err(PkgError::UnknownCap(_))
    ));
}

#[test]
fn rejects_missing_binary_file() {
    let manifest = good_packaged_manifest();
    let tar = build_tar(&[("manifest.toml", manifest.as_bytes())]);
    let err = validate_bundle(&tar).unwrap_err();
    assert!(matches!(err, PkgError::InvalidPath(_)));
}

#[test]
fn rejects_missing_manifest() {
    let tar = build_tar(&[("bin/good.wasm", good_wasm())]);
    let err = validate_bundle(&tar).unwrap_err();
    assert!(matches!(err, PkgError::MissingField("manifest.toml")));
}

#[test]
fn empty_archive_rejected() {
    let err = validate_bundle(&[]).unwrap_err();
    assert!(matches!(err, PkgError::BundleEmpty));
}

#[test]
fn rejects_bundle_without_payload_integrity() {
    let tar = build_tar(&[
        ("manifest.toml", good_manifest().as_bytes()),
        ("bin/good.wasm", good_wasm()),
    ]);
    assert!(matches!(
        validate_bundle(&tar),
        Err(PkgError::MissingIntegrity(path)) if path == "bin/good.wasm"
    ));
}

#[test]
fn rejects_integrity_entry_without_payload() {
    let manifest = format!(
        "{}\n[integrity]\nsha256 = {{ \"bin/good.wasm\" = \"{}\", \"assets/missing\" = \"{}\" }}\n",
        good_manifest(),
        sha256_hex(good_wasm()),
        sha256_hex(b"missing")
    );
    let tar = build_tar(&[
        ("manifest.toml", manifest.as_bytes()),
        ("bin/good.wasm", good_wasm()),
    ]);
    assert!(matches!(
        validate_bundle(&tar),
        Err(PkgError::UnexpectedIntegrity(path)) if path == "assets/missing"
    ));
}
