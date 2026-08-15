//! PMos implementation of the shell process backend.

use std::collections::BTreeSet;
use std::path::Path;

use crate::process::{
    ExecutionPlan, ExecutionResult, PlannedInput, PlannedOutput, ProcessBackend, ProcessError,
    ProcessIo,
};
use crate::spawn_wire::{encode_spawn_manifest_v1, SpawnWireManifest};

/// Narrow syscall boundary used by [`PmosProcessBackend`].
///
/// Keeping this boundary injectable makes fd ownership, PATH fallback, wait
/// status decoding, and rollback testable without host processes. Production
/// WASM uses [`WasmPmosSyscalls`].
pub trait PmosSyscalls {
    fn pipe(&mut self) -> Result<(u32, u32), i32>;
    fn open_read(&mut self, path: &Path) -> Result<u32, i32>;
    fn open_write(&mut self, path: &Path, append: bool) -> Result<u32, i32>;
    fn close(&mut self, fd: u32);
    fn read(&mut self, fd: u32, buffer: &mut [u8]) -> Result<usize, i32>;
    fn write(&mut self, fd: u32, buffer: &[u8]) -> Result<usize, i32>;
    fn set_nonblocking(&mut self, _fd: u32) -> Result<(), i32> {
        Ok(())
    }
    fn spawn_manifest(&mut self, blob: &[u8]) -> i32;
    fn wait(&mut self, pid: i32) -> Result<i64, i32>;
    /// Nonblocking child-status probe. `Ok(None)` means the child is still
    /// live; production implements this with `proc_wait(WNOHANG)`.
    fn try_wait(&mut self, _pid: i32) -> Result<Option<i64>, i32> {
        Err(abi::errno::ENOTSUP)
    }
    /// Drain bounded SIGCHLD records from the well-known signal channel.
    fn drain_sigchld(&mut self) -> Result<bool, i32> {
        Ok(false)
    }
    fn kill(&mut self, pid: i32, signal: i32) -> Result<(), i32>;
}

/// Process backend backed by the documented PMos extension imports.
pub struct PmosProcessBackend<S> {
    syscalls: S,
}

impl<S> PmosProcessBackend<S> {
    pub fn new(syscalls: S) -> Self {
        Self { syscalls }
    }

    pub fn into_syscalls(self) -> S {
        self.syscalls
    }
}

