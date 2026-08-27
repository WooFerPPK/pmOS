//! `/usr/bin/files` — file manager (T151 + T152..T159).
//!
//! Event-driven toolkit client with keyboard and pointer browsing,
//! VFS-backed create/rename/delete actions, a read-only text preview,
//! and capability-gated host import/export streams.
//!
//! Host-build CLI subcommands (used by the cargo isolation
//! tests):
//!   files                     — print the current directory
//!   files list <dir>          — print <dir>
//!   files import <dir> <name> <bytes_hex>
//!                             — exercise host-import
//!   files export <path>       — write file bytes to stdout
//!   files rename <old> <new>  — POSIX rename
//!   files dispatch <path>     — print the .desktop dispatch

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::process::ExitCode;

use display_proto::events::{key_state, pointer_button_state, KeyboardKey, PointerButton};
use display_proto::Interface;
use files::{
    configured_window_size, DialogKind, DoubleActivation, FileAction, FileManagerState, FileSystem,
    PendingDirectoryAction, PointerTarget, StepwiseAction, UiKey, ViewMode, NORMAL_WINDOW_HEIGHT,
    NORMAL_WINDOW_WIDTH, TITLEBAR_HEIGHT,
};
#[cfg(target_arch = "wasm32")]
use files::{
    create_unique_file, default_app_for, resolve_text_dispatch, HostFileNotification,
    HostNotificationDecoder, StdFileSystem,
};
#[cfg(not(target_arch = "wasm32"))]
use files::{
    default_app_for, export_bytes, import_and_dispatch, list_dir, rename, sanitise_filename,
};
use toolkit::draw::{Canvas, Color, Rect};
use toolkit::theme::Theme;
use toolkit::widget::frame::{PointerOutcome as ChromePointerOutcome, WindowFrame};
use toolkit::{App, BufferPool, Window, WindowFramePatch, WindowFramePatchProgress};

#[cfg(any(target_arch = "wasm32", test))]
mod transfer_pump;

#[cfg(any(target_arch = "wasm32", test))]
const HOST_NOTIFICATION_READS_PER_TURN: usize = 16;

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum BoundedReadStop {
    Budget,
    WouldBlock,
    Eof,
    Error(i32),
}

