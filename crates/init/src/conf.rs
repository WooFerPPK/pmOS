//! `/etc/init.conf` parser + defaults.
//!
//! Implements the contract in
//! [`specs/001-browser-os-v1/contracts/init-conf.md`]. The parser
//! is a minimal hand-rolled TOML subset matching what `init.conf`
//! actually uses:
//!
//! * Tables: top-level `[section]` and `[section.subsection]`
//!   (one level of nesting — `capabilities.display-server` etc.).
//! * Keys: `key = value` with leading/trailing whitespace ignored.
//! * Values: bare strings (`"..."` or `'...'`) and string arrays
//!   (`["a", "b"]`). Numbers and booleans aren't currently
//!   needed; the [`debug`] section's `serial_shell = false` is
//!   accepted as a degenerate string-or-bool case.
//! * Comments: `#` to end of line.
//! * Whitespace + blank lines between entries.
//!
//! Anything else in the file (multi-line strings, dotted-key
//! shorthand, inline tables, datetimes) is rejected with a
//! parse error that surfaces a line number — init falls back to
//! the built-in defaults rather than booting with a half-parsed
//! config.

use std::collections::BTreeMap;

/// Parsed `/etc/init.conf` view used by init's spawn loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitConfig {
    pub boot: Boot,
    pub capabilities: BTreeMap<String, Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub debug: Debug,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    pub display_server: String,
    pub shell: String,
    pub autostart: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Debug {
    pub kernel_log_level: String,
    pub serial_shell: bool,
}

impl Default for InitConfig {
    fn default() -> Self {
        Self::builtin_defaults()
    }
}

impl InitConfig {
    /// The contract's documented defaults — what init uses when
    /// `/etc/init.conf` is missing or unparseable.
    pub fn builtin_defaults() -> Self {
        InitConfig {
            boot: Boot {
                display_server: String::from("/usr/bin/display-server"),
                shell: String::from("/usr/bin/shell"),
                autostart: Vec::new(),
            },
            capabilities: BTreeMap::new(),
            env: BTreeMap::new(),
            debug: Debug {
                kernel_log_level: String::from("info"),
                serial_shell: false,
            },
        }
    }

    /// Parse `/etc/init.conf` text into an `InitConfig`. Errors
    /// carry a 1-indexed line number so init's warning log is
    /// actionable. Caller is expected to fall back to
    /// [`builtin_defaults`] on any error.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let mut out = Self::builtin_defaults();
        let mut current_section: Vec<String> = Vec::new();
        for (idx, raw_line) in input.lines().enumerate() {
            let line_no = idx + 1;
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[') {
                let header = rest
                    .strip_suffix(']')
                    .ok_or_else(|| ParseError::new(line_no, "section header missing closing ]"))?;
                current_section = header
                    .split('.')
                    .map(|s| s.trim().to_string())
                    .collect::<Vec<_>>();
                if current_section.iter().any(|s| s.is_empty()) {
                    return Err(ParseError::new(line_no, "empty section component"));
                }
                continue;
            }
            let eq = line
                .find('=')
                .ok_or_else(|| ParseError::new(line_no, "expected `key = value`"))?;
            let key = line[..eq].trim().to_string();
            let value = line[eq + 1..].trim();
            if key.is_empty() {
                return Err(ParseError::new(line_no, "empty key"));
            }
            apply_assignment(&mut out, &current_section, &key, value, line_no)?;
        }
        Ok(out)
    }
}

fn apply_assignment(
    cfg: &mut InitConfig,
    section: &[String],
    key: &str,
    value: &str,
    line_no: usize,
) -> Result<(), ParseError> {
    match section {
        [] => Err(ParseError::new(line_no, "key outside any [section]")),
        [s] if s == "boot" => apply_boot(&mut cfg.boot, key, value, line_no),
        [s] if s == "env" => {
            let v = parse_string(value, line_no)?;
            cfg.env.insert(key.to_string(), v);
            Ok(())
        }
        [s] if s == "debug" => apply_debug(&mut cfg.debug, key, value, line_no),
        [s, name] if s == "capabilities" => {
            if key != "grant" {
                return Err(ParseError::new(
                    line_no,
                    "[capabilities.<name>] only supports `grant = [\"...\", ...]`",
                ));
            }
            let caps = parse_string_array(value, line_no)?;
            cfg.capabilities.insert(name.to_string(), caps);
            Ok(())
        }
        _ => Err(ParseError::new(line_no, "unknown section")),
    }
}

