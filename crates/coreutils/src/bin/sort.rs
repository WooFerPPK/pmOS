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
//! `-k N` selects field N (1-indexed) as the comparison key (POSIX
//! `--key=N` long form deferred). The line is whitespace-tokenized
//! via `str::split_whitespace` (POSIX `[[:blank:]]` runs collapse to
//! a single separator); the Nth token IS the key, with tokens after
//! N ignored for sorting. Both `-k N` (space-separated) and `-kN`
//! (no space) forms are accepted, mirroring `-o FILE` shape. Lines
//! with fewer than N tokens treat the missing key as the empty
//! string (which sorts first among empty-key lines, preserved by
//! stable sort). The OUTPUT is the FULL ORIGINAL LINE — `-k` is a
//! key SELECTOR, not a field PROJECTOR; only the comparison position
//! reflects the chosen field. Composition order is select_field →
//! trim → filter (printable / dictionary) → fold, so `-k` composes
//! with `-n` (numeric on the field), `-f` (case-fold the field),
//! `-r` (reverse the field-key sort), `-u` (dedupe by field key),
//! `-c`/`-C` (check by field key), `-b` (trim leading blanks of the
//! field — usually a no-op since `split_whitespace` already trims),
//! `-d` (dictionary filter on the field), `-i` (printable filter on
//! the field). Invalid `-k` parameters — `-k 0` (zero is invalid;
//! fields are 1-indexed), `-k -1` (negative), `-k foo` (non-integer),
//! `-k` with no following argv value (placeholder `<missing>`),
//! `-k ""` (empty string) — all write
//! `sort: invalid field specification: <value>\n` to stderr and exit 1.
//! Explicitly deferred (out of slice scope, future `-k` follow-ups):
//! `-k N,M` start-end range form; `-k N.C` start-field-plus-character-offset
//! form; the full `-k F[.C][OPTS][,F[.C][OPTS]]` POSIX notation; per-key
//! sort options like `-k 2n,3` (per-key flag overrides); `-t SEP` custom
//! field separator (currently hardcoded to whitespace); `--key=` long
//! form alias.
//! `-V` version sort (POSIX `--version-sort` long form deferred): walks
//! both compared strings simultaneously, classifying each position as a
//! digit run (consecutive `[0-9]`) or a non-digit run (everything else),
//! and compares run-by-run. Digit runs parse to `u64` and compare
//! numerically so `file2` < `file10` < `file100` (the canonical use case
//! that lex sort gets wrong: lex puts `file10` < `file2` because the byte
//! `1` precedes byte `2`). Non-digit runs compare lex (byte-order ASCII).
//! Equal-value digit-run tiebreak: the run with FEWER characters
//! (i.e. fewer leading zeros) sorts FIRST — `001` < `01` < `1` since they
//! all parse to value 1 but the shorter representation wins; this matches
//! the GNU convention. When the two cursors land on different run TYPES
//! (one digit, one non-digit), the v1 simplification compares byte-order
//! on the first differing byte (matches GNU output for plain ASCII inputs
//! without needing the full GNU algorithm). Equal common prefix: the
//! shorter string sorts first. `version_compare` is the dedicated helper;
//! a third `Key::Version(String)` variant carries the raw string and
//! delegates `cmp` to `version_compare` (the `Ord` derive walks variant
//! tags first then payloads, so `Numeric` < `Bytes` < `Version` for
//! cross-variant comparisons — never expected to occur because a single
//! sort run picks ONE key kind for every line). Numeric overflow: if a
//! digit run is longer than 20 chars (u64 max is 20 digits) the parse
//! falls back to lex compare for THAT run pair (no panic, safe semantic
//! for pathological inputs — pinned by `dash_V_overflow_falls_back_to_lex`).
//! Composition: `-V` dispatches AFTER `-n` (numeric dominates — `-Vn`
//! and `-nV` both produce numeric sort, pinned by
//! `dash_Vn_numeric_dominates`) but BEFORE the filter / fold / blanks
//! paths (since version_compare consumes the raw string as one indivisible
//! unit — running the dictionary or printable filter on it first would
//! drop the very dots/dashes that delimit version segments). `-Vr`
//! reverses the version-sorted order; `-Vu` dedupes by version-key
//! (two strings that compare equal under version sort collapse); `-Vc` /
//! `-VC` checks the input is in version order; `-V -k N` sorts by the
//! version comparator on the Nth field; `-V -f` is allowed and case-folds
//! the raw string before version_compare runs (digit runs are unaffected
//! by case-fold so the numeric compare path is identical; only the
//! non-digit runs see the fold). Explicitly deferred (out of slice scope,
//! future `-V` follow-ups): the full GNU file-extension special case
//! (e.g. `foo.tar.gz` vs `foo.tar.bz2` where GNU treats `.gz` and `.bz2`
//! as suffix metadata sorted last); locale-aware non-digit compare (v1
//! stays C / POSIX locale = byte-order); the `--version-sort` long-form
//! alias; the `~` (tilde) Debian-version special case where `~` sorts
//! BEFORE everything (so `0.9~rc1` < `0.9` < `1.0`); BigInt /
//! arbitrary-precision digit-run support (overflow falls back to lex per
//! spec above, no extension to u128 in v1).
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
    let mut version_sort = false;
    let mut output: Option<String> = None;
    let mut key_field: Option<usize> = None;
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
                    'V' => version_sort = true,
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
                    'k' => {
                        let value_opt: Option<String> = if ci < cluster.len() {
                            let rest: String = cluster[ci..].iter().collect();
                            ci = cluster.len();
                            Some(rest)
                        } else if idx < args.len() {
                            let v = args[idx].clone();
                            idx += 1;
                            Some(v)
                        } else {
                            None
                        };
                        let parsed = value_opt
                            .as_deref()
                            .and_then(|v| v.parse::<usize>().ok().filter(|&n| n > 0));
                        match parsed {
                            Some(n) => key_field = Some(n),
                            None => {
                                let display = value_opt.as_deref().unwrap_or("<missing>");
                                let _ = writeln!(
                                    io::stderr(),
                                    "sort: invalid field specification: {display}"
                                );
                                return ExitCode::from(1);
                            }
                        }
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
            key_field,
            version_sort,
            reverse,
            unique,
            silent_check,
        );
    }

    if key_field.is_some() {
        lines.sort_by_key(|line| {
            key_for(
                line,
                numeric,
                fold,
                ignore_blanks,
                ignore_nonprinting,
                dictionary_order,
                key_field,
                version_sort,
            )
        });
    } else if numeric {
        lines.sort_by_key(|line| parse_leading_int(line));
    } else if version_sort {
        lines.sort_by_key(|line| Key::Version(maybe_fold_for_version(line, fold)));
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
        if key_field.is_some() {
            lines.dedup_by_key(|line| {
                key_for(
                    line,
                    numeric,
                    fold,
                    ignore_blanks,
                    ignore_nonprinting,
                    dictionary_order,
                    key_field,
                    version_sort,
                )
            });
        } else if numeric {
            lines.dedup();
        } else if version_sort {
            lines.dedup_by_key(|line| Key::Version(maybe_fold_for_version(line, fold)));
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

#[derive(PartialEq, Eq)]
enum Key {
    Numeric(i64),
    Bytes(Vec<u8>),
    Version(String),
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Key::Numeric(a), Key::Numeric(b)) => a.cmp(b),
            (Key::Bytes(a), Key::Bytes(b)) => a.cmp(b),
            (Key::Version(a), Key::Version(b)) => version_compare(a, b),
            (Key::Numeric(_), _) => std::cmp::Ordering::Less,
            (_, Key::Numeric(_)) => std::cmp::Ordering::Greater,
            (Key::Bytes(_), _) => std::cmp::Ordering::Less,
            (_, Key::Bytes(_)) => std::cmp::Ordering::Greater,
        }
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn key_for(
    line: &str,
    numeric: bool,
    fold: bool,
    ignore_blanks: bool,
    ignore_nonprinting: bool,
    dictionary_order: bool,
    key_field: Option<usize>,
    version_sort: bool,
) -> Key {
    let base: &str = match key_field {
        Some(n) => select_field(line, n),
        None => line,
    };
    if numeric {
        Key::Numeric(parse_leading_int(base))
    } else if version_sort {
        Key::Version(maybe_fold_for_version(base, fold))
    } else if dictionary_order {
        Key::Bytes(filter_dictionary_then_maybe_fold(base, ignore_blanks, fold))
    } else if ignore_nonprinting {
        Key::Bytes(filter_printable_then_maybe_fold(base, ignore_blanks, fold))
    } else {
        let s = maybe_trim(base, ignore_blanks);
        if fold {
            Key::Bytes(fold_to_upper_bytes(s))
        } else {
            Key::Bytes(s.as_bytes().to_vec())
        }
    }
}

fn select_field(line: &str, field: usize) -> &str {
    line.split_whitespace().nth(field - 1).unwrap_or("")
}

fn check_sorted(
    lines: &[String],
    numeric: bool,
    fold: bool,
    ignore_blanks: bool,
    ignore_nonprinting: bool,
    dictionary_order: bool,
    key_field: Option<usize>,
    version_sort: bool,
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
            key_field,
            version_sort,
        );
        let curr = key_for(
            &lines[i],
            numeric,
            fold,
            ignore_blanks,
            ignore_nonprinting,
            dictionary_order,
            key_field,
            version_sort,
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

fn maybe_fold_for_version(line: &str, fold: bool) -> String {
    if fold {
        line.bytes()
            .map(|b| if b.is_ascii_lowercase() { b - 32 } else { b })
            .map(char::from)
            .collect()
    } else {
        line.to_string()
    }
}

fn version_compare(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < ab.len() && j < bb.len() {
        let a_is_digit = ab[i].is_ascii_digit();
        let b_is_digit = bb[j].is_ascii_digit();
        if a_is_digit && b_is_digit {
            let a_end = digit_run_end(ab, i);
            let b_end = digit_run_end(bb, j);
            let a_run = &ab[i..a_end];
            let b_run = &bb[j..b_end];
            let av = parse_u64_run(a_run);
            let bv = parse_u64_run(b_run);
            match (av, bv) {
                (Some(av), Some(bv)) => {
                    let value_ord = av.cmp(&bv);
                    if value_ord != Ordering::Equal {
                        return value_ord;
                    }
                    let len_ord = a_run.len().cmp(&b_run.len());
                    if len_ord != Ordering::Equal {
                        return len_ord;
                    }
                }
                _ => {
                    let lex_ord = a_run.cmp(b_run);
                    if lex_ord != Ordering::Equal {
                        return lex_ord;
                    }
                }
            }
            i = a_end;
            j = b_end;
        } else {
            let byte_ord = ab[i].cmp(&bb[j]);
            if byte_ord != Ordering::Equal {
                return byte_ord;
            }
            i += 1;
            j += 1;
        }
    }
    ab.len().cmp(&bb.len())
}

fn digit_run_end(bytes: &[u8], start: usize) -> usize {
    let mut k = start;
    while k < bytes.len() && bytes[k].is_ascii_digit() {
        k += 1;
    }
    k
}

fn parse_u64_run(run: &[u8]) -> Option<u64> {
    if run.len() > 20 {
        return None;
    }
    let mut value: u64 = 0;
    for &b in run {
        let digit = u64::from(b - b'0');
        value = value.checked_mul(10)?.checked_add(digit)?;
    }
    Some(value)
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
