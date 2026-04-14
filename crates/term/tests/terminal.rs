//! Isolation tests for the `term` crate's `Terminal` state
//! machine. These exercise the same behaviours as the TS-side
//! `web/tests/unit/terminal.test.ts` plus the extra surface
//! that comes from owning an embedded `sh::Shell`.

use term::{Key, KeyFeedResult, LineKind, Terminal, TerminalOptions};

fn opts() -> TerminalOptions {
    TerminalOptions::default()
}

#[test]
fn new_terminal_is_empty_and_has_default_prompt() {
    let term = Terminal::new(opts());
    assert!(term.is_empty());
    assert_eq!(term.input_buffer(), "");
    assert_eq!(term.prompt(), "> ");
    let snap = term.snapshot();
    assert_eq!(snap.lines.len(), 0);
    assert_eq!(snap.input_buffer, "");
    assert_eq!(snap.prompt, "> ");
}

#[test]
fn banner_lines_are_rendered_as_banner_kind() {
    let term = Terminal::new(TerminalOptions {
        banner: vec!["hello".to_string(), "world".to_string()],
        ..opts()
    });
    let snap = term.snapshot();
    assert_eq!(snap.lines.len(), 2);
    assert_eq!(snap.lines[0].text, "hello");
    assert_eq!(snap.lines[0].kind, LineKind::Banner);
    assert_eq!(snap.lines[1].text, "world");
    assert_eq!(snap.lines[1].kind, LineKind::Banner);
    // Banner lines still count as non-empty scrollback.
    assert!(!term.is_empty());
}

#[test]
#[should_panic(expected = "max_lines must be > 0")]
fn max_lines_zero_panics() {
    Terminal::new(TerminalOptions {
        max_lines: 0,
        ..opts()
    });
}

#[test]
fn printable_char_appends_to_input_buffer() {
    let mut term = Terminal::new(opts());
    assert_eq!(term.feed_key(Key::Char('h')), KeyFeedResult::Edited);
    assert_eq!(term.feed_key(Key::Char('i')), KeyFeedResult::Edited);
    assert_eq!(term.input_buffer(), "hi");
}

#[test]
fn unicode_printable_chars_are_accepted() {
    let mut term = Terminal::new(opts());
    assert_eq!(term.feed_key(Key::Char('π')), KeyFeedResult::Edited);
    assert_eq!(term.feed_key(Key::Char('á')), KeyFeedResult::Edited);
    assert_eq!(term.input_buffer(), "πá");
}

#[test]
fn control_chars_are_ignored() {
    let mut term = Terminal::new(opts());
    // DEL
    assert_eq!(term.feed_key(Key::Char('\u{7f}')), KeyFeedResult::Ignored);
    // NUL
    assert_eq!(term.feed_key(Key::Char('\0')), KeyFeedResult::Ignored);
    // BEL
    assert_eq!(term.feed_key(Key::Char('\u{7}')), KeyFeedResult::Ignored);
    assert_eq!(term.input_buffer(), "");
}

#[test]
fn backspace_removes_last_char() {
    let mut term = Terminal::new(opts());
    term.feed_key(Key::Char('a'));
    term.feed_key(Key::Char('b'));
    term.feed_key(Key::Char('c'));
    assert_eq!(term.input_buffer(), "abc");
    assert_eq!(term.feed_key(Key::Backspace), KeyFeedResult::Edited);
    assert_eq!(term.input_buffer(), "ab");
    assert_eq!(term.feed_key(Key::Backspace), KeyFeedResult::Edited);
    assert_eq!(term.feed_key(Key::Backspace), KeyFeedResult::Edited);
    assert_eq!(term.input_buffer(), "");
}

#[test]
fn backspace_on_empty_is_ignored() {
    let mut term = Terminal::new(opts());
    assert_eq!(term.feed_key(Key::Backspace), KeyFeedResult::Ignored);
    assert_eq!(term.input_buffer(), "");
}

#[test]
fn enter_on_empty_line_produces_no_output() {
    let mut term = Terminal::new(opts());
    match term.feed_key(Key::Enter) {
        KeyFeedResult::Committed {
            line,
            output,
            exited,
        } => {
            assert_eq!(line, "");
            assert!(output.stdout.is_empty());
            assert!(output.stderr.is_empty());
            assert!(!exited);
        }
        other => panic!("expected Committed, got {other:?}"),
    }
    // A prompt-only input line still goes into scrollback so
    // the user can see a blank Enter happened.
    let snap = term.snapshot();
    assert_eq!(snap.lines.len(), 1);
    assert_eq!(snap.lines[0].text, "> ");
    assert_eq!(snap.lines[0].kind, LineKind::Input);
}

