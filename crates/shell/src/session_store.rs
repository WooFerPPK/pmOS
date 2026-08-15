//! Bounded durable desktop-session schema.
//!
//! This module deliberately contains no display or filesystem code. The live
//! shell owns the incremental reader/writer and feeds complete snapshots into
//! this strict parser; keeping the format pure makes corruption and forward-
//! version behaviour independently testable.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub const SESSION_PATH: &str = "/home/user/.config/pmos/session-v1";
pub const MAX_SESSION_BYTES: usize = 64 * 1024;
pub const MAX_SESSION_INSTANCES: usize = 64;
pub const MAX_SESSION_WINDOWS: usize = 64;
pub const MAX_SESSION_IDENTIFIER_BYTES: usize = 64;
pub const SESSION_IO_CHUNK_BYTES: usize = 16 * 1024;
const MAX_TEMP_CREATE_ATTEMPTS: usize = 16;
pub const SESSION_FLAG_MINIMIZED: u32 = 1 << 0;
pub const SESSION_FLAG_MAXIMIZED: u32 = 1 << 1;
const SESSION_FLAGS: u32 = SESSION_FLAG_MINIMIZED | SESSION_FLAG_MAXIMIZED;
const HEADER: &str = "PMOS_SESSION_V1";
const FNV1A_64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_64_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 64-bit over the exact canonical session bytes.
///
/// The browser acceptance gate implements the same dependency-free digest to
/// bind a target-file read to the writer's post-sync durability event.
pub fn session_digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV1A_64_OFFSET_BASIS, |digest, byte| {
        (digest ^ u64::from(*byte)).wrapping_mul(FNV1A_64_PRIME)
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredInstance {
    pub id: u32,
    pub desktop_entry_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredWindow {
    pub id: u32,
    pub instance_id: u32,
    pub ordinal: u32,
    pub z_rank: u32,
    pub normal_x: i32,
    pub normal_y: i32,
    pub normal_width: u32,
    pub normal_height: u32,
    pub flags: u32,
}

impl StoredWindow {
    pub fn minimized(&self) -> bool {
        self.flags & SESSION_FLAG_MINIMIZED != 0
    }

    pub fn maximized(&self) -> bool {
        self.flags & SESSION_FLAG_MAXIMIZED != 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSession {
    pub output_width: u32,
    pub output_height: u32,
    pub focused_window: Option<u32>,
    pub instances: Vec<StoredInstance>,
    pub windows: Vec<StoredWindow>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionFormatError {
    TooLarge,
    InvalidUtf8,
    InvalidHeader,
    InvalidRecord,
    DuplicateRecord,
    MissingRecord,
    InvalidIdentifier,
    LimitExceeded,
    InvalidReference,
    InvalidGeometry,
    InvalidFlags,
    InvalidStacking,
    InvalidFocus,
}

impl StoredSession {
    pub fn empty(output_width: u32, output_height: u32) -> Result<Self, SessionFormatError> {
        let session = Self {
            output_width,
            output_height,
            focused_window: None,
            instances: Vec::new(),
            windows: Vec::new(),
        };
        session.validate()?;
        Ok(session)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, SessionFormatError> {
        if bytes.len() > MAX_SESSION_BYTES {
            return Err(SessionFormatError::TooLarge);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| SessionFormatError::InvalidUtf8)?;
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(SessionFormatError::InvalidHeader);
        }

        let mut output = None;
        let mut focus = None;
        let mut instances = Vec::new();
        let mut windows = Vec::new();
        for line in lines {
            if line.is_empty() {
                return Err(SessionFormatError::InvalidRecord);
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                ["output", width, height] => {
                    if output
                        .replace((parse_u32(width)?, parse_u32(height)?))
                        .is_some()
                    {
                        return Err(SessionFormatError::DuplicateRecord);
                    }
                }
                ["focus", id] => {
                    if focus.replace(parse_u32(id)?).is_some() {
                        return Err(SessionFormatError::DuplicateRecord);
                    }
                }
                ["instance", id, desktop_entry_id] => {
                    instances.push(StoredInstance {
                        id: parse_u32(id)?,
                        desktop_entry_id: (*desktop_entry_id).to_string(),
                    });
                    if instances.len() > MAX_SESSION_INSTANCES {
                        return Err(SessionFormatError::LimitExceeded);
                    }
                }
                ["window", id, instance_id, ordinal, z_rank, normal_x, normal_y, normal_width, normal_height, flags] =>
                {
                    windows.push(StoredWindow {
                        id: parse_u32(id)?,
                        instance_id: parse_u32(instance_id)?,
                        ordinal: parse_u32(ordinal)?,
                        z_rank: parse_u32(z_rank)?,
                        normal_x: parse_i32(normal_x)?,
                        normal_y: parse_i32(normal_y)?,
                        normal_width: parse_u32(normal_width)?,
                        normal_height: parse_u32(normal_height)?,
                        flags: parse_u32(flags)?,
                    });
                    if windows.len() > MAX_SESSION_WINDOWS {
                        return Err(SessionFormatError::LimitExceeded);
                    }
                }
                _ => return Err(SessionFormatError::InvalidRecord),
            }
        }

        let (output_width, output_height) = output.ok_or(SessionFormatError::MissingRecord)?;
        let focused_window = match focus.ok_or(SessionFormatError::MissingRecord)? {
            0 => None,
            id => Some(id),
        };
        let session = Self {
            output_width,
            output_height,
            focused_window,
            instances,
            windows,
        };
        session.validate()?;
        Ok(session)
    }

    pub fn serialize(&self) -> Result<String, SessionFormatError> {
        self.validate()?;
        let mut instances = self.instances.iter().collect::<Vec<_>>();
        instances.sort_by_key(|instance| instance.id);
        let mut windows = self.windows.iter().collect::<Vec<_>>();
        windows.sort_by_key(|window| window.z_rank);

        let mut text = String::new();
        text.push_str(HEADER);
        text.push('\n');
        text.push_str(&format!(
            "output {} {}\n",
            self.output_width, self.output_height
        ));
        text.push_str(&format!(
            "focus {}\n",
            self.focused_window.unwrap_or_default()
        ));
        for instance in instances {
            text.push_str(&format!(
                "instance {} {}\n",
                instance.id, instance.desktop_entry_id
            ));
        }
        for window in windows {
            text.push_str(&format!(
                "window {} {} {} {} {} {} {} {} {}\n",
                window.id,
                window.instance_id,
                window.ordinal,
                window.z_rank,
                window.normal_x,
                window.normal_y,
                window.normal_width,
                window.normal_height,
                window.flags
            ));
        }
        if text.len() > MAX_SESSION_BYTES {
            return Err(SessionFormatError::TooLarge);
        }
        Ok(text)
    }

    fn validate(&self) -> Result<(), SessionFormatError> {
        if self.output_width == 0 || self.output_height == 0 {
            return Err(SessionFormatError::InvalidGeometry);
        }
        if self.instances.len() > MAX_SESSION_INSTANCES || self.windows.len() > MAX_SESSION_WINDOWS
        {
            return Err(SessionFormatError::LimitExceeded);
        }

        let mut instance_ids = BTreeSet::new();
        let mut instance_window_counts = BTreeMap::<u32, usize>::new();
        for instance in &self.instances {
            if instance.id == 0 || !instance_ids.insert(instance.id) {
                return Err(SessionFormatError::DuplicateRecord);
            }
            if !valid_identifier(&instance.desktop_entry_id) {
                return Err(SessionFormatError::InvalidIdentifier);
            }
            instance_window_counts.insert(instance.id, 0);
        }

        let mut window_ids = BTreeSet::new();
        let mut z_ranks = BTreeSet::new();
        let mut ordinals = BTreeSet::new();
        for window in &self.windows {
            if window.id == 0 || !window_ids.insert(window.id) {
                return Err(SessionFormatError::DuplicateRecord);
            }
            let Some(count) = instance_window_counts.get_mut(&window.instance_id) else {
                return Err(SessionFormatError::InvalidReference);
            };
            *count += 1;
            if window.ordinal == 0 || !ordinals.insert((window.instance_id, window.ordinal)) {
                return Err(SessionFormatError::DuplicateRecord);
            }
            if window.normal_width == 0 || window.normal_height == 0 {
                return Err(SessionFormatError::InvalidGeometry);
            }
            if window.flags & !SESSION_FLAGS != 0 {
                return Err(SessionFormatError::InvalidFlags);
            }
            if !z_ranks.insert(window.z_rank) {
                return Err(SessionFormatError::InvalidStacking);
            }
        }
        if instance_window_counts.values().any(|count| *count == 0) {
            return Err(SessionFormatError::InvalidReference);
        }
        if z_ranks.iter().copied().ne(0..self.windows.len() as u32) {
            return Err(SessionFormatError::InvalidStacking);
        }
        if let Some(focused) = self.focused_window {
            let Some(window) = self.windows.iter().find(|window| window.id == focused) else {
                return Err(SessionFormatError::InvalidFocus);
            };
            if window.minimized() {
                return Err(SessionFormatError::InvalidFocus);
            }
        }
        Ok(())
    }
}

fn parse_u32(value: &str) -> Result<u32, SessionFormatError> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_| SessionFormatError::InvalidRecord)?;
    if parsed.to_string() != value {
        return Err(SessionFormatError::InvalidRecord);
    }
    Ok(parsed)
}

fn parse_i32(value: &str) -> Result<i32, SessionFormatError> {
    let parsed = value
        .parse::<i32>()
        .map_err(|_| SessionFormatError::InvalidRecord)?;
    if parsed.to_string() != value {
        return Err(SessionFormatError::InvalidRecord);
    }
    Ok(parsed)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_IDENTIFIER_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Open file used by the stepwise session loader and atomic writer.  Closing
/// is an explicit filesystem operation on [`SessionFilesystem`] so a shell
/// turn never hides a second syscall in an otherwise bounded step.
pub trait SessionFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize>;
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize>;
    fn sync_all(&mut self) -> io::Result<()>;
    fn wait_fd(&self) -> Option<i32>;
}

/// Small filesystem seam shared by production and failure-injection tests.
/// Every method represents exactly one filesystem operation.
pub trait SessionFilesystem {
    fn open_read(&mut self, path: &Path) -> io::Result<Box<dyn SessionFile>>;
    fn create_new(&mut self, path: &Path) -> io::Result<Box<dyn SessionFile>>;
    fn open_sync(&mut self, path: &Path) -> io::Result<Box<dyn SessionFile>>;
    fn close(&mut self, file: Box<dyn SessionFile>) -> io::Result<()>;
    fn create_dir(&mut self, path: &Path) -> io::Result<()>;
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&mut self, path: &Path) -> io::Result<()>;
}

struct StdSessionFile(File);

impl SessionFile for StdSessionFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        Read::read(&mut self.0, buffer)
    }

    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        Write::write(&mut self.0, buffer)
    }

    fn sync_all(&mut self) -> io::Result<()> {
        self.0.sync_all()
    }

    fn wait_fd(&self) -> Option<i32> {
        #[cfg(any(unix, target_os = "wasi"))]
        {
            use std::os::fd::AsRawFd;
            Some(self.0.as_raw_fd())
        }
        #[cfg(not(any(unix, target_os = "wasi")))]
        {
            None
        }
    }
}

