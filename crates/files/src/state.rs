use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PREVIEW_LIMIT_BYTES: u64 = 32 * 1024;
pub const DIRECTORY_ENTRIES_PER_STEP: usize = 16;
pub const DIRECTORY_ENTRY_LIMIT: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    CreateFolder,
    Rename,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPreview {
    pub path: PathBuf,
    pub lines: Vec<String>,
    pub scroll: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewMode {
    Browse,
    Input {
        kind: DialogKind,
        value: String,
        original_name: Option<String>,
        replace_on_type: bool,
    },
    ConfirmDelete {
        name: String,
        is_dir: bool,
    },
    Preview(TextPreview),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiKey {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Delete,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerTarget {
    Entry(usize),
    Parent,
    NewFolder,
    Rename,
    Delete,
    Refresh,
    Open,
    Preview,
    Import,
    Export,
    ScrollUp,
    ScrollDown,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAction {
    Navigate {
        path: PathBuf,
        select_name: Option<String>,
    },
    Preview(PathBuf),
    OpenDefault(PathBuf),
    CreateFolder(PathBuf),
    Rename {
        old_path: PathBuf,
        new_path: PathBuf,
    },
    Delete {
        path: PathBuf,
        is_dir: bool,
    },
    Refresh,
    RequestHostImport,
    RequestHostExport(PathBuf),
    Close,
}

/// Pure double-activation recognizer for list rows. Files feeds it monotonic
/// elapsed time only on primary-button presses, so it neither polls nor wakes
/// the process while idle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoubleActivation {
    last: Option<(usize, Duration)>,
}

impl DoubleActivation {
    pub const MAX_INTERVAL: Duration = Duration::from_millis(500);

    pub fn press(&mut self, index: usize, now: Duration) -> bool {
        let activated = self.last.is_some_and(|(previous, at)| {
            previous == index && now.saturating_sub(at) <= Self::MAX_INTERVAL
        });
        self.last = if activated { None } else { Some((index, now)) };
        activated
    }

    pub fn cancel(&mut self) {
        self.last = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionOutcome {
    pub close: bool,
    pub changed: bool,
    pub log: Option<String>,
}

pub trait FileSystem {
    fn read_directory(&self, path: &Path) -> io::Result<Vec<FileEntry>>;
    fn begin_directory_scan(&self, _path: &Path) -> io::Result<Option<DirectoryScanner>> {
        Ok(None)
    }
    fn create_folder(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, old_path: &Path, new_path: &Path) -> io::Result<()>;
    fn delete(&self, path: &Path, is_dir: bool) -> io::Result<()>;
    fn text_preview(&self, path: &Path) -> io::Result<TextPreview>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct StdFileSystem;

impl FileSystem for StdFileSystem {
    fn read_directory(&self, path: &Path) -> io::Result<Vec<FileEntry>> {
        read_directory(path)
    }

    fn begin_directory_scan(&self, path: &Path) -> io::Result<Option<DirectoryScanner>> {
        DirectoryScanner::start(path).map(Some)
    }

    fn create_folder(&self, path: &Path) -> io::Result<()> {
        fs::create_dir(path)
    }

    fn rename(&self, old_path: &Path, new_path: &Path) -> io::Result<()> {
        fs::rename(old_path, new_path)
    }

    fn delete(&self, path: &Path, is_dir: bool) -> io::Result<()> {
        if is_dir {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        }
    }

    fn text_preview(&self, path: &Path) -> io::Result<TextPreview> {
        load_text_preview(path)
    }
}

pub enum DirectoryScanStep {
    Pending,
    Complete(io::Result<Vec<FileEntry>>),
}

/// Incremental directory reader used by the production GUI.
pub struct DirectoryScanner {
    entries: fs::ReadDir,
    collected: Vec<FileEntry>,
}

impl DirectoryScanner {
    pub fn start(path: &Path) -> io::Result<Self> {
        Ok(Self {
            entries: fs::read_dir(path)?,
            collected: Vec::new(),
        })
    }

    /// Inspect at most 16 entries and keep the stable result private until EOF.
    pub fn step(&mut self) -> DirectoryScanStep {
        for _ in 0..DIRECTORY_ENTRIES_PER_STEP {
            let Some(result) = self.entries.next() else {
                return DirectoryScanStep::Complete(Ok(core::mem::take(&mut self.collected)));
            };
            let entry = match result {
                Ok(entry) => entry,
                Err(error) => return DirectoryScanStep::Complete(Err(error)),
            };
            if self.collected.len() >= DIRECTORY_ENTRY_LIMIT {
                return DirectoryScanStep::Complete(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory exceeds 4096-entry GUI limit",
                )));
            }
            let file_entry = match entry.file_type() {
                Ok(file_type) => FileEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    is_dir: file_type.is_dir(),
                },
                Err(error) => return DirectoryScanStep::Complete(Err(error)),
            };
            let at = self
                .collected
                .binary_search_by(|candidate| compare_file_entries(candidate, &file_entry))
                .unwrap_or_else(|at| at);
            self.collected.insert(at, file_entry);
        }
        DirectoryScanStep::Pending
    }

    pub fn collected_len(&self) -> usize {
        self.collected.len()
    }
}

pub struct PendingDirectoryAction {
    scanner: DirectoryScanner,
    path: PathBuf,
    select_name: Option<String>,
    status: String,
    log: String,
    failure_operation: &'static str,
}

pub enum StepwiseAction {
    Complete(ActionOutcome),
    Pending(PendingDirectoryAction),
}

impl ActionOutcome {
    fn changed(log: String) -> Self {
        Self {
            close: false,
            changed: true,
            log: Some(log),
        }
    }

    fn close() -> Self {
        Self {
            close: true,
            changed: false,
            log: Some("close requested".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileManagerState {
    cwd: PathBuf,
    entries: Vec<FileEntry>,
    selected: Option<usize>,
    scroll: usize,
    mode: ViewMode,
    status: String,
}

impl FileManagerState {
    pub fn from_directory(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::from_filesystem(path, &StdFileSystem)
    }

    pub fn from_filesystem(
        path: impl AsRef<Path>,
        filesystem: &dyn FileSystem,
    ) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let entries = filesystem.read_directory(&path)?;
        let selected = (!entries.is_empty()).then_some(0);
        Ok(Self {
            cwd: path,
            entries,
            selected,
            scroll: 0,
            mode: ViewMode::Browse,
            status: "Ready".to_string(),
        })
    }

    pub fn from_entries(path: impl Into<PathBuf>, entries: Vec<FileEntry>) -> Self {
        let selected = (!entries.is_empty()).then_some(0);
        Self {
            cwd: path.into(),
            entries,
            selected,
            scroll: 0,
            mode: ViewMode::Browse,
            status: "Ready".to_string(),
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn entries(&self) -> &[FileEntry] {
        &self.entries
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn selected_entry(&self) -> Option<&FileEntry> {
        self.selected.and_then(|index| self.entries.get(index))
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn mode(&self) -> &ViewMode {
        &self.mode
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn counts(&self) -> (usize, usize) {
        self.entries.iter().fold((0, 0), |(dirs, files), entry| {
            if entry.is_dir {
                (dirs + 1, files)
            } else {
                (dirs, files + 1)
            }
        })
    }

    /// Record that the browser picker was accepted. Completion remains
    /// asynchronous and arrives through `/run/host-files`.
    pub fn host_import_pending(&mut self) {
        self.status = "Waiting for host file selection...".to_string();
    }

    /// Refresh the directory after an imported file has been written and keep
    /// that file selected so the result is immediately visible.
    pub fn complete_host_import(
        &mut self,
        path: &Path,
        filesystem: &dyn FileSystem,
    ) -> ActionOutcome {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned);
        if path.parent() == Some(self.cwd.as_path()) {
            if let Err(error) = self.reload(name.as_deref(), filesystem) {
                return self.failure("refresh after import", path, error);
            }
        }
        self.status = format!("Imported {}", path.display());
        ActionOutcome::changed(format!("imported {}", path.display()))
    }

    /// Begin the production post-import refresh without enumerating the
    /// current directory in the host-transfer turn. The imported file is
    /// selected only when the complete replacement snapshot is published.
    pub fn begin_complete_host_import(
        &mut self,
        path: &Path,
        filesystem: &dyn FileSystem,
    ) -> StepwiseAction {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned);
        if path.parent() == Some(self.cwd.as_path()) {
            return self.begin_directory_action(
                self.cwd.clone(),
                name,
                format!("Imported {}", path.display()),
                format!("imported {}", path.display()),
                "refresh after import",
                filesystem,
            );
        }
        self.status = format!("Imported {}", path.display());
        StepwiseAction::Complete(ActionOutcome::changed(format!(
            "imported {}",
            path.display()
        )))
    }

    pub fn host_export_complete(&mut self, path: &Path) -> ActionOutcome {
        self.status = format!("Exported {}", path.display());
        ActionOutcome::changed(format!("exported {}", path.display()))
    }

    pub fn host_transfer_progress(
        &mut self,
        operation: &str,
        name: impl AsRef<str>,
        completed: usize,
        total: usize,
    ) {
        let total = total.max(completed);
        self.status = format!("{operation} {}: {completed} / {total} bytes", name.as_ref());
    }

    pub fn host_transfer_failed(&mut self, operation: &str, message: impl AsRef<str>) {
        self.status = format!("Error: {operation}: {}", message.as_ref());
    }

    pub fn default_open_started(&mut self, path: &Path, pid: i32) {
        self.status = format!("Opened {} in Edit (pid {pid})", path.display());
    }

    pub fn default_open_failed(&mut self, path: &Path, message: impl AsRef<str>) {
        self.status = format!("Error: open {}: {}", path.display(), message.as_ref());
    }

    pub fn handle_key(&mut self, key: UiKey, visible_rows: usize) -> Option<FileAction> {
        match self.mode.clone() {
            ViewMode::Browse => self.handle_browse_key(key, visible_rows),
            ViewMode::Input {
                kind,
                mut value,
                original_name,
                mut replace_on_type,
            } => match key {
                UiKey::Char(ch) if !ch.is_control() => {
                    if replace_on_type {
                        value.clear();
                        replace_on_type = false;
                    }
                    value.push(ch);
                    self.mode = ViewMode::Input {
                        kind,
                        value,
                        original_name,
                        replace_on_type,
                    };
                    None
                }
                UiKey::Backspace => {
                    if replace_on_type {
                        value.clear();
                        replace_on_type = false;
                    } else {
                        value.pop();
                    }
                    self.mode = ViewMode::Input {
                        kind,
                        value,
                        original_name,
                        replace_on_type,
                    };
                    None
                }
                UiKey::Enter => {
                    let name = match validated_child_name(&value) {
                        Ok(name) => name,
                        Err(message) => {
                            self.status = format!("Error: {message}");
                            self.mode = ViewMode::Input {
                                kind,
                                value,
                                original_name,
                                replace_on_type,
                            };
                            return None;
                        }
                    };
                    self.mode = ViewMode::Browse;
                    match kind {
                        DialogKind::CreateFolder => {
                            Some(FileAction::CreateFolder(self.cwd.join(name)))
                        }
                        DialogKind::Rename => {
                            let Some(old_name) = original_name else {
                                self.status = "Error: selection disappeared".to_string();
                                return None;
                            };
                            if old_name == name {
                                self.status = "Rename cancelled: name unchanged".to_string();
                                return None;
                            }
                            Some(FileAction::Rename {
                                old_path: self.cwd.join(old_name),
                                new_path: self.cwd.join(name),
                            })
                        }
                    }
                }
                UiKey::Escape => {
                    self.mode = ViewMode::Browse;
                    self.status = "Cancelled".to_string();
                    None
                }
                _ => {
                    self.mode = ViewMode::Input {
                        kind,
                        value,
                        original_name,
                        replace_on_type,
                    };
                    None
                }
            },
            ViewMode::ConfirmDelete { name, is_dir } => match key {
                UiKey::Enter | UiKey::Char('y') | UiKey::Char('Y') => {
                    self.mode = ViewMode::Browse;
                    Some(FileAction::Delete {
                        path: self.cwd.join(name),
                        is_dir,
                    })
                }
                UiKey::Escape | UiKey::Char('n') | UiKey::Char('N') => {
                    self.mode = ViewMode::Browse;
                    self.status = "Delete cancelled".to_string();
                    None
                }
                _ => None,
            },
            ViewMode::Preview(mut preview) => match key {
                UiKey::Escape | UiKey::Backspace => {
                    self.mode = ViewMode::Browse;
                    self.status = "Preview closed".to_string();
                    None
                }
                UiKey::Up => {
                    preview.scroll = preview.scroll.saturating_sub(1);
                    self.mode = ViewMode::Preview(preview);
                    None
                }
                UiKey::Down => {
                    preview.scroll = preview
                        .scroll
                        .saturating_add(1)
                        .min(preview.lines.len().saturating_sub(1));
                    self.mode = ViewMode::Preview(preview);
                    None
                }
                UiKey::PageUp => {
                    preview.scroll = preview.scroll.saturating_sub(visible_rows.max(1));
                    self.mode = ViewMode::Preview(preview);
                    None
                }
                UiKey::PageDown => {
                    preview.scroll = preview
                        .scroll
                        .saturating_add(visible_rows.max(1))
                        .min(preview.lines.len().saturating_sub(1));
                    self.mode = ViewMode::Preview(preview);
                    None
                }
                UiKey::Close => Some(FileAction::Close),
                _ => None,
            },
        }
    }

    pub fn handle_pointer(
        &mut self,
        target: PointerTarget,
        visible_rows: usize,
    ) -> Option<FileAction> {
        if !matches!(self.mode, ViewMode::Browse) {
            return match target {
                PointerTarget::Close => Some(FileAction::Close),
                PointerTarget::Parent if matches!(self.mode, ViewMode::Preview(_)) => {
                    self.mode = ViewMode::Browse;
                    self.status = "Preview closed".to_string();
                    None
                }
                PointerTarget::ScrollUp => {
                    if let ViewMode::Preview(preview) = &mut self.mode {
                        preview.scroll = preview.scroll.saturating_sub(1);
                    }
                    None
                }
                PointerTarget::ScrollDown => {
                    if let ViewMode::Preview(preview) = &mut self.mode {
                        preview.scroll = preview
                            .scroll
                            .saturating_add(1)
                            .min(preview.lines.len().saturating_sub(1));
                    }
                    None
                }
                _ => None,
            };
        }
        match target {
            PointerTarget::Entry(index) => {
                if index < self.entries.len() {
                    self.selected = Some(index);
                    self.ensure_selected_visible(visible_rows);
                    self.status = format!("Selected {}", self.entries[index].name);
                }
                None
            }
            PointerTarget::Parent => self.parent_action(),
            PointerTarget::NewFolder => {
                self.begin_create_folder();
                None
            }
            PointerTarget::Rename => {
                self.begin_rename();
                None
            }
            PointerTarget::Delete => {
                self.begin_delete();
                None
            }
            PointerTarget::Refresh => Some(FileAction::Refresh),
            PointerTarget::Open => self.activate_selection(),
            PointerTarget::Preview => self.preview_selection(),
            PointerTarget::Import => Some(FileAction::RequestHostImport),
            PointerTarget::Export => self.request_host_export(),
            PointerTarget::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(1);
                None
            }
            PointerTarget::ScrollDown => {
                self.scroll = self
                    .scroll
                    .saturating_add(1)
                    .min(self.entries.len().saturating_sub(visible_rows.max(1)));
                None
            }
            PointerTarget::Close => Some(FileAction::Close),
        }
    }

    pub fn execute(&mut self, action: FileAction) -> ActionOutcome {
        self.execute_with(action, &StdFileSystem)
    }

    pub fn execute_with(
        &mut self,
        action: FileAction,
        filesystem: &dyn FileSystem,
    ) -> ActionOutcome {
        match action {
            FileAction::Close => ActionOutcome::close(),
            FileAction::Navigate { path, select_name } => {
                match self.load_directory(path.clone(), select_name.as_deref(), filesystem) {
                    Ok(()) => {
                        self.status = format!("Opened {}", path.display());
                        ActionOutcome::changed(format!("cwd {}", path.display()))
                    }
                    Err(error) => self.failure("open", &path, error),
                }
            }
            FileAction::Preview(path) => match filesystem.text_preview(&path) {
                Ok(preview) => {
                    self.mode = ViewMode::Preview(preview);
                    self.status = format!("Read-only preview: {}", path.display());
                    ActionOutcome::changed(format!("preview {}", path.display()))
                }
                Err(error) => self.failure("preview", &path, error),
            },
            FileAction::OpenDefault(path) => {
                self.status = format!("Opening {}...", path.display());
                ActionOutcome {
                    close: false,
                    changed: false,
                    log: Some(format!("open requested {}", path.display())),
                }
            }
            FileAction::CreateFolder(path) => match filesystem.create_folder(&path) {
                Ok(()) => {
                    let name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned);
                    if let Err(error) = self.reload(name.as_deref(), filesystem) {
                        return self.failure("refresh", &self.cwd.clone(), error);
                    }
                    self.status = format!("Created folder {}", path.display());
                    ActionOutcome::changed(format!("created folder {}", path.display()))
                }
                Err(error) => self.failure("create folder", &path, error),
            },
            FileAction::Rename { old_path, new_path } => {
                match filesystem.rename(&old_path, &new_path) {
                    Ok(()) => {
                        let name = new_path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .map(str::to_owned);
                        if let Err(error) = self.reload(name.as_deref(), filesystem) {
                            return self.failure("refresh", &self.cwd.clone(), error);
                        }
                        self.status =
                            format!("Renamed {} to {}", old_path.display(), new_path.display());
                        ActionOutcome::changed(format!(
                            "renamed {} -> {}",
                            old_path.display(),
                            new_path.display()
                        ))
                    }
                    Err(error) => self.failure("rename", &old_path, error),
                }
            }
            FileAction::Delete { path, is_dir } => {
                let result = filesystem.delete(&path, is_dir);
                match result {
                    Ok(()) => {
                        if let Err(error) = self.reload(None, filesystem) {
                            return self.failure("refresh", &self.cwd.clone(), error);
                        }
                        self.status = format!("Deleted {}", path.display());
                        ActionOutcome::changed(format!("deleted {}", path.display()))
                    }
                    Err(error) => self.failure("delete", &path, error),
                }
            }
            FileAction::Refresh => {
                let selected = self.selected_entry().map(|entry| entry.name.clone());
                match self.reload(selected.as_deref(), filesystem) {
                    Ok(()) => {
                        self.status = format!("Refreshed {}", self.cwd.display());
                        ActionOutcome::changed(format!("refreshed {}", self.cwd.display()))
                    }
                    Err(error) => self.failure("refresh", &self.cwd.clone(), error),
                }
            }
            FileAction::RequestHostImport => ActionOutcome {
                close: false,
                changed: false,
                log: Some("host import requested".to_string()),
            },
            FileAction::RequestHostExport(path) => ActionOutcome {
                close: false,
                changed: false,
                log: Some(format!("host export requested {}", path.display())),
            },
        }
    }

    /// Start a production directory-changing action without enumerating the
    /// target directory in the input/paint turn. Test filesystems that do not
    /// opt into scanning retain the synchronous compatibility path.
    pub fn begin_stepwise_action(
        &mut self,
        action: FileAction,
        filesystem: &dyn FileSystem,
    ) -> StepwiseAction {
        match action {
            FileAction::Navigate { path, select_name } => self.begin_directory_action(
                path.clone(),
                select_name,
                format!("Opened {}", path.display()),
                format!("cwd {}", path.display()),
                "open",
                filesystem,
            ),
            FileAction::Refresh => {
                let path = self.cwd.clone();
                let selected = self.selected_entry().map(|entry| entry.name.clone());
                self.begin_directory_action(
                    path.clone(),
                    selected,
                    format!("Refreshed {}", path.display()),
                    format!("refreshed {}", path.display()),
                    "refresh",
                    filesystem,
                )
            }
            FileAction::CreateFolder(path) => match filesystem.create_folder(&path) {
                Ok(()) => {
                    let select_name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned);
                    self.begin_directory_action(
                        self.cwd.clone(),
                        select_name,
                        format!("Created folder {}", path.display()),
                        format!("created folder {}", path.display()),
                        "refresh",
                        filesystem,
                    )
                }
                Err(error) => StepwiseAction::Complete(self.failure("create folder", &path, error)),
            },
            FileAction::Rename { old_path, new_path } => {
                match filesystem.rename(&old_path, &new_path) {
                    Ok(()) => {
                        let select_name = new_path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .map(str::to_owned);
                        self.begin_directory_action(
                            self.cwd.clone(),
                            select_name,
                            format!("Renamed {} to {}", old_path.display(), new_path.display()),
                            format!("renamed {} -> {}", old_path.display(), new_path.display()),
                            "refresh",
                            filesystem,
                        )
                    }
                    Err(error) => {
                        StepwiseAction::Complete(self.failure("rename", &old_path, error))
                    }
                }
            }
            FileAction::Delete { path, is_dir } => match filesystem.delete(&path, is_dir) {
                Ok(()) => self.begin_directory_action(
                    self.cwd.clone(),
                    None,
                    format!("Deleted {}", path.display()),
                    format!("deleted {}", path.display()),
                    "refresh",
                    filesystem,
                ),
                Err(error) => StepwiseAction::Complete(self.failure("delete", &path, error)),
            },
            other => StepwiseAction::Complete(self.execute_with(other, filesystem)),
        }
    }

    fn begin_directory_action(
        &mut self,
        path: PathBuf,
        select_name: Option<String>,
        status: String,
        log: String,
        failure_operation: &'static str,
        filesystem: &dyn FileSystem,
    ) -> StepwiseAction {
        match filesystem.begin_directory_scan(&path) {
            Ok(Some(scanner)) => {
                self.status = format!("Loading {}...", path.display());
                StepwiseAction::Pending(PendingDirectoryAction {
                    scanner,
                    path,
                    select_name,
                    status,
                    log,
                    failure_operation,
                })
            }
            Ok(None) => match self.load_directory(path.clone(), select_name.as_deref(), filesystem)
            {
                Ok(()) => {
                    self.status = status;
                    StepwiseAction::Complete(ActionOutcome::changed(log))
                }
                Err(error) => {
                    StepwiseAction::Complete(self.failure(failure_operation, &path, error))
                }
            },
            Err(error) => StepwiseAction::Complete(self.failure(failure_operation, &path, error)),
        }
    }

    fn handle_browse_key(&mut self, key: UiKey, visible_rows: usize) -> Option<FileAction> {
        match key {
            UiKey::Up => self.move_selection(-1, visible_rows),
            UiKey::Down => self.move_selection(1, visible_rows),
            UiKey::PageUp => self.move_selection(-(visible_rows.max(1) as isize), visible_rows),
            UiKey::PageDown => self.move_selection(visible_rows.max(1) as isize, visible_rows),
            UiKey::Home => {
                if !self.entries.is_empty() {
                    self.selected = Some(0);
                    self.ensure_selected_visible(visible_rows);
                }
            }
            UiKey::End => {
                if !self.entries.is_empty() {
                    self.selected = Some(self.entries.len() - 1);
                    self.ensure_selected_visible(visible_rows);
                }
            }
            UiKey::Enter | UiKey::Char('o') | UiKey::Char('O') => {
                return self.activate_selection();
            }
            UiKey::Char('p') | UiKey::Char('P') => return self.preview_selection(),
            UiKey::Backspace => return self.parent_action(),
            UiKey::Delete | UiKey::Char('d') | UiKey::Char('D') => self.begin_delete(),
            UiKey::Char('n') | UiKey::Char('N') => self.begin_create_folder(),
            UiKey::Char('r') | UiKey::Char('R') => self.begin_rename(),
            UiKey::Char('g') | UiKey::Char('G') => return Some(FileAction::Refresh),
            UiKey::Char('i') | UiKey::Char('I') => {
                return Some(FileAction::RequestHostImport);
            }
            UiKey::Char('e') | UiKey::Char('E') => return self.request_host_export(),
            UiKey::Escape => {
                self.selected = None;
                self.status = "Selection cleared".to_string();
            }
            UiKey::Close | UiKey::Char('q') | UiKey::Char('Q') => {
                return Some(FileAction::Close);
            }
            UiKey::Char(_) => {}
        }
        None
    }

    fn request_host_export(&mut self) -> Option<FileAction> {
        let Some(entry) = self.selected_entry() else {
            self.status = "Error: export: select a file first".to_string();
            return None;
        };
        if entry.is_dir {
            self.status = "Error: export: folders are not supported".to_string();
            return None;
        }
        Some(FileAction::RequestHostExport(self.cwd.join(&entry.name)))
    }

    fn begin_create_folder(&mut self) {
        self.mode = ViewMode::Input {
            kind: DialogKind::CreateFolder,
            value: String::new(),
            original_name: None,
            replace_on_type: false,
        };
        self.status = "Create folder: enter a name".to_string();
    }

    fn begin_rename(&mut self) {
        let Some(entry) = self.selected_entry() else {
            self.status = "Error: select an item to rename".to_string();
            return;
        };
        self.mode = ViewMode::Input {
            kind: DialogKind::Rename,
            value: entry.name.clone(),
            original_name: Some(entry.name.clone()),
            replace_on_type: true,
        };
        self.status = "Rename: type a replacement name".to_string();
    }

    fn begin_delete(&mut self) {
        let Some(entry) = self.selected_entry() else {
            self.status = "Error: select an item to delete".to_string();
            return;
        };
        let name = entry.name.clone();
        let is_dir = entry.is_dir;
        self.mode = ViewMode::ConfirmDelete {
            name: name.clone(),
            is_dir,
        };
        self.status = format!("Delete {name}? Enter/Y confirms");
    }

    fn activate_selection(&self) -> Option<FileAction> {
        let entry = self.selected_entry()?;
        let path = self.cwd.join(&entry.name);
        if entry.is_dir {
            Some(FileAction::Navigate {
                path,
                select_name: None,
            })
        } else {
            Some(FileAction::OpenDefault(path))
        }
    }

    fn preview_selection(&self) -> Option<FileAction> {
        let entry = self.selected_entry()?;
        if entry.is_dir {
            return None;
        }
        Some(FileAction::Preview(self.cwd.join(&entry.name)))
    }

    fn parent_action(&self) -> Option<FileAction> {
        let parent = self.cwd.parent()?;
        if parent == self.cwd {
            return None;
        }
        Some(FileAction::Navigate {
            path: parent.to_path_buf(),
            select_name: self
                .cwd
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned),
        })
    }

    fn move_selection(&mut self, delta: isize, visible_rows: usize) {
        if self.entries.is_empty() {
            self.selected = None;
            self.scroll = 0;
            return;
        }
        self.selected = Some(match self.selected {
            Some(current) => current
                .saturating_add_signed(delta)
                .min(self.entries.len() - 1),
            None if delta < 0 => self.entries.len() - 1,
            None => 0,
        });
        self.ensure_selected_visible(visible_rows);
    }

    fn ensure_selected_visible(&mut self, visible_rows: usize) {
        let rows = visible_rows.max(1);
        let Some(selected) = self.selected else {
            return;
        };
        if selected < self.scroll {
            self.scroll = selected;
        } else if selected >= self.scroll.saturating_add(rows) {
            self.scroll = selected + 1 - rows;
        }
    }

    fn load_directory(
        &mut self,
        path: PathBuf,
        select_name: Option<&str>,
        filesystem: &dyn FileSystem,
    ) -> io::Result<()> {
        let entries = filesystem.read_directory(&path)?;
        self.apply_directory_entries(path, entries, select_name);
        Ok(())
    }

    fn apply_directory_entries(
        &mut self,
        path: PathBuf,
        entries: Vec<FileEntry>,
        select_name: Option<&str>,
    ) {
        self.cwd = path;
        self.entries = entries;
        self.selected = select_name
            .and_then(|name| self.entries.iter().position(|entry| entry.name == name))
            .or_else(|| (!self.entries.is_empty()).then_some(0));
        self.scroll = 0;
        self.mode = ViewMode::Browse;
    }

    fn reload(&mut self, select_name: Option<&str>, filesystem: &dyn FileSystem) -> io::Result<()> {
        let entries = filesystem.read_directory(&self.cwd)?;
        self.apply_directory_entries(self.cwd.clone(), entries, select_name);
        Ok(())
    }

    fn failure(&mut self, operation: &str, path: &Path, error: io::Error) -> ActionOutcome {
        let message = format!("Error: {operation} {}: {error}", path.display());
        self.status = message.clone();
        ActionOutcome::changed(format!("error {operation} {}: {error}", path.display()))
    }
}

impl PendingDirectoryAction {
    /// Advance one bounded scan quantum. The visible directory stays stable
    /// until a complete, successfully sorted snapshot is available.
    pub fn step(&mut self, state: &mut FileManagerState) -> Option<ActionOutcome> {
        match self.scanner.step() {
            DirectoryScanStep::Pending => None,
            DirectoryScanStep::Complete(Ok(entries)) => {
                state.apply_directory_entries(
                    self.path.clone(),
                    entries,
                    self.select_name.as_deref(),
                );
                state.status.clone_from(&self.status);
                Some(ActionOutcome::changed(self.log.clone()))
            }
            DirectoryScanStep::Complete(Err(error)) => {
                Some(state.failure(self.failure_operation, &self.path, error))
            }
        }
    }
}

pub fn read_directory(path: &Path) -> io::Result<Vec<FileEntry>> {
    let mut scanner = DirectoryScanner::start(path)?;
    loop {
        match scanner.step() {
            DirectoryScanStep::Pending => {}
            DirectoryScanStep::Complete(result) => return result,
        }
    }
}

fn compare_file_entries(a: &FileEntry, b: &FileEntry) -> std::cmp::Ordering {
    match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    }
}

pub fn load_text_preview(path: &Path) -> io::Result<TextPreview> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "preview target is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(PREVIEW_LIMIT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    text_preview_from_bytes(path, bytes)
}

fn text_preview_from_bytes(path: &Path, mut bytes: Vec<u8>) -> io::Result<TextPreview> {
    let truncated = bytes.len() as u64 > PREVIEW_LIMIT_BYTES;
    if truncated {
        bytes.truncate(PREVIEW_LIMIT_BYTES as usize);
    }
    if bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "binary files do not have a text preview",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file is not valid UTF-8 text"))?;
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    Ok(TextPreview {
        path: path.to_path_buf(),
        lines,
        scroll: 0,
        truncated,
    })
}

fn validated_child_name(input: &str) -> Result<String, &'static str> {
    let name = input.trim();
    if name.is_empty() {
        return Err("name cannot be empty");
    }
    if name == "." || name == ".." {
        return Err("name cannot be . or ..");
    }
    if name.contains('/') || name.contains('\\') {
        return Err("name cannot contain a path separator");
    }
    Ok(name.to_string())
}
