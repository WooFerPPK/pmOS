use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read};
use std::path::Path;

use abi::cap::{Cap, CapSet};

pub const TEXT_EDITOR_DESKTOP: &str = "/usr/share/applications/edit.desktop";
pub const MAX_DESKTOP_ENTRY_BYTES: usize = 16 * 1024;
const MAX_EXEC_ARGS: usize = 64;
const MAX_EXEC_ARG_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopDispatch {
    pub desktop_path: String,
    pub executable: String,
    pub argv: Vec<String>,
    pub caps: CapSet,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchError {
    NotText,
    Io(String),
    TooLarge,
    InvalidUtf8,
    MissingSection,
    DuplicateKey(String),
    MissingKey(&'static str),
    NotApplication,
    MimeMismatch,
    InvalidExec,
    UnsupportedExecFieldCode(String),
    UnknownCapability(String),
    CapabilityNotDelegable(String),
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotText => write!(f, "no text MIME handler for this file"),
            Self::Io(message) => write!(f, "desktop entry I/O: {message}"),
            Self::TooLarge => write!(
                f,
                "desktop entry exceeds the {MAX_DESKTOP_ENTRY_BYTES}-byte limit"
            ),
            Self::InvalidUtf8 => write!(f, "desktop entry is not valid UTF-8"),
            Self::MissingSection => write!(f, "desktop entry is missing [Desktop Entry]"),
            Self::DuplicateKey(key) => write!(f, "desktop entry repeats {key}"),
            Self::MissingKey(key) => write!(f, "desktop entry is missing {key}"),
            Self::NotApplication => write!(f, "desktop entry Type is not Application"),
            Self::MimeMismatch => write!(f, "desktop entry does not declare the selected MIME"),
            Self::InvalidExec => write!(f, "desktop entry has an invalid Exec command"),
            Self::UnsupportedExecFieldCode(code) => {
                write!(f, "desktop entry uses unsupported Exec field code {code}")
            }
            Self::UnknownCapability(name) => {
                write!(f, "desktop entry names unknown capability {name}")
            }
            Self::CapabilityNotDelegable(name) => {
                write!(f, "Files cannot delegate capability {name}")
            }
        }
    }
}

impl std::error::Error for DispatchError {}

/// Resolve and validate the installed text editor entry for one selected path.
///
/// The fixed association is intentionally small for v1, but the entry itself
/// remains authoritative for executable argv and capabilities. `held_caps`
/// comes from the kernel's `cap_list` extension; checking it here gives a clear
/// user-facing error before `proc_spawn_manifest` performs the same mandatory
/// subset check.
pub fn resolve_text_dispatch(
    selected_path: &Path,
    held_caps: CapSet,
) -> Result<DesktopDispatch, DispatchError> {
    let mime = text_mime_for_path(selected_path).ok_or(DispatchError::NotText)?;
    let bytes = read_bounded(TEXT_EDITOR_DESKTOP)?;
    parse_text_dispatch(TEXT_EDITOR_DESKTOP, &bytes, selected_path, mime, held_caps)
}

