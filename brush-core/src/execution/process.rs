//! Logical process ownership for single-threaded WASM commands.
//!
//! Pipe writes report to the process being polled, rather than to the process that created the
//! pipe, so inherited descriptors cannot kill an ancestor. Numbered processes can also receive
//! HUP, INT, KILL and TERM from `kill`; default dispositions terminate the process at its next
//! poll, and caught ones wait for a trap safe point.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    future::Future,
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context, Poll, Waker},
};

use crate::{
    Error, ExecutionResult,
    process_table::{Pid, ProcessStatus, ProcessTable},
    traps::{PipeDisposition, TrapHandlerConfig},
};

/// Numbers of signals the engine itself raises or treats specially.
pub mod signals {
    /// Hangup.
    pub const HUP: u8 = 1;
    /// Interrupt.
    pub const INT: u8 = 2;
    /// Quit.
    pub const QUIT: u8 = 3;
    /// Uncatchable kill.
    pub const KILL: u8 = 9;
    /// Write to a pipe without readers.
    pub const PIPE: u8 = 13;
    /// Termination request.
    pub const TERM: u8 = 15;
    /// Continue a stopped process.
    pub const CONT: u8 = 18;
    /// Uncatchable stop.
    pub const STOP: u8 = 19;
    /// The highest signal number.
    pub const MAX: u8 = 64;
}

/// What a signal does to a process that neither catches nor ignores it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DefaultAction {
    Terminate,
    Ignore,
    Continue,
    Stop,
}

/// Linux's default action for `signal`. The C library's reserved 32 to 34 do nothing.
const fn default_action(signal: u8) -> DefaultAction {
    match signal {
        17 | 23 | 28 | 32..=34 => DefaultAction::Ignore,
        signals::CONT => DefaultAction::Continue,
        signals::STOP | 20..=22 => DefaultAction::Stop,
        _ => DefaultAction::Terminate,
    }
}

/// Per-signal delivery policy of one logical process, indexed by signal number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Dispositions([PipeDisposition; signals::MAX as usize + 1]);

impl Default for Dispositions {
    fn default() -> Self {
        Self([PipeDisposition::Default; signals::MAX as usize + 1])
    }
}

impl Dispositions {
    /// Reads every disposition from a shell's trap configuration.
    pub fn from_traps(traps: &TrapHandlerConfig) -> Self {
        let mut dispositions = Self::default();
        for (signal, disposition) in traps.signal_dispositions() {
            dispositions.set(signal, disposition);
        }
        dispositions
    }

    /// Caught handlers reset across a command boundary; ignored signals stay ignored.
    #[must_use]
    pub fn for_exec(mut self) -> Self {
        for disposition in &mut self.0 {
            *disposition = disposition.for_exec();
        }
        self
    }

    /// Disposition of one signal number. KILL, STOP and unknown numbers are always default.
    pub const fn get(self, signal: u8) -> PipeDisposition {
        match signal {
            signals::KILL | signals::STOP => PipeDisposition::Default,
            1..=signals::MAX => self.0[signal as usize],
            _ => PipeDisposition::Default,
        }
    }

