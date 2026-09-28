//! Session-wide synthetic process numbers for embedders without operating-system processes.
//!
//! One table is shared by a shell and every clone of it, so subshells and jobs can never hand
//! out duplicate numbers. It holds metadata only and is `Send`; live process state is kept by the
//! executor (see `execution::process`).

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
}

#[derive(Debug)]
struct Inner {
    shell_pid: Pid,
    next_pid: Pid,
    entries: Vec<ProcessEntry>,
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
                entries: Vec::new(),
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
        let mut inner = self.lock();
        let mut candidate = inner.next_pid;
        for _ in 0..=PID_MAX {
            let taken = candidate == inner.shell_pid
                || inner
                    .entries
                    .iter()
                    .any(|entry| entry.pid == candidate && entry.status == ProcessStatus::Running);
            if !taken {
                break;
            }
            candidate = successor(candidate);
        }
        inner.next_pid = successor(candidate);
        inner.entries.retain(|entry| entry.pid != candidate);
        inner.entries.push(ProcessEntry {
            pid: candidate,
            ppid,
            command,
            status: ProcessStatus::Running,
        });
        candidate
    }

    /// Records a final or intermediate status. Unknown numbers are ignored.
    pub fn set_status(&self, pid: Pid, status: ProcessStatus) {
        if let Some(entry) = self
            .lock()
            .entries
            .iter_mut()
            .find(|entry| entry.pid == pid)
        {
            entry.status = status;
        }
    }

    /// Returns a copy of one entry.
    pub fn entry(&self, pid: Pid) -> Option<ProcessEntry> {
        self.lock()
            .entries
            .iter()
            .find(|entry| entry.pid == pid)
            .cloned()
    }

    /// Whether `pid` is a running process of this session.
    pub fn is_running(&self, pid: Pid) -> bool {
        self.entry(pid)
            .is_some_and(|entry| entry.status == ProcessStatus::Running)
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
