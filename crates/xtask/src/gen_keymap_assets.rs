//! Deterministic PMKM v1 keymap asset generator.

use std::fs;
use std::path::PathBuf;

type Result<T> = std::result::Result<T, String>;
type Entry = (u8, u32, u32);

pub fn run(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return Err("gen-keymap-assets takes no arguments".to_string());
    }
    let root = repo_root()?;
    let keymap_dir = root.join("crates/display-server/assets/keymaps");
    fs::create_dir_all(&keymap_dir).map_err(|error| error.to_string())?;

    let us = us_qwerty();
    let uk = uk_qwerty();
    let dvorak = dvorak();
    write_keymap(&keymap_dir.join("us-qwerty.bin"), &us)?;
    write_keymap(&keymap_dir.join("uk-qwerty.bin"), &uk)?;
    write_keymap(&keymap_dir.join("dvorak.bin"), &dvorak)?;
    println!(
        "[xtask] generated 3 PMKM keymaps in {}",
        keymap_dir.display()
    );
    Ok(())
}

fn repo_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir().map_err(|error| error.to_string())?;
    loop {
        if dir.join("Cargo.toml").is_file() && dir.join("crates/display-server").is_dir() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err("could not find PMos repository root".to_string());
        }
    }
}

fn write_keymap(path: &std::path::Path, entries: &[Entry]) -> Result<()> {
    let count = u16::try_from(entries.len()).map_err(|_| "too many keymap entries")?;
    let mut bytes = Vec::with_capacity(8 + entries.len() * 11);
    bytes.extend_from_slice(b"PMKM");
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    for (scancode, unshifted, shifted) in entries {
        bytes.push(*scancode);
        bytes.extend_from_slice(&unshifted.to_le_bytes());
        bytes.extend_from_slice(&shifted.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    fs::write(path, bytes).map_err(|error| format!("{}: {error}", path.display()))
}

fn us_qwerty() -> Vec<Entry> {
    let mut entries = Vec::new();
    for (index, ch) in ('a'..='z').enumerate() {
        entries.push((
            0x04 + index as u8,
            ch as u32,
            ch.to_ascii_uppercase() as u32,
        ));
    }
    for (index, (plain, shifted)) in "1234567890".chars().zip("!@#$%^&*()".chars()).enumerate() {
        entries.push((0x1e + index as u8, plain as u32, shifted as u32));
    }
    entries.extend([
        (0x28, '\r' as u32, '\r' as u32),
        (0x2a, '\u{8}' as u32, '\u{8}' as u32),
        (0x2b, '\t' as u32, '\t' as u32),
        (0x2c, ' ' as u32, ' ' as u32),
        (0x2d, '-' as u32, '_' as u32),
        (0x2e, '=' as u32, '+' as u32),
        (0x2f, '[' as u32, '{' as u32),
        (0x30, ']' as u32, '}' as u32),
        (0x31, '\\' as u32, '|' as u32),
        (0x33, ';' as u32, ':' as u32),
        (0x34, '\'' as u32, '"' as u32),
        (0x35, '`' as u32, '~' as u32),
        (0x36, ',' as u32, '<' as u32),
        (0x37, '.' as u32, '>' as u32),
        (0x38, '/' as u32, '?' as u32),
        (0xe0, 0, 0),
        (0xe1, 0, 0),
        (0xe4, 0, 0),
        (0xe5, 0, 0),
    ]);
    entries
}

fn uk_qwerty() -> Vec<Entry> {
    let mut entries = us_qwerty();
    replace(&mut entries, 0x1f, '2', '"');
    replace(&mut entries, 0x20, '3', '£');
    replace(&mut entries, 0x34, '\'', '@');
    replace(&mut entries, 0x35, '`', '¬');
    entries
}

fn dvorak() -> Vec<Entry> {
    let mut entries = us_qwerty();
    let physical_letters = 0x04u8..=0x1d;
    let dvorak_letters = "axje.uidchtnmbrl'poygk,qf;";
    for (scancode, ch) in physical_letters.zip(dvorak_letters.chars()) {
        let shifted = if ch.is_ascii_alphabetic() {
            ch.to_ascii_uppercase()
        } else {
            match ch {
                '\'' => '"',
                ',' => '<',
                '.' => '>',
                ';' => ':',
                other => other,
            }
        };
        replace(&mut entries, scancode, ch, shifted);
    }
    replace(&mut entries, 0x2f, '/', '?');
    replace(&mut entries, 0x2e, '=', '+');
    replace(&mut entries, 0x33, 's', 'S');
    replace(&mut entries, 0x34, '-', '_');
    replace(&mut entries, 0x36, 'w', 'W');
    replace(&mut entries, 0x37, 'v', 'V');
    replace(&mut entries, 0x38, 'z', 'Z');
    entries
}

fn replace(entries: &mut [Entry], scancode: u8, plain: char, shifted: char) {
    let entry = entries
        .iter_mut()
        .find(|entry| entry.0 == scancode)
        .unwrap_or_else(|| panic!("missing scancode {scancode:#x}"));
    entry.1 = plain as u32;
    entry.2 = shifted as u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(entries: &[Entry], scancode: u8) -> (u32, u32) {
        let entry = entries.iter().find(|entry| entry.0 == scancode).unwrap();
        (entry.1, entry.2)
    }

    #[test]
    fn layouts_are_distinct_and_pin_representative_keys() {
        assert_eq!(mapping(&us_qwerty(), 0x1f), ('2' as u32, '@' as u32));
        assert_eq!(mapping(&uk_qwerty(), 0x1f), ('2' as u32, '"' as u32));
        assert_eq!(mapping(&uk_qwerty(), 0x20), ('3' as u32, '£' as u32));
        assert_eq!(mapping(&dvorak(), 0x05), ('x' as u32, 'X' as u32));
        assert_eq!(mapping(&dvorak(), 0x16), ('o' as u32, 'O' as u32));
        assert_ne!(us_qwerty(), uk_qwerty());
        assert_ne!(us_qwerty(), dvorak());
    }
}
