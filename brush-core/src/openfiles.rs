//! Managing files open within a shell instance.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::process::Stdio;
use std::sync::Arc;

use crate::ShellFd;
use crate::error;
use crate::sys;

/// A trait representing a stream that can be read from and written to.
/// This is used for custom stream implementations in `OpenFile`.
///
/// Types that implement this trait are expected to be cloneable via the
/// `clone_box` function.
pub trait Stream: std::io::Read + std::io::Write + Send + Sync {
    /// Clones the stream into a boxed trait object.
    fn clone_box(&self) -> Box<dyn Stream>;

    /// Converts the stream into an `OwnedFd`. Returns an error if the operation
    /// is not supported or if it fails.
    #[cfg(unix)]
    fn try_clone_to_owned(&self) -> Result<std::os::fd::OwnedFd, error::Error>;

    /// Borrows the stream as a `BorrowedFd`. Returns an error if the operation
    /// is not supported or if it fails.
    #[cfg(unix)]
    fn try_borrow_as_fd(&self) -> Result<std::os::fd::BorrowedFd<'_>, error::Error>;

    /// Returns the stream as [`std::any::Any`], letting the shell recognize stream types it
    /// provides itself. Custom streams can keep the default, which returns `None`.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
}

/// Represents a file open in a shell context.
///
/// File- and pipe-backed variants hold their handle behind an [`Arc`], so cloning an `OpenFile`
/// shares the underlying descriptor by reference count. The shell opens a fresh context for each
/// subshell, command substitution, background job, and function call and runs them as in-process
/// tasks against one process-wide descriptor table (rather than via `fork(2)` like a traditional
/// shell); sharing the descriptor keeps deeply nested or highly concurrent execution from
/// exhausting that table. A descriptor is duplicated for real only when an independently owned
/// copy is needed to hand to an external child process — see [`OpenFile::try_clone_to_owned`] and
/// the `Stdio` conversion below.
pub enum OpenFile {
    /// The original standard input this process was started with.
    Stdin(std::io::Stdin),
    /// The original standard output this process was started with.
    Stdout(std::io::Stdout),
    /// The original standard error this process was started with.
    Stderr(std::io::Stderr),
    /// A file open for reading or writing.
    File(Arc<std::fs::File>),
    /// A read end of a pipe.
    PipeReader(Arc<std::io::PipeReader>),
    /// A write end of a pipe.
    PipeWriter(Arc<std::io::PipeWriter>),
    /// A custom stream.
    Stream(Box<dyn Stream>),
}

#[cfg(feature = "serde")]
impl serde::Serialize for OpenFile {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Stdin(_) => serializer.serialize_str("stdin"),
            Self::Stdout(_) => serializer.serialize_str("stdout"),
            Self::Stderr(_) => serializer.serialize_str("stderr"),
            Self::File(_) => serializer.serialize_str("file"),
            Self::PipeReader(_) => serializer.serialize_str("pipe_reader"),
            Self::PipeWriter(_) => serializer.serialize_str("pipe_writer"),
            Self::Stream(_) => serializer.serialize_str("stream"),
        }
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for OpenFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "stdin" => return Ok(std::io::stdin().into()),
            "stdout" => return Ok(std::io::stdout().into()),
            "stderr" => return Ok(std::io::stderr().into()),
            "file" => (),
            "pipe_reader" => (),
            "pipe_writer" => (),
            "stream" => (),
            _ => return Err(serde::de::Error::custom("invalid open file")),
        }

        // TODO(serde): Figure out something better to do with open pipes and files.
        null().map_err(serde::de::Error::custom)
    }
}

/// Returns an open file that will discard all I/O.
pub fn null() -> Result<OpenFile, error::Error> {
    let file = sys::fs::open_null_file()?;
    Ok(file.into())
}

impl Clone for OpenFile {
    fn clone(&self) -> Self {
        match self {
            Self::Stdin(_) => std::io::stdin().into(),
            Self::Stdout(_) => std::io::stdout().into(),
            Self::Stderr(_) => std::io::stderr().into(),
            // File and pipe handles are shared by reference count; cloning never issues a
            // syscall and so cannot fail.
            Self::File(f) => Self::File(Arc::clone(f)),
            Self::PipeReader(r) => Self::PipeReader(Arc::clone(r)),
            Self::PipeWriter(w) => Self::PipeWriter(Arc::clone(w)),
            Self::Stream(s) => Self::Stream(s.clone_box()),
        }
    }
}

impl std::fmt::Display for OpenFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdin(_) => write!(f, "stdin"),
            Self::Stdout(_) => write!(f, "stdout"),
            Self::Stderr(_) => write!(f, "stderr"),
            Self::File(_) => write!(f, "file"),
            Self::PipeReader(_) => write!(f, "pipe reader"),
            Self::PipeWriter(_) => write!(f, "pipe writer"),
            Self::Stream(_) => write!(f, "stream"),
        }
    }
}

