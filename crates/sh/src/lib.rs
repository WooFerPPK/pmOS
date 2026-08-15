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
//! * **External commands** are planned as isolated PMos processes and the
//!   production WASM binary launches them through the versioned
//!   `proc_spawn_manifest` extension. PATH lookup, kernel pipes,
//!   redirection, and exit-status propagation are handled without `fork`.
//!
//! Control-flow scripting (if/while/for/functions) and full job control are
//! intentionally still outside the v1 implementation.

extern crate alloc;

pub mod shell;
pub mod tokenize;

#[cfg(feature = "std")]
mod builtin;
#[cfg(feature = "std")]
pub mod jobs;
#[cfg(feature = "std")]
pub mod parser;
#[cfg(feature = "std")]
pub mod pmos_process;
#[cfg(feature = "std")]
pub mod process;
#[cfg(feature = "std")]
pub mod run;
#[cfg(feature = "std")]
pub mod spawn_wire;

pub use shell::{Shell, ShellOutput, BUILTINS};
pub use tokenize::tokenize;

#[cfg(feature = "std")]
pub use builtin::ShellFlags;
#[cfg(feature = "std")]
pub use jobs::{Job, JobStatus, JobTable};
#[cfg(feature = "std")]
pub use parser::{parse_pipeline, ParseError, Pipeline, RedirOp, Redirection, Stage, WordKind};
#[cfg(all(feature = "std", target_arch = "wasm32"))]
pub use pmos_process::WasmPmosSyscalls;
#[cfg(feature = "std")]
pub use pmos_process::{PmosProcessBackend, PmosSyscalls};
#[cfg(feature = "std")]
pub use process::{
    build_execution_plan, path_candidates, ExecutionPlan, ExecutionResult, NoProcessBackend,
    PlanError, PlannedInput, PlannedOutput, PlannedStage, ProcessBackend, ProcessError, ProcessIo,
    DEFAULT_PATH,
};
#[cfg(feature = "std")]
pub use run::{
    run, run_command_with_env_and_backend, run_with_env, run_with_env_and_backend, ExitStatus,
    ExpandError,
};
#[cfg(feature = "std")]
pub use spawn_wire::{encode_spawn_manifest_v1, SpawnEncodeError, SpawnWireManifest};
