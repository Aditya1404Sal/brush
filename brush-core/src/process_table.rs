//! Session-wide synthetic process numbers for embedders without operating-system processes.
//!
//! One table is shared by a shell and every clone of it, so subshells and jobs can never hand
//! out duplicate numbers. It holds metadata only and is `Send`; live process state is kept by the
//! executor (see `execution::process`).

use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

/// A synthetic process number.
pub type Pid = i32;

/// Largest number handed out; Linux's default `pid_max` ceiling.
pub const PID_MAX: Pid = 4_194_303;

/// Status of a numbered process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessStatus {
    /// Still running.
    Running,
    /// Exited with this status.
    Exited(u8),
    /// Terminated by this signal number.
    Signaled(u8),
}

/// One numbered process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessEntry {
    /// This process's number.
    pub pid: Pid,
    /// The number of the process that started it.
    pub ppid: Pid,
    /// Command text as `jobs` displays it.
    pub command: String,
    /// Current status.
    pub status: ProcessStatus,
    /// Whether this process leads a background job (`&`) or runs an output process
    /// substitution that outlives its command (`exec > >(list)`).
    pub job: bool,
    /// Whether this is such an output process substitution, which ends once its input closes.
    pub substitution: bool,
    /// Allocation order within the table, for reporting processes in the order they started.
    pub order: u64,
    /// The number the process is known by: for a background pipeline, its last stage (`$!`).
    pub shown: Pid,
}

/// How many finished processes keep their entry, so a late status lookup still finds them.
const FINISHED_KEPT: usize = 1024;

#[derive(Debug)]
struct Inner {
    shell_pid: Pid,
    next_pid: Pid,
    /// Running processes, and the most recently finished ones (see `finished`).
    entries: HashMap<Pid, ProcessEntry>,
    /// Finished processes in the order they finished; the oldest lose their entry first.
    finished: VecDeque<Pid>,
    /// Background job leaders still running, anywhere in the session.
    running_jobs: usize,
    allocations: u64,
}

/// Shared, clonable handle to one session's process numbers.
#[derive(Clone, Debug)]
pub struct ProcessTable {
    id: u64,
    inner: Arc<Mutex<Inner>>,
}

static NEXT_TABLE_ID: AtomicU64 = AtomicU64::new(1);

const fn clamp(pid: Pid) -> Pid {
    if pid < 1 {
        1
    } else if pid > PID_MAX {
        PID_MAX
    } else {
        pid
    }
}

const fn successor(pid: Pid) -> Pid {
    if pid >= PID_MAX { 1 } else { pid + 1 }
}

impl Default for ProcessTable {
    fn default() -> Self {
        Self::new(1, 2)
    }
}