impl OpenFile {
    /// Converts the open file into an `OwnedFd`. For shared file/pipe handles this materializes
    /// a real duplicate via `dup(2)` so the caller receives an independently owned descriptor.
    #[cfg(unix)]
    pub(crate) fn try_clone_to_owned(self) -> Result<std::os::fd::OwnedFd, error::Error> {
        use std::os::fd::AsFd as _;

        match self {
            Self::Stdin(f) => Ok(f.as_fd().try_clone_to_owned()?),
            Self::Stdout(f) => Ok(f.as_fd().try_clone_to_owned()?),
            Self::Stderr(f) => Ok(f.as_fd().try_clone_to_owned()?),
            Self::File(f) => Ok(f.as_fd().try_clone_to_owned()?),
            Self::PipeReader(r) => Ok(r.as_fd().try_clone_to_owned()?),
            Self::PipeWriter(w) => Ok(w.as_fd().try_clone_to_owned()?),
            Self::Stream(s) => s.try_clone_to_owned(),
        }
    }

    /// Borrows the open file as a `BorrowedFd`.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is not supported for the underlying file type.
    #[cfg(unix)]
    pub fn try_borrow_as_fd(&self) -> Result<std::os::fd::BorrowedFd<'_>, error::Error> {
        use std::os::fd::AsFd as _;

        match self {
            Self::Stdin(f) => Ok(f.as_fd()),
            Self::Stdout(f) => Ok(f.as_fd()),
            Self::Stderr(f) => Ok(f.as_fd()),
            Self::File(f) => Ok(f.as_fd()),
            Self::PipeReader(r) => Ok(r.as_fd()),
            Self::PipeWriter(w) => Ok(w.as_fd()),
            Self::Stream(s) => s.try_borrow_as_fd(),
        }
    }

    pub(crate) fn is_dir(&self) -> bool {
        match self {
            Self::Stdin(_) | Self::Stdout(_) | Self::Stderr(_) => false,
            Self::File(file) => file.metadata().is_ok_and(|m| m.is_dir()),
            Self::PipeReader(_) | Self::PipeWriter(_) | Self::Stream(_) => false,
        }
    }

    /// Checks if the open file is associated with a terminal.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Stdin(f) => f.is_terminal(),
            Self::Stdout(f) => f.is_terminal(),
            Self::Stderr(f) => f.is_terminal(),
            Self::File(f) => f.is_terminal(),
            Self::PipeReader(_) | Self::PipeWriter(_) | Self::Stream(_) => false,
        }
    }
}

impl From<std::io::Stdin> for OpenFile {
    /// Creates an `OpenFile` from standard input.
    fn from(stdin: std::io::Stdin) -> Self {
        Self::Stdin(stdin)
    }
}

impl From<std::io::Stdout> for OpenFile {
    /// Creates an `OpenFile` from standard output.
    fn from(stdout: std::io::Stdout) -> Self {
        Self::Stdout(stdout)
    }
}

impl From<std::io::Stderr> for OpenFile {
    /// Creates an `OpenFile` from standard error.
    fn from(stderr: std::io::Stderr) -> Self {
        Self::Stderr(stderr)
    }
}

impl From<std::fs::File> for OpenFile {
    fn from(file: std::fs::File) -> Self {
        Self::File(Arc::new(file))
    }
}

impl From<std::io::PipeReader> for OpenFile {
    fn from(reader: std::io::PipeReader) -> Self {
        Self::PipeReader(Arc::new(reader))
    }
}

impl From<std::io::PipeWriter> for OpenFile {
    fn from(writer: std::io::PipeWriter) -> Self {
        Self::PipeWriter(Arc::new(writer))
    }
}

impl TryFrom<OpenFile> for Stdio {
    type Error = error::Error;

    fn try_from(open_file: OpenFile) -> Result<Self, Self::Error> {
        // File and pipe handles are shared behind an `Arc`, so the descriptor cannot be moved
        // out; duplicate it to give the child an independently owned descriptor. Duplication can
        // fail (e.g. under descriptor exhaustion), so the conversion is fallible and the error is
        // surfaced to the caller rather than silently degrading the child's streams.
        match open_file {
            OpenFile::Stdin(_) | OpenFile::Stdout(_) | OpenFile::Stderr(_) => Ok(Self::inherit()),
            OpenFile::File(f) => Ok(f.try_clone()?.into()),
            OpenFile::PipeReader(r) => Ok(r.try_clone()?.into()),
            OpenFile::PipeWriter(w) => Ok(w.try_clone()?.into()),
            // Custom streams have no descriptor to hand to a child process.
            OpenFile::Stream(_) => Ok(Self::null()),
        }
    }
}

