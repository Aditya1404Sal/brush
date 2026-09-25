//! Job management

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::Display;

use futures::FutureExt;

use crate::ExecutionResult;
use crate::error;
use crate::processes;
use crate::sys;
use crate::trace_categories;
use crate::traps;

pub(crate) type JobJoinHandle = crate::execution::CommandTask;
pub(crate) type JobResult = (Job, Result<ExecutionResult, error::Error>);

/// Manages the jobs that are currently managed by the shell.
#[derive(Default)]
pub struct JobManager {
    /// The jobs that are currently managed by the shell.
    pub jobs: Vec<Job>,
    /// Jobs `disown` took out of the table: they keep running, but `jobs` no longer lists them
    /// and `wait` no longer waits for them.
    disowned: Vec<Job>,
    /// Final statuses of reaped jobs by their process numbers, oldest first: `wait PID` still
    /// finds one after its job has left the table, as with bash's saved statuses.
    reaped: VecDeque<(sys::process::ProcessId, u8, Option<u8>)>,
}

/// Why a job specification names no job.
#[derive(Debug)]
pub enum JobSpecError {
    /// No job matches.
    NoSuchJob,
    /// More than one job's command matches a `%name` or `%?text` prefix.
    Ambiguous,
}

/// How many reaped jobs' statuses a shell remembers.
const REAPED_KEPT: usize = 1024;

/// Represents a task that is part of a job.
pub enum JobTask {
    /// An external process.
    External(processes::ChildProcess),
    /// An internal asynchronous task.
    Internal(JobJoinHandle),
}

/// Represents the result of waiting on a job task.
pub enum JobTaskWaitResult {
    /// The task has completed.
    Completed(ExecutionResult),
    /// The task was stopped.
    Stopped,
}

impl JobTask {
    /// Returns whether the task is an external process.
    pub const fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }

    /// Waits for the task to complete. Returns the result of the wait.
    pub async fn wait(&mut self) -> Result<JobTaskWaitResult, error::Error> {
        match self {
            Self::External(process) => {
                let wait_result = process.wait().await?;
                match wait_result {
                    processes::ProcessWaitResult::Completed(output) => {
                        Ok(JobTaskWaitResult::Completed(output.into()))
                    }
                    processes::ProcessWaitResult::Stopped => Ok(JobTaskWaitResult::Stopped),
                }
            }
            Self::Internal(handle) => match handle.await {
                Ok(r) => Ok(JobTaskWaitResult::Completed(r?)),
                // An aborted task is a synthetically killed job: report the conventional
                // 128+SIGKILL status instead of bubbling a join error.
                Err(e) if e.is_cancelled() => {
                    Ok(JobTaskWaitResult::Completed(ExecutionResult::new(137)))
                }
                Err(e) => Err(e.into()),
            },
        }
    }

    /// Polls the task for completion. Returns `Some(result)` if the task has completed,
    /// or `None` if it is still running. The result is the execution result of the task.
    /// Behaves in a best-effort manner; if an internal error occurs during polling,
    /// it will return `None`.
    fn poll(&mut self) -> Option<Result<ExecutionResult, error::Error>> {
        match self {
            Self::External(process) => {
                let check_result = process.poll();
                check_result.map(|polled_result| polled_result.map(|output| output.into()))
            }
            Self::Internal(handle) => match handle.now_or_never() {
                Some(Ok(inner)) => Some(inner),
                // A cancelled (aborted) task must still be reapable: report it as killed rather
                // than pending forever.
                Some(Err(e)) if e.is_cancelled() => Some(Ok(ExecutionResult::new(137))),
                // Panicked task: preserve the existing best-effort behavior.
                Some(Err(_)) | None => None,
            },
        }
    }
}

/// Numbers every job ever added, so a job can be found again after its table changed.
static NEXT_JOB_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Most background jobs one session keeps running at once, however deeply they are nested.
pub const MAX_RUNNING_JOBS: usize = 256;