/// Production filesystem implementation. All paths are PMos VFS paths in a
/// WASI build and ordinary host paths in native isolation tests.
#[derive(Default)]
pub struct StdSessionFilesystem;

impl SessionFilesystem for StdSessionFilesystem {
    fn open_read(&mut self, path: &Path) -> io::Result<Box<dyn SessionFile>> {
        File::open(path).map(|file| Box::new(StdSessionFile(file)) as Box<dyn SessionFile>)
    }

    fn create_new(&mut self, path: &Path) -> io::Result<Box<dyn SessionFile>> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map(|file| Box::new(StdSessionFile(file)) as Box<dyn SessionFile>)
    }

    fn open_sync(&mut self, path: &Path) -> io::Result<Box<dyn SessionFile>> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map(|file| Box::new(StdSessionFile(file)) as Box<dyn SessionFile>)
    }

    fn close(&mut self, file: Box<dyn SessionFile>) -> io::Result<()> {
        drop(file);
        Ok(())
    }

    fn create_dir(&mut self, path: &Path) -> io::Result<()> {
        std::fs::create_dir(path)
    }

    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SessionWait {
    Read(i32),
    Write(i32),
}

#[derive(Debug, PartialEq, Eq)]
pub enum SessionLoadStep {
    Pending,
    Complete(Option<StoredSession>),
}