impl std::io::Read for OpenFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Stdin(f) => f.read(buf),
            Self::Stdout(_) => Err(std::io::Error::other(
                error::ErrorKind::OpenFileNotReadable("stdout"),
            )),
            Self::Stderr(_) => Err(std::io::Error::other(
                error::ErrorKind::OpenFileNotReadable("stderr"),
            )),
            // The handle is shared behind an `Arc`; read through a shared reference (`&File`
            // and `&PipeReader` both implement `Read`).
            Self::File(f) => f.as_ref().read(buf),
            Self::PipeReader(reader) => reader.as_ref().read(buf),
            Self::PipeWriter(_) => Err(std::io::Error::other(
                error::ErrorKind::OpenFileNotReadable("pipe writer"),
            )),
            Self::Stream(s) => s.read(buf),
        }
    }
}

impl std::io::Write for OpenFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Stdin(_) => Err(std::io::Error::other(
                error::ErrorKind::OpenFileNotWritable("stdin"),
            )),
            Self::Stdout(f) => f.write(buf),
            Self::Stderr(f) => f.write(buf),
            // The handle is shared behind an `Arc`; write through a shared reference (`&File`
            // and `&PipeWriter` both implement `Write`).
            Self::File(f) => f.as_ref().write(buf),
            Self::PipeReader(_) => Err(std::io::Error::other(
                error::ErrorKind::OpenFileNotWritable("pipe reader"),
            )),
            Self::PipeWriter(writer) => writer.as_ref().write(buf),
            Self::Stream(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Stdin(_) => Ok(()),
            Self::Stdout(f) => f.flush(),
            Self::Stderr(f) => f.flush(),
            Self::File(f) => f.as_ref().flush(),
            Self::PipeReader(_) => Ok(()),
            Self::PipeWriter(writer) => writer.as_ref().flush(),
            Self::Stream(s) => s.flush(),
        }
    }
}

/// What a builtin needs to find in its input before it starts reading, when that input is an
/// in-memory pipe fed by another pipeline stage. See [`wait_for_input`].
///
/// With neither `delimiter` nor `min_bytes` set, the input is ready only at end-of-stream.
#[derive(Clone, Copy, Debug, Default)]
pub struct InputReadiness {
    /// Ready once this byte is buffered (see `min_delimiters`).
    pub delimiter: Option<u8>,
    /// How many `delimiter`s must be buffered; 0 counts as 1.
    pub min_delimiters: usize,
    /// When set, a `delimiter` preceded by an odd number of backslashes is escaped and does not
    /// count.
    pub backslash_escapes: bool,
    /// Ready once at least this many bytes are buffered.
    pub min_bytes: Option<usize>,
}

