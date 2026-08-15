//! Process-execution planning for the interactive shell.
//!
//! Parsing answers *what* the user typed; this module answers how that
//! command maps onto PMos process primitives.  The plan is deliberately
//! fd-agnostic: a backend turns the logical parent/file/pipe endpoints into
//! concrete descriptors, calls the documented `proc_spawn` extension, and
//! reaps foreground children with `proc_wait`.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};

use crate::parser::{Pipeline, RedirOp};

/// Search path used when the shell was spawned without a `PATH` entry.
///
/// Production binaries are currently registered below `/bin`; `/usr/bin`
/// remains in the fallback for compatibility with the public syscall and
/// package examples.
pub const DEFAULT_PATH: &str = "/bin:/usr/bin";

/// Logical stdin source for one child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlannedInput {
    /// Inherit the shell's stdin.
    Parent,
    /// Read from the numbered pipeline edge.
    Pipe(usize),
    /// Open this path for reading and pass it as stdin.
    File(PathBuf),
}

/// Logical stdout destination for one child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlannedOutput {
    /// Inherit the shell's stdout.
    Parent,
    /// Write to the numbered pipeline edge.
    Pipe(usize),
    /// Open this path for writing and pass it as stdout.
    File { path: PathBuf, append: bool },
}

/// One process in a pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedStage {
    /// Command spelling used in diagnostics.
    pub command: String,
    /// Absolute executable candidates in `PATH` order.  Resolution is
    /// performed by attempting `proc_spawn` because PMos's executable
    /// registry is authoritative and is not required to mirror VFS files.
    pub path_candidates: Vec<String>,
    /// Child argv, including argv[0].
    pub argv: Vec<String>,
    /// Deterministically sorted child environment.
    pub env: Vec<(String, String)>,
    /// Absolute lexical cwd inherited by the child.
    pub cwd: PathBuf,
    pub stdin: PlannedInput,
    pub stdout: PlannedOutput,
}

/// A complete spawn topology.  Every stage is a separate process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionPlan {
    pub stages: Vec<PlannedStage>,
    /// Number of kernel pipes the backend must allocate.
    pub pipe_count: usize,
    /// A background plan is published into the job table without waiting.
    pub background: bool,
}

/// Failure while converting a parsed pipeline into a spawn plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    EmptyCommand,
}

/// Streams supplied to an execution backend.
///
/// The PMos backend maps these to descriptors 0/1/2.  Isolation-test
/// backends can write deterministic bytes directly without launching host
/// processes.
pub struct ProcessIo<'a> {
    pub stdin: &'a mut dyn BufRead,
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
}

/// Successful process execution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutionResult {
    /// Pids in stage order.
    pub pids: Vec<i32>,
    /// Wait statuses in stage order.  Empty for a published background job.
    pub statuses: Vec<i32>,
}

impl ExecutionResult {
    /// POSIX pipeline status: the status of the last stage.
    pub fn pipeline_status(&self) -> i32 {
        self.statuses.last().copied().unwrap_or(0)
    }
}

/// Backend error surfaced as a shell diagnostic/status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessError {
    CommandNotFound { command: String },
    PermissionDenied { command: String },
    Spawn { command: String, errno: i32 },
    Redirection { path: PathBuf, errno: i32 },
    Io,
}

/// Process substrate used by [`crate::run::run_with_env_and_backend`].
pub trait ProcessBackend {
    fn execute(
        &mut self,
        plan: &ExecutionPlan,
        io: ProcessIo<'_>,
    ) -> Result<ExecutionResult, ProcessError>;
}

/// Compatibility backend for callers that have not supplied a PMos process
/// substrate.  It preserves the historical command-not-found behaviour.
#[derive(Default)]
pub struct NoProcessBackend;

impl ProcessBackend for NoProcessBackend {
    fn execute(
        &mut self,
        plan: &ExecutionPlan,
        _io: ProcessIo<'_>,
    ) -> Result<ExecutionResult, ProcessError> {
        let command = plan
            .stages
            .iter()
            .find(|stage| !stage_is_wrapped_builtin(stage))
            .or_else(|| plan.stages.first())
            .map(|stage| stage.command.clone())
            .unwrap_or_default();
        Err(ProcessError::CommandNotFound { command })
    }
}