enum LoaderState {
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
    Complete,
}

/// Incremental, fail-closed loader. A call to [`Self::step`] performs at most
/// one filesystem operation and reads no more than 16 KiB.
pub struct SessionLoader {
    path: PathBuf,
    state: LoaderState,
}

impl SessionLoader {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            state: LoaderState::Open,
        }
    }

    pub fn wait(&self) -> Option<SessionWait> {
        match &self.state {
            LoaderState::Read {
                file,
                blocked: true,
                ..
            } => file.wait_fd().map(SessionWait::Read),
            _ => None,
        }
    }

    pub fn complete(&self) -> bool {
        matches!(self.state, LoaderState::Complete)
    }

    pub fn step(&mut self, filesystem: &mut dyn SessionFilesystem) -> SessionLoadStep {
        let state = core::mem::replace(&mut self.state, LoaderState::Complete);
        match state {
            LoaderState::Open => match filesystem.open_read(&self.path) {
                Ok(file) => {
                    self.state = LoaderState::Read {
                        file,
                        bytes: Vec::new(),
                        invalid: false,
                        blocked: false,
                    };
                    SessionLoadStep::Pending
                }
                Err(error) => {
                    if error.kind() != io::ErrorKind::NotFound {
                        eprintln!("shell: session load failed: {error}");
                    }
                    SessionLoadStep::Complete(None)
                }
            },
            LoaderState::Read {
                mut file,
                mut bytes,
                invalid,
                ..
            } => {
                let mut chunk = [0u8; SESSION_IO_CHUNK_BYTES];
                match file.read(&mut chunk) {
                    Ok(0) => {
                        self.state = LoaderState::Close {
                            file,
                            bytes,
                            invalid,
                        };
                    }
                    Ok(read) => {
                        let remaining = MAX_SESSION_BYTES
                            .saturating_add(1)
                            .saturating_sub(bytes.len());
                        bytes.extend_from_slice(&chunk[..read.min(remaining)]);
                        if invalid || bytes.len() > MAX_SESSION_BYTES {
                            self.state = LoaderState::Close {
                                file,
                                bytes,
                                invalid: true,
                            };
                        } else {
                            self.state = LoaderState::Read {
                                file,
                                bytes,
                                invalid: false,
                                blocked: false,
                            };
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        self.state = LoaderState::Read {
                            file,
                            bytes,
                            invalid,
                            blocked: true,
                        };
                    }
                    Err(error) => {
                        eprintln!("shell: session read failed: {error}");
                        self.state = LoaderState::Close {
                            file,
                            bytes,
                            invalid: true,
                        };
                    }
                }
                SessionLoadStep::Pending
            }
            LoaderState::Close {
                file,
                bytes,
                invalid,
            } => {
                let closed = filesystem.close(file).is_ok();
                let session = (!invalid && closed)
                    .then(|| StoredSession::parse(&bytes).ok())
                    .flatten();
                if session.is_none() && !bytes.is_empty() {
                    eprintln!("shell: ignoring invalid durable session");
                }
                SessionLoadStep::Complete(session)
            }
            LoaderState::Complete => SessionLoadStep::Complete(None),
        }
    }
}

#[derive(Clone)]
struct PendingRevision {
    revision: u64,
    bytes: Vec<u8>,
}

struct WriteJob {
    revision: u64,
    bytes: Vec<u8>,
    temp_path: PathBuf,
    temp_attempts: usize,
}

