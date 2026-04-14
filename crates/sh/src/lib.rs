#![cfg_attr(not(test), no_std)]

//! PMos command-line shell (`/bin/sh`).
//!
//! This is a **minimal** POSIX-ish shell, organised as a
//! library so other crates can embed it for tests (in
//! particular the kernel's T077 headless-shell gate can
//! replace its inline faux-shell with a real instance of
//! [`Shell`] to prove the two parse identically).
//!
//! The v1 feature set is deliberately narrow:
//!
//! * **Tokenizer** with single- and double-quote handling
//!   and backslash escapes. No variable expansion, no
//!   command substitution, no globbing.
//! * **Built-ins**: `echo`, `pwd`, `cd`, `env`, `set`,
//!   `unset`, `exit`, `true`, `false`, `help`.
//! * **External commands** fall through to a
//!   "command-not-found" error with exit code 127. When a
//!   future slice bridges `proc_spawn` into userland, the
//!   external path will actually fork+exec; until then the
//!   error path is enough to keep the shell usable.
//!
//! No pipes, no redirection, no job control, no scripting
//! constructs (if/while/for/functions). Those land in
//! Phase 6 with `T142..T145`.
//!
//! The library is `no_std + alloc` so it can be linked
//! both into the userland `sh` binary (which targets
//! wasm32-wasi) and into the kernel crate's test harness
//! (which runs on the native host).

extern crate alloc;

pub mod shell;
pub mod tokenize;

pub use shell::{Shell, ShellOutput, BUILTINS};
pub use tokenize::tokenize;
