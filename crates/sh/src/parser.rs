//! Pipeline + redirection parser (T142).
//!
//! Takes the quote-aware tokeniser's `Vec<Vec<TokenPart>>`
//! output and produces a [`Pipeline`] structure: one or more
//! [`Stage`]s connected by `|`, optional per-stage
//! [`Redirection`]s, and a `background` flag set by a
//! trailing `&`.
//!
//! Operator detection runs at the **word** level, not the
//! **byte** level: a word whose only segment is
//! [`TokenPart::Operator`] (emitted by the tokenizer when
//! it sees a bare `|` / `<` / `>` / `>>` / `&` outside any
//! quoting) is treated as an operator. A literal `|` or `>`
//! inside `'...'` or `"..."` round-trips as a quoted segment
//! and the parser sees it as a regular argv word — so `echo
//! '|' foo` is one stage with three argv words `["echo", "|",
//! "foo"]`, NOT a pipeline.
//!
//! What this parser supports:
//!
//! * Pipelines: `cmd0 | cmd1 | cmd2`. At least one argv word
//!   per stage; an empty stage (consecutive `|` with nothing
//!   between, or a trailing `|`) returns
//!   [`ParseError::EmptyStage`].
//! * Redirections: `> path`, `>> path`, `< path`. Each
//!   redirection is one operator word followed by exactly
//!   one path word. A redirection with no path (the
//!   operator is the last word in the line) returns
//!   [`ParseError::MissingRedirTarget`]. Redirections may
//!   appear before, after, or between argv words on a stage:
//!   `> out echo hi` is the same as `echo hi > out`.
//! * Background: a single trailing `&` flips
//!   [`Pipeline::background`]. The `&` MUST be the last
//!   word; an `&` mid-line returns
//!   [`ParseError::AmpersandNotAtEnd`].
//!
//! What this parser does NOT yet support (deferred to
//! follow-up T142 partials):
//!
//! * `2>` / `2>&1` / `&>` stream-specific redirections —
//!   v1 redirects only stdout (`>` / `>>`) and stdin (`<`).
//! * Heredocs (`<<`).
//! * Logical operators (`&&` / `||`) and command sequences
//!   (`;` / newline).
//! * Subshells / grouping (`(...)`, `{...}`).
//!
//! The parser is **expansion-aware** in one direction only:
//! redirection targets are taken POST-expansion (the caller
//! has already run [`crate::run::assemble_token`] over each
//! word). So `echo $X > $LOG` expands `$LOG` to its env
//! value before opening the file. The parser itself never
//! reads env or `$?` — it just routes assembled strings.

use crate::run::{is_operator_word, TokenPart};

/// What a word is at the parser-input level: either an
/// operator (one of `|`, `<`, `>`, `>>`, `&`) or a regular
/// argv word. The parser consumes a slice of these alongside
/// a parallel slice of assembled (post-expansion) strings.
///
/// The shell wires this up by classifying each
/// `Vec<TokenPart>` from the tokenizer: a word whose only
/// segment is `TokenPart::Operator` becomes
/// [`WordKind::Operator`]; everything else becomes
/// [`WordKind::Argv`]. Tests can construct `WordKind`
/// instances directly without touching the crate-private
/// `TokenPart` type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WordKind {
    /// A regular argv word.
    Argv,
    /// An operator word (the assembled string is the operator).
    Operator,
}

impl WordKind {
    /// Build a `Vec<WordKind>` from the tokenizer's
    /// `Vec<Vec<TokenPart>>` output. The output is parallel
    /// to the input — same length, one entry per word.
    pub(crate) fn classify(parts_per_word: &[Vec<TokenPart>]) -> Vec<WordKind> {
        parts_per_word
            .iter()
            .map(|parts| {
                if is_operator_word(parts) {
                    WordKind::Operator
                } else {
                    WordKind::Argv
                }
            })
            .collect()
    }
}

/// Kinds of redirections the parser recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirOp {
    /// `< path` — open `path` for reading and dup it onto the
    /// stage's stdin (fd 0).
    Stdin,
    /// `> path` — open `path` for writing (truncate or
    /// create) and dup it onto the stage's stdout (fd 1).
    Stdout,
    /// `>> path` — open `path` for writing (append or
    /// create) and dup it onto the stage's stdout.
    StdoutAppend,
}

/// One redirection on a stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirection {
    /// The operator (`<`, `>`, `>>`).
    pub op: RedirOp,
    /// The post-expansion path the operator targets.
    pub target: String,
}

/// One stage of a pipeline: an argv plus zero or more
/// redirections.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stage {
    /// Command + args, in the order they appeared on the
    /// line. Always non-empty for a successfully parsed
    /// stage; an empty argv returns
    /// [`ParseError::EmptyStage`] from [`parse_pipeline`].
    pub argv: Vec<String>,
    /// Redirections in source order. v1 evaluates them in
    /// order, so `> a > b` ends up writing to `b` (the
    /// second open clobbers the first); a future slice may
    /// flag duplicate-fd redirects as a parse error.
    pub redirs: Vec<Redirection>,
}

