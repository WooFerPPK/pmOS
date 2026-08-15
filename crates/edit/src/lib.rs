//! Pure document and lifecycle state for the PMos text editor.
//!
//! The GUI binary translates display-protocol input into [`EditorInput`].
//! This module owns every data-loss-sensitive transition and can therefore be
//! tested natively without a display server or browser.

use std::cmp::min;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

/// Maximum UTF-8 document size accepted by Edit.
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
/// Maximum document body transferred by one GUI event-loop turn.
pub const DOCUMENT_IO_CHUNK_BYTES: usize = 16 * 1024;
/// Maximum UTF-8 path size accepted by Edit's path dialogs.
pub const MAX_PATH_BYTES: usize = 1024;
/// Initial directory shown by Open and Save As.
pub const DEFAULT_DOCUMENT_DIRECTORY: &str = "/home/user/Documents";

#[derive(Debug)]
pub enum DocumentError {
    InvalidPath(String),
    TooLarge {
        path: String,
        bytes: usize,
        max: usize,
    },
    InvalidUtf8(String),
    Io {
        path: String,
        source: io::Error,
    },
}

impl DocumentError {
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Io { source, .. } if source.kind() == io::ErrorKind::NotFound
        )
    }
}

impl fmt::Display for DocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(reason) => write!(f, "invalid VFS path: {reason}"),
            Self::TooLarge { path, bytes, max } => {
                write!(f, "{path} is {bytes} bytes; limit is {max} bytes")
            }
            Self::InvalidUtf8(path) => write!(f, "{path} is not valid UTF-8 text"),
            Self::Io { path, source } => write!(f, "{path}: {source}"),
        }
    }
}

impl std::error::Error for DocumentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn validate_path(path: &str) -> Result<(), DocumentError> {
    if path.is_empty() {
        return Err(DocumentError::InvalidPath("path is empty".to_string()));
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(DocumentError::InvalidPath(format!(
            "path exceeds {MAX_PATH_BYTES} bytes"
        )));
    }
    if path.as_bytes().contains(&0) {
        return Err(DocumentError::InvalidPath(
            "path contains a NUL byte".to_string(),
        ));
    }
    if !Path::new(path).is_absolute() {
        return Err(DocumentError::InvalidPath(
            "enter an absolute PMos path beginning with /".to_string(),
        ));
    }
    Ok(())
}

fn io_error(path: &Path, source: io::Error) -> DocumentError {
    DocumentError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Read one bounded UTF-8 document from the process's PMos VFS preopen.
pub fn read_file(path: &str) -> Result<String, DocumentError> {
    validate_path(path)?;
    let path = Path::new(path);
    let mut file = File::open(path).map_err(|error| io_error(path, error))?;
    read_open_file(path, &mut file)
}

fn read_open_file(path: &Path, file: &mut File) -> Result<String, DocumentError> {
    file.seek(io::SeekFrom::Start(0))
        .map_err(|error| io_error(path, error))?;
    let mut bytes = Vec::new();
    Read::by_ref(file)
        .take((MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(path, error))?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(DocumentError::TooLarge {
            path: path.display().to_string(),
            bytes: bytes.len(),
            max: MAX_DOCUMENT_BYTES,
        });
    }
    file.seek(io::SeekFrom::Start(0))
        .map_err(|error| io_error(path, error))?;
    String::from_utf8(bytes).map_err(|_| DocumentError::InvalidUtf8(path.display().to_string()))
}

/// Safely replace one bounded UTF-8 document in the PMos VFS.
///
/// Bytes are written and synced to a fresh sibling before `rename` replaces
/// the destination. A failed write therefore leaves the prior document
/// untouched, and a failed rename leaves no partial destination.
pub fn write_file(path: &str, contents: &str) -> Result<(), DocumentError> {
    validate_path(path)?;
    if contents.len() > MAX_DOCUMENT_BYTES {
        return Err(DocumentError::TooLarge {
            path: path.to_string(),
            bytes: contents.len(),
            max: MAX_DOCUMENT_BYTES,
        });
    }

    let destination = Path::new(path);
    let parent = destination.parent().ok_or_else(|| {
        DocumentError::InvalidPath("destination has no parent directory".to_string())
    })?;
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| DocumentError::InvalidPath("destination has no file name".to_string()))?;

    let (temporary_path, mut temporary) = create_temporary(parent, file_name)?;
    let write_result = temporary
        .write_all(contents.as_bytes())
        .and_then(|()| temporary.sync_all());
    if let Err(error) = write_result {
        drop(temporary);
        let _ = fs::remove_file(&temporary_path);
        return Err(io_error(&temporary_path, error));
    }
    drop(temporary);

    if let Err(error) = fs::rename(&temporary_path, destination) {
        let _ = fs::remove_file(&temporary_path);
        return Err(io_error(destination, error));
    }
    Ok(())
}