/// Waits until `file` holds enough input for a synchronous read to proceed without blocking.
///
/// Pipeline stages on `wasm32` share one thread and connect through in-memory pipes, and a
/// builtin's synchronous read cannot wait for another stage to produce. A builtin that reads its
/// input calls this first: it returns once `readiness` is satisfied or every writer has closed,
/// letting the producing stage run in the meantime. Returns immediately for every other kind of
/// file, and on every other target.
#[cfg_attr(
    not(target_arch = "wasm32"),
    allow(
        clippy::unused_async,
        reason = "only in-memory pipes, which exist on wasm32, need to wait"
    )
)]
pub async fn wait_for_input(file: &OpenFile, readiness: InputReadiness) {
    #[cfg(target_arch = "wasm32")]
    if let OpenFile::Stream(stream) = file {
        if let Some(reader) = stream
            .as_any()
            .and_then(|any| any.downcast_ref::<mem_pipe::MemPipeReader>())
        {
            reader.wait_ready(readiness).await;
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    let _ = (file, readiness);
}

/// An in-memory pipe used to connect pipeline stages on `wasm32` targets, where
/// `std::io::pipe()` is unsupported ("operation not supported on this platform") and there is
/// only one thread.
///
/// Pipeline stages run as tasks on that one thread and hand control to each other cooperatively,
/// so neither end of the pipe may block:
///
/// - A write never waits. It fails with `BrokenPipe` once every reader is gone (the stage is then
///   ended, as `SIGPIPE` would end it) or once the buffer would exceed its capacity.
/// - A read never waits either. A reader that must not see a premature end-of-stream first awaits
///   [`MemPipeReader::wait_ready`]; a read that still finds the buffer empty while a writer
///   remains fails with `WouldBlock` rather than silently reporting end-of-stream.
/// - Stages yield after each builtin (see [`yield_to_pipe_peers`]) once more than 64 KiB is
///   buffered — the point at which a producer would block on a full OS pipe, and what lets the
///   next stage start at all when the producer never finishes — and while any reader is parked
///   waiting for a line or a byte count, so the producer and that reader interleave.
///
/// The adapter is pure in-memory logic (no wasm-specific APIs), so it also compiles under `cfg(test)`
/// to be unit-tested natively; only its *use* in the pipeline wiring is `wasm32`-only.
#[cfg(any(target_arch = "wasm32", test))]
mod mem_pipe {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use tokio::sync::Notify;

    /// The default buffer capacity. A producer that outruns a reader waiting for end-of-stream
    /// stops here instead of growing memory without bound.
    pub(crate) const DEFAULT_CAPACITY: usize = 64 * 1024 * 1024;

    /// Once more than this many bytes are buffered across pipes, stages yield after each builtin.
    /// It matches a Linux pipe's default buffer, where a producer would block and the consumer
    /// would get scheduled.
    pub(crate) const SOFT_LIMIT: usize = 64 * 1024;

    thread_local! {
        /// Readers currently parked waiting for a line or a byte count. While non-zero, stages
        /// yield after each builtin so the waiting reader gets a turn.
        static STREAMING_WAITERS: Cell<usize> = const { Cell::new(0) };

        /// Set when a write found no reader, so the writing stage yields once and its watcher can
        /// end it.
        static BROKEN_WRITE: Cell<bool> = const { Cell::new(false) };

        /// Bytes written into pipes on this thread and not yet read or discarded.
        static BUFFERED: Cell<usize> = const { Cell::new(0) };
    }

    /// Returns whether more than [`SOFT_LIMIT`] bytes are buffered across pipes.
    pub(crate) fn over_soft_limit() -> bool {
        BUFFERED.get() > SOFT_LIMIT
    }

    /// Adjusts the buffered-byte count; tolerates thread-local teardown, since pipes can be dropped
    /// while the thread exits.
    fn account(add: usize, remove: usize) {
        let _ = BUFFERED.try_with(|b| b.set(b.get().saturating_add(add).saturating_sub(remove)));
    }

    /// The buffer and the handle counts behind one pipe. End-of-stream is "buffer drained and no
    /// writers remain", so the explicit counts — not the `Arc` strong count, which every handle and
    /// watch contributes to — are what signal EOF and a broken pipe.
    struct Inner {
        buf: VecDeque<u8>,
        writers: usize,
        readers: usize,
        capacity: usize,
        broken: bool,
        overflowed: bool,
    }

    impl Drop for Inner {
        // Bytes nobody read are discarded with the pipe.
        fn drop(&mut self) {
            account(0, self.buf.len());
        }
    }

    struct Shared {
        inner: Mutex<Inner>,
        /// Notified on every state change a waiter could care about: bytes written, the last writer
        /// closed, the pipe broken.
        changed: Notify,
    }

    /// Locks the shared state, recovering from a poisoned mutex: the state is a byte buffer and a
    /// few counters, always left consistent between statements, so a panic elsewhere while
    /// holding the lock cannot have left it half-updated.
    fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Inner> {
        shared
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The write half. Cloning bumps the live-writer count; dropping decrements it, and reaching
    /// zero is the EOF signal to the reader.
    pub(crate) struct MemPipeWriter(Arc<Shared>);

    /// The read half. Drains the shared buffer; reports EOF once the buffer is empty and every
    /// writer handle has been dropped.
    pub(crate) struct MemPipeReader(Arc<Shared>);

    /// Observes a pipe without holding either end, so it never keeps the pipe open.
    pub(crate) struct MemPipeWatch(Arc<Shared>);

    /// Creates a connected (reader, writer) pair sharing one in-memory buffer, plus a watch on it.
    pub(crate) fn pipe(capacity: usize) -> (MemPipeReader, MemPipeWriter, MemPipeWatch) {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                buf: VecDeque::new(),
                writers: 1,
                readers: 1,
                capacity,
                broken: false,
                overflowed: false,
            }),
            changed: Notify::new(),
        });
        (
            MemPipeReader(Arc::clone(&shared)),
            MemPipeWriter(Arc::clone(&shared)),
            MemPipeWatch(shared),
        )
    }

    /// Yields to other tasks when more than [`SOFT_LIMIT`] bytes are buffered, when a stage is
    /// parked waiting for a line or a byte count, or when this task just wrote to a pipe nobody
    /// reads. Called after every builtin.
    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn yield_to_pipe_peers() {
        let broken_write = BROKEN_WRITE.replace(false);
        if broken_write || STREAMING_WAITERS.get() > 0 || over_soft_limit() {
            tokio::task::yield_now().await;
        }
    }

    /// Counts a parked streaming reader for as long as it is alive.
    struct StreamingWaiter;

    impl StreamingWaiter {
        fn new() -> Self {
            STREAMING_WAITERS.set(STREAMING_WAITERS.get() + 1);
            Self
        }
    }

    impl Drop for StreamingWaiter {
        fn drop(&mut self) {
            let _ = STREAMING_WAITERS.try_with(|w| w.set(w.get().saturating_sub(1)));
        }
    }

    /// Returns whether `buf` contains at least `needed` (minimum 1) `delimiter`s not escaped by a
    /// preceding odd run of backslashes (when `backslash_escapes` is set).
    fn contains_delimiters(
        buf: &VecDeque<u8>,
        delimiter: u8,
        needed: usize,
        backslash_escapes: bool,
    ) -> bool {
        let needed = needed.max(1);
        let mut found = 0usize;
        let mut backslashes = 0usize;
        for &byte in buf {
            if byte == delimiter && !(backslash_escapes && backslashes % 2 == 1) {
                found += 1;
                if found >= needed {
                    return true;
                }
            }
            if backslash_escapes && byte == b'\\' {
                backslashes += 1;
            } else {
                backslashes = 0;
            }
        }
        false
    }

    fn is_ready(inner: &Inner, readiness: super::InputReadiness) -> bool {
        inner.writers == 0
            || readiness.min_bytes.is_some_and(|n| inner.buf.len() >= n)
            || readiness.delimiter.is_some_and(|d| {
                contains_delimiters(
                    &inner.buf,
                    d,
                    readiness.min_delimiters,
                    readiness.backslash_escapes,
                )
            })
    }

    impl MemPipeReader {
        /// Waits until `readiness` is satisfied or every writer has closed.
        pub(crate) async fn wait_ready(&self, readiness: super::InputReadiness) {
            let streaming = readiness.delimiter.is_some() || readiness.min_bytes.is_some();
            loop {
                let notified = self.0.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

                if is_ready(&lock(&self.0), readiness) {
                    return;
                }

                // Only a reader that can start before end-of-stream asks producers to yield; a reader
                // waiting for end-of-stream is best served by letting the producer run on.
                let _waiter = streaming.then(StreamingWaiter::new);
                notified.await;
            }
        }
    }

    impl MemPipeWatch {
        /// Resolves once a write has found no reader, or has exceeded the capacity.
        pub(crate) async fn wait_broken(&self) {
            loop {
                let notified = self.0.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

                if lock(&self.0).broken {
                    return;
                }
                notified.await;
            }
        }

        /// Returns whether the pipe broke because a write exceeded its capacity.
        pub(crate) fn overflowed(&self) -> bool {
            lock(&self.0).overflowed
        }
    }

    impl Clone for MemPipeWriter {
        fn clone(&self) -> Self {
            lock(&self.0).writers += 1;
            Self(Arc::clone(&self.0))
        }
    }

    impl Drop for MemPipeWriter {
        fn drop(&mut self) {
            let mut inner = lock(&self.0);
            inner.writers -= 1;
            let closed = inner.writers == 0;
            drop(inner);
            if closed {
                self.0.changed.notify_waiters();
            }
        }
    }

    impl std::io::Write for MemPipeWriter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            let mut inner = lock(&self.0);
            if inner.readers == 0 {
                inner.broken = true;
                drop(inner);
                BROKEN_WRITE.set(true);
                self.0.changed.notify_waiters();
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            if inner.buf.len().saturating_add(data.len()) > inner.capacity {
                inner.broken = true;
                inner.overflowed = true;
                drop(inner);
                BROKEN_WRITE.set(true);
                self.0.changed.notify_waiters();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "in-memory pipe buffer limit exceeded",
                ));
            }
            inner.buf.extend(data.iter().copied());
            drop(inner);
            account(data.len(), 0);
            self.0.changed.notify_waiters();
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // A writer is never read from; surface an EOF rather than an error so generic `copy` loops end.
    impl std::io::Read for MemPipeWriter {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    impl super::Stream for MemPipeWriter {
        fn clone_box(&self) -> Box<dyn super::Stream> {
            Box::new(self.clone())
        }

        // An in-memory pipe has no OS descriptor to materialize. These are never invoked on the
        // wasm pipeline path (they are `#[cfg(unix)]`); implemented for native testability, matching
        // `FailingReaderWriter`.
        #[cfg(unix)]
        fn try_clone_to_owned(&self) -> Result<std::os::fd::OwnedFd, super::error::Error> {
            Err(super::error::ErrorKind::CannotConvertToNativeFd.into())
        }

        #[cfg(unix)]
        fn try_borrow_as_fd(&self) -> Result<std::os::fd::BorrowedFd<'_>, super::error::Error> {
            Err(super::error::ErrorKind::CannotConvertToNativeFd.into())
        }
    }

    impl Clone for MemPipeReader {
        fn clone(&self) -> Self {
            lock(&self.0).readers += 1;
            Self(Arc::clone(&self.0))
        }
    }

    impl Drop for MemPipeReader {
        fn drop(&mut self) {
            lock(&self.0).readers -= 1;
        }
    }

    impl std::io::Read for MemPipeReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut inner = lock(&self.0);
            if inner.buf.is_empty() {
                if inner.writers == 0 {
                    return Ok(0);
                }
                // A writer remains, so this is not end-of-stream. Blocking would stall the only
                // thread, and reporting EOF would silently truncate the input.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "in-memory pipe has no data yet; the reading builtin must first await \
                     `openfiles::wait_for_input`",
                ));
            }
            let n = std::cmp::min(buf.len(), inner.buf.len());
            for (slot, byte) in buf.iter_mut().zip(inner.buf.drain(..n)) {
                *slot = byte;
            }
            drop(inner);
            account(0, n);
            Ok(n)
        }
    }

    // A reader is never written to; surface the same "not writable" behavior as a pipe reader by
    // erroring, matching `OpenFile::PipeReader`'s write arm.
    impl std::io::Write for MemPipeReader {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other(
                super::error::ErrorKind::OpenFileNotWritable("pipe reader"),
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl super::Stream for MemPipeReader {
        fn clone_box(&self) -> Box<dyn super::Stream> {
            Box::new(self.clone())
        }

        #[cfg(unix)]
        fn try_clone_to_owned(&self) -> Result<std::os::fd::OwnedFd, super::error::Error> {
            Err(super::error::ErrorKind::CannotConvertToNativeFd.into())
        }

        #[cfg(unix)]
        fn try_borrow_as_fd(&self) -> Result<std::os::fd::BorrowedFd<'_>, super::error::Error> {
            Err(super::error::ErrorKind::CannotConvertToNativeFd.into())
        }

        fn as_any(&self) -> Option<&dyn std::any::Any> {
            Some(self)
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) use mem_pipe::{MemPipeWatch, yield_to_pipe_peers};

/// Creates an in-memory pipe as a connected `(reader, writer)` pair of [`OpenFile`]s plus a watch
/// on it, for use on `wasm32` where `std::io::pipe()` is unsupported. See [`mem_pipe`].
#[cfg(target_arch = "wasm32")]
pub(crate) fn open_mem_pipe() -> (OpenFile, OpenFile, MemPipeWatch) {
    let (reader, writer, watch) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);
    (
        OpenFile::Stream(Box::new(reader)),
        OpenFile::Stream(Box::new(writer)),
        watch,
    )
}

/// Tristate representing the an `OpenFile` entry in an `OpenFiles` structure.
pub enum OpenFileEntry<'a> {
    /// File descriptor is present and has a valid associated `OpenFile`.
    Open(&'a OpenFile),
    /// File descriptor is explicitly marked as not being mapped to any `OpenFile`.
    NotPresent,
    /// File descriptor is not specified in any way; it may be provided by a
    /// parent context of some kind.
    NotSpecified,
}

/// Represents the open files in a shell context.
#[derive(Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpenFiles {
    /// Maps shell file descriptors to open files.
    files: HashMap<ShellFd, Option<OpenFile>>,
}

