//! `pkg` — package-format validation library (T201).
//!
//! Hand-rolled (no `tar`/`toml` deps) parser + validator for
//! `.pmpkg.tar` bundles and their `manifest.toml` per
//! `specs/001-browser-os-v1/contracts/package-manifest.md`.
//!
//! Shared between `pkginstall` (T198) and the launcher (T200).

#![allow(clippy::too_many_lines)]

use std::collections::BTreeMap;

/// Known capability names (`data-model.md §5`).
pub const KNOWN_CAPS: &[&str] = &[
    "DISPLAY_CLIENT",
    "SHELL",
    "PROC_ENUMERATE",
    "PROC_KILL_ANY",
    "FS_WRITE_ROOT",
    "NET_CLIENT",
    "KEYMAP_ADMIN",
    "CAP_GRANT",
];

/// Parsed manifest with the four required sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub display_name: String,
    pub author: String,
    pub summary: String,
    pub binary: String,
    pub argv: Vec<String>,
    pub envp: BTreeMap<String, String>,
    pub icon: Option<String>,
    pub mime_types: Vec<String>,
    pub categories: Vec<String>,
    pub caps_required: Vec<String>,
    pub caps_optional: Vec<String>,
}

/// Validation error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkgError {
    InvalidUtf8,
    MalformedToml(String),
    MissingField(&'static str),
    InvalidName(String),
    InvalidVersion(String),
    InvalidPath(String),
    UnknownCap(String),
    BadWasmMagic,
    NotTar,
    BundleEmpty,
    BadEntry(String),
}

impl std::fmt::Display for PkgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUtf8 => write!(f, "invalid UTF-8"),
            Self::MalformedToml(s) => write!(f, "malformed manifest.toml: {s}"),
            Self::MissingField(s) => write!(f, "missing required field {s}"),
            Self::InvalidName(s) => write!(f, "invalid package.name {s:?}"),
            Self::InvalidVersion(s) => write!(f, "invalid semver version {s:?}"),
            Self::InvalidPath(s) => write!(f, "invalid path {s:?}"),
            Self::UnknownCap(s) => write!(f, "unknown capability {s}"),
            Self::BadWasmMagic => write!(f, "exec.binary is not a WASM file (bad magic)"),
            Self::NotTar => write!(f, "archive is not a valid tar"),
            Self::BundleEmpty => write!(f, "bundle is empty"),
            Self::BadEntry(s) => write!(f, "bad bundle entry: {s}"),
        }
    }
}

impl std::error::Error for PkgError {}

/// Parse a manifest.toml from raw bytes per the schema.
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, PkgError> {
    let text = std::str::from_utf8(bytes).map_err(|_| PkgError::InvalidUtf8)?;
    let parsed = parse_toml_minimal(text)?;

    let pkg = parsed.get("package").ok_or(PkgError::MissingField("[package]"))?;
    let exec = parsed.get("exec").ok_or(PkgError::MissingField("[exec]"))?;
    let caps = parsed
        .get("capabilities")
        .ok_or(PkgError::MissingField("[capabilities]"))?;
    let ui = parsed.get("ui");

    let name = pkg
        .get_str("name")
        .ok_or(PkgError::MissingField("package.name"))?
        .to_string();
    if !valid_pkg_name(&name) {
        return Err(PkgError::InvalidName(name));
    }
    let version = pkg
        .get_str("version")
        .ok_or(PkgError::MissingField("package.version"))?
        .to_string();
    if !valid_semver(&version) {
        return Err(PkgError::InvalidVersion(version));
    }
    let display_name = pkg
        .get_str("display_name")
        .ok_or(PkgError::MissingField("package.display_name"))?
        .to_string();
    let author = pkg
        .get_str("author")
        .ok_or(PkgError::MissingField("package.author"))?
        .to_string();
    let summary = pkg
        .get_str("summary")
        .ok_or(PkgError::MissingField("package.summary"))?
        .to_string();

    let binary = exec
        .get_str("binary")
        .ok_or(PkgError::MissingField("exec.binary"))?
        .to_string();
    if !valid_relative_path(&binary) {
        return Err(PkgError::InvalidPath(binary));
    }
    let argv = exec.get_array("argv").unwrap_or_default();
    let envp = exec.get_table("envp").unwrap_or_default();

    let icon = ui.and_then(|u| u.get_str("icon")).map(String::from);
    if let Some(p) = &icon {
        if !valid_relative_path(p) {
            return Err(PkgError::InvalidPath(p.clone()));
        }
    }
    let mime_types = ui.and_then(|u| u.get_array("mime_types")).unwrap_or_default();
    let categories = ui.and_then(|u| u.get_array("categories")).unwrap_or_default();

    let caps_required = caps
        .get_array("required")
        .ok_or(PkgError::MissingField("capabilities.required"))?;
    for c in &caps_required {
        if !KNOWN_CAPS.contains(&c.as_str()) {
            return Err(PkgError::UnknownCap(c.clone()));
        }
    }
    let caps_optional = caps.get_array("optional").unwrap_or_default();
    for c in &caps_optional {
        if !KNOWN_CAPS.contains(&c.as_str()) {
            return Err(PkgError::UnknownCap(c.clone()));
        }
    }

    Ok(Manifest {
        name,
        version,
        display_name,
        author,
        summary,
        binary,
        argv,
        envp,
        icon,
        mime_types,
        categories,
        caps_required,
        caps_optional,
    })
}

