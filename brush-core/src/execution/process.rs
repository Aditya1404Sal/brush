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

/// Numbers of the synthetic signal set.
pub mod signals {
    /// Hangup.
    pub const HUP: u8 = 1;
    /// Interrupt.
    pub const INT: u8 = 2;
    /// Uncatchable kill.
    pub const KILL: u8 = 9;
    /// Write to a pipe without readers.
    pub const PIPE: u8 = 13;
    /// Termination request.
    pub const TERM: u8 = 15;
}

/// Per-signal delivery policy of one logical process.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Dispositions {
    /// HUP disposition.
    pub hup: PipeDisposition,
    /// INT disposition.
    pub int: PipeDisposition,
    /// PIPE disposition.
    pub pipe: PipeDisposition,
    /// TERM disposition.
    pub term: PipeDisposition,
}

impl Dispositions {
    /// Reads every disposition from a shell's trap configuration.
    pub fn from_traps(traps: &TrapHandlerConfig) -> Self {
        let get = |name: &str| {
            name.parse().map_or(PipeDisposition::Default, |signal| {
                traps.signal_disposition(signal)
            })
        };
        Self {
            hup: get("HUP"),
            int: get("INT"),
            pipe: get("PIPE"),
            term: get("TERM"),
        }
    }

    /// Caught handlers reset across a command boundary; ignored signals stay ignored.
    #[must_use]
    pub const fn for_exec(self) -> Self {
        Self {
            hup: self.hup.for_exec(),
            int: self.int.for_exec(),
            pipe: self.pipe.for_exec(),
            term: self.term.for_exec(),
        }
    }

    /// Disposition of one signal number. KILL and unknown numbers are always default.
    pub const fn get(self, signal: u8) -> PipeDisposition {
        match signal {
            signals::HUP => self.hup,
            signals::INT => self.int,
            signals::PIPE => self.pipe,
            signals::TERM => self.term,
            _ => PipeDisposition::Default,
        }
    }

