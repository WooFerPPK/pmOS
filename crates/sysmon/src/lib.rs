//! `/proc` collection and pure interaction state for System Monitor.
//!
//! The production GUI supplies the real PMos filesystem and process-control
//! syscall. Tests drive this module with synthetic `/proc` trees and explicit
//! timestamps, keeping process enumeration and user actions isolated from the
//! display server.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const NAME_COL: usize = 16;
pub const PROC_ROOT_ENTRIES_PER_STEP: usize = 16;
pub const PROC_FD_ENTRIES_PER_STEP: usize = 32;
const PROC_STATUS_MAX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub pid: u32,
    pub name: String,
    pub state: String,
    pub ppid: u32,
    pub vm_size_kib: u64,
    pub vm_peak_kib: u64,
    pub open_fds: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSnapshot {
    pub pid: u32,
    pub name: String,
    pub state: String,
    pub ppid: u32,
    pub vm_size_kib: u64,
    pub vm_peak_kib: u64,
    pub open_fds: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessCollection {
    pub processes: Vec<ProcessSnapshot>,
    /// Per-pid races or malformed rows. The collection remains usable, but the
    /// GUI surfaces the count instead of silently presenting partial data.
    pub warnings: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProcessScanStep {
    Pending,
    Complete(ProcessCollection),
}

struct PendingFdScan {
    status: StatusSnapshot,
    entries: fs::ReadDir,
    count: usize,
    had_error: bool,
}

enum ProcessScanState {
    Enumerating(fs::ReadDir),
    Reading {
        pids: Vec<u32>,
        index: usize,
        fds: Option<PendingFdScan>,
    },
    Complete,
}

/// Bounded, multi-turn `/proc` collector used by the GUI event loop.
pub struct ProcessScanner {
    proc_root: PathBuf,
    state: ProcessScanState,
    pids: Vec<u32>,
    collection: ProcessCollection,
}

impl ProcessScanner {
    pub fn start(proc_root: &Path) -> Result<Self, String> {
        let entries = fs::read_dir(proc_root)
            .map_err(|error| format!("failed to open {}: {error}", proc_root.display()))?;
        Ok(Self {
            proc_root: proc_root.to_path_buf(),
            state: ProcessScanState::Enumerating(entries),
            pids: Vec::new(),
            collection: ProcessCollection::default(),
        })
    }

    pub fn discovered_pid_count(&self) -> usize {
        match &self.state {
            ProcessScanState::Enumerating(_) => self.pids.len(),
            ProcessScanState::Reading { pids, .. } => pids.len(),
            ProcessScanState::Complete => self.collection.processes.len(),
        }
    }

    pub fn pending_fd_count(&self) -> Option<usize> {
        match &self.state {
            ProcessScanState::Reading { fds: Some(fds), .. } => Some(fds.count),
            _ => None,
        }
    }

    /// Consume at most 16 root entries, one status file, or 32 fd entries.
    pub fn step(&mut self) -> ProcessScanStep {
        match &mut self.state {
            ProcessScanState::Enumerating(entries) => {
                let mut complete = false;
                for _ in 0..PROC_ROOT_ENTRIES_PER_STEP {
                    let Some(entry) = entries.next() else {
                        complete = true;
                        break;
                    };
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            self.collection
                                .warnings
                                .push(format!("proc root entry failed: {error}"));
                            continue;
                        }
                    };
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if let Ok(pid) = name.parse::<u32>() {
                        if let Err(at) = self.pids.binary_search(&pid) {
                            self.pids.insert(at, pid);
                        }
                    }
                }
                if complete {
                    self.state = ProcessScanState::Reading {
                        pids: core::mem::take(&mut self.pids),
                        index: 0,
                        fds: None,
                    };
                }
                ProcessScanStep::Pending
            }
            ProcessScanState::Reading { pids, index, fds } => {
                if let Some(fd_scan) = fds.as_mut() {
                    for _ in 0..PROC_FD_ENTRIES_PER_STEP {
                        match fd_scan.entries.next() {
                            Some(Ok(_)) => fd_scan.count = fd_scan.count.saturating_add(1),
                            Some(Err(_)) => fd_scan.had_error = true,
                            None => {
                                let completed = fds.take().expect("fd scan exists");
                                if completed.had_error {
                                    self.collection.warnings.push(format!(
                                        "pid {}: fd enumeration was partial",
                                        completed.status.pid
                                    ));
                                } else {
                                    self.collection.processes.push(ProcessSnapshot {
                                        pid: completed.status.pid,
                                        name: completed.status.name,
                                        state: completed.status.state,
                                        ppid: completed.status.ppid,
                                        vm_size_kib: completed.status.vm_size_kib,
                                        vm_peak_kib: completed.status.vm_peak_kib,
                                        open_fds: Some(completed.count),
                                    });
                                }
                                *index += 1;
                                return ProcessScanStep::Pending;
                            }
                        }
                    }
                    return ProcessScanStep::Pending;
                }

                if *index >= pids.len() {
                    self.state = ProcessScanState::Complete;
                    return ProcessScanStep::Complete(core::mem::take(&mut self.collection));
                }
                let pid = pids[*index];
                let pid_root = self.proc_root.join(pid.to_string());
                match read_status(&pid_root.join("status")) {
                    Ok(status) if status.pid == pid => {
                        if let Some(open_fds) = status.open_fds {
                            self.collection.processes.push(ProcessSnapshot {
                                pid,
                                name: status.name,
                                state: status.state,
                                ppid: status.ppid,
                                vm_size_kib: status.vm_size_kib,
                                vm_peak_kib: status.vm_peak_kib,
                                open_fds: Some(open_fds),
                            });
                            *index += 1;
                        } else {
                            match fs::read_dir(pid_root.join("fd")) {
                                Ok(entries) => {
                                    *fds = Some(PendingFdScan {
                                        status,
                                        entries,
                                        count: 0,
                                        had_error: false,
                                    });
                                }
                                Err(error) => {
                                    self.collection
                                        .warnings
                                        .push(format!("pid {pid}: fd enumeration failed: {error}"));
                                    *index += 1;
                                }
                            }
                        }
                    }
                    Ok(status) => {
                        self.collection.warnings.push(format!(
                            "pid {pid}: status reported mismatched pid {}",
                            status.pid
                        ));
                        *index += 1;
                    }
                    Err(reason) => {
                        self.collection
                            .warnings
                            .push(format!("pid {pid}: {reason}"));
                        *index += 1;
                    }
                }
                ProcessScanStep::Pending
            }
            ProcessScanState::Complete => {
                ProcessScanStep::Complete(core::mem::take(&mut self.collection))
            }
        }
    }
}

