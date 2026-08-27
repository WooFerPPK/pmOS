//! Shell-owned durable desktop-session capture and restore coordinator.
//!
//! Display identity and geometry enter through authenticated shell-manager v2
//! events. Filesystem work is deliberately advanced one operation at a time;
//! protocol requests are returned as actions so the paint loop can preserve
//! its display-first ordering.

use crate::launcher::DesktopEntry;
use crate::session_store::{
    AtomicSessionWriter, SessionFile, SessionFilesystem, SessionLoadStep, SessionLoader,
    SessionWait, SessionWriteStep, StdSessionFilesystem, StoredInstance, StoredSession,
    StoredWindow, MAX_SESSION_IDENTIFIER_BYTES, MAX_SESSION_WINDOWS, SESSION_FLAG_MAXIMIZED,
    SESSION_FLAG_MINIMIZED, SESSION_IO_CHUNK_BYTES, SESSION_PATH,
};
use display_proto::events::{shell_restore_status, shell_window_state_flags, ShellWindowState};
use display_proto::requests::MAX_SHELL_RESTORE_TIMEOUT_MS;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

pub const SESSION_SNAPSHOT_ID: u32 = 1;
pub const SESSION_RESTORE_ID: u32 = 1;
pub const SESSION_RESTORE_SOFT_DEADLINE: Duration = Duration::from_millis(2_250);
pub const SESSION_CAPTURE_COALESCE: Duration = Duration::from_millis(250);
const MAX_PROC_STATUS_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogIdentity {
    pub id: String,
    pub exec: String,
}