#[test]
fn echo_hello_produces_input_plus_output_lines() {
    let mut term = Terminal::new(opts());
    for ch in "echo hello".chars() {
        term.feed_key(Key::Char(ch));
    }
    let result = term.feed_key(Key::Enter);
    let KeyFeedResult::Committed { output, line, .. } = result else {
        panic!("expected Committed, got {result:?}");
    };
    assert_eq!(line, "echo hello");
    assert_eq!(output.stdout, b"hello\n");
    assert!(output.stderr.is_empty());

    let snap = term.snapshot();
    assert_eq!(snap.lines.len(), 2);
    assert_eq!(snap.lines[0].text, "> echo hello");
    assert_eq!(snap.lines[0].kind, LineKind::Input);
    assert_eq!(snap.lines[1].text, "hello");
    assert_eq!(snap.lines[1].kind, LineKind::Output);
    // Input buffer is cleared after commit.
    assert_eq!(term.input_buffer(), "");
}

#[test]
fn unknown_command_produces_error_line_in_scrollback() {
    let mut term = Terminal::new(opts());
    for ch in "nope".chars() {
        term.feed_key(Key::Char(ch));
    }
    let result = term.feed_key(Key::Enter);
    let KeyFeedResult::Committed { output, .. } = result else {
        panic!("expected Committed");
    };
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());

    let snap = term.snapshot();
    assert_eq!(snap.lines.len(), 2);
    assert_eq!(snap.lines[0].text, "> nope");
    assert_eq!(snap.lines[1].kind, LineKind::Error);
    assert!(snap.lines[1].text.contains("command not found"));
    assert!(snap.lines[1].text.contains("nope"));
}

#[test]
fn help_command_produces_builtin_listing() {
    let mut term = Terminal::new(opts());
    for ch in "help".chars() {
        term.feed_key(Key::Char(ch));
    }
    let _ = term.feed_key(Key::Enter);
    let snap = term.snapshot();
    // [input, "builtins:", "  help", "  echo", ..., "  false"].
    assert!(snap.lines.len() > 3);
    assert_eq!(snap.lines[0].text, "> help");
    assert_eq!(snap.lines[1].text, "builtins:");
    assert_eq!(snap.lines[1].kind, LineKind::Output);
    assert!(snap.lines.iter().any(|l| l.text == "  help"));
    assert!(snap.lines.iter().any(|l| l.text == "  exit"));
    assert!(snap.lines.iter().any(|l| l.text == "  echo"));
}

#[test]
fn exit_flips_has_exited_and_committed_result() {
    let mut term = Terminal::new(opts());
    for ch in "exit 7".chars() {
        term.feed_key(Key::Char(ch));
    }
    let result = term.feed_key(Key::Enter);
    let KeyFeedResult::Committed { output, exited, .. } = result else {
        panic!("expected Committed");
    };
    assert_eq!(output.exit_code, Some(7));
    assert!(exited);
    assert!(term.has_exited());
}

#[test]
fn cd_updates_embedded_shell_cwd_and_pwd_reports_it() {
    let mut term = Terminal::new(opts());
    for ch in "cd /usr".chars() {
        term.feed_key(Key::Char(ch));
    }
    let _ = term.feed_key(Key::Enter);
    assert_eq!(term.shell().cwd(), "/usr");

    for ch in "pwd".chars() {
        term.feed_key(Key::Char(ch));
    }
    let _ = term.feed_key(Key::Enter);
    let snap = term.snapshot();
    assert!(snap.lines.iter().any(|l| l.text == "/usr"));
}

#[test]
fn set_env_is_visible_via_embedded_shell() {
    let mut term = Terminal::new(opts());
    for ch in "set FOO=bar".chars() {
        term.feed_key(Key::Char(ch));
    }
    let _ = term.feed_key(Key::Enter);
    assert_eq!(term.shell().get_env("FOO"), Some("bar"));
}

#[test]
fn append_output_streams_partial_lines_until_newline() {
    let mut term = Terminal::new(opts());
    term.append_output(b"hel");
    // No newline yet — nothing in scrollback.
    assert_eq!(term.snapshot().lines.len(), 0);
    term.append_output(b"lo\nworld");
    let snap = term.snapshot();
    assert_eq!(snap.lines.len(), 1);
    assert_eq!(snap.lines[0].text, "hello");
    assert_eq!(snap.lines[0].kind, LineKind::Output);
    // The "world" fragment is still pending.
    term.append_output(b"\n");
    let snap = term.snapshot();
    assert_eq!(snap.lines.len(), 2);
    assert_eq!(snap.lines[1].text, "world");
    assert_eq!(snap.lines[1].kind, LineKind::Output);
}