/// Collect one coherent-enough process-table view from `proc_root`.
///
/// Processes may exit between root enumeration and reading `status`/`fd`.
/// Those rows are skipped and reported as warnings. Failure to enumerate the
/// root itself is fatal so callers can preserve their last good snapshot.
pub fn collect_processes(proc_root: &Path) -> Result<ProcessCollection, String> {
    let mut scanner = ProcessScanner::start(proc_root)?;
    loop {
        match scanner.step() {
            ProcessScanStep::Pending => {}
            ProcessScanStep::Complete(collection) => return Ok(collection),
        }
    }
}

/// Compatibility formatter used by the native CLI and older callers.
pub fn collect_snapshot(proc_root: &Path) -> Vec<String> {
    collect_processes(proc_root)
        .map(|collection| {
            collection
                .processes
                .iter()
                .map(format_process_row)
                .collect()
        })
        .unwrap_or_default()
}

pub fn format_process_row(process: &ProcessSnapshot) -> String {
    format!(
        "{:<7}{:<16}  {:<11} {:<7} {:>8} {:>5}",
        process.pid,
        truncate_name(&process.name),
        process.state,
        process.ppid,
        process.vm_size_kib,
        process
            .open_fds
            .map(|count| count.to_string())
            .unwrap_or_else(|| "?".to_string()),
    )
}

