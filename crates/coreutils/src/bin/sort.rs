//! T146 follow-up — POSIX-ish `sort`: read each input (stdin when no
//! file args, otherwise every path in turn) into a `Vec<String>` of
//! lines, concatenate the results into a single bucket, sort with the
//! Rust default `Vec::sort` (lexicographic byte-order — matches POSIX
//! `sort` under the C / POSIX locale), and emit each line on its own
//! `\n`-terminated row. Open/read errors write `sort: <path>: <err>`
//! to stderr, set a had-error flag, and continue with remaining
//! files. Exit 0 on full success, 1 on any per-file failure.
//!
//! Line-splitting semantics: `bytes.split('\n')` over the file as
//! UTF-8; an exact final empty segment caused by a trailing `\n` is
//! dropped so a file ending in `c\nb\na\n` contributes three lines
//! `c` / `b` / `a` (not four with a phantom empty tail). A file
//! without a trailing newline keeps every segment, so `c\nb\na`
//! contributes the same three lines.
//!
//! Flags mirror grep's POSIX-style short-flag clustering (commit
//! `f667018`): `-r` reverses the sorted result via `Vec::reverse`;
//! `-u` collapses adjacent duplicates via `Vec::dedup` after the
//! sort (since the input is sorted, dedup gives unique entries
//! globally); `-n` switches the comparison key from byte order to
//! the leading signed integer parsed via `parse_leading_int` (lines
//! whose leading non-whitespace token isn't numeric compare as 0,
//! so they cluster among themselves in input order — POSIX leaves
//! that order unspecified for v1); `-f` folds ASCII lowercase to
//! uppercase for the comparison key only (output bytes are the
//! original input bytes — only the sort/dedup KEY is folded), via
//! `fold_to_upper_bytes`. Non-ASCII bytes pass through unchanged
//! (no Unicode case-folding in v1). When both `-n` and `-f` are
//! set, numeric dominates: case-folding is a no-op for the
//! integer-parsing key. `-c` switches sort into check-only mode:
//! emit no stdout, walk consecutive line pairs computing the same
//! comparison key the sort path would have used, and on the FIRST
//! out-of-order pair write `sort: -:<line-num>: disorder: <line>\n`
//! to stderr (POSIX-conformant diagnostic; the `-:` prefix is
//! POSIX's "stdin file name" since v1 sort doesn't accept file args
//! in check mode either) and exit 1; otherwise exit 0 with no
//! output. `-c` composes with the comparison flags: `-cn` checks
//! numeric ordering, `-cf` checks fold ordering, `-cr` checks
//! descending order, `-cu` ALSO checks uniqueness (equal adjacent
//! keys count as a violation, since under `-u` the input would have
//! been collapsed). `-C` is the POSIX-2008 silent-check variant
//! (`--check=quiet` long form): identical check semantics to `-c`
//! (same composition with `-n` / `-f` / `-b` / `-i` / `-d` / `-r` /
//! `-u`, same exit code 0/1, same `check_sorted` code path) BUT
//! suppresses the disorder diagnostic on stderr — designed for
//! script-friendly conditionals like `if sort -C foo; then ...`
//! where stderr noise would pollute the calling context. When BOTH
//! `-c` and `-C` are passed (e.g. `-cC` or `-Cc`), the silent
//! semantic dominates: silent is the more restrictive output choice
//! and asking for silence at any point in the cluster is honoured —
//! pinned by `dash_capital_c_with_lowercase_c_silent_dominates`.
//! `-b` ignores leading blanks (POSIX `[[:blank:]]`
//! — space + horizontal tab only) when computing the comparison key
//! for the lex / fold comparators (the trim is COMPARISON-ONLY:
//! original line bytes are emitted unchanged on stdout, mirroring
//! the `-f` case-preservation invariant); the trim is leading-only
//! (trailing whitespace is NOT trimmed). `-b` is effectively a
//! no-op when `-n` is set since `parse_leading_int` already trims
//! leading whitespace internally — the test pins this. `-bf` trims
//! THEN folds (`   Apple` and `apple` both reduce to `APPLE` for
//! the key); `-bu` dedupes after trim (`   apple` and `apple`
//! collapse to one), via `trim_leading_blanks`. `-i` filters out
//! non-printable bytes from the comparison key (POSIX `[[:print:]]`
//! is bytes 32..=126 inclusive — printable ASCII including space,
//! no control characters, no DEL=127, no high-bit bytes 128..=255);
//! the filter is COMPARISON-ONLY (original line bytes preserved on
//! output, mirroring `-b` and `-f`), via `filter_printable`. `-i`
//! composes with the existing comparators: `-if` filters THEN folds
//! (both transforms are byte-level so order is well-defined); `-ib`
//! pairs with the leading-blank trim (both are byte-filters and
//! commute in practice — verified by test); `-iu` dedupes after the
//! filter; `-in` is a no-op since `parse_leading_int` only consumes
//! ASCII digits which are all printable; `-ic` checks ordering with
//! filtered keys. `-d` filters the comparison key down to POSIX
//! `[[:blank:]]` (space + horizontal tab) plus POSIX `[[:alnum:]]`
//! (ASCII letters A-Z / a-z plus digits 0-9) — STRICTER than `-i`
//! since `-d` also drops punctuation that `-i` keeps; the filter is
//! COMPARISON-ONLY (original line bytes preserved on output, mirroring
//! `-b` / `-f` / `-i`), via `filter_dictionary`. `-d` composes with
//! the existing comparators: `-df` filters THEN folds; `-db` pairs
//! with the leading-blank trim (since blanks are kept by the
//! dictionary set the trim is mostly subsumed but composes cleanly);
//! `-du` dedupes after filter; `-dn` is dominated by numeric since
//! digits are kept by `-d`; `-dc` checks with dictionary keys. When
//! BOTH `-d` and `-i` are set, `-d` dominates: `-d`'s output is a
//! strict subset of `-i`'s, so applying `-i` after `-d` would be a
//! no-op — the dispatch checks `dictionary_order` first to pin this
//! invariant. `-ru` / `-ur` /
//! `-nr` / `-nu` / `-nru` / `-fr` / `-fu` / `-fnu` / `-cn` / `-cf` /
//! `-cr` / `-cu` / `-b` / `-bf` / `-bu` / `-bfu` / `-cb` / `-i` /
//! `-if` / `-iu` / `-ib` / `-ic` / `-d` / `-df` / `-du` / `-db` /
//! `-dc` / `-di` / `-C` / `-Cn` / `-Cf` / `-Cr` / `-Cu` / `-Cb` /
//! `-Ci` / `-Cd` / `-cC` (silent dominates) etc. apply the chosen combination.
//! `-o FILE` redirects the sorted output to FILE instead of stdout
//! (POSIX `--output=FILE` long form). Both `-o FILE` (space-separated)
//! and `-oFILE` (no space) forms are accepted; the parameter is the
//! NEXT arg or the rest of the cluster, NOT an input path. The output
//! file is opened with truncate semantics (existing content discarded).
//! Critical invariant: input is fully READ then sorted IN MEMORY
//! before the output file is ever opened, so the in-place sort
//! `sort -o foo foo` works cleanly — `foo` is read first, sorted
//! second, then truncated and rewritten with the sorted result.
//! File-write errors (parent dir missing, permission denied, disk
//! full) write `sort: <path>: <error>\n` to stderr and exit 1.
//! `-o` composes with every other sort flag: `sort -no FILE` writes
//! the numeric-sorted result to FILE; `sort -uo FILE` writes the
//! unique result; `sort -fo FILE` writes the case-folded result; etc.
//! The ONE rejected combination is `-co` / `-Co` (output-redirect
//! with check-only mode) — check mode produces no sorted output to
//! write, so combining `-c`/`-C` with `-o` is a usage error:
//! `sort: cannot combine -c and -o` (or `-C and -o`) on stderr, exit 1,
//! no file created.
//! Unknown flags write `sort: unknown flag: <flag>` to stderr and
//! exit 2 (matching grep's open-error/usage exit code).
//!
//! Pattern precedent: `crates/coreutils/src/bin/{cat,grep,wc,head}.rs`.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut reverse = false;
    let mut unique = false;
    let mut numeric = false;
    let mut fold = false;
    let mut check_only = false;
    let mut silent_check = false;
    let mut ignore_blanks = false;
    let mut ignore_nonprinting = false;
    let mut dictionary_order = false;
    let mut output: Option<String> = None;
    let mut paths: Vec<String> = Vec::new();
    let mut sep_seen = false;
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx].clone();
        idx += 1;
        if !sep_seen && arg == "--" {
            sep_seen = true;
            continue;
        }
        if !sep_seen && arg.starts_with('-') && arg != "-" {
            let cluster: Vec<char> = arg[1..].chars().collect();
            let mut ci = 0usize;
            while ci < cluster.len() {
                let ch = cluster[ci];
                ci += 1;
                match ch {
                    'r' => reverse = true,
                    'u' => unique = true,
                    'n' => numeric = true,
                    'f' => fold = true,
                    'c' => check_only = true,
                    'C' => silent_check = true,
                    'b' => ignore_blanks = true,
                    'i' => ignore_nonprinting = true,
                    'd' => dictionary_order = true,
                    'o' => {
                        let value = if ci < cluster.len() {
                            let rest: String = cluster[ci..].iter().collect();
                            ci = cluster.len();
                            rest
                        } else if idx < args.len() {
                            let v = args[idx].clone();
                            idx += 1;
                            v
                        } else {
                            let _ = writeln!(io::stderr(), "sort: option requires an argument: -o");
                            return ExitCode::from(2);
                        };
                        output = Some(value);
                    }
                    _ => {
                        let _ = writeln!(io::stderr(), "sort: unknown flag: {arg}");
                        return ExitCode::from(2);
                    }
                }
            }
        } else {
            paths.push(arg);
        }
    }

    if output.is_some() && check_only {
        let _ = writeln!(io::stderr(), "sort: cannot combine -c and -o");
        return ExitCode::from(1);
    }
    if output.is_some() && silent_check {
        let _ = writeln!(io::stderr(), "sort: cannot combine -C and -o");
        return ExitCode::from(1);
    }

    let mut lines: Vec<String> = Vec::new();
    let mut had_error = false;

    if paths.is_empty() {
        let mut buf = String::new();
        match io::stdin().lock().read_to_string(&mut buf) {
            Ok(_) => append_lines(&buf, &mut lines),
            Err(e) => {
                let _ = writeln!(io::stderr(), "sort: stdin: {e}");
                had_error = true;
            }
        }
    } else {
        for path in &paths {
            match fs::read_to_string(path) {
                Ok(text) => append_lines(&text, &mut lines),
                Err(e) => {
                    let _ = writeln!(io::stderr(), "sort: {path}: {e}");
                    had_error = true;
                }
            }
        }
    }

    if check_only || silent_check {
        return check_sorted(
            &lines,
            numeric,
            fold,
            ignore_blanks,
            ignore_nonprinting,
            dictionary_order,
            reverse,
            unique,
            silent_check,
        );
    }

    if numeric {
        lines.sort_by_key(|line| parse_leading_int(line));
    } else if dictionary_order {
        lines.sort_by_key(|line| filter_dictionary_then_maybe_fold(line, ignore_blanks, fold));
    } else if ignore_nonprinting {
        lines.sort_by_key(|line| filter_printable_then_maybe_fold(line, ignore_blanks, fold));
    } else if fold {
        lines.sort_by_key(|line| fold_to_upper_bytes(maybe_trim(line, ignore_blanks)));
    } else if ignore_blanks {
        lines.sort_by_key(|line| trim_leading_blanks(line).as_bytes().to_vec());
    } else {
        lines.sort();
    }
    if reverse {
        lines.reverse();
    }
    if unique {
        if numeric {
            lines.dedup();
        } else if dictionary_order {
            lines.dedup_by_key(|line| filter_dictionary_then_maybe_fold(line, ignore_blanks, fold));
        } else if ignore_nonprinting {
            lines.dedup_by_key(|line| filter_printable_then_maybe_fold(line, ignore_blanks, fold));
        } else if fold {
            lines.dedup_by_key(|line| fold_to_upper_bytes(maybe_trim(line, ignore_blanks)));
        } else if ignore_blanks {
            lines.dedup_by_key(|line| trim_leading_blanks(line).as_bytes().to_vec());
        } else {
            lines.dedup();
        }
    }

    if let Some(path) = output.as_deref() {
        match fs::File::create(path) {
            Ok(mut file) => {
                for line in &lines {
                    if let Err(e) = writeln!(file, "{line}") {
                        let _ = writeln!(io::stderr(), "sort: {path}: {e}");
                        return ExitCode::from(1);
                    }
                }
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "sort: {path}: {e}");
                return ExitCode::from(1);
            }
        }
    } else {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        for line in &lines {
            if writeln!(out, "{line}").is_err() {
                had_error = true;
                break;
            }
        }
    }

    if had_error { ExitCode::from(1) } else { ExitCode::from(0) }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Numeric(i64),
    Bytes(Vec<u8>),
}

