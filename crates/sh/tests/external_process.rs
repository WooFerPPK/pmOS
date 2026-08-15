//! External-command planner/dispatcher isolation tests.
//!
//! These tests never launch host processes.  A recording backend sees the
//! exact topology the PMos syscall adapter will consume, which pins argv,
//! PATH order, cwd/env inheritance, pipe endpoints, status propagation, and
//! the parent-shell builtin boundary independently of browser timing.

use std::collections::{BTreeMap, VecDeque};
use std::io::Cursor;

use sh::{
    run_with_env_and_backend, ExecutionPlan, ExecutionResult, ExitStatus, PlannedInput,
    PlannedOutput, ProcessBackend, ProcessError, ProcessIo, ShellFlags,
};

#[derive(Default)]
struct RecordingBackend {
    plans: Vec<ExecutionPlan>,
    outcomes: VecDeque<Result<ExecutionResult, ProcessError>>,
    output: VecDeque<Vec<u8>>,
}

impl ProcessBackend for RecordingBackend {
    fn execute(
        &mut self,
        plan: &ExecutionPlan,
        io: ProcessIo<'_>,
    ) -> Result<ExecutionResult, ProcessError> {
        self.plans.push(plan.clone());
        if let Some(bytes) = self.output.pop_front() {
            io.stdout.write_all(&bytes).map_err(|_| ProcessError::Io)?;
        }
        self.outcomes.pop_front().unwrap_or_else(|| {
            Ok(ExecutionResult {
                pids: (0..plan.stages.len()).map(|i| 100 + i as i32).collect(),
                statuses: if plan.background {
                    Vec::new()
                } else {
                    vec![0; plan.stages.len()]
                },
            })
        })
    }
}

fn drive(
    input: &str,
    env: &mut BTreeMap<String, String>,
    backend: &mut RecordingBackend,
) -> (ExitStatus, String, String) {
    let mut flags = ShellFlags::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let status = run_with_env_and_backend(
        Cursor::new(input.as_bytes().to_vec()),
        &mut stdout,
        &mut stderr,
        env,
        &mut flags,
        backend,
    );
    (
        status,
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

#[test]
fn external_argv_path_cwd_and_sorted_env_reach_backend() {
    let mut env = BTreeMap::from([
        ("ZED".to_string(), "last".to_string()),
        ("PATH".to_string(), "/apps:/bin".to_string()),
        ("ALPHA".to_string(), "first".to_string()),
    ]);
    let mut backend = RecordingBackend::default();
    backend.output.push_back(b"ran widget\n".to_vec());

    let (status, stdout, stderr) =
        drive("widget --name 'two words'\nexit\n", &mut env, &mut backend);

    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stdout.contains("ran widget\n"), "stdout: {stdout:?}");
    assert!(stderr.is_empty(), "stderr: {stderr:?}");
    assert_eq!(backend.plans.len(), 1);
    let stage = &backend.plans[0].stages[0];
    assert_eq!(stage.command, "widget");
    assert_eq!(stage.path_candidates, vec!["/apps/widget", "/bin/widget"]);
    assert_eq!(stage.argv, vec!["widget", "--name", "two words"]);
    assert!(stage.cwd.is_absolute());
    assert_eq!(
        stage.env,
        vec![
            ("ALPHA".to_string(), "first".to_string()),
            ("PATH".to_string(), "/apps:/bin".to_string()),
            ("PWD".to_string(), stage.cwd.to_string_lossy().into_owned(),),
            ("ZED".to_string(), "last".to_string()),
        ]
    );
    assert_eq!(stage.stdin, PlannedInput::Parent);
    assert_eq!(stage.stdout, PlannedOutput::Parent);
}

#[test]
fn real_pipeline_plan_connects_adjacent_stage_fds_and_redirects_last_stdout() {
    let mut env = BTreeMap::from([("PATH".to_string(), "/bin".to_string())]);
    let mut backend = RecordingBackend::default();

    let (status, _stdout, stderr) = drive(
        "ls /home | grep notes > results.txt\nexit\n",
        &mut env,
        &mut backend,
    );

    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "stderr: {stderr:?}");
    let plan = &backend.plans[0];
    assert_eq!(plan.pipe_count, 1);
    assert_eq!(plan.stages.len(), 2);
    assert_eq!(plan.stages[0].path_candidates, vec!["/bin/ls"]);
    assert_eq!(plan.stages[0].stdout, PlannedOutput::Pipe(0));
    assert_eq!(plan.stages[1].path_candidates, vec!["/bin/grep"]);
    assert_eq!(plan.stages[1].stdin, PlannedInput::Pipe(0));
    let PlannedOutput::File { path, append } = &plan.stages[1].stdout else {
        panic!("last stage should redirect to a file");
    };
    assert!(!append);
    assert!(path.is_absolute());
    assert!(path.ends_with("results.txt"));
}