pub fn read_status(path: &Path) -> Result<StatusSnapshot, String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|error| format!("status read failed: {error}"))?
        .take(PROC_STATUS_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("status read failed: {error}"))?;
    if bytes.len() as u64 > PROC_STATUS_MAX_BYTES {
        return Err("status exceeds 64 KiB".to_string());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| String::from("status is not UTF-8"))?;
    let mut name = None;
    let mut state = None;
    let mut pid = None;
    let mut ppid = None;
    let mut vm_size_kib = None;
    let mut vm_peak_kib = None;
    let mut open_fds = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("Name:\t") {
            name = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("State:\t") {
            state = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("Pid:\t") {
            pid = Some(parse_u32_field(value, "Pid")?);
        } else if let Some(value) = line.strip_prefix("PPid:\t") {
            ppid = Some(parse_u32_field(value, "PPid")?);
        } else if let Some(value) = line.strip_prefix("VmSize:\t") {
            vm_size_kib = Some(parse_u64_field(value, "VmSize")?);
        } else if let Some(value) = line.strip_prefix("VmPeak:\t") {
            vm_peak_kib = Some(parse_u64_field(value, "VmPeak")?);
        } else if let Some(value) = line.strip_prefix("FDCount:\t") {
            if open_fds.is_some() {
                return Err("duplicate FDCount".to_string());
            }
            open_fds = Some(parse_usize_field(value, "FDCount")?);
        }
    }
    Ok(StatusSnapshot {
        pid: pid.ok_or_else(|| String::from("missing Pid"))?,
        name: name.ok_or_else(|| String::from("missing Name"))?,
        state: state.ok_or_else(|| String::from("missing State"))?,
        ppid: ppid.ok_or_else(|| String::from("missing PPid"))?,
        // Older synthetic proc fixtures pre-date the memory fields. Treat
        // absent values as unknown/zero while rejecting malformed fields.
        vm_size_kib: vm_size_kib.unwrap_or(0),
        vm_peak_kib: vm_peak_kib.unwrap_or(0),
        open_fds,
    })
}

fn parse_u32_field(value: &str, name: &str) -> Result<u32, String> {
    value
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("empty {name}"))?
        .parse()
        .map_err(|_| format!("bad {name}"))
}

fn parse_u64_field(value: &str, name: &str) -> Result<u64, String> {
    value
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("empty {name}"))?
        .parse()
        .map_err(|_| format!("bad {name}"))
}

fn parse_usize_field(value: &str, name: &str) -> Result<usize, String> {
    let mut fields = value.split_whitespace();
    let value = fields.next().ok_or_else(|| format!("empty {name}"))?;
    if fields.next().is_some() {
        return Err(format!("bad {name}"));
    }
    value.parse().map_err(|_| format!("bad {name}"))
}

