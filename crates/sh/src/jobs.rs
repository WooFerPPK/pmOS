//! Job table for `&` background pipelines and the `jobs`
//! builtin (T145).
//!
//! A [`Job`] is one tracked background pipeline. Each job
//! has a shell-local id (`%1`, `%2`, …, recycled as jobs
//! complete), the pid of its first stage (used as the proxy
//! for "the job's pid" — POSIX usually tracks a process
//! group, but PMos has no pgid concept yet), the original
//! command line for display, and a [`JobStatus`].
//!
//! The shell adds a job via [`JobTable::add`] when a
//! pipeline is launched with a trailing `&`; calls
//! [`JobTable::reap`] periodically to flip
//! [`JobStatus::Done`] / [`JobStatus::Exited`] /
//! [`JobStatus::Signaled`] on jobs whose root pid has
//! transitioned to zombie; and walks the table for the
//! `jobs` builtin's output.
//!
//! Foreground job tracking lives elsewhere: when the shell
//! runs a foreground pipeline it stashes the pid in a
//! separate slot (the "current foreground pid") so the
//! Ctrl-C handler can `proc_kill(pid, SIGINT)` without
//! confusing it with backgrounded jobs.

use std::collections::BTreeMap;
use std::io::Write;

/// One tracked background pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Shell-local job id — `%N` in the bash convention.
    /// Stable for the life of the job; recycled when the
    /// job is removed from the table.
    pub id: u32,
    /// pid of the job's first stage. Used as the target for
    /// `kill %N` (a future builtin) and as the lookup key
    /// for `reap`.
    pub pid: i32,
    /// The command line as the user typed it (post-expansion
    /// is acceptable — the convention is human-readable
    /// rather than re-runnable). Used by `jobs` to render
    /// each entry.
    pub command: String,
    /// Current status — flipped by [`JobTable::reap`].
    pub status: JobStatus,
}

/// Status of a tracked job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Job is still alive.
    Running,
    /// Job exited normally with the given status. `code`
    /// matches the status byte the kernel's `proc_wait`
    /// returned (i.e. POSIX `WEXITSTATUS`).
    Exited { code: i32 },
    /// Job was killed by a signal. `signum` matches
    /// `WTERMSIG`.
    Signaled { signum: u16 },
    /// Catch-all for "we know it's dead but we couldn't
    /// classify the exit". Carried only for diagnostics; the
    /// `jobs` output renders this as `Done` (the bash
    /// fallback).
    Done,
}

/// Background-job table.
#[derive(Debug, Clone, Default)]
pub struct JobTable {
    jobs: BTreeMap<u32, Job>,
    next_id: u32,
}

impl JobTable {
    pub fn new() -> Self {
        JobTable {
            jobs: BTreeMap::new(),
            next_id: 1,
        }
    }

    /// Number of currently-tracked jobs (any status).
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    /// True iff the table has zero entries.
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// Insert a new job. Returns its shell-local id (`%N`).
    pub fn add(&mut self, pid: i32, command: String) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.jobs.insert(
            id,
            Job {
                id,
                pid,
                command,
                status: JobStatus::Running,
            },
        );
        id
    }

    /// Lookup a job by id.
    pub fn get(&self, id: u32) -> Option<&Job> {
        self.jobs.get(&id)
    }

    /// Mark a job's status. No-op when `id` is unknown.
    pub fn set_status(&mut self, id: u32, status: JobStatus) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.status = status;
        }
    }

    /// Drop every job whose status is no longer
    /// [`JobStatus::Running`]. Called after the `jobs`
    /// builtin renders its output so a completed job
    /// disappears from subsequent listings.
    pub fn purge_completed(&mut self) {
        self.jobs
            .retain(|_, j| matches!(j.status, JobStatus::Running));
    }

    /// Return every job in id-order. Used by the `jobs`
    /// builtin.
    pub fn iter(&self) -> impl Iterator<Item = &Job> {
        self.jobs.values()
    }

    /// Find a job by pid. Returns the id of the matching
    /// entry. Used by the (future) reap path: when
    /// `proc_wait` reports a zombie pid, this maps it back
    /// to a job id so we can flip its status.
    pub fn find_by_pid(&self, pid: i32) -> Option<u32> {
        self.jobs
            .values()
            .find(|j| j.pid == pid)
            .map(|j| j.id)
    }
}