impl JobManager {
    /// Returns a new job manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a job to the job manager and marks it as the current job;
    /// returns an immutable reference to the job.
    ///
    /// # Arguments
    ///
    /// * `job` - The job to add.
    #[allow(
        clippy::missing_panics_doc,
        reason = "push() guarantees the vector length is >= 1"
    )]
    pub fn add_as_current(&mut self, mut job: Job) -> &Job {
        self.clean_up_reaped();
        // The current job becomes the previous one, and the previous one loses its mark.
        for j in &mut self.jobs {
            j.annotation = match j.annotation {
                JobAnnotation::Current => JobAnnotation::Previous,
                JobAnnotation::Previous | JobAnnotation::None => JobAnnotation::None,
            };
        }

        // Allocate above the highest live id — `len() + 1` collides once jobs are removed out of
        // order (kill %1 while job 2 lives → the next job would also get id 2).
        let id = self.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
        job.id = id;
        job.serial = NEXT_JOB_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        job.annotation = JobAnnotation::Current;
        self.jobs.push(job);

        #[allow(clippy::unwrap_used, reason = "we just pushed an element")]
        self.jobs.last().unwrap()
    }

    /// Returns the current job, if there is one.
    pub fn current_job(&self) -> Option<&Job> {
        self.jobs
            .iter()
            .find(|j| matches!(j.annotation, JobAnnotation::Current))
    }

    /// Returns a mutable reference to the current job, if there is one.
    pub fn current_job_mut(&mut self) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .find(|j| matches!(j.annotation, JobAnnotation::Current))
    }

    /// Returns the previous job, if there is one.
    pub fn prev_job(&self) -> Option<&Job> {
        self.jobs
            .iter()
            .find(|j| matches!(j.annotation, JobAnnotation::Previous))
    }

    /// Returns a mutable reference to the previous job, if there is one.
    pub fn prev_job_mut(&mut self) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .find(|j| matches!(j.annotation, JobAnnotation::Previous))
    }

    /// The table a pipeline stage or command substitution starts with: its parent's jobs, to
    /// list only, as bash lists them there.
    pub(crate) fn listing_copy(&self) -> Self {
        Self {
            jobs: self.jobs.iter().map(Job::listing_copy).collect(),
            disowned: Vec::new(),
            reaped: VecDeque::new(),
        }
    }

    /// Marks a job `wait` has waited for as reaped, remembering its final status for its process
    /// numbers. As in bash, it stays in the table (so `wait %N` finds it again) until the table
    /// is next cleaned up: when a new job starts or `jobs` runs.
    pub fn mark_reaped(&mut self, serial: u64, status: &ExecutionResult) {
        let Some(job) = self
            .jobs
            .iter_mut()
            .chain(self.disowned.iter_mut())
            .find(|job| job.serial == serial)
        else {
            return;
        };
        job.reaped = true;
        let pids: Vec<_> = job.pids.iter().chain(job.leader.iter()).copied().collect();
        self.remember(&pids, status);
    }

    /// Takes a finished job out of the table, as `wait -n` and `wait` without operands do; the
    /// status is remembered only when `remember` is set.
    pub fn remove_finished(&mut self, serial: u64, status: Option<&ExecutionResult>) {
        let removed = if let Some(index) = self.jobs.iter().position(|job| job.serial == serial) {
            Some(self.remove_listed(index))
        } else {
            self.disowned
                .iter()
                .position(|job| job.serial == serial)
                .map(|index| self.disowned.remove(index))
        };
        if let (Some(job), Some(status)) = (removed, status) {
            let pids: Vec<_> = job.pids.iter().chain(job.leader.iter()).copied().collect();
            self.remember(&pids, status);
        }
    }

    /// Drops the jobs already reaped by `wait` from the table.
    fn clean_up_reaped(&mut self) {
        while let Some(index) = self.jobs.iter().position(|job| job.reaped) {
            self.remove_listed(index);
        }
        self.disowned.retain(|job| !job.reaped);
    }

    /// Removes a listed job; the previous job becomes current if it was.
    fn remove_listed(&mut self, index: usize) -> Job {
        let job = self.jobs.remove(index);
        if matches!(job.annotation, JobAnnotation::Current)
            && let Some(previous) = self.prev_job_mut()
        {
            previous.annotation = JobAnnotation::Current;
        }
        job
    }

    fn remember(&mut self, pids: &[sys::process::ProcessId], status: &ExecutionResult) {
        for pid in pids {
            self.reaped.retain(|(reaped, _, _)| reaped != pid);
            self.reaped
                .push_back((*pid, u8::from(status.exit_code), status.terminating_signal));
        }
        while self.reaped.len() > REAPED_KEPT {
            self.reaped.pop_front();
        }
    }

    /// The final status of a job reaped earlier, by one of its process numbers.
    pub fn reaped_status(&self, pid: sys::process::ProcessId) -> Option<ExecutionResult> {
        self.reaped
            .iter()
            .rev()
            .find(|(reaped, _, _)| *reaped == pid)
            .map(|(_, code, signal)| ExecutionResult {
                terminating_signal: *signal,
                ..ExecutionResult::new(*code)
            })
    }

    /// Takes the job with this id out of the table, as `disown` does; returns whether there was
    /// one.
    pub fn disown(&mut self, id: usize) -> bool {
        let Some(index) = self.jobs.iter().position(|job| job.id == id) else {
            return false;
        };
        let job = self.jobs.remove(index);
        if matches!(job.annotation, JobAnnotation::Current)
            && let Some(previous) = self.prev_job_mut()
        {
            previous.annotation = JobAnnotation::Current;
        }
        self.disowned.push(job);
        true
    }

    /// Jobs `disown` took out of the table, still running or not yet reaped.
    pub fn disowned(&self) -> &[Job] {
        &self.disowned
    }

    /// The job of this shell, listed or disowned, one of whose process numbers is `pid`. A
    /// subshell's copies of its parent's jobs are not its own.
    pub fn job_with_pid_mut(&mut self, pid: crate::process_table::Pid) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .chain(self.disowned.iter_mut())
            .filter(|job| !job.listing_only)
            .find(|job| job.pids.contains(&pid) || job.leader == Some(pid))
    }

    /// Tries to resolve the given job specification to a job.
    ///
    /// # Arguments
    ///
    /// * `job_spec` - The job specification to resolve.
    pub fn resolve_job_spec(&mut self, job_spec: &str) -> Option<&mut Job> {
        self.find_job_spec(job_spec).ok()
    }

    /// Resolves a job specification to one of this shell's own jobs: `%N`, `%%`, `%+`, `%-`,
    /// `%name` (a command starting with `name`) or `%?text` (a command containing `text`). A
    /// subshell's copies of its parent's jobs are listed by `jobs` but are not its own.
    ///
    /// # Arguments
    ///
    /// * `job_spec` - The job specification to resolve.
    pub fn find_job_spec(&mut self, job_spec: &str) -> Result<&mut Job, JobSpecError> {
        let remainder = job_spec.strip_prefix('%').ok_or(JobSpecError::NoSuchJob)?;
        let own = |job: &&mut Job| !job.listing_only;
        match remainder {
            "" | "%" | "+" => self.current_job_mut().filter(|job| !job.listing_only),
            "-" => self.prev_job_mut().filter(|job| !job.listing_only),
            s if s.chars().all(char::is_numeric) => {
                let id = s.parse::<usize>().map_err(|_| JobSpecError::NoSuchJob)?;
                self.jobs.iter_mut().filter(own).find(|j| j.id == id)
            }
            s => {
                let matches = |job: &Job| match s.strip_prefix('?') {
                    Some(text) => job.command_line.contains(text),
                    None => job.command_line.starts_with(s),
                };
                let count = self
                    .jobs
                    .iter()
                    .filter(|job| !job.listing_only && matches(job))
                    .count();
                if count > 1 {
                    return Err(JobSpecError::Ambiguous);
                }
                self.jobs.iter_mut().filter(own).find(|job| matches(job))
            }
        }
        .ok_or(JobSpecError::NoSuchJob)
    }

    /// Waits for all managed jobs to complete.
    pub async fn wait_all(&mut self) -> Result<Vec<Job>, error::Error> {
        for job in &mut self.jobs {
            job.wait().await?;
        }

        Ok(self.sweep_completed_jobs())
    }

    /// Polls all managed jobs for completion.
    pub fn poll(&mut self) -> Result<Vec<JobResult>, error::Error> {
        self.clean_up_reaped();
        let mut results = Vec::with_capacity(self.jobs.len());

        let mut i = 0;
        while i != self.jobs.len() {
            if let Some(result) = self.jobs[i].poll_done()? {
                let job = self.jobs.remove(i);
                if let Ok(status) = &result {
                    let pids: Vec<_> = job.pids.iter().chain(job.leader.iter()).copied().collect();
                    self.remember(&pids, status);
                }
                results.push((job, result));
            } else if matches!(self.jobs[i].state, JobState::Done) {
                // TODO(jobs): This is a workaround to remove jobs that are done but for which we
                // don't know what happened.
                results.push((self.jobs.remove(i), Ok(ExecutionResult::success())));
            } else {
                i += 1;
            }
        }

        Ok(results)
    }

    fn sweep_completed_jobs(&mut self) -> Vec<Job> {
        let mut completed_jobs = vec![];

        let mut i = 0;
        while i != self.jobs.len() {
            if self.jobs[i].tasks.is_empty() {
                completed_jobs.push(self.jobs.remove(i));
            } else {
                i += 1;
            }
        }

        completed_jobs
    }
}