/// Build a process plan for a pipeline containing at least one external
/// command.
///
/// Builtins in a mixed pipeline run in a child `/bin/sh -c ...`, matching
/// POSIX's subshell semantics while keeping simple/stateful builtins in the
/// parent shell.  Redirections are represented as descriptor endpoints and
/// are not included in the `-c` text.
pub fn build_execution_plan(
    pipeline: &Pipeline,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    is_builtin: impl Fn(&str) -> bool,
) -> Result<ExecutionPlan, PlanError> {
    let pipe_count = pipeline.stages.len().saturating_sub(1);
    let cwd = absolute_lexical(cwd, Path::new("."));
    let mut child_env: BTreeMap<String, String> = env.clone();
    child_env.insert("PWD".to_string(), cwd.to_string_lossy().into_owned());
    let child_env: Vec<(String, String)> = child_env.into_iter().collect();
    let mut stages = Vec::with_capacity(pipeline.stages.len());

    for (index, stage) in pipeline.stages.iter().enumerate() {
        let Some(command) = stage.argv.first() else {
            return Err(PlanError::EmptyCommand);
        };
        if command.is_empty() {
            return Err(PlanError::EmptyCommand);
        }

        let (path_candidates, argv) = if is_builtin(command) {
            (
                vec!["/bin/sh".to_string()],
                vec!["sh".to_string(), "-c".to_string(), quote_argv(&stage.argv)],
            )
        } else {
            (
                path_candidates(command, &cwd, env.get("PATH").map(String::as_str)),
                stage.argv.clone(),
            )
        };

        let stdin = stage
            .redirs
            .iter()
            .rev()
            .find(|redir| redir.op == RedirOp::Stdin)
            .map(|redir| PlannedInput::File(absolute_lexical(&cwd, Path::new(&redir.target))))
            .unwrap_or_else(|| {
                if index == 0 {
                    PlannedInput::Parent
                } else {
                    PlannedInput::Pipe(index - 1)
                }
            });

        let stdout = stage
            .redirs
            .iter()
            .rev()
            .find(|redir| matches!(redir.op, RedirOp::Stdout | RedirOp::StdoutAppend))
            .map(|redir| PlannedOutput::File {
                path: absolute_lexical(&cwd, Path::new(&redir.target)),
                append: redir.op == RedirOp::StdoutAppend,
            })
            .unwrap_or_else(|| {
                if index + 1 == pipeline.stages.len() {
                    PlannedOutput::Parent
                } else {
                    PlannedOutput::Pipe(index)
                }
            });

        stages.push(PlannedStage {
            command: command.clone(),
            path_candidates,
            argv,
            env: child_env.clone(),
            cwd: cwd.clone(),
            stdin,
            stdout,
        });
    }

    Ok(ExecutionPlan {
        stages,
        pipe_count,
        background: pipeline.background,
    })
}

/// Return absolute executable candidates in `PATH` order.
pub fn path_candidates(command: &str, cwd: &Path, path: Option<&str>) -> Vec<String> {
    if command.contains('/') {
        return vec![absolute_lexical(cwd, Path::new(command))
            .to_string_lossy()
            .into_owned()];
    }

    path.unwrap_or(DEFAULT_PATH)
        .split(':')
        .map(|entry| {
            let directory = if entry.is_empty() {
                cwd.to_path_buf()
            } else {
                absolute_lexical(cwd, Path::new(entry))
            };
            absolute_lexical(&directory, Path::new(command))
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn absolute_lexical(cwd: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::from("/");
    for component in joined.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(segment) => out.push(segment),
            Component::Prefix(_) => {}
        }
    }
    out
}

fn quote_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|word| {
            let escaped = word.replace('\'', "'\\''");
            format!("'{escaped}'")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn stage_is_wrapped_builtin(stage: &PlannedStage) -> bool {
    stage.path_candidates.as_slice() == ["/bin/sh"]
        && stage.argv.get(1).map(String::as_str) == Some("-c")
}

#[cfg(test)]
mod tests {
    use super::{path_candidates, DEFAULT_PATH};
    use std::path::Path;

    #[test]
    fn default_path_prefers_runtime_binary_registry() {
        assert_eq!(DEFAULT_PATH, "/bin:/usr/bin");
        assert_eq!(
            path_candidates("grep", Path::new("/work"), None),
            vec!["/bin/grep", "/usr/bin/grep"]
        );
    }

    #[test]
    fn relative_and_empty_path_entries_resolve_against_cwd() {
        assert_eq!(
            path_candidates("tool", Path::new("/home/user"), Some("tools::/bin")),
            vec!["/home/user/tools/tool", "/home/user/tool", "/bin/tool"]
        );
    }

    #[test]
    fn command_with_slash_bypasses_path() {
        assert_eq!(
            path_candidates("../bin/tool", Path::new("/home/user"), Some("/ignored")),
            vec!["/home/bin/tool"]
        );
    }
}