#[test]
fn append_output_handles_multiple_newlines_in_one_chunk() {
    let mut term = Terminal::new(opts());
    term.append_output(b"a\nb\nc\n");
    let snap = term.snapshot();
    assert_eq!(snap.lines.len(), 3);
    assert_eq!(snap.lines[0].text, "a");
    assert_eq!(snap.lines[1].text, "b");
    assert_eq!(snap.lines[2].text, "c");
}

#[test]
fn scrollback_is_bounded_by_max_lines() {
    let mut term = Terminal::new(TerminalOptions {
        max_lines: 3,
        ..opts()
    });
    term.append_output(b"one\ntwo\nthree\nfour\nfive\n");
    let snap = term.snapshot();
    assert_eq!(snap.lines.len(), 3);
    assert_eq!(snap.lines[0].text, "three");
    assert_eq!(snap.lines[1].text, "four");
    assert_eq!(snap.lines[2].text, "five");
}

#[test]
fn commit_line_also_respects_max_lines_bound() {
    let mut term = Terminal::new(TerminalOptions {
        max_lines: 2,
        ..opts()
    });
    for ch in "echo one".chars() {
        term.feed_key(Key::Char(ch));
    }
    let _ = term.feed_key(Key::Enter);
    for ch in "echo two".chars() {
        term.feed_key(Key::Char(ch));
    }
    let _ = term.feed_key(Key::Enter);
    let snap = term.snapshot();
    // Two commands produced four lines (2 input + 2 output);
    // only the last two survive.
    assert_eq!(snap.lines.len(), 2);
    assert_eq!(snap.lines[0].text, "> echo two");
    assert_eq!(snap.lines[1].text, "two");
}

#[test]
fn clear_wipes_scrollback_and_input_but_preserves_shell_state() {
    let mut term = Terminal::new(opts());
    for ch in "set FOO=bar".chars() {
        term.feed_key(Key::Char(ch));
    }
    let _ = term.feed_key(Key::Enter);
    for ch in "mid".chars() {
        term.feed_key(Key::Char(ch));
    }
    assert!(!term.is_empty());
    assert_eq!(term.input_buffer(), "mid");

    term.clear();

    assert!(term.is_empty());
    assert_eq!(term.input_buffer(), "");
    assert_eq!(term.line_count(), 0);
    // Shell state persists.
    assert_eq!(term.shell().get_env("FOO"), Some("bar"));
}

#[test]
fn multiple_commands_accumulate_scrollback_in_order() {
    let mut term = Terminal::new(opts());
    for cmd in ["echo one", "echo two", "echo three"] {
        for ch in cmd.chars() {
            term.feed_key(Key::Char(ch));
        }
        let _ = term.feed_key(Key::Enter);
    }
    let snap = term.snapshot();
    // Each command produces two lines (input + output).
    assert_eq!(snap.lines.len(), 6);
    assert_eq!(snap.lines[0].text, "> echo one");
    assert_eq!(snap.lines[1].text, "one");
    assert_eq!(snap.lines[2].text, "> echo two");
    assert_eq!(snap.lines[3].text, "two");
    assert_eq!(snap.lines[4].text, "> echo three");
    assert_eq!(snap.lines[5].text, "three");
}

#[test]
fn custom_prompt_is_used_for_input_lines() {
    let mut term = Terminal::new(TerminalOptions {
        prompt: "pmos$ ".to_string(),
        ..opts()
    });
    for ch in "echo hi".chars() {
        term.feed_key(Key::Char(ch));
    }
    let _ = term.feed_key(Key::Enter);
    let snap = term.snapshot();
    assert_eq!(snap.prompt, "pmos$ ");
    assert_eq!(snap.lines[0].text, "pmos$ echo hi");
}

#[test]
fn has_exited_starts_false_and_is_sticky() {
    let mut term = Terminal::new(opts());
    assert!(!term.has_exited());
    for ch in "echo alive".chars() {
        term.feed_key(Key::Char(ch));
    }
    let _ = term.feed_key(Key::Enter);
    assert!(!term.has_exited());
    for ch in "exit".chars() {
        term.feed_key(Key::Char(ch));
    }
    let _ = term.feed_key(Key::Enter);
    assert!(term.has_exited());
}