/// `[a-z0-9_-]+`, 1..=40.
pub fn valid_pkg_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// Semver `MAJOR.MINOR.PATCH` (digits only; no pre-release/build metadata for v1).
pub fn valid_semver(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Relative path: no leading `/`, no `..` segments, no empty.
pub fn valid_relative_path(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.split('/').any(|seg| seg == ".." || seg.is_empty())
}

/// Validate the bytes of a `.pmpkg.tar` bundle end-to-end.
///
/// Steps mirror `package-manifest.md §2.3`:
///  1. Tar is well-formed and contains `manifest.toml`.
///  2. manifest is parseable + valid.
///  3. exec.binary file is present and starts with WASM magic.
///  4. ui.icon, if present, exists.
pub fn validate_bundle(bytes: &[u8]) -> Result<Manifest, PkgError> {
    let entries = parse_tar(bytes)?;
    if entries.is_empty() {
        return Err(PkgError::BundleEmpty);
    }
    for (name, _) in &entries {
        if name.starts_with('/') || name.contains("..") {
            return Err(PkgError::InvalidPath(name.clone()));
        }
    }
    let manifest_bytes = entries
        .iter()
        .find(|(n, _)| n == "manifest.toml")
        .map(|(_, b)| b.as_slice())
        .ok_or(PkgError::MissingField("manifest.toml"))?;
    let manifest = parse_manifest(manifest_bytes)?;

    let bin = entries
        .iter()
        .find(|(n, _)| n == &manifest.binary)
        .ok_or_else(|| PkgError::InvalidPath(manifest.binary.clone()))?;
    let bin_bytes = &bin.1;
    if bin_bytes.len() < 4 || &bin_bytes[..4] != b"\0asm" {
        return Err(PkgError::BadWasmMagic);
    }
    if let Some(icon) = &manifest.icon {
        if !entries.iter().any(|(n, _)| n == icon) {
            return Err(PkgError::InvalidPath(icon.clone()));
        }
    }
    Ok(manifest)
}

/// Parse a POSIX tar archive (ustar or v7 — header overlap suffices for v1).
/// Returns a Vec of (path, contents). Skips directory entries.
pub fn parse_tar(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, PkgError> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + 512 <= bytes.len() {
        let header = &bytes[off..off + 512];
        // Two zero blocks signal end-of-archive.
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let name = read_cstr(&header[0..100]);
        let typeflag = header[156];
        let size = parse_octal(&header[124..136])?;
        off += 512;
        if typeflag == b'0' || typeflag == 0 {
            // Regular file.
            let end = off + size;
            if end > bytes.len() {
                return Err(PkgError::NotTar);
            }
            out.push((name, bytes[off..end].to_vec()));
        }
        // Round size up to the next 512-byte block.
        let blocks = (size + 511) / 512;
        off += blocks * 512;
    }
    Ok(out)
}

/// Build a minimal POSIX tar archive from (path, bytes) entries. ustar magic.
pub fn build_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, data) in entries {
        let mut header = [0u8; 512];
        let name_bytes = path.as_bytes();
        assert!(name_bytes.len() < 100, "path too long: {path}");
        header[..name_bytes.len()].copy_from_slice(name_bytes);
        // mode = 0644
        header[100..107].copy_from_slice(b"0000644");
        // uid/gid = 0
        header[108..115].copy_from_slice(b"0000000");
        header[116..123].copy_from_slice(b"0000000");
        // size as 11-octal-digit string + space+null
        let size_str = format!("{:011o}", data.len());
        header[124..135].copy_from_slice(size_str.as_bytes());
        // mtime = 0
        header[136..147].copy_from_slice(b"00000000000");
        // checksum placeholder (8 spaces) for now
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        // ustar magic
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        // checksum
        let csum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        let csum_str = format!("{:06o}\0 ", csum);
        header[148..156].copy_from_slice(csum_str.as_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        out.extend(vec![0u8; pad]);
    }
    // Two zero blocks terminator.
    out.extend(vec![0u8; 1024]);
    out
}