impl From<&DesktopEntry> for CatalogIdentity {
    fn from(entry: &DesktopEntry) -> Self {
        Self {
            id: entry.id.clone(),
            exec: entry.exec.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionAction {
    BeginRestore {
        restore_id: u32,
        timeout_ms: u32,
    },
    Spawn {
        instance_id: u32,
        exec: String,
    },
    Place {
        restore_id: u32,
        window_id: u32,
        normal_x: i32,
        normal_y: i32,
        normal_width: u32,
        normal_height: u32,
        z_rank: u32,
        flags: u32,
    },
    EndRestore {
        restore_id: u32,
        focus_window_id: u32,
    },
}

#[derive(Clone, Debug)]
struct RestoreInstance {
    stored_id: u32,
    desktop_entry_id: String,
    exec: String,
    pid: Option<u32>,
    attempted: bool,
    failed: bool,
}

#[derive(Clone, Debug)]
struct RestoreWindow {
    stored: StoredWindow,
    live_window_id: Option<u32>,
    placement_sent: bool,
    sent_rank: Option<u32>,
}

#[derive(Clone, Debug)]
struct RestorePlan {
    instances: Vec<RestoreInstance>,
    windows: Vec<RestoreWindow>,
    focused_record: Option<u32>,
}

impl RestorePlan {
    fn from_stored(stored: &StoredSession, catalog: &[CatalogIdentity]) -> Option<Self> {
        let mut instances = Vec::new();
        let mut admitted = BTreeSet::new();
        for instance in &stored.instances {
            let mut matches = catalog
                .iter()
                .filter(|entry| entry.id == instance.desktop_entry_id);
            let Some(entry) = matches.next() else {
                continue;
            };
            if matches.next().is_some() {
                continue;
            }
            admitted.insert(instance.id);
            instances.push(RestoreInstance {
                stored_id: instance.id,
                desktop_entry_id: instance.desktop_entry_id.clone(),
                exec: entry.exec.clone(),
                pid: None,
                attempted: false,
                failed: false,
            });
        }
        let mut windows = stored
            .windows
            .iter()
            .filter(|window| admitted.contains(&window.instance_id))
            .cloned()
            .map(|stored| RestoreWindow {
                stored,
                live_window_id: None,
                placement_sent: false,
                sent_rank: None,
            })
            .collect::<Vec<_>>();
        windows.sort_by_key(|window| window.stored.z_rank);
        for (rank, window) in windows.iter_mut().enumerate() {
            window.stored.z_rank = rank as u32;
        }
        instances.retain(|instance| {
            windows
                .iter()
                .any(|window| window.stored.instance_id == instance.stored_id)
        });
        (!instances.is_empty() && !windows.is_empty()).then_some(Self {
            instances,
            windows,
            focused_record: stored.focused_window,
        })
    }

    fn instance_mut(&mut self, id: u32) -> Option<&mut RestoreInstance> {
        self.instances
            .iter_mut()
            .find(|instance| instance.stored_id == id)
    }

    fn instance(&self, id: u32) -> Option<&RestoreInstance> {
        self.instances
            .iter()
            .find(|instance| instance.stored_id == id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RestorePhase {
    Waiting,
    NeedBegin,
    BeginSent,
    AwaitBarrier { callback_id: u32 },
    Active,
    AwaitFinished,
    Finished,
}

enum ProcReaderState {
    Open,
    Read {
        file: Box<dyn SessionFile>,
        bytes: Vec<u8>,
        invalid: bool,
        blocked: bool,
    },
    Close {
        file: Box<dyn SessionFile>,
        bytes: Vec<u8>,
        invalid: bool,
    },
}

struct ProcReader {
    pid: u32,
    path: PathBuf,
    state: ProcReaderState,
}

impl ProcReader {
    fn new(pid: u32) -> Self {
        Self {
            pid,
            path: PathBuf::from(format!("/proc/{pid}/status")),
            state: ProcReaderState::Open,
        }
    }

    fn wait(&self) -> Option<SessionWait> {
        match &self.state {
            ProcReaderState::Read {
                file,
                blocked: true,
                ..
            } => file.wait_fd().map(SessionWait::Read),
            _ => None,
        }
    }

    fn step(&mut self, filesystem: &mut dyn SessionFilesystem) -> Option<(u32, Option<String>)> {
        let state = core::mem::replace(&mut self.state, ProcReaderState::Open);
        match state {
            ProcReaderState::Open => match filesystem.open_read(&self.path) {
                Ok(file) => {
                    self.state = ProcReaderState::Read {
                        file,
                        bytes: Vec::new(),
                        invalid: false,
                        blocked: false,
                    };
                    None
                }
                Err(_) => Some((self.pid, None)),
            },
            ProcReaderState::Read {
                mut file,
                mut bytes,
                invalid,
                ..
            } => {
                let mut chunk = [0u8; SESSION_IO_CHUNK_BYTES];
                match file.read(&mut chunk) {
                    Ok(0) => {
                        self.state = ProcReaderState::Close {
                            file,
                            bytes,
                            invalid,
                        };
                    }
                    Ok(read) => {
                        let remaining = MAX_PROC_STATUS_BYTES
                            .saturating_add(1)
                            .saturating_sub(bytes.len());
                        bytes.extend_from_slice(&chunk[..read.min(remaining)]);
                        if invalid || bytes.len() > MAX_PROC_STATUS_BYTES {
                            self.state = ProcReaderState::Close {
                                file,
                                bytes,
                                invalid: true,
                            };
                        } else {
                            self.state = ProcReaderState::Read {
                                file,
                                bytes,
                                invalid: false,
                                blocked: false,
                            };
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        self.state = ProcReaderState::Read {
                            file,
                            bytes,
                            invalid,
                            blocked: true,
                        };
                    }
                    Err(_) => {
                        self.state = ProcReaderState::Close {
                            file,
                            bytes,
                            invalid: true,
                        };
                    }
                }
                None
            }
            ProcReaderState::Close {
                file,
                bytes,
                invalid,
            } => {
                let closed = filesystem.close(file).is_ok();
                let name = (!invalid && closed)
                    .then(|| parse_proc_name(&bytes))
                    .flatten();
                Some((self.pid, name))
            }
        }
    }
}

fn parse_proc_name(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut names = text.lines().filter_map(|line| line.strip_prefix("Name:\t"));
    let name = names.next()?;
    if names.next().is_some()
        || name.is_empty()
        || name.len() > MAX_PROC_STATUS_BYTES
        || name
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return None;
    }
    Some(name.to_string())
}

fn is_shell_owned_state(state: &ShellWindowState) -> bool {
    state.flags & shell_window_state_flags::SHELL_OWNED != 0
}

/// Shell session runtime. Legacy shell entry points do not construct this
/// value, preserving their historical no-session fixture behaviour.
pub struct SessionRuntime {
    filesystem: Box<dyn SessionFilesystem>,
    own_pid: Option<u32>,
    loader: SessionLoader,
    writer: AtomicSessionWriter,
    load_complete: bool,
    loaded: Option<StoredSession>,
    catalog_ready: bool,
    catalog: Vec<CatalogIdentity>,
    snapshot_done: bool,
    live: BTreeMap<u32, ShellWindowState>,
    focused_window: Option<u32>,
    identities: BTreeMap<u32, Option<String>>,
    identity_queue: VecDeque<u32>,
    proc_reader: Option<ProcReader>,
    restore_phase: RestorePhase,
    barrier_callback_id: Option<u32>,
    restore_plan: Option<RestorePlan>,
    restore_deadline: Option<Duration>,
    restore_soft_expired: bool,
    capture_dirty: bool,
    capture_timestamp_pending: bool,
    capture_deadline: Option<Duration>,
    now: Duration,
    output_size: (u32, u32),
    revision_counts: BTreeMap<u64, (usize, usize)>,
    restored_reported: bool,
}

impl SessionRuntime {
    pub fn production(own_pid: u32) -> Self {
        Self::with_filesystem_for_pid(
            SESSION_PATH,
            Box::<StdSessionFilesystem>::default(),
            own_pid,
        )
    }

    /// Kernel-authenticated PID of the shell process, when this runtime was
    /// constructed for production or a production-shaped isolation test.
    pub const fn own_pid(&self) -> Option<u32> {
        self.own_pid
    }

    pub fn with_filesystem(
        path: impl Into<PathBuf>,
        filesystem: Box<dyn SessionFilesystem>,
    ) -> Self {
        Self::with_optional_pid(path, filesystem, None)
    }

    pub fn with_filesystem_for_pid(
        path: impl Into<PathBuf>,
        filesystem: Box<dyn SessionFilesystem>,
        own_pid: u32,
    ) -> Self {
        Self::with_optional_pid(path, filesystem, Some(own_pid))
    }

    fn with_optional_pid(
        path: impl Into<PathBuf>,
        filesystem: Box<dyn SessionFilesystem>,
        own_pid: Option<u32>,
    ) -> Self {
        let path = path.into();
        Self {
            filesystem,
            own_pid,
            loader: SessionLoader::new(path.clone()),
            writer: AtomicSessionWriter::new(path),
            load_complete: false,
            loaded: None,
            catalog_ready: false,
            catalog: Vec::new(),
            snapshot_done: false,
            live: BTreeMap::new(),
            focused_window: None,
            identities: BTreeMap::new(),
            identity_queue: VecDeque::new(),
            proc_reader: None,
            restore_phase: RestorePhase::Waiting,
            barrier_callback_id: None,
            restore_plan: None,
            restore_deadline: None,
            restore_soft_expired: false,
            capture_dirty: false,
            capture_timestamp_pending: false,
            capture_deadline: None,
            now: Duration::ZERO,
            output_size: (0, 0),
            revision_counts: BTreeMap::new(),
            restored_reported: false,
        }
    }

    pub fn set_catalog(&mut self, entries: &[DesktopEntry]) {
        let next = entries
            .iter()
            .filter(|entry| valid_catalog_id(&entry.id))
            .map(CatalogIdentity::from)
            .collect::<Vec<_>>();
        if self.catalog != next {
            self.catalog = next;
            self.identities.clear();
            self.identity_queue.clear();
            if self.restore_phase == RestorePhase::Finished {
                self.mark_capture_dirty();
            }
        }
        self.catalog_ready = true;
    }

    pub fn mark_empty_catalog_ready(&mut self) {
        self.catalog_ready = true;
    }

    pub fn set_output_size(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 && self.output_size != (width, height) {
            self.output_size = (width, height);
            if self.restore_phase == RestorePhase::Finished {
                self.mark_capture_dirty();
            }
        }
    }

    pub fn set_now(&mut self, now: Duration) {
        self.now = now;
        if self.capture_dirty && self.capture_timestamp_pending {
            self.capture_deadline = Some(now + SESSION_CAPTURE_COALESCE);
            self.capture_timestamp_pending = false;
        }
    }

    pub fn now(&self) -> Duration {
        self.now
    }

    pub fn needs_clock_sample(&self) -> bool {
        self.capture_dirty
            || self.restore_phase == RestorePhase::Waiting
                && self.load_complete
                && self.catalog_ready
                && self.snapshot_done
                && self.proc_reader.is_none()
                && self.identity_queue.is_empty()
            || self.restore_phase == RestorePhase::NeedBegin
            || self.restore_deadline.is_some()
                && matches!(
                    self.restore_phase,
                    RestorePhase::BeginSent
                        | RestorePhase::AwaitBarrier { .. }
                        | RestorePhase::Active
                )
    }

    pub fn observe_window_state(&mut self, state: ShellWindowState) {
        if is_shell_owned_state(&state) {
            let removed_owner = self
                .live
                .remove(&state.window_id)
                .map(|removed| removed.owner_pid);
            let focus_changed = self.focused_window == Some(state.window_id);
            if focus_changed {
                self.focused_window = None;
            }
            if let Some(owner_pid) = removed_owner {
                self.prune_identity_if_unused(owner_pid);
            }
            if (removed_owner.is_some() || focus_changed)
                && self.restore_phase == RestorePhase::Finished
            {
                self.mark_capture_dirty();
            }
            return;
        }
        let changed = self.live.get(&state.window_id) != Some(&state);
        let previous_owner = self
            .live
            .get(&state.window_id)
            .map(|previous| previous.owner_pid);
        if state.flags & shell_window_state_flags::FOCUSED != 0 {
            self.focused_window = Some(state.window_id);
        } else if self.focused_window == Some(state.window_id) {
            self.focused_window = None;
        }
        self.live.insert(state.window_id, state);
        if let Some(previous_owner) = previous_owner {
            self.prune_identity_if_unused(previous_owner);
        }
        if changed && self.restore_phase == RestorePhase::Finished {
            self.mark_capture_dirty();
        }
    }

    pub fn observe_window_destroyed(&mut self, window_id: u32) {
        if let Some(removed) = self.live.remove(&window_id) {
            if self.focused_window == Some(window_id) {
                self.focused_window = None;
            }
            self.prune_identity_if_unused(removed.owner_pid);
            if self.restore_phase == RestorePhase::Finished {
                self.mark_capture_dirty();
            }
        }
    }

    pub fn observe_window_focused(&mut self, window_id: u32) {
        let focused = (window_id != 0).then_some(window_id);
        if self.focused_window != focused {
            self.focused_window = focused;
            if self.restore_phase == RestorePhase::Finished {
                self.mark_capture_dirty();
            }
        }
    }

    pub fn observe_snapshot_done(&mut self, snapshot_id: u32) {
        if snapshot_id == SESSION_SNAPSHOT_ID {
            self.snapshot_done = true;
        }
    }

    pub fn barrier_sent(&mut self, callback_id: u32) {
        if self.restore_phase == RestorePhase::BeginSent {
            self.barrier_callback_id = Some(callback_id);
            self.restore_phase = RestorePhase::AwaitBarrier { callback_id };
        }
    }

    pub fn observe_callback(&mut self, callback_id: u32) -> bool {
        if self.barrier_callback_id != Some(callback_id) {
            return false;
        }
        self.barrier_callback_id = None;
        if self.restore_phase == (RestorePhase::AwaitBarrier { callback_id }) {
            self.restore_phase = RestorePhase::Active;
        }
        true
    }

    pub fn spawn_result(&mut self, instance_id: u32, result: i32) {
        let Some(plan) = self.restore_plan.as_mut() else {
            return;
        };
        let Some(instance) = plan.instance_mut(instance_id) else {
            return;
        };
        if result > 0 {
            let pid = result as u32;
            instance.pid = Some(pid);
            self.identities
                .insert(pid, Some(instance.desktop_entry_id.clone()));
        } else {
            instance.failed = true;
        }
    }

    pub fn observe_restore_finished(&mut self, restore_id: u32, status: u32, placed: u32) {
        if restore_id != SESSION_RESTORE_ID
            || !matches!(
                self.restore_phase,
                RestorePhase::BeginSent
                    | RestorePhase::AwaitBarrier { .. }
                    | RestorePhase::Active
                    | RestorePhase::AwaitFinished
            )
        {
            return;
        }
        let apps = self
            .restore_plan
            .as_ref()
            .map(|plan| {
                plan.instances
                    .iter()
                    .filter(|instance| instance.pid.is_some())
                    .count()
            })
            .unwrap_or(0);
        if !self.restored_reported {
            println!(
                "shell: session restored status={} apps={} windows={}",
                restore_status_name(status),
                apps,
                placed
            );
            self.restored_reported = true;
        }
        self.restore_phase = RestorePhase::Finished;
        self.restore_deadline = None;
        self.prune_identities_without_live_windows();
        self.mark_capture_dirty();
    }

    /// Advance one filesystem operation. Returns whether another non-blocked
    /// local step is immediately available.
    pub fn step_io(&mut self) -> bool {
        if !self.load_complete {
            match self.loader.step(self.filesystem.as_mut()) {
                SessionLoadStep::Pending => return self.loader.wait().is_none(),
                SessionLoadStep::Complete(session) => {
                    self.loaded = session;
                    self.load_complete = true;
                    return true;
                }
            }
        }

        self.queue_unresolved_identities();
        if self.proc_reader.is_none() {
            if let Some(pid) = self.identity_queue.pop_front() {
                self.proc_reader = Some(ProcReader::new(pid));
            }
        }
        if self.capture_due() {
            self.prepare_capture();
            return self.step_writer();
        }
        if self
            .proc_reader
            .as_ref()
            .and_then(ProcReader::wait)
            .is_some()
            && self.writer.pending()
        {
            return self.step_writer();
        }
        if let Some(reader) = self.proc_reader.as_mut() {
            if let Some((pid, name)) = reader.step(self.filesystem.as_mut()) {
                self.proc_reader = None;
                let resolved = name.and_then(|name| self.unique_catalog_id_for_exec(&name));
                self.record_identity(pid, resolved);
                return true;
            }
            return self
                .proc_reader
                .as_ref()
                .and_then(ProcReader::wait)
                .is_none();
        }

        self.maybe_prepare_restore();
        if self.capture_due() {
            self.prepare_capture();
        }
        self.step_writer()
    }

    fn capture_due(&self) -> bool {
        self.restore_phase == RestorePhase::Finished
            && self.capture_dirty
            && !self.capture_timestamp_pending
            && self
                .capture_deadline
                .is_none_or(|deadline| self.now >= deadline)
    }

    fn prepare_capture(&mut self) {
        let snapshot = self.build_snapshot();
        self.capture_dirty = false;
        self.capture_timestamp_pending = false;
        self.capture_deadline = None;
        if let Some(snapshot) = snapshot {
            match self.writer.request(&snapshot) {
                Ok(revision) => {
                    self.revision_counts
                        .retain(|pending, _| self.writer.has_queued_revision(*pending));
                    if self.writer.has_queued_revision(revision) {
                        self.revision_counts
                            .insert(revision, (snapshot.instances.len(), snapshot.windows.len()));
                    }
                }
                Err(error) => eprintln!("shell: session snapshot rejected: {error:?}"),
            }
        }
    }

    fn step_writer(&mut self) -> bool {
        match self.writer.step(self.filesystem.as_mut()) {
            SessionWriteStep::Idle => false,
            SessionWriteStep::Pending => self.writer.wait().is_none(),
            SessionWriteStep::Durable {
                revision,
                bytes,
                digest,
            } => {
                let (apps, windows) = self.revision_counts.remove(&revision).unwrap_or_default();
                self.revision_counts
                    .retain(|pending, _| *pending > revision);
                println!(
                    "shell: session durable revision={revision} apps={apps} windows={windows} bytes={bytes} digest={digest:016x}"
                );
                self.writer.pending()
            }
            SessionWriteStep::Failed(revision) => {
                self.revision_counts.remove(&revision);
                self.writer.pending()
            }
        }
    }

    pub fn next_action(&mut self, now: Duration) -> Option<SessionAction> {
        self.now = now;
        self.maybe_prepare_restore();
        if self
            .restore_deadline
            .is_some_and(|deadline| now >= deadline)
            && matches!(
                self.restore_phase,
                RestorePhase::BeginSent | RestorePhase::AwaitBarrier { .. } | RestorePhase::Active
            )
        {
            self.restore_deadline = None;
            self.restore_soft_expired = true;
        }
        match self.restore_phase {
            RestorePhase::NeedBegin => {
                self.restore_phase = RestorePhase::BeginSent;
                self.restore_deadline = Some(now + SESSION_RESTORE_SOFT_DEADLINE);
                self.restore_soft_expired = false;
                Some(SessionAction::BeginRestore {
                    restore_id: SESSION_RESTORE_ID,
                    timeout_ms: MAX_SHELL_RESTORE_TIMEOUT_MS,
                })
            }
            RestorePhase::Active => {
                let plan = self.restore_plan.as_mut()?;
                if !self.restore_soft_expired {
                    if let Some(instance) = plan
                        .instances
                        .iter_mut()
                        .find(|instance| !instance.attempted)
                    {
                        instance.attempted = true;
                        return Some(SessionAction::Spawn {
                            instance_id: instance.stored_id,
                            exec: instance.exec.clone(),
                        });
                    }
                }

                let instance_pids = plan
                    .instances
                    .iter()
                    .filter_map(|instance| instance.pid.map(|pid| (instance.stored_id, pid)))
                    .collect::<BTreeMap<_, _>>();
                let arrival_rank = plan
                    .windows
                    .iter()
                    .filter(|candidate| candidate.placement_sent)
                    .count() as u32;
                if !self.restore_soft_expired {
                    for window in &mut plan.windows {
                        if window.placement_sent {
                            continue;
                        }
                        let Some(pid) = instance_pids.get(&window.stored.instance_id).copied()
                        else {
                            continue;
                        };
                        let Some(state) = self.live.values().find(|state| {
                            state.owner_pid == pid && state.ordinal == window.stored.ordinal
                        }) else {
                            continue;
                        };
                        window.live_window_id = Some(state.window_id);
                        window.placement_sent = true;
                        window.sent_rank = Some(arrival_rank);
                        return Some(SessionAction::Place {
                            restore_id: SESSION_RESTORE_ID,
                            window_id: state.window_id,
                            normal_x: window.stored.normal_x,
                            normal_y: window.stored.normal_y,
                            normal_width: window.stored.normal_width,
                            normal_height: window.stored.normal_height,
                            z_rank: arrival_rank,
                            flags: window.stored.flags,
                        });
                    }
                }

                let mut placed = plan
                    .windows
                    .iter()
                    .enumerate()
                    .filter(|(_, window)| window.placement_sent)
                    .map(|(index, window)| (index, window.stored.z_rank))
                    .collect::<Vec<_>>();
                placed.sort_by_key(|(_, stored_rank)| *stored_rank);
                if let Some((index, desired_rank)) =
                    placed.iter().enumerate().find_map(|(rank, (index, _))| {
                        (plan.windows[*index].sent_rank != Some(rank as u32))
                            .then_some((*index, rank as u32))
                    })
                {
                    let window = &mut plan.windows[index];
                    window.sent_rank = Some(desired_rank);
                    return Some(SessionAction::Place {
                        restore_id: SESSION_RESTORE_ID,
                        window_id: window
                            .live_window_id
                            .expect("placed restore window has a live id"),
                        normal_x: window.stored.normal_x,
                        normal_y: window.stored.normal_y,
                        normal_width: window.stored.normal_width,
                        normal_height: window.stored.normal_height,
                        z_rank: desired_rank,
                        flags: window.stored.flags,
                    });
                }

                let all_attempted = plan.instances.iter().all(|instance| instance.attempted);
                let all_available_settled = plan.windows.iter().all(|window| {
                    let Some(instance) = plan.instance(window.stored.instance_id) else {
                        return true;
                    };
                    if instance.failed {
                        return true;
                    }
                    let Some(window_id) = window.live_window_id else {
                        return false;
                    };
                    window.placement_sent
                        && self.live.get(&window_id).is_some_and(|state| {
                            state.flags & shell_window_state_flags::MAPPED != 0
                                && state.flags & shell_window_state_flags::RESTORE_PLACEMENT_APPLIED
                                    != 0
                        })
                });
                let all_placed_applied = plan.windows.iter().all(|window| {
                    if !window.placement_sent {
                        return true;
                    }
                    window.live_window_id.is_some_and(|window_id| {
                        self.live.get(&window_id).is_some_and(|state| {
                            state.flags & shell_window_state_flags::MAPPED != 0
                                && state.flags & shell_window_state_flags::RESTORE_PLACEMENT_APPLIED
                                    != 0
                        })
                    })
                });
                if all_attempted
                    && (all_available_settled || self.restore_soft_expired && all_placed_applied)
                {
                    let focus_window_id = plan
                        .focused_record
                        .and_then(|record| {
                            plan.windows
                                .iter()
                                .find(|window| window.stored.id == record)
                        })
                        .filter(|window| !window.stored.minimized())
                        .and_then(|window| window.live_window_id)
                        .unwrap_or(0);
                    self.restore_phase = RestorePhase::AwaitFinished;
                    return Some(SessionAction::EndRestore {
                        restore_id: SESSION_RESTORE_ID,
                        focus_window_id,
                    });
                }
                None
            }
            _ => None,
        }
    }

    pub fn next_deadline(&self, now: Duration) -> Option<Duration> {
        let restore = if matches!(
            self.restore_phase,
            RestorePhase::BeginSent | RestorePhase::AwaitBarrier { .. } | RestorePhase::Active
        ) {
            self.restore_deadline
                .map(|deadline| deadline.saturating_sub(now))
        } else {
            None
        };
        let capture = (self.restore_phase == RestorePhase::Finished
            && self.capture_dirty
            && !self.capture_timestamp_pending)
            .then(|| {
                self.capture_deadline
                    .map(|deadline| deadline.saturating_sub(now))
            })
            .flatten();
        match (restore, capture) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        }
    }

    pub fn wait_fds(&self) -> Vec<SessionWait> {
        let mut waits = Vec::new();
        if !self.load_complete {
            waits.extend(self.loader.wait());
        }
        if let Some(reader) = self.proc_reader.as_ref() {
            waits.extend(reader.wait());
        }
        waits.extend(self.writer.wait());
        waits
    }

    pub fn has_local_work(&self) -> bool {
        if !self.load_complete {
            return self.loader.wait().is_none();
        }
        if let Some(reader) = self.proc_reader.as_ref() {
            return self.capture_due()
                || reader.wait().is_none()
                || self.writer.pending() && self.writer.wait().is_none();
        }
        !self.identity_queue.is_empty()
            || self.capture_dirty
                && self
                    .capture_deadline
                    .is_none_or(|deadline| self.now >= deadline)
            || self.writer.pending() && self.writer.wait().is_none()
            || self.restore_phase == RestorePhase::NeedBegin
            || self.restore_phase == RestorePhase::Active && self.active_actionable()
    }

    pub fn ready_for_desktop(&self) -> bool {
        self.load_complete
            && self.catalog_ready
            && self.snapshot_done
            && self.restore_phase == RestorePhase::Finished
    }

    pub fn allows_user_launch(&self) -> bool {
        self.restore_phase == RestorePhase::Finished
    }

    fn queue_unresolved_identities(&mut self) {
        if self.restore_phase != RestorePhase::Finished
            || !self.snapshot_done
            || !self.catalog_ready
        {
            return;
        }
        let mut queued = self.identity_queue.iter().copied().collect::<BTreeSet<_>>();
        if let Some(reader) = self.proc_reader.as_ref() {
            queued.insert(reader.pid);
        }
        for state in self.live.values() {
            if !self.identities.contains_key(&state.owner_pid) && queued.insert(state.owner_pid) {
                self.identity_queue.push_back(state.owner_pid);
            }
        }
    }

    fn unique_catalog_id_for_exec(&self, exec: &str) -> Option<String> {
        let mut matches = self.catalog.iter().filter(|entry| entry.exec == exec);
        let id = matches.next()?.id.clone();
        matches.next().is_none().then_some(id)
    }

    fn record_identity(&mut self, pid: u32, resolved: Option<String>) {
        if !self.live.values().any(|state| state.owner_pid == pid) {
            self.identities.remove(&pid);
            return;
        }
        let changed = self.identities.get(&pid) != Some(&resolved);
        self.identities.insert(pid, resolved);
        if changed && self.restore_phase == RestorePhase::Finished {
            self.mark_capture_dirty();
        }
    }

    fn prune_identity_if_unused(&mut self, pid: u32) {
        if self.live.values().all(|state| state.owner_pid != pid) {
            self.identities.remove(&pid);
            self.identity_queue.retain(|queued| *queued != pid);
        }
    }

    fn prune_identities_without_live_windows(&mut self) {
        let live = self
            .live
            .values()
            .map(|state| state.owner_pid)
            .collect::<BTreeSet<_>>();
        self.identities.retain(|pid, _| live.contains(pid));
        self.identity_queue.retain(|pid| live.contains(pid));
    }

    fn maybe_prepare_restore(&mut self) {
        if self.restore_phase != RestorePhase::Waiting
            || !self.load_complete
            || !self.catalog_ready
            || !self.snapshot_done
        {
            return;
        }

        let replacement_has_apps = self.own_pid.is_some_and(|_| {
            self.live.values().any(|state| {
                !is_shell_owned_state(state) && state.flags & shell_window_state_flags::MAPPED != 0
            })
        });
        if replacement_has_apps {
            self.restore_phase = RestorePhase::Finished;
            self.mark_capture_dirty();
            return;
        }

        self.restore_plan = self
            .loaded
            .as_ref()
            .and_then(|stored| RestorePlan::from_stored(stored, &self.catalog));
        if self.restore_plan.is_some() {
            self.restore_phase = RestorePhase::NeedBegin;
        } else {
            self.restore_phase = RestorePhase::Finished;
            self.mark_capture_dirty();
        }
    }

    fn mark_capture_dirty(&mut self) {
        self.capture_dirty = true;
        self.capture_timestamp_pending = true;
        self.capture_deadline = None;
    }

    fn active_actionable(&self) -> bool {
        let Some(plan) = self.restore_plan.as_ref() else {
            return false;
        };
        if !self.restore_soft_expired && plan.instances.iter().any(|instance| !instance.attempted) {
            return true;
        }
        if !self.restore_soft_expired
            && plan.windows.iter().any(|window| {
                !window.placement_sent
                    && plan
                        .instance(window.stored.instance_id)
                        .and_then(|instance| instance.pid)
                        .is_some_and(|pid| {
                            self.live.values().any(|state| {
                                state.owner_pid == pid && state.ordinal == window.stored.ordinal
                            })
                        })
            })
        {
            return true;
        }
        let mut placed = plan
            .windows
            .iter()
            .filter(|window| window.placement_sent)
            .collect::<Vec<_>>();
        placed.sort_by_key(|window| window.stored.z_rank);
        if placed
            .iter()
            .enumerate()
            .any(|(rank, window)| window.sent_rank != Some(rank as u32))
        {
            return true;
        }
        let all_available_settled = plan.windows.iter().all(|window| {
            let Some(instance) = plan.instance(window.stored.instance_id) else {
                return true;
            };
            if instance.failed {
                return true;
            }
            let Some(window_id) = window.live_window_id else {
                return false;
            };
            window.placement_sent
                && self.live.get(&window_id).is_some_and(|state| {
                    state.flags & shell_window_state_flags::MAPPED != 0
                        && state.flags & shell_window_state_flags::RESTORE_PLACEMENT_APPLIED != 0
                })
        });
        let all_placed_applied = plan.windows.iter().all(|window| {
            if !window.placement_sent {
                return true;
            }
            window.live_window_id.is_some_and(|window_id| {
                self.live.get(&window_id).is_some_and(|state| {
                    state.flags & shell_window_state_flags::MAPPED != 0
                        && state.flags & shell_window_state_flags::RESTORE_PLACEMENT_APPLIED != 0
                })
            })
        });
        let all_attempted = plan.instances.iter().all(|instance| instance.attempted);
        all_attempted && (all_available_settled || self.restore_soft_expired && all_placed_applied)
    }

    fn build_snapshot(&self) -> Option<StoredSession> {
        let (output_width, output_height) = self.output_size;
        if output_width == 0 || output_height == 0 {
            return None;
        }
        let mut states = self
            .live
            .values()
            .filter(|state| !is_shell_owned_state(state))
            .filter(|state| state.flags & shell_window_state_flags::MAPPED != 0)
            .filter(|state| {
                self.identities
                    .get(&state.owner_pid)
                    .is_some_and(Option::is_some)
            })
            .cloned()
            .collect::<Vec<_>>();
        states.sort_by_key(|state| state.z_rank);
        if states.len() > MAX_SESSION_WINDOWS {
            states.drain(..states.len() - MAX_SESSION_WINDOWS);
        }

        let mut pids = states
            .iter()
            .map(|state| state.owner_pid)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        pids.truncate(MAX_SESSION_WINDOWS);
        let instance_ids = pids
            .iter()
            .enumerate()
            .map(|(index, pid)| (*pid, index as u32 + 1))
            .collect::<BTreeMap<_, _>>();
        let instances = pids
            .iter()
            .filter_map(|pid| {
                Some(StoredInstance {
                    id: *instance_ids.get(pid)?,
                    desktop_entry_id: self.identities.get(pid)?.as_ref()?.clone(),
                })
            })
            .collect::<Vec<_>>();

        let mut focused_window = None;
        let mut windows = Vec::new();
        for state in states {
            let Some(instance_id) = instance_ids.get(&state.owner_pid).copied() else {
                continue;
            };
            let (normal_x, normal_y, normal_width, normal_height) =
                if state.normal_width > 0 && state.normal_height > 0 {
                    (
                        state.normal_x,
                        state.normal_y,
                        state.normal_width,
                        state.normal_height,
                    )
                } else if state.current_width > 0 && state.current_height > 0 {
                    (
                        state.current_x,
                        state.current_y,
                        state.current_width,
                        state.current_height,
                    )
                } else {
                    continue;
                };
            let mut flags = 0;
            if state.flags & shell_window_state_flags::MINIMIZED != 0 {
                flags |= SESSION_FLAG_MINIMIZED;
            }
            if state.flags & shell_window_state_flags::MAXIMIZED != 0 {
                flags |= SESSION_FLAG_MAXIMIZED;
            }
            let id = windows.len() as u32 + 1;
            if self.focused_window == Some(state.window_id) && flags & SESSION_FLAG_MINIMIZED == 0 {
                focused_window = Some(id);
            }
            windows.push(StoredWindow {
                id,
                instance_id,
                ordinal: state.ordinal,
                z_rank: windows.len() as u32,
                normal_x,
                normal_y,
                normal_width,
                normal_height,
                flags,
            });
        }

        let represented = windows
            .iter()
            .map(|window| window.instance_id)
            .collect::<BTreeSet<_>>();
        let instances = instances
            .into_iter()
            .filter(|instance| represented.contains(&instance.id))
            .collect();
        Some(StoredSession {
            output_width,
            output_height,
            focused_window,
            instances,
            windows,
        })
    }
}

fn restore_status_name(status: u32) -> &'static str {
    match status {
        shell_restore_status::COMPLETED => "completed",
        shell_restore_status::ABORTED => "aborted",
        shell_restore_status::TIMED_OUT => "timed-out",
        shell_restore_status::BUSY => "busy",
        _ => "unknown",
    }
}

fn valid_catalog_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_IDENTIFIER_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[derive(Default)]
    struct NoopFilesystem;

    impl SessionFilesystem for NoopFilesystem {
        fn open_read(&mut self, _path: &Path) -> io::Result<Box<dyn SessionFile>> {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }

        fn create_new(&mut self, _path: &Path) -> io::Result<Box<dyn SessionFile>> {
            Err(io::Error::other("unused"))
        }

        fn open_sync(&mut self, _path: &Path) -> io::Result<Box<dyn SessionFile>> {
            Err(io::Error::other("unused"))
        }

        fn close(&mut self, _file: Box<dyn SessionFile>) -> io::Result<()> {
            Ok(())
        }

        fn create_dir(&mut self, _path: &Path) -> io::Result<()> {
            Ok(())
        }

        fn rename(&mut self, _from: &Path, _to: &Path) -> io::Result<()> {
            Ok(())
        }

        fn remove_file(&mut self, _path: &Path) -> io::Result<()> {
            Ok(())
        }
    }

    fn stored_window(id: u32, instance_id: u32, z_rank: u32) -> StoredWindow {
        StoredWindow {
            id,
            instance_id,
            ordinal: 1,
            z_rank,
            normal_x: 10 * id as i32,
            normal_y: 10 * id as i32,
            normal_width: 640,
            normal_height: 480,
            flags: 0,
        }
    }

    fn restore_instance(id: u32, pid: Option<u32>, failed: bool) -> RestoreInstance {
        RestoreInstance {
            stored_id: id,
            desktop_entry_id: format!("app-{id}"),
            exec: format!("/bin/app-{id}"),
            pid,
            attempted: true,
            failed,
        }
    }

    fn restore_window(stored: StoredWindow) -> RestoreWindow {
        RestoreWindow {
            stored,
            live_window_id: None,
            placement_sent: false,
            sent_rank: None,
        }
    }

    fn live(window_id: u32, pid: u32, flags: u32) -> ShellWindowState {
        ShellWindowState {
            snapshot_id: SESSION_SNAPSHOT_ID,
            window_id,
            owner_pid: pid,
            ordinal: 1,
            current_x: 0,
            current_y: 0,
            current_width: 640,
            current_height: 480,
            normal_x: 0,
            normal_y: 0,
            normal_width: 640,
            normal_height: 480,
            flags,
            z_rank: 0,
            title: String::new(),
            app_id: String::new(),
        }
    }

    fn active_runtime(plan: RestorePlan) -> SessionRuntime {
        let mut runtime = SessionRuntime::with_filesystem("/session", Box::new(NoopFilesystem));
        runtime.load_complete = true;
        runtime.catalog_ready = true;
        runtime.snapshot_done = true;
        runtime.restore_phase = RestorePhase::Active;
        runtime.restore_plan = Some(plan);
        runtime.restore_deadline = Some(SESSION_RESTORE_SOFT_DEADLINE);
        runtime
    }

    #[test]
    fn restore_deadline_policy_is_bounded_with_a_causal_settlement_tail() {
        let hard_deadline = Duration::from_millis(u64::from(MAX_SHELL_RESTORE_TIMEOUT_MS));

        assert_eq!(SESSION_RESTORE_SOFT_DEADLINE, Duration::from_millis(2_250));
        assert_eq!(hard_deadline, Duration::from_millis(2_500));
        assert_eq!(
            hard_deadline - SESSION_RESTORE_SOFT_DEADLINE,
            Duration::from_millis(250)
        );
        assert!(hard_deadline < Duration::from_secs(3));
    }

    #[test]
    fn proc_name_is_exact_and_unambiguous() {
        assert_eq!(
            parse_proc_name(b"Name:\t/bin/term\nState:\tR\n"),
            Some("/bin/term".into())
        );
        assert_eq!(parse_proc_name(b"Name: /bin/term\n"), None);
        assert_eq!(parse_proc_name(b"Name:\ta\nName:\tb\n"), None);
    }

    #[test]
    fn restore_plan_drops_missing_and_ambiguous_catalog_entries() {
        let stored = StoredSession {
            output_width: 10,
            output_height: 10,
            focused_window: None,
            instances: vec![
                StoredInstance {
                    id: 1,
                    desktop_entry_id: "term".into(),
                },
                StoredInstance {
                    id: 2,
                    desktop_entry_id: "missing".into(),
                },
            ],
            windows: vec![
                StoredWindow {
                    id: 1,
                    instance_id: 1,
                    ordinal: 1,
                    z_rank: 0,
                    normal_x: 0,
                    normal_y: 0,
                    normal_width: 1,
                    normal_height: 1,
                    flags: 0,
                },
                StoredWindow {
                    id: 2,
                    instance_id: 2,
                    ordinal: 1,
                    z_rank: 1,
                    normal_x: 0,
                    normal_y: 0,
                    normal_width: 1,
                    normal_height: 1,
                    flags: 0,
                },
            ],
        };
        let catalog = vec![CatalogIdentity {
            id: "term".into(),
            exec: "/bin/term".into(),
        }];
        let plan = RestorePlan::from_stored(&stored, &catalog).unwrap();
        assert_eq!(plan.instances.len(), 1);
        assert_eq!(plan.windows.len(), 1);
        assert_eq!(plan.instances[0].exec, "/bin/term");
    }

    #[test]
    fn loader_completion_is_resampled_before_begin_deadline_is_anchored() {
        struct BytesFile(std::io::Cursor<Vec<u8>>);

        impl SessionFile for BytesFile {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                std::io::Read::read(&mut self.0, buffer)
            }

            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("unused"))
            }

            fn sync_all(&mut self) -> io::Result<()> {
                Err(io::Error::other("unused"))
            }

            fn wait_fd(&self) -> Option<i32> {
                None
            }
        }

        struct LoaderFilesystem(Option<Vec<u8>>);

        impl SessionFilesystem for LoaderFilesystem {
            fn open_read(&mut self, _path: &Path) -> io::Result<Box<dyn SessionFile>> {
                let bytes = self.0.take().ok_or_else(|| io::Error::other("reopened"))?;
                Ok(Box::new(BytesFile(std::io::Cursor::new(bytes))))
            }

            fn create_new(&mut self, _path: &Path) -> io::Result<Box<dyn SessionFile>> {
                Err(io::Error::other("unused"))
            }

            fn open_sync(&mut self, _path: &Path) -> io::Result<Box<dyn SessionFile>> {
                Err(io::Error::other("unused"))
            }

            fn close(&mut self, _file: Box<dyn SessionFile>) -> io::Result<()> {
                Ok(())
            }

            fn create_dir(&mut self, _path: &Path) -> io::Result<()> {
                Err(io::Error::other("unused"))
            }

            fn rename(&mut self, _from: &Path, _to: &Path) -> io::Result<()> {
                Err(io::Error::other("unused"))
            }

            fn remove_file(&mut self, _path: &Path) -> io::Result<()> {
                Err(io::Error::other("unused"))
            }
        }

        let stored = StoredSession {
            output_width: 800,
            output_height: 600,
            focused_window: Some(1),
            instances: vec![StoredInstance {
                id: 1,
                desktop_entry_id: "term".into(),
            }],
            windows: vec![stored_window(1, 1, 0)],
        };
        let bytes = stored.serialize().unwrap().into_bytes();
        let mut runtime =
            SessionRuntime::with_filesystem("/session", Box::new(LoaderFilesystem(Some(bytes))));
        runtime.catalog_ready = true;
        runtime.catalog = vec![CatalogIdentity {
            id: "term".into(),
            exec: "/bin/term".into(),
        }];
        runtime.snapshot_done = true;

        assert!(!runtime.needs_clock_sample());
        while !runtime.load_complete {
            runtime.step_io();
        }
        assert!(
            runtime.needs_clock_sample(),
            "the loop must resample after the loader's completion turn",
        );
        let begin_at = Duration::from_millis(123);
        runtime.set_now(begin_at);
        assert!(matches!(
            runtime.next_action(begin_at),
            Some(SessionAction::BeginRestore { .. })
        ));
        assert_eq!(
            runtime.next_deadline(begin_at),
            Some(SESSION_RESTORE_SOFT_DEADLINE),
        );
    }