impl ProcessTable {
    /// Creates a table whose shell is `shell_pid` and whose next number is `next_pid`.
    pub fn new(shell_pid: Pid, next_pid: Pid) -> Self {
        Self {
            id: NEXT_TABLE_ID.fetch_add(1, Ordering::Relaxed),
            inner: Arc::new(Mutex::new(Inner {
                shell_pid: clamp(shell_pid),
                next_pid: clamp(next_pid),
                entries: HashMap::new(),
                finished: VecDeque::new(),
                running_jobs: 0,
                allocations: 0,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Distinguishes tables that share one executor thread.
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Replaces the shell number and the next number, e.g. from an embedder's saved state.
    pub fn seed(&self, shell_pid: Pid, next_pid: Pid) {
        let mut inner = self.lock();
        inner.shell_pid = clamp(shell_pid);
        inner.next_pid = clamp(next_pid);
    }

    /// The shell's own number (`$$`).
    pub fn shell_pid(&self) -> Pid {
        self.lock().shell_pid
    }

    /// The number the next allocation starts searching from.
    pub fn next_pid(&self) -> Pid {
        self.lock().next_pid
    }

    /// Hands out the next free number, skipping the shell and running processes, wrapping at
    /// [`PID_MAX`].
    pub fn allocate(&self, ppid: Pid, command: String) -> Pid {
        self.allocate_entry(ppid, command, false)
    }

    /// Hands out a number, as [`Self::allocate`], for the leader of a background job; it counts
    /// towards [`Self::running_jobs`] until its status is final.
    pub fn allocate_job(&self, ppid: Pid, command: String) -> Pid {
        self.allocate_entry(ppid, command, true)
    }

    /// Hands out a number, as [`Self::allocate_job`], for an output process substitution that
    /// outlives its command.
    pub fn allocate_substitution(&self, ppid: Pid, command: String) -> Pid {
        let pid = self.allocate_entry(ppid, command, true);
        if let Some(entry) = self.lock().entries.get_mut(&pid) {
            entry.substitution = true;
        }
        pid
    }

    fn allocate_entry(&self, ppid: Pid, command: String, job: bool) -> Pid {
        let mut inner = self.lock();
        let mut candidate = inner.next_pid;
        for _ in 0..=PID_MAX {
            let taken = candidate == inner.shell_pid
                || inner
                    .entries
                    .get(&candidate)
                    .is_some_and(|entry| entry.status == ProcessStatus::Running);
            if !taken {
                break;
            }
            candidate = successor(candidate);
        }
        inner.next_pid = successor(candidate);
        inner.allocations += 1;
        let order = inner.allocations;
        if job {
            inner.running_jobs += 1;
        }
        inner.entries.insert(
            candidate,
            ProcessEntry {
                pid: candidate,
                ppid,
                command,
                status: ProcessStatus::Running,
                job,
                substitution: false,
                order,
                shown: candidate,
            },
        );
        candidate
    }

    /// Records a final or intermediate status. Unknown numbers are ignored.
    pub fn set_status(&self, pid: Pid, status: ProcessStatus) {
        let mut inner = self.lock();
        let Some(entry) = inner.entries.get_mut(&pid) else {
            return;
        };
        let was_running = entry.status == ProcessStatus::Running;
        let job = entry.job;
        entry.status = status;
        if was_running && status != ProcessStatus::Running {
            if job {
                inner.running_jobs -= 1;
            }
            inner.finished.push_back(pid);
            while inner.finished.len() > FINISHED_KEPT {
                if let Some(oldest) = inner.finished.pop_front()
                    && inner
                        .entries
                        .get(&oldest)
                        .is_some_and(|entry| entry.status != ProcessStatus::Running)
                {
                    inner.entries.remove(&oldest);
                }
            }
        }
    }

    /// Records the number process `pid` is known by, such as a background pipeline's last stage.
    pub fn set_shown_pid(&self, pid: Pid, shown: Pid) {
        if let Some(entry) = self.lock().entries.get_mut(&pid) {
            entry.shown = shown;
        }
    }

    /// Returns a copy of one entry.
    pub fn entry(&self, pid: Pid) -> Option<ProcessEntry> {
        self.lock().entries.get(&pid).cloned()
    }

    /// Whether `pid` is a running process of this session.
    pub fn is_running(&self, pid: Pid) -> bool {
        self.entry(pid)
            .is_some_and(|entry| entry.status == ProcessStatus::Running)
    }

    /// The most recently started process `ppid` started, other than a background job, that a
    /// signal ended: the one a foreground death notice names.
    pub fn last_signaled_child(&self, ppid: Pid) -> Option<Pid> {
        self.lock()
            .entries
            .values()
            .filter(|entry| {
                entry.ppid == ppid
                    && !entry.job
                    && matches!(entry.status, ProcessStatus::Signaled(_))
            })
            .max_by_key(|entry| entry.order)
            .map(|entry| entry.pid)
    }

    /// How many background jobs are running in the whole session, however deeply nested and
    /// whether or not a shell still lists them.
    pub fn running_jobs(&self) -> usize {
        self.lock().running_jobs
    }

    /// The background jobs still running, in the order they started.
    pub fn running_job_entries(&self) -> Vec<ProcessEntry> {
        let mut jobs: Vec<ProcessEntry> = self
            .lock()
            .entries
            .values()
            .filter(|entry| entry.job && entry.status == ProcessStatus::Running)
            .cloned()
            .collect();
        jobs.sort_by_key(|entry| entry.order);
        jobs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_from_seed_and_skips_the_shell() {
        let table = ProcessTable::new(100, 100);
        assert_eq!(table.allocate(100, "a".into()), 101);
        assert_eq!(table.allocate(100, "b".into()), 102);
        assert_eq!(table.next_pid(), 103);
    }

    #[test]
    fn wraps_at_pid_max_and_skips_running_numbers() {
        let table = ProcessTable::new(2, PID_MAX);
        assert_eq!(table.allocate(2, "last".into()), PID_MAX);
        assert_eq!(table.allocate(2, "wrapped".into()), 1);
        // 2 is the shell; 3 is free.
        assert_eq!(table.allocate(2, "after shell".into()), 3);
        table.seed(2, PID_MAX);
        // PID_MAX, 1 and 3 are still running and 2 is the shell, so the allocator moves on to 4.
        assert_eq!(table.allocate(2, "again".into()), 4);
    }

    #[test]
    fn clones_share_numbers() {
        let table = ProcessTable::new(10, 11);
        let clone = table.clone();
        assert_eq!(table.allocate(10, "a".into()), 11);
        assert_eq!(clone.allocate(10, "b".into()), 12);
        assert_eq!(clone.id(), table.id());
    }

    #[test]
    fn counts_running_jobs_across_the_session_and_forgets_old_finished_processes() {
        let table = ProcessTable::new(1, 2);
        let job = table.allocate_job(1, "job".into());
        let nested = table.clone().allocate_job(job, "nested job".into());
        let stage = table.allocate(1, "stage".into());
        assert_eq!(table.running_jobs(), 2);
        table.set_status(stage, ProcessStatus::Exited(0));
        assert_eq!(table.running_jobs(), 2);
        table.set_status(job, ProcessStatus::Exited(0));
        // A second final status does not count the job twice.
        table.set_status(job, ProcessStatus::Signaled(9));
        assert_eq!(table.running_jobs(), 1);
        assert_eq!(
            table
                .running_job_entries()
                .iter()
                .map(|entry| entry.pid)
                .collect::<Vec<_>>(),
            vec![nested]
        );
        for _ in 0..FINISHED_KEPT + 8 {
            let pid = table.allocate(1, "short".into());
            table.set_status(pid, ProcessStatus::Exited(0));
        }
        assert!(table.entry(job).is_none());
        assert!(table.is_running(nested));
        assert!(table.lock().entries.len() <= FINISHED_KEPT + 1);
    }

    #[test]
    fn records_status_and_clamps_seeds() {
        let table = ProcessTable::new(0, PID_MAX + 5);
        assert_eq!(table.shell_pid(), 1);
        assert_eq!(table.next_pid(), PID_MAX);
        let pid = table.allocate(1, "job".into());
        assert!(table.is_running(pid));
        table.set_status(pid, ProcessStatus::Signaled(15));
        assert!(!table.is_running(pid));
        assert_eq!(
            table.entry(pid).unwrap().status,
            ProcessStatus::Signaled(15)
        );
        assert_eq!(table.entry(pid).unwrap().ppid, 1);
    }
}