fn read_cstr(slice: &[u8]) -> String {
    let end = slice.iter().position(|b| *b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).into_owned()
}

fn parse_octal(slice: &[u8]) -> Result<usize, PkgError> {
    let mut s = std::str::from_utf8(slice)
        .map_err(|_| PkgError::NotTar)?
        .trim_end_matches('\0')
        .trim();
    if s.is_empty() {
        s = "0";
    }
    usize::from_str_radix(s, 8).map_err(|_| PkgError::NotTar)
}

// ---- minimal TOML parser ------------------------------------------------

#[derive(Debug, Default)]
struct Section {
    pairs: Vec<(String, Value)>,
}

#[derive(Debug, Clone)]
enum Value {
    String(String),
    Array(Vec<String>),
    Table(BTreeMap<String, String>),
}

impl Section {
    fn get_str(&self, key: &str) -> Option<&str> {
        self.pairs.iter().find_map(|(k, v)| {
            if k == key {
                if let Value::String(s) = v {
                    return Some(s.as_str());
                }
            }
            None
        })
    }
    fn get_array(&self, key: &str) -> Option<Vec<String>> {
        self.pairs.iter().find_map(|(k, v)| {
            if k == key {
                if let Value::Array(a) = v {
                    return Some(a.clone());
                }
            }
            None
        })
    }
    fn get_table(&self, key: &str) -> Option<BTreeMap<String, String>> {
        self.pairs.iter().find_map(|(k, v)| {
            if k == key {
                if let Value::Table(t) = v {
                    return Some(t.clone());
                }
            }
            None
        })
    }
}

fn parse_toml_minimal(text: &str) -> Result<BTreeMap<String, Section>, PkgError> {
    let mut sections: BTreeMap<String, Section> = BTreeMap::new();
    let mut current = String::new();
    sections.insert(String::new(), Section::default());
    for (lineno, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            current = rest.trim().to_string();
            sections.entry(current.clone()).or_default();
            continue;
        }
        let eq = line.find('=').ok_or_else(|| {
            PkgError::MalformedToml(format!("line {}: no '='", lineno + 1))
        })?;
        let key = line[..eq].trim().to_string();
        let val = line[eq + 1..].trim();
        let value = parse_value(val).map_err(|e| {
            PkgError::MalformedToml(format!("line {}: {}", lineno + 1, e))
        })?;
        sections
            .entry(current.clone())
            .or_default()
            .pairs
            .push((key, value));
    }
    Ok(sections)
}