    #[test]
    fn restore_decision_and_readiness_do_not_wait_for_background_identities() {
        fn restorable() -> StoredSession {
            StoredSession {
                output_width: 800,
                output_height: 600,
                focused_window: Some(1),
                instances: vec![StoredInstance {
                    id: 1,
                    desktop_entry_id: "term".into(),
                }],
                windows: vec![stored_window(1, 1, 0)],
            }
        }

        fn prepared(loaded: Option<StoredSession>) -> SessionRuntime {
            let mut runtime =
                SessionRuntime::with_filesystem_for_pid("/session", Box::new(NoopFilesystem), 7);
            runtime.load_complete = true;
            runtime.loaded = loaded;
            runtime.catalog_ready = true;
            runtime.catalog = vec![CatalogIdentity {
                id: "term".into(),
                exec: "/bin/term".into(),
            }];
            runtime.snapshot_done = true;
            runtime.observe_window_state(live(
                90,
                7,
                shell_window_state_flags::MAPPED | shell_window_state_flags::SHELL_OWNED,
            ));
            runtime.observe_window_state(live(
                89,
                8,
                shell_window_state_flags::MAPPED | shell_window_state_flags::SHELL_OWNED,
            ));
            runtime.observe_window_state(live(91, 99, 0));
            runtime
        }

        let mut runtime = prepared(Some(restorable()));
        assert!(!runtime.live.contains_key(&89));
        assert!(!runtime.live.contains_key(&90));
        assert!(runtime.identities.is_empty());
        assert!(matches!(
            runtime.next_action(Duration::from_millis(25)),
            Some(SessionAction::BeginRestore { .. })
        ));
        runtime.queue_unresolved_identities();
        assert!(
            runtime.identity_queue.is_empty(),
            "capture identity reads stay out of the restore transaction",
        );

        let mut empty = prepared(None);
        assert_eq!(empty.next_action(Duration::from_millis(25)), None);
        assert!(empty.ready_for_desktop());
        assert!(empty.identities.is_empty());

        let mut replacement = prepared(Some(restorable()));
        let mut spoof = live(92, 42, shell_window_state_flags::MAPPED);
        spoof.app_id = "pmos.shell".into();
        replacement.observe_window_state(spoof);
        assert_eq!(replacement.next_action(Duration::from_millis(25)), None);
        assert!(replacement.ready_for_desktop());
        assert_eq!(replacement.restore_phase, RestorePhase::Finished);
        assert!(
            replacement.restore_plan.is_none(),
            "a mapped non-self window suppresses duplicate replacement-shell spawning without procfs",
        );

        replacement.queue_unresolved_identities();
        assert!(!replacement.identity_queue.is_empty());
        replacement.proc_reader = Some(ProcReader::new(42));
        assert!(
            replacement.ready_for_desktop(),
            "background procfs identity work cannot revoke readiness",
        );
    }