/// Render the table for the `jobs` builtin.
///
/// Format mirrors bash:
///
/// ```text
/// [1]  Running   sleep 5 &
/// [2]- Done      echo hi
/// [3]+ Running   long_running_pipeline | xargs
/// ```
///
/// (The `+` / `-` markers denote the "current" / "previous"
/// job in bash; v1 omits them — every line starts with just
/// `[N]  ` to keep the output deterministic for tests.)
///
/// Returns `BuiltinOutcome::Continue` on success, `IoError`
/// on a stdout/stderr write failure. The caller (the `jobs`
/// builtin in builtin.rs) translates that into the matching
/// [`crate::builtin::BuiltinOutcome`] variant — we keep this
/// helper independent of the dispatch types so it can be
/// unit-tested without standing up a `dispatch_builtin`
/// fixture.
pub fn render_jobs<W: Write>(table: &JobTable, stdout: &mut W) -> std::io::Result<()> {
    for job in table.iter() {
        let status = match job.status {
            JobStatus::Running => "Running",
            JobStatus::Exited { code: 0 } => "Done",
            JobStatus::Exited { .. } => "Exited",
            JobStatus::Signaled { .. } => "Signaled",
            JobStatus::Done => "Done",
        };
        writeln!(stdout, "[{}]  {:<9} {}", job.id, status, job.command)?;
    }
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn empty_table_renders_nothing() {
        let table = JobTable::new();
        let mut buf = Cursor::new(Vec::new());
        render_jobs(&table, &mut buf).unwrap();
        assert!(buf.into_inner().is_empty());
    }

    #[test]
    fn add_assigns_sequential_ids() {
        let mut table = JobTable::new();
        let a = table.add(100, "sleep 5 &".to_string());
        let b = table.add(200, "echo hi".to_string());
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn set_status_updates_existing_job() {
        let mut table = JobTable::new();
        let id = table.add(100, "x".to_string());
        table.set_status(id, JobStatus::Exited { code: 0 });
        assert_eq!(
            table.get(id).unwrap().status,
            JobStatus::Exited { code: 0 }
        );
    }

    #[test]
    fn purge_completed_drops_finished_jobs() {
        let mut table = JobTable::new();
        let a = table.add(1, "a".to_string());
        let b = table.add(2, "b".to_string());
        table.set_status(a, JobStatus::Exited { code: 0 });
        table.purge_completed();
        assert!(table.get(a).is_none());
        assert!(table.get(b).is_some());
    }

    #[test]
    fn find_by_pid_returns_matching_id() {
        let mut table = JobTable::new();
        let a = table.add(42, "a".to_string());
        assert_eq!(table.find_by_pid(42), Some(a));
        assert_eq!(table.find_by_pid(99), None);
    }

    #[test]
    fn render_includes_running_and_done_status() {
        let mut table = JobTable::new();
        let id = table.add(100, "sleep 1 &".to_string());
        let mut buf = Cursor::new(Vec::new());
        render_jobs(&table, &mut buf).unwrap();
        let s = String::from_utf8(buf.into_inner()).unwrap();
        assert!(s.contains("[1]  Running   sleep 1 &"));
        table.set_status(id, JobStatus::Exited { code: 0 });
        let mut buf = Cursor::new(Vec::new());
        render_jobs(&table, &mut buf).unwrap();
        let s = String::from_utf8(buf.into_inner()).unwrap();
        assert!(s.contains("[1]  Done      sleep 1 &"));
    }

    #[test]
    fn render_signaled_status() {
        let mut table = JobTable::new();
        let id = table.add(100, "sleep 999 &".to_string());
        table.set_status(id, JobStatus::Signaled { signum: 9 });
        let mut buf = Cursor::new(Vec::new());
        render_jobs(&table, &mut buf).unwrap();
        let s = String::from_utf8(buf.into_inner()).unwrap();
        assert!(s.contains("Signaled"));
    }
}