/// A whole pipeline: one or more stages plus a background flag.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pipeline {
    /// Stages in left-to-right order. `cmd0 | cmd1 | cmd2`
    /// produces three stages where `cmd0`'s stdout feeds
    /// `cmd1`'s stdin, `cmd1`'s stdout feeds `cmd2`'s stdin,
    /// and `cmd2`'s stdout reaches the parent.
    pub stages: Vec<Stage>,
    /// Set by a trailing `&`. The runner spawns the pipeline
    /// without waiting and returns control to the REPL
    /// immediately.
    pub background: bool,
}

impl Pipeline {
    /// True iff the pipeline is exactly one stage with no
    /// redirections — the simple case the legacy v1
    /// dispatcher already handles in-process. Used by the
    /// REPL to short-circuit the redirection-aware path
    /// when nothing about the pipeline calls for it.
    pub fn is_simple(&self) -> bool {
        !self.background
            && self.stages.len() == 1
            && self.stages[0].redirs.is_empty()
    }
}

/// Why a parse failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// A pipeline stage had no argv words (consecutive `|`
    /// with nothing between, or a leading `|`, or a trailing
    /// `|`). Carries the offending operator for the
    /// diagnostic.
    EmptyStage,
    /// A redirection operator (`<`, `>`, `>>`) was the last
    /// word on the line — there's no path to redirect to.
    /// Carries the operator string for the diagnostic.
    MissingRedirTarget(String),
    /// A redirection's target slot was an operator word
    /// rather than a path (e.g. `> | grep` — the `|` would
    /// have been the redir target). Carries the offending
    /// operator that appeared where a path was expected.
    UnexpectedOperator(String),
    /// `&` appeared mid-line rather than as the trailing
    /// word. v1 has no command sequencing yet (`;`, `\n`),
    /// so a non-trailing `&` is unambiguously a syntax
    /// error.
    AmpersandNotAtEnd,
}

impl ParseError {
    /// Render the parser error as the kind of one-line
    /// diagnostic POSIX shells write to stderr. Keeps the
    /// dispatch loop's error path small.
    pub fn diagnostic(&self) -> String {
        match self {
            ParseError::EmptyStage => "sh: syntax error: empty pipeline stage".to_string(),
            ParseError::MissingRedirTarget(op) => {
                format!("sh: syntax error: missing target for redirection `{op}`")
            }
            ParseError::UnexpectedOperator(op) => {
                format!("sh: syntax error: unexpected operator `{op}` after redirection")
            }
            ParseError::AmpersandNotAtEnd => {
                "sh: syntax error: `&` must appear only as the last token".to_string()
            }
        }
    }
}