    #[test]
    fn shell_owned_state_evicts_transient_live_capture_and_identity_state() {
        let mut runtime =
            SessionRuntime::with_filesystem_for_pid("/session", Box::new(NoopFilesystem), 7);
        runtime.output_size = (800, 600);
        runtime.restore_phase = RestorePhase::Finished;
        runtime.observe_window_state(live(
            90,
            8,
            shell_window_state_flags::MAPPED | shell_window_state_flags::FOCUSED,
        ));
        runtime.identities.insert(8, Some("shell-spoof".into()));
        assert_eq!(runtime.focused_window, Some(90));
        assert_eq!(runtime.build_snapshot().unwrap().windows.len(), 1);

        runtime.observe_window_state(live(
            90,
            8,
            shell_window_state_flags::MAPPED
                | shell_window_state_flags::FOCUSED
                | shell_window_state_flags::SHELL_OWNED,
        ));

        assert!(!runtime.live.contains_key(&90));
        assert_eq!(runtime.focused_window, None);
        assert!(!runtime.identities.contains_key(&8));
        assert!(runtime.build_snapshot().unwrap().windows.is_empty());
    }

    #[test]
    fn begin_barrier_precedes_spawn_and_unsettled_place_parks_without_spin() {
        let plan = RestorePlan {
            instances: vec![RestoreInstance {
                attempted: false,
                ..restore_instance(1, None, false)
            }],
            windows: vec![restore_window(stored_window(1, 1, 0))],
            focused_record: Some(1),
        };
        let mut runtime = active_runtime(plan);
        runtime.restore_phase = RestorePhase::NeedBegin;

        assert_eq!(
            runtime.next_action(Duration::ZERO),
            Some(SessionAction::BeginRestore {
                restore_id: SESSION_RESTORE_ID,
                timeout_ms: MAX_SHELL_RESTORE_TIMEOUT_MS,
            })
        );
        assert_eq!(runtime.next_action(Duration::ZERO), None);
        runtime.barrier_sent(77);
        assert!(!runtime.observe_callback(76));
        assert_eq!(runtime.next_action(Duration::ZERO), None);
        assert!(runtime.observe_callback(77));
        assert_eq!(
            runtime.next_action(Duration::ZERO),
            Some(SessionAction::Spawn {
                instance_id: 1,
                exec: "/bin/app-1".into(),
            })
        );
        runtime.spawn_result(1, 42);
        runtime.set_now(Duration::from_millis(10));
        assert!(!runtime.has_local_work());
        assert_eq!(
            runtime.next_deadline(Duration::from_millis(10)),
            Some(SESSION_RESTORE_SOFT_DEADLINE - Duration::from_millis(10))
        );

        runtime.observe_window_state(live(9, 42, shell_window_state_flags::MAPPED));
        assert!(runtime.has_local_work());
        assert!(matches!(
            runtime.next_action(Duration::from_millis(20)),
            Some(SessionAction::Place {
                window_id: 9,
                z_rank: 0,
                ..
            })
        ));
        assert!(!runtime.has_local_work());

        // The soft boundary removes its own timer but neither spins nor sends
        // an End that the server would reject while placement is unsettled.
        assert_eq!(runtime.next_action(SESSION_RESTORE_SOFT_DEADLINE), None);
        runtime.set_now(SESSION_RESTORE_SOFT_DEADLINE);
        assert!(!runtime.has_local_work());
        assert_eq!(runtime.next_deadline(SESSION_RESTORE_SOFT_DEADLINE), None);

        runtime.observe_window_state(live(
            9,
            42,
            shell_window_state_flags::MAPPED | shell_window_state_flags::RESTORE_PLACEMENT_APPLIED,
        ));
        assert!(runtime.has_local_work());
        assert_eq!(
            runtime.next_action(SESSION_RESTORE_SOFT_DEADLINE + Duration::from_millis(10)),
            Some(SessionAction::EndRestore {
                restore_id: SESSION_RESTORE_ID,
                focus_window_id: 9,
            })
        );
        assert_eq!(
            runtime.next_action(SESSION_RESTORE_SOFT_DEADLINE + Duration::from_millis(10)),
            None,
        );
    }