enum WriterState {
    Idle,
    EnsureDirectory {
        job: WriteJob,
        directories: Vec<PathBuf>,
        next: usize,
    },
    Create(WriteJob),
    Write {
        job: WriteJob,
        file: Box<dyn SessionFile>,
        offset: usize,
        blocked: bool,
    },
    SyncTemp {
        job: WriteJob,
        file: Box<dyn SessionFile>,
    },
    CloseTemp {
        job: WriteJob,
        file: Box<dyn SessionFile>,
    },
    Rename(WriteJob),
    Reopen(WriteJob),
    SyncTarget {
        job: WriteJob,
        file: Box<dyn SessionFile>,
    },
    CloseTarget {
        job: WriteJob,
        file: Box<dyn SessionFile>,
    },
    FailureClose {
        job: WriteJob,
        file: Box<dyn SessionFile>,
        cleanup_temp: bool,
    },
    Cleanup(WriteJob),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SessionWriteStep {
    Idle,
    Pending,
    Durable {
        revision: u64,
        bytes: usize,
        digest: u64,
    },
    Failed(u64),
}

/// Complete-snapshot atomic writer with revision coalescing. The target is
/// replaced only after a synced temporary file is closed, then reopened and
/// synced before the revision is reported durable.
pub struct AtomicSessionWriter {
    path: PathBuf,
    state: WriterState,
    pending: Option<PendingRevision>,
    next_revision: u64,
    next_temp: u64,
    durable_revision: u64,
    durable_bytes: Vec<u8>,
}

impl AtomicSessionWriter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            state: WriterState::Idle,
            pending: None,
            next_revision: 1,
            next_temp: 1,
            durable_revision: 0,
            durable_bytes: Vec::new(),
        }
    }

    pub fn request(&mut self, session: &StoredSession) -> Result<u64, SessionFormatError> {
        let bytes = session.serialize()?.into_bytes();
        if let Some(pending) = self
            .pending
            .as_ref()
            .filter(|pending| pending.bytes == bytes)
        {
            return Ok(pending.revision);
        }
        if let Some((revision, active)) = self.active_revision_bytes() {
            if active == bytes {
                if self.pending.is_some() {
                    return Ok(self.queue_revision(bytes));
                }
                return Ok(revision);
            }
        }
        if matches!(self.state, WriterState::Idle)
            && self.durable_revision != 0
            && self.durable_bytes == bytes
        {
            self.pending = None;
            return Ok(self.durable_revision);
        }
        Ok(self.queue_revision(bytes))
    }

    pub fn durable_revision(&self) -> u64 {
        self.durable_revision
    }

    pub fn pending(&self) -> bool {
        !matches!(self.state, WriterState::Idle) || self.pending.is_some()
    }

    pub fn has_queued_revision(&self, revision: u64) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.revision == revision)
            || self
                .active_revision_bytes()
                .is_some_and(|(active, _)| active == revision)
    }

    pub fn wait(&self) -> Option<SessionWait> {
        match &self.state {
            WriterState::Write {
                file,
                blocked: true,
                ..
            } => file.wait_fd().map(SessionWait::Write),
            _ => None,
        }
    }

    pub fn step(&mut self, filesystem: &mut dyn SessionFilesystem) -> SessionWriteStep {
        if matches!(self.state, WriterState::Idle) {
            let Some(pending) = self.pending.take() else {
                return SessionWriteStep::Idle;
            };
            let temp_path = self.temp_path();
            let job = WriteJob {
                revision: pending.revision,
                bytes: pending.bytes,
                temp_path,
                temp_attempts: 1,
            };
            let directories = directory_chain(&self.path);
            self.state = if directories.is_empty() {
                WriterState::Create(job)
            } else {
                WriterState::EnsureDirectory {
                    job,
                    directories,
                    next: 0,
                }
            };
        }

        let state = core::mem::replace(&mut self.state, WriterState::Idle);
        match state {
            WriterState::Idle => SessionWriteStep::Idle,
            WriterState::EnsureDirectory {
                job,
                directories,
                next,
            } => match filesystem.create_dir(&directories[next]) {
                Ok(()) => {
                    self.continue_directories(job, directories, next + 1);
                    SessionWriteStep::Pending
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    self.continue_directories(job, directories, next + 1);
                    SessionWriteStep::Pending
                }
                Err(error) => self.fail(job, error, false),
            },
            WriterState::Create(mut job) => match filesystem.create_new(&job.temp_path) {
                Ok(file) => {
                    self.state = WriterState::Write {
                        job,
                        file,
                        offset: 0,
                        blocked: false,
                    };
                    SessionWriteStep::Pending
                }
                Err(error)
                    if error.kind() == io::ErrorKind::AlreadyExists
                        && job.temp_attempts < MAX_TEMP_CREATE_ATTEMPTS =>
                {
                    job.temp_attempts += 1;
                    job.temp_path = self.temp_path();
                    self.state = WriterState::Create(job);
                    SessionWriteStep::Pending
                }
                Err(error) => self.fail(job, error, false),
            },
            WriterState::Write {
                job,
                mut file,
                offset,
                ..
            } => {
                let end = offset
                    .saturating_add(SESSION_IO_CHUNK_BYTES)
                    .min(job.bytes.len());
                match file.write(&job.bytes[offset..end]) {
                    Ok(0) => self.fail_with_file(
                        job,
                        file,
                        io::Error::new(io::ErrorKind::WriteZero, "zero-progress session write"),
                        true,
                    ),
                    Ok(written) => {
                        let offset = offset + written;
                        self.state = if offset == job.bytes.len() {
                            WriterState::SyncTemp { job, file }
                        } else {
                            WriterState::Write {
                                job,
                                file,
                                offset,
                                blocked: false,
                            }
                        };
                        SessionWriteStep::Pending
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        self.state = WriterState::Write {
                            job,
                            file,
                            offset,
                            blocked: true,
                        };
                        SessionWriteStep::Pending
                    }
                    Err(error) => self.fail_with_file(job, file, error, true),
                }
            }
            WriterState::SyncTemp { job, mut file } => match file.sync_all() {
                Ok(()) => {
                    self.state = WriterState::CloseTemp { job, file };
                    SessionWriteStep::Pending
                }
                Err(error) => self.fail_with_file(job, file, error, true),
            },
            WriterState::CloseTemp { job, file } => match filesystem.close(file) {
                Ok(()) => {
                    self.state = WriterState::Rename(job);
                    SessionWriteStep::Pending
                }
                Err(error) => self.fail(job, error, true),
            },
            WriterState::Rename(job) => match filesystem.rename(&job.temp_path, &self.path) {
                Ok(()) => {
                    self.state = WriterState::Reopen(job);
                    SessionWriteStep::Pending
                }
                Err(error) => self.fail(job, error, true),
            },
            WriterState::Reopen(job) => match filesystem.open_sync(&self.path) {
                Ok(file) => {
                    self.state = WriterState::SyncTarget { job, file };
                    SessionWriteStep::Pending
                }
                Err(error) => self.fail(job, error, false),
            },
            WriterState::SyncTarget { job, mut file } => match file.sync_all() {
                Ok(()) => {
                    self.state = WriterState::CloseTarget { job, file };
                    SessionWriteStep::Pending
                }
                Err(error) => self.fail_with_file(job, file, error, false),
            },
            WriterState::CloseTarget { job, file } => match filesystem.close(file) {
                Ok(()) => {
                    let revision = self
                        .pending
                        .as_ref()
                        .filter(|pending| pending.bytes == job.bytes)
                        .map(|pending| pending.revision)
                        .unwrap_or(job.revision);
                    if revision != job.revision {
                        self.pending = None;
                    }
                    self.durable_revision = self.durable_revision.max(revision);
                    let bytes = job.bytes.len();
                    let digest = session_digest(&job.bytes);
                    self.durable_bytes = job.bytes;
                    SessionWriteStep::Durable {
                        revision,
                        bytes,
                        digest,
                    }
                }
                Err(error) => self.fail(job, error, false),
            },
            WriterState::FailureClose {
                job,
                file,
                cleanup_temp,
            } => {
                if let Err(error) = filesystem.close(file) {
                    eprintln!(
                        "shell: session revision {} failure-close failed: {error}",
                        job.revision
                    );
                }
                if cleanup_temp {
                    self.state = WriterState::Cleanup(job);
                    SessionWriteStep::Pending
                } else {
                    SessionWriteStep::Failed(job.revision)
                }
            }
            WriterState::Cleanup(job) => {
                let revision = job.revision;
                match filesystem.remove_file(&job.temp_path) {
                    Ok(()) => SessionWriteStep::Failed(revision),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        SessionWriteStep::Failed(revision)
                    }
                    Err(error) => {
                        eprintln!("shell: could not remove failed session temp: {error}");
                        SessionWriteStep::Failed(revision)
                    }
                }
            }
        }
    }

    fn continue_directories(&mut self, job: WriteJob, directories: Vec<PathBuf>, next: usize) {
        self.state = if next == directories.len() {
            WriterState::Create(job)
        } else {
            WriterState::EnsureDirectory {
                job,
                directories,
                next,
            }
        };
    }

    fn queue_revision(&mut self, bytes: Vec<u8>) -> u64 {
        let revision = self.next_revision;
        self.next_revision = self.next_revision.wrapping_add(1).max(1);
        self.pending = Some(PendingRevision { revision, bytes });
        revision
    }

    fn fail(&mut self, job: WriteJob, error: io::Error, cleanup: bool) -> SessionWriteStep {
        eprintln!("shell: session revision {} failed: {error}", job.revision);
        let revision = job.revision;
        if cleanup {
            self.state = WriterState::Cleanup(job);
            SessionWriteStep::Pending
        } else {
            SessionWriteStep::Failed(revision)
        }
    }

    fn fail_with_file(
        &mut self,
        job: WriteJob,
        file: Box<dyn SessionFile>,
        error: io::Error,
        cleanup_temp: bool,
    ) -> SessionWriteStep {
        eprintln!("shell: session revision {} failed: {error}", job.revision);
        self.state = WriterState::FailureClose {
            job,
            file,
            cleanup_temp,
        };
        SessionWriteStep::Pending
    }

    fn active_revision_bytes(&self) -> Option<(u64, &[u8])> {
        let job = match &self.state {
            WriterState::EnsureDirectory { job, .. }
            | WriterState::Create(job)
            | WriterState::Write { job, .. }
            | WriterState::SyncTemp { job, .. }
            | WriterState::CloseTemp { job, .. }
            | WriterState::Rename(job)
            | WriterState::Reopen(job)
            | WriterState::SyncTarget { job, .. }
            | WriterState::CloseTarget { job, .. } => job,
            WriterState::Idle | WriterState::FailureClose { .. } | WriterState::Cleanup(_) => {
                return None
            }
        };
        Some((job.revision, &job.bytes))
    }

    fn temp_path(&mut self) -> PathBuf {
        let suffix = self.next_temp;
        self.next_temp = self.next_temp.wrapping_add(1).max(1);
        let name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session-v1");
        self.path.with_file_name(format!("{name}.tmp.{suffix}"))
    }
}

