//! Execution services for single-threaded shell embedders.
//!
//! A spawned task owns its cancellation and completion. Tasks started while a parent
//! is being polled belong to that parent: cancelling or completing the parent cancels
//! its remaining children and awaits their cleanup before reporting completion.

#![allow(
    clippy::future_not_send,
    reason = "execution services explicitly support local futures on a single thread"
)]

use futures::{
    FutureExt,
    channel::oneshot,
    future::{AbortHandle, Abortable, LocalBoxFuture, Shared},
};
use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    time::Duration,
};

/// Logical process boundaries and synthetic pipe signals for cooperative WASM execution.
#[cfg(any(target_arch = "wasm32", test))]
pub mod process;

/// Futures dispatched by native shells must be `Send`.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}

/// WASM shell futures may carry local executor resources.
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

/// Embedder callbacks used by WASM shells. The default requires a Tokio `LocalSet`.
#[derive(Clone, Copy, Debug)]
pub struct ExecutionServices {
    /// Schedules a local future until completion. Ownership is managed by Brush.
    pub spawn_local: fn(LocalBoxFuture<'static, ()>),
    /// Suspends the current task for the specified duration.
    pub sleep: fn(Duration) -> LocalBoxFuture<'static, ()>,
    /// Gives other ready tasks an opportunity to run.
    pub yield_now: fn() -> LocalBoxFuture<'static, ()>,
}

impl Default for ExecutionServices {
    fn default() -> Self {
        Self {
            spawn_local: |future| {
                tokio::task::spawn_local(future);
            },
            sleep: |duration| Box::pin(tokio::time::sleep(duration)),
            yield_now: || Box::pin(tokio::task::yield_now()),
        }
    }
}

thread_local! {
    static CURRENT_SCOPE: RefCell<Option<Rc<TaskScope>>> = const { RefCell::new(None) };
}

#[derive(Default)]
pub(super) struct TaskScope {
    children: RefCell<Vec<TaskControl>>,
    scopes: RefCell<Vec<Rc<Self>>>,
    done: Cell<bool>,
}

impl Drop for TaskScope {
    fn drop(&mut self) {
        self.abort();
    }
}

impl TaskScope {
    fn abort(&self) {
        for child in self.children.borrow().iter() {
            child.abort();
        }
        for scope in self.scopes.borrow().iter() {
            scope.abort();
        }
    }

    fn cancel_and_join(&self) -> LocalBoxFuture<'_, ()> {
        Box::pin(async {
            loop {
                self.abort();
                // Keep observers registered while awaiting: this cleanup future can itself
                // be cancelled, and its ancestor must still be able to join descendants.
                let children = self.children.borrow().clone();
                let scopes = self.scopes.borrow().clone();
                for child in children {
                    child.join().await;
                }
                for scope in scopes {
                    scope.cancel_and_join().await;
                }
                self.children.borrow_mut().retain(|child| !child.done.get());
                self.scopes.borrow_mut().retain(|scope| !scope.done.get());
                if self.children.borrow().is_empty() && self.scopes.borrow().is_empty() {
                    self.done.set(true);
                    break;
                }
            }
        })
    }

    #[cfg(any(target_arch = "wasm32", test))]
    fn nested() -> Rc<Self> {
        let scope = Rc::new(Self::default());
        CURRENT_SCOPE.with_borrow(|parent| {
            if let Some(parent) = parent {
                let mut scopes = parent.scopes.borrow_mut();
                scopes.retain(|scope| !scope.done.get());
                scopes.push(scope.clone());
            }
        });
        scope
    }
}

struct ScopedFuture<F> {
    future: Pin<Box<F>>,
    scope: Rc<TaskScope>,
    #[cfg(any(target_arch = "wasm32", test))]
    process: Option<Rc<process::ProcessState>>,
}

struct RestoreScope(Option<Rc<TaskScope>>);
impl Drop for RestoreScope {
    fn drop(&mut self) {
        CURRENT_SCOPE.replace(self.0.take());
    }
}

impl<F: Future> Future for ScopedFuture<F> {
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let prior = CURRENT_SCOPE.replace(Some(self.scope.clone()));
        // Restore even if a command panics. No scope is installed across a suspended poll.
        let _restore = RestoreScope(prior);
        #[cfg(any(target_arch = "wasm32", test))]
        let _process = process::install(self.process.clone());
        self.future.as_mut().poll(cx)
    }
}

/// Cancellation and completion observer that never keeps command pipe ends open.
#[derive(Clone)]
pub struct TaskControl {
    abort: AbortHandle,
    finished: Shared<LocalBoxFuture<'static, ()>>,
    done: Rc<Cell<bool>>,
}

impl TaskControl {
    /// Requests cancellation at the next poll. Accepted external effects are not rolled back.
    pub fn abort(&self) {
        self.abort.abort();
    }
    /// Waits until the task and its nested tasks have released their resources.
    pub async fn join(self) {
        self.finished.await;
    }
}