pub fn truncate_name(name: &str) -> String {
    if name.chars().count() <= NAME_COL {
        return name.to_string();
    }
    let mut out: String = name.chars().take(NAME_COL - 1).collect();
    out.push('…');
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorKey {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Refresh,
    Terminate,
    Enter,
    Escape,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerTarget {
    Row(usize),
    Refresh,
    Terminate,
    ScrollUp,
    ScrollDown,
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MonitorMode {
    Browse,
    ConfirmTerminate { pid: u32, name: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorAction {
    Refresh,
    Terminate(u32),
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorState {
    processes: Vec<ProcessSnapshot>,
    selected_pid: Option<u32>,
    scroll: usize,
    mode: MonitorMode,
    status: String,
    self_pid: Option<u32>,
    terminate_capable: bool,
}

impl MonitorState {
    pub fn new(self_pid: Option<u32>, terminate_capable: bool) -> Self {
        let status = if terminate_capable {
            "Ready; termination requires confirmation".to_string()
        } else {
            "Read-only: PROC_KILL_ANY is not available".to_string()
        };
        Self {
            processes: Vec::new(),
            selected_pid: None,
            scroll: 0,
            mode: MonitorMode::Browse,
            status,
            self_pid,
            terminate_capable,
        }
    }

    pub fn processes(&self) -> &[ProcessSnapshot] {
        &self.processes
    }

    pub fn selected_pid(&self) -> Option<u32> {
        self.selected_pid
    }

    pub fn selected_index(&self) -> Option<usize> {
        let selected = self.selected_pid?;
        self.processes
            .iter()
            .position(|process| process.pid == selected)
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn mode(&self) -> &MonitorMode {
        &self.mode
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn terminate_capable(&self) -> bool {
        self.terminate_capable
    }

    /// Apply a refreshed snapshot while preserving selection by PID. A failed
    /// refresh leaves the last good rows on screen and exposes the error.
    pub fn apply_refresh(
        &mut self,
        result: Result<ProcessCollection, String>,
        visible_rows: usize,
    ) {
        match result {
            Ok(collection) => {
                let pending_termination = match &self.mode {
                    MonitorMode::ConfirmTerminate { pid, name } => Some((*pid, name.clone())),
                    MonitorMode::Browse => None,
                };
                let old_index = self.selected_index().unwrap_or(0);
                let selected_still_exists = self.selected_pid.filter(|pid| {
                    collection
                        .processes
                        .iter()
                        .any(|process| process.pid == *pid)
                });
                self.processes = collection.processes;
                self.selected_pid = selected_still_exists.or_else(|| {
                    self.processes
                        .get(old_index.min(self.processes.len().saturating_sub(1)))
                        .map(|process| process.pid)
                });
                match pending_termination {
                    Some((pid, name))
                        if self.processes.iter().any(|process| process.pid == pid) =>
                    {
                        self.mode = MonitorMode::ConfirmTerminate { pid, name };
                        self.status = format!("Confirm termination of PID {pid}");
                    }
                    Some((pid, _)) => {
                        self.mode = MonitorMode::Browse;
                        self.status =
                            format!("Process PID {pid} exited before termination was confirmed");
                    }
                    None => {
                        self.mode = MonitorMode::Browse;
                        self.status = if collection.warnings.is_empty() {
                            format!(
                                "{} process{}; refreshed",
                                self.processes.len(),
                                if self.processes.len() == 1 { "" } else { "es" }
                            )
                        } else {
                            format!(
                                "Warning: {} process row{} changed during refresh",
                                collection.warnings.len(),
                                if collection.warnings.len() == 1 {
                                    ""
                                } else {
                                    "s"
                                }
                            )
                        };
                    }
                }
                self.ensure_selection_visible(visible_rows);
            }
            Err(error) => {
                self.status = format!("Error: {error}");
            }
        }
    }

    pub fn handle_key(&mut self, key: MonitorKey, visible_rows: usize) -> Option<MonitorAction> {
        match self.mode.clone() {
            MonitorMode::ConfirmTerminate { pid, .. } => match key {
                MonitorKey::Enter => {
                    self.mode = MonitorMode::Browse;
                    Some(MonitorAction::Terminate(pid))
                }
                MonitorKey::Escape => {
                    self.mode = MonitorMode::Browse;
                    self.status = "Termination cancelled".to_string();
                    None
                }
                MonitorKey::Close => Some(MonitorAction::Close),
                _ => None,
            },
            MonitorMode::Browse => match key {
                MonitorKey::Up => {
                    self.move_selection(-1, visible_rows);
                    None
                }
                MonitorKey::Down => {
                    self.move_selection(1, visible_rows);
                    None
                }
                MonitorKey::PageUp => {
                    self.move_selection(-(visible_rows.max(1) as isize), visible_rows);
                    None
                }
                MonitorKey::PageDown => {
                    self.move_selection(visible_rows.max(1) as isize, visible_rows);
                    None
                }
                MonitorKey::Home => {
                    self.select_index(0, visible_rows);
                    None
                }
                MonitorKey::End => {
                    self.select_index(self.processes.len().saturating_sub(1), visible_rows);
                    None
                }
                MonitorKey::Refresh => Some(MonitorAction::Refresh),
                MonitorKey::Terminate => {
                    self.begin_terminate();
                    None
                }
                MonitorKey::Close => Some(MonitorAction::Close),
                MonitorKey::Enter | MonitorKey::Escape => None,
            },
        }
    }

    pub fn handle_pointer(
        &mut self,
        target: PointerTarget,
        visible_rows: usize,
    ) -> Option<MonitorAction> {
        if !matches!(self.mode, MonitorMode::Browse) {
            return None;
        }
        match target {
            PointerTarget::Row(index) => {
                self.select_index(index, visible_rows);
                None
            }
            PointerTarget::Refresh => Some(MonitorAction::Refresh),
            PointerTarget::Terminate => {
                self.begin_terminate();
                None
            }
            PointerTarget::ScrollUp => {
                self.move_selection(-1, visible_rows);
                None
            }
            PointerTarget::ScrollDown => {
                self.move_selection(1, visible_rows);
                None
            }
            PointerTarget::Close => Some(MonitorAction::Close),
        }
    }

    pub fn complete_termination(&mut self, pid: u32, result: Result<(), String>) {
        self.status = match result {
            Ok(()) => format!("Terminate requested for PID {pid}"),
            Err(error) => format!("Error: failed to terminate PID {pid}: {error}"),
        };
    }

    fn begin_terminate(&mut self) {
        if !self.terminate_capable {
            self.status = "Read-only: PROC_KILL_ANY is not available".to_string();
            return;
        }
        let Some(process) = self
            .selected_pid
            .and_then(|pid| self.processes.iter().find(|process| process.pid == pid))
        else {
            self.status = "Select a process first".to_string();
            return;
        };
        if self.self_pid == Some(process.pid) {
            self.status = "Refusing to terminate System Monitor itself".to_string();
            return;
        }
        self.mode = MonitorMode::ConfirmTerminate {
            pid: process.pid,
            name: process.name.clone(),
        };
        self.status = format!("Confirm termination of PID {}", process.pid);
    }

    fn move_selection(&mut self, delta: isize, visible_rows: usize) {
        if self.processes.is_empty() {
            self.selected_pid = None;
            self.scroll = 0;
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let last = self.processes.len().saturating_sub(1) as isize;
        self.select_index((current + delta).clamp(0, last) as usize, visible_rows);
    }

    fn select_index(&mut self, index: usize, visible_rows: usize) {
        if let Some(process) = self.processes.get(index) {
            self.selected_pid = Some(process.pid);
            self.ensure_selection_visible(visible_rows);
        }
    }

    fn ensure_selection_visible(&mut self, visible_rows: usize) {
        let visible = visible_rows.max(1);
        let max_scroll = self.processes.len().saturating_sub(visible);
        if let Some(index) = self.selected_index() {
            if index < self.scroll {
                self.scroll = index;
            } else if index >= self.scroll.saturating_add(visible) {
                self.scroll = index + 1 - visible;
            }
        }
        self.scroll = self.scroll.min(max_scroll);
    }
}

/// Deterministic one-second refresh scheduler. The GUI feeds elapsed
/// monotonic milliseconds; isolation tests feed exact values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshSchedule {
    interval_ms: u64,
    next_ms: u64,
}

impl RefreshSchedule {
    pub fn new(now_ms: u64, interval_ms: u64) -> Self {
        Self {
            interval_ms: interval_ms.max(1),
            next_ms: now_ms.saturating_add(interval_ms.max(1)),
        }
    }

    pub fn take_due(&mut self, now_ms: u64) -> bool {
        if now_ms < self.next_ms {
            return false;
        }
        self.next_ms = now_ms.saturating_add(self.interval_ms);
        true
    }

    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.next_ms.saturating_sub(now_ms)
    }
}