/// Represents the current execution state of a job.
#[derive(Clone)]
pub enum JobState {
    /// Unknown state.
    Unknown,
    /// The job is running.
    Running,
    /// The job is stopped.
    Stopped,
    /// The job has completed.
    Done,
}

impl Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "Unknown"),
            Self::Running => write!(f, "Running"),
            Self::Stopped => write!(f, "Stopped"),
            Self::Done => write!(f, "Done"),
        }
    }
}

/// Represents an annotation for a job.
#[derive(Clone)]
pub enum JobAnnotation {
    /// No annotation.
    None,
    /// The job is the current job.
    Current,
    /// The job is the previous job.
    Previous,
}

impl Display for JobAnnotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, " "),
            Self::Current => write!(f, "+"),
            Self::Previous => write!(f, "-"),
        }
    }
}

/// Encapsulates a set of processes managed by the shell as a single unit.
pub struct Job {
    /// The tasks that make up the job.
    tasks: VecDeque<JobTask>,

    /// If available, the process group ID of the job's processes.
    pgid: Option<sys::process::ProcessId>,

    /// The annotation of the job (e.g., current, previous).
    annotation: JobAnnotation,

    /// The shell-internal ID of the job.
    pub id: usize,

    /// Identifies this job for as long as it exists, unlike `id`, which a later job can reuse.
    pub serial: u64,