    #[test]
    fn soft_deadline_during_barrier_wait_is_consumed_and_late_callback_never_spawns() {
        let plan = RestorePlan {
            instances: vec![RestoreInstance {
                attempted: false,
                ..restore_instance(1, None, false)
            }],
            windows: vec![restore_window(stored_window(1, 1, 0))],
            focused_record: Some(1),
        };
        let mut runtime = active_runtime(plan);
        runtime.restore_phase = RestorePhase::NeedBegin;
        assert!(matches!(
            runtime.next_action(Duration::ZERO),
            Some(SessionAction::BeginRestore { .. })
        ));
        runtime.barrier_sent(77);

        assert_eq!(
            runtime.next_deadline(SESSION_RESTORE_SOFT_DEADLINE - Duration::from_millis(1)),
            Some(Duration::from_millis(1)),
        );
        assert_eq!(runtime.next_action(SESSION_RESTORE_SOFT_DEADLINE), None);
        assert!(runtime.restore_soft_expired);
        assert_eq!(runtime.restore_deadline, None);
        runtime.set_now(SESSION_RESTORE_SOFT_DEADLINE);
        assert!(!runtime.has_local_work());
        assert_eq!(runtime.next_deadline(SESSION_RESTORE_SOFT_DEADLINE), None);
        assert!(!runtime.needs_clock_sample());

        assert!(runtime.observe_callback(77));
        assert_eq!(runtime.restore_phase, RestorePhase::Active);
        assert!(!runtime.has_local_work());
        assert_eq!(
            runtime.next_action(SESSION_RESTORE_SOFT_DEADLINE + Duration::from_millis(10)),
            None,
        );
        assert!(
            !runtime.restore_plan.as_ref().unwrap().instances[0].attempted,
            "a callback after the shell soft deadline must not start a child",
        );
        assert_eq!(
            runtime.next_deadline(SESSION_RESTORE_SOFT_DEADLINE + Duration::from_millis(10)),
            None,
        );

        let plan = RestorePlan {
            instances: vec![RestoreInstance {
                attempted: false,
                ..restore_instance(1, None, false)
            }],
            windows: vec![restore_window(stored_window(1, 1, 0))],
            focused_record: None,
        };
        let mut runtime = active_runtime(plan);
        runtime.restore_phase = RestorePhase::NeedBegin;
        assert!(runtime.next_action(Duration::ZERO).is_some());
        assert_eq!(runtime.restore_phase, RestorePhase::BeginSent);
        assert_eq!(runtime.next_action(SESSION_RESTORE_SOFT_DEADLINE), None);
        assert_eq!(runtime.restore_deadline, None);
        assert!(runtime.restore_soft_expired);
    }