impl OpenFiles {
    /// File descriptor used for standard input.
    pub const STDIN_FD: ShellFd = 0;
    /// File descriptor used for standard output.
    pub const STDOUT_FD: ShellFd = 1;
    /// File descriptor used for standard error.
    pub const STDERR_FD: ShellFd = 2;

    /// First file descriptor available for non-stdio files.
    const FIRST_NON_STDIO_FD: ShellFd = 3;
    /// Maximum file descriptor number allowed.
    const MAX_FD: ShellFd = 1024;

    /// Creates a new `OpenFiles` instance populated with stdin, stdout, and stderr
    /// from the host environment.
    pub(crate) fn new() -> Self {
        Self {
            files: HashMap::from([
                (Self::STDIN_FD, Some(std::io::stdin().into())),
                (Self::STDOUT_FD, Some(std::io::stdout().into())),
                (Self::STDERR_FD, Some(std::io::stderr().into())),
            ]),
        }
    }

    /// Updates the open files from the provided iterator of (fd number, `OpenFile`) pairs.
    /// Any existing entries for the provided file descriptors will be overwritten.
    ///
    /// # Arguments
    ///
    /// * `files`: An iterator of (fd number, `OpenFile`) pairs to update the open files with.
    pub fn update_from(&mut self, files: impl Iterator<Item = (ShellFd, OpenFile)>) {
        for (fd, file) in files {
            let _ = self.files.insert(fd, Some(file));
        }
    }

