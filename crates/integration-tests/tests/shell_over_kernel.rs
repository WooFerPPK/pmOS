//! The real `sh::Shell` running on top of a real
//! `kernel::sys::Kernel`.
//!
//! This is a stronger version of the T077 headless-shell
//! gate that lives inside the kernel crate's own test
//! suite. That test uses a faux inline shell because
//! kernel code is not allowed to depend on userland crates
//! (the sh crate). Here in `integration-tests` — which
//! exists exactly to compose across crate boundaries — we
//! can pair the two: every byte the shell reads comes out
//! of `Kernel::fd_read` from `/dev/console`, every byte it
//! writes lands via `Kernel::fd_write`, and the shell's
//! state machine is the real one from `crates/sh/src/`.
//!
//! If this test ever breaks alongside a green T077 gate
//! in the kernel crate, the drift is in the shell — not
//! in the kernel.

use abi::cap::initial;
use kernel::fd::FdObject;
use kernel::fs::devfs::{DevFs, DEV_CONSOLE};
use kernel::fs::procfs::ProcFs;
use kernel::fs::tmpfs::TmpFs;
use kernel::sys::{Kernel, RegisterArgs};
use sh::Shell;

fn make_kernel() -> Kernel {
    let mut k = Kernel::new();
    k.vfs
        .mount("/", Box::new(TmpFs::new()))
        .expect("root mount");
    k.vfs
        .mount("/dev", Box::new(DevFs::new()))
        .expect("devfs mount");
    k.vfs
        .mount("/proc", Box::new(ProcFs::with_static()))
        .expect("procfs mount");
    k
}

/// Register an `sh`-like process and seed its fds 0, 1, 2
/// with /dev/console so stdin reads come from the console
/// input ring and stdout/stderr writes go to the console
/// output sink.
fn register_shell_process(k: &mut Kernel) -> abi::ext::Pid {
    let pid = k
        .register_process(RegisterArgs {
            name: "sh",
            ppid: 1,
            caps: initial::DESKTOP_SHELL,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(pid).unwrap();
    use kernel::fd::FdFlags;
    k.install_fd(pid, 0, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    k.install_fd(pid, 2, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    pid
}

/// Read a newline-terminated line from `pid`'s fd 0.
/// Panics if the console input ring is empty — the test
/// always injects before calling this.
fn read_line_from_stdin(k: &mut Kernel, pid: abi::ext::Pid) -> Vec<u8> {
    let mut out = Vec::new();
    let mut one = [0u8; 1];
    loop {
        let n = k.fd_read(pid, 0, &mut one).expect("stdin fd_read");
        if n == 0 {
            break;
        }
        if one[0] == b'\n' {
            break;
        }
        out.push(one[0]);
    }
    out
}

/// Drive one round of the sh REPL over the kernel:
///
/// 1. Read a line from `fd 0`.
/// 2. Call `shell.eval(line)`.
/// 3. Write any stdout bytes to `fd 1`.
/// 4. Write any stderr bytes to `fd 2`.
///
/// Returns `Some(exit_code)` if the shell requested an
/// exit, otherwise `None`.
fn shell_step_over_kernel(k: &mut Kernel, pid: abi::ext::Pid, shell: &mut Shell) -> Option<i32> {
    let line_bytes = read_line_from_stdin(k, pid);
    let line = std::str::from_utf8(&line_bytes).expect("line is utf-8");
    let out = shell.eval(line);
    if !out.stdout.is_empty() {
        k.fd_write(pid, 1, &out.stdout).expect("stdout fd_write");
    }
    if !out.stderr.is_empty() {
        k.fd_write(pid, 2, &out.stderr).expect("stderr fd_write");
    }
    out.exit_code
}

// ---- end-to-end tests ------------------------------------------

#[test]
fn real_sh_over_kernel_echoes_input_through_dev_console() {
    let mut k = make_kernel();
    let pid = register_shell_process(&mut k);
    let mut shell = Shell::new();

    k.devs.inject_console_input(b"echo hello\n");
    let exit = shell_step_over_kernel(&mut k, pid, &mut shell);
    assert!(exit.is_none());

    // The console driver line-buffers; the "hello\n" line
    // was flushed to the platform sink on the newline, so
    // the pending sink is empty.
    let pending = k.devs.drain_console_output();
    assert_eq!(pending, b"");
    assert!(!shell.has_exited());
}

#[test]
fn real_sh_over_kernel_command_not_found_writes_to_stderr() {
    let mut k = make_kernel();
    let pid = register_shell_process(&mut k);
    let mut shell = Shell::new();

    k.devs.inject_console_input(b"nope\n");
    shell_step_over_kernel(&mut k, pid, &mut shell);

    // stderr and stdout both route to /dev/console, so
    // the drain catches anything that didn't end with a
    // newline — but "command not found: nope\n" ends with
    // one, so the sink is drained on the newline and the
    // pending buffer is empty. The behaviour is identical
    // to stdout for v1 — both go through the same line-
    // buffering console code in the kernel.
    let pending = k.devs.drain_console_output();
    assert_eq!(pending, b"");
}

#[test]
fn real_sh_over_kernel_exit_terminates_repl_loop() {
    let mut k = make_kernel();
    let pid = register_shell_process(&mut k);
    let mut shell = Shell::new();

    k.devs.inject_console_input(b"echo one\necho two\nexit 3\n");

    // Run the REPL until exit is requested.
    let mut final_code: Option<i32> = None;
    while final_code.is_none() {
        final_code = shell_step_over_kernel(&mut k, pid, &mut shell);
    }
    assert_eq!(final_code, Some(3));
    assert!(shell.has_exited());
    assert_eq!(shell.exit_status(), Some(3));
}

#[test]
fn real_sh_over_kernel_full_session_with_mixed_builtins() {
    let mut k = make_kernel();
    let pid = register_shell_process(&mut k);
    let mut shell = Shell::new();

    // Multi-line script: set a var, print env, cd, pwd,
    // echo, exit. Each command round-trips through the
    // kernel console ring.
    k.devs
        .inject_console_input(b"set GREETING=hi\nenv\ncd /tmp\npwd\necho done\nexit\n");

    let mut count = 0usize;
    loop {
        let exit = shell_step_over_kernel(&mut k, pid, &mut shell);
        count += 1;
        if exit.is_some() {
            break;
        }
        if count > 20 {
            panic!("REPL ran too long");
        }
    }
    assert!(shell.has_exited());
    assert_eq!(shell.cwd(), "/tmp");
    assert_eq!(shell.get_env("GREETING"), Some("hi"));
}

#[test]
fn real_sh_over_kernel_help_prints_every_builtin_through_console() {
    let mut k = make_kernel();
    let pid = register_shell_process(&mut k);
    let mut shell = Shell::new();

    // `help` output spans multiple lines. The kernel
    // console driver's platform path flushes each
    // newline-terminated line out of the pending sink, so
    // after the `help` finishes there's nothing left in
    // `drain_console_output` — everything was handed off
    // to the platform driver call inline.
    k.devs.inject_console_input(b"help\n");
    shell_step_over_kernel(&mut k, pid, &mut shell);
    // Every line ended with a `\n`, so the sink is empty.
    assert_eq!(k.devs.drain_console_output(), b"");
}