    #[test]
    fn failed_bottom_and_missing_middle_compact_to_contiguous_ranks() {
        let plan = RestorePlan {
            instances: vec![
                restore_instance(1, None, true),
                restore_instance(2, Some(22), false),
            ],
            windows: vec![
                restore_window(stored_window(1, 1, 0)),
                restore_window(stored_window(2, 2, 1)),
            ],
            focused_record: Some(2),
        };
        let mut runtime = active_runtime(plan);
        runtime.observe_window_state(live(
            20,
            22,
            shell_window_state_flags::MAPPED | shell_window_state_flags::RESTORE_PLACEMENT_APPLIED,
        ));
        assert!(matches!(
            runtime.next_action(Duration::ZERO),
            Some(SessionAction::Place { z_rank: 0, .. })
        ));
        assert!(matches!(
            runtime.next_action(Duration::ZERO),
            Some(SessionAction::EndRestore {
                focus_window_id: 20,
                ..
            })
        ));

        let plan = RestorePlan {
            instances: vec![
                restore_instance(1, Some(11), false),
                restore_instance(2, Some(22), false),
                restore_instance(3, Some(33), false),
            ],
            windows: vec![
                restore_window(stored_window(1, 1, 0)),
                restore_window(stored_window(2, 2, 1)),
                restore_window(stored_window(3, 3, 2)),
            ],
            focused_record: None,
        };
        let mut runtime = active_runtime(plan);
        runtime.observe_window_state(live(
            10,
            11,
            shell_window_state_flags::MAPPED | shell_window_state_flags::RESTORE_PLACEMENT_APPLIED,
        ));
        runtime.observe_window_state(live(
            30,
            33,
            shell_window_state_flags::MAPPED | shell_window_state_flags::RESTORE_PLACEMENT_APPLIED,
        ));
        assert!(matches!(
            runtime.next_action(Duration::ZERO),
            Some(SessionAction::Place {
                window_id: 10,
                z_rank: 0,
                ..
            })
        ));
        assert!(matches!(
            runtime.next_action(Duration::ZERO),
            Some(SessionAction::Place {
                window_id: 30,
                z_rank: 1,
                ..
            })
        ));
        assert!(matches!(
            runtime.next_action(SESSION_RESTORE_SOFT_DEADLINE),
            Some(SessionAction::EndRestore { .. })
        ));
    }