fn key_for(
    line: &str,
    numeric: bool,
    fold: bool,
    ignore_blanks: bool,
    ignore_nonprinting: bool,
    dictionary_order: bool,
) -> Key {
    if numeric {
        Key::Numeric(parse_leading_int(line))
    } else if dictionary_order {
        Key::Bytes(filter_dictionary_then_maybe_fold(line, ignore_blanks, fold))
    } else if ignore_nonprinting {
        Key::Bytes(filter_printable_then_maybe_fold(line, ignore_blanks, fold))
    } else {
        let s = maybe_trim(line, ignore_blanks);
        if fold {
            Key::Bytes(fold_to_upper_bytes(s))
        } else {
            Key::Bytes(s.as_bytes().to_vec())
        }
    }
}

fn check_sorted(
    lines: &[String],
    numeric: bool,
    fold: bool,
    ignore_blanks: bool,
    ignore_nonprinting: bool,
    dictionary_order: bool,
    reverse: bool,
    unique: bool,
    silent: bool,
) -> ExitCode {
    use std::cmp::Ordering;
    for i in 1..lines.len() {
        let prev = key_for(
            &lines[i - 1],
            numeric,
            fold,
            ignore_blanks,
            ignore_nonprinting,
            dictionary_order,
        );
        let curr = key_for(
            &lines[i],
            numeric,
            fold,
            ignore_blanks,
            ignore_nonprinting,
            dictionary_order,
        );
        let ord = prev.cmp(&curr);
        let violation = match (reverse, unique) {
            (false, false) => ord == Ordering::Greater,
            (false, true) => ord != Ordering::Less,
            (true, false) => ord == Ordering::Less,
            (true, true) => ord != Ordering::Greater,
        };
        if violation {
            if !silent {
                let _ = writeln!(
                    io::stderr(),
                    "sort: -:{}: disorder: {}",
                    i + 1,
                    lines[i]
                );
            }
            return ExitCode::from(1);
        }
    }
    ExitCode::from(0)
}