/// A task was cancelled or stopped without delivering a result.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    /// The owner requested cancellation.
    #[error("shell task cancelled")]
    Cancelled,
    /// The executor dropped the task, for example after a panic.
    #[error("shell task ended without a result")]
    Lost,
}

impl TaskError {
    /// Returns whether this is ordinary cancellation.
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

impl From<TaskError> for crate::error::Error {
    fn from(error: TaskError) -> Self {
        std::io::Error::other(error).into()
    }
}

/// An owned local task. Dropping the handle requests cancellation, unless the task was started
/// as a background job its session's job scope owns; awaiting it joins cleanup.
pub struct LocalTaskHandle<T> {
    result: oneshot::Receiver<Result<T, TaskError>>,
    control: TaskControl,
    /// Owned by a job scope, which cancels it; dropping this handle leaves it running.
    #[cfg(any(target_arch = "wasm32", test))]
    adopted: bool,
}

impl<T> LocalTaskHandle<T> {
    /// Returns an independent cancellation/completion observer.
    pub fn abort_handle(&self) -> TaskControl {
        self.control.clone()
    }
    /// Requests cancellation.
    pub fn abort(&self) {
        self.control.abort();
    }
}

impl<T> Drop for LocalTaskHandle<T> {
    fn drop(&mut self) {
        #[cfg(any(target_arch = "wasm32", test))]
        if self.adopted {
            return;
        }
        self.control.abort();
    }
}

impl<T> Future for LocalTaskHandle<T> {
    type Output = Result<T, TaskError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.result
            .poll_unpin(cx)
            .map(|result| result.unwrap_or(Err(TaskError::Lost)))
    }
}

impl ExecutionServices {
    /// Starts an owned local task using this embedder's executor.
    pub fn spawn<T: 'static>(
        &self,
        future: impl Future<Output = T> + 'static,
    ) -> LocalTaskHandle<T> {
        let (abort, registration) = AbortHandle::new_pair();
        let (send, result) = oneshot::channel();
        let (finished_send, finished_recv) = oneshot::channel();
        let control = TaskControl {
            abort,
            finished: async {
                let _ = finished_recv.await;
            }
            .boxed_local()
            .shared(),
            done: Rc::new(Cell::new(false)),
        };
        CURRENT_SCOPE.with_borrow(|parent| {
            if let Some(parent) = parent {
                let mut children = parent.children.borrow_mut();
                children.retain(|child| !child.done.get());
                children.push(control.clone());
            }
        });
        let scope = Rc::new(TaskScope::default());
        let done = control.done.clone();
        #[cfg(any(target_arch = "wasm32", test))]
        let process = process::current();
        (self.spawn_local)(Box::pin(async move {
            let output = std::panic::AssertUnwindSafe(ScopedFuture {
                future: Box::pin(Abortable::new(future, registration)),
                scope: scope.clone(),
                #[cfg(any(target_arch = "wasm32", test))]
                process,
            })
            .catch_unwind()
            .await;
            scope.cancel_and_join().await;
            let output = match output {
                Ok(output) => output.map_err(|_| TaskError::Cancelled),
                Err(_) => Err(TaskError::Lost),
            };
            done.set(true);
            let _ = send.send(output);
            let _ = finished_send.send(());
        }));
        LocalTaskHandle {
            result,
            control,
            #[cfg(any(target_arch = "wasm32", test))]
            adopted: false,
        }
    }
}

/// Target-specific command completion handle.
#[cfg(target_arch = "wasm32")]
pub type CommandTask =
    LocalTaskHandle<Result<crate::results::ExecutionResult, crate::error::Error>>;
/// Native command completion continues to use Tokio's thread-capable task handle.
#[cfg(not(target_arch = "wasm32"))]
pub type CommandTask =
    tokio::task::JoinHandle<Result<crate::results::ExecutionResult, crate::error::Error>>;

#[cfg(test)]
mod tests {
    use super::*;

    struct Released(Rc<Cell<bool>>);
    impl Drop for Released {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    fn run(future: impl Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokio::task::LocalSet::new().run_until(future));
    }

    #[test]
    fn cancelling_parent_joins_nested_tasks_before_returning() {
        run(async {
            let services = ExecutionServices::default();
            let released = Rc::new(Cell::new(false));
            let observed = released.clone();
            let (started_send, started) = oneshot::channel();
            let parent = services.spawn(async move {
                let _child = services.spawn(async move {
                    let _guard = Released(observed);
                    let _ = started_send.send(());
                    futures::future::pending::<()>().await;
                });
                futures::future::pending::<()>().await;
            });
            started.await.unwrap();
            parent.abort();
            assert!(matches!(parent.await, Err(TaskError::Cancelled)));
            assert!(released.get());
        });
    }