    #[test]
    fn authoritative_state_captures_focus_and_complete_current_rect_fallback() {
        let mut runtime = SessionRuntime::with_filesystem("/session", Box::new(NoopFilesystem));
        runtime.load_complete = true;
        runtime.catalog_ready = true;
        runtime.snapshot_done = true;
        runtime.restore_phase = RestorePhase::Finished;
        runtime.output_size = (800, 600);
        runtime.identities.insert(42, Some("term".into()));
        let mut state = live(
            9,
            42,
            shell_window_state_flags::MAPPED | shell_window_state_flags::FOCUSED,
        );
        state.current_x = 31;
        state.current_y = 47;
        state.current_width = 700;
        state.current_height = 500;
        state.normal_x = -999;
        state.normal_y = -999;
        state.normal_width = 0;
        state.normal_height = 0;
        runtime.observe_window_state(state);

        let snapshot = runtime.build_snapshot().unwrap();
        assert_eq!(snapshot.focused_window, Some(1));
        assert_eq!(snapshot.instances[0].desktop_entry_id, "term");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(
            (
                snapshot.windows[0].normal_x,
                snapshot.windows[0].normal_y,
                snapshot.windows[0].normal_width,
                snapshot.windows[0].normal_height,
            ),
            (31, 47, 700, 500),
        );
    }

    #[test]
    fn dirty_capture_coalesces_without_blocking_ready_or_clean_idle() {
        let mut runtime = SessionRuntime::with_filesystem("/session", Box::new(NoopFilesystem));
        runtime.load_complete = true;
        runtime.catalog_ready = true;
        runtime.snapshot_done = true;
        runtime.restore_phase = RestorePhase::Finished;
        assert!(runtime.ready_for_desktop());
        assert!(!runtime.needs_clock_sample());
        assert_eq!(runtime.next_deadline(Duration::ZERO), None);
        assert!(!runtime.has_local_work());

        runtime.mark_capture_dirty();
        assert!(runtime.capture_timestamp_pending);
        assert!(
            runtime.has_local_work(),
            "a first-dirty timestamp gets exactly one local continuation",
        );
        runtime.set_now(Duration::from_millis(10));
        assert!(runtime.ready_for_desktop());
        assert!(runtime.needs_clock_sample());
        assert!(!runtime.has_local_work());
        assert_eq!(
            runtime.next_deadline(Duration::from_millis(10)),
            Some(SESSION_CAPTURE_COALESCE)
        );
        assert!(!runtime.has_local_work());
        runtime.mark_capture_dirty();
        runtime.set_now(Duration::from_millis(100));
        assert_eq!(
            runtime.next_deadline(Duration::from_millis(100)),
            Some(SESSION_CAPTURE_COALESCE)
        );

        runtime.set_now(Duration::from_secs(9));
        runtime.mark_capture_dirty();
        assert_eq!(runtime.next_deadline(Duration::from_secs(9)), None);
        runtime.set_now(Duration::from_secs(10));
        assert_eq!(
            runtime.next_deadline(Duration::from_secs(10)),
            Some(SESSION_CAPTURE_COALESCE),
            "a dirty transition starts from its fresh sample, not the prior clock value",
        );
    }

    #[test]
    fn capture_expiry_before_first_output_parks_and_output_change_rearms_once() {
        let mut runtime = SessionRuntime::with_filesystem("/session", Box::new(NoopFilesystem));
        runtime.load_complete = true;
        runtime.catalog_ready = true;
        runtime.snapshot_done = true;
        runtime.restore_phase = RestorePhase::Finished;
        runtime.mark_capture_dirty();
        runtime.set_now(Duration::ZERO);

        runtime.set_now(SESSION_CAPTURE_COALESCE);
        assert!(!runtime.step_io());
        assert!(!runtime.capture_dirty);
        assert!(!runtime.capture_timestamp_pending);
        assert_eq!(runtime.next_deadline(SESSION_CAPTURE_COALESCE), None);
        assert!(!runtime.has_local_work());
        assert!(!runtime.needs_clock_sample());

        runtime.set_output_size(800, 600);
        assert!(runtime.capture_timestamp_pending);
        assert!(runtime.has_local_work());
        runtime.set_now(Duration::from_millis(400));
        assert!(!runtime.has_local_work());
        assert_eq!(
            runtime.next_deadline(Duration::from_millis(400)),
            Some(SESSION_CAPTURE_COALESCE),
            "the first valid output schedules a fresh one-shot capture",
        );
    }