    /// Sets one signal's disposition; unknown numbers are ignored.
    pub const fn set(&mut self, signal: u8, disposition: PipeDisposition) {
        if signal >= 1 && signal <= signals::MAX {
            self.0[signal as usize] = disposition;
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<ProcessState>>> = const { RefCell::new(None) };
    static REGISTRY: RefCell<HashMap<(u64, Pid), Weak<ProcessState>>> =
        RefCell::new(HashMap::new());
    static JOB_SCOPES: RefCell<HashMap<u64, Rc<super::TaskScope>>> =
        RefCell::new(HashMap::new());
}

/// Where one session's background jobs run: apart from the processes that start them, so a job
/// outlives the subshell, pipeline stage, command substitution or child shell that started it,
/// as an orphaned process does. The embedder stops the jobs still running when it is done (see
/// [`ProcessTable::running_job_entries`]) and then cancels what is left with
/// [`JobScope::cancel_and_join`]. Without a job scope, a job belongs to the process that starts
/// it and ends with it.
pub struct JobScope {
    table: u64,
    scope: Rc<super::TaskScope>,
}

impl JobScope {
    /// Makes this the job scope of `table`'s session until it is dropped.
    #[must_use]
    pub fn new(table: &ProcessTable) -> Self {
        let scope = Rc::new(super::TaskScope::default());
        JOB_SCOPES.with_borrow_mut(|scopes| scopes.insert(table.id(), scope.clone()));
        Self {
            table: table.id(),
            scope,
        }
    }

    /// Cancels every job still running and waits until they have released their resources.
    pub async fn cancel_and_join(&self) {
        self.scope.cancel_and_join().await;
    }
}

impl Drop for JobScope {
    fn drop(&mut self) {
        JOB_SCOPES.with_borrow_mut(|scopes| {
            if scopes
                .get(&self.table)
                .is_some_and(|scope| Rc::ptr_eq(scope, &self.scope))
            {
                scopes.remove(&self.table);
            }
        });
        self.scope.abort();
    }
}

/// Starts the task of a background job of `table`'s session: in the session's [`JobScope`], if
/// the embedder made one, where it outlives the process that started it and dropping the handle
/// leaves it running; otherwise as an ordinary task of the current process.
pub fn spawn_job<T: 'static>(
    services: &super::ExecutionServices,
    table: &ProcessTable,
    future: impl Future<Output = T> + 'static,
) -> super::LocalTaskHandle<T> {
    let Some(scope) = JOB_SCOPES.with_borrow(|scopes| scopes.get(&table.id()).cloned()) else {
        return services.spawn(future);
    };
    let _restore = super::RestoreScope(super::CURRENT_SCOPE.replace(Some(scope)));
    let mut handle = services.spawn(future);
    handle.adopted = true;
    handle
}

pub(super) struct ProcessState {
    dispositions: Cell<Dispositions>,
    terminated: Cell<Option<u8>>,
    /// Stopped by STOP, TSTP, TTIN or TTOU until CONT: not polled, and signals other than KILL
    /// wait for it to continue.
    stopped: Cell<bool>,
    /// A background job's process, which starts ignoring INT and QUIT once it runs.
    background: Cell<bool>,
    started: Cell<bool>,
    pending: RefCell<Vec<u8>>,
    handling: Cell<bool>,
    waker: RefCell<Option<Waker>>,
    children: RefCell<Vec<Weak<Self>>>,
}

impl ProcessState {
    fn new(dispositions: Dispositions) -> Rc<Self> {
        Self::with_parent(dispositions, current().as_ref())
    }

    fn with_parent(dispositions: Dispositions, parent: Option<&Rc<Self>>) -> Rc<Self> {
        let state = Rc::new(Self {
            dispositions: Cell::new(dispositions),
            terminated: Cell::new(None),
            stopped: Cell::new(false),
            background: Cell::new(false),
            started: Cell::new(false),
            pending: RefCell::new(Vec::new()),
            handling: Cell::new(false),
            waker: RefCell::new(None),
            children: RefCell::new(Vec::new()),
        });
        if let Some(parent) = parent {
            let mut children = parent.children.borrow_mut();
            children.retain(|child| child.strong_count() > 0);
            children.push(Rc::downgrade(&state));
        }
        state
    }