    #[test]
    fn parent_completion_cleans_up_unfinished_children() {
        run(async {
            let services = ExecutionServices::default();
            let released = Rc::new(Cell::new(false));
            let observed = released.clone();
            let parent = services.spawn(async move {
                let (started_send, started) = oneshot::channel();
                let _child = services.spawn(async move {
                    let _guard = Released(observed);
                    let _ = started_send.send(());
                    futures::future::pending::<()>().await;
                });
                started.await.unwrap();
                42
            });
            assert_eq!(parent.await.unwrap(), 42);
            assert!(released.get());
        });
    }

    #[test]
    fn cancelling_borrowed_process_during_cleanup_retains_join_observers() {
        run(async {
            struct NotifyDrop(Option<oneshot::Sender<()>>);
            impl Drop for NotifyDrop {
                fn drop(&mut self) {
                    let _ = self.0.take().unwrap().send(());
                }
            }

            let services = ExecutionServices::default();
            let (cleanup_started_send, cleanup_started) = oneshot::channel();
            let (finish_send, finish) = oneshot::channel();
            let (body_dropped_send, body_dropped) = oneshot::channel();
            let done = Rc::new(Cell::new(false));
            let observed = done.clone();
            let (abort, _registration) = AbortHandle::new_pair();
            // A held completion observer lets cancellation interrupt the precise await
            // inside nested cleanup without relying on executor timing or a sleep.
            let descendant = TaskControl {
                abort,
                done: done.clone(),
                finished: async move {
                    let _ = cleanup_started_send.send(());
                    finish.await.unwrap();
                    done.set(true);
                }
                .boxed_local()
                .shared(),
            };
            let mut parent = services.spawn(async move {
                let _body = NotifyDrop(Some(body_dropped_send));
                process::run_process(crate::traps::PipeDisposition::Default, async move {
                    CURRENT_SCOPE.with_borrow(|scope| {
                        scope
                            .as_ref()
                            .unwrap()
                            .children
                            .borrow_mut()
                            .push(descendant);
                    });
                    Ok(crate::ExecutionResult::success())
                })
                .await
            });
            cleanup_started.await.unwrap();
            parent.abort();
            body_dropped.await.unwrap();
            assert!((&mut parent).now_or_never().is_none());
            assert!(!observed.get());
            finish_send.send(()).unwrap();
            assert!(matches!(parent.await, Err(TaskError::Cancelled)));
            assert!(observed.get());
        });
    }

    #[test]
    fn dropping_handle_cancels_and_observer_joins() {
        run(async {
            let services = ExecutionServices::default();
            let released = Rc::new(Cell::new(false));
            let observed = released.clone();
            let (started_send, started) = oneshot::channel();
            let task = services.spawn(async move {
                let _guard = Released(observed);
                let _ = started_send.send(());
                futures::future::pending::<()>().await;
            });
            let control = task.abort_handle();
            started.await.unwrap();
            drop(task);
            control.join().await;
            assert!(released.get());
        });
    }

    #[test]
    fn timer_wait_allows_peer_to_progress() {
        run(async {
            let services = ExecutionServices::default();
            let progress = Rc::new(Cell::new(false));
            let observed = progress.clone();
            let waiting = services.spawn(async move {
                (services.sleep)(Duration::from_millis(5)).await;
                assert!(observed.get());
            });
            let peer = services.spawn(async move {
                progress.set(true);
            });
            waiting.await.unwrap();
            peer.await.unwrap();
        });
    }

    #[test]
    fn endless_producer_through_bounded_copy_terminates_on_early_consumer_exit() {
        run(async {
            use futures::io::{AsyncReadExt, AsyncWriteExt};
            for capacity in [1, 8, 64 * 1024] {
                let services = ExecutionServices::default();
                let (mut input, mut producer_output) = crate::openfiles::test_pipe(capacity);
                let (mut consumer_input, mut output) = crate::openfiles::test_pipe(capacity);
                let peer_progress = Rc::new(Cell::new(false));
                let observed = peer_progress.clone();
                let producer = services.spawn(async move {
                    loop {
                        producer_output.async_io().write_all(b"x\n").await?;
                    }
                    #[allow(unreachable_code)]
                    Ok::<(), std::io::Error>(())
                });
                let copy =
                    services.spawn(async move { futures::io::copy(&mut input, &mut output).await });
                let consumer = services.spawn(async move {
                    let mut byte = [0];
                    consumer_input
                        .async_io()
                        .read_exact(&mut byte)
                        .await
                        .unwrap();
                    assert_eq!(byte, [b'x']);
                });
                let peer = services.spawn(async move {
                    peer_progress.set(true);
                });
                consumer.await.unwrap();
                assert_eq!(
                    copy.await.unwrap().unwrap_err().kind(),
                    std::io::ErrorKind::BrokenPipe
                );
                assert_eq!(
                    producer.await.unwrap().unwrap_err().kind(),
                    std::io::ErrorKind::BrokenPipe
                );
                peer.await.unwrap();
                assert!(observed.get());
            }
        });
    }
}