    /// `wait` has waited for this job; it leaves the table at the next cleanup.
    reaped: bool,

    /// How the job ended, once it has: its status and the signal that ended it, if one did.
    final_status: Option<(u8, Option<u8>)>,

    /// The command line of the job.
    pub command_line: String,

    /// The current operational state of the job.
    pub state: JobState,

    /// The numbered logical process that runs this job.
    leader: Option<crate::process_table::Pid>,

    /// Numbers reported for this job; the last is `$!`.
    pids: Vec<crate::process_table::Pid>,

    /// A copy a pipeline stage or command substitution keeps of its parent's job, to list it
    /// as bash does; it has no tasks, never completes, and cannot be waited for.
    listing_only: bool,
}

/// A job's line as `jobs` prints it, as bash lays it out: `[N]+  Running                    cmd &`.
impl Display for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}]{}  {:<27}{}",
            self.id,
            self.annotation,
            self.state.to_string(),
            self.display_command()
        )
    }
}

impl Job {
    /// Returns a new job object.
    ///
    /// # Arguments
    ///
    /// * `children` - The job's known child processes.
    /// * `command_line` - The command line of the job.
    /// * `state` - The current operational state of the job.
    pub(crate) fn new<I>(tasks: I, command_line: String, state: JobState) -> Self
    where
        I: IntoIterator<Item = JobTask>,
    {
        Self {
            id: 0,
            serial: 0,
            reaped: false,
            final_status: None,
            tasks: tasks.into_iter().collect(),
            pgid: None,
            annotation: JobAnnotation::None,
            command_line,
            state,
            leader: None,
            pids: Vec::new(),
            listing_only: false,
        }
    }