#[test]
fn mixed_pipeline_wraps_builtin_in_child_shell() {
    let mut env = BTreeMap::from([("PATH".to_string(), "/bin".to_string())]);
    let mut backend = RecordingBackend::default();

    let (_status, _stdout, stderr) = drive("echo hello | grep ell\nexit\n", &mut env, &mut backend);

    assert!(stderr.is_empty(), "stderr: {stderr:?}");
    let plan = &backend.plans[0];
    assert_eq!(plan.stages[0].command, "echo");
    assert_eq!(plan.stages[0].path_candidates, vec!["/bin/sh"]);
    assert_eq!(plan.stages[0].argv[0..2], ["sh", "-c"]);
    assert_eq!(plan.stages[0].argv[2], "'echo' 'hello'");
    assert_eq!(plan.stages[0].stdout, PlannedOutput::Pipe(0));
    assert_eq!(plan.stages[1].stdin, PlannedInput::Pipe(0));
}

#[test]
fn external_pipeline_status_updates_dollar_question() {
    let mut env = BTreeMap::new();
    let mut backend = RecordingBackend::default();
    backend.outcomes.push_back(Ok(ExecutionResult {
        pids: vec![41, 42],
        statuses: vec![0, 7],
    }));

    let (_status, stdout, stderr) = drive(
        "ls | grep missing\necho status=$?\nexit\n",
        &mut env,
        &mut backend,
    );

    assert!(stderr.is_empty(), "stderr: {stderr:?}");
    assert!(stdout.contains("status=7\n"), "stdout: {stdout:?}");
}

#[test]
fn backend_not_found_reports_127_and_names_failed_command() {
    let mut env = BTreeMap::new();
    let mut backend = RecordingBackend::default();
    backend
        .outcomes
        .push_back(Err(ProcessError::CommandNotFound {
            command: "missing-tool".to_string(),
        }));

    let (_status, stdout, stderr) = drive(
        "missing-tool arg\necho status=$?\nexit\n",
        &mut env,
        &mut backend,
    );

    assert_eq!(stderr, "sh: command not found: missing-tool\n");
    assert!(stdout.contains("status=127\n"), "stdout: {stdout:?}");
}

#[test]
fn builtin_only_pipeline_does_not_cross_process_boundary() {
    let mut env = BTreeMap::new();
    let mut backend = RecordingBackend::default();

    let (_status, stdout, stderr) = drive(
        "echo hello | read VALUE\necho value=$VALUE\nexit\n",
        &mut env,
        &mut backend,
    );

    assert!(backend.plans.is_empty());
    assert!(stderr.is_empty(), "stderr: {stderr:?}");
    assert!(stdout.contains("value=hello\n"), "stdout: {stdout:?}");
}

#[test]
fn background_external_job_is_published_without_wait_status() {
    let mut env = BTreeMap::new();
    let mut backend = RecordingBackend::default();
    backend.outcomes.push_back(Ok(ExecutionResult {
        pids: vec![77],
        statuses: Vec::new(),
    }));

    let (_status, stdout, stderr) = drive("worker &\njobs\nexit\n", &mut env, &mut backend);

    assert!(stderr.is_empty(), "stderr: {stderr:?}");
    assert_eq!(stdout, "$ $ [1]  Running   worker &\n$ ");
    assert!(backend.plans[0].background);
}