/// Parse the tokenised line into a [`Pipeline`].
///
/// Two parallel slices in:
///
/// * `kinds[i]` says whether word `i` is an argv word or an
///   operator word. The shell's REPL classifies the
///   tokenizer's output via [`WordKind::classify`]; tests
///   construct the slice manually.
/// * `assembled[i]` is the post-expansion string for word
///   `i`. For argv words it's whatever expansion produced;
///   for operator words it's the operator string itself
///   (`|` / `<` / `>` / `>>` / `&`).
///
/// The split lets the parser distinguish a bare `|` from a
/// quoted `'|'` — both have the same `assembled` value but
/// different [`WordKind`]s.
pub fn parse_pipeline(
    kinds: &[WordKind],
    assembled: &[String],
) -> Result<Pipeline, ParseError> {
    debug_assert_eq!(kinds.len(), assembled.len());

    let mut pipeline = Pipeline::default();
    let mut current = Stage::default();
    let mut i = 0usize;

    while i < kinds.len() {
        if matches!(kinds[i], WordKind::Operator) {
            let op = assembled[i].as_str();
            match op {
                "|" => {
                    if current.argv.is_empty() {
                        return Err(ParseError::EmptyStage);
                    }
                    pipeline.stages.push(core::mem::take(&mut current));
                    i += 1;
                    continue;
                }
                "<" | ">" | ">>" => {
                    let redir_op = match op {
                        "<" => RedirOp::Stdin,
                        ">" => RedirOp::Stdout,
                        ">>" => RedirOp::StdoutAppend,
                        _ => unreachable!(),
                    };
                    let next_idx = i + 1;
                    if next_idx >= kinds.len() {
                        return Err(ParseError::MissingRedirTarget(op.to_string()));
                    }
                    if matches!(kinds[next_idx], WordKind::Operator) {
                        return Err(ParseError::UnexpectedOperator(
                            assembled[next_idx].clone(),
                        ));
                    }
                    current.redirs.push(Redirection {
                        op: redir_op,
                        target: assembled[next_idx].clone(),
                    });
                    i += 2;
                    continue;
                }
                "&" => {
                    if i + 1 != kinds.len() {
                        return Err(ParseError::AmpersandNotAtEnd);
                    }
                    pipeline.background = true;
                    i += 1;
                    continue;
                }
                _ => {
                    // Tokenizer should never emit anything
                    // other than the five operators above.
                    // Surface as an unexpected-operator error
                    // so a future tokenizer change can't
                    // silently break the parser.
                    return Err(ParseError::UnexpectedOperator(op.to_string()));
                }
            }
        }
        // Regular argv word.
        current.argv.push(assembled[i].clone());
        i += 1;
    }

    if current.argv.is_empty() && !pipeline.stages.is_empty() {
        // The line ended with a `|` (no stage after it).
        return Err(ParseError::EmptyStage);
    }
    if !current.argv.is_empty() || !current.redirs.is_empty() {
        if current.argv.is_empty() {
            // Redirections without a command — POSIX allows
            // this for side-effects like `> file` (truncate
            // an empty file), but the v1 sh has no use case
            // and treating it as an empty stage is safer.
            return Err(ParseError::EmptyStage);
        }
        pipeline.stages.push(current);
    }

    Ok(pipeline)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: Vec<(WordKind, &str)>) -> Result<Pipeline, ParseError> {
        let kinds: Vec<WordKind> = words.iter().map(|(k, _)| k.clone()).collect();
        let assembled: Vec<String> = words.iter().map(|(_, s)| s.to_string()).collect();
        parse_pipeline(&kinds, &assembled)
    }

    fn argv(s: &str) -> (WordKind, &str) {
        (WordKind::Argv, s)
    }
    fn op(s: &str) -> (WordKind, &str) {
        (WordKind::Operator, s)
    }

    #[test]
    fn single_stage_is_simple() {
        let p = parse(vec![argv("echo"), argv("hi")]).unwrap();
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].argv, vec!["echo", "hi"]);
        assert!(p.is_simple());
    }

    #[test]
    fn pipeline_two_stages() {
        let p = parse(vec![argv("echo"), argv("hi"), op("|"), argv("grep"), argv("hi")]).unwrap();
        assert_eq!(p.stages.len(), 2);
        assert_eq!(p.stages[0].argv, vec!["echo", "hi"]);
        assert_eq!(p.stages[1].argv, vec!["grep", "hi"]);
        assert!(!p.is_simple());
    }

    #[test]
    fn redirect_stdout_truncate() {
        let p = parse(vec![argv("echo"), argv("hi"), op(">"), argv("/tmp/out")]).unwrap();
        assert_eq!(p.stages[0].redirs[0].op, RedirOp::Stdout);
        assert_eq!(p.stages[0].redirs[0].target, "/tmp/out");
    }

    #[test]
    fn redirect_stdout_append() {
        let p = parse(vec![argv("echo"), op(">>"), argv("log")]).unwrap();
        assert_eq!(p.stages[0].redirs[0].op, RedirOp::StdoutAppend);
    }

    #[test]
    fn redirect_stdin() {
        let p = parse(vec![argv("cat"), op("<"), argv("/etc/hosts")]).unwrap();
        assert_eq!(p.stages[0].redirs[0].op, RedirOp::Stdin);
        assert_eq!(p.stages[0].redirs[0].target, "/etc/hosts");
    }

    #[test]
    fn background_trailing_ampersand() {
        let p = parse(vec![argv("sleep"), argv("1"), op("&")]).unwrap();
        assert!(p.background);
        assert_eq!(p.stages[0].argv, vec!["sleep", "1"]);
    }

    #[test]
    fn empty_stage_between_pipes_errors() {
        let err = parse(vec![argv("a"), op("|"), op("|"), argv("b")]).unwrap_err();
        assert_eq!(err, ParseError::EmptyStage);
    }

    #[test]
    fn trailing_pipe_errors() {
        let err = parse(vec![argv("a"), op("|")]).unwrap_err();
        assert_eq!(err, ParseError::EmptyStage);
    }

    #[test]
    fn redir_without_target_errors() {
        let err = parse(vec![argv("echo"), op(">")]).unwrap_err();
        assert_eq!(err, ParseError::MissingRedirTarget(">".to_string()));
    }

    #[test]
    fn redir_target_is_operator_errors() {
        let err = parse(vec![argv("echo"), op(">"), op("|"), argv("g")]).unwrap_err();
        assert_eq!(err, ParseError::UnexpectedOperator("|".to_string()));
    }

    #[test]
    fn ampersand_mid_line_errors() {
        let err = parse(vec![argv("a"), op("&"), argv("b")]).unwrap_err();
        assert_eq!(err, ParseError::AmpersandNotAtEnd);
    }

    #[test]
    fn redir_can_precede_argv() {
        let p = parse(vec![op(">"), argv("out"), argv("echo"), argv("hi")]).unwrap();
        assert_eq!(p.stages[0].argv, vec!["echo", "hi"]);
        assert_eq!(p.stages[0].redirs[0].target, "out");
    }
}