    /// The command as `jobs` shows it: with ` &` while it runs in the background.
    pub fn display_command(&self) -> String {
        match self.state {
            JobState::Running => std::format!("{} &", self.command_line),
            _ => self.command_line.clone(),
        }
    }

    /// A copy of this job for listing only (see `listing_only`).
    fn listing_copy(&self) -> Self {
        Self {
            tasks: VecDeque::new(),
            pgid: self.pgid,
            annotation: self.annotation.clone(),
            id: self.id,
            serial: self.serial,
            reaped: self.reaped,
            final_status: self.final_status,
            command_line: self.command_line.clone(),
            state: self.state.clone(),
            leader: self.leader,
            pids: self.pids.clone(),
            listing_only: true,
        }
    }

    /// Returns a running job whose work runs as numbered process `leader`.
    #[cfg_attr(
        not(target_arch = "wasm32"),
        expect(dead_code, reason = "only WASM numbers background jobs")
    )]
    pub(crate) fn new_numbered<I>(
        tasks: I,
        command_line: String,
        leader: crate::process_table::Pid,
        pids: Vec<crate::process_table::Pid>,
    ) -> Self
    where
        I: IntoIterator<Item = JobTask>,
    {
        let mut job = Self::new(tasks, command_line, JobState::Running);
        job.leader = Some(leader);
        job.pids = pids;
        job
    }

    /// Whether this is a subshell's copy of its parent's job: `jobs` lists it, but it is not
    /// this shell's to wait for or signal.
    pub const fn is_listing_copy(&self) -> bool {
        self.listing_only
    }

    /// Whether `wait` has already waited for this job.
    pub const fn is_reaped(&self) -> bool {
        self.reaped
    }

    /// The numbered process running this job, if it has one.
    pub const fn leader(&self) -> Option<crate::process_table::Pid> {
        self.leader
    }

    /// Numbers reported for this job, in pipeline order.
    pub fn pids(&self) -> &[crate::process_table::Pid] {
        &self.pids
    }

    /// Returns a pid-style string for the job.
    pub fn to_pid_style_string(&self) -> String {
        let display_pid = self
            .representative_pid()
            .map_or(Cow::Borrowed("<pid unknown>"), |pid| {
                Cow::Owned(pid.to_string())
            });
        std::format!("[{}]{}\t{}", self.id, self.annotation, display_pid)
    }

    /// Returns the annotation of the job.
    pub fn annotation(&self) -> JobAnnotation {
        self.annotation.clone()
    }

    /// Returns the command name of the job.
    pub fn command_name(&self) -> &str {
        self.command_line
            .split_ascii_whitespace()
            .next()
            .unwrap_or_default()
    }

    /// Returns whether the job is the current job.
    pub const fn is_current(&self) -> bool {
        matches!(self.annotation, JobAnnotation::Current)
    }

    /// Returns whether the job is the previous job.
    pub const fn is_prev(&self) -> bool {
        matches!(self.annotation, JobAnnotation::Previous)
    }

    /// Polls whether the job has completed.
    pub fn poll_done(
        &mut self,
    ) -> Result<Option<Result<ExecutionResult, error::Error>>, error::Error> {
        let mut result: Option<Result<ExecutionResult, error::Error>> = None;
        if self.listing_only {
            return Ok(None);
        }

        tracing::debug!(target: trace_categories::JOBS, "Polling job {} for completion...", self.id);

        while !self.tasks.is_empty() {
            let task = &mut self.tasks[0];
            match task.poll() {
                Some(r) => {
                    self.tasks.remove(0);
                    result = Some(r);
                }
                None => {
                    return Ok(None);
                }
            }
        }

        tracing::debug!(target: trace_categories::JOBS, "Job {} has completed.", self.id);

        self.state = JobState::Done;
        if let Some(Ok(result)) = &result {
            self.record_final_status(result);
        }

        Ok(result)
    }

    /// How the job ended, once it has.
    pub fn final_status(&self) -> Option<ExecutionResult> {
        self.final_status.map(|(code, signal)| ExecutionResult {
            terminating_signal: signal,
            ..ExecutionResult::new(code)
        })
    }

    fn record_final_status(&mut self, result: &ExecutionResult) {
        self.final_status = Some((u8::from(result.exit_code), result.terminating_signal));
    }

    /// Waits for the job to complete.
    pub async fn wait(&mut self) -> Result<ExecutionResult, error::Error> {
        // Waiting again for a finished job gives the same status.
        if self.tasks.is_empty()
            && let Some(status) = self.final_status()
        {
            return Ok(status);
        }
        let mut result = ExecutionResult::success();

        while let Some(task) = self.tasks.back_mut() {
            match task.wait().await? {
                JobTaskWaitResult::Completed(execution_result) => {
                    result = execution_result;
                    self.tasks.pop_back();
                }
                JobTaskWaitResult::Stopped => {
                    self.state = JobState::Stopped;
                    return Ok(ExecutionResult::stopped());
                }
            }
        }

        self.state = JobState::Done;
        self.record_final_status(&result);

        Ok(result)
    }

    /// Moves the job to execute in the background.
    pub fn move_to_background(&mut self) -> Result<(), error::Error> {
        if matches!(self.state, JobState::Stopped) {
            if let Some(pgid) = self.process_group_id() {
                sys::signal::continue_process(pgid)?;
                self.state = JobState::Running;
                Ok(())
            } else {
                Err(error::ErrorKind::FailedToSendSignal.into())
            }
        } else {
            error::unimp("move job to background")
        }
    }

    /// Moves the job to execute in the foreground.
    pub fn move_to_foreground(&mut self) -> Result<(), error::Error> {
        if matches!(self.state, JobState::Stopped) {
            if let Some(pgid) = self.process_group_id() {
                sys::signal::continue_process(pgid)?;
                self.state = JobState::Running;
            } else {
                return Err(error::ErrorKind::FailedToSendSignal.into());
            }
        }

        if let Some(pgid) = self.process_group_id() {
            sys::terminal::move_to_foreground(pgid)?;
        }

        Ok(())
    }

    /// Checks whether the job can be signaled.
    pub fn check_signalable(&self) -> Result<(), error::Error> {
        if let Some(pid) = self.process_group_id() {
            sys::signal::check_signalable(pid)
        } else {
            Err(error::ErrorKind::FailedToSendSignal.into())
        }
    }

    /// Aborts the job's internal async tasks — the synthetic `kill` on targets with no real
    /// processes. Each `JobTask::Internal` future is dropped at its next await point (or never
    /// polled at all if it hadn't started); the task then reports as killed (exit 137) through
    /// [`JobTask::wait`]/[`JobTask::poll`]. External-process tasks are untouched (use
    /// [`Job::kill`] for those).
    pub fn abort(&mut self) {
        for task in &self.tasks {
            if let JobTask::Internal(handle) = task {
                handle.abort();
            }
        }
        self.state = JobState::Done;
    }

    /// Kills the job.
    ///
    /// # Arguments
    ///
    /// * `signal` - The signal to send to the job.
    pub fn kill(&self, signal: traps::TrapSignal) -> Result<(), error::Error> {
        if let Some(pid) = self.process_group_id() {
            sys::signal::kill_process(pid, signal)
        } else {
            Err(error::ErrorKind::FailedToSendSignal.into())
        }
    }

    /// Tries to retrieve a "representative" pid for the job.
    pub fn representative_pid(&self) -> Option<sys::process::ProcessId> {
        if let Some(pid) = self.pids.last() {
            return Some(*pid);
        }
        for task in &self.tasks {
            match task {
                JobTask::External(p) => {
                    if let Some(pid) = p.pid() {
                        return Some(pid);
                    }
                }
                JobTask::Internal(_) => (),
            }
        }
        None
    }

    /// Tries to retrieve the process group ID (PGID) of the job.
    pub fn process_group_id(&self) -> Option<sys::process::ProcessId> {
        // TODO(jobs): Don't assume that the first PID is the PGID.
        self.pgid.or_else(|| self.representative_pid())
    }
}
