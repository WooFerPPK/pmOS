use term::{
    CommandRunResult, CommandRunner, Key, KeyFeedResult, LineKind, Terminal, TerminalOptions,
};

#[derive(Default)]
struct RecordingRunner {
    commands: Vec<String>,
}

impl CommandRunner for RecordingRunner {
    fn run_command(&mut self, line: &str) -> CommandRunResult {
        self.commands.push(line.to_string());
        CommandRunResult {
            stdout: b"two matches\n".to_vec(),
            stderr: Vec::new(),
            status: 0,
            exited: false,
        }
    }
}

#[test]
fn enter_delegates_complete_pipeline_to_isolated_runner() {
    let mut terminal = Terminal::new(TerminalOptions::default());
    let mut runner = RecordingRunner::default();
    let command = "cat /tmp/items | grep two";
    for character in command.chars() {
        terminal.feed_key_with_runner(Key::Char(character), &mut runner);
    }
    let result = terminal.feed_key_with_runner(Key::Enter, &mut runner);
    assert!(matches!(
        result,
        KeyFeedResult::Committed { exited: false, .. }
    ));
    assert_eq!(runner.commands, vec![command]);
    let snapshot = terminal.snapshot();
    assert_eq!(snapshot.lines[0].text, format!("> {command}"));
    assert_eq!(snapshot.lines[0].kind, LineKind::Input);
    assert_eq!(snapshot.lines[1].text, "two matches");
    assert_eq!(snapshot.lines[1].kind, LineKind::Output);
}

#[test]
fn isolated_shell_exit_closes_terminal_loop() {
    struct ExitRunner;
    impl CommandRunner for ExitRunner {
        fn run_command(&mut self, _line: &str) -> CommandRunResult {
            CommandRunResult {
                status: 7,
                exited: true,
                ..CommandRunResult::default()
            }
        }
    }

    let mut terminal = Terminal::new(TerminalOptions::default());
    terminal.feed_key_with_runner(Key::Char('x'), &mut ExitRunner);
    assert!(matches!(
        terminal.feed_key_with_runner(Key::Enter, &mut ExitRunner),
        KeyFeedResult::Committed {
            exited: true,
            output: sh::ShellOutput {
                exit_code: Some(7),
                ..
            },
            ..
        }
    ));
}
