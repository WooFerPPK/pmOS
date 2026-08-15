//! Persistent `/bin/sh` session for the graphical terminal.

use sh::{encode_spawn_manifest_v1, PmosSyscalls, SpawnWireManifest, DEFAULT_PATH};

use crate::terminal::{CommandRunResult, CommandRunner};

const PROMPT_SENTINEL: &[u8] = b"\x1ePMOS:READY:7f4a\x1f";
const OUTPUT_READS_PER_TURN: usize = 16;
const INPUT_WRITES_PER_TURN: usize = 16;
const MAX_PENDING_INPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StepwiseShellUpdate {
    pub output: Vec<u8>,
    pub ready: bool,
    pub completed: bool,
    pub exited: bool,
    pub status: i32,
}

/// Event-loop-shaped command transport used only by the production terminal.
/// Native terminal fixtures retain the synchronous [`CommandRunner`] seam.
pub trait StepwiseCommandRunner {
    fn start_command(&mut self, line: &str) -> Result<(), i32>;
    fn send_input(&mut self, bytes: &[u8]) -> Result<(), i32>;
    fn flush_input(&mut self) -> Result<(), i32>;
    fn is_ready(&self) -> bool;
    fn readable_output_fd(&self) -> Option<u32>;
    fn signal_fd(&self) -> Option<u32>;
    fn writable_input_fd(&self) -> Option<u32>;
    fn drain_output(&mut self) -> StepwiseShellUpdate;
    fn terminate(&mut self);
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ShellState {
    Starting,
    Idle,
    Running,
    Reaping,
}

/// One isolated shell Worker connected to the terminal by kernel pipes.
pub struct PmosShellSession<S: PmosSyscalls> {
    syscalls: S,
    pid: i32,
    stdin_fd: u32,
    output_fd: u32,
    pending: Vec<u8>,
    pending_input: Vec<u8>,
    state: ShellState,
    alive: bool,
}

impl<S: PmosSyscalls> PmosShellSession<S> {
    /// Spawn the shell and consume its initial private prompt marker.
    pub fn start(syscalls: S) -> Result<Self, i32> {
        let inherited_timezone = std::env::var("TZ").ok();
        Self::start_with_timezone(syscalls, inherited_timezone.as_deref())
    }

    /// Production constructor: return after spawn with nonblocking pipe ends.
    /// The window loop consumes the initial prompt sentinel through
    /// [`StepwiseCommandRunner::drain_output`].
    pub fn start_stepwise(syscalls: S) -> Result<Self, i32> {
        let inherited_timezone = std::env::var("TZ").ok();
        let mut session = Self::spawn_with_timezone(syscalls, inherited_timezone.as_deref())?;
        session.syscalls.set_nonblocking(session.stdin_fd)?;
        session.syscalls.set_nonblocking(session.output_fd)?;
        Ok(session)
    }

    fn start_with_timezone(syscalls: S, inherited_timezone: Option<&str>) -> Result<Self, i32> {
        let mut session = Self::spawn_with_timezone(syscalls, inherited_timezone)?;
        match session.read_until_prompt() {
            Ok(Some(_)) => {
                session.state = ShellState::Idle;
                Ok(session)
            }
            Ok(None) => {
                session.alive = false;
                Err(abi::errno::EIO)
            }
            Err(errno) => Err(errno),
        }
    }