fn apply_boot(boot: &mut Boot, key: &str, value: &str, line_no: usize) -> Result<(), ParseError> {
    match key {
        "display_server" => {
            boot.display_server = parse_string(value, line_no)?;
            Ok(())
        }
        "shell" => {
            boot.shell = parse_string(value, line_no)?;
            Ok(())
        }
        "autostart" => {
            boot.autostart = parse_string_array(value, line_no)?;
            Ok(())
        }
        _ => Err(ParseError::new(line_no, "unknown [boot] key")),
    }
}

fn apply_debug(
    debug: &mut Debug,
    key: &str,
    value: &str,
    line_no: usize,
) -> Result<(), ParseError> {
    match key {
        "kernel_log_level" => {
            let v = parse_string(value, line_no)?;
            match v.as_str() {
                "trace" | "debug" | "info" | "warn" | "error" => {
                    debug.kernel_log_level = v;
                    Ok(())
                }
                _ => Err(ParseError::new(line_no, "kernel_log_level must be one of trace|debug|info|warn|error")),
            }
        }
        "serial_shell" => {
            debug.serial_shell = parse_bool(value, line_no)?;
            Ok(())
        }
        _ => Err(ParseError::new(line_no, "unknown [debug] key")),
    }
}

fn parse_string(value: &str, line_no: usize) -> Result<String, ParseError> {
    let value = value.trim();
    if value.len() < 2 {
        return Err(ParseError::new(line_no, "string value too short"));
    }
    let first = value.as_bytes()[0];
    let last = value.as_bytes()[value.len() - 1];
    if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
        Ok(value[1..value.len() - 1].to_string())
    } else {
        Err(ParseError::new(line_no, "string value must be quoted"))
    }
}

fn parse_string_array(value: &str, line_no: usize) -> Result<Vec<String>, ParseError> {
    let value = value.trim();
    let inner = value
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| ParseError::new(line_no, "array value must be wrapped in [...]"))?;
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for part in trimmed.split(',') {
        let s = parse_string(part.trim(), line_no)?;
        out.push(s);
    }
    Ok(out)
}

fn parse_bool(value: &str, line_no: usize) -> Result<bool, ParseError> {
    match value.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ParseError::new(line_no, "boolean value must be `true` or `false`")),
    }
}

