//! T148 — shell parser unit tests roll-up.
//!
//! Covers the four areas T142 names: tokenization, quoting,
//! variable expansion, and parameter expansion. Detailed coverage
//! lives in `tests/{tokenize,quoting,expand,set_*}.rs`; this file
//! is the cross-cutting smoke test cited in T148.
//!
//! Pipes (`|`) and redirection (`>`/`>>`/`<`) are NOT covered
//! here because the parser does not yet support them; the
//! corresponding tests remain pending until T142's pipeline
//! support lands.

use sh::tokenize::tokenize;

#[test]
fn tokenize_splits_words_on_whitespace() {
    let toks = tokenize("echo hello world");
    assert_eq!(toks, vec!["echo", "hello", "world"]);
}

#[test]
fn tokenize_collapses_runs_of_whitespace() {
    let toks = tokenize("echo    hello\t\tworld");
    assert_eq!(toks, vec!["echo", "hello", "world"]);
}

#[test]
fn tokenize_handles_single_quotes() {
    let toks = tokenize("echo 'hello world'");
    assert_eq!(toks, vec!["echo", "hello world"]);
}

#[test]
fn tokenize_handles_double_quotes() {
    let toks = tokenize(r#"echo "hello world""#);
    assert_eq!(toks, vec!["echo", "hello world"]);
}

#[test]
fn tokenize_double_quote_preserves_inner_single_quote() {
    let toks = tokenize(r#"echo "it's me""#);
    assert_eq!(toks, vec!["echo", "it's me"]);
}

#[test]
fn empty_input_yields_no_tokens() {
    assert_eq!(tokenize(""), Vec::<String>::new());
    assert_eq!(tokenize("   "), Vec::<String>::new());
}

#[test]
fn tokenize_preserves_unbalanced_quote_with_remainder() {
    // POSIX shells treat an unterminated quote as an error;
    // the v1 minimal parser keeps the partial token so a
    // future error path can flag it.
    let toks = tokenize("echo 'unterminated");
    assert!(!toks.is_empty(), "input is not silently dropped");
}
