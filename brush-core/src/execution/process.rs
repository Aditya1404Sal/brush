//! Logical process ownership for single-threaded WASM commands.
//!
//! Pipe writes report to the process being polled, rather than to the process that
//! created the pipe. Inherited descriptors therefore cannot kill an ancestor.

use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

use crate::{Error, ExecutionResult, traps::PipeDisposition};

thread_local! {
    static CURRENT: RefCell<Option<Rc<ProcessState>>> = const { RefCell::new(None) };
}

pub(super) struct ProcessState {
    disposition: Cell<PipeDisposition>,
    terminated: Cell<bool>,
    pending: Cell<bool>,
    handling: Cell<bool>,
    waker: RefCell<Option<Waker>>,
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
    current().map_or(PipeDisposition::Default, |scope| scope.disposition.get())
}

/// Updates the running process after a shell trap configuration change.
pub fn set_pipe_disposition(disposition: PipeDisposition) {
    if let Some(scope) = current() {
        scope.disposition.set(disposition);
    }
}

/// Records a failed, nonempty write. No handler executes in the I/O callback.
pub(crate) fn record_broken_pipe() {
    if let Some(scope) = current() {
        match scope.disposition.get() {
            PipeDisposition::Default => scope.terminated.set(true),
            PipeDisposition::Caught if !scope.handling.get() => scope.pending.set(true),
            PipeDisposition::Caught | PipeDisposition::Ignored => {}
        }
        let waker = scope.waker.borrow().clone();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

pub(crate) fn take_pending_pipe_trap() -> bool {
    current().is_some_and(|scope| scope.pending.replace(false))
}

pub(crate) struct HandlingPipe(Option<Rc<ProcessState>>);
impl Drop for HandlingPipe {
    fn drop(&mut self) {
        if let Some(scope) = self.0.take() {
            scope.handling.set(false);
        }
    }
}

pub(crate) fn handling_pipe() -> HandlingPipe {
    let scope = current();
    if let Some(scope) = &scope {
        scope.handling.set(true);
    }
    HandlingPipe(scope)
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
        if self.state.terminated.get() {
            return Poll::Ready(Ok(ExecutionResult::terminated_by_signal(13)));
        }
        let _process = install(Some(self.state.clone()));
        let _tasks = super::RestoreScope(super::CURRENT_SCOPE.replace(Some(self.tasks.clone())));
        let result = self.body.as_mut().poll(cx);
        // A failed write can happen in the very poll that completes the body.
        if self.state.terminated.get() {
            Poll::Ready(Ok(ExecutionResult::terminated_by_signal(13)))
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

/// Runs a borrowed command future in a new logical process and joins its descendants.
/// A failed write with default SIGPIPE disposition ends this process with status 141.
pub async fn run_process(
    disposition: PipeDisposition,
    body: impl Future<Output = Result<ExecutionResult, Error>>,
) -> Result<ExecutionResult, Error> {
    let tasks = super::TaskScope::nested();
    let _abort = AbortScope(tasks.clone());
    let result = ProcessFuture {
        body: Box::pin(body),
        state: Rc::new(ProcessState {
            disposition: Cell::new(disposition),
            terminated: Cell::new(false),
            pending: Cell::new(false),
            handling: Cell::new(false),
            waker: RefCell::new(None),
        }),
        tasks: tasks.clone(),
    }
    .await;
    tasks.cancel_and_join().await;
    result
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
                        take_pending_pipe_trap(),
                        disposition == PipeDisposition::Caught
                    );
                    assert!(!take_pending_pipe_trap());
                    let _handling = handling_pipe();
                    let _ = writer.write(b"x");
                    assert!(!take_pending_pipe_trap());
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
}