impl<S: PmosSyscalls> ProcessBackend for PmosProcessBackend<S> {
    fn execute(
        &mut self,
        plan: &ExecutionPlan,
        _io: ProcessIo<'_>,
    ) -> Result<ExecutionResult, ProcessError> {
        let mut parent_fds = BTreeSet::new();
        let mut pipes = Vec::with_capacity(plan.pipe_count);
        for _ in 0..plan.pipe_count {
            match self.syscalls.pipe() {
                Ok((read_fd, write_fd)) => {
                    parent_fds.insert(read_fd);
                    parent_fds.insert(write_fd);
                    pipes.push((read_fd, write_fd));
                }
                Err(_) => {
                    close_all(&mut self.syscalls, &mut parent_fds);
                    return Err(ProcessError::Io);
                }
            }
        }

        let mut resolved = Vec::with_capacity(plan.stages.len());
        for stage in &plan.stages {
            let stdin_fd = match &stage.stdin {
                PlannedInput::Parent => abi::fd::STDIN,
                PlannedInput::Pipe(index) => match pipes.get(*index) {
                    Some((read_fd, _)) => *read_fd,
                    None => {
                        close_all(&mut self.syscalls, &mut parent_fds);
                        return Err(ProcessError::Io);
                    }
                },
                PlannedInput::File(path) => match self.syscalls.open_read(path) {
                    Ok(fd) => {
                        parent_fds.insert(fd);
                        fd
                    }
                    Err(errno) => {
                        close_all(&mut self.syscalls, &mut parent_fds);
                        return Err(ProcessError::Redirection {
                            path: path.clone(),
                            errno,
                        });
                    }
                },
            };
            let stdout_fd = match &stage.stdout {
                PlannedOutput::Parent => abi::fd::STDOUT,
                PlannedOutput::Pipe(index) => match pipes.get(*index) {
                    Some((_, write_fd)) => *write_fd,
                    None => {
                        close_all(&mut self.syscalls, &mut parent_fds);
                        return Err(ProcessError::Io);
                    }
                },
                PlannedOutput::File { path, append } => {
                    match self.syscalls.open_write(path, *append) {
                        Ok(fd) => {
                            parent_fds.insert(fd);
                            fd
                        }
                        Err(errno) => {
                            close_all(&mut self.syscalls, &mut parent_fds);
                            return Err(ProcessError::Redirection {
                                path: path.clone(),
                                errno,
                            });
                        }
                    }
                }
            };
            resolved.push((stdin_fd, stdout_fd));
        }

        let mut pids = Vec::with_capacity(plan.stages.len());
        for (stage, (stdin_fd, stdout_fd)) in plan.stages.iter().zip(resolved) {
            let cwd = stage.cwd.to_string_lossy();
            let mut last_errno = abi::errno::ENOENT;
            let mut spawned = None;
            for candidate in &stage.path_candidates {
                let manifest = SpawnWireManifest {
                    path: candidate,
                    argv: &stage.argv,
                    env: &stage.env,
                    stdin_fd: Some(stdin_fd),
                    stdout_fd: Some(stdout_fd),
                    stderr_fd: Some(abi::fd::STDERR),
                    extra_fds: &[],
                    cwd: Some(cwd.as_ref()),
                    caps: None,
                };
                let blob = encode_spawn_manifest_v1(&manifest).map_err(|_| {
                    close_all(&mut self.syscalls, &mut parent_fds);
                    terminate_and_reap(&mut self.syscalls, &pids);
                    ProcessError::Spawn {
                        command: stage.command.clone(),
                        errno: abi::errno::EINVAL,
                    }
                })?;
                let result = self.syscalls.spawn_manifest(&blob);
                if result > 0 {
                    spawned = Some(result);
                    break;
                }
                last_errno = result.checked_neg().unwrap_or(abi::errno::EIO);
                if last_errno != abi::errno::ENOENT {
                    break;
                }
            }
            let Some(pid) = spawned else {
                close_all(&mut self.syscalls, &mut parent_fds);
                terminate_and_reap(&mut self.syscalls, &pids);
                return Err(spawn_error(&stage.command, last_errno));
            };
            pids.push(pid);
        }

        close_all(&mut self.syscalls, &mut parent_fds);
        // PMos does not yet expose an event-driven child-reaper to the shell.
        // Keep the historical synchronous `&` behaviour until it does; a
        // fire-and-forget child would otherwise become an unreaped zombie.
        let mut statuses = Vec::with_capacity(pids.len());
        for &pid in &pids {
            match self.syscalls.wait(pid) {
                Ok(packed) => statuses.push(shell_status(packed)),
                Err(errno) => {
                    terminate_and_reap(&mut self.syscalls, &pids);
                    return Err(ProcessError::Spawn {
                        command: plan
                            .stages
                            .last()
                            .map(|stage| stage.command.clone())
                            .unwrap_or_default(),
                        errno,
                    });
                }
            }
        }
        Ok(ExecutionResult { pids, statuses })
    }
}

fn close_all<S: PmosSyscalls>(syscalls: &mut S, fds: &mut BTreeSet<u32>) {
    for fd in core::mem::take(fds) {
        syscalls.close(fd);
    }
}

fn terminate_and_reap<S: PmosSyscalls>(syscalls: &mut S, pids: &[i32]) {
    for &pid in pids {
        let _ = syscalls.kill(pid, i32::from(abi::ext::sig::SIGKILL));
    }
    for &pid in pids {
        let _ = syscalls.wait(pid);
    }
}

fn spawn_error(command: &str, errno: i32) -> ProcessError {
    match errno {
        abi::errno::ENOENT => ProcessError::CommandNotFound {
            command: command.to_string(),
        },
        abi::errno::EACCES | abi::errno::ENOEXEC => ProcessError::PermissionDenied {
            command: command.to_string(),
        },
        _ => ProcessError::Spawn {
            command: command.to_string(),
            errno,
        },
    }
}