#[cfg(any(target_arch = "wasm32", test))]
fn read_host_chunks_bounded<R>(mut read: R) -> (Vec<Vec<u8>>, BoundedReadStop)
where
    R: FnMut(&mut [u8]) -> Result<usize, i32>,
{
    let mut chunks = Vec::new();
    for _ in 0..HOST_NOTIFICATION_READS_PER_TURN {
        let mut bytes = vec![0u8; 1024];
        match read(&mut bytes) {
            Ok(0) => return (chunks, BoundedReadStop::Eof),
            Ok(nread) if nread <= bytes.len() => {
                bytes.truncate(nread);
                chunks.push(bytes);
            }
            Ok(_) => return (chunks, BoundedReadStop::Error(abi::errno::EIO)),
            Err(errno) if errno == abi::errno::EAGAIN => {
                return (chunks, BoundedReadStop::WouldBlock)
            }
            Err(errno) => return (chunks, BoundedReadStop::Error(errno)),
        }
    }
    (chunks, BoundedReadStop::Budget)
}

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::path::Path;
    use toolkit::{FdConnection, WaitFd};

    use super::transfer_pump::{
        enqueue_bounded, ExportBody, ImportBody, PumpError, PumpIoError, TransferInterest,
        TransferTurn,
    };

    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn fd_read(fd: i32, iovs_ptr: *const Iovec, iovs_len: i32, nread_ptr: *mut u32) -> i32;
        fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32)
            -> i32;
        fn fd_close(fd: i32) -> i32;
        fn fd_renumber(from: i32, to: i32) -> i32;
        fn proc_exit(rval: i32) -> !;
    }

    #[link(wasm_import_module = "pmos_ext")]
    extern "C" {
        fn ipc_socket(ty: i32) -> i32;
        fn ipc_connect(fd: i32, path_ptr: *const u8, path_len: i32) -> i32;
        fn host_file_recv(token: u32) -> i32;
        fn host_file_pick() -> i32;
        fn host_file_send(
            name_ptr: *const u8,
            name_len: i32,
            mime_ptr: *const u8,
            mime_len: i32,
        ) -> i32;
        fn cap_list(caps_out_ptr: *mut u64) -> i32;
        fn proc_spawn_manifest(manifest_ptr: *const u8, manifest_len: u32) -> i32;
        fn proc_wait(target_pid: i32, options: i32, status_out_ptr: *mut i64) -> i32;
    }

    const EAGAIN: i32 = 6;
    const HOST_RECONNECT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
    const SIGNAL_FD: i32 = abi::fd::SIGNAL as i32;

    #[repr(C)]
    pub struct Ciovec {
        pub buf: *const u8,
        pub buf_len: u32,
    }
    #[repr(C)]
    pub struct Iovec {
        pub buf: *mut u8,
        pub buf_len: u32,
    }

    struct ActiveImport {
        fd: i32,
        destination_path: std::path::PathBuf,
        destination: std::fs::File,
        notification: HostFileNotification,
        body: ImportBody,
    }

    struct ActiveExport {
        host_fd: i32,
        path: std::path::PathBuf,
        file: std::fs::File,
        total_hint: usize,
        body: ExportBody,
    }

    enum ActiveTransfer {
        Import(ActiveImport),
        Export(ActiveExport),
    }

    impl ActiveTransfer {
        fn wait_fd(&self) -> WaitFd {
            let (fd, interest) = match self {
                Self::Import(import) => {
                    let interest = import.body.interest();
                    let fd = match interest {
                        TransferInterest::Read => import.fd,
                        TransferInterest::Write => import.destination.as_raw_fd(),
                    };
                    (fd, interest)
                }
                Self::Export(export) => {
                    let interest = export.body.interest();
                    let fd = match interest {
                        TransferInterest::Read => export.file.as_raw_fd(),
                        TransferInterest::Write => export.host_fd,
                    };
                    (fd, interest)
                }
            };
            match interest {
                TransferInterest::Read => WaitFd::readable(fd),
                TransferInterest::Write => WaitFd::writable(fd),
            }
        }

        fn is_import(&self) -> bool {
            matches!(self, Self::Import(_))
        }
    }

    pub struct HostTransfer {
        notification_fd: Option<i32>,
        decoder: HostNotificationDecoder,
        reconnect_at: std::time::Instant,
        pending_imports: VecDeque<HostFileNotification>,
        active: Option<ActiveTransfer>,
        post_import_refresh: Option<std::path::PathBuf>,
    }

    impl HostTransfer {
        pub fn new() -> Self {
            let mut transfer = Self {
                notification_fd: None,
                decoder: HostNotificationDecoder::default(),
                reconnect_at: std::time::Instant::now(),
                pending_imports: VecDeque::new(),
                active: None,
                post_import_refresh: None,
            };
            transfer.try_connect();
            transfer
        }

        fn try_connect(&mut self) {
            if self.notification_fd.is_some() || std::time::Instant::now() < self.reconnect_at {
                return;
            }
            let fd = unsafe { ipc_socket(0) };
            if fd < 0 {
                self.reconnect_at = std::time::Instant::now() + HOST_RECONNECT_INTERVAL;
                return;
            }
            let endpoint = abi::ext::host_file::ENDPOINT.as_bytes();
            let rc = unsafe { ipc_connect(fd, endpoint.as_ptr(), endpoint.len() as i32) };
            if rc == 0 {
                self.notification_fd = Some(fd);
                println!("files: host transfer ready");
            } else {
                unsafe {
                    let _ = fd_close(fd);
                }
                self.reconnect_at = std::time::Instant::now() + HOST_RECONNECT_INTERVAL;
            }
        }

        pub fn wait_sources(&self) -> Vec<WaitFd> {
            let mut sources = Vec::with_capacity(3);
            sources.push(WaitFd::readable(SIGNAL_FD));
            if let Some(fd) = self.notification_fd {
                sources.push(WaitFd::readable(fd));
            }
            if let Some(active) = self.active.as_ref() {
                sources.push(active.wait_fd());
            }
            sources
        }

        pub fn wait_timeout(&self) -> Option<std::time::Duration> {
            self.notification_fd.is_none().then(|| {
                self.reconnect_at
                    .saturating_duration_since(std::time::Instant::now())
            })
        }

        pub fn handle_action(
            &mut self,
            state: &mut FileManagerState,
            action: FileAction,
            filesystem: &dyn FileSystem,
        ) -> bool {
            match action {
                FileAction::RequestHostImport => {
                    let rc = unsafe { host_file_pick() };
                    if rc == 0 {
                        state.host_import_pending();
                        println!("files: host picker requested");
                    } else {
                        state.host_transfer_failed("import", format_errno(rc));
                        println!("files: error host picker errno {}", -rc);
                    }
                    false
                }
                FileAction::RequestHostExport(path) => {
                    if self.active.is_some() || !self.pending_imports.is_empty() {
                        state.host_transfer_failed(
                            "export",
                            "another host transfer is already in progress",
                        );
                        return false;
                    }
                    match self.start_export(&path) {
                        Ok(active) => {
                            state.host_transfer_progress(
                                "Exporting",
                                path.display().to_string(),
                                0,
                                active.total_hint,
                            );
                            self.active = Some(ActiveTransfer::Export(active));
                        }
                        Err(error) => {
                            println!("files: error export {}: {error}", path.display());
                            state.host_transfer_failed("export", error);
                        }
                    }
                    false
                }
                FileAction::OpenDefault(path) => {
                    match self.open_default(&path, state.cwd()) {
                        Ok((pid, desktop, executable, caps)) => {
                            state.default_open_started(&path, pid);
                            println!(
                                "files: opened {} via {} exec={} pid={} caps=0x{:x}",
                                path.display(),
                                desktop,
                                executable,
                                pid,
                                caps,
                            );
                        }
                        Err(error) => {
                            println!("files: error open {}: {error}", path.display());
                            state.default_open_failed(&path, error);
                        }
                    }
                    false
                }
                action => super::execute_and_log(state, action, filesystem),
            }
        }

        fn open_default(
            &self,
            path: &Path,
            cwd: &Path,
        ) -> Result<(i32, String, String, u64), String> {
            let mut held_caps = 0_u64;
            let cap_rc = unsafe { cap_list(&mut held_caps) };
            if cap_rc != 0 {
                return Err(format_errno(cap_rc));
            }
            let dispatch = resolve_text_dispatch(path, abi::cap::CapSet(held_caps))
                .map_err(|error| error.to_string())?;
            let cwd = cwd
                .to_str()
                .ok_or_else(|| "current directory is not UTF-8".to_string())?;
            let environment = std::env::vars().collect::<Vec<_>>();
            let manifest = sh::SpawnWireManifest {
                path: &dispatch.executable,
                argv: &dispatch.argv,
                env: &environment,
                stdin_fd: None,
                stdout_fd: None,
                stderr_fd: None,
                extra_fds: &[],
                cwd: Some(cwd),
                caps: Some(dispatch.caps.0),
            };
            let blob = sh::encode_spawn_manifest_v1(&manifest)
                .map_err(|_| "invalid spawn manifest".to_string())?;
            let pid = unsafe { proc_spawn_manifest(blob.as_ptr(), blob.len() as u32) };
            if pid < 0 {
                return Err(format_errno(pid));
            }
            Ok((
                pid,
                dispatch.desktop_path,
                dispatch.executable,
                dispatch.caps.0,
            ))
        }

        pub fn reap_children(&mut self) {
            let mut saw_sigchld = false;
            loop {
                let mut signals = [0u8; 32];
                let iov = Iovec {
                    buf: signals.as_mut_ptr(),
                    buf_len: signals.len() as u32,
                };
                let mut nread = 0u32;
                let rc = unsafe { fd_read(SIGNAL_FD, &iov, 1, &mut nread) };
                if rc == EAGAIN || (rc == 0 && nread == 0) {
                    break;
                }
                if rc != 0 {
                    return;
                }
                saw_sigchld |= signals[..nread as usize].chunks_exact(2).any(|record| {
                    u16::from_le_bytes([record[0], record[1]]) == abi::ext::sig::SIGCHLD
                });
            }
            if !saw_sigchld {
                return;
            }
            loop {
                let mut status = 0_i64;
                let pid = unsafe { proc_wait(-1, 1, &mut status) };
                if pid <= 0 {
                    return;
                }
                println!("files: reaped child pid={pid} status={status}");
            }
        }

        pub fn poll_into(
            &mut self,
            state: &mut FileManagerState,
            filesystem: &dyn FileSystem,
            pending_directory: &mut Option<PendingDirectoryAction>,
        ) -> bool {
            self.try_connect();
            let mut changed = self.start_post_import_refresh(state, filesystem, pending_directory);
            changed |= self.poll_notifications(state);
            if self.active.is_none() {
                changed |= self.start_next_import(state);
            }
            let Some(active) = self.active.as_mut() else {
                return changed;
            };

            let before = match active {
                ActiveTransfer::Import(import) => import.body.completed_bytes(),
                ActiveTransfer::Export(export) => export.body.completed_bytes(),
            };
            let turn = match active {
                ActiveTransfer::Import(import) => {
                    let fd = import.fd;
                    let destination = &mut import.destination;
                    import.body.pump_turn(
                        |buf| {
                            let iov = Iovec {
                                buf: buf.as_mut_ptr(),
                                buf_len: buf.len() as u32,
                            };
                            let mut nread = 0u32;
                            let rc = unsafe { fd_read(fd, &iov, 1, &mut nread) };
                            if rc == 0 {
                                Ok(nread as usize)
                            } else if rc == EAGAIN {
                                Err(PumpIoError::WouldBlock)
                            } else {
                                Err(PumpIoError::Failed(PumpError::Os(rc)))
                            }
                        },
                        |buf| match destination.write(buf) {
                            Ok(written) => Ok(written),
                            Err(error)
                                if error.raw_os_error() == Some(EAGAIN)
                                    || error.kind() == std::io::ErrorKind::WouldBlock =>
                            {
                                Err(PumpIoError::WouldBlock)
                            }
                            Err(error) => {
                                Err(PumpIoError::Failed(PumpError::Message(error.to_string())))
                            }
                        },
                    )
                }
                ActiveTransfer::Export(export) => {
                    let host_fd = export.host_fd;
                    let file = &mut export.file;
                    export.body.pump_turn(
                        |buf| {
                            file.read(buf)
                                .map_err(|error| PumpError::Message(error.to_string()))
                        },
                        |buf| {
                            let iov = Ciovec {
                                buf: buf.as_ptr(),
                                buf_len: buf.len() as u32,
                            };
                            let mut written = 0u32;
                            let rc = unsafe { fd_write(host_fd, &iov, 1, &mut written) };
                            if rc == 0 {
                                Ok(written as usize)
                            } else if rc == EAGAIN {
                                Err(PumpIoError::WouldBlock)
                            } else {
                                Err(PumpIoError::Failed(PumpError::Os(rc)))
                            }
                        },
                    )
                }
            };
            let after = match active {
                ActiveTransfer::Import(import) => {
                    let completed = import.body.completed_bytes();
                    if completed != before {
                        state.host_transfer_progress(
                            "Importing",
                            &import.notification.name,
                            completed,
                            import.notification.size as usize,
                        );
                    }
                    completed
                }
                ActiveTransfer::Export(export) => {
                    let completed = export.body.completed_bytes();
                    if completed != before {
                        state.host_transfer_progress(
                            "Exporting",
                            export.path.display().to_string(),
                            completed,
                            export.total_hint,
                        );
                    }
                    completed
                }
            };
            changed |= after != before;

            match turn {
                TransferTurn::Pending => return changed,
                TransferTurn::Complete => {
                    changed = true;
                    if let Some(path) = self.complete_active(state) {
                        self.post_import_refresh = Some(path);
                    }
                }
                TransferTurn::Failed(error) => {
                    changed = true;
                    self.fail_active(state, error);
                }
            }
            if self.active.is_none() {
                changed |= self.start_next_import(state);
            }
            changed |= self.start_post_import_refresh(state, filesystem, pending_directory);
            changed
        }

        fn start_post_import_refresh(
            &mut self,
            state: &mut FileManagerState,
            filesystem: &dyn FileSystem,
            pending_directory: &mut Option<PendingDirectoryAction>,
        ) -> bool {
            if pending_directory.is_some() {
                return false;
            }
            let Some(path) = self.post_import_refresh.take() else {
                return false;
            };
            match state.begin_complete_host_import(&path, filesystem) {
                StepwiseAction::Pending(pending) => {
                    *pending_directory = Some(pending);
                    true
                }
                StepwiseAction::Complete(outcome) => {
                    if let Some(message) = outcome.log {
                        println!("files: {message}");
                    }
                    outcome.changed
                }
            }
        }

        fn poll_notifications(&mut self, state: &mut FileManagerState) -> bool {
            let Some(fd) = self.notification_fd else {
                return false;
            };
            let mut changed = false;
            let (chunks, stop) = read_host_chunks_bounded(|buf| {
                let iov = Iovec {
                    buf: buf.as_mut_ptr(),
                    buf_len: buf.len() as u32,
                };
                let mut nread = 0u32;
                let rc = unsafe { fd_read(fd, &iov, 1, &mut nread) };
                if rc == 0 {
                    Ok(nread as usize)
                } else {
                    Err(rc)
                }
            });
            for chunk in chunks {
                match self.decoder.push(&chunk) {
                    Ok(notifications) => {
                        for notification in notifications {
                            changed |= self.enqueue_import(state, notification);
                        }
                    }
                    Err(error) => {
                        let message = format!("malformed notification: {error:?}");
                        println!("files: error {message}");
                        state.host_transfer_failed("import", message);
                        changed = true;
                    }
                }
            }
            if matches!(stop, BoundedReadStop::Eof | BoundedReadStop::Error(_)) {
                if let BoundedReadStop::Error(errno) = stop {
                    state.host_transfer_failed("notification stream", format_errno(errno));
                    changed = true;
                }
                unsafe {
                    let _ = fd_close(fd);
                }
                self.notification_fd = None;
                self.decoder = HostNotificationDecoder::default();
                self.reconnect_at = std::time::Instant::now() + HOST_RECONNECT_INTERVAL;
            }
            changed
        }

        fn enqueue_import(
            &mut self,
            state: &mut FileManagerState,
            notification: HostFileNotification,
        ) -> bool {
            let already_admitted = self.active.as_ref().is_some_and(|active| {
                matches!(
                    active,
                    ActiveTransfer::Import(import)
                        if import.notification.token == notification.token
                )
            }) || self
                .pending_imports
                .iter()
                .any(|pending| pending.token == notification.token);
            if already_admitted {
                return false;
            }
            let active_imports =
                usize::from(self.active.as_ref().is_some_and(ActiveTransfer::is_import));
            let queue_capacity = abi::ext::host_file::MAX_LIVE_IMPORTS - active_imports;
            if let Err(notification) =
                enqueue_bounded(&mut self.pending_imports, notification, queue_capacity)
            {
                let name = notification.name.clone();
                discard_import(notification);
                let message = format!(
                    "discarded {name}: the {}-file import queue is full",
                    abi::ext::host_file::MAX_LIVE_IMPORTS
                );
                println!("files: error {message}");
                state.host_transfer_failed("import", message);
                return true;
            }
            false
        }

        fn start_next_import(&mut self, state: &mut FileManagerState) -> bool {
            let mut changed = false;
            while let Some(notification) = self.pending_imports.pop_front() {
                if notification.size > abi::ext::host_file::MAX_IMPORT_BYTES as u64 {
                    let name = notification.name.clone();
                    discard_import(notification);
                    let error = format!("{name} is larger than the 16 MiB v1 transfer limit");
                    println!("files: error import {error}");
                    state.host_transfer_failed("import", error);
                    changed = true;
                    continue;
                }
                let fd = unsafe { host_file_recv(notification.token) };
                if fd < 0 {
                    let error = format_errno(fd);
                    println!("files: error import {}: {error}", notification.name);
                    state.host_transfer_failed("import", error);
                    changed = true;
                    continue;
                }
                let (destination_path, destination) =
                    match open_import_destination(state.cwd(), &notification.name) {
                        Ok(destination) => destination,
                        Err(error) => {
                            unsafe {
                                let _ = fd_close(fd);
                            }
                            println!("files: error import {}: {error}", notification.name);
                            state.host_transfer_failed("import", error);
                            changed = true;
                            continue;
                        }
                    };
                let expected = notification.size as usize;
                state.host_transfer_progress("Importing", &notification.name, 0, expected);
                self.active = Some(ActiveTransfer::Import(ActiveImport {
                    fd,
                    destination_path,
                    destination,
                    notification,
                    body: ImportBody::new(expected),
                }));
                return true;
            }
            changed
        }

        fn start_export(&self, path: &Path) -> Result<ActiveExport, String> {
            let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
            if !metadata.is_file() {
                return Err("only regular files can be exported".to_string());
            }
            if metadata.len() > abi::ext::host_file::MAX_DOWNLOAD_BYTES as u64 {
                return Err("file is larger than the 16 MiB v1 transfer limit".to_string());
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| "file name is not UTF-8".to_string())?;
            if name.len() > abi::ext::host_file::MAX_NAME_BYTES {
                return Err("file name is too long for host export".to_string());
            }
            let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
            let mime = mime_for_path(path);
            let host_fd = unsafe {
                host_file_send(
                    name.as_ptr(),
                    name.len() as i32,
                    mime.as_ptr(),
                    mime.len() as i32,
                )
            };
            if host_fd < 0 {
                return Err(format_errno(host_fd));
            }
            Ok(ActiveExport {
                host_fd,
                path: path.to_path_buf(),
                file,
                total_hint: metadata.len() as usize,
                body: ExportBody::new(abi::ext::host_file::MAX_DOWNLOAD_BYTES),
            })
        }

        fn complete_active(&mut self, state: &mut FileManagerState) -> Option<std::path::PathBuf> {
            match self.active.take().expect("active transfer") {
                ActiveTransfer::Import(import) => complete_import(import, state),
                ActiveTransfer::Export(export) => {
                    complete_export(export, state);
                    None
                }
            }
        }

        fn fail_active(&mut self, state: &mut FileManagerState, error: PumpError) {
            match self.active.take().expect("active transfer") {
                ActiveTransfer::Import(import) => {
                    let name = import.notification.name.clone();
                    let cleanup = cancel_import(import).err();
                    let message = match cleanup {
                        Some(cleanup) => format!("{error}; cleanup failed: {cleanup}"),
                        None => error.to_string(),
                    };
                    println!("files: error import {name}: {message}");
                    state.host_transfer_failed("import", message);
                }
                ActiveTransfer::Export(export) => {
                    let path = export.path.clone();
                    cancel_export(export);
                    println!("files: error export {}: {error}", path.display());
                    state.host_transfer_failed("export", error.to_string());
                }
            }
        }
    }

    impl Drop for HostTransfer {
        fn drop(&mut self) {
            if let Some(active) = self.active.take() {
                match active {
                    ActiveTransfer::Import(import) => {
                        if let Err(error) = cancel_import(import) {
                            println!("files: error cancelling import on close: {error}");
                        }
                    }
                    ActiveTransfer::Export(export) => cancel_export(export),
                }
            }
            while let Some(notification) = self.pending_imports.pop_front() {
                discard_import(notification);
            }
            if let Some(fd) = self.notification_fd.take() {
                unsafe {
                    let _ = fd_close(fd);
                }
            }
        }
    }

    fn complete_import(
        import: ActiveImport,
        state: &mut FileManagerState,
    ) -> Option<std::path::PathBuf> {
        let close_rc = unsafe { fd_close(import.fd) };
        if close_rc != 0 {
            let error = format_errno(close_rc);
            let name = import.notification.name.clone();
            let cleanup = cancel_import(import).err();
            let message = match cleanup {
                Some(cleanup) => format!("{error}; cleanup failed: {cleanup}"),
                None => error,
            };
            println!("files: error import {name}: {message}");
            state.host_transfer_failed("import", message);
            return None;
        }
        let ActiveImport {
            destination_path,
            destination,
            notification,
            ..
        } = import;
        drop(destination);
        let display_name = destination_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&notification.name);
        if let Some(desktop) = default_app_for(display_name, Some(&notification.mime)) {
            println!("files: imported handler {desktop}");
        }
        Some(destination_path)
    }

    fn open_import_destination(
        target_dir: &Path,
        name: &str,
    ) -> Result<(std::path::PathBuf, std::fs::File), String> {
        if !target_dir.is_dir() {
            return Err(format!(
                "target {} is not a directory",
                target_dir.display()
            ));
        }
        create_unique_file(target_dir, name).map_err(|error| error.to_string())
    }

    fn cancel_import(import: ActiveImport) -> Result<(), String> {
        let close_rc = unsafe { fd_close(import.fd) };
        let destination_path = import.destination_path.clone();
        drop(import.destination);
        let remove_error = match std::fs::remove_file(&destination_path) {
            Ok(()) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(error.to_string()),
        };
        match (close_rc, remove_error) {
            (0, None) => Ok(()),
            (0, Some(remove)) => Err(format!(
                "could not remove partial {}: {remove}",
                destination_path.display()
            )),
            (close, None) => Err(format_errno(close)),
            (close, Some(remove)) => Err(format!(
                "{}; could not remove partial {}: {remove}",
                format_errno(close),
                destination_path.display()
            )),
        }
    }

    fn complete_export(export: ActiveExport, state: &mut FileManagerState) {
        let close_rc = unsafe { fd_close(export.host_fd) };
        if close_rc == 0 {
            if let Some(message) = state.host_export_complete(&export.path).log {
                println!("files: {message}");
            }
        } else {
            let error = format_errno(close_rc);
            println!("files: error export {}: {error}", export.path.display());
            state.host_transfer_failed("export", error);
        }
    }

    fn discard_import(notification: HostFileNotification) {
        let fd = unsafe { host_file_recv(notification.token) };
        if fd >= 0 {
            unsafe {
                let _ = fd_close(fd);
            }
        } else {
            println!(
                "files: discarded import token {} ({}) with {}",
                notification.token,
                notification.name,
                format_errno(fd)
            );
        }
    }

    fn cancel_export(export: ActiveExport) {
        let source_fd = export.file.as_raw_fd();
        let renumber_rc = unsafe { fd_renumber(source_fd, export.host_fd) };
        if renumber_rc != 0 {
            println!(
                "files: fatal: could not cancel export {}: {}",
                export.path.display(),
                format_errno(renumber_rc)
            );
            unsafe { proc_exit(1) };
        }
        let close_rc = unsafe { fd_close(export.host_fd) };
        // fd_renumber moved ownership out of File's recorded descriptor and
        // fd_close closed the moved source. Do not let File close a descriptor
        // number that may be reused later.
        std::mem::forget(export.file);
        if close_rc != 0 {
            println!(
                "files: fatal: could not close cancelled export {}: {}",
                export.path.display(),
                format_errno(close_rc)
            );
            unsafe { proc_exit(1) };
        }
    }

    fn mime_for_path(path: &Path) -> &'static str {
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "txt" | "md" | "rs" | "toml" | "json" | "log" | "conf" | "ini" => "text/plain",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "svg" => "image/svg+xml",
            "pdf" => "application/pdf",
            _ => "application/octet-stream",
        }
    }

    fn format_errno(rc: i32) -> String {
        format!("OS error {}", rc.saturating_abs())
    }

    pub fn run() {
        println!("files: starting");
        let conn = match FdConnection::connect() {
            Ok(connection) => connection,
            Err(errno) => unsafe { proc_exit(errno) },
        };
        match super::run_window(conn, &StdFileSystem) {
            Ok(_) => unsafe { proc_exit(0) },
            Err(_) => unsafe { proc_exit(1) },
        }
    }
}

