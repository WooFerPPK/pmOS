//! T146 follow-up — POSIX-ish `tr`: read stdin, translate, delete, or
//! squeeze characters according to one or two SET arguments, write to
//! stdout. No file args (POSIX `tr` is always stdin-only). Exit 0 on
//! success, 1 on usage / unknown-flag error.
//!
//! Modes:
//!   `tr SET1 SET2` — translate each char in SET1 to the corresponding
//!   char in SET2 (1-to-1 char-by-char mapping, in source order). If
//!   SET2 is shorter than SET1, the last char of SET2 pads the
//!   remaining SET1 entries (POSIX-required behaviour). Chars outside
//!   SET1 pass through unchanged.
//!   `tr -d SET1` — delete every char in SET1; everything else passes
//!   through. POSIX forbids SET2 with `-d`; this implementation
//!   matches that and rejects `tr -d SET1 SET2` as a usage error.
//!   `tr -s SET1` — squeeze repeated runs of any char in SET1 down to a
//!   single occurrence; chars outside SET1 pass through unchanged.
//!   Standalone form only: `tr -s SET1 SET2` (post-translate squeeze)
//!   and `tr -ds SET1 SET2` (delete + squeeze) are **explicitly
//!   deferred** to future slices, as is the `tr SET1 SET2` +
//!   post-translate-squeeze interaction. Combining `-s` with `-d` is
//!   rejected as a usage error in v1; passing a SET2 to `-s` is also
//!   a usage error.
//!
//! SETs are LITERAL char sequences in v1 (NOT regex). Range syntax
//! (`a-z` → expanded to `abc...xyz`) is **explicitly deferred** to a
//! future slice — `tr 'a-z' 'A-Z'` would treat `-` literally, mapping
//! a→a, -→-, z→Z. Backslash escapes (`\n`, `\t`, `\\`, `\r`, `\NNN`
//! octal) are also deferred — v1 takes input bytes literally as the
//! shell delivers them.
//!
//! Errors: wrong arg count writes `tr: usage: tr SET1 SET2  or  tr -d
//! SET1  or  tr -s SET1` to stderr and exits 1; unknown flag writes
//! `tr: unknown flag: <flag>` to stderr and exits 1; SET2 with `-d`
//! or `-s`, or `-d` combined with `-s`, also exits 1 with a usage
//! diagnostic.
//!
//! Pattern precedent: `crates/coreutils/src/bin/{cat,grep,sort,head}.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{self, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut delete = false;
    let mut squeeze = false;
    let mut sets: Vec<String> = Vec::new();
    let mut sep_seen = false;
    for arg in args {
        if !sep_seen && arg == "--" {
            sep_seen = true;
            continue;
        }
        if !sep_seen && arg.starts_with('-') && arg != "-" && !arg.is_empty() {
            for ch in arg[1..].chars() {
                match ch {
                    'd' => delete = true,
                    's' => squeeze = true,
                    _ => {
                        let _ = writeln!(io::stderr(), "tr: unknown flag: {arg}");
                        return ExitCode::from(1);
                    }
                }
            }
        } else {
            sets.push(arg);
        }
    }

    let usage = "tr: usage: tr SET1 SET2  or  tr -d SET1  or  tr -s SET1";

    if delete && squeeze {
        let _ = writeln!(io::stderr(), "{usage}");
        return ExitCode::from(1);
    }

    if delete || squeeze {
        if sets.len() != 1 {
            let _ = writeln!(io::stderr(), "{usage}");
            return ExitCode::from(1);
        }
    } else if sets.len() != 2 {
        let _ = writeln!(io::stderr(), "{usage}");
        return ExitCode::from(1);
    }

    let mut input = String::new();
    if let Err(e) = io::stdin().lock().read_to_string(&mut input) {
        let _ = writeln!(io::stderr(), "tr: stdin: {e}");
        return ExitCode::from(1);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();

    if delete {
        let drop: BTreeSet<char> = sets[0].chars().collect();
        let filtered: String = input.chars().filter(|c| !drop.contains(c)).collect();
        if out.write_all(filtered.as_bytes()).is_err() {
            return ExitCode::from(1);
        }
    } else if squeeze {
        let set: BTreeSet<char> = sets[0].chars().collect();
        let mut buf = String::with_capacity(input.len());
        let mut prev: Option<char> = None;
        for c in input.chars() {
            if set.contains(&c) && prev == Some(c) {
                continue;
            }
            buf.push(c);
            prev = Some(c);
        }
        if out.write_all(buf.as_bytes()).is_err() {
            return ExitCode::from(1);
        }
    } else {
        let map = build_translation_map(&sets[0], &sets[1]);
        let mut buf = String::with_capacity(input.len());
        for c in input.chars() {
            buf.push(*map.get(&c).unwrap_or(&c));
        }
        if out.write_all(buf.as_bytes()).is_err() {
            return ExitCode::from(1);
        }
    }

    ExitCode::from(0)
}

fn build_translation_map(set1: &str, set2: &str) -> BTreeMap<char, char> {
    let mut map = BTreeMap::new();
    let s1: Vec<char> = set1.chars().collect();
    let s2: Vec<char> = set2.chars().collect();
    let pad = s2.last().copied();
    for (i, src) in s1.iter().enumerate() {
        let dst = if i < s2.len() {
            s2[i]
        } else if let Some(p) = pad {
            p
        } else {
            continue;
        };
        map.insert(*src, dst);
    }
    map
}