    fn deliver(&self, signal: u8) {
        if signal == signals::KILL {
            self.terminated.set(Some(signals::KILL));
        } else if signal == signals::STOP {
            self.stopped.set(true);
        } else {
            // CONT continues a stopped process whatever its disposition.
            if signal == signals::CONT {
                self.stopped.set(false);
            }
            match self.dispositions.get().get(signal) {
                PipeDisposition::Default => match default_action(signal) {
                    DefaultAction::Terminate => {
                        if self.terminated.get().is_none() {
                            self.terminated.set(Some(signal));
                        }
                    }
                    DefaultAction::Stop => self.stopped.set(true),
                    DefaultAction::Ignore | DefaultAction::Continue => {}
                },
                PipeDisposition::Ignored => {}
                PipeDisposition::Caught => {
                    // A PIPE raised while its own handler runs is dropped, as before.
                    if signal == signals::PIPE && self.handling.get() {
                        return;
                    }
                    let mut pending = self.pending.borrow_mut();
                    if !pending.contains(&signal) {
                        pending.push(signal);
                    }
                }
            }
        }
        let waker = self.waker.borrow().clone();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn deliver_group(&self, signal: u8) {
        self.deliver(signal);
        let children: Vec<Rc<Self>> = self
            .children
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for child in children {
            child.deliver_group(signal);
        }
    }
}

pub(super) fn current() -> Option<Rc<ProcessState>> {
    CURRENT.with_borrow(Clone::clone)
}

pub(super) struct RestoreProcess(Option<Rc<ProcessState>>);
impl Drop for RestoreProcess {
    fn drop(&mut self) {
        CURRENT.replace(self.0.take());
    }
}

pub(super) fn install(process: Option<Rc<ProcessState>>) -> RestoreProcess {
    RestoreProcess(CURRENT.replace(process))
}

/// Effective SIGPIPE disposition of the currently executing logical process.
/// The executor installs this context only while polling its future.
pub fn pipe_disposition() -> PipeDisposition {
    current().map_or(PipeDisposition::Default, |state| {
        state.dispositions.get().get(signals::PIPE)
    })
}

/// Updates the running process after a shell trap configuration change.
pub fn set_pipe_disposition(disposition: PipeDisposition) {
    set_signal_disposition(signals::PIPE, disposition);
}

/// Updates one signal's disposition in the running process.
pub fn set_signal_disposition(signal: u8, disposition: PipeDisposition) {
    if let Some(state) = current() {
        let mut dispositions = state.dispositions.get();
        dispositions.set(signal, disposition);
        state.dispositions.set(dispositions);
    }
}

/// Every disposition of the running process, or defaults outside a process.
pub fn current_dispositions() -> Dispositions {
    current().map_or_else(Dispositions::default, |state| state.dispositions.get())
}

/// Dispositions a child command inherits: the running process's, reset for exec, with `pipe`.
pub fn inherited_dispositions(pipe: PipeDisposition) -> Dispositions {
    let mut dispositions = current_dispositions().for_exec();
    dispositions.set(signals::PIPE, pipe);
    dispositions
}

/// Applies a shell's trap configuration to the running process.
///
/// Signals with a handler take its disposition, and PIPE always follows the configuration, as
/// before. A caught signal whose handler was removed returns to default. Signals the
/// configuration never mentions keep their inherited disposition, such as a background job's
/// ignored INT.
pub fn apply_trap_dispositions(traps: &TrapHandlerConfig) {
    let Some(state) = current() else {
        return;
    };
    let mut dispositions = state.dispositions.get();
    for signal in 1..=signals::MAX {
        if dispositions.get(signal) == PipeDisposition::Caught {
            dispositions.set(signal, PipeDisposition::Default);
        }
    }
    for (signal, disposition) in traps.signal_dispositions() {
        dispositions.set(signal, disposition);
    }
    dispositions.set(signals::PIPE, traps.pipe_disposition());
    state.dispositions.set(dispositions);
}

/// Records a failed, nonempty write. No handler executes in the I/O callback.
pub(crate) fn record_broken_pipe() {
    if let Some(state) = current() {
        state.deliver(signals::PIPE);
    }
}

/// Takes the oldest caught signal waiting for a trap safe point.
pub(crate) fn take_pending_trap() -> Option<u8> {
    current().and_then(|state| {
        let mut pending = state.pending.borrow_mut();
        (!pending.is_empty()).then(|| pending.remove(0))
    })
}

/// The oldest caught signal waiting for a trap safe point in the running process, left pending.
pub fn pending_trapped_signal() -> Option<u8> {
    current().and_then(|state| state.pending.borrow().first().copied())
}

/// Resolves with [`pending_trapped_signal`] once there is one. `wait` returns early with it, and
/// the trap then runs, as in bash.
pub fn trapped_signal() -> impl Future<Output = u8> {
    // A delivery wakes the process's task, which polls this again.
    std::future::poll_fn(|_| pending_trapped_signal().map_or(Poll::Pending, Poll::Ready))
}

pub(crate) struct HandlingPipe(Option<Rc<ProcessState>>);
impl Drop for HandlingPipe {
    fn drop(&mut self) {
        if let Some(state) = self.0.take() {
            state.handling.set(false);
        }
    }
}

pub(crate) fn handling_pipe() -> HandlingPipe {
    let state = current();
    if let Some(state) = &state {
        state.handling.set(true);
    }
    HandlingPipe(state)
}

struct ProcessFuture<F> {
    body: Pin<Box<F>>,
    state: Rc<ProcessState>,
    tasks: Rc<super::TaskScope>,
}

impl<F: Future<Output = Result<ExecutionResult, Error>>> Future for ProcessFuture<F> {
    type Output = Result<ExecutionResult, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        *self.state.waker.borrow_mut() = Some(cx.waker().clone());
        let terminated = self.state.terminated.get();
        if terminated == Some(signals::KILL) {
            return Poll::Ready(Ok(ExecutionResult::terminated_by_signal(signals::KILL)));
        }
        // A stopped process waits for CONT; only KILL ends it meanwhile.
        if self.state.stopped.get() {
            return Poll::Pending;
        }
        if let Some(signal) = terminated {
            return Poll::Ready(Ok(ExecutionResult::terminated_by_signal(signal)));
        }
        // Without job control, a background job ignores INT and QUIT once it runs; one sent
        // before its first turn still ends it, as in bash.
        if !self.state.started.replace(true) && self.state.background.get() {
            let mut dispositions = self.state.dispositions.get();
            for signal in [signals::INT, signals::QUIT] {
                dispositions.set(signal, PipeDisposition::Ignored);
            }
            self.state.dispositions.set(dispositions);
        }
        let _process = install(Some(self.state.clone()));
        let _tasks = super::RestoreScope(super::CURRENT_SCOPE.replace(Some(self.tasks.clone())));
        let result = self.body.as_mut().poll(cx);
        // A failed write or a delivered signal can happen in the very poll that completes the body.
        if let Some(signal) = self.state.terminated.get() {
            Poll::Ready(Ok(ExecutionResult::terminated_by_signal(signal)))
        } else {
            result
        }
    }
}

struct AbortScope(Rc<super::TaskScope>);
impl Drop for AbortScope {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Registration(Option<(u64, Pid)>);
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(key) = self.0.take() {
            REGISTRY.with_borrow_mut(|registry| registry.remove(&key));
        }
    }
}