pub fn parse_text_dispatch(
    desktop_path: &str,
    bytes: &[u8],
    selected_path: &Path,
    selected_mime: &str,
    held_caps: CapSet,
) -> Result<DesktopDispatch, DispatchError> {
    if bytes.len() > MAX_DESKTOP_ENTRY_BYTES {
        return Err(DispatchError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| DispatchError::InvalidUtf8)?;
    let fields = parse_desktop_fields(text)?;
    if fields.get("Type").map(String::as_str) != Some("Application") {
        return Err(DispatchError::NotApplication);
    }
    let exec = fields
        .get("Exec")
        .ok_or(DispatchError::MissingKey("Exec"))?;
    let declared_mimes = fields
        .get("MimeType")
        .ok_or(DispatchError::MissingKey("MimeType"))?;
    if !split_semicolons(declared_mimes).any(|mime| mime == selected_mime || mime == "text/*") {
        return Err(DispatchError::MimeMismatch);
    }

    let mut argv = tokenize_exec(exec)?;
    let mut inserted_path = false;
    for arg in &mut argv {
        if arg == "%f" {
            *arg = selected_path.to_string_lossy().into_owned();
            inserted_path = true;
        } else if let Some(offset) = arg.find('%') {
            let code = arg[offset..].chars().take(2).collect::<String>();
            return Err(DispatchError::UnsupportedExecFieldCode(code));
        }
    }
    if !inserted_path {
        argv.push(selected_path.to_string_lossy().into_owned());
    }
    if argv.len() > MAX_EXEC_ARGS {
        return Err(DispatchError::InvalidExec);
    }
    let executable = argv.first().cloned().ok_or(DispatchError::InvalidExec)?;
    if !executable.starts_with('/') || executable.len() > MAX_EXEC_ARG_BYTES {
        return Err(DispatchError::InvalidExec);
    }

    let mut caps = CapSet::EMPTY;
    if let Some(names) = fields.get("X-PMos-Caps") {
        for name in split_semicolons(names) {
            let cap = Cap::from_name(name)
                .ok_or_else(|| DispatchError::UnknownCapability(name.to_string()))?;
            caps.insert(cap);
        }
    }
    for raw in 0..64_u32 {
        let bit = 1_u64 << raw;
        if caps.0 & bit == 0 {
            continue;
        }
        let name = Cap::from_u32(raw).map(Cap::name).unwrap_or("unknown");
        if abi::cap::initial::FILES.0 & bit == 0 || held_caps.0 & bit == 0 {
            return Err(DispatchError::CapabilityNotDelegable(name.to_string()));
        }
    }

    Ok(DesktopDispatch {
        desktop_path: desktop_path.to_string(),
        executable,
        argv,
        caps,
    })
}

pub fn text_mime_for_path(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "md" | "markdown" => Some("text/markdown"),
        "txt" | "rs" | "toml" | "json" | "log" | "conf" | "ini" | "csv" | "yaml" | "yml" => {
            Some("text/plain")
        }
        _ => None,
    }
}

fn read_bounded(path: &str) -> Result<Vec<u8>, DispatchError> {
    let file = std::fs::File::open(path).map_err(|error| DispatchError::Io(error.to_string()))?;
    let mut bytes = Vec::new();
    file.take((MAX_DESKTOP_ENTRY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| DispatchError::Io(error.to_string()))?;
    if bytes.len() > MAX_DESKTOP_ENTRY_BYTES {
        return Err(DispatchError::TooLarge);
    }
    Ok(bytes)
}

fn parse_desktop_fields(text: &str) -> Result<BTreeMap<String, String>, DispatchError> {
    let mut in_desktop = false;
    let mut saw_desktop = false;
    let mut fields = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_desktop = line == "[Desktop Entry]";
            saw_desktop |= in_desktop;
            continue;
        }
        if !in_desktop {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !matches!(key, "Type" | "Exec" | "MimeType" | "X-PMos-Caps") {
            continue;
        }
        if fields
            .insert(key.to_string(), value.trim().to_string())
            .is_some()
        {
            return Err(DispatchError::DuplicateKey(key.to_string()));
        }
    }
    if !saw_desktop {
        return Err(DispatchError::MissingSection);
    }
    Ok(fields)
}

fn split_semicolons(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
}

fn tokenize_exec(exec: &str) -> Result<Vec<String>, DispatchError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut started = false;
    for character in exec.chars() {
        if character.is_control() {
            return Err(DispatchError::InvalidExec);
        }
        if escaped {
            current.push(character);
            escaped = false;
            started = true;
            continue;
        }
        match (quote, character) {
            (Quote::None | Quote::Double, '\\') => escaped = true,
            (Quote::None, '\'') => {
                quote = Quote::Single;
                started = true;
            }
            (Quote::Single, '\'') => quote = Quote::None,
            (Quote::None, '"') => {
                quote = Quote::Double;
                started = true;
            }
            (Quote::Double, '"') => quote = Quote::None,
            (Quote::None, character) if character.is_whitespace() => {
                if started {
                    push_exec_arg(&mut args, std::mem::take(&mut current))?;
                    started = false;
                }
            }
            (_, character) => {
                current.push(character);
                started = true;
                if current.len() > MAX_EXEC_ARG_BYTES {
                    return Err(DispatchError::InvalidExec);
                }
            }
        }
    }
    if escaped || quote != Quote::None {
        return Err(DispatchError::InvalidExec);
    }
    if started {
        push_exec_arg(&mut args, current)?;
    }
    if args.is_empty() {
        return Err(DispatchError::InvalidExec);
    }
    Ok(args)
}

fn push_exec_arg(args: &mut Vec<String>, arg: String) -> Result<(), DispatchError> {
    if arg.len() > MAX_EXEC_ARG_BYTES || args.len() >= MAX_EXEC_ARGS {
        return Err(DispatchError::InvalidExec);
    }
    args.push(arg);
    Ok(())
}

impl From<io::Error> for DispatchError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}