#[cfg(target_arch = "wasm32")]
extern crate alloc;

const TOOLBAR_Y: i32 = TITLEBAR_HEIGHT as i32;
const TOOLBAR_ROW_HEIGHT: u32 = 26;
const TOOLBAR_HEIGHT: u32 = TOOLBAR_ROW_HEIGHT * 2;
const ADDRESS_Y: i32 = TOOLBAR_Y + TOOLBAR_HEIGHT as i32 + 2;
const ADDRESS_HEIGHT: u32 = 20;
const LIST_TOP: i32 = ADDRESS_Y + ADDRESS_HEIGHT as i32 + 4;
const STATUS_HEIGHT: u32 = 22;
const ROW_HEIGHT: i32 = 18;
const SCROLLBAR_WIDTH: i32 = 22;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseAcknowledgement {
    DisplayServer,
    ClientTitlebar,
}

fn close_acknowledgement(source: CloseAcknowledgement) -> &'static str {
    match source {
        CloseAcknowledgement::DisplayServer => "files: close requested by display server",
        CloseAcknowledgement::ClientTitlebar => "files: close requested by client titlebar",
    }
}

#[derive(Default, Debug, Clone, Copy)]
struct Modifiers {
    shift: bool,
    ctrl: bool,
}

mod sc {
    pub const KEY_A: u32 = 0x04;
    pub const KEY_Z: u32 = 0x1d;
    pub const DIGIT_1: u32 = 0x1e;
    pub const DIGIT_0: u32 = 0x27;
    pub const ENTER: u32 = 0x28;
    pub const ESCAPE: u32 = 0x29;
    pub const BACKSPACE: u32 = 0x2a;
    pub const SPACE: u32 = 0x2c;
    pub const MINUS: u32 = 0x2d;
    pub const EQUAL: u32 = 0x2e;
    pub const BRACKET_LEFT: u32 = 0x2f;
    pub const BRACKET_RIGHT: u32 = 0x30;
    pub const BACKSLASH: u32 = 0x31;
    pub const SEMICOLON: u32 = 0x33;
    pub const QUOTE: u32 = 0x34;
    pub const BACKQUOTE: u32 = 0x35;
    pub const COMMA: u32 = 0x36;
    pub const PERIOD: u32 = 0x37;
    pub const SLASH: u32 = 0x38;
    pub const HOME: u32 = 0x4a;
    pub const PAGE_UP: u32 = 0x4b;
    pub const DELETE: u32 = 0x4c;
    pub const END: u32 = 0x4d;
    pub const PAGE_DOWN: u32 = 0x4e;
    pub const ARROW_DOWN: u32 = 0x51;
    pub const ARROW_UP: u32 = 0x52;
    pub const SHIFT_LEFT: u32 = 0xe1;
    pub const SHIFT_RIGHT: u32 = 0xe5;
    pub const CONTROL_LEFT: u32 = 0xe0;
    pub const CONTROL_RIGHT: u32 = 0xe4;
}