fn append_lines(text: &str, sink: &mut Vec<String>) {
    let mut parts: Vec<&str> = text.split('\n').collect();
    if matches!(parts.last(), Some(&"")) {
        parts.pop();
    }
    for p in parts {
        sink.push(p.to_string());
    }
}

fn fold_to_upper_bytes(s: &str) -> Vec<u8> {
    s.bytes()
        .map(|b| if b.is_ascii_lowercase() { b - 32 } else { b })
        .collect()
}

fn trim_leading_blanks(s: &str) -> &str {
    s.trim_start_matches([' ', '\t'])
}

fn maybe_trim(s: &str, ignore_blanks: bool) -> &str {
    if ignore_blanks { trim_leading_blanks(s) } else { s }
}

fn filter_printable(s: &str) -> Vec<u8> {
    s.bytes().filter(|b| (b' '..=b'~').contains(b)).collect()
}

fn filter_printable_then_maybe_fold(line: &str, ignore_blanks: bool, fold: bool) -> Vec<u8> {
    let trimmed = maybe_trim(line, ignore_blanks);
    let filtered = filter_printable(trimmed);
    if fold {
        filtered
            .iter()
            .map(|b| if b.is_ascii_lowercase() { b - 32 } else { *b })
            .collect()
    } else {
        filtered
    }
}

fn filter_dictionary(s: &str) -> Vec<u8> {
    s.bytes()
        .filter(|b| {
            matches!(
                b,
                b' ' | b'\t'
                    | b'0'..=b'9'
                    | b'A'..=b'Z'
                    | b'a'..=b'z'
            )
        })
        .collect()
}

fn filter_dictionary_then_maybe_fold(line: &str, ignore_blanks: bool, fold: bool) -> Vec<u8> {
    let trimmed = maybe_trim(line, ignore_blanks);
    let filtered = filter_dictionary(trimmed);
    if fold {
        filtered
            .iter()
            .map(|b| if b.is_ascii_lowercase() { b - 32 } else { *b })
            .collect()
    } else {
        filtered
    }
}

fn parse_leading_int(s: &str) -> i64 {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    let mut negative = false;
    if i < bytes.len() && (bytes[i] == b'-' || bytes[i] == b'+') {
        negative = bytes[i] == b'-';
        i += 1;
    }
    let digits_start = i;
    let mut value: i64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        let digit = i64::from(bytes[i] - b'0');
        value = value
            .saturating_mul(10)
            .saturating_add(if negative { -digit } else { digit });
        i += 1;
    }
    if i == digits_start {
        return 0;
    }
    value
}
