//! Scrollback + input-buffer state machine for the Rust
//! terminal emulator.
//!
//! [`Terminal`] owns a [`sh::Shell`] instance so committed
//! input lines are evaluated in-process and their stdout /
//! stderr are appended to scrollback directly. A scrollback
//! line carries a [`LineKind`] discriminator so the future
//! display-protocol renderer can colour input, banner, normal
//! output, and stderr differently.
//!
//! External bytes (for example, a future `proc_spawn`-ed
//! child piping stdout at this terminal) flow through
//! [`Terminal::append_output`], which does streaming UTF-8
//! decoding and splits on `\n` into scrollback lines — the
//! same behaviour as the TS-side `Terminal.appendOutput`.

use sh::{Shell, ShellOutput};

/// Default number of scrollback lines kept before old ones
/// fall off the top. The `term` bin driver can override this
/// via [`TerminalOptions::max_lines`].
pub const DEFAULT_MAX_LINES: usize = 512;

/// One rendered line in the scrollback buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalLine {
    pub text: String,
    pub kind: LineKind,
}

/// The semantic class of a scrollback line. Rendering code
/// keys off this to colour stdout/stderr differently and to
/// prefix input lines with the shell prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// Pre-baked banner or system line printed at startup.
    Banner,
    /// A line the user typed and committed. The displayed
    /// text already includes the prompt prefix.
    Input,
    /// Normal output: shell stdout or bytes appended via
    /// [`Terminal::append_output`].
    Output,
    /// Shell stderr — error output rendered distinctly.
    Error,
}

/// Terminal configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalOptions {
    /// Maximum scrollback depth. Must be greater than zero.
    pub max_lines: usize,
    /// Banner lines printed before any interaction. Each line
    /// becomes a [`LineKind::Banner`] entry in scrollback.
    pub banner: Vec<String>,
    /// Prompt string rendered before each input line. The
    /// canonical PMos prompt is `"> "`.
    pub prompt: String,
}

impl Default for TerminalOptions {
    fn default() -> Self {
        TerminalOptions {
            max_lines: DEFAULT_MAX_LINES,
            banner: Vec::new(),
            prompt: "> ".to_string(),
        }
    }
}

/// A frozen view of the terminal, suitable for paint callbacks
/// and test assertions. Cloned out of the terminal so callers
/// can render without holding a borrow on the state machine.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TerminalSnapshot {
    pub lines: Vec<TerminalLine>,
    pub input_buffer: String,
    pub prompt: String,
}

/// A keystroke fed to [`Terminal::feed_key`].
///
/// This is a deliberately tiny enum: the v1 terminal is not a
/// full VT100 emulator, so modifier keys, arrow keys, and
/// escape sequences are not part of the public surface. A
/// future slice can add variants without breaking callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A single printable character (already decoded from a
    /// DOM KeyboardEvent or a TTY escape sequence).
    Char(char),
    /// Return / Enter — commits the current input buffer.
    Enter,
    /// Backspace / DEL — drops the last character of the
    /// input buffer (no-op when the buffer is empty).
    Backspace,
}

/// Result of [`Terminal::feed_key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyFeedResult {
    /// The key was not recognised and no state changed.
    Ignored,
    /// The input buffer was edited (printable char appended
    /// or last char removed). Rendering should repaint.
    Edited,
    /// The user pressed Enter. The terminal evaluated the
    /// committed line through its embedded shell and appended
    /// the resulting stdout / stderr to scrollback. Callers
    /// typically only need to repaint; `output` is exposed so
    /// they can also mirror the bytes to an external sink.
    Committed {
        /// The line the user typed, without a trailing newline.
        line: String,
        /// Output returned by [`Shell::eval`].
        output: ShellOutput,
        /// True iff the embedded shell's exit flag is now set.
        exited: bool,
    },
}

/// The terminal state machine.
pub struct Terminal {
    max_lines: usize,
    prompt: String,
    lines: Vec<TerminalLine>,
    input_buffer: String,
    /// Streaming partial-line buffer used by
    /// [`Terminal::append_output`]. Shell output bypasses this
    /// buffer entirely so stdout → stderr transitions don't
    /// intermingle bytes of different kinds.
    pending_output: Vec<u8>,
    shell: Shell,
}

impl Terminal {
    /// Construct a new terminal with a fresh [`Shell`] and
    /// the banner lines baked into scrollback.
    ///
    /// # Panics
    ///
    /// Panics if `options.max_lines == 0`.
    pub fn new(options: TerminalOptions) -> Self {
        assert!(options.max_lines > 0, "Terminal: max_lines must be > 0");
        let mut term = Terminal {
            max_lines: options.max_lines,
            prompt: options.prompt,
            lines: Vec::new(),
            input_buffer: String::new(),
            pending_output: Vec::new(),
            shell: Shell::new(),
        };
        for line in options.banner {
            term.push_line(TerminalLine {
                text: line,
                kind: LineKind::Banner,
            });
        }
        term
    }

    /// Borrow the embedded shell — primarily for tests that
    /// want to assert on cwd / env after running commands.
    pub fn shell(&self) -> &Shell {
        &self.shell
    }