fn directory_chain(path: &Path) -> Vec<PathBuf> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let mut directories = parent
        .ancestors()
        .filter(|ancestor| ancestor.parent().is_some())
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    directories.reverse();
    directories
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn sample() -> StoredSession {
        StoredSession {
            output_width: 1024,
            output_height: 768,
            focused_window: Some(2),
            instances: vec![
                StoredInstance {
                    id: 2,
                    desktop_entry_id: "terminal".into(),
                },
                StoredInstance {
                    id: 1,
                    desktop_entry_id: "files".into(),
                },
            ],
            windows: vec![
                StoredWindow {
                    id: 2,
                    instance_id: 2,
                    ordinal: 1,
                    z_rank: 1,
                    normal_x: 100,
                    normal_y: 80,
                    normal_width: 720,
                    normal_height: 480,
                    flags: SESSION_FLAG_MAXIMIZED,
                },
                StoredWindow {
                    id: 1,
                    instance_id: 1,
                    ordinal: 1,
                    z_rank: 0,
                    normal_x: -10,
                    normal_y: 22,
                    normal_width: 640,
                    normal_height: 420,
                    flags: 0,
                },
            ],
        }
    }

    #[test]
    fn canonical_round_trip_sorts_records() {
        let encoded = sample().serialize().unwrap();
        assert_eq!(
            encoded,
            concat!(
                "PMOS_SESSION_V1\n",
                "output 1024 768\n",
                "focus 2\n",
                "instance 1 files\n",
                "instance 2 terminal\n",
                "window 1 1 1 0 -10 22 640 420 0\n",
                "window 2 2 1 1 100 80 720 480 2\n",
            )
        );
        assert!(encoded.ends_with('\n'));
        assert_eq!(encoded.len(), 141);
        assert_eq!(session_digest(encoded.as_bytes()), 0xd19b_87b8_a6ba_8247);
        let decoded = StoredSession::parse(encoded.as_bytes()).unwrap();
        assert_eq!(decoded.serialize().unwrap(), encoded);
    }

    #[test]
    fn empty_session_is_valid() {
        let session = StoredSession::empty(1024, 768).unwrap();
        let encoded = session.serialize().unwrap();
        assert_eq!(StoredSession::parse(encoded.as_bytes()).unwrap(), session);
    }

    #[test]
    fn malformed_or_unsafe_snapshots_fail_closed() {
        let cases = [
            "PMOS_SESSION_V2\noutput 1 1\nfocus 0\n",
            "PMOS_SESSION_V1\noutput 1 1 extra\nfocus 0\n",
            "PMOS_SESSION_V1\noutput 1 1\nfocus 0\ninstance 1 ../term\nwindow 1 1 1 0 0 0 1 1 0\n",
            "PMOS_SESSION_V1\noutput 1 1\nfocus 2\ninstance 1 term\nwindow 1 1 1 0 0 0 1 1 0\n",
            "PMOS_SESSION_V1\noutput 1 1\nfocus 1\ninstance 1 term\nwindow 1 1 1 0 0 0 1 1 1\n",
            "PMOS_SESSION_V1\noutput 1 1\nfocus 0\ninstance 1 term\nwindow 1 1 1 1 0 0 1 1 0\n",
            "PMOS_SESSION_V1\noutput 1 1\nfocus 0\ninstance 1 term\nwindow 1 1 1 0 0 0 0 1 0\n",
            "PMOS_SESSION_V1\noutput 1 1\nfocus 0\ninstance 1 term\nwindow 1 1 1 0 0 0 1 1 4\n",
            "PMOS_SESSION_V1\noutput 01 1\nfocus 0\n",
            "PMOS_SESSION_V1\noutput 1 1\nfocus +0\n",
        ];
        for case in cases {
            assert!(StoredSession::parse(case.as_bytes()).is_err(), "{case}");
        }
    }

    #[test]
    fn exact_limits_are_accepted_and_one_over_rejected() {
        let mut session = StoredSession::empty(1, 1).unwrap();
        for id in 1..=MAX_SESSION_WINDOWS as u32 {
            session.instances.push(StoredInstance {
                id,
                desktop_entry_id: format!("app-{id}"),
            });
            session.windows.push(StoredWindow {
                id,
                instance_id: id,
                ordinal: 1,
                z_rank: id - 1,
                normal_x: 0,
                normal_y: 0,
                normal_width: 1,
                normal_height: 1,
                flags: 0,
            });
        }
        assert!(session.serialize().is_ok());
        session.instances.push(StoredInstance {
            id: 65,
            desktop_entry_id: "overflow".into(),
        });
        assert_eq!(session.serialize(), Err(SessionFormatError::LimitExceeded));
    }

    #[test]
    fn oversized_input_is_rejected_before_utf8_or_token_work() {
        let bytes = vec![b'x'; MAX_SESSION_BYTES + 1];
        assert_eq!(
            StoredSession::parse(&bytes),
            Err(SessionFormatError::TooLarge)
        );
    }

    #[derive(Default)]
    struct MemoryFsState {
        files: BTreeMap<PathBuf, Vec<u8>>,
        operations: usize,
        close_operations: usize,
        fail_next_write: bool,
    }

    #[derive(Clone, Default)]
    struct MemoryFs {
        state: Rc<RefCell<MemoryFsState>>,
    }

    struct MemoryFile {
        state: Rc<RefCell<MemoryFsState>>,
        path: PathBuf,
        offset: usize,
    }

    impl SessionFile for MemoryFile {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let mut state = self.state.borrow_mut();
            state.operations += 1;
            let bytes = state.files.get(&self.path).cloned().unwrap_or_default();
            let count = buffer.len().min(bytes.len().saturating_sub(self.offset));
            buffer[..count].copy_from_slice(&bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        }

        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let mut state = self.state.borrow_mut();
            state.operations += 1;
            if core::mem::take(&mut state.fail_next_write) {
                return Err(io::Error::other("injected write failure"));
            }
            let bytes = state.files.entry(self.path.clone()).or_default();
            if bytes.len() < self.offset {
                bytes.resize(self.offset, 0);
            }
            let end = self.offset + buffer.len();
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            bytes[self.offset..end].copy_from_slice(buffer);
            self.offset = end;
            Ok(buffer.len())
        }

        fn sync_all(&mut self) -> io::Result<()> {
            self.state.borrow_mut().operations += 1;
            Ok(())
        }

        fn wait_fd(&self) -> Option<i32> {
            None
        }
    }

    impl SessionFilesystem for MemoryFs {
        fn open_read(&mut self, path: &Path) -> io::Result<Box<dyn SessionFile>> {
            let mut state = self.state.borrow_mut();
            state.operations += 1;
            if !state.files.contains_key(path) {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            Ok(Box::new(MemoryFile {
                state: self.state.clone(),
                path: path.to_path_buf(),
                offset: 0,
            }))
        }

        fn create_new(&mut self, path: &Path) -> io::Result<Box<dyn SessionFile>> {
            let mut state = self.state.borrow_mut();
            state.operations += 1;
            if state.files.contains_key(path) {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            state.files.insert(path.to_path_buf(), Vec::new());
            Ok(Box::new(MemoryFile {
                state: self.state.clone(),
                path: path.to_path_buf(),
                offset: 0,
            }))
        }

        fn open_sync(&mut self, path: &Path) -> io::Result<Box<dyn SessionFile>> {
            self.open_read(path)
        }

        fn close(&mut self, file: Box<dyn SessionFile>) -> io::Result<()> {
            let mut state = self.state.borrow_mut();
            state.operations += 1;
            state.close_operations += 1;
            drop(state);
            drop(file);
            Ok(())
        }

        fn create_dir(&mut self, _path: &Path) -> io::Result<()> {
            self.state.borrow_mut().operations += 1;
            Err(io::Error::from(io::ErrorKind::AlreadyExists))
        }

        fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
            let mut state = self.state.borrow_mut();
            state.operations += 1;
            let bytes = state
                .files
                .remove(from)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            state.files.insert(to.to_path_buf(), bytes);
            Ok(())
        }

        fn remove_file(&mut self, path: &Path) -> io::Result<()> {
            let mut state = self.state.borrow_mut();
            state.operations += 1;
            state
                .files
                .remove(path)
                .map(|_| ())
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
    }

    impl MemoryFs {
        fn put(&self, path: &str, bytes: impl Into<Vec<u8>>) {
            self.state
                .borrow_mut()
                .files
                .insert(PathBuf::from(path), bytes.into());
        }

        fn get(&self, path: &str) -> Option<Vec<u8>> {
            self.state.borrow().files.get(Path::new(path)).cloned()
        }

        fn operations(&self) -> usize {
            self.state.borrow().operations
        }

        fn close_operations(&self) -> usize {
            self.state.borrow().close_operations
        }
    }

    #[test]
    fn loader_and_writer_perform_at_most_one_filesystem_operation_per_step() {
        let session = sample();
        let encoded = session.serialize().unwrap();
        let mut filesystem = MemoryFs::default();
        filesystem.put("/session", encoded.clone());
        let mut loader = SessionLoader::new("/session");
        loop {
            let before = filesystem.operations();
            let step = loader.step(&mut filesystem);
            assert!(filesystem.operations() - before <= 1);
            if let SessionLoadStep::Complete(loaded) = step {
                assert_eq!(
                    loaded.and_then(|loaded| loaded.serialize().ok()),
                    Some(encoded.clone())
                );
                break;
            }
        }

        let replacement = StoredSession::empty(800, 600).unwrap();
        let expected = replacement.serialize().unwrap().into_bytes();
        let mut writer = AtomicSessionWriter::new("/session");
        let revision = writer.request(&replacement).unwrap();
        let writer_close_baseline = filesystem.close_operations();
        loop {
            let before = filesystem.operations();
            let closes_before = filesystem.close_operations();
            let step = writer.step(&mut filesystem);
            assert!(filesystem.operations() - before <= 1);
            let target = filesystem.get("/session").unwrap();
            assert!(target == encoded.as_bytes() || target == expected);
            if let SessionWriteStep::Durable {
                revision: durable_revision,
                bytes,
                digest,
            } = step
            {
                assert_eq!(durable_revision, revision);
                assert_eq!(bytes, expected.len());
                assert_eq!(digest, session_digest(&expected));
                assert_eq!(filesystem.close_operations(), closes_before + 1);
                assert_eq!(filesystem.close_operations(), writer_close_baseline + 2);
                break;
            }
        }
        assert_eq!(filesystem.get("/session"), Some(expected));
    }

    #[test]
    fn failed_revision_can_retry_identical_bytes_and_preserves_old_target() {
        let old = sample().serialize().unwrap().into_bytes();
        let replacement = StoredSession::empty(800, 600).unwrap();
        let mut filesystem = MemoryFs::default();
        filesystem.put("/session", old.clone());
        filesystem.state.borrow_mut().fail_next_write = true;
        let mut writer = AtomicSessionWriter::new("/session");
        let failed_revision = writer.request(&replacement).unwrap();
        loop {
            let before = filesystem.operations();
            let step = writer.step(&mut filesystem);
            assert!(filesystem.operations() - before <= 1);
            if step == SessionWriteStep::Failed(failed_revision) {
                break;
            }
        }
        assert_eq!(filesystem.get("/session"), Some(old));

        let retry_revision = writer.request(&replacement).unwrap();
        assert!(retry_revision > failed_revision);
        loop {
            let step = writer.step(&mut filesystem);
            if let SessionWriteStep::Durable {
                revision,
                bytes,
                digest,
            } = step
            {
                let expected = replacement.serialize().unwrap().into_bytes();
                assert_eq!(revision, retry_revision);
                assert_eq!(bytes, expected.len());
                assert_eq!(digest, session_digest(&expected));
                break;
            }
        }
        assert_eq!(
            filesystem.get("/session"),
            Some(replacement.serialize().unwrap().into_bytes())
        );
    }

    #[test]
    fn latest_state_matching_active_cancels_a_different_pending_revision() {
        let state_a = sample();
        let state_b = StoredSession::empty(800, 600).unwrap();
        let mut filesystem = MemoryFs::default();
        let mut writer = AtomicSessionWriter::new("/session");

        let revision_a = writer.request(&state_a).unwrap();
        assert_eq!(writer.step(&mut filesystem), SessionWriteStep::Pending);
        let revision_b = writer.request(&state_b).unwrap();
        assert!(writer.has_queued_revision(revision_b));
        let latest_a = writer.request(&state_a).unwrap();
        assert!(latest_a > revision_b);
        assert!(writer.has_queued_revision(revision_a));
        assert!(
            !writer.has_queued_revision(revision_b),
            "the stale B successor must be canceled when latest state returns to active A",
        );
        assert!(writer.has_queued_revision(latest_a));

        let mut durable = Vec::new();
        while writer.pending() {
            if let SessionWriteStep::Durable { revision, .. } = writer.step(&mut filesystem) {
                durable.push(revision);
            }
        }
        assert_eq!(durable, vec![latest_a]);
        assert_eq!(
            filesystem.get("/session"),
            Some(state_a.serialize().unwrap().into_bytes()),
        );

        let revision_b = writer.request(&state_b).unwrap();
        assert!(writer.has_queued_revision(revision_b));
        assert_eq!(writer.request(&state_a).unwrap(), latest_a);
        assert!(
            !writer.pending(),
            "latest state equal to durable A cancels an unstarted B",
        );
    }

    #[test]
    fn active_failure_after_a_b_a_retries_the_latest_a_revision() {
        let state_a = sample();
        let state_b = StoredSession::empty(800, 600).unwrap();
        let mut filesystem = MemoryFs::default();
        let mut writer = AtomicSessionWriter::new("/session");

        let active_a = writer.request(&state_a).unwrap();
        assert_eq!(writer.step(&mut filesystem), SessionWriteStep::Pending);
        let stale_b = writer.request(&state_b).unwrap();
        let latest_a = writer.request(&state_a).unwrap();
        assert!(latest_a > stale_b);
        filesystem.state.borrow_mut().fail_next_write = true;

        let mut failed = Vec::new();
        let mut durable = Vec::new();
        while writer.pending() {
            match writer.step(&mut filesystem) {
                SessionWriteStep::Failed(revision) => failed.push(revision),
                SessionWriteStep::Durable { revision, .. } => durable.push(revision),
                SessionWriteStep::Idle | SessionWriteStep::Pending => {}
            }
        }
        assert_eq!(failed, vec![active_a]);
        assert_eq!(durable, vec![latest_a]);
        assert_eq!(
            filesystem.get("/session"),
            Some(state_a.serialize().unwrap().into_bytes()),
        );
    }

    #[test]
    fn temp_name_collisions_fail_after_a_finite_number_of_create_operations() {
        let old = sample().serialize().unwrap().into_bytes();
        let replacement = StoredSession::empty(800, 600).unwrap();
        let mut filesystem = MemoryFs::default();
        filesystem.put("/session", old.clone());
        for suffix in 1..=MAX_TEMP_CREATE_ATTEMPTS {
            filesystem.put(&format!("/session.tmp.{suffix}"), b"collision".to_vec());
        }
        let mut writer = AtomicSessionWriter::new("/session");
        let revision = writer.request(&replacement).unwrap();

        let before = filesystem.operations();
        let mut steps = 0;
        loop {
            steps += 1;
            let step_before = filesystem.operations();
            let step = writer.step(&mut filesystem);
            assert_eq!(
                filesystem.operations() - step_before,
                1,
                "each collision turn performs only create_new",
            );
            if step == SessionWriteStep::Failed(revision) {
                break;
            }
            assert_eq!(step, SessionWriteStep::Pending);
        }

        assert_eq!(steps, MAX_TEMP_CREATE_ATTEMPTS);
        assert_eq!(filesystem.operations() - before, MAX_TEMP_CREATE_ATTEMPTS,);
        assert_eq!(filesystem.close_operations(), 0);
        assert!(!writer.pending());
        assert_eq!(writer.step(&mut filesystem), SessionWriteStep::Idle);
        assert_eq!(filesystem.get("/session"), Some(old));
    }

    #[test]
    fn loader_closes_immediately_after_crossing_the_size_limit() {
        let mut filesystem = MemoryFs::default();
        filesystem.put("/session", vec![b'x'; 2 * 1024 * 1024]);
        let mut loader = SessionLoader::new("/session");
        let mut steps = 0;
        loop {
            steps += 1;
            if let SessionLoadStep::Complete(result) = loader.step(&mut filesystem) {
                assert_eq!(result, None);
                break;
            }
        }
        // open + four exact 16KiB reads + one crossing read + explicit close.
        assert_eq!(steps, 7);
        assert_eq!(filesystem.operations(), 7);
    }
}