fn parse_value(s: &str) -> Result<Value, String> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        return Ok(Value::String(rest.to_string()));
    }
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        let inner = inner.trim();
        if inner.is_empty() {
            return Ok(Value::Array(Vec::new()));
        }
        let mut items = Vec::new();
        for raw in inner.split(',') {
            let raw = raw.trim();
            let unquoted = raw
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .ok_or_else(|| format!("array element not a quoted string: {raw}"))?;
            items.push(unquoted.to_string());
        }
        return Ok(Value::Array(items));
    }
    if let Some(inner) = s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        let inner = inner.trim();
        let mut t = BTreeMap::new();
        if inner.is_empty() {
            return Ok(Value::Table(t));
        }
        for raw in inner.split(',') {
            let raw = raw.trim();
            let eq = raw.find('=').ok_or_else(|| format!("inline-table item: {raw}"))?;
            let k = raw[..eq].trim().to_string();
            let v_raw = raw[eq + 1..].trim();
            let unquoted = v_raw
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .ok_or_else(|| format!("inline-table value not quoted: {v_raw}"))?;
            t.insert(k, unquoted.to_string());
        }
        return Ok(Value::Table(t));
    }
    Err(format!("unrecognised value: {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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

[capabilities]
required = ["DISPLAY_CLIENT"]
"#
    }

    #[test]
    fn parses_minimal_manifest() {
        let m = parse_manifest(manifest_toml().as_bytes()).unwrap();
        assert_eq!(m.name, "hello");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.binary, "bin/hello.wasm");
        assert_eq!(m.caps_required, vec!["DISPLAY_CLIENT"]);
    }

    #[test]
    fn rejects_invalid_name() {
        let bad = manifest_toml().replace("hello", "Hello!");
        let err = parse_manifest(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, PkgError::InvalidName(_)));
    }

    #[test]
    fn rejects_invalid_version() {
        let bad = manifest_toml().replace("0.1.0", "0.1");
        let err = parse_manifest(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, PkgError::InvalidVersion(_)));
    }

    #[test]
    fn rejects_unknown_cap() {
        let bad = manifest_toml().replace("DISPLAY_CLIENT", "FAKE_CAP");
        let err = parse_manifest(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, PkgError::UnknownCap(_)));
    }

    #[test]
    fn tar_roundtrip() {
        let tar = build_tar(&[
            ("manifest.toml", manifest_toml().as_bytes()),
            ("bin/hello.wasm", b"\0asm\x01\0\0\0"),
        ]);
        let entries = parse_tar(&tar).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "manifest.toml");
        assert_eq!(entries[1].0, "bin/hello.wasm");
        assert_eq!(&entries[1].1, b"\0asm\x01\0\0\0");
    }

    #[test]
    fn validate_full_bundle() {
        let tar = build_tar(&[
            ("manifest.toml", manifest_toml().as_bytes()),
            ("bin/hello.wasm", b"\0asm\x01\0\0\0"),
        ]);
        let m = validate_bundle(&tar).unwrap();
        assert_eq!(m.name, "hello");
    }

    #[test]
    fn rejects_bad_wasm_magic() {
        let tar = build_tar(&[
            ("manifest.toml", manifest_toml().as_bytes()),
            ("bin/hello.wasm", b"NOTWASM!"),
        ]);
        assert!(matches!(validate_bundle(&tar), Err(PkgError::BadWasmMagic)));
    }

    #[test]
    fn rejects_path_with_dotdot() {
        let bad_tar = build_tar(&[
            ("../etc/passwd", b"x"),
            ("manifest.toml", manifest_toml().as_bytes()),
            ("bin/hello.wasm", b"\0asm\x01\0\0\0"),
        ]);
        assert!(matches!(
            validate_bundle(&bad_tar),
            Err(PkgError::InvalidPath(_))
        ));
    }

    #[test]
    fn valid_relative_path_rejects_absolute() {
        assert!(!valid_relative_path("/etc/passwd"));
        assert!(!valid_relative_path("a/../b"));
        assert!(!valid_relative_path(""));
        assert!(valid_relative_path("bin/hello.wasm"));
    }
}