    #[test]
    fn identity_completion_after_partial_capture_schedules_one_corrective_snapshot() {
        let mut runtime = SessionRuntime::with_filesystem("/session", Box::new(NoopFilesystem));
        runtime.load_complete = true;
        runtime.catalog_ready = true;
        runtime.snapshot_done = true;
        runtime.output_size = (800, 600);
        runtime.observe_window_state(live(9, 42, shell_window_state_flags::MAPPED));
        runtime.restore_phase = RestorePhase::Finished;
        runtime.identities.insert(42, None);
        runtime.mark_capture_dirty();
        runtime.set_now(Duration::ZERO);

        runtime.set_now(SESSION_CAPTURE_COALESCE);
        runtime.step_io();
        assert!(!runtime.capture_dirty);
        assert!(runtime.build_snapshot().unwrap().windows.is_empty());

        runtime.record_identity(42, Some("term".into()));
        assert!(runtime.capture_timestamp_pending);
        runtime.set_now(Duration::from_millis(400));
        let corrected = runtime.build_snapshot().unwrap();
        assert_eq!(corrected.instances.len(), 1);
        assert_eq!(corrected.windows.len(), 1);
        assert_eq!(
            runtime.next_deadline(Duration::from_millis(400)),
            Some(SESSION_CAPTURE_COALESCE),
        );

        runtime.set_now(Duration::from_millis(450));
        runtime.record_identity(42, Some("term".into()));
        assert_eq!(
            runtime.next_deadline(Duration::from_millis(450)),
            Some(Duration::from_millis(200)),
            "an unchanged identity must not keep resetting the coalescing window",
        );
    }

    #[test]
    fn identity_state_is_bounded_by_live_window_owners() {
        let mut runtime = SessionRuntime::with_filesystem("/session", Box::new(NoopFilesystem));
        runtime.load_complete = true;
        runtime.catalog_ready = true;
        runtime.snapshot_done = true;
        runtime.restore_phase = RestorePhase::Finished;

        for offset in 0..128u32 {
            let pid = 1_000 + offset;
            let window_id = 2_000 + offset;
            runtime.observe_window_state(live(window_id, pid, shell_window_state_flags::MAPPED));
            runtime.record_identity(pid, Some("term".into()));
            runtime.identity_queue.push_back(pid);
            assert!(runtime.identities.contains_key(&pid));
            runtime.observe_window_destroyed(window_id);
            assert!(runtime.identities.is_empty());
            assert!(runtime.identity_queue.is_empty());
        }

        runtime.observe_window_state(live(9, 42, shell_window_state_flags::MAPPED));
        runtime.proc_reader = Some(ProcReader::new(42));
        runtime.observe_window_destroyed(9);
        assert_eq!(
            runtime.proc_reader.as_ref().map(|reader| reader.pid),
            Some(42)
        );
        runtime.record_identity(42, Some("term".into()));
        assert!(
            !runtime.identities.contains_key(&42),
            "a completing in-flight read cannot reinsert an exited owner",
        );

        runtime.identities.insert(77, Some("term".into()));
        runtime.prune_identities_without_live_windows();
        assert!(runtime.identities.is_empty());
    }

    #[test]
    fn catalog_reload_keeps_in_flight_proc_file_until_explicit_close() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct DropFile(Rc<Cell<usize>>);

        impl Drop for DropFile {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        impl SessionFile for DropFile {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }

            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("unused"))
            }

            fn sync_all(&mut self) -> io::Result<()> {
                Err(io::Error::other("unused"))
            }

            fn wait_fd(&self) -> Option<i32> {
                None
            }
        }

        let drops = Rc::new(Cell::new(0));
        let mut runtime = SessionRuntime::with_filesystem("/session", Box::new(NoopFilesystem));
        runtime.load_complete = true;
        runtime.catalog_ready = true;
        runtime.snapshot_done = true;
        runtime.restore_phase = RestorePhase::Finished;
        runtime.observe_window_state(live(9, 42, shell_window_state_flags::MAPPED));
        runtime.proc_reader = Some(ProcReader {
            pid: 42,
            path: PathBuf::from("/proc/42/status"),
            state: ProcReaderState::Read {
                file: Box::new(DropFile(drops.clone())),
                bytes: Vec::new(),
                invalid: false,
                blocked: false,
            },
        });

        runtime.set_catalog(&[DesktopEntry {
            id: "term".into(),
            name: "Terminal".into(),
            exec: "/bin/term".into(),
            icon: None,
            summary: None,
            mime_types: Vec::new(),
            categories: Vec::new(),
            caps: Vec::new(),
        }]);
        assert_eq!(drops.get(), 0);
        assert!(runtime.proc_reader.is_some());

        runtime.step_io();
        assert_eq!(drops.get(), 0, "EOF only transitions to explicit Close");
        runtime.step_io();
        assert_eq!(drops.get(), 1, "the later filesystem close owns the drop");
        assert!(runtime.proc_reader.is_none());
    }

    #[test]
    fn blocked_identity_read_consumes_overdue_capture_once_without_spin() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct BlockedFile {
            ready: Rc<Cell<bool>>,
            bytes: Vec<u8>,
            offset: usize,
        }

        impl SessionFile for BlockedFile {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if !self.ready.get() {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                let remaining = &self.bytes[self.offset..];
                let read = remaining.len().min(buffer.len());
                buffer[..read].copy_from_slice(&remaining[..read]);
                self.offset += read;
                Ok(read)
            }

            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("unused"))
            }

            fn sync_all(&mut self) -> io::Result<()> {
                Err(io::Error::other("unused"))
            }

            fn wait_fd(&self) -> Option<i32> {
                Some(55)
            }
        }

        let mut runtime = SessionRuntime::with_filesystem("/session", Box::new(NoopFilesystem));
        runtime.load_complete = true;
        runtime.catalog_ready = true;
        runtime.snapshot_done = true;
        runtime.restore_phase = RestorePhase::Finished;
        runtime.output_size = (800, 600);
        runtime
            .live
            .insert(9, live(9, 42, shell_window_state_flags::MAPPED));
        runtime.catalog = vec![CatalogIdentity {
            id: "term".into(),
            exec: "/bin/term".into(),
        }];
        runtime.capture_dirty = true;
        runtime.capture_deadline = Some(SESSION_CAPTURE_COALESCE);
        runtime.now = Duration::from_millis(300);
        let ready = Rc::new(Cell::new(false));
        runtime.proc_reader = Some(ProcReader {
            pid: 42,
            path: PathBuf::from("/proc/42/status"),
            state: ProcReaderState::Read {
                file: Box::new(BlockedFile {
                    ready: ready.clone(),
                    bytes: b"Name:\t/bin/term\n".to_vec(),
                    offset: 0,
                }),
                bytes: Vec::new(),
                invalid: false,
                blocked: true,
            },
        });

        assert!(runtime.has_local_work());
        assert!(runtime.needs_clock_sample());
        assert_eq!(
            runtime.next_deadline(Duration::from_millis(300)),
            Some(Duration::ZERO),
        );
        assert_eq!(runtime.wait_fds(), vec![SessionWait::Read(55)]);

        assert!(!runtime.step_io());
        assert!(!runtime.capture_dirty);
        assert!(!runtime.has_local_work());
        assert!(!runtime.needs_clock_sample());
        assert_eq!(runtime.next_deadline(Duration::from_millis(300)), None);
        assert_eq!(runtime.wait_fds(), vec![SessionWait::Read(55)]);

        ready.set(true);
        assert!(runtime.step_io());
        assert!(runtime.step_io());
        assert!(runtime.step_io());
        assert!(runtime.proc_reader.is_none());
        assert_eq!(runtime.identities.get(&42), Some(&Some("term".into())));
        assert!(runtime.capture_timestamp_pending);
        runtime.set_now(Duration::from_millis(400));
        assert_eq!(runtime.build_snapshot().unwrap().windows.len(), 1);
        assert_eq!(
            runtime.next_deadline(Duration::from_millis(400)),
            Some(SESSION_CAPTURE_COALESCE),
        );
    }
}