impl Modifiers {
    fn update(&mut self, scancode: u32, pressed: bool) -> bool {
        match scancode {
            sc::SHIFT_LEFT | sc::SHIFT_RIGHT => {
                self.shift = pressed;
                true
            }
            sc::CONTROL_LEFT | sc::CONTROL_RIGHT => {
                self.ctrl = pressed;
                true
            }
            _ => false,
        }
    }
}

fn decode_key(scancode: u32, modifiers: Modifiers) -> Option<UiKey> {
    let navigation = match scancode {
        sc::ENTER => Some(UiKey::Enter),
        sc::ESCAPE => Some(UiKey::Escape),
        sc::BACKSPACE => Some(UiKey::Backspace),
        sc::DELETE => Some(UiKey::Delete),
        sc::ARROW_UP => Some(UiKey::Up),
        sc::ARROW_DOWN => Some(UiKey::Down),
        sc::PAGE_UP => Some(UiKey::PageUp),
        sc::PAGE_DOWN => Some(UiKey::PageDown),
        sc::HOME => Some(UiKey::Home),
        sc::END => Some(UiKey::End),
        _ => None,
    };
    if navigation.is_some() {
        return navigation;
    }
    if (sc::KEY_A..=sc::KEY_Z).contains(&scancode) {
        let lower = (b'a' + (scancode - sc::KEY_A) as u8) as char;
        if modifiers.ctrl {
            return (lower == 'q').then_some(UiKey::Close);
        }
        return Some(UiKey::Char(if modifiers.shift {
            lower.to_ascii_uppercase()
        } else {
            lower
        }));
    }
    if modifiers.ctrl {
        return None;
    }
    if scancode == sc::SPACE {
        return Some(UiKey::Char(' '));
    }
    if (sc::DIGIT_1..=sc::DIGIT_0).contains(&scancode) {
        let plain = match scancode {
            sc::DIGIT_1 => '1',
            0x1f => '2',
            0x20 => '3',
            0x21 => '4',
            0x22 => '5',
            0x23 => '6',
            0x24 => '7',
            0x25 => '8',
            0x26 => '9',
            sc::DIGIT_0 => '0',
            _ => return None,
        };
        let shifted = match plain {
            '1' => '!',
            '2' => '@',
            '3' => '#',
            '4' => '$',
            '5' => '%',
            '6' => '^',
            '7' => '&',
            '8' => '*',
            '9' => '(',
            '0' => ')',
            _ => plain,
        };
        return Some(UiKey::Char(if modifiers.shift { shifted } else { plain }));
    }
    let ch = match scancode {
        sc::MINUS => Some(if modifiers.shift { '_' } else { '-' }),
        sc::EQUAL => Some(if modifiers.shift { '+' } else { '=' }),
        sc::BRACKET_LEFT => Some(if modifiers.shift { '{' } else { '[' }),
        sc::BRACKET_RIGHT => Some(if modifiers.shift { '}' } else { ']' }),
        sc::BACKSLASH => Some(if modifiers.shift { '|' } else { '\\' }),
        sc::SEMICOLON => Some(if modifiers.shift { ':' } else { ';' }),
        sc::QUOTE => Some(if modifiers.shift { '"' } else { '\'' }),
        sc::BACKQUOTE => Some(if modifiers.shift { '~' } else { '`' }),
        sc::COMMA => Some(if modifiers.shift { '<' } else { ',' }),
        sc::PERIOD => Some(if modifiers.shift { '>' } else { '.' }),
        sc::SLASH => Some(if modifiers.shift { '?' } else { '/' }),
        _ => None,
    }?;
    Some(UiKey::Char(ch))
}