    const fn set(&mut self, signal: u8, disposition: PipeDisposition) {
        match signal {
            signals::HUP => self.hup = disposition,
            signals::INT => self.int = disposition,
            signals::PIPE => self.pipe = disposition,
            signals::TERM => self.term = disposition,
            _ => {}
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<ProcessState>>> = const { RefCell::new(None) };
    static REGISTRY: RefCell<HashMap<(u64, Pid), Weak<ProcessState>>> =
        RefCell::new(HashMap::new());
}

pub(super) struct ProcessState {
    dispositions: Cell<Dispositions>,
    terminated: Cell<Option<u8>>,
    pending: RefCell<Vec<u8>>,
    handling: Cell<bool>,
    waker: RefCell<Option<Waker>>,
    children: RefCell<Vec<Weak<ProcessState>>>,
}

impl ProcessState {
    fn new(dispositions: Dispositions) -> Rc<Self> {
        let state = Rc::new(Self {
            dispositions: Cell::new(dispositions),
            terminated: Cell::new(None),
            pending: RefCell::new(Vec::new()),
            handling: Cell::new(false),
            waker: RefCell::new(None),
            children: RefCell::new(Vec::new()),
        });
        if let Some(parent) = current() {
            let mut children = parent.children.borrow_mut();
            children.retain(|child| child.strong_count() > 0);
            children.push(Rc::downgrade(&state));
        }
        state
    }

    fn deliver(&self, signal: u8) {
        if signal == signals::KILL {
            self.terminated.set(Some(signals::KILL));
        } else {
            match self.dispositions.get().get(signal) {
                PipeDisposition::Default => {
                    if self.terminated.get().is_none() {
                        self.terminated.set(Some(signal));
                    }
                }
                PipeDisposition::Ignored => return,
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
        state.dispositions.get().pipe
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
    dispositions.pipe = pipe;
    dispositions
}

/// Applies a shell's trap configuration to the running process. Signals with a handler take its
/// disposition, and PIPE always follows the configuration, as before. A caught signal whose
/// handler was removed returns to default. Signals the configuration never mentions keep their
/// inherited disposition, such as a background job's ignored INT.
pub fn apply_trap_dispositions(traps: &TrapHandlerConfig) {
    let Some(state) = current() else {
        return;
    };
    let mut dispositions = state.dispositions.get();
    for (name, number) in [
        ("HUP", signals::HUP),
        ("INT", signals::INT),
        ("PIPE", signals::PIPE),
        ("TERM", signals::TERM),
    ] {
        let Ok(signal) = name.parse() else {
            continue;
        };
        if traps.get_effective_handler(signal).is_some()
            || number == signals::PIPE
            || dispositions.get(number) == PipeDisposition::Caught
        {
            dispositions.set(number, traps.signal_disposition(signal));
        }
    }
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
        if let Some(signal) = self.state.terminated.get() {
            return Poll::Ready(Ok(ExecutionResult::terminated_by_signal(signal)));
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

/// Runs `body` as numbered process `pid` of `table`, reachable by [`signal_process`], and
/// records its final status in the table.
pub async fn run_numbered_process(
    table: &ProcessTable,
    pid: Pid,
    dispositions: Dispositions,
    body: impl Future<Output = Result<ExecutionResult, Error>>,
) -> Result<ExecutionResult, Error> {
    let state = ProcessState::new(dispositions);
    let key = (table.id(), pid);
    REGISTRY.with_borrow_mut(|registry| registry.insert(key, Rc::downgrade(&state)));
    let _registration = Registration(Some(key));
    let observed = state.clone();
    let result = run_state(state, body).await;
    // Only this process's own termination is a signal death; a normal exit that merely returns
    // a killed child's status (143) is an ordinary exit, as for a bash subshell.
    let status = match (observed.terminated.get(), &result) {
        (Some(signal), _) => ProcessStatus::Signaled(signal),
        (None, Ok(result)) => ProcessStatus::Exited(u8::from(result.exit_code)),
        (None, Err(_)) => ProcessStatus::Exited(1),
    };
    table.set_status(pid, status);
    result
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

    fn caught(term: bool) -> Dispositions {
        Dispositions {
            term: if term {
                PipeDisposition::Caught
            } else {
                PipeDisposition::Default
            },
            ..Dispositions::default()
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
            let ignore = Dispositions {
                term: PipeDisposition::Ignored,
                ..Dispositions::default()
            };
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
    fn kill_ignores_dispositions() {
        run(async {
            let table = ProcessTable::new(10, 11);
            let pid = table.allocate(10, "stubborn".into());
            let all_ignored = Dispositions {
                hup: PipeDisposition::Ignored,
                int: PipeDisposition::Ignored,
                pipe: PipeDisposition::Ignored,
                term: PipeDisposition::Ignored,
            };
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
            let leader = Dispositions {
                term: PipeDisposition::Caught,
                hup: PipeDisposition::Ignored,
                ..Dispositions::default()
            };
            let (result, ()) = futures::join!(
                run_numbered_process(&table, pid, leader, async {
                    let inherited = inherited_dispositions(PipeDisposition::Default);
                    assert_eq!(inherited.term, PipeDisposition::Default);
                    assert_eq!(inherited.hup, PipeDisposition::Ignored);
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
            let inherited = Dispositions {
                int: PipeDisposition::Ignored,
                ..Dispositions::default()
            };
            run_numbered_process(&table, pid, inherited, async {
                assert!(process_exists(&table, pid));
                let mut traps = TrapHandlerConfig::default();
                traps.register_handler(
                    "TERM".parse()?,
                    "echo t".into(),
                    crate::SourceInfo::from("test"),
                );
                apply_trap_dispositions(&traps);
                assert_eq!(current_dispositions().term, PipeDisposition::Caught);
                assert_eq!(current_dispositions().int, PipeDisposition::Ignored);
                traps.remove_handlers("TERM".parse()?);
                apply_trap_dispositions(&traps);
                assert_eq!(current_dispositions().term, PipeDisposition::Default);
                Ok(ExecutionResult::success())
            })
            .await
            .unwrap();
            assert!(!process_exists(&table, pid));
        });
    }
}