fn shell_status(packed: i64) -> i32 {
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

/// Production PMos syscall adapter.
#[cfg(target_arch = "wasm32")]
#[derive(Default)]
pub struct WasmPmosSyscalls {
    files: std::collections::BTreeMap<u32, std::fs::File>,
}

#[cfg(target_arch = "wasm32")]
impl PmosSyscalls for WasmPmosSyscalls {
    fn pipe(&mut self) -> Result<(u32, u32), i32> {
        let mut fds = [0_i32; 2];
        let result = unsafe { wasm_imports::ipc_pipe(fds.as_mut_ptr()) };
        if result < 0 {
            Err(-result)
        } else {
            Ok((fds[0] as u32, fds[1] as u32))
        }
    }

    fn open_read(&mut self, path: &Path) -> Result<u32, i32> {
        use std::os::fd::AsRawFd;

        let file = std::fs::File::open(path).map_err(io_errno)?;
        let fd = file.as_raw_fd() as u32;
        self.files.insert(fd, file);
        Ok(fd)
    }

    fn open_write(&mut self, path: &Path, append: bool) -> Result<u32, i32> {
        use std::os::fd::AsRawFd;

        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .append(append)
            .truncate(!append)
            .open(path)
            .map_err(io_errno)?;
        let fd = file.as_raw_fd() as u32;
        self.files.insert(fd, file);
        Ok(fd)
    }

    fn close(&mut self, fd: u32) {
        if self.files.remove(&fd).is_none() {
            let _ = unsafe { wasm_imports::fd_close(fd as i32) };
        }
    }

    fn read(&mut self, fd: u32, buffer: &mut [u8]) -> Result<usize, i32> {
        let mut nread = 0_u32;
        let iovec = wasm_imports::Iovec {
            buffer: buffer.as_mut_ptr(),
            length: buffer.len() as u32,
        };
        let result = unsafe { wasm_imports::fd_read(fd as i32, &iovec, 1, &mut nread) };
        if result == 0 {
            Ok(nread as usize)
        } else {
            Err(result)
        }
    }

    fn write(&mut self, fd: u32, buffer: &[u8]) -> Result<usize, i32> {
        let mut nwritten = 0_u32;
        let iovec = wasm_imports::Ciovec {
            buffer: buffer.as_ptr(),
            length: buffer.len() as u32,
        };
        let result = unsafe { wasm_imports::fd_write(fd as i32, &iovec, 1, &mut nwritten) };
        if result == 0 {
            Ok(nwritten as usize)
        } else {
            Err(result)
        }
    }

    fn set_nonblocking(&mut self, fd: u32) -> Result<(), i32> {
        let result = unsafe {
            wasm_imports::fd_fdstat_set_flags(fd as i32, abi::wasi::fdflags::NONBLOCK as i32)
        };
        if result == 0 {
            Ok(())
        } else {
            Err(result)
        }
    }

    fn spawn_manifest(&mut self, blob: &[u8]) -> i32 {
        unsafe { wasm_imports::proc_spawn_manifest(blob.as_ptr(), blob.len() as u32) }
    }

    fn wait(&mut self, pid: i32) -> Result<i64, i32> {
        let mut status = 0_i64;
        let result = unsafe { wasm_imports::proc_wait(pid, 0, &mut status) };
        if result < 0 {
            Err(-result)
        } else if result != pid {
            Err(abi::errno::ECHILD)
        } else {
            Ok(status)
        }
    }

    fn try_wait(&mut self, pid: i32) -> Result<Option<i64>, i32> {
        let mut status = 0_i64;
        let result = unsafe {
            wasm_imports::proc_wait(pid, abi::ext::wait_opts::WNOHANG as i32, &mut status)
        };
        if result == pid {
            Ok(Some(status))
        } else if result < 0 && -result == abi::errno::EAGAIN {
            Ok(None)
        } else if result < 0 {
            Err(-result)
        } else {
            Err(abi::errno::ECHILD)
        }
    }

    fn drain_sigchld(&mut self) -> Result<bool, i32> {
        let mut saw_sigchld = false;
        for _ in 0..16 {
            let mut records = [0_u8; 32];
            match self.read(abi::fd::SIGNAL, &mut records) {
                Ok(0) => break,
                Ok(count) if count <= records.len() => {
                    saw_sigchld |= records[..count].chunks_exact(2).any(|record| {
                        u16::from_le_bytes([record[0], record[1]]) == abi::ext::sig::SIGCHLD
                    });
                }
                Ok(_) => return Err(abi::errno::EIO),
                Err(errno) if errno == abi::errno::EAGAIN => break,
                Err(errno) => return Err(errno),
            }
        }
        Ok(saw_sigchld)
    }

    fn kill(&mut self, pid: i32, signal: i32) -> Result<(), i32> {
        let result = unsafe { wasm_imports::proc_kill(pid, signal) };
        if result < 0 {
            Err(-result)
        } else {
            Ok(())
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn io_errno(error: std::io::Error) -> i32 {
    error.raw_os_error().unwrap_or(abi::errno::EIO)
}

#[cfg(target_arch = "wasm32")]
mod wasm_imports {
    #[repr(C)]
    pub struct Iovec {
        pub buffer: *mut u8,
        pub length: u32,
    }

    #[repr(C)]
    pub struct Ciovec {
        pub buffer: *const u8,
        pub length: u32,
    }

    #[link(wasm_import_module = "pmos_ext")]
    extern "C" {
        pub fn ipc_pipe(fds: *mut i32) -> i32;
        pub fn proc_spawn_manifest(manifest: *const u8, len: u32) -> i32;
        pub fn proc_wait(pid: i32, options: i32, status: *mut i64) -> i32;
        pub fn proc_kill(pid: i32, signal: i32) -> i32;
    }

    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        pub fn fd_close(fd: i32) -> i32;
        pub fn fd_fdstat_set_flags(fd: i32, flags: i32) -> i32;
        pub fn fd_read(fd: i32, iovecs: *const Iovec, len: i32, nread: *mut u32) -> i32;
        pub fn fd_write(fd: i32, iovecs: *const Ciovec, len: i32, nwritten: *mut u32) -> i32;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};

    use super::{PmosProcessBackend, PmosSyscalls};
    use crate::process::{
        ExecutionPlan, PlannedInput, PlannedOutput, PlannedStage, ProcessBackend, ProcessIo,
    };

    #[derive(Default)]
    struct MockSyscalls {
        next_fd: u32,
        pipes: Vec<(u32, u32)>,
        closes: Vec<u32>,
        manifests: Vec<Vec<u8>>,
        spawn_results: VecDeque<i32>,
        waits: Vec<i32>,
        kills: Vec<(i32, i32)>,
    }

    impl PmosSyscalls for MockSyscalls {
        fn pipe(&mut self) -> Result<(u32, u32), i32> {
            let pair = (self.next_fd, self.next_fd + 1);
            self.next_fd += 2;
            self.pipes.push(pair);
            Ok(pair)
        }

        fn open_read(&mut self, _path: &Path) -> Result<u32, i32> {
            unreachable!()
        }

        fn open_write(&mut self, _path: &Path, _append: bool) -> Result<u32, i32> {
            unreachable!()
        }

        fn close(&mut self, fd: u32) {
            self.closes.push(fd);
        }

        fn read(&mut self, _fd: u32, _buffer: &mut [u8]) -> Result<usize, i32> {
            unreachable!()
        }

        fn write(&mut self, _fd: u32, _buffer: &[u8]) -> Result<usize, i32> {
            unreachable!()
        }

        fn spawn_manifest(&mut self, blob: &[u8]) -> i32 {
            self.manifests.push(blob.to_vec());
            self.spawn_results.pop_front().unwrap()
        }

        fn wait(&mut self, pid: i32) -> Result<i64, i32> {
            self.waits.push(pid);
            Ok((0x01_i64 << 40) | i64::from(pid == 22))
        }

        fn kill(&mut self, pid: i32, signal: i32) -> Result<(), i32> {
            self.kills.push((pid, signal));
            Ok(())
        }
    }

    fn stage(
        command: &str,
        paths: &[&str],
        stdin: PlannedInput,
        stdout: PlannedOutput,
    ) -> PlannedStage {
        PlannedStage {
            command: command.to_string(),
            path_candidates: paths.iter().map(|path| (*path).to_string()).collect(),
            argv: vec![command.to_string()],
            env: vec![("PATH".to_string(), "/bin".to_string())],
            cwd: PathBuf::from("/work"),
            stdin,
            stdout,
        }
    }

    #[test]
    fn pipeline_spawns_every_stage_before_wait_and_closes_parent_pipe() {
        let syscalls = MockSyscalls {
            next_fd: 10,
            spawn_results: VecDeque::from([21, 22]),
            ..MockSyscalls::default()
        };
        let mut backend = PmosProcessBackend::new(syscalls);
        let plan = ExecutionPlan {
            stages: vec![
                stage(
                    "cat",
                    &["/bin/cat"],
                    PlannedInput::Parent,
                    PlannedOutput::Pipe(0),
                ),
                stage(
                    "wc",
                    &["/bin/wc"],
                    PlannedInput::Pipe(0),
                    PlannedOutput::Parent,
                ),
            ],
            pipe_count: 1,
            background: false,
        };
        let mut input = Cursor::new(Vec::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let result = backend
            .execute(
                &plan,
                ProcessIo {
                    stdin: &mut input,
                    stdout: &mut output,
                    stderr: &mut error,
                },
            )
            .unwrap();
        assert_eq!(result.pids, vec![21, 22]);
        assert_eq!(result.statuses, vec![0, 1]);
        let syscalls = backend.into_syscalls();
        assert_eq!(syscalls.pipes, vec![(10, 11)]);
        assert_eq!(syscalls.closes, vec![10, 11]);
        assert_eq!(syscalls.waits, vec![21, 22]);
        assert_eq!(syscalls.manifests.len(), 2);
        assert_eq!(
            i32::from_le_bytes(syscalls.manifests[0][28..32].try_into().unwrap()),
            11
        );
        assert_eq!(
            i32::from_le_bytes(syscalls.manifests[1][24..28].try_into().unwrap()),
            10
        );
    }

    #[test]
    fn path_fallback_tries_only_enoent_candidates() {
        let syscalls = MockSyscalls {
            spawn_results: VecDeque::from([-abi::errno::ENOENT, 31]),
            ..MockSyscalls::default()
        };
        let mut backend = PmosProcessBackend::new(syscalls);
        let plan = ExecutionPlan {
            stages: vec![stage(
                "tool",
                &["/missing/tool", "/bin/tool"],
                PlannedInput::Parent,
                PlannedOutput::Parent,
            )],
            pipe_count: 0,
            background: false,
        };
        let mut input = Cursor::new(Vec::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let result = backend
            .execute(
                &plan,
                ProcessIo {
                    stdin: &mut input,
                    stdout: &mut output,
                    stderr: &mut error,
                },
            )
            .unwrap();
        assert_eq!(result.pids, vec![31]);
        assert_eq!(result.statuses, vec![0]);
        assert_eq!(backend.into_syscalls().manifests.len(), 2);
    }

    #[test]
    fn later_spawn_failure_kills_and_reaps_published_children() {
        let syscalls = MockSyscalls {
            next_fd: 20,
            spawn_results: VecDeque::from([41, -abi::errno::EACCES]),
            ..MockSyscalls::default()
        };
        let mut backend = PmosProcessBackend::new(syscalls);
        let plan = ExecutionPlan {
            stages: vec![
                stage(
                    "cat",
                    &["/bin/cat"],
                    PlannedInput::Parent,
                    PlannedOutput::Pipe(0),
                ),
                stage(
                    "private",
                    &["/bin/private"],
                    PlannedInput::Pipe(0),
                    PlannedOutput::Parent,
                ),
            ],
            pipe_count: 1,
            background: false,
        };
        let mut input = Cursor::new(Vec::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let result = backend.execute(
            &plan,
            ProcessIo {
                stdin: &mut input,
                stdout: &mut output,
                stderr: &mut error,
            },
        );
        assert!(matches!(
            result,
            Err(crate::ProcessError::PermissionDenied { .. })
        ));
        let syscalls = backend.into_syscalls();
        assert_eq!(syscalls.closes, vec![20, 21]);
        assert_eq!(
            syscalls.kills,
            vec![(41, i32::from(abi::ext::sig::SIGKILL))]
        );
        assert_eq!(syscalls.waits, vec![41]);
    }
}