fn create_temporary(parent: &Path, file_name: &str) -> Result<(PathBuf, File), DocumentError> {
    for suffix in 0..32_u8 {
        let candidate = parent.join(format!(".{file_name}.pmos-save-{suffix}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(&candidate, error)),
        }
    }
    Err(DocumentError::Io {
        path: parent.display().to_string(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free atomic-save temporary name",
        ),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DocumentHandle(u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenDocument {
    pub contents: String,
    pub handle: Option<DocumentHandle>,
}

pub trait DocumentStore {
    fn read_document(&mut self, path: &str) -> Result<String, DocumentError>;
    fn write_document(&mut self, path: &str, contents: &str) -> Result<(), DocumentError>;

    fn open_document(&mut self, path: &str) -> Result<OpenDocument, DocumentError> {
        self.read_document(path).map(|contents| OpenDocument {
            contents,
            handle: None,
        })
    }

    fn create_document(
        &mut self,
        path: &str,
        contents: &str,
    ) -> Result<OpenDocument, DocumentError> {
        self.write_document(path, contents)?;
        self.open_document(path)
    }

    fn write_open_document(
        &mut self,
        _handle: DocumentHandle,
        path: &str,
        contents: &str,
    ) -> Result<(), DocumentError> {
        self.write_document(path, contents)
    }

    fn close_document(&mut self, _handle: DocumentHandle) {}
}

#[derive(Debug, Default)]
pub struct StdDocumentStore {
    next_handle: u64,
    open_files: BTreeMap<DocumentHandle, File>,
}

impl DocumentStore for StdDocumentStore {
    fn read_document(&mut self, path: &str) -> Result<String, DocumentError> {
        read_file(path)
    }

    fn write_document(&mut self, path: &str, contents: &str) -> Result<(), DocumentError> {
        write_file(path, contents)
    }

    fn open_document(&mut self, path: &str) -> Result<OpenDocument, DocumentError> {
        validate_path(path)?;
        let filesystem_path = Path::new(path);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(filesystem_path)
            .map_err(|error| io_error(filesystem_path, error))?;
        let contents = read_open_file(filesystem_path, &mut file)?;
        self.next_handle = self.next_handle.wrapping_add(1).max(1);
        let handle = DocumentHandle(self.next_handle);
        self.open_files.insert(handle, file);
        Ok(OpenDocument {
            contents,
            handle: Some(handle),
        })
    }

    fn create_document(
        &mut self,
        path: &str,
        contents: &str,
    ) -> Result<OpenDocument, DocumentError> {
        // New and Save As have no stable inode identity to preserve, so keep
        // the crash-safe sibling-write + atomic-rename behavior.
        write_file(path, contents)?;
        self.open_document(path)
    }

    fn write_open_document(
        &mut self,
        handle: DocumentHandle,
        path: &str,
        contents: &str,
    ) -> Result<(), DocumentError> {
        validate_path(path)?;
        if contents.len() > MAX_DOCUMENT_BYTES {
            return Err(DocumentError::TooLarge {
                path: path.to_string(),
                bytes: contents.len(),
                max: MAX_DOCUMENT_BYTES,
            });
        }
        let filesystem_path = Path::new(path);
        let file = self.open_files.get_mut(&handle).ok_or_else(|| {
            io_error(
                filesystem_path,
                io::Error::new(io::ErrorKind::NotFound, "open document handle was closed"),
            )
        })?;

        // Normal Save deliberately writes through the retained fd. POSIX/WASI
        // keeps that fd attached to the inode when Files renames the directory
        // entry, so this cannot recreate a stale pathname.
        file.seek(io::SeekFrom::Start(0))
            .and_then(|_| file.write_all(contents.as_bytes()))
            .and_then(|_| file.set_len(contents.len() as u64))
            .and_then(|_| file.sync_all())
            .and_then(|_| file.seek(io::SeekFrom::Start(0)).map(|_| ()))
            .map_err(|error| io_error(filesystem_path, error))
    }

    fn close_document(&mut self, handle: DocumentHandle) {
        self.open_files.remove(&handle);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentWaitInterest {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentWait {
    pub fd: RawFd,
    pub interest: DocumentWaitInterest,
}

#[derive(Debug)]
pub enum DocumentJobSuccess {
    Opened(OpenDocument),
    Saved,
    Created(OpenDocument),
}

#[derive(Debug)]
pub enum DocumentJobTurn {
    /// One bounded filesystem operation completed. Return to display dispatch
    /// before advancing the job again.
    Progress,
    /// The current descriptor returned `EAGAIN`; park on this exact interest.
    Blocked(DocumentWait),
    Complete(DocumentJobSuccess),
    Failed(DocumentError),
}

#[derive(Debug)]
enum ReadPumpTurn {
    Progress,
    Blocked,
    Eof,
    Failed(io::Error),
}

#[derive(Debug, Default)]
struct ReadPump {
    bytes: Vec<u8>,
}

impl ReadPump {
    fn step(&mut self, mut read: impl FnMut(&mut [u8]) -> io::Result<usize>) -> ReadPumpTurn {
        let remaining = MAX_DOCUMENT_BYTES
            .saturating_add(1)
            .saturating_sub(self.bytes.len());
        if remaining == 0 {
            return ReadPumpTurn::Eof;
        }
        let mut chunk = vec![0_u8; remaining.min(DOCUMENT_IO_CHUNK_BYTES)];
        match read(&mut chunk) {
            Ok(0) => ReadPumpTurn::Eof,
            Ok(read) if read <= chunk.len() => {
                chunk.truncate(read);
                self.bytes.extend_from_slice(&chunk);
                ReadPumpTurn::Progress
            }
            Ok(_) => ReadPumpTurn::Failed(io::Error::other(
                "document read exceeded its destination buffer",
            )),
            Err(error) if io_would_block(&error) => ReadPumpTurn::Blocked,
            Err(error) => ReadPumpTurn::Failed(error),
        }
    }
}

#[derive(Debug)]
enum WritePumpTurn {
    Progress,
    Blocked,
    Complete,
    Failed(io::Error),
}

#[derive(Debug)]
struct WritePump {
    bytes: Vec<u8>,
    offset: usize,
}

impl WritePump {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, offset: 0 }
    }

    fn step(&mut self, mut write: impl FnMut(&[u8]) -> io::Result<usize>) -> WritePumpTurn {
        if self.offset == self.bytes.len() {
            return WritePumpTurn::Complete;
        }
        let end = self
            .offset
            .saturating_add(DOCUMENT_IO_CHUNK_BYTES)
            .min(self.bytes.len());
        let remaining = &self.bytes[self.offset..end];
        match write(remaining) {
            Ok(0) => WritePumpTurn::Failed(io::Error::new(
                io::ErrorKind::WriteZero,
                "document write made no progress",
            )),
            Ok(written) if written <= remaining.len() => {
                self.offset += written;
                WritePumpTurn::Progress
            }
            Ok(_) => WritePumpTurn::Failed(io::Error::other(
                "document write exceeded its source buffer",
            )),
            Err(error) if io_would_block(&error) => WritePumpTurn::Blocked,
            Err(error) => WritePumpTurn::Failed(error),
        }
    }
}

fn io_would_block(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock || error.raw_os_error() == Some(abi::errno::EAGAIN)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenStage {
    Open,
    Read,
    Rewind,
}

#[derive(Debug)]
struct OpenJob {
    path: PathBuf,
    stage: OpenStage,
    file: Option<File>,
    pump: ReadPump,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetainedSaveStage {
    RewindBeforeWrite,
    Write,
    Truncate,
    Sync,
    RewindAfterWrite,
}

#[derive(Debug)]
struct RetainedSaveJob {
    handle: DocumentHandle,
    path: PathBuf,
    stage: RetainedSaveStage,
    file: Option<File>,
    pump: WritePump,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtomicSaveStage {
    CreateParent,
    CreateTemporary,
    Write,
    SyncTemporary,
    Rename,
    Reopen,
    Cleanup,
}

#[derive(Debug)]
struct AtomicSaveJob {
    destination: PathBuf,
    directories: Vec<PathBuf>,
    directory_index: usize,
    file_name: String,
    temporary_attempt: u8,
    temporary_path: Option<PathBuf>,
    temporary: Option<File>,
    stage: AtomicSaveStage,
    pump: WritePump,
    pending_error: Option<DocumentError>,
}

#[derive(Debug)]
enum DocumentJobInner {
    Open(OpenJob),
    RetainedSave(RetainedSaveJob),
    AtomicSave(AtomicSaveJob),
    Finished,
}

/// One bounded GUI document operation. Every call to [`Self::step`] performs
/// at most one filesystem operation and transfers at most 16 KiB.
#[derive(Debug)]
pub struct DocumentJob {
    inner: DocumentJobInner,
}

impl StdDocumentStore {
    fn insert_open_file(&mut self, file: File) -> DocumentHandle {
        self.next_handle = self.next_handle.wrapping_add(1).max(1);
        let handle = DocumentHandle(self.next_handle);
        self.open_files.insert(handle, file);
        handle
    }

    pub fn start_open(&mut self, path: &str) -> Result<DocumentJob, DocumentError> {
        validate_path(path)?;
        Ok(DocumentJob {
            inner: DocumentJobInner::Open(OpenJob {
                path: PathBuf::from(path),
                stage: OpenStage::Open,
                file: None,
                pump: ReadPump::default(),
            }),
        })
    }

    pub fn start_retained_save(
        &mut self,
        handle: DocumentHandle,
        path: &str,
        contents: String,
    ) -> Result<DocumentJob, DocumentError> {
        validate_document_write(path, &contents)?;
        let filesystem_path = PathBuf::from(path);
        let file = self.open_files.remove(&handle).ok_or_else(|| {
            io_error(
                &filesystem_path,
                io::Error::new(io::ErrorKind::NotFound, "open document handle was closed"),
            )
        })?;
        Ok(DocumentJob {
            inner: DocumentJobInner::RetainedSave(RetainedSaveJob {
                handle,
                path: filesystem_path,
                stage: RetainedSaveStage::RewindBeforeWrite,
                file: Some(file),
                pump: WritePump::new(contents.into_bytes()),
            }),
        })
    }

    pub fn start_atomic_save(
        &mut self,
        path: &str,
        contents: String,
    ) -> Result<DocumentJob, DocumentError> {
        validate_document_write(path, &contents)?;
        let destination = PathBuf::from(path);
        let parent = destination.parent().ok_or_else(|| {
            DocumentError::InvalidPath("destination has no parent directory".to_string())
        })?;
        let file_name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| DocumentError::InvalidPath("destination has no file name".to_string()))?
            .to_string();
        let directories = directory_prefixes(parent);
        Ok(DocumentJob {
            inner: DocumentJobInner::AtomicSave(AtomicSaveJob {
                destination,
                directories,
                directory_index: 0,
                file_name,
                temporary_attempt: 0,
                temporary_path: None,
                temporary: None,
                stage: AtomicSaveStage::CreateParent,
                pump: WritePump::new(contents.into_bytes()),
                pending_error: None,
            }),
        })
    }

    /// Cancel an operation that has not modified a retained document. Open is
    /// always cancellable. Atomic Save As is cancellable until its rename;
    /// deterministic cleanup removes any temporary sibling first.
    pub fn cancel_job(&mut self, mut job: DocumentJob) -> Result<(), DocumentError> {
        let inner = std::mem::replace(&mut job.inner, DocumentJobInner::Finished);
        match inner {
            DocumentJobInner::Open(_) | DocumentJobInner::Finished => Ok(()),
            DocumentJobInner::RetainedSave(mut save) => {
                if let Some(file) = save.file.take() {
                    self.open_files.insert(save.handle, file);
                }
                Err(DocumentError::Io {
                    path: save.path.display().to_string(),
                    source: io::Error::new(
                        io::ErrorKind::Unsupported,
                        "an in-place save cannot be cancelled after it starts",
                    ),
                })
            }
            DocumentJobInner::AtomicSave(mut save) => {
                if save.stage == AtomicSaveStage::Reopen {
                    return Err(DocumentError::Io {
                        path: save.destination.display().to_string(),
                        source: io::Error::new(
                            io::ErrorKind::Unsupported,
                            "an atomically renamed save cannot be cancelled before rebinding",
                        ),
                    });
                }
                drop(save.temporary.take());
                if let Some(path) = save.temporary_path.take() {
                    match fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(io_error(&path, error)),
                    }
                }
                Ok(())
            }
        }
    }
}

fn validate_document_write(path: &str, contents: &str) -> Result<(), DocumentError> {
    validate_path(path)?;
    if contents.len() > MAX_DOCUMENT_BYTES {
        return Err(DocumentError::TooLarge {
            path: path.to_string(),
            bytes: contents.len(),
            max: MAX_DOCUMENT_BYTES,
        });
    }
    Ok(())
}

fn directory_prefixes(parent: &Path) -> Vec<PathBuf> {
    let mut prefixes = Vec::new();
    let mut current = PathBuf::new();
    for component in parent.components() {
        current.push(component.as_os_str());
        if current.parent().is_some() {
            prefixes.push(current.clone());
        }
    }
    prefixes
}

impl DocumentJob {
    pub fn is_cancellable(&self) -> bool {
        match &self.inner {
            DocumentJobInner::Open(_) => true,
            DocumentJobInner::AtomicSave(save) => save.stage != AtomicSaveStage::Reopen,
            DocumentJobInner::RetainedSave(_) | DocumentJobInner::Finished => false,
        }
    }

    pub fn step(&mut self, store: &mut StdDocumentStore) -> DocumentJobTurn {
        match &mut self.inner {
            DocumentJobInner::Open(open) => step_open(open, store),
            DocumentJobInner::RetainedSave(save) => step_retained_save(save, store),
            DocumentJobInner::AtomicSave(save) => step_atomic_save(save, store),
            DocumentJobInner::Finished => DocumentJobTurn::Failed(DocumentError::Io {
                path: "document job".to_string(),
                source: io::Error::other("document job was already completed"),
            }),
        }
        .also_finish(&mut self.inner)
    }
}

trait FinishDocumentTurn {
    fn also_finish(self, inner: &mut DocumentJobInner) -> Self;
}

impl FinishDocumentTurn for DocumentJobTurn {
    fn also_finish(self, inner: &mut DocumentJobInner) -> Self {
        if matches!(self, Self::Complete(_) | Self::Failed(_)) {
            *inner = DocumentJobInner::Finished;
        }
        self
    }
}

fn step_open(open: &mut OpenJob, store: &mut StdDocumentStore) -> DocumentJobTurn {
    match open.stage {
        OpenStage::Open => match OpenOptions::new().read(true).write(true).open(&open.path) {
            Ok(file) => {
                open.file = Some(file);
                open.stage = OpenStage::Read;
                DocumentJobTurn::Progress
            }
            Err(error) => DocumentJobTurn::Failed(io_error(&open.path, error)),
        },
        OpenStage::Read => {
            let file = open.file.as_mut().expect("open stage installed file");
            match open.pump.step(|bytes| file.read(bytes)) {
                ReadPumpTurn::Progress => {
                    if open.pump.bytes.len() > MAX_DOCUMENT_BYTES {
                        DocumentJobTurn::Failed(DocumentError::TooLarge {
                            path: open.path.display().to_string(),
                            bytes: open.pump.bytes.len(),
                            max: MAX_DOCUMENT_BYTES,
                        })
                    } else {
                        DocumentJobTurn::Progress
                    }
                }
                ReadPumpTurn::Blocked => DocumentJobTurn::Blocked(DocumentWait {
                    fd: file.as_raw_fd(),
                    interest: DocumentWaitInterest::Read,
                }),
                ReadPumpTurn::Eof => {
                    open.stage = OpenStage::Rewind;
                    DocumentJobTurn::Progress
                }
                ReadPumpTurn::Failed(error) => DocumentJobTurn::Failed(io_error(&open.path, error)),
            }
        }
        OpenStage::Rewind => {
            let file = open.file.as_mut().expect("open stage installed file");
            match file.seek(io::SeekFrom::Start(0)) {
                Ok(_) => {
                    let bytes = std::mem::take(&mut open.pump.bytes);
                    let contents = match String::from_utf8(bytes) {
                        Ok(contents) => contents,
                        Err(_) => {
                            return DocumentJobTurn::Failed(DocumentError::InvalidUtf8(
                                open.path.display().to_string(),
                            ))
                        }
                    };
                    let file = open.file.take().expect("open file remains owned");
                    let handle = store.insert_open_file(file);
                    DocumentJobTurn::Complete(DocumentJobSuccess::Opened(OpenDocument {
                        contents,
                        handle: Some(handle),
                    }))
                }
                Err(error) if io_would_block(&error) => DocumentJobTurn::Blocked(DocumentWait {
                    fd: file.as_raw_fd(),
                    interest: DocumentWaitInterest::Read,
                }),
                Err(error) => DocumentJobTurn::Failed(io_error(&open.path, error)),
            }
        }
    }
}

fn step_retained_save(save: &mut RetainedSaveJob, store: &mut StdDocumentStore) -> DocumentJobTurn {
    let file = save.file.as_mut().expect("retained save owns file");
    let turn = match save.stage {
        RetainedSaveStage::RewindBeforeWrite => match file.seek(io::SeekFrom::Start(0)) {
            Ok(_) => {
                save.stage = RetainedSaveStage::Write;
                DocumentJobTurn::Progress
            }
            Err(error) if io_would_block(&error) => blocked(file, DocumentWaitInterest::Write),
            Err(error) => DocumentJobTurn::Failed(io_error(&save.path, error)),
        },
        RetainedSaveStage::Write => match save.pump.step(|bytes| file.write(bytes)) {
            WritePumpTurn::Progress => DocumentJobTurn::Progress,
            WritePumpTurn::Blocked => blocked(file, DocumentWaitInterest::Write),
            WritePumpTurn::Complete => {
                save.stage = RetainedSaveStage::Truncate;
                DocumentJobTurn::Progress
            }
            WritePumpTurn::Failed(error) => DocumentJobTurn::Failed(io_error(&save.path, error)),
        },
        RetainedSaveStage::Truncate => match file.set_len(save.pump.bytes.len() as u64) {
            Ok(()) => {
                save.stage = RetainedSaveStage::Sync;
                DocumentJobTurn::Progress
            }
            Err(error) if io_would_block(&error) => blocked(file, DocumentWaitInterest::Write),
            Err(error) => DocumentJobTurn::Failed(io_error(&save.path, error)),
        },
        RetainedSaveStage::Sync => match file.sync_all() {
            Ok(()) => {
                save.stage = RetainedSaveStage::RewindAfterWrite;
                DocumentJobTurn::Progress
            }
            Err(error) if io_would_block(&error) => blocked(file, DocumentWaitInterest::Write),
            Err(error) => DocumentJobTurn::Failed(io_error(&save.path, error)),
        },
        RetainedSaveStage::RewindAfterWrite => match file.seek(io::SeekFrom::Start(0)) {
            Ok(_) => DocumentJobTurn::Complete(DocumentJobSuccess::Saved),
            Err(error) if io_would_block(&error) => blocked(file, DocumentWaitInterest::Write),
            Err(error) => DocumentJobTurn::Failed(io_error(&save.path, error)),
        },
    };
    if matches!(
        turn,
        DocumentJobTurn::Complete(_) | DocumentJobTurn::Failed(_)
    ) {
        let file = save.file.take().expect("retained save owns file");
        store.open_files.insert(save.handle, file);
    }
    turn
}

fn step_atomic_save(save: &mut AtomicSaveJob, store: &mut StdDocumentStore) -> DocumentJobTurn {
    match save.stage {
        AtomicSaveStage::CreateParent => {
            let Some(directory) = save.directories.get(save.directory_index) else {
                save.stage = AtomicSaveStage::CreateTemporary;
                return DocumentJobTurn::Progress;
            };
            match fs::create_dir(directory) {
                Ok(()) => {
                    save.directory_index += 1;
                    DocumentJobTurn::Progress
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    save.directory_index += 1;
                    DocumentJobTurn::Progress
                }
                Err(error) => DocumentJobTurn::Failed(io_error(directory, error)),
            }
        }
        AtomicSaveStage::CreateTemporary => {
            if save.temporary_attempt >= 32 {
                return DocumentJobTurn::Failed(DocumentError::Io {
                    path: save.destination.display().to_string(),
                    source: io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "no free atomic-save temporary name",
                    ),
                });
            }
            let parent = save
                .destination
                .parent()
                .expect("validated destination has parent");
            let candidate = parent.join(format!(
                ".{}.pmos-save-{}",
                save.file_name, save.temporary_attempt
            ));
            save.temporary_attempt += 1;
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => {
                    save.temporary_path = Some(candidate);
                    save.temporary = Some(file);
                    save.stage = AtomicSaveStage::Write;
                    DocumentJobTurn::Progress
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    DocumentJobTurn::Progress
                }
                Err(error) => DocumentJobTurn::Failed(io_error(&candidate, error)),
            }
        }
        AtomicSaveStage::Write => {
            let file = save.temporary.as_mut().expect("temporary file exists");
            match save.pump.step(|bytes| file.write(bytes)) {
                WritePumpTurn::Progress => DocumentJobTurn::Progress,
                WritePumpTurn::Blocked => blocked(file, DocumentWaitInterest::Write),
                WritePumpTurn::Complete => {
                    save.stage = AtomicSaveStage::SyncTemporary;
                    DocumentJobTurn::Progress
                }
                WritePumpTurn::Failed(error) => fail_atomic(save, error),
            }
        }
        AtomicSaveStage::SyncTemporary => {
            let file = save.temporary.as_mut().expect("temporary file exists");
            match file.sync_all() {
                Ok(()) => {
                    save.stage = AtomicSaveStage::Rename;
                    DocumentJobTurn::Progress
                }
                Err(error) if io_would_block(&error) => blocked(file, DocumentWaitInterest::Write),
                Err(error) => fail_atomic(save, error),
            }
        }
        AtomicSaveStage::Rename => {
            drop(save.temporary.take());
            let temporary = save
                .temporary_path
                .as_ref()
                .expect("temporary path exists")
                .clone();
            match fs::rename(&temporary, &save.destination) {
                Ok(()) => {
                    save.temporary_path = None;
                    save.stage = AtomicSaveStage::Reopen;
                    DocumentJobTurn::Progress
                }
                Err(error) => fail_atomic(save, error),
            }
        }
        AtomicSaveStage::Reopen => match OpenOptions::new()
            .read(true)
            .write(true)
            .open(&save.destination)
        {
            Ok(file) => {
                let handle = store.insert_open_file(file);
                let contents = String::from_utf8(std::mem::take(&mut save.pump.bytes))
                    .expect("editor supplied UTF-8 contents");
                DocumentJobTurn::Complete(DocumentJobSuccess::Created(OpenDocument {
                    contents,
                    handle: Some(handle),
                }))
            }
            Err(error) => DocumentJobTurn::Failed(io_error(&save.destination, error)),
        },
        AtomicSaveStage::Cleanup => {
            drop(save.temporary.take());
            let cleanup = save.temporary_path.take().map_or(Ok(()), |path| {
                fs::remove_file(&path).or_else(|error| {
                    if error.kind() == io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(io_error(&path, error))
                    }
                })
            });
            match cleanup {
                Ok(()) => DocumentJobTurn::Failed(
                    save.pending_error
                        .take()
                        .expect("atomic cleanup retains original failure"),
                ),
                Err(error) => DocumentJobTurn::Failed(error),
            }
        }
    }
}

fn blocked(file: &File, interest: DocumentWaitInterest) -> DocumentJobTurn {
    DocumentJobTurn::Blocked(DocumentWait {
        fd: file.as_raw_fd(),
        interest,
    })
}

fn fail_atomic(save: &mut AtomicSaveJob, error: io::Error) -> DocumentJobTurn {
    drop(save.temporary.take());
    save.pending_error = Some(io_error(&save.destination, error));
    save.stage = AtomicSaveStage::Cleanup;
    DocumentJobTurn::Progress
}

impl Drop for DocumentJob {
    fn drop(&mut self) {
        if let DocumentJobInner::AtomicSave(save) = &mut self.inner {
            drop(save.temporary.take());
            if let Some(path) = save.temporary_path.take() {
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferTooLarge {
    pub bytes: usize,
    pub max: usize,
}

impl fmt::Display for BufferTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "document is {} bytes; limit is {} bytes",
            self.bytes, self.max
        )
    }
}

/// In-memory UTF-8 document with an insertion caret and a hard byte ceiling.
#[derive(Debug, Clone)]
pub struct EditBuffer {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    byte_len: usize,
    dirty: bool,
    revision: u64,
}

impl EditBuffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            byte_len: 0,
            dirty: false,
            revision: 0,
        }
    }

    pub fn try_from_text(text: &str) -> Result<Self, BufferTooLarge> {
        if text.len() > MAX_DOCUMENT_BYTES {
            return Err(BufferTooLarge {
                bytes: text.len(),
                max: MAX_DOCUMENT_BYTES,
            });
        }
        let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Ok(Self {
            lines,
            cursor_line: 0,
            cursor_col: 0,
            byte_len: text.len(),
            dirty: false,
            revision: 0,
        })
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn byte_len(&self) -> usize {
        self.byte_len
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_line, self.cursor_col)
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn document_text(&self) -> String {
        self.lines.join("\n")
    }

    /// Insert a character, returning false without mutation at the size cap.
    pub fn insert_char(&mut self, ch: char) -> bool {
        if ch == '\n' {
            return self.insert_newline();
        }
        if self.byte_len.saturating_add(ch.len_utf8()) > MAX_DOCUMENT_BYTES {
            return false;
        }
        let line = &mut self.lines[self.cursor_line];
        let byte_idx = char_idx_to_byte_idx(line, self.cursor_col);
        line.insert(byte_idx, ch);
        self.cursor_col += 1;
        self.byte_len += ch.len_utf8();
        self.dirty = true;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Split the current line, returning false without mutation at the cap.
    pub fn insert_newline(&mut self) -> bool {
        if self.byte_len >= MAX_DOCUMENT_BYTES {
            return false;
        }
        let line = &mut self.lines[self.cursor_line];
        let byte_idx = char_idx_to_byte_idx(line, self.cursor_col);
        let tail = line.split_off(byte_idx);
        self.lines.insert(self.cursor_line + 1, tail);
        self.cursor_line += 1;
        self.cursor_col = 0;
        self.byte_len += 1;
        self.dirty = true;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_line];
            let target = self.cursor_col - 1;
            let byte_idx = char_idx_to_byte_idx(line, target);
            let next_byte = char_idx_to_byte_idx(line, self.cursor_col);
            self.byte_len -= next_byte - byte_idx;
            line.replace_range(byte_idx..next_byte, "");
            self.cursor_col = target;
            self.dirty = true;
            self.revision = self.revision.wrapping_add(1);
        } else if self.cursor_line > 0 {
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            let previous = &mut self.lines[self.cursor_line];
            let new_col = previous.chars().count();
            previous.push_str(&current);
            self.cursor_col = new_col;
            self.byte_len -= 1;
            self.dirty = true;
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let line_len = self.lines[self.cursor_line].chars().count();
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        } else if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor_line > 0 {
            self.cursor_line -= 1;
            let len = self.lines[self.cursor_line].chars().count();
            self.cursor_col = min(self.cursor_col, len);
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            let len = self.lines[self.cursor_line].chars().count();
            self.cursor_col = min(self.cursor_col, len);
        }
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    pub fn mark_saved_if_revision(&mut self, revision: u64) -> bool {
        if self.revision == revision {
            self.mark_saved();
            true
        } else {
            false
        }
    }
}

impl Default for EditBuffer {
    fn default() -> Self {
        Self::new()
    }
}

fn char_idx_to_byte_idx(text: &str, character: usize) -> usize {
    text.char_indices()
        .nth(character)
        .map_or(text.len(), |(byte, _)| byte)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAction {
    Open,
    SaveAs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAction {
    Close,
    New,
    Open,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorMode {
    Editing,
    Path {
        action: PathAction,
        input: String,
        after_save: Option<PendingAction>,
    },
    ConfirmDiscard(PendingAction),
    Busy(EditorBusy),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorBusy {
    Opening,
    Saving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorInput {
    Character(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    Left,
    Right,
    Up,
    Down,
    New,
    Open,
    Save,
    SaveAs,
    RequestClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorEffect {
    Continue,
    Close,
}

#[derive(Debug)]
pub enum EditorStepwiseEffect {
    Continue,
    Close,
    Started(Box<EditorIoJob>),
}

#[derive(Debug, Clone)]
enum EditorIoContext {
    Open {
        path: String,
        restore_mode: EditorMode,
    },
    Save {
        path: String,
        bytes: usize,
        revision: u64,
        after_save: Option<PendingAction>,
        restore_mode: EditorMode,
        atomic: bool,
    },
}

#[derive(Debug)]
pub struct EditorIoJob {
    context: Option<EditorIoContext>,
    document: Option<DocumentJob>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EditorIoTurn {
    Progress,
    Blocked(DocumentWait),
    Complete(EditorEffect),
}

impl EditorIoJob {
    pub fn step(
        &mut self,
        session: &mut EditorSession,
        store: &mut StdDocumentStore,
    ) -> EditorIoTurn {
        let turn = self
            .document
            .as_mut()
            .expect("active editor I/O owns a document job")
            .step(store);
        match turn {
            DocumentJobTurn::Progress => EditorIoTurn::Progress,
            DocumentJobTurn::Blocked(wait) => EditorIoTurn::Blocked(wait),
            DocumentJobTurn::Complete(success) => {
                self.document = None;
                let context = self.context.take().expect("active editor I/O context");
                EditorIoTurn::Complete(session.finish_editor_io(context, Ok(success), store))
            }
            DocumentJobTurn::Failed(error) => {
                self.document = None;
                let context = self.context.take().expect("active editor I/O context");
                EditorIoTurn::Complete(session.finish_editor_io(context, Err(error), store))
            }
        }
    }
}

#[derive(Debug)]
pub struct EditorSession {
    path: Option<String>,
    open_handle: Option<DocumentHandle>,
    buffer: EditBuffer,
    mode: EditorMode,
    status: String,
}

impl EditorSession {
    pub fn new() -> Self {
        Self {
            path: None,
            open_handle: None,
            buffer: EditBuffer::new(),
            mode: EditorMode::Editing,
            status: "new document".to_string(),
        }
    }

    pub fn new_at(path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            path: Some(path.clone()),
            open_handle: None,
            buffer: EditBuffer::new(),
            mode: EditorMode::Editing,
            status: format!("new file {path}"),
        }
    }

    pub fn from_document(path: impl Into<String>, text: &str) -> Result<Self, BufferTooLarge> {
        let path = path.into();
        Ok(Self {
            path: Some(path.clone()),
            open_handle: None,
            buffer: EditBuffer::try_from_text(text)?,
            mode: EditorMode::Editing,
            status: format!("opened {path} bytes={}", text.len()),
        })
    }

    pub fn from_open_document(
        path: impl Into<String>,
        document: OpenDocument,
    ) -> Result<Self, BufferTooLarge> {
        let path = path.into();
        Ok(Self {
            path: Some(path.clone()),
            open_handle: document.handle,
            buffer: EditBuffer::try_from_text(&document.contents)?,
            mode: EditorMode::Editing,
            status: format!("opened {path} bytes={}", document.contents.len()),
        })
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    pub fn document_label(&self) -> &str {
        self.path.as_deref().unwrap_or("Untitled")
    }

    pub fn has_open_handle(&self) -> bool {
        self.open_handle.is_some()
    }

    pub fn buffer(&self) -> &EditBuffer {
        &self.buffer
    }

    pub fn mode(&self) -> &EditorMode {
        &self.mode
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.status = format!("error: {}", message.into());
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = message.into();
    }

    pub fn handle(&mut self, input: EditorInput, store: &mut impl DocumentStore) -> EditorEffect {
        if input == EditorInput::RequestClose {
            return self.request_close(store);
        }

        let active_mode = std::mem::replace(&mut self.mode, EditorMode::Editing);
        match active_mode {
            EditorMode::Editing => self.handle_editing(input, store),
            EditorMode::Path {
                action,
                input: path_input,
                after_save,
            } => self.handle_path(input, action, path_input, after_save, store),
            EditorMode::ConfirmDiscard(pending) => self.handle_confirmation(input, pending, store),
            EditorMode::Busy(busy) => {
                self.mode = EditorMode::Busy(busy);
                EditorEffect::Continue
            }
        }
    }

    /// Handle one GUI input while turning any document I/O into a bounded job.
    /// Non-I/O transitions continue to use the same lifecycle implementation
    /// as native/CLI callers.
    pub fn handle_stepwise(
        &mut self,
        input: EditorInput,
        store: &mut StdDocumentStore,
    ) -> EditorStepwiseEffect {
        if matches!(self.mode, EditorMode::Busy(_)) {
            return EditorStepwiseEffect::Continue;
        }

        let mode = self.mode.clone();
        let request = match (&mode, input) {
            (EditorMode::Editing, EditorInput::Save) => self.path.clone().map(|path| {
                let restore_mode = EditorMode::Editing;
                self.prepare_save(path, None, restore_mode, false, store)
            }),
            (EditorMode::ConfirmDiscard(pending), EditorInput::Character(character))
                if character.eq_ignore_ascii_case(&'s') =>
            {
                self.path.clone().map(|path| {
                    self.prepare_save(
                        path,
                        Some(*pending),
                        EditorMode::ConfirmDiscard(*pending),
                        false,
                        store,
                    )
                })
            }
            (EditorMode::ConfirmDiscard(pending), EditorInput::Save) => {
                self.path.clone().map(|path| {
                    self.prepare_save(
                        path,
                        Some(*pending),
                        EditorMode::ConfirmDiscard(*pending),
                        false,
                        store,
                    )
                })
            }
            (
                EditorMode::Path {
                    action,
                    input: path_input,
                    after_save,
                },
                EditorInput::Enter,
            ) => {
                let selected = path_input.trim().to_string();
                if selected.is_empty() {
                    None
                } else {
                    let restore_mode = mode.clone();
                    Some(match action {
                        PathAction::Open => self.prepare_open(selected, restore_mode, store),
                        PathAction::SaveAs => {
                            self.prepare_save(selected, *after_save, restore_mode, true, store)
                        }
                    })
                }
            }
            _ => None,
        };

        if let Some(request) = request {
            return request;
        }

        match self.handle(input, store) {
            EditorEffect::Continue => EditorStepwiseEffect::Continue,
            EditorEffect::Close => EditorStepwiseEffect::Close,
        }
    }

    fn prepare_open(
        &mut self,
        path: String,
        restore_mode: EditorMode,
        store: &mut StdDocumentStore,
    ) -> EditorStepwiseEffect {
        match store.start_open(&path) {
            Ok(document) => {
                self.mode = EditorMode::Busy(EditorBusy::Opening);
                self.status = format!("opening {path}");
                EditorStepwiseEffect::Started(Box::new(EditorIoJob {
                    context: Some(EditorIoContext::Open { path, restore_mode }),
                    document: Some(document),
                }))
            }
            Err(error) => {
                self.mode = restore_mode;
                self.status = format!("error: open failed: {error}");
                EditorStepwiseEffect::Continue
            }
        }
    }

    fn prepare_save(
        &mut self,
        path: String,
        after_save: Option<PendingAction>,
        restore_mode: EditorMode,
        atomic: bool,
        store: &mut StdDocumentStore,
    ) -> EditorStepwiseEffect {
        let contents = self.buffer.document_text();
        let bytes = contents.len();
        let revision = self.buffer.revision();
        let use_atomic = atomic || self.open_handle.is_none();
        let result = match (use_atomic, self.open_handle) {
            (true, _) => store.start_atomic_save(&path, contents),
            (false, Some(handle)) => store.start_retained_save(handle, &path, contents),
            (false, None) => unreachable!("non-atomic save requires an open handle"),
        };
        match result {
            Ok(document) => {
                self.mode = EditorMode::Busy(EditorBusy::Saving);
                self.status = format!("saving {path}");
                EditorStepwiseEffect::Started(Box::new(EditorIoJob {
                    context: Some(EditorIoContext::Save {
                        path,
                        bytes,
                        revision,
                        after_save,
                        restore_mode,
                        atomic: use_atomic,
                    }),
                    document: Some(document),
                }))
            }
            Err(error) => {
                self.mode = restore_mode;
                self.status = format!("error: save failed: {error}");
                EditorStepwiseEffect::Continue
            }
        }
    }

    /// Keep display input live while a save progresses. Editing a saving
    /// snapshot advances the buffer revision, so completion cannot clear the
    /// newer dirty state. Open jobs accept Escape/close cancellation; other
    /// input receives immediate busy feedback rather than blocking dispatch.
    pub fn handle_during_io(
        &mut self,
        input: EditorInput,
        active: &mut Option<EditorIoJob>,
        store: &mut StdDocumentStore,
    ) -> EditorEffect {
        let Some(context) = active
            .as_ref()
            .and_then(|job| job.context.as_ref())
            .cloned()
        else {
            return EditorEffect::Continue;
        };
        match context {
            EditorIoContext::Save { path, .. } => match input {
                EditorInput::Character(character) => self.insert(character),
                EditorInput::Enter => self.insert('\n'),
                EditorInput::Tab => self.insert('\t'),
                EditorInput::Backspace => {
                    self.buffer.backspace();
                }
                EditorInput::Left => self.buffer.move_left(),
                EditorInput::Right => self.buffer.move_right(),
                EditorInput::Up => self.buffer.move_up(),
                EditorInput::Down => self.buffer.move_down(),
                EditorInput::RequestClose => {
                    let pending = set_io_after_save(active, PendingAction::Close);
                    self.status = format!("saving {path} before {}", pending.label());
                }
                EditorInput::New => {
                    let pending = set_io_after_save(active, PendingAction::New);
                    self.status = format!("saving {path} before {}", pending.label());
                }
                EditorInput::Open => {
                    let pending = set_io_after_save(active, PendingAction::Open);
                    self.status = format!("saving {path} before {}", pending.label());
                }
                EditorInput::Save | EditorInput::SaveAs | EditorInput::Escape => {
                    self.status = format!("saving {path}");
                }
            },
            EditorIoContext::Open {
                path, restore_mode, ..
            } => match input {
                EditorInput::Escape | EditorInput::RequestClose => {
                    let mut job = active.take().expect("active editor I/O job");
                    let document = job.document.take().expect("active document job");
                    match store.cancel_job(document) {
                        Ok(()) => {
                            self.mode = restore_mode.clone();
                            self.status = "open cancelled".to_string();
                            if input == EditorInput::RequestClose {
                                return self.request_close(store);
                            }
                        }
                        Err(error) => {
                            self.status = format!("error: open cancellation failed: {error}");
                        }
                    }
                }
                _ => self.status = format!("opening {path}; Escape cancels"),
            },
        }
        EditorEffect::Continue
    }

    fn finish_editor_io(
        &mut self,
        context: EditorIoContext,
        result: Result<DocumentJobSuccess, DocumentError>,
        store: &mut StdDocumentStore,
    ) -> EditorEffect {
        match context {
            EditorIoContext::Open { path, restore_mode } => match result {
                Ok(DocumentJobSuccess::Opened(opened)) => {
                    let bytes = opened.contents.len();
                    match EditBuffer::try_from_text(&opened.contents) {
                        Ok(buffer) => {
                            self.replace_open_handle(opened.handle, store);
                            self.path = Some(path.clone());
                            self.buffer = buffer;
                            self.mode = EditorMode::Editing;
                            self.status = format!("opened {path} bytes={bytes}");
                        }
                        Err(error) => {
                            if let Some(handle) = opened.handle {
                                store.close_document(handle);
                            }
                            self.mode = restore_mode;
                            self.status = format!("error: open failed: {error}");
                        }
                    }
                    EditorEffect::Continue
                }
                Ok(_) => {
                    self.mode = restore_mode;
                    self.status = "error: open failed: invalid document job result".to_string();
                    EditorEffect::Continue
                }
                Err(error) => {
                    self.mode = restore_mode;
                    self.status = format!("error: open failed: {error}");
                    EditorEffect::Continue
                }
            },
            EditorIoContext::Save {
                path,
                bytes,
                revision,
                after_save,
                restore_mode,
                atomic,
            } => match result {
                Ok(success) => {
                    match success {
                        DocumentJobSuccess::Created(opened) if atomic => {
                            self.replace_open_handle(opened.handle, store);
                            self.path = Some(path.clone());
                        }
                        DocumentJobSuccess::Saved if !atomic => {}
                        _ => {
                            self.mode = restore_mode;
                            self.status =
                                "error: save failed: invalid document job result".to_string();
                            return EditorEffect::Continue;
                        }
                    }
                    let saved_current_revision = self.buffer.mark_saved_if_revision(revision);
                    self.status = if saved_current_revision {
                        format!("saved {path} bytes={bytes}")
                    } else {
                        format!("saved {path} bytes={bytes}; newer changes pending")
                    };
                    self.mode = EditorMode::Editing;
                    if let Some(pending) = after_save {
                        if saved_current_revision {
                            self.continue_pending(pending, store)
                        } else {
                            self.mode = EditorMode::ConfirmDiscard(pending);
                            EditorEffect::Continue
                        }
                    } else {
                        EditorEffect::Continue
                    }
                }
                Err(error) => {
                    self.mode = restore_mode;
                    self.status = format!("error: save failed: {error}");
                    EditorEffect::Continue
                }
            },
        }
    }

    fn handle_editing(
        &mut self,
        input: EditorInput,
        store: &mut impl DocumentStore,
    ) -> EditorEffect {
        match input {
            EditorInput::Character(character) => self.insert(character),
            EditorInput::Enter => self.insert('\n'),
            EditorInput::Tab => self.insert('\t'),
            EditorInput::Backspace => {
                self.buffer.backspace();
                if self.buffer.dirty() {
                    self.status = "modified".to_string();
                }
            }
            EditorInput::Left => self.buffer.move_left(),
            EditorInput::Right => self.buffer.move_right(),
            EditorInput::Up => self.buffer.move_up(),
            EditorInput::Down => self.buffer.move_down(),
            EditorInput::New => self.guard_or_continue(PendingAction::New, store),
            EditorInput::Open => self.guard_or_continue(PendingAction::Open, store),
            EditorInput::Save => {
                let _ = self.save(None, store);
            }
            EditorInput::SaveAs => self.show_path(PathAction::SaveAs, None),
            EditorInput::Escape | EditorInput::RequestClose => {}
        }
        EditorEffect::Continue
    }

    fn handle_path(
        &mut self,
        input: EditorInput,
        action: PathAction,
        mut path_input: String,
        after_save: Option<PendingAction>,
        store: &mut impl DocumentStore,
    ) -> EditorEffect {
        match input {
            EditorInput::Character(character) => {
                if path_input.len().saturating_add(character.len_utf8()) <= MAX_PATH_BYTES {
                    path_input.push(character);
                } else {
                    self.status = format!("error: path exceeds {MAX_PATH_BYTES} bytes");
                }
                self.mode = EditorMode::Path {
                    action,
                    input: path_input,
                    after_save,
                };
            }
            EditorInput::Backspace => {
                path_input.pop();
                self.mode = EditorMode::Path {
                    action,
                    input: path_input,
                    after_save,
                };
            }
            EditorInput::Enter => {
                let selected = path_input.trim().to_string();
                if selected.is_empty() {
                    self.status = "error: path is required".to_string();
                    self.mode = EditorMode::Path {
                        action,
                        input: path_input,
                        after_save,
                    };
                } else {
                    return match action {
                        PathAction::Open => self.open_selected(&selected, store),
                        PathAction::SaveAs => {
                            if self.save_as(&selected, store) {
                                match after_save {
                                    Some(pending) => self.continue_pending(pending, store),
                                    None => EditorEffect::Continue,
                                }
                            } else {
                                self.mode = EditorMode::Path {
                                    action,
                                    input: path_input,
                                    after_save,
                                };
                                EditorEffect::Continue
                            }
                        }
                    };
                }
            }
            EditorInput::Escape => {
                self.status = match after_save {
                    Some(PendingAction::Close) => "close cancelled".to_string(),
                    Some(PendingAction::New) => "new cancelled".to_string(),
                    Some(PendingAction::Open) => "open cancelled".to_string(),
                    None => "dialog cancelled".to_string(),
                };
            }
            _ => {
                self.mode = EditorMode::Path {
                    action,
                    input: path_input,
                    after_save,
                };
            }
        }
        EditorEffect::Continue
    }

    fn handle_confirmation(
        &mut self,
        input: EditorInput,
        pending: PendingAction,
        store: &mut impl DocumentStore,
    ) -> EditorEffect {
        let choice = match input {
            EditorInput::Character(character) => character.to_ascii_lowercase(),
            EditorInput::Save => 's',
            EditorInput::Escape => 'c',
            _ => {
                self.mode = EditorMode::ConfirmDiscard(pending);
                return EditorEffect::Continue;
            }
        };

        match choice {
            's' => self.save(Some(pending), store),
            'd' => {
                self.status = match pending {
                    PendingAction::Close => "discarded changes; closing".to_string(),
                    PendingAction::New => "discarded changes; new document".to_string(),
                    PendingAction::Open => "discarded changes; open document".to_string(),
                };
                self.continue_pending(pending, store)
            }
            'c' => {
                self.status = match pending {
                    PendingAction::Close => "close cancelled".to_string(),
                    PendingAction::New => "new cancelled".to_string(),
                    PendingAction::Open => "open cancelled".to_string(),
                };
                EditorEffect::Continue
            }
            _ => {
                self.mode = EditorMode::ConfirmDiscard(pending);
                EditorEffect::Continue
            }
        }
    }

    fn insert(&mut self, character: char) {
        if self.buffer.insert_char(character) {
            self.status = "modified".to_string();
        } else {
            self.status = format!("error: document limit is {MAX_DOCUMENT_BYTES} bytes");
        }
    }

    fn request_close(&mut self, store: &mut impl DocumentStore) -> EditorEffect {
        if self.buffer.dirty() {
            self.mode = EditorMode::ConfirmDiscard(PendingAction::Close);
            self.status = "unsaved changes: save, discard, or cancel".to_string();
            EditorEffect::Continue
        } else {
            self.status = "closing".to_string();
            self.close_open_handle(store);
            EditorEffect::Close
        }
    }

    fn guard_or_continue(&mut self, pending: PendingAction, store: &mut impl DocumentStore) {
        if self.buffer.dirty() {
            self.mode = EditorMode::ConfirmDiscard(pending);
            self.status = "unsaved changes: save, discard, or cancel".to_string();
        } else {
            let _ = self.continue_pending(pending, store);
        }
    }

    fn continue_pending(
        &mut self,
        pending: PendingAction,
        store: &mut impl DocumentStore,
    ) -> EditorEffect {
        match pending {
            PendingAction::Close => {
                self.close_open_handle(store);
                EditorEffect::Close
            }
            PendingAction::New => {
                self.close_open_handle(store);
                *self = Self::new();
                EditorEffect::Continue
            }
            PendingAction::Open => {
                self.show_path(PathAction::Open, None);
                EditorEffect::Continue
            }
        }
    }

    fn show_path(&mut self, action: PathAction, after_save: Option<PendingAction>) {
        self.mode = EditorMode::Path {
            action,
            input: self.initial_directory(),
            after_save,
        };
        self.status = match action {
            PathAction::Open => "open: enter a VFS path".to_string(),
            PathAction::SaveAs => "save as: enter a VFS path".to_string(),
        };
    }

    fn initial_directory(&self) -> String {
        let parent = self
            .path
            .as_deref()
            .and_then(|path| Path::new(path).parent())
            .filter(|parent| !parent.as_os_str().is_empty())
            .and_then(Path::to_str)
            .unwrap_or(DEFAULT_DOCUMENT_DIRECTORY);
        format!("{}/", parent.trim_end_matches('/'))
    }

    fn save(
        &mut self,
        after_save: Option<PendingAction>,
        store: &mut impl DocumentStore,
    ) -> EditorEffect {
        let Some(path) = self.path.clone() else {
            self.show_path(PathAction::SaveAs, after_save);
            return EditorEffect::Continue;
        };
        if !self.save_current(&path, store) {
            if let Some(pending) = after_save {
                self.mode = EditorMode::ConfirmDiscard(pending);
            }
            return EditorEffect::Continue;
        }
        match after_save {
            Some(pending) => self.continue_pending(pending, store),
            None => EditorEffect::Continue,
        }
    }

    fn save_current(&mut self, path: &str, store: &mut impl DocumentStore) -> bool {
        let text = self.buffer.document_text();
        let result = if let Some(handle) = self.open_handle {
            store
                .write_open_document(handle, path, &text)
                .map(|()| None)
        } else {
            store.create_document(path, &text).map(Some)
        };
        match result {
            Ok(opened) => {
                if let Some(opened) = opened {
                    self.replace_open_handle(opened.handle, store);
                }
                self.path = Some(path.to_string());
                self.buffer.mark_saved();
                self.status = format!("saved {path} bytes={}", text.len());
                true
            }
            Err(error) => {
                self.status = format!("error: save failed: {error}");
                false
            }
        }
    }

    fn save_as(&mut self, path: &str, store: &mut impl DocumentStore) -> bool {
        let text = self.buffer.document_text();
        match store.create_document(path, &text) {
            Ok(opened) => {
                self.replace_open_handle(opened.handle, store);
                self.path = Some(path.to_string());
                self.buffer.mark_saved();
                self.status = format!("saved {path} bytes={}", text.len());
                true
            }
            Err(error) => {
                self.status = format!("error: save failed: {error}");
                false
            }
        }
    }

    fn open_selected(&mut self, path: &str, store: &mut impl DocumentStore) -> EditorEffect {
        match store.open_document(path) {
            Ok(opened) => match EditBuffer::try_from_text(&opened.contents) {
                Ok(buffer) => {
                    self.replace_open_handle(opened.handle, store);
                    self.path = Some(path.to_string());
                    self.buffer = buffer;
                    self.status = format!("opened {path} bytes={}", opened.contents.len());
                }
                Err(error) => {
                    if let Some(handle) = opened.handle {
                        store.close_document(handle);
                    }
                    self.status = format!("error: open failed: {error}");
                    self.mode = EditorMode::Path {
                        action: PathAction::Open,
                        input: path.to_string(),
                        after_save: None,
                    };
                }
            },
            Err(error) => {
                self.status = format!("error: open failed: {error}");
                self.mode = EditorMode::Path {
                    action: PathAction::Open,
                    input: path.to_string(),
                    after_save: None,
                };
            }
        }
        EditorEffect::Continue
    }

    fn replace_open_handle(
        &mut self,
        replacement: Option<DocumentHandle>,
        store: &mut impl DocumentStore,
    ) {
        if self.open_handle != replacement {
            if let Some(handle) = self.open_handle.take() {
                store.close_document(handle);
            }
            self.open_handle = replacement;
        }
    }

    fn close_open_handle(&mut self, store: &mut impl DocumentStore) {
        if let Some(handle) = self.open_handle.take() {
            store.close_document(handle);
        }
    }
}

impl Default for EditorSession {
    fn default() -> Self {
        Self::new()
    }
}

fn set_io_after_save(active: &mut Option<EditorIoJob>, pending: PendingAction) -> PendingAction {
    if let Some(EditorIoContext::Save { after_save, .. }) =
        active.as_mut().and_then(|job| job.context.as_mut())
    {
        *after_save.get_or_insert(pending)
    } else {
        pending
    }
}

impl PendingAction {
    fn label(self) -> &'static str {
        match self {
            Self::Close => "close",
            Self::New => "new document",
            Self::Open => "open",
        }
    }
}

#[cfg(test)]
mod stepwise_pump_tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn write_pump_keeps_the_exact_suffix_across_partial_and_eagain_turns() {
        let body = vec![0x5a; DOCUMENT_IO_CHUNK_BYTES + 97];
        let mut pump = WritePump::new(body.clone());
        let mut script = VecDeque::from([
            Ok(31_usize),
            Err(io::Error::from(io::ErrorKind::WouldBlock)),
            Ok(DOCUMENT_IO_CHUNK_BYTES - 31),
            Ok(97),
        ]);
        let mut written = Vec::new();
        let mut offered = Vec::new();
        loop {
            let turn = pump.step(|bytes| {
                offered.push(bytes.len());
                match script.pop_front().expect("scripted write turn") {
                    Ok(count) => {
                        written.extend_from_slice(&bytes[..count]);
                        Ok(count)
                    }
                    Err(error) => Err(error),
                }
            });
            match turn {
                WritePumpTurn::Progress | WritePumpTurn::Blocked => {}
                WritePumpTurn::Complete => break,
                WritePumpTurn::Failed(error) => panic!("unexpected write failure: {error}"),
            }
        }
        assert_eq!(written, body);
        assert_eq!(
            offered,
            [
                DOCUMENT_IO_CHUNK_BYTES,
                DOCUMENT_IO_CHUNK_BYTES,
                DOCUMENT_IO_CHUNK_BYTES,
                97,
            ]
        );
        assert!(offered
            .into_iter()
            .all(|bytes| bytes <= DOCUMENT_IO_CHUNK_BYTES));
    }

    #[test]
    fn read_pump_yields_on_eagain_and_never_offers_more_than_one_quantum() {
        let body = vec![0x41; DOCUMENT_IO_CHUNK_BYTES + 7];
        let mut source_offset = 0_usize;
        let mut blocked_once = false;
        let mut offered = Vec::new();
        let mut pump = ReadPump::default();
        loop {
            let turn = pump.step(|destination| {
                offered.push(destination.len());
                if !blocked_once {
                    blocked_once = true;
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                let count = destination.len().min(body.len() - source_offset);
                destination[..count].copy_from_slice(&body[source_offset..source_offset + count]);
                source_offset += count;
                Ok(count)
            });
            match turn {
                ReadPumpTurn::Progress | ReadPumpTurn::Blocked => {}
                ReadPumpTurn::Eof => break,
                ReadPumpTurn::Failed(error) => panic!("unexpected read failure: {error}"),
            }
        }
        assert_eq!(pump.bytes, body);
        assert!(offered
            .into_iter()
            .all(|bytes| bytes <= DOCUMENT_IO_CHUNK_BYTES));
    }
}