    /// Mutable access to the embedded shell. Use sparingly:
    /// mutating the shell outside [`Terminal::feed_key`]
    /// bypasses scrollback rendering.
    pub fn shell_mut(&mut self) -> &mut Shell {
        &mut self.shell
    }

    /// Current input buffer — what the user has typed but
    /// not yet committed.
    pub fn input_buffer(&self) -> &str {
        &self.input_buffer
    }

    /// Current prompt string.
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// Current scrollback length (banner + committed lines +
    /// output, up to `max_lines`).
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// True iff the embedded shell has exited.
    pub fn has_exited(&self) -> bool {
        self.shell.has_exited()
    }

    /// Frozen view of the terminal suitable for rendering.
    pub fn snapshot(&self) -> TerminalSnapshot {
        TerminalSnapshot {
            lines: self.lines.clone(),
            input_buffer: self.input_buffer.clone(),
            prompt: self.prompt.clone(),
        }
    }

    /// True iff there is nothing in scrollback and no pending
    /// input. Used by the first-paint gate in the bin driver.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.input_buffer.is_empty()
    }

    /// Consume a key event. See [`Key`] for the set of
    /// supported keystrokes.
    pub fn feed_key(&mut self, key: Key) -> KeyFeedResult {
        match key {
            Key::Enter => self.commit_line(),
            Key::Backspace => {
                if self.input_buffer.pop().is_some() {
                    KeyFeedResult::Edited
                } else {
                    KeyFeedResult::Ignored
                }
            }
            Key::Char(c) => {
                if is_printable(c) {
                    self.input_buffer.push(c);
                    KeyFeedResult::Edited
                } else {
                    KeyFeedResult::Ignored
                }
            }
        }
    }

    /// Append raw output bytes from an external streaming
    /// source. Bytes are decoded as lossy UTF-8 and split on
    /// `\n` into [`LineKind::Output`] entries. Partial lines
    /// (no trailing `\n`) are held in an internal buffer until
    /// a later call completes them.
    pub fn append_output(&mut self, bytes: &[u8]) {
        self.pending_output.extend_from_slice(bytes);
        while let Some(pos) = self.pending_output.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.pending_output.drain(..=pos).collect();
            let text =
                String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]).into_owned();
            self.push_line(TerminalLine {
                text,
                kind: LineKind::Output,
            });
        }
    }

    /// Wipe all scrollback, the streaming pending buffer, and
    /// the input buffer. The embedded shell state (cwd, env,
    /// exit flag) is preserved.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.input_buffer.clear();
        self.pending_output.clear();
    }

    // ---- Internal helpers -----------------------------------

    fn commit_line(&mut self) -> KeyFeedResult {
        let line = std::mem::take(&mut self.input_buffer);

        // Push the typed line into scrollback, prefixed with
        // the prompt, so the user can scroll back and see
        // what they ran.
        let mut display = String::with_capacity(self.prompt.len() + line.len());
        display.push_str(&self.prompt);
        display.push_str(&line);
        self.push_line(TerminalLine {
            text: display,
            kind: LineKind::Input,
        });

        // Evaluate the line through the embedded shell. The
        // `exit` builtin flips the shell's exit flag.
        let output = self.shell.eval(&line);
        if !output.stdout.is_empty() {
            self.push_bytes_as_lines(&output.stdout, LineKind::Output);
        }
        if !output.stderr.is_empty() {
            self.push_bytes_as_lines(&output.stderr, LineKind::Error);
        }
        let exited = self.shell.has_exited();
        KeyFeedResult::Committed {
            line,
            output,
            exited,
        }
    }

    /// Split `bytes` on `\n` and push each piece as a
    /// scrollback line of the given kind. A trailing fragment
    /// with no terminating `\n` is pushed as its own line.
    /// Bypasses `pending_output` because shell output is a
    /// complete chunk per call, not a stream.
    fn push_bytes_as_lines(&mut self, bytes: &[u8], kind: LineKind) {
        let mut start = 0;
        while start < bytes.len() {
            match bytes[start..].iter().position(|&b| b == b'\n') {
                Some(rel) => {
                    let end = start + rel;
                    let text = String::from_utf8_lossy(&bytes[start..end]).into_owned();
                    self.push_line(TerminalLine { text, kind });
                    start = end + 1;
                }
                None => {
                    let text = String::from_utf8_lossy(&bytes[start..]).into_owned();
                    self.push_line(TerminalLine { text, kind });
                    break;
                }
            }
        }
    }

    fn push_line(&mut self, line: TerminalLine) {
        self.lines.push(line);
        while self.lines.len() > self.max_lines {
            self.lines.remove(0);
        }
    }
}

fn is_printable(c: char) -> bool {
    // Printable-ish: not a C0 control, not DEL. This matches
    // the ASCII-range check in `web/src/terminal.ts`. Unicode
    // above 0x7f is allowed since the terminal uses lossy
    // UTF-8 decoding for the streaming path.
    let code = c as u32;
    code >= 0x20 && code != 0x7f
}