    /// Retrieves the file backing standard input in this context.
    pub fn try_stdin(&self) -> Option<&OpenFile> {
        self.files.get(&Self::STDIN_FD).and_then(|f| f.as_ref())
    }

    /// Retrieves the file backing standard output in this context.
    pub fn try_stdout(&self) -> Option<&OpenFile> {
        self.files.get(&Self::STDOUT_FD).and_then(|f| f.as_ref())
    }

    /// Retrieves the file backing standard error in this context.
    pub fn try_stderr(&self) -> Option<&OpenFile> {
        self.files.get(&Self::STDERR_FD).and_then(|f| f.as_ref())
    }

    /// Tries to remove an open file by its file descriptor. If the file descriptor
    /// is not used, `None` will be returned; otherwise, the removed file will
    /// be returned.
    ///
    /// Arguments:
    ///
    /// * `fd`: The file descriptor to remove.
    pub fn remove_fd(&mut self, fd: ShellFd) -> Option<OpenFile> {
        self.files.insert(fd, None).and_then(|f| f)
    }

    /// Tries to lookup the `OpenFile` associated with a file descriptor.
    /// Returns `None` if the file descriptor is not present.
    ///
    /// Arguments:
    ///
    /// * `fd`: The file descriptor to lookup.
    pub fn try_fd(&self, fd: ShellFd) -> Option<&OpenFile> {
        self.files.get(&fd).and_then(|f| f.as_ref())
    }