async fn run_state(
    state: Rc<ProcessState>,
    body: impl Future<Output = Result<ExecutionResult, Error>>,
) -> Result<ExecutionResult, Error> {
    let tasks = super::TaskScope::nested();
    let _abort = AbortScope(tasks.clone());
    let result = ProcessFuture {
        body: Box::pin(body),
        state,
        tasks: tasks.clone(),
    }
    .await;
    tasks.cancel_and_join().await;
    result
}

/// Runs a borrowed command future in a new unnumbered logical process and joins its descendants.
/// HUP/INT/TERM are inherited from the running process (reset for exec); PIPE is `disposition`.
pub async fn run_process(
    disposition: PipeDisposition,
    body: impl Future<Output = Result<ExecutionResult, Error>>,
) -> Result<ExecutionResult, Error> {
    run_state(ProcessState::new(inherited_dispositions(disposition)), body).await
}

/// A numbered process registered before it first runs, so it can be signalled at once (for
/// example `sleep 5 & kill $!`). Dropping it unrun records it as killed.
pub struct NumberedProcess {
    table: ProcessTable,
    pid: Pid,
    state: Rc<ProcessState>,
    _registration: Registration,
    finished: bool,
}

impl NumberedProcess {
    /// Registers `pid` as a child of the running process.
    #[must_use]
    pub fn register(table: &ProcessTable, pid: Pid, dispositions: Dispositions) -> Self {
        Self::register_under(table, pid, dispositions, current().as_ref())
    }

    /// Registers `pid` as a child of this process, so group signals reach it before it runs.
    #[must_use]
    pub fn register_child(&self, pid: Pid, dispositions: Dispositions) -> Self {
        Self::register_under(&self.table, pid, dispositions, Some(&self.state))
    }

    fn register_under(
        table: &ProcessTable,
        pid: Pid,
        dispositions: Dispositions,
        parent: Option<&Rc<ProcessState>>,
    ) -> Self {
        let state = ProcessState::with_parent(dispositions, parent);
        let key = (table.id(), pid);
        REGISTRY.with_borrow_mut(|registry| registry.insert(key, Rc::downgrade(&state)));
        Self {
            table: table.clone(),
            pid,
            state,
            _registration: Registration(Some(key)),
            finished: false,
        }
    }

    /// Marks this as a process of a background job: once it runs, it ignores INT and QUIT.
    #[must_use]
    pub fn in_background(self) -> Self {
        self.state.background.set(true);
        self
    }

    /// This process's number.
    pub const fn pid(&self) -> Pid {
        self.pid
    }

    /// Runs `body` as this process and records its final status in the table.
    pub async fn run(
        mut self,
        body: impl Future<Output = Result<ExecutionResult, Error>>,
    ) -> Result<ExecutionResult, Error> {
        let result = run_state(self.state.clone(), body).await;
        // Only this process's own termination is a signal death; a normal exit that merely
        // returns a killed child's status (143) is an ordinary exit, as for a bash subshell.
        let status = match (self.state.terminated.get(), &result) {
            (Some(signal), _) => ProcessStatus::Signaled(signal),
            (None, Ok(result)) => ProcessStatus::Exited(u8::from(result.exit_code)),
            (None, Err(_)) => ProcessStatus::Exited(1),
        };
        self.table.set_status(self.pid, status);
        self.finished = true;
        result
    }
}

impl Drop for NumberedProcess {
    fn drop(&mut self) {
        if !self.finished {
            self.table
                .set_status(self.pid, ProcessStatus::Signaled(signals::KILL));
        }
    }
}

