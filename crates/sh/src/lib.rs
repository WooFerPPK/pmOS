#![cfg_attr(all(not(test), not(feature = "std")), no_std)]

//! PMos command-line shell (`/bin/sh`).
//!
//! Organised as a library so other crates can embed the
//! shell for tests. The `no_std + alloc` core provides the
//! tokenizer and the [`Shell`] state machine; the `std`
//! feature (default-on) adds the [`run`] REPL driver that
//! the userland `sh` binary wires into stdin / stdout /
//! stderr.
//!
//! The v1 feature set is deliberately narrow:
//!
//! * **Tokenizer** with single- and double-quote handling
//!   and backslash escapes. No variable expansion, no
//!   command substitution, no globbing.
//! * **Built-ins**: `echo`, `pwd`, `cd`, `env`, `set`,
//!   `unset`, `exit`, `true`, `false`, `help`.
//! * **REPL** (`run`): whitespace-split tokenisation over
//!   the same [`Shell`] state machine, with a minimal
//!   four-builtin dispatch (`echo`, `exit`, `cd`, `pwd`)
//!   plus a strict `exit` argument parse that distinguishes
//!   garbage from a missing code.
//! * **External commands** fall through to a
//!   "command-not-found" error with exit code 127. When a
//!   future slice bridges `proc_spawn` into userland, the
//!   external path will actually fork+exec; until then the
//!   error path is enough to keep the shell usable.
//!
//! No pipes, no redirection, no job control, no scripting
//! constructs (if/while/for/functions). Those land in
//! Phase 6 with `T142..T145`.

extern crate alloc;

pub mod shell;
pub mod tokenize;

#[cfg(feature = "std")]
mod builtin;
#[cfg(feature = "std")]
pub mod run;

pub use shell::{Shell, ShellOutput, BUILTINS};
pub use tokenize::tokenize;

#[cfg(feature = "std")]
pub use run::{run, run_with_env, ExitStatus};