fn strip_comment(line: &str) -> &str {
    // TOML allows `#` inside double-quoted strings, but the
    // init.conf schema's strings never contain `#`, so a simple
    // first-`#` split is exact for the documented schema.
    let mut in_string: Option<char> = None;
    let bytes = line.as_bytes();
    for (i, ch) in line.char_indices() {
        match (ch, in_string) {
            ('"' | '\'', None) => in_string = Some(ch),
            (c, Some(open)) if c == open => in_string = None,
            ('#', None) => return &line[..i],
            _ => {}
        }
        let _ = bytes; // appease clippy
    }
    line
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl ParseError {
    fn new(line: usize, message: &str) -> Self {
        ParseError {
            line,
            message: message.to_string(),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "/etc/init.conf:{}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_values() {
        let d = InitConfig::builtin_defaults();
        assert_eq!(d.boot.display_server, "/usr/bin/display-server");
        assert_eq!(d.boot.shell, "/usr/bin/shell");
        assert!(d.boot.autostart.is_empty());
        assert!(d.capabilities.is_empty());
        assert!(d.env.is_empty());
        assert_eq!(d.debug.kernel_log_level, "info");
        assert!(!d.debug.serial_shell);
    }

    #[test]
    fn parse_empty_input_returns_defaults() {
        let cfg = InitConfig::parse("").unwrap();
        assert_eq!(cfg, InitConfig::builtin_defaults());
    }

    #[test]
    fn parse_full_documented_example() {
        let input = r#"
[boot]
display_server = "/usr/bin/display-server"
shell          = "/usr/bin/shell"
autostart      = ["sysmon", "settings"]

[capabilities.display-server]
grant = ["DISPLAY_SERVER", "DEV_BLOCK"]

[capabilities.shell]
grant = ["DISPLAY_CLIENT", "SHELL", "PROC_ENUMERATE", "KEYMAP_ADMIN"]

[env]
PATH = "/bin:/usr/bin"
HOME = "/home/user"

[debug]
kernel_log_level = "info"
serial_shell     = false
"#;
        let cfg = InitConfig::parse(input).unwrap();
        assert_eq!(cfg.boot.display_server, "/usr/bin/display-server");
        assert_eq!(cfg.boot.shell, "/usr/bin/shell");
        assert_eq!(cfg.boot.autostart, vec!["sysmon", "settings"]);
        assert_eq!(
            cfg.capabilities.get("display-server").unwrap(),
            &vec!["DISPLAY_SERVER".to_string(), "DEV_BLOCK".to_string()]
        );
        assert_eq!(cfg.env.get("HOME").unwrap(), "/home/user");
        assert_eq!(cfg.debug.kernel_log_level, "info");
        assert!(!cfg.debug.serial_shell);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let input = r#"
# top-level comment

[boot]
# inline-section comment
shell = "/usr/bin/sh"   # trailing comment after a value

# blank line above
"#;
        let cfg = InitConfig::parse(input).unwrap();
        assert_eq!(cfg.boot.shell, "/usr/bin/sh");
    }

    #[test]
    fn missing_section_for_a_key_is_an_error() {
        let err = InitConfig::parse("shell = \"/x\"").unwrap_err();
        assert!(err.message.contains("outside any"));
    }

    #[test]
    fn unknown_section_is_an_error() {
        let err = InitConfig::parse("[unknown]\nx = \"y\"").unwrap_err();
        assert!(err.message.contains("unknown section"));
    }

    #[test]
    fn unknown_boot_key_is_an_error() {
        let err = InitConfig::parse("[boot]\nfoo = \"bar\"").unwrap_err();
        assert!(err.message.contains("unknown [boot] key"));
    }

    #[test]
    fn unquoted_string_value_is_an_error() {
        let err = InitConfig::parse("[boot]\nshell = bare").unwrap_err();
        assert!(err.message.contains("string"));
    }

    #[test]
    fn malformed_array_is_an_error() {
        let err = InitConfig::parse("[boot]\nautostart = \"not_an_array\"").unwrap_err();
        assert!(err.message.contains("array"));
    }

    #[test]
    fn empty_array_is_accepted() {
        let cfg = InitConfig::parse("[boot]\nautostart = []").unwrap();
        assert!(cfg.boot.autostart.is_empty());
    }

    #[test]
    fn invalid_kernel_log_level_rejected() {
        let err = InitConfig::parse(
            "[debug]\nkernel_log_level = \"loud\"",
        )
        .unwrap_err();
        assert!(err.message.contains("kernel_log_level"));
    }

    #[test]
    fn invalid_boolean_rejected() {
        let err = InitConfig::parse(
            "[debug]\nserial_shell = nope",
        )
        .unwrap_err();
        assert!(err.message.contains("boolean"));
    }

    #[test]
    fn capabilities_only_accepts_grant_key() {
        let err = InitConfig::parse(
            "[capabilities.shell]\nrevoke = [\"X\"]",
        )
        .unwrap_err();
        assert!(err.message.contains("grant"));
    }

    #[test]
    fn env_section_collects_string_pairs_in_sorted_order() {
        let input = r#"
[env]
ZED = "z"
ALPHA = "a"
"#;
        let cfg = InitConfig::parse(input).unwrap();
        // BTreeMap keeps keys sorted — used by the
        // env-var-whitelist passthrough so the test pinning
        // sorted order is deterministic.
        let keys: Vec<&str> = cfg.env.keys().map(|s| s.as_str()).collect();
        assert_eq!(keys, vec!["ALPHA", "ZED"]);
    }

    #[test]
    fn single_quoted_strings_round_trip() {
        let cfg = InitConfig::parse("[boot]\nshell = '/usr/bin/sh'").unwrap();
        assert_eq!(cfg.boot.shell, "/usr/bin/sh");
    }

    #[test]
    fn line_numbers_in_errors_point_at_the_offending_line() {
        let input = "[boot]\nshell = \"/x\"\nbroken_line";
        let err = InitConfig::parse(input).unwrap_err();
        assert_eq!(err.line, 3);
    }

    #[test]
    fn bracketed_section_with_unclosed_bracket_errors() {
        let err = InitConfig::parse("[boot\nshell = \"/x\"").unwrap_err();
        assert_eq!(err.line, 1);
        assert!(err.message.contains("closing"));
    }

    #[test]
    fn comments_inside_string_values_are_preserved() {
        // The documented schema doesn't actually use `#` inside
        // quoted strings, but the parser is defensive about it.
        let input = "[env]\nX = \"a # not_a_comment\"";
        let cfg = InitConfig::parse(input).unwrap();
        assert_eq!(cfg.env.get("X").unwrap(), "a # not_a_comment");
    }
}
