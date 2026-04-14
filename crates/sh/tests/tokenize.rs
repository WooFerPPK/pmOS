//! Tokenizer isolation tests.

use sh::tokenize;

#[test]
fn empty_input_yields_empty_vector() {
    assert_eq!(tokenize(""), Vec::<String>::new());
}

#[test]
fn whitespace_only_input_yields_empty_vector() {
    assert_eq!(tokenize("   \t  "), Vec::<String>::new());
}

#[test]
fn splits_on_whitespace() {
    assert_eq!(
        tokenize("echo hello world"),
        vec!["echo", "hello", "world"]
    );
}

#[test]
fn multiple_spaces_and_tabs_collapse() {
    assert_eq!(tokenize("a   b\t\tc"), vec!["a", "b", "c"]);
}

#[test]
fn leading_and_trailing_whitespace_is_ignored() {
    assert_eq!(tokenize("   hello   "), vec!["hello"]);
}

#[test]
fn single_quotes_preserve_whitespace_literally() {
    assert_eq!(
        tokenize("echo 'hello  world'"),
        vec!["echo", "hello  world"]
    );
}

#[test]
fn single_quotes_do_not_unescape_backslash() {
    // Inside single quotes, everything is literal — even
    // backslashes. `'it\'` closes at the `'` right after
    // the `\` (since `\` has no quoting power inside
    // single quotes), then the bare `s` concatenates with
    // the previous segment per POSIX token rules. After
    // the space, `fine` starts a new token, and the
    // trailing `'` opens a new (unterminated) single
    // quote that tokenize absorbs into the final token.
    assert_eq!(
        tokenize(r"echo 'it\'s fine'"),
        vec!["echo", "it\\s", "fine"]
    );
}

#[test]
fn double_quotes_preserve_whitespace_literally() {
    assert_eq!(
        tokenize(r#"echo "hello  world""#),
        vec!["echo", "hello  world"]
    );
}

#[test]
fn double_quotes_honour_backslash_escapes() {
    assert_eq!(
        tokenize(r#"echo "she said \"hi\"""#),
        vec!["echo", r#"she said "hi""#]
    );
}

#[test]
fn backslash_outside_quotes_escapes_the_next_char() {
    assert_eq!(
        tokenize(r"echo hello\ world"),
        vec!["echo", "hello world"]
    );
}

#[test]
fn empty_quoted_strings_yield_empty_tokens_but_are_discarded_if_alone() {
    // `echo ''` → ["echo"] because the empty token never
    // has any chars pushed to `current`, so nothing is
    // emitted.
    assert_eq!(tokenize(r#"echo '' "" "#), vec!["echo"]);
}

#[test]
fn adjacent_quoted_and_bare_segments_form_one_token() {
    assert_eq!(
        tokenize("echo pre'mid'post"),
        vec!["echo", "premidpost"]
    );
}

#[test]
fn nested_quote_kinds_are_preserved_literally() {
    assert_eq!(
        tokenize(r#"echo "double 'single' inside""#),
        vec!["echo", "double 'single' inside"]
    );
    assert_eq!(
        tokenize(r#"echo 'single "double" inside'"#),
        vec!["echo", r#"single "double" inside"#]
    );
}
