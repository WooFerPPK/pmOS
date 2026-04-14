//! Shell state and command dispatch.
//!
//! [`Shell`] is the stateful entry point. It owns the
//! current working directory, the environment map, and an
//! `exited` flag. [`Shell::eval`] takes one input line,
//! tokenises it, and dispatches the first token against
//! the built-in table in [`BUILTINS`] or — if no built-in
//! matches — returns a "command not found" error.
//!
//! The return shape is a [`ShellOutput`] carrying both
//! stdout and stderr byte vectors plus an optional exit
//! code (set when the shell has just been told to exit).
//! Callers that plug this into a byte-stream transport
//! (the kernel's T077 gate, for example) take both byte
//! vectors and ship them to their respective fd.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::tokenize::tokenize;

/// List of built-in command names, in the order `help`
/// prints them. Exported so external code can display
/// the same list the built-in `help` outputs.
pub const BUILTINS: &[&str] = &[
    "help", "echo", "pwd", "cd", "env", "set", "unset", "exit", "true", "false",
];

/// Output of [`Shell::eval`] for a single input line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Set to `Some(code)` when the shell is exiting. The
    /// same effect is reflected on [`Shell::has_exited`].
    pub exit_code: Option<i32>,
}

impl ShellOutput {
    fn empty() -> Self {
        ShellOutput::default()
    }
}

/// The PMos CLI shell state machine.
pub struct Shell {
    cwd: String,
    env: BTreeMap<String, String>,
    exited: Option<i32>,
}

impl Shell {
    /// Build a fresh shell with `cwd = "/"` and an empty
    /// env map.
    pub fn new() -> Self {
        Shell {
            cwd: "/".to_string(),
            env: BTreeMap::new(),
            exited: None,
        }
    }

    /// Build a shell with a specific cwd and env. Used by
    /// tests and by the userland `sh` binary that inherits
    /// envp from `argv` / `envp`.
    pub fn with_state(cwd: &str, env: BTreeMap<String, String>) -> Self {
        Shell {
            cwd: cwd.to_string(),
            env,
            exited: None,
        }
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn get_env(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(String::as_str)
    }

    /// Has this shell been told to exit? Mirrors
    /// `ShellOutput::exit_code` across calls.
    pub fn has_exited(&self) -> bool {
        self.exited.is_some()
    }

    /// Exit code from the most recent `exit` built-in, if
    /// any.
    pub fn exit_status(&self) -> Option<i32> {
        self.exited
    }

    /// Evaluate one input line.
    ///
    /// Empty or whitespace-only lines return an empty
    /// output. Unrecognised commands yield a
    /// `command-not-found` error on stderr and no exit
    /// code. Built-ins are dispatched via [`BUILTINS`].
    pub fn eval(&mut self, line: &str) -> ShellOutput {
        let tokens = tokenize(line);
        if tokens.is_empty() {
            return ShellOutput::empty();
        }
        let (cmd, rest) = tokens.split_first().unwrap();
        match cmd.as_str() {
            "help" => self.builtin_help(),
            "echo" => builtin_echo(rest),
            "pwd" => self.builtin_pwd(),
            "cd" => self.builtin_cd(rest),
            "env" => self.builtin_env(),
            "set" => self.builtin_set(rest),
            "unset" => self.builtin_unset(rest),
            "exit" => self.builtin_exit(rest),
            "true" => ShellOutput::empty(),
            "false" => ShellOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: Some(1),
            },
            _ => command_not_found(cmd),
        }
    }

    // ---- Built-ins ----------------------------------------------

    fn builtin_help(&self) -> ShellOutput {
        let mut stdout = Vec::new();
        stdout.extend_from_slice(b"builtins:\n");
        for name in BUILTINS {
            stdout.extend_from_slice(b"  ");
            stdout.extend_from_slice(name.as_bytes());
            stdout.push(b'\n');
        }
        ShellOutput {
            stdout,
            stderr: Vec::new(),
            exit_code: None,
        }
    }