    /// Tries to lookup the `OpenFile` associated with a file descriptor. Returns
    /// an `OpenFileEntry` representing the state of the file descriptor.
    ///
    /// Arguments:
    ///
    /// * `fd`: The file descriptor to lookup.
    pub fn fd_entry(&self, fd: ShellFd) -> OpenFileEntry<'_> {
        self.files
            .get(&fd)
            .map_or(OpenFileEntry::NotSpecified, |opt_file| match opt_file {
                Some(f) => OpenFileEntry::Open(f),
                None => OpenFileEntry::NotPresent,
            })
    }

    /// Checks if the given file descriptor is in use.
    pub fn contains_fd(&self, fd: ShellFd) -> bool {
        self.files.contains_key(&fd)
    }

    /// Associates the given file descriptor with the provided file. If the file descriptor
    /// is already in use, the previous file will be returned; otherwise, `None`
    /// will be returned.
    ///
    /// Arguments:
    ///
    /// * `fd`: The file descriptor to associate with the file.
    /// * `file`: The file to associate with the file descriptor.
    pub fn set_fd(&mut self, fd: ShellFd, file: OpenFile) -> Option<OpenFile> {
        self.files.insert(fd, Some(file)).and_then(|f| f)
    }

    /// Iterates over all file descriptors.
    pub fn iter_fds(&self) -> impl Iterator<Item = (ShellFd, &OpenFile)> {
        self.files
            .iter()
            .filter_map(|(fd, file)| file.as_ref().map(|f| (*fd, f)))
    }

    /// Adds a new open file, returning the assigned file descriptor.
    ///
    /// # Arguments
    ///
    /// * `file`: The open file to add.
    pub fn add(&mut self, file: OpenFile) -> Result<ShellFd, error::Error> {
        // Start searching for free file descriptors after the standard ones.
        let mut fd = Self::FIRST_NON_STDIO_FD;
        while self.files.contains_key(&fd) {
            if fd >= Self::MAX_FD {
                return Err(error::ErrorKind::TooManyOpenFiles.into());
            }

            fd += 1;
        }

        self.files.insert(fd, Some(file));
        Ok(fd)
    }
}

impl<I> From<I> for OpenFiles
where
    I: Iterator<Item = (ShellFd, OpenFile)>,
{
    fn from(iter: I) -> Self {
        let files = iter.map(|(fd, file)| (fd, Some(file))).collect();
        Self { files }
    }
}

#[cfg(test)]
mod mem_pipe_tests {
    use super::{InputReadiness, mem_pipe};
    use futures::FutureExt as _;
    use std::io::{Read, Write};

    const LINE: InputReadiness = InputReadiness {
        delimiter: Some(b'\n'),
        min_delimiters: 0,
        backslash_escapes: false,
        min_bytes: None,
    };

    /// Bytes written are read back in order. With the writer still open, an empty read is an
    /// error, not end-of-stream; once the writer drops it is a clean EOF.
    #[test]
    fn round_trips_bytes_then_reports_eof_on_writer_drop() {
        let (mut reader, mut writer, _watch) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);

        writer.write_all(b"hello ").unwrap();
        writer.write_all(b"world").unwrap();

        let mut got = [0u8; 11];
        reader.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello world");