    fn spawn_with_timezone(mut syscalls: S, inherited_timezone: Option<&str>) -> Result<Self, i32> {
        let (child_stdin, terminal_stdin) = syscalls.pipe()?;
        let (terminal_output, child_output) = match syscalls.pipe() {
            Ok(pair) => pair,
            Err(errno) => {
                syscalls.close(child_stdin);
                syscalls.close(terminal_stdin);
                return Err(errno);
            }
        };
        let argv = vec!["sh".to_string()];
        let env = vec![
            ("PATH".to_string(), DEFAULT_PATH.to_string()),
            (
                "PS1".to_string(),
                String::from_utf8_lossy(PROMPT_SENTINEL).into_owned(),
            ),
            ("PWD".to_string(), "/".to_string()),
            (
                "TZ".to_string(),
                preferences::normalize_timezone_name(inherited_timezone).to_string(),
            ),
        ];
        let blob = match encode_spawn_manifest_v1(&SpawnWireManifest {
            path: "/bin/sh",
            argv: &argv,
            env: &env,
            stdin_fd: Some(child_stdin),
            stdout_fd: Some(child_output),
            stderr_fd: Some(child_output),
            extra_fds: &[],
            cwd: Some("/"),
            caps: None,
        }) {
            Ok(blob) => blob,
            Err(_) => {
                syscalls.close(child_stdin);
                syscalls.close(terminal_stdin);
                syscalls.close(terminal_output);
                syscalls.close(child_output);
                return Err(abi::errno::EINVAL);
            }
        };
        let pid = syscalls.spawn_manifest(&blob);
        syscalls.close(child_stdin);
        syscalls.close(child_output);
        if pid <= 0 {
            syscalls.close(terminal_stdin);
            syscalls.close(terminal_output);
            return Err(pid.checked_neg().unwrap_or(abi::errno::EIO));
        }

        Ok(Self {
            syscalls,
            pid,
            stdin_fd: terminal_stdin,
            output_fd: terminal_output,
            pending: Vec::new(),
            pending_input: Vec::new(),
            state: ShellState::Starting,
            alive: true,
        })
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), i32> {
        let mut written = 0;
        while written < bytes.len() {
            let count = self.syscalls.write(self.stdin_fd, &bytes[written..])?;
            if count == 0 {
                return Err(abi::errno::EIO);
            }
            written += count;
        }
        Ok(())
    }

    /// Return output before the next prompt, or `None` after pipe EOF.
    fn read_until_prompt(&mut self) -> Result<Option<Vec<u8>>, i32> {
        loop {
            if let Some(offset) = find_bytes(&self.pending, PROMPT_SENTINEL) {
                let output = self.pending[..offset].to_vec();
                self.pending.drain(..offset + PROMPT_SENTINEL.len());
                return Ok(Some(output));
            }
            let mut chunk = [0_u8; 4096];
            let count = self.syscalls.read(self.output_fd, &mut chunk)?;
            if count == 0 {
                return Ok(None);
            }
            self.pending.extend_from_slice(&chunk[..count]);
        }
    }

    fn finish_after_eof(&mut self, output: Vec<u8>) -> CommandRunResult {
        self.alive = false;
        let status = self
            .syscalls
            .wait(self.pid)
            .map(decode_wait_status)
            .unwrap_or(125);
        CommandRunResult {
            stdout: output,
            stderr: Vec::new(),
            status,
            exited: true,
        }
    }

    fn fatal(&mut self, errno: i32) -> CommandRunResult {
        if self.alive {
            let _ = self
                .syscalls
                .kill(self.pid, i32::from(abi::ext::sig::SIGKILL));
            let _ = self.syscalls.wait(self.pid);
            self.alive = false;
        }
        CommandRunResult {
            stdout: Vec::new(),
            stderr: format!("term: shell transport failed (errno {errno})\n").into_bytes(),
            status: 125,
            exited: true,
        }
    }

    fn flush_input_bounded(&mut self) -> Result<(), i32> {
        for _ in 0..INPUT_WRITES_PER_TURN {
            if self.pending_input.is_empty() {
                return Ok(());
            }
            match self.syscalls.write(self.stdin_fd, &self.pending_input) {
                Ok(0) => return Err(abi::errno::EIO),
                Ok(written) if written <= self.pending_input.len() => {
                    self.pending_input.drain(..written);
                }
                Ok(_) => return Err(abi::errno::EIO),
                Err(errno) if errno == abi::errno::EAGAIN => return Ok(()),
                Err(errno) => return Err(errno),
            }
        }
        Ok(())
    }