/// Runs `body` as numbered process `pid` of `table`, reachable by [`signal_process`], and
/// records its final status in the table.
pub async fn run_numbered_process(
    table: &ProcessTable,
    pid: Pid,
    dispositions: Dispositions,
    body: impl Future<Output = Result<ExecutionResult, Error>>,
) -> Result<ExecutionResult, Error> {
    NumberedProcess::register(table, pid, dispositions)
        .run(body)
        .await
}

fn lookup(table: &ProcessTable, pid: Pid) -> Option<Rc<ProcessState>> {
    REGISTRY.with_borrow(|registry| registry.get(&(table.id(), pid)).and_then(Weak::upgrade))
}

/// Whether numbered process `pid` (including the main script, `$$`) is still running.
pub fn process_exists(table: &ProcessTable, pid: Pid) -> bool {
    lookup(table, pid).is_some()
}

/// Delivers `signal` to numbered process `pid`. Returns false if it is not running.
pub fn signal_process(table: &ProcessTable, pid: Pid, signal: u8) -> bool {
    lookup(table, pid).is_some_and(|state| {
        state.deliver(signal);
        true
    })
}

/// Delivers `signal` to numbered process `pid` and every logical process it started.
pub fn signal_process_group(table: &ProcessTable, pid: Pid, signal: u8) -> bool {
    lookup(table, pid).is_some_and(|state| {
        state.deliver_group(signal);
        true
    })
}

