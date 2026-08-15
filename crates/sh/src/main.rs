//! `/usr/bin/sh` — thin driver that wires real stdin /
//! stdout / stderr into the [`sh::run`] REPL loop.
//!
//! This file selects the PMos process backend, seeds the inherited
//! environment, supports `-c`, wires real stdio into the shell loop, and
//! translates [`sh::ExitStatus`] into a process exit code.
//!
//! Distinct from `/usr/bin/shell` (the desktop shell).

use std::collections::BTreeMap;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stderr = io::stderr();

    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    env.entry("PATH".to_string())
        .or_insert_with(|| sh::DEFAULT_PATH.to_string());
    let cwd = env
        .get("PWD")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut flags = sh::ShellFlags::default();
    let args: Vec<String> = std::env::args().collect();

    #[cfg(target_arch = "wasm32")]
    let mut backend = sh::PmosProcessBackend::new(sh::WasmPmosSyscalls::default());
    #[cfg(not(target_arch = "wasm32"))]
    let mut backend = sh::NoProcessBackend;

    let status = if args.get(1).map(String::as_str) == Some("-c") {
        let Some(command) = args.get(2) else {
            eprintln!("sh: -c requires an argument");
            return ExitCode::from(2);
        };
        sh::run_command_with_env_and_backend(
            command,
            stdout.lock(),
            stderr.lock(),
            &mut env,
            &mut flags,
            &mut backend,
            cwd,
        )
    } else {
        let reader = BufReader::new(stdin.lock());
        sh::run_with_env_and_backend(
            reader,
            stdout.lock(),
            stderr.lock(),
            &mut env,
            &mut flags,
            &mut backend,
        )
    };

    let code = status.code();
    // `ExitCode::from(u8)` clamps: treat negative /
    // wrapping shell codes as zero. In practice `exit`
    // accepts any i32 but the process-level surface is u8.
    let byte = u8::try_from(code.rem_euclid(256)).unwrap_or(0);
    ExitCode::from(byte)
}