    fn queue_input(&mut self, bytes: &[u8]) -> Result<(), i32> {
        if self.pending_input.len().saturating_add(bytes.len()) > MAX_PENDING_INPUT_BYTES {
            return Err(abi::errno::ENOSPC);
        }
        self.pending_input.extend_from_slice(bytes);
        self.flush_input_bounded()
    }

    fn take_prompt_output(&mut self, output: &mut Vec<u8>) -> Option<bool> {
        if self.state == ShellState::Idle {
            return None;
        }
        let offset = find_bytes(&self.pending, PROMPT_SENTINEL)?;
        let completed = self.state == ShellState::Running;
        output.extend(self.pending.drain(..offset));
        self.pending.drain(..PROMPT_SENTINEL.len());
        self.state = ShellState::Idle;
        Some(completed)
    }

    fn stream_safe_prefix(&mut self, output: &mut Vec<u8>) {
        let retained = if self.state == ShellState::Idle {
            0
        } else {
            PROMPT_SENTINEL.len().saturating_sub(1)
        };
        let safe = self.pending.len().saturating_sub(retained);
        output.extend(self.pending.drain(..safe));
    }
}

impl<S: PmosSyscalls> CommandRunner for PmosShellSession<S> {
    fn run_command(&mut self, line: &str) -> CommandRunResult {
        if !self.alive {
            return CommandRunResult {
                stderr: b"term: shell process has exited\n".to_vec(),
                status: 125,
                exited: true,
                ..CommandRunResult::default()
            };
        }
        let mut command = Vec::with_capacity(line.len() + 1);
        command.extend_from_slice(line.as_bytes());
        command.push(b'\n');
        if let Err(errno) = self.write_all(&command) {
            return self.fatal(errno);
        }
        match self.read_until_prompt() {
            Ok(Some(output)) => CommandRunResult {
                stdout: output,
                stderr: Vec::new(),
                status: 0,
                exited: false,
            },
            Ok(None) => {
                let output = core::mem::take(&mut self.pending);
                self.finish_after_eof(output)
            }
            Err(errno) => self.fatal(errno),
        }
    }
}

impl<S: PmosSyscalls> StepwiseCommandRunner for PmosShellSession<S> {
    fn start_command(&mut self, line: &str) -> Result<(), i32> {
        if !self.alive || self.state != ShellState::Idle {
            return Err(abi::errno::EAGAIN);
        }
        self.state = ShellState::Running;
        let mut command = Vec::with_capacity(line.len() + 1);
        command.extend_from_slice(line.as_bytes());
        command.push(b'\n');
        if let Err(errno) = self.queue_input(&command) {
            self.state = ShellState::Idle;
            return Err(errno);
        }
        Ok(())
    }

    fn send_input(&mut self, bytes: &[u8]) -> Result<(), i32> {
        if !self.alive || self.state != ShellState::Running {
            return Err(abi::errno::EINVAL);
        }
        self.queue_input(bytes)
    }

    fn flush_input(&mut self) -> Result<(), i32> {
        self.flush_input_bounded()
    }

    fn is_ready(&self) -> bool {
        self.alive && self.state != ShellState::Starting
    }

    fn readable_output_fd(&self) -> Option<u32> {
        (self.alive && self.state != ShellState::Reaping).then_some(self.output_fd)
    }

    fn signal_fd(&self) -> Option<u32> {
        self.alive.then_some(abi::fd::SIGNAL)
    }

    fn writable_input_fd(&self) -> Option<u32> {
        (!self.pending_input.is_empty()).then_some(self.stdin_fd)
    }