fn run_window<C: toolkit::protocol::Connection>(
    connection: C,
    filesystem: &dyn FileSystem,
) -> Result<(), toolkit::ClientError> {
    let mut app = App::connect_with_shell(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("Files")?;
    window.set_app_id("pmos.files")?;
    window.commit()?;

    let (mut state, mut pending_directory) = initial_state(filesystem);
    let mut modifiers = Modifiers::default();
    let mut needs_paint = true;
    let mut configured = false;
    let mut ready_logged = false;
    let mut pool: Option<BufferPool> = None;
    let mut chrome_patch: Option<WindowFramePatch> = None;
    let mut size = (NORMAL_WINDOW_WIDTH, NORMAL_WINDOW_HEIGHT);
    let mut was_maximized = false;
    let mut was_activated = false;
    let started = std::time::Instant::now();
    let mut double_activation = DoubleActivation::default();
    #[cfg(target_arch = "wasm32")]
    let mut host_transfer = wasm_main::HostTransfer::new();

    loop {
        let events = window.dispatch()?;
        let focus_changed = window.is_activated() != was_activated;
        if focus_changed {
            was_activated = window.is_activated();
        }
        if window.take_close_requested() {
            println!(
                "{}",
                close_acknowledgement(CloseAcknowledgement::DisplayServer)
            );
            return Ok(());
        }
        let rows = visible_rows(size.1);
        for event in events {
            match (event.interface, event.opcode) {
                (Interface::Keyboard, 1) => {
                    if let Ok(key) = KeyboardKey::decode(&event.payload) {
                        let pressed = key.state == key_state::PRESSED;
                        let modifier = modifiers.update(key.key, pressed);
                        if pressed && !modifier {
                            if let Some(decoded) = decode_key(key.key, modifiers) {
                                double_activation.cancel();
                                let action = state.handle_key(decoded, rows);
                                needs_paint = true;
                                if let Some(action) = action {
                                    #[cfg(target_arch = "wasm32")]
                                    let close = if matches!(
                                        &action,
                                        FileAction::RequestHostImport
                                            | FileAction::RequestHostExport(_)
                                            | FileAction::OpenDefault(_)
                                    ) {
                                        host_transfer.handle_action(&mut state, action, filesystem)
                                    } else {
                                        execute_stepwise_and_log(
                                            &mut state,
                                            action,
                                            filesystem,
                                            &mut pending_directory,
                                        )
                                    };
                                    #[cfg(not(target_arch = "wasm32"))]
                                    let close = execute_stepwise_and_log(
                                        &mut state,
                                        action,
                                        filesystem,
                                        &mut pending_directory,
                                    );
                                    if close {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }
                (Interface::Pointer, 2) => {
                    if let Ok(button) = PointerButton::decode(&event.payload) {
                        if button.button == 1 && button.state == pointer_button_state::PRESSED {
                            let mut chrome = files_window_frame(
                                size.0,
                                size.1,
                                window.is_activated(),
                                window.is_maximized(),
                            );
                            match chrome.pointer_down(button.x, button.y) {
                                ChromePointerOutcome::Minimize => {
                                    window.set_minimized()?;
                                    continue;
                                }
                                ChromePointerOutcome::ToggleMaximize => {
                                    if window.is_maximized() {
                                        window.unset_maximized()?;
                                    } else {
                                        window.set_maximized()?;
                                    }
                                    continue;
                                }
                                ChromePointerOutcome::Close => {
                                    println!(
                                        "{}",
                                        close_acknowledgement(CloseAcknowledgement::ClientTitlebar)
                                    );
                                    return Ok(());
                                }
                                ChromePointerOutcome::Titlebar => {
                                    if !window.is_maximized() {
                                        window.request_move(button.serial)?;
                                        println!("files: move requested serial={}", button.serial);
                                    }
                                    continue;
                                }
                                ChromePointerOutcome::Content => {}
                                ChromePointerOutcome::Outside => continue,
                            }
                            if let Some(target) =
                                pointer_target(&state, button.x, button.y, size.0, size.1)
                            {
                                let target = match target {
                                    PointerTarget::Entry(index) => {
                                        if double_activation.press(index, started.elapsed()) {
                                            PointerTarget::Open
                                        } else {
                                            PointerTarget::Entry(index)
                                        }
                                    }
                                    other => {
                                        double_activation.cancel();
                                        other
                                    }
                                };
                                let action = state.handle_pointer(target, rows);
                                if matches!(target, PointerTarget::Entry(_)) {
                                    if let Some(entry) = state.selected_entry() {
                                        println!(
                                            "files: selected {}",
                                            state.cwd().join(&entry.name).display()
                                        );
                                    }
                                }
                                needs_paint = true;
                                if let Some(action) = action {
                                    #[cfg(target_arch = "wasm32")]
                                    let close = if matches!(
                                        &action,
                                        FileAction::RequestHostImport
                                            | FileAction::RequestHostExport(_)
                                            | FileAction::OpenDefault(_)
                                    ) {
                                        host_transfer.handle_action(&mut state, action, filesystem)
                                    } else {
                                        execute_stepwise_and_log(
                                            &mut state,
                                            action,
                                            filesystem,
                                            &mut pending_directory,
                                        )
                                    };
                                    #[cfg(not(target_arch = "wasm32"))]
                                    let close = execute_stepwise_and_log(
                                        &mut state,
                                        action,
                                        filesystem,
                                        &mut pending_directory,
                                    );
                                    if close {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }
                (Interface::Buffer, 1) => {
                    if let Some(buffers) = pool.as_mut() {
                        let _ = buffers.handle_release(event.object_id);
                    }
                }
                _ => {}
            }
        }

        if let Some(buffers) = pool.as_mut().filter(|buffers| buffers.commit_pending()) {
            let _ = buffers.progress_commit(&mut window)?;
        }

        #[cfg(target_arch = "wasm32")]
        {
            host_transfer.reap_children();
            if host_transfer.poll_into(&mut state, filesystem, &mut pending_directory) {
                needs_paint = true;
            }
        }

        let completed_directory = pending_directory
            .as_mut()
            .and_then(|pending| pending.step(&mut state));
        if let Some(outcome) = completed_directory {
            pending_directory = None;
            if let Some(message) = outcome.log {
                println!("files: {message}");
            }
            needs_paint |= outcome.changed;
            if outcome.close {
                return Ok(());
            }
        }

        if !configured && window.is_configured() {
            configured = true;
            size = configured_window_size(window.is_maximized(), window.configured_size());
            BufferPool::replace(&mut pool, window.app_mut(), size.0, size.1)?;
            was_maximized = window.is_maximized();
            needs_paint = true;
        }

        if configured
            && window.is_maximized() != was_maximized
            && !pool.as_ref().is_some_and(BufferPool::commit_pending)
        {
            let maximized = window.is_maximized();
            let offered = window.configured_size();
            size = configured_window_size(maximized, offered);
            BufferPool::replace(&mut pool, window.app_mut(), size.0, size.1)?;
            was_maximized = maximized;
            needs_paint = true;
            println!(
                "files: window {} {}x{}",
                if maximized { "maximized" } else { "restored" },
                size.0,
                size.1
            );
        }

        if needs_paint {
            chrome_patch = None;
        } else {
            if focus_changed && configured {
                chrome_patch = Some(WindowFramePatch::new(&files_window_frame(
                    size.0,
                    size.1,
                    was_activated,
                    window.is_maximized(),
                )));
            }
            if let (Some(patch), Some(buffers)) = (chrome_patch.as_mut(), pool.as_mut()) {
                match patch.progress(buffers, &mut window)? {
                    WindowFramePatchProgress::Complete => chrome_patch = None,
                    WindowFramePatchProgress::Unavailable => {
                        chrome_patch = None;
                        needs_paint = true;
                    }
                    WindowFramePatchProgress::Deferred | WindowFramePatchProgress::Pending => {}
                }
            }
        }

        if configured && needs_paint {
            let buffers = pool.as_mut().expect("buffer pool configured");
            if let Some(mut canvas) = buffers.acquire_back_canvas() {
                paint_files(
                    &mut canvas,
                    size.0,
                    size.1,
                    &state,
                    was_activated,
                    window.is_maximized(),
                );
                drop(canvas);
                let _ = buffers.commit_and_swap(&mut window)?;
                needs_paint = false;
                chrome_patch = None;
                if !ready_logged {
                    println!("files: ready {}", state.cwd().display());
                    ready_logged = true;
                }
            }
        }

        #[cfg(target_arch = "wasm32")]
        {
            // All immediately available display, host-notification, child
            // reap, and paint work is drained above. Park on the exact
            // remaining interests; the clock exists only while reconnecting
            // the optional host-file notification endpoint.
            let sources = host_transfer.wait_sources();
            window.flush_outbound()?;
            if pending_directory.is_some()
                || ((pool.as_ref().is_some_and(BufferPool::commit_pending)
                    || chrome_patch.is_some())
                    && !window.outbound_pending())
            {
                continue;
            }
            window.wait_with(&sources, host_transfer.wait_timeout())?;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            window.flush_outbound()?;
            if pending_directory.is_some()
                || ((pool.as_ref().is_some_and(BufferPool::commit_pending)
                    || chrome_patch.is_some())
                    && !window.outbound_pending())
            {
                continue;
            }
            window.wait(None)?;
        }
    }
}

fn initial_state(
    filesystem: &dyn FileSystem,
) -> (FileManagerState, Option<PendingDirectoryAction>) {
    let candidates = [
        std::env::var("HOME").ok(),
        Some("/home/user".to_string()),
        Some("/".to_string()),
    ];
    for candidate in candidates.into_iter().flatten() {
        let mut state = FileManagerState::from_entries(&candidate, Vec::new());
        match state.begin_stepwise_action(
            FileAction::Navigate {
                path: candidate.into(),
                select_name: None,
            },
            filesystem,
        ) {
            StepwiseAction::Pending(pending) => return (state, Some(pending)),
            StepwiseAction::Complete(_) if !state.status().starts_with("Error:") => {
                return (state, None);
            }
            StepwiseAction::Complete(_) => {}
        }
    }
    (FileManagerState::from_entries("/", Vec::new()), None)
}

fn execute_stepwise_and_log(
    state: &mut FileManagerState,
    action: FileAction,
    filesystem: &dyn FileSystem,
    pending: &mut Option<PendingDirectoryAction>,
) -> bool {
    if pending.is_some() {
        return matches!(action, FileAction::Close);
    }
    match state.begin_stepwise_action(action, filesystem) {
        StepwiseAction::Pending(action) => {
            *pending = Some(action);
            false
        }
        StepwiseAction::Complete(outcome) => {
            if let Some(message) = outcome.log {
                println!("files: {message}");
            }
            outcome.close
        }
    }
}

fn execute_and_log(
    state: &mut FileManagerState,
    action: FileAction,
    filesystem: &dyn FileSystem,
) -> bool {
    let outcome = state.execute_with(action, filesystem);
    if let Some(message) = outcome.log {
        println!("files: {message}");
    }
    outcome.close
}

fn visible_rows(height: u32) -> usize {
    ((height as i32 - STATUS_HEIGHT as i32 - LIST_TOP).max(ROW_HEIGHT) / ROW_HEIGHT) as usize
}

fn pointer_target(
    state: &FileManagerState,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Option<PointerTarget> {
    if (TOOLBAR_Y..TOOLBAR_Y + TOOLBAR_HEIGHT as i32).contains(&y) {
        let target = if y < TOOLBAR_Y + TOOLBAR_ROW_HEIGHT as i32 {
            match x {
                6..=63 => PointerTarget::Parent,
                68..=167 => PointerTarget::NewFolder,
                172..=243 => PointerTarget::Rename,
                248..=311 => PointerTarget::Delete,
                316..=391 => PointerTarget::Refresh,
                _ => return None,
            }
        } else {
            match x {
                6..=61 => PointerTarget::Open,
                68..=139 => PointerTarget::Preview,
                146..=209 => PointerTarget::Import,
                216..=279 => PointerTarget::Export,
                _ => return None,
            }
        };
        return Some(target);
    }
    let status_top = height as i32 - STATUS_HEIGHT as i32;
    if y < LIST_TOP || y >= status_top {
        return None;
    }
    if x >= width as i32 - SCROLLBAR_WIDTH {
        return if y < LIST_TOP + 24 {
            Some(PointerTarget::ScrollUp)
        } else if y >= status_top - 24 {
            Some(PointerTarget::ScrollDown)
        } else {
            None
        };
    }
    if matches!(state.mode(), ViewMode::Browse) {
        let row = ((y - LIST_TOP) / ROW_HEIGHT) as usize;
        return Some(PointerTarget::Entry(state.scroll().saturating_add(row)));
    }
    None
}

fn paint_files(
    canvas: &mut Canvas<'_>,
    width: u32,
    height: u32,
    state: &FileManagerState,
    focused: bool,
    maximized: bool,
) {
    let theme = Theme::LIGHT;
    let background = theme.window_background;
    let toolbar = theme.titlebar_inactive;
    let button = theme.button_fill;
    let row_alt = Color::rgb(0xfa, 0xfa, 0xfa);
    let selected = theme.border_active;
    let text = theme.label_text;
    let muted = theme.text_input_placeholder_fg;

    canvas.fill_rect(Rect::new(0, 0, width, height), background);
    canvas.fill_rect(Rect::new(0, TOOLBAR_Y, width, TOOLBAR_HEIGHT), toolbar);
    draw_button(canvas, 6, TOOLBAR_Y, 58, "Parent", button, text);
    draw_button(canvas, 68, TOOLBAR_Y, 100, "New Folder", button, text);
    draw_button(canvas, 172, TOOLBAR_Y, 72, "Rename", button, text);
    draw_button(canvas, 248, TOOLBAR_Y, 64, "Delete", button, text);
    draw_button(canvas, 316, TOOLBAR_Y, 76, "Refresh", button, text);
    let second_row = TOOLBAR_Y + TOOLBAR_ROW_HEIGHT as i32;
    draw_button(canvas, 6, second_row, 56, "Open", button, text);
    draw_button(canvas, 68, second_row, 72, "Preview", button, text);
    draw_button(canvas, 146, second_row, 64, "Import", button, text);
    draw_button(canvas, 216, second_row, 64, "Export", button, text);

    canvas.fill_rect(
        Rect::new(6, ADDRESS_Y, width.saturating_sub(12), ADDRESS_HEIGHT),
        theme.text_input_bg,
    );
    canvas.stroke_rect(
        Rect::new(6, ADDRESS_Y, width.saturating_sub(12), ADDRESS_HEIGHT),
        theme.text_input_border,
    );
    let address = match state.mode() {
        ViewMode::Preview(preview) => preview.path.display().to_string(),
        _ => state.cwd().display().to_string(),
    };
    canvas.draw_text(10, ADDRESS_Y + 4, &elide(&address, width, 28), text);

    match state.mode() {
        ViewMode::Preview(preview) => {
            let rows = visible_rows(height);
            for (row, line) in preview
                .lines
                .iter()
                .skip(preview.scroll)
                .take(rows)
                .enumerate()
            {
                let y = LIST_TOP + row as i32 * ROW_HEIGHT;
                let label = format!("{:>4}  {}", preview.scroll + row + 1, line);
                canvas.draw_text(8, y + 4, &elide(&label, width, 36), text);
            }
        }
        _ => {
            let rows = visible_rows(height);
            if state.entries().is_empty() {
                canvas.draw_text(12, LIST_TOP + 6, "(empty directory)", muted);
            }
            for (row, (index, entry)) in state
                .entries()
                .iter()
                .enumerate()
                .skip(state.scroll())
                .take(rows)
                .enumerate()
            {
                let y = LIST_TOP + row as i32 * ROW_HEIGHT;
                let is_selected = state.selected_index() == Some(index);
                let fill = if is_selected {
                    selected
                } else if index % 2 == 1 {
                    row_alt
                } else {
                    background
                };
                canvas.fill_rect(
                    Rect::new(
                        0,
                        y,
                        width.saturating_sub(SCROLLBAR_WIDTH as u32),
                        ROW_HEIGHT as u32,
                    ),
                    fill,
                );
                let label = if entry.is_dir {
                    format!("[DIR] {}", entry.name)
                } else {
                    format!("      {}", entry.name)
                };
                canvas.draw_text(
                    10,
                    y + 4,
                    &elide(&label, width.saturating_sub(SCROLLBAR_WIDTH as u32), 24),
                    if is_selected {
                        Color::rgb(0xff, 0xff, 0xff)
                    } else if entry.is_dir {
                        theme.border_active
                    } else {
                        text
                    },
                );
            }
        }
    }

    let status_top = height as i32 - STATUS_HEIGHT as i32;
    let (scroll_total, scroll_offset) = match state.mode() {
        ViewMode::Preview(preview) => (preview.lines.len(), preview.scroll),
        _ => (state.entries().len(), state.scroll()),
    };
    draw_scrollbar(
        canvas,
        width,
        status_top,
        scroll_total,
        scroll_offset,
        visible_rows(height),
        toolbar,
        button,
        text,
    );
    canvas.fill_rect(Rect::new(0, status_top, width, STATUS_HEIGHT), toolbar);
    let (dirs, files) = state.counts();
    let status = match state.mode() {
        ViewMode::Preview(preview) => format!(
            "Read-only preview{} | {} | Esc/Backspace returns",
            if preview.truncated {
                " (first 32 KiB)"
            } else {
                ""
            },
            state.status()
        ),
        _ => format!("{dirs} folders, {files} files | {}", state.status()),
    };
    canvas.draw_text(
        8,
        status_top + 5,
        &elide(&status, width, 16),
        if state.status().starts_with("Error:") {
            Color::rgb(0x9a, 0x18, 0x18)
        } else {
            text
        },
    );

    match state.mode() {
        ViewMode::Input { kind, value, .. } => {
            let label = match kind {
                DialogKind::CreateFolder => "Create folder",
                DialogKind::Rename => "Rename selected item",
            };
            draw_dialog(
                canvas,
                width,
                height,
                label,
                value,
                "Enter confirms | Esc cancels",
            );
        }
        ViewMode::ConfirmDelete { name, is_dir } => {
            let kind = if *is_dir { "folder" } else { "file" };
            draw_dialog(
                canvas,
                width,
                height,
                &format!("Delete {kind}?"),
                name,
                "Enter/Y confirms | N/Esc cancels",
            );
        }
        _ => {}
    }

    let frame = files_window_frame(width, height, focused, maximized);
    frame.draw(canvas);
}

fn files_window_frame(width: u32, height: u32, focused: bool, maximized: bool) -> WindowFrame {
    let mut frame = WindowFrame::new(Rect::new(0, 0, width, height), "Files");
    frame.set_theme(Theme::LIGHT);
    frame.set_focused(focused);
    frame.set_maximized(maximized);
    frame
}

#[allow(clippy::too_many_arguments)]
fn draw_scrollbar(
    canvas: &mut Canvas<'_>,
    width: u32,
    status_top: i32,
    total: usize,
    offset: usize,
    visible: usize,
    track_color: Color,
    button_color: Color,
    text_color: Color,
) {
    let x = width as i32 - SCROLLBAR_WIDTH;
    let list_height = (status_top - LIST_TOP).max(48);
    canvas.fill_rect(
        Rect::new(x, LIST_TOP, SCROLLBAR_WIDTH as u32, list_height as u32),
        track_color,
    );
    canvas.fill_rect(
        Rect::new(x + 2, LIST_TOP + 2, SCROLLBAR_WIDTH as u32 - 4, 20),
        button_color,
    );
    canvas.draw_text(x + 7, LIST_TOP + 7, "^", text_color);
    canvas.fill_rect(
        Rect::new(x + 2, status_top - 22, SCROLLBAR_WIDTH as u32 - 4, 20),
        button_color,
    );
    canvas.draw_text(x + 7, status_top - 17, "v", text_color);

    if total <= visible.max(1) {
        return;
    }
    let track_top = LIST_TOP + 24;
    let track_height = (list_height - 48).max(1);
    let thumb_height = ((track_height as usize * visible.max(1)) / total)
        .max(18)
        .min(track_height as usize) as i32;
    let max_scroll = total.saturating_sub(visible.max(1));
    let travel = track_height.saturating_sub(thumb_height);
    let thumb_y =
        track_top + ((offset.min(max_scroll) * travel as usize) / max_scroll.max(1)) as i32;
    canvas.fill_rect(
        Rect::new(
            x + 5,
            thumb_y,
            SCROLLBAR_WIDTH as u32 - 10,
            thumb_height as u32,
        ),
        Color::rgb(0x8d, 0x9b, 0xa8),
    );
}

fn draw_button(
    canvas: &mut Canvas<'_>,
    x: i32,
    y: i32,
    width: u32,
    label: &str,
    background: Color,
    foreground: Color,
) {
    canvas.fill_rect(
        Rect::new(x, y + 2, width, TOOLBAR_ROW_HEIGHT - 4),
        background,
    );
    canvas.draw_text(x + 5, y + 7, label, foreground);
}

fn draw_dialog(
    canvas: &mut Canvas<'_>,
    width: u32,
    height: u32,
    title: &str,
    value: &str,
    hint: &str,
) {
    let dialog_width = width.saturating_sub(120).max(240);
    let x = ((width.saturating_sub(dialog_width)) / 2) as i32;
    let y = (height as i32 / 2).saturating_sub(50);
    canvas.fill_rect(
        Rect::new(x, y, dialog_width, 100),
        Color::rgb(0x59, 0x69, 0x78),
    );
    canvas.draw_text(x + 10, y + 10, title, Color::rgb(0xff, 0xff, 0xff));
    canvas.fill_rect(
        Rect::new(x + 10, y + 32, dialog_width.saturating_sub(20), 24),
        Color::rgb(0xff, 0xff, 0xff),
    );
    canvas.draw_text(
        x + 15,
        y + 39,
        &elide(value, dialog_width.saturating_sub(30), 0),
        Color::rgb(0x10, 0x10, 0x10),
    );
    canvas.draw_text(x + 10, y + 72, hint, Color::rgb(0xee, 0xee, 0xee));
}

fn elide(value: &str, width: u32, reserved_px: u32) -> String {
    let chars = width.saturating_sub(reserved_px).saturating_div(8) as usize;
    if value.chars().count() <= chars {
        return value.to_string();
    }
    if chars <= 3 {
        return value.chars().take(chars).collect();
    }
    let mut out: String = value.chars().take(chars - 3).collect();
    out.push_str("...");
    out
}

#[cfg(not(target_arch = "wasm32"))]
fn run_cli() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    match cmd.as_deref() {
        None | Some("list") => {
            let dir = args
                .next()
                .or_else(|| std::env::var("HOME").ok())
                .unwrap_or_else(|| "/".to_string());
            let (entries, dirs, files) = list_dir(&dir);
            println!("files (host build) — listing {}", dir);
            for (name, is_dir) in &entries {
                println!("{} {}", if *is_dir { "d" } else { "-" }, name);
            }
            println!("{} folders, {} files", dirs, files);
            ExitCode::SUCCESS
        }
        Some("import") => {
            let Some(target) = args.next() else {
                eprintln!("usage: files import <dir> <name> <hex-bytes>");
                return ExitCode::from(2);
            };
            let Some(name) = args.next() else {
                eprintln!("usage: files import <dir> <name> <hex-bytes>");
                return ExitCode::from(2);
            };
            let hex = args.next().unwrap_or_default();
            let bytes = match decode_hex(&hex) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("files: hex: {}", e);
                    return ExitCode::FAILURE;
                }
            };
            match import_and_dispatch(&target, &name, None, &bytes) {
                Ok((path, dispatch)) => {
                    println!("imported {}", path.display());
                    if let Some(d) = dispatch {
                        println!("dispatch {}", d);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("files: import: {}", e);
                    ExitCode::FAILURE
                }
            }
        }
        Some("export") => {
            let Some(path) = args.next() else {
                eprintln!("usage: files export <path>");
                return ExitCode::from(2);
            };
            match export_bytes(&path) {
                Ok(bytes) => {
                    use std::io::Write;
                    if let Err(e) = std::io::stdout().write_all(&bytes) {
                        eprintln!("files: export write: {}", e);
                        return ExitCode::FAILURE;
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("files: export: {}", e);
                    ExitCode::FAILURE
                }
            }
        }
        Some("rename") => {
            let (Some(old), Some(new)) = (args.next(), args.next()) else {
                eprintln!("usage: files rename <old> <new>");
                return ExitCode::from(2);
            };
            match rename(&old, &new) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("files: rename: {}", e);
                    ExitCode::FAILURE
                }
            }
        }
        Some("dispatch") => {
            let Some(path) = args.next() else {
                eprintln!("usage: files dispatch <path>");
                return ExitCode::from(2);
            };
            let safe = sanitise_filename(&path);
            match default_app_for(&safe, None) {
                Some(d) => {
                    println!("{}", d);
                    ExitCode::SUCCESS
                }
                None => {
                    eprintln!("files: no default app for {}", safe);
                    ExitCode::FAILURE
                }
            }
        }
        Some(other) => {
            eprintln!("files: unknown subcommand {:?}", other);
            ExitCode::from(2)
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd-length hex".to_string());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut chars = s.chars();
    while let (Some(a), Some(b)) = (chars.next(), chars.next()) {
        let hi = a.to_digit(16).ok_or_else(|| format!("bad nibble {a:?}"))?;
        let lo = b.to_digit(16).ok_or_else(|| format!("bad nibble {b:?}"))?;
        out.push(((hi as u8) << 4) | (lo as u8));
    }
    Ok(out)
}

fn main() -> ExitCode {
    #[cfg(target_arch = "wasm32")]
    {
        wasm_main::run();
        // wasm_main::run hands completion to proc_exit.
        #[allow(unreachable_code)]
        ExitCode::SUCCESS
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        run_cli()
    }
}

#[cfg(test)]
mod fairness_tests {
    use super::*;

    #[test]
    fn perpetual_host_notifications_yield_at_read_budget() {
        let mut reads = 0;
        let (chunks, stop) = read_host_chunks_bounded(|buf| {
            reads += 1;
            buf[0] = reads as u8;
            Ok(1)
        });
        assert_eq!(stop, BoundedReadStop::Budget);
        assert_eq!(reads, HOST_NOTIFICATION_READS_PER_TURN);
        assert_eq!(chunks.len(), HOST_NOTIFICATION_READS_PER_TURN);
    }

    #[test]
    fn finite_host_notification_backlog_drains_to_would_block() {
        let mut remaining = 3;
        let mut reads = 0;
        let (chunks, stop) = read_host_chunks_bounded(|buf| {
            reads += 1;
            if remaining == 0 {
                Err(abi::errno::EAGAIN)
            } else {
                remaining -= 1;
                buf[..2].copy_from_slice(b"ok");
                Ok(2)
            }
        });
        assert_eq!(stop, BoundedReadStop::WouldBlock);
        assert_eq!(reads, 4);
        assert_eq!(chunks, vec![b"ok".to_vec(); 3]);
    }

    #[test]
    fn close_acknowledgements_distinguish_protocol_and_client_titlebar_paths() {
        assert_eq!(
            close_acknowledgement(CloseAcknowledgement::DisplayServer),
            "files: close requested by display server"
        );
        assert_eq!(
            close_acknowledgement(CloseAcknowledgement::ClientTitlebar),
            "files: close requested by client titlebar"
        );
    }
}