/// Whether execution is currently inside an owned logical process.
pub(crate) fn is_active() -> bool {
    current().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{execution::ExecutionServices, openfiles::test_pipe};
    use std::io::Write;

    fn run(future: impl Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokio::task::LocalSet::new().run_until(future));
    }

    #[test]
    fn reader_closure_does_not_cancel_remaining_work_or_replace_status() {
        run(async {
            for capacity in [1, 8, 65_536] {
                let (reader, mut writer) = test_pipe(capacity);
                let result = run_process(PipeDisposition::Default, async {
                    writer.write_all(b"x").unwrap();
                    drop(reader);
                    tokio::task::yield_now().await;
                    Ok(ExecutionResult::new(7))
                })
                .await
                .unwrap();
                assert_eq!(u8::from(result.exit_code), 7);
            }
        });
    }

    #[test]
    fn failed_write_wins_even_when_body_completes_in_the_same_poll() {
        run(async {
            let (reader, mut writer) = test_pipe(1);
            drop(reader);
            let empty = run_process(PipeDisposition::Default, async {
                assert_eq!(writer.write(b"").unwrap(), 0);
                Ok(ExecutionResult::success())
            })
            .await
            .unwrap();
            assert!(empty.is_success());
            assert_eq!(empty.terminating_signal, None);
            let result = run_process(PipeDisposition::Default, async {
                assert!(is_active());
                assert_eq!(
                    writer.write(b"x").unwrap_err().kind(),
                    std::io::ErrorKind::BrokenPipe
                );
                Ok(ExecutionResult::success())
            })
            .await
            .unwrap();
            assert_eq!(u8::from(result.exit_code), 141);
            assert_eq!(result.terminating_signal, Some(13));
            assert!(!is_active());
        });
    }

    #[test]
    fn explicit_exit_141_is_not_a_signal_termination() {
        run(async {
            let result = run_process(PipeDisposition::Default, async {
                Ok(ExecutionResult::new(141))
            })
            .await
            .unwrap();
            assert_eq!(u8::from(result.exit_code), 141);
            assert_eq!(result.terminating_signal, None);
        });
    }

    #[test]
    fn inherited_writer_signals_nested_process_without_killing_caller() {
        run(async {
            let (reader, mut writer) = test_pipe(1);
            drop(reader);
            let result = run_process(PipeDisposition::Default, async {
                let child = run_process(PipeDisposition::Default, async {
                    let _ = writer.write(b"x");
                    Ok(ExecutionResult::success())
                })
                .await?;
                assert_eq!(u8::from(child.exit_code), 141);
                Ok(ExecutionResult::new(7))
            })
            .await
            .unwrap();
            assert_eq!(u8::from(result.exit_code), 7);
        });
    }

    #[test]
    fn ignored_and_caught_writes_preserve_error_handling_and_coalesce_traps() {
        run(async {
            for disposition in [PipeDisposition::Ignored, PipeDisposition::Caught] {
                let (reader, mut writer) = test_pipe(1);
                drop(reader);
                let result = run_process(disposition, async {
                    let _ = writer.write(b"x");
                    let _ = writer.write(b"x");
                    assert_eq!(
                        take_pending_trap() == Some(signals::PIPE),
                        disposition == PipeDisposition::Caught
                    );
                    assert_eq!(take_pending_trap(), None);
                    let _handling = handling_pipe();
                    let _ = writer.write(b"x");
                    assert_eq!(take_pending_trap(), None);
                    Ok(ExecutionResult::new(1))
                })
                .await
                .unwrap();
                assert_eq!(u8::from(result.exit_code), 1);
            }
        });
    }

    #[test]
    fn writer_error_override_does_not_affect_other_clones() {
        run(async {
            let (reader, mut normal) = test_pipe(1);
            let mut handled = normal.clone();
            handled.set_broken_pipe_cancellation(false);
            drop(reader);
            let result = run_process(PipeDisposition::Default, async {
                let _ = handled.write(b"x");
                Ok(ExecutionResult::success())
            })
            .await
            .unwrap();
            assert_eq!(u8::from(result.exit_code), 0);
            let result = run_process(PipeDisposition::Default, async {
                let _ = normal.write(b"x");
                let _ = handled.write(b"x");
                Ok(ExecutionResult::success())
            })
            .await
            .unwrap();
            assert_eq!(u8::from(result.exit_code), 141);
        });
    }

    struct Released(Rc<Cell<bool>>);
    impl Drop for Released {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    #[test]
    fn spawned_task_inherits_process_signal_and_is_joined() {
        run(async {
            let services = ExecutionServices::default();
            let released = Rc::new(Cell::new(false));
            let observed = released.clone();
            let result = run_process(PipeDisposition::Default, async move {
                let _task = services.spawn(async move {
                    let _guard = Released(observed);
                    record_broken_pipe();
                    futures::future::pending::<()>().await;
                });
                futures::future::pending::<Result<ExecutionResult, Error>>().await
            })
            .await
            .unwrap();
            assert_eq!(u8::from(result.exit_code), 141);
            assert!(released.get());
        });
    }

    #[test]
    fn a_job_outlives_its_process_in_a_job_scope_and_ends_with_it_otherwise() {
        run(async {
            let services = ExecutionServices::default();
            let table = ProcessTable::new(10, 11);
            for adopted in [true, false] {
                let jobs = adopted.then(|| JobScope::new(&table));
                let released = Rc::new(Cell::new(false));
                let guard = Released(released.clone());
                run_process(PipeDisposition::Default, async {
                    // Dropping the handle, as a subshell's job table is dropped, keeps it running.
                    drop(spawn_job(&services, &table, async move {
                        let _guard = guard;
                        futures::future::pending::<()>().await;
                    }));
                    Ok(ExecutionResult::success())
                })
                .await
                .unwrap();
                assert_eq!(released.get(), !adopted);
                if let Some(jobs) = jobs {
                    jobs.cancel_and_join().await;
                    assert!(released.get());
                }
            }
        });
    }

    #[test]
    fn dropping_borrowed_process_body_joins_its_nested_tasks() {
        run(async {
            let released = Rc::new(Cell::new(false));
            let observed = released.clone();
            let services = ExecutionServices::default();
            let (started_send, started) = futures::channel::oneshot::channel();
            let result = run_process(PipeDisposition::Default, async {
                let child = run_process(PipeDisposition::Default, async move {
                    let _task = services.spawn(async move {
                        let _guard = Released(observed);
                        let _ = started_send.send(());
                        futures::future::pending::<()>().await;
                    });
                    futures::future::pending::<Result<ExecutionResult, Error>>().await
                });
                let interrupt = async {
                    started.await.unwrap();
                    record_broken_pipe();
                    futures::future::pending::<()>().await;
                };
                futures::join!(child, interrupt).0
            })
            .await
            .unwrap();
            assert_eq!(u8::from(result.exit_code), 141);
            assert!(released.get());
        });
    }

    use crate::process_table::{ProcessStatus, ProcessTable};
    use crate::traps::TrapHandlerConfig;

    fn with(settings: &[(u8, PipeDisposition)]) -> Dispositions {
        let mut dispositions = Dispositions::default();
        for (signal, disposition) in settings {
            dispositions.set(*signal, *disposition);
        }
        dispositions
    }

    fn caught(term: bool) -> Dispositions {
        if term {
            with(&[(signals::TERM, PipeDisposition::Caught)])
        } else {
            Dispositions::default()
        }
    }

    #[test]
    fn default_term_terminates_a_numbered_process_with_143() {
        run(async {
            let table = ProcessTable::new(10, 11);
            let pid = table.allocate(10, "sleep".into());
            let (result, ()) = futures::join!(
                run_numbered_process(&table, pid, Dispositions::default(), async {
                    futures::future::pending::<()>().await;
                    Ok(ExecutionResult::success())
                }),
                async {
                    tokio::task::yield_now().await;
                    assert!(signal_process(&table, pid, signals::TERM));
                }
            );
            let result = result.unwrap();
            assert_eq!(u8::from(result.exit_code), 143);
            assert_eq!(result.terminating_signal, Some(signals::TERM));
            assert_eq!(
                table.entry(pid).unwrap().status,
                ProcessStatus::Signaled(15)
            );
            assert!(!signal_process(&table, pid, signals::TERM));
        });
    }

    #[test]
    fn ignored_signal_is_dropped_and_caught_signal_waits_for_a_safe_point() {
        run(async {
            let table = ProcessTable::new(10, 11);
            let ignored = table.allocate(10, "ignored".into());
            let caught_pid = table.allocate(10, "caught".into());
            let ignore = with(&[(signals::TERM, PipeDisposition::Ignored)]);
            let (ignored_result, caught_result, ()) = futures::join!(
                run_numbered_process(&table, ignored, ignore, async {
                    for _ in 0..4 {
                        tokio::task::yield_now().await;
                    }
                    Ok(ExecutionResult::new(7))
                }),
                run_numbered_process(&table, caught_pid, caught(true), async {
                    loop {
                        if take_pending_trap() == Some(signals::TERM) {
                            return Ok(ExecutionResult::new(3));
                        }
                        tokio::task::yield_now().await;
                    }
                }),
                async {
                    tokio::task::yield_now().await;
                    assert!(signal_process(&table, ignored, signals::TERM));
                    assert!(signal_process(&table, caught_pid, signals::TERM));
                }
            );
            assert_eq!(u8::from(ignored_result.unwrap().exit_code), 7);
            assert_eq!(u8::from(caught_result.unwrap().exit_code), 3);
            assert_eq!(
                table.entry(ignored).unwrap().status,
                ProcessStatus::Exited(7)
            );
        });
    }

    #[test]
    fn trapped_signal_wakes_a_waiter_and_stays_pending() {
        run(async {
            let table = ProcessTable::new(10, 11);
            let pid = table.allocate(10, "waiter".into());
            let (result, ()) = futures::join!(
                run_numbered_process(&table, pid, caught(true), async {
                    assert_eq!(pending_trapped_signal(), None);
                    let signal = trapped_signal().await;
                    assert_eq!(pending_trapped_signal(), Some(signal));
                    assert_eq!(take_pending_trap(), Some(signals::TERM));
                    Ok(ExecutionResult::new(signal))
                }),
                async {
                    tokio::task::yield_now().await;
                    assert!(signal_process(&table, pid, signals::TERM));
                }
            );
            assert_eq!(u8::from(result.unwrap().exit_code), signals::TERM);
        });
    }

    #[test]
    fn stop_holds_a_process_until_cont_and_kill_still_ends_it() {
        run(async {
            let table = ProcessTable::new(10, 11);
            let (stopped, killed) = (
                table.allocate(10, "stopped".into()),
                table.allocate(10, "killed".into()),
            );
            let progress = Rc::new(Cell::new(0));
            let counted = progress.clone();
            let (resumed, dead, ()) = futures::join!(
                run_numbered_process(&table, stopped, Dispositions::default(), async move {
                    for _ in 0..8 {
                        counted.set(counted.get() + 1);
                        tokio::task::yield_now().await;
                    }
                    Ok(ExecutionResult::new(5))
                }),
                run_numbered_process(&table, killed, Dispositions::default(), async {
                    futures::future::pending::<()>().await;
                    Ok(ExecutionResult::success())
                }),
                async {
                    tokio::task::yield_now().await;
                    for pid in [stopped, killed] {
                        assert!(signal_process(&table, pid, signals::STOP));
                    }
                    // TERM waits for CONT; KILL does not.
                    signal_process(&table, stopped, signals::TERM);
                    signal_process(&table, killed, signals::KILL);
                    let seen = progress.get();
                    for _ in 0..8 {
                        tokio::task::yield_now().await;
                    }
                    assert_eq!(progress.get(), seen);
                    signal_process(&table, stopped, signals::CONT);
                }
            );
            assert_eq!(u8::from(resumed.unwrap().exit_code), 143);
            assert_eq!(u8::from(dead.unwrap().exit_code), 137);
        });
    }

    #[test]
    fn default_actions_ignore_some_signals_and_background_jobs_ignore_interrupts_once_started() {
        run(async {
            let table = ProcessTable::new(10, 11);
            let job = table.allocate(10, "job".into());
            let early = table.allocate(10, "early".into());
            let early_process =
                NumberedProcess::register(&table, early, Dispositions::default()).in_background();
            // INT before the job's first turn ends it.
            assert!(signal_process(&table, early, signals::INT));
            let (result, early_result, ()) = futures::join!(
                NumberedProcess::register(&table, job, Dispositions::default())
                    .in_background()
                    .run(async {
                        for _ in 0..4 {
                            tokio::task::yield_now().await;
                        }
                        Ok(ExecutionResult::new(3))
                    }),
                early_process.run(async { Ok(ExecutionResult::success()) }),
                async {
                    tokio::task::yield_now().await;
                    // CHLD, WINCH and URG are ignored by default; INT and QUIT once it runs.
                    for signal in [17, 28, 23, signals::INT, signals::QUIT] {
                        assert!(signal_process(&table, job, signal));
                    }
                }
            );
            assert_eq!(u8::from(result.unwrap().exit_code), 3);
            assert_eq!(u8::from(early_result.unwrap().exit_code), 130);
        });
    }

    #[test]
    fn kill_ignores_dispositions() {
        run(async {
            let table = ProcessTable::new(10, 11);
            let pid = table.allocate(10, "stubborn".into());
            let all_ignored = with(
                &(1..=signals::MAX)
                    .map(|signal| (signal, PipeDisposition::Ignored))
                    .collect::<Vec<_>>(),
            );
            let (result, ()) = futures::join!(
                run_numbered_process(&table, pid, all_ignored, async {
                    futures::future::pending::<()>().await;
                    Ok(ExecutionResult::success())
                }),
                async {
                    tokio::task::yield_now().await;
                    signal_process(&table, pid, signals::KILL);
                }
            );
            assert_eq!(u8::from(result.unwrap().exit_code), 137);
        });
    }

    #[test]
    fn group_signal_reaches_unnumbered_children_and_children_inherit_for_exec() {
        run(async {
            let table = ProcessTable::new(10, 11);
            let pid = table.allocate(10, "job".into());
            // The leader catches TERM (children reset it to default) and ignores HUP (inherited).
            let leader = with(&[
                (signals::TERM, PipeDisposition::Caught),
                (signals::HUP, PipeDisposition::Ignored),
            ]);
            let (result, ()) = futures::join!(
                run_numbered_process(&table, pid, leader, async {
                    let inherited = inherited_dispositions(PipeDisposition::Default);
                    assert_eq!(inherited.get(signals::TERM), PipeDisposition::Default);
                    assert_eq!(inherited.get(signals::HUP), PipeDisposition::Ignored);
                    let child = run_process(PipeDisposition::Default, async {
                        futures::future::pending::<()>().await;
                        Ok(ExecutionResult::success())
                    })
                    .await?;
                    Ok(child)
                }),
                async {
                    tokio::task::yield_now().await;
                    assert!(signal_process_group(&table, pid, signals::TERM));
                }
            );
            // The catching leader only queues TERM; its default-disposition child is terminated.
            // The leader itself exits normally with the child's status, as a bash subshell does.
            assert_eq!(u8::from(result.unwrap().exit_code), 143);
            assert_eq!(table.entry(pid).unwrap().status, ProcessStatus::Exited(143));
        });
    }

    #[test]
    fn trap_configuration_updates_explicit_signals_and_keeps_inherited_ignores() {
        run(async {
            let table = ProcessTable::new(10, 11);
            let pid = table.allocate(10, "job".into());
            let inherited = with(&[(signals::INT, PipeDisposition::Ignored)]);
            run_numbered_process(&table, pid, inherited, async {
                assert!(process_exists(&table, pid));
                let mut traps = TrapHandlerConfig::default();
                traps.register_handler(
                    "TERM".parse()?,
                    "echo t".into(),
                    crate::SourceInfo::from("test"),
                );
                apply_trap_dispositions(&traps);
                assert_eq!(
                    current_dispositions().get(signals::TERM),
                    PipeDisposition::Caught
                );
                assert_eq!(
                    current_dispositions().get(signals::INT),
                    PipeDisposition::Ignored
                );
                traps.remove_handlers("TERM".parse()?);
                apply_trap_dispositions(&traps);
                assert_eq!(
                    current_dispositions().get(signals::TERM),
                    PipeDisposition::Default
                );
                Ok(ExecutionResult::success())
            })
            .await
            .unwrap();
            assert!(!process_exists(&table, pid));
        });
    }
}