    fn drain_output(&mut self) -> StepwiseShellUpdate {
        let mut update = StepwiseShellUpdate::default();
        if !self.alive {
            return update;
        }
        let saw_sigchld = match self.syscalls.drain_sigchld() {
            Ok(saw) => saw,
            Err(errno) => {
                update.output.extend(
                    format!("term: signal transport failed (errno {errno})\n").into_bytes(),
                );
                self.terminate();
                update.completed = true;
                update.exited = true;
                update.status = 125;
                return update;
            }
        };
        for _ in 0..OUTPUT_READS_PER_TURN {
            if self.state == ShellState::Reaping {
                break;
            }
            if let Some(completed) = self.take_prompt_output(&mut update.output) {
                update.ready = true;
                update.completed = completed;
                self.stream_safe_prefix(&mut update.output);
                break;
            }
            let mut chunk = [0u8; 4096];
            match self.syscalls.read(self.output_fd, &mut chunk) {
                Ok(0) => {
                    update.output.extend(core::mem::take(&mut self.pending));
                    update.completed = true;
                    self.state = ShellState::Reaping;
                    break;
                }
                Ok(count) if count <= chunk.len() => {
                    self.pending.extend_from_slice(&chunk[..count]);
                }
                Ok(_) => {
                    update.output.extend(
                        format!("term: shell transport failed (errno {})\n", abi::errno::EIO)
                            .into_bytes(),
                    );
                    self.terminate();
                    update.completed = true;
                    update.exited = true;
                    update.status = 125;
                    return update;
                }
                Err(errno) if errno == abi::errno::EAGAIN => break,
                Err(errno) => {
                    update.output.extend(
                        format!("term: shell transport failed (errno {errno})\n").into_bytes(),
                    );
                    self.terminate();
                    update.completed = true;
                    update.exited = true;
                    update.status = 125;
                    return update;
                }
            }
        }
        if self.state != ShellState::Reaping {
            if let Some(completed) = self.take_prompt_output(&mut update.output) {
                update.ready = true;
                update.completed = completed;
                self.stream_safe_prefix(&mut update.output);
            } else {
                self.stream_safe_prefix(&mut update.output);
            }
        }
        if self.state == ShellState::Reaping || saw_sigchld {
            match self.syscalls.try_wait(self.pid) {
                Ok(Some(status)) => {
                    self.alive = false;
                    self.state = ShellState::Idle;
                    update.completed = true;
                    update.exited = true;
                    update.status = decode_wait_status(status);
                }
                Ok(None) | Err(abi::errno::EAGAIN) => {}
                Err(errno) => {
                    update
                        .output
                        .extend(format!("term: shell reap failed (errno {errno})\n").into_bytes());
                    self.terminate();
                    update.completed = true;
                    update.exited = true;
                    update.status = 125;
                }
            }
        }
        update
    }

