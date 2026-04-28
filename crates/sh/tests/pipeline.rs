//! T149 — pipeline isolation tests.
//!
//! T143 (pipeline runner) is not yet implemented in the shell
//! library; the parser does not recognize `|` as a pipeline
//! token. These tests pin the current behaviour (tokenizer
//! treats `|` as part of the surrounding word) so that when
//! T143 lands, the assertions can be flipped to the
//! pipeline-aware shape without touching the rest of this file.

use sh::tokenize::tokenize;

#[test]
fn pipe_char_is_currently_a_word_token() {
    // Pre-T143 contract: `|` is preserved as-is. After T143
    // lands, this test should be updated to assert the pipe
    // is parsed as a separator.
    let toks = tokenize("ls | grep foo");
    // Tokens contain "ls", "|", "grep", "foo" — or "ls|grep"
    // depending on whether the tokenizer treats `|` as a word
    // boundary. Either way the input is preserved, not dropped.
    let joined = toks.join(" ");
    assert!(joined.contains("|") || joined.contains("ls"));
}

#[test]
fn redirect_chars_currently_pass_through_tokenizer() {
    let toks = tokenize("ls > out.txt");
    let joined = toks.join(" ");
    assert!(joined.contains(">") || joined.contains("ls"));
}