    fn builtin_pwd(&self) -> ShellOutput {
        let mut stdout = Vec::with_capacity(self.cwd.len() + 1);
        stdout.extend_from_slice(self.cwd.as_bytes());
        stdout.push(b'\n');
        ShellOutput {
            stdout,
            ..Default::default()
        }
    }

    fn builtin_cd(&mut self, args: &[String]) -> ShellOutput {
        let target = match args.first() {
            Some(a) => a.clone(),
            None => {
                // POSIX `cd` with no args means $HOME.
                // We don't maintain $HOME yet in v1; use "/".
                "/".to_string()
            }
        };
        // Absolute vs relative. Absolute starts with "/".
        let new_cwd = if target.starts_with('/') {
            target
        } else {
            join_paths(&self.cwd, &target)
        };
        let normalised = normalize_path(&new_cwd);
        self.cwd = normalised;
        ShellOutput::empty()
    }

    fn builtin_env(&self) -> ShellOutput {
        let mut stdout = Vec::new();
        for (k, v) in &self.env {
            stdout.extend_from_slice(k.as_bytes());
            stdout.push(b'=');
            stdout.extend_from_slice(v.as_bytes());
            stdout.push(b'\n');
        }
        ShellOutput {
            stdout,
            ..Default::default()
        }
    }

    fn builtin_set(&mut self, args: &[String]) -> ShellOutput {
        // `set KEY=VALUE`.
        let Some(first) = args.first() else {
            return self.builtin_env(); // `set` with no args prints env, POSIX-ish.
        };
        if let Some(eq) = first.find('=') {
            let (key, value) = first.split_at(eq);
            // Strip the leading '=' from value.
            let value = &value[1..];
            self.env.insert(key.to_string(), value.to_string());
            ShellOutput::empty()
        } else {
            let mut stderr = Vec::new();
            stderr.extend_from_slice(b"set: usage: set KEY=VALUE\n");
            ShellOutput {
                stdout: Vec::new(),
                stderr,
                exit_code: None,
            }
        }
    }

    fn builtin_unset(&mut self, args: &[String]) -> ShellOutput {
        let Some(key) = args.first() else {
            let mut stderr = Vec::new();
            stderr.extend_from_slice(b"unset: usage: unset KEY\n");
            return ShellOutput {
                stdout: Vec::new(),
                stderr,
                exit_code: None,
            };
        };
        self.env.remove(key);
        ShellOutput::empty()
    }

    fn builtin_exit(&mut self, args: &[String]) -> ShellOutput {
        let code = match args.first() {
            Some(s) => s.parse::<i32>().unwrap_or(0),
            None => 0,
        };
        self.exited = Some(code);
        ShellOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: Some(code),
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Shell::new()
    }
}

fn builtin_echo(args: &[String]) -> ShellOutput {
    let mut stdout = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            stdout.push(b' ');
        }
        stdout.extend_from_slice(arg.as_bytes());
    }
    stdout.push(b'\n');
    ShellOutput {
        stdout,
        ..Default::default()
    }
}

fn command_not_found(cmd: &str) -> ShellOutput {
    let mut stderr = Vec::new();
    stderr.extend_from_slice(b"sh: command not found: ");
    stderr.extend_from_slice(cmd.as_bytes());
    stderr.push(b'\n');
    ShellOutput {
        stdout: Vec::new(),
        stderr,
        exit_code: None,
    }
}

// ---- Path helpers ----------------------------------------------

fn join_paths(base: &str, rel: &str) -> String {
    if base.ends_with('/') {
        alloc::format!("{base}{rel}")
    } else {
        alloc::format!("{base}/{rel}")
    }
}

/// Simple path normaliser. Handles `.` and `..` and
/// collapses repeated `/`. Not a full POSIX realpath —
/// doesn't resolve symlinks — but adequate for the v1
/// shell's `cd` built-in.
fn normalize_path(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    if stack.is_empty() {
        "/".to_string()
    } else {
        let mut out = String::new();
        for seg in stack {
            out.push('/');
            out.push_str(seg);
        }
        out
    }
}