        let mut chunk = [0u8; 4];
        let err = reader.read(&mut chunk).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);

        drop(writer);
        assert_eq!(reader.read(&mut chunk).unwrap(), 0);
    }

    /// A stage that finishes before the next one reads: the reader sees every byte, then EOF.
    #[test]
    fn completed_stage_handoff() {
        let (mut reader, mut writer, _watch) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);

        writer.write_all(b"line1\nline2\nline3\n").unwrap();
        drop(writer);

        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        assert_eq!(out, "line1\nline2\nline3\n");
    }

    /// Cloning a writer keeps the pipe open until *every* writer handle is dropped (EOF is the live
    /// writer count reaching zero, not the buffer `Arc`'s strong count).
    #[test]
    fn eof_waits_for_all_writer_clones_to_drop() {
        use super::Stream as _;
        let (mut reader, writer, _watch) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);
        let mut writer2 = writer.clone_box();

        writer2.write_all(b"x").unwrap();
        drop(writer);

        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 1);
        assert_eq!(&buf[..1], b"x");
        assert_eq!(
            reader.read(&mut buf).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );

        drop(writer2);
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
    }

    /// Writing after every reader is gone fails like `EPIPE` and trips the watch.
    #[test]
    fn write_without_readers_breaks_the_pipe() {
        use super::Stream as _;
        let (reader, mut writer, watch) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);
        let reader2 = reader.clone_box();

        drop(reader);
        writer.write_all(b"still read").unwrap();
        assert!(watch.wait_broken().now_or_never().is_none());

        drop(reader2);
        let err = writer.write_all(b"nobody").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        assert!(watch.wait_broken().now_or_never().is_some());
        assert!(!watch.overflowed());
    }

    /// A write that would exceed the capacity fails, and the watch reports why.
    #[test]
    fn write_beyond_capacity_breaks_the_pipe() {
        let (_reader, mut writer, watch) = mem_pipe::pipe(8);

        writer.write_all(b"12345678").unwrap();
        let err = writer.write_all(b"9").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        assert!(watch.wait_broken().now_or_never().is_some());
        assert!(watch.overflowed());
    }

    /// A line reader becomes ready at the delimiter, a byte-count reader at the count, and every
    /// reader at end-of-stream.
    #[test]
    fn readiness_by_delimiter_count_and_eof() {
        let (reader, mut writer, _watch) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);

        writer.write_all(b"abc").unwrap();
        assert!(reader.wait_ready(LINE).now_or_never().is_none());
        assert!(
            reader
                .wait_ready(InputReadiness::default())
                .now_or_never()
                .is_none()
        );
        let three = InputReadiness {
            min_bytes: Some(3),
            ..InputReadiness::default()
        };
        assert!(reader.wait_ready(three).now_or_never().is_some());

        writer.write_all(b"\n").unwrap();
        assert!(reader.wait_ready(LINE).now_or_never().is_some());

        drop(writer);
        assert!(
            reader
                .wait_ready(InputReadiness::default())
                .now_or_never()
                .is_some()
        );
    }

    /// With backslash escapes on, an escaped delimiter does not end the line; an escaped backslash
    /// before the delimiter does not escape it.
    #[test]
    fn escaped_delimiter_is_not_ready() {
        let escaped = InputReadiness {
            backslash_escapes: true,
            ..LINE
        };

        let (reader, mut writer, _watch) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);
        writer.write_all(b"a\\\n").unwrap();
        assert!(reader.wait_ready(escaped).now_or_never().is_none());
        assert!(reader.wait_ready(LINE).now_or_never().is_some());

        let (reader, mut writer, _watch) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);
        writer.write_all(b"a\\\\\n").unwrap();
        assert!(reader.wait_ready(escaped).now_or_never().is_some());
    }

    /// A reader that needs several lines is ready only once that many delimiters are buffered.
    #[test]
    fn readiness_waits_for_several_delimiters() {
        let three_lines = InputReadiness {
            min_delimiters: 3,
            ..LINE
        };
        let (reader, mut writer, _watch) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);

        writer.write_all(b"1\n2\n").unwrap();
        assert!(reader.wait_ready(LINE).now_or_never().is_some());
        assert!(reader.wait_ready(three_lines).now_or_never().is_none());

        writer.write_all(b"3\n").unwrap();
        assert!(reader.wait_ready(three_lines).now_or_never().is_some());
    }

    /// Buffered bytes count toward the soft limit until they are read or the pipe is dropped.
    #[test]
    fn buffered_bytes_track_the_soft_limit() {
        let (mut reader, mut writer, watch) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);
        assert!(!mem_pipe::over_soft_limit());

        writer
            .write_all(&vec![b'y'; mem_pipe::SOFT_LIMIT + 1])
            .unwrap();
        assert!(mem_pipe::over_soft_limit());

        let mut chunk = [0u8; 2];
        reader.read_exact(&mut chunk).unwrap();
        assert!(!mem_pipe::over_soft_limit());

        writer.write_all(&[b'y'; 8]).unwrap();
        assert!(mem_pipe::over_soft_limit());
        drop((reader, writer, watch));
        assert!(!mem_pipe::over_soft_limit());
    }

    /// A parked line reader stays parked until the delimiter arrives, and is woken by the write.
    #[test]
    fn parked_reader_wakes_on_write() {
        let (reader, mut writer, _watch) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);
        let waker = futures::task::noop_waker_ref();
        let mut cx = std::task::Context::from_waker(waker);

        let mut wait = Box::pin(reader.wait_ready(LINE));
        assert!(wait.poll_unpin(&mut cx).is_pending());

        writer.write_all(b"partial").unwrap();
        assert!(wait.poll_unpin(&mut cx).is_pending());

        writer.write_all(b"\n").unwrap();
        assert!(wait.poll_unpin(&mut cx).is_ready());
    }
}