    fn terminate(&mut self) {
        if self.alive {
            self.state = ShellState::Reaping;
            let killed = self
                .syscalls
                .kill(self.pid, i32::from(abi::ext::sig::SIGKILL));
            if killed.is_ok() {
                match self.syscalls.try_wait(self.pid) {
                    Ok(Some(_)) | Err(abi::errno::ECHILD) => {
                        self.alive = false;
                        self.state = ShellState::Idle;
                    }
                    Ok(None) | Err(_) => {}
                }
            }
        }
    }
}

impl<S: PmosSyscalls> Drop for PmosShellSession<S> {
    fn drop(&mut self) {
        self.syscalls.close(self.stdin_fd);
        self.syscalls.close(self.output_fd);
        if self.alive {
            let _ = self
                .syscalls
                .kill(self.pid, i32::from(abi::ext::sig::SIGKILL));
            let _ = self.syscalls.wait(self.pid);
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_wait_status(packed: i64) -> i32 {
    let bits = packed as u64;
    let flags = ((bits >> 40) & 0xff) as u8;
    if flags & 0x01 != 0 {
        bits as u32 as i32
    } else if flags & 0x02 != 0 {
        128 + ((bits >> 32) & 0xff) as i32
    } else {
        125
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;

    use super::{PmosShellSession, StepwiseCommandRunner, OUTPUT_READS_PER_TURN, PROMPT_SENTINEL};
    use crate::terminal::CommandRunner;
    use sh::PmosSyscalls;

    struct MockSyscalls {
        pipes: VecDeque<(u32, u32)>,
        reads: VecDeque<Result<Vec<u8>, i32>>,
        writes: Vec<Vec<u8>>,
        nonblocking: Vec<u32>,
        sigchld: VecDeque<Result<bool, i32>>,
        try_waits: VecDeque<Result<Option<i64>, i32>>,
        try_wait_pids: Vec<i32>,
        blocking_waits: usize,
        kills: Vec<(i32, i32)>,
        closed: Vec<u32>,
        spawned: Vec<Vec<u8>>,
    }

    impl PmosSyscalls for MockSyscalls {
        fn pipe(&mut self) -> Result<(u32, u32), i32> {
            Ok(self.pipes.pop_front().unwrap())
        }

        fn open_read(&mut self, _path: &Path) -> Result<u32, i32> {
            unreachable!()
        }

        fn open_write(&mut self, _path: &Path, _append: bool) -> Result<u32, i32> {
            unreachable!()
        }

        fn close(&mut self, fd: u32) {
            self.closed.push(fd);
        }

        fn read(&mut self, _fd: u32, buffer: &mut [u8]) -> Result<usize, i32> {
            let bytes = self.reads.pop_front().unwrap_or(Err(abi::errno::EAGAIN))?;
            buffer[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }

        fn write(&mut self, _fd: u32, buffer: &[u8]) -> Result<usize, i32> {
            self.writes.push(buffer.to_vec());
            Ok(buffer.len())
        }

        fn set_nonblocking(&mut self, fd: u32) -> Result<(), i32> {
            self.nonblocking.push(fd);
            Ok(())
        }

        fn try_wait(&mut self, pid: i32) -> Result<Option<i64>, i32> {
            self.try_wait_pids.push(pid);
            self.try_waits.pop_front().unwrap_or(Ok(None))
        }

        fn drain_sigchld(&mut self) -> Result<bool, i32> {
            self.sigchld.pop_front().unwrap_or(Ok(false))
        }

        fn spawn_manifest(&mut self, blob: &[u8]) -> i32 {
            self.spawned.push(blob.to_vec());
            42
        }

        fn wait(&mut self, _pid: i32) -> Result<i64, i32> {
            self.blocking_waits += 1;
            Ok(0x01_i64 << 40)
        }

        fn kill(&mut self, pid: i32, signal: i32) -> Result<(), i32> {
            self.kills.push((pid, signal));
            Ok(())
        }
    }

    #[test]
    fn persistent_session_strips_split_prompt_and_reuses_child() {
        let mut first_prompt = PROMPT_SENTINEL[..5].to_vec();
        let second_prompt = PROMPT_SENTINEL[5..].to_vec();
        first_prompt.shrink_to_fit();
        let mock = MockSyscalls {
            pipes: VecDeque::from([(10, 11), (12, 13)]),
            reads: VecDeque::from([
                Ok(first_prompt),
                Ok(second_prompt),
                Ok([b"hello\n".as_slice(), PROMPT_SENTINEL].concat()),
            ]),
            writes: Vec::new(),
            nonblocking: Vec::new(),
            sigchld: VecDeque::new(),
            try_waits: VecDeque::new(),
            try_wait_pids: Vec::new(),
            blocking_waits: 0,
            kills: Vec::new(),
            closed: Vec::new(),
            spawned: Vec::new(),
        };
        let mut session = PmosShellSession::start(mock).unwrap();
        let result = session.run_command("echo hello");
        assert_eq!(result.stdout, b"hello\n");
        assert!(!result.exited);
        assert_eq!(session.syscalls.writes, vec![b"echo hello\n".to_vec()]);
        assert!(session.syscalls.nonblocking.is_empty());
        assert_eq!(session.syscalls.closed, vec![10, 13]);
    }

    #[test]
    fn child_shell_manifest_inherits_only_a_validated_timezone() {
        let mock = MockSyscalls {
            pipes: VecDeque::from([(10, 11), (12, 13)]),
            reads: VecDeque::from([Ok(PROMPT_SENTINEL.to_vec())]),
            writes: Vec::new(),
            nonblocking: Vec::new(),
            sigchld: VecDeque::new(),
            try_waits: VecDeque::new(),
            try_wait_pids: Vec::new(),
            blocking_waits: 0,
            kills: Vec::new(),
            closed: Vec::new(),
            spawned: Vec::new(),
        };
        let session =
            PmosShellSession::start_with_timezone(mock, Some("America/New_York")).unwrap();
        let environment = decode_environment(&session.syscalls.spawned[0]);
        assert_eq!(
            environment,
            vec![
                ("PATH".to_string(), sh::DEFAULT_PATH.to_string()),
                (
                    "PS1".to_string(),
                    String::from_utf8_lossy(PROMPT_SENTINEL).into_owned(),
                ),
                ("PWD".to_string(), "/".to_string()),
                ("TZ".to_string(), "America/New_York".to_string()),
            ],
        );

        let mock = MockSyscalls {
            pipes: VecDeque::from([(20, 21), (22, 23)]),
            reads: VecDeque::from([Ok(PROMPT_SENTINEL.to_vec())]),
            writes: Vec::new(),
            nonblocking: Vec::new(),
            sigchld: VecDeque::new(),
            try_waits: VecDeque::new(),
            try_wait_pids: Vec::new(),
            blocking_waits: 0,
            kills: Vec::new(),
            closed: Vec::new(),
            spawned: Vec::new(),
        };
        let session = PmosShellSession::start_with_timezone(mock, Some("unsupported")).unwrap();
        assert!(decode_environment(&session.syscalls.spawned[0])
            .contains(&("TZ".to_string(), "UTC".to_string())));
    }

    fn stepwise_mock(reads: impl IntoIterator<Item = Result<Vec<u8>, i32>>) -> MockSyscalls {
        MockSyscalls {
            pipes: VecDeque::from([(10, 11), (12, 13)]),
            reads: reads.into_iter().collect(),
            writes: Vec::new(),
            nonblocking: Vec::new(),
            sigchld: VecDeque::new(),
            try_waits: VecDeque::new(),
            try_wait_pids: Vec::new(),
            blocking_waits: 0,
            kills: Vec::new(),
            closed: Vec::new(),
            spawned: Vec::new(),
        }
    }

    #[test]
    fn stepwise_start_returns_before_prompt_and_only_it_sets_nonblocking() {
        let session = PmosShellSession::start_stepwise(stepwise_mock([])).unwrap();

        assert_eq!(session.state, super::ShellState::Starting);
        assert_eq!(session.syscalls.nonblocking, vec![11, 12]);
        assert!(session.syscalls.reads.is_empty());
    }

    #[test]
    fn fragmented_start_and_command_prompt_drive_stepwise_state() {
        let mut session = PmosShellSession::start_stepwise(stepwise_mock([
            Ok(PROMPT_SENTINEL[..5].to_vec()),
            Err(abi::errno::EAGAIN),
        ]))
        .unwrap();

        let first = session.drain_output();
        assert!(!first.ready);
        assert!(first.output.is_empty());

        session
            .syscalls
            .reads
            .extend([Ok(PROMPT_SENTINEL[5..].to_vec())]);
        let ready = session.drain_output();
        assert!(ready.ready);
        assert!(!ready.completed);
        assert!(ready.output.is_empty());
        assert!(session.is_ready());

        session.start_command("read answer").unwrap();
        assert_eq!(session.syscalls.writes, vec![b"read answer\n".to_vec()]);
        session.send_input(b"yes\n").unwrap();
        assert_eq!(
            session.syscalls.writes,
            vec![b"read answer\n".to_vec(), b"yes\n".to_vec()]
        );

        let split = PROMPT_SENTINEL.len() / 2;
        session.syscalls.reads.extend([
            Ok([b"accepted yes\n".as_slice(), &PROMPT_SENTINEL[..split]].concat()),
            Err(abi::errno::EAGAIN),
        ]);
        let partial = session.drain_output();
        assert!(!partial.completed);

        session
            .syscalls
            .reads
            .push_back(Ok(PROMPT_SENTINEL[split..].to_vec()));
        let complete = session.drain_output();
        assert!(complete.completed);
        assert!(!complete.exited);
        assert_eq!(
            [partial.output, complete.output].concat(),
            b"accepted yes\n"
        );
        assert!(session.is_ready());
    }

    #[test]
    fn idle_background_output_and_read_budget_prevent_ready_spin_monopoly() {
        let mut session =
            PmosShellSession::start_stepwise(stepwise_mock([Ok(PROMPT_SENTINEL.to_vec())]))
                .unwrap();
        assert!(session.drain_output().ready);
        for _ in 0..=OUTPUT_READS_PER_TURN {
            session.syscalls.reads.push_back(Ok(b"x".to_vec()));
        }

        let update = session.drain_output();
        assert_eq!(update.output, vec![b'x'; OUTPUT_READS_PER_TURN]);
        assert_eq!(session.syscalls.reads.len(), 1);
        assert!(!update.completed);
    }

    #[test]
    fn idle_shell_eof_is_reaped_and_exits_terminal_transport() {
        let mut session = PmosShellSession::start_stepwise(stepwise_mock([
            Ok(PROMPT_SENTINEL.to_vec()),
            Ok(Vec::new()),
        ]))
        .unwrap();
        assert!(session.drain_output().ready);
        session
            .syscalls
            .try_waits
            .extend([Ok(None), Ok(Some(0x01_i64 << 40))]);
        session.syscalls.sigchld.extend([Ok(false), Ok(true)]);

        let eof = session.drain_output();
        assert!(eof.completed);
        assert!(!eof.exited);
        assert_eq!(session.readable_output_fd(), None);

        let reaped = session.drain_output();
        assert!(reaped.completed);
        assert!(reaped.exited);
        assert_eq!(reaped.status, 0);
        assert!(!session.alive);
    }

    #[test]
    fn idle_child_crash_reaps_on_sigchld_even_before_pipe_eof() {
        let mut session =
            PmosShellSession::start_stepwise(stepwise_mock([Ok(PROMPT_SENTINEL.to_vec())]))
                .unwrap();
        assert!(session.drain_output().ready);
        session.syscalls.sigchld.push_back(Ok(true));
        session
            .syscalls
            .try_waits
            .push_back(Ok(Some((0x02_i64 << 40) | (9_i64 << 32))));

        let update = session.drain_output();
        assert!(update.completed);
        assert!(update.exited);
        assert_eq!(update.status, 137);
        assert!(!session.alive);
    }

    #[test]
    fn repeated_stepwise_teardown_kills_and_reaps_without_blocking_waits() {
        for _ in 0..32 {
            let mut mock = stepwise_mock([]);
            mock.try_waits.push_back(Ok(Some(0x01_i64 << 40)));
            let mut session = PmosShellSession::start_stepwise(mock).unwrap();

            session.terminate();

            assert!(!session.alive);
            assert_eq!(
                session.syscalls.kills,
                vec![(42, i32::from(abi::ext::sig::SIGKILL))]
            );
            assert_eq!(session.syscalls.try_wait_pids, vec![42]);
            assert_eq!(session.syscalls.blocking_waits, 0);
        }
    }

    fn decode_environment(blob: &[u8]) -> Vec<(String, String)> {
        use abi::ext::spawn_v1 as wire;

        let read_u16 = |offset: usize| {
            u16::from_le_bytes(blob[offset..offset + 2].try_into().unwrap()) as usize
        };
        let mut offset =
            wire::HEADER_LEN + read_u16(wire::OFF_PATH_LEN) + read_u16(wire::OFF_CWD_LEN);
        for _ in 0..read_u16(wire::OFF_ARGC) {
            let len = read_u16(offset);
            offset += 2 + len;
        }
        let mut environment = Vec::new();
        for _ in 0..read_u16(wire::OFF_ENVC) {
            let key_len = read_u16(offset);
            let value_len = read_u16(offset + 2);
            offset += 4;
            let key = std::str::from_utf8(&blob[offset..offset + key_len])
                .unwrap()
                .to_string();
            offset += key_len;
            let value = std::str::from_utf8(&blob[offset..offset + value_len])
                .unwrap()
                .to_string();
            offset += value_len;
            environment.push((key, value));
        }
        environment
    }
}
