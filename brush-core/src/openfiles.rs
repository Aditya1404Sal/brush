//! Managing files open within a shell instance.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::process::Stdio;
use std::sync::Arc;

use crate::ShellFd;
use crate::error;

/// A borrowed asynchronous stream view with no synchronous read/write methods.
pub trait AsyncStream: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin + Send {}
impl<T: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin + Send> AsyncStream for T {}

/// A trait representing a stream that can be read from and written to.
/// This is used for custom stream implementations in `OpenFile`.
///
/// Types that implement this trait are expected to be cloneable via the
/// `clone_box` function.
pub trait Stream: std::io::Read + std::io::Write + Send + Sync {
    /// Polls a read. Ready streams may use the synchronous default; cooperative streams
    /// register the supplied waker when no bytes are available yet.
    fn poll_read(
        &mut self,
        _cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(self.read(buf))
    }

    /// Polls a write, suspending when a cooperative stream has no capacity.
    fn poll_write(
        &mut self,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(self.write(buf))
    }

    /// Reports immediately readable input without consuming it, for `read -t 0`.
    /// Custom streams may return `None` when they do not support this query.
    fn input_ready(&self) -> Option<bool> {
        None
    }

    /// Selects whether this writer reports a synthetic SIGPIPE to its logical process.
    /// A command such as `tee -p` handles `BrokenPipe` itself. Other streams ignore this.
    fn set_broken_pipe_cancellation(&mut self, _enabled: bool) {}

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

    /// Identifies where the stream writes, shared by its clones (as `2>&1` makes), so
    /// [`OpenFile::same_target`] can tell two descriptors write to one place. `None` when the
    /// stream cannot say.
    fn target_id(&self) -> Option<usize> {
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
    #[cfg(target_arch = "wasm32")]
    return Ok(null_sink());
    #[cfg(not(target_arch = "wasm32"))]
    {
        let file = crate::sys::fs::open_null_file()?;
        Ok(file.into())
    }
}

/// The null device as a stream: reads see end-of-file and writes vanish, with nothing written
/// anywhere. WASI has no device files.
pub fn null_sink() -> OpenFile {
    #[derive(Clone)]
    struct Null;
    impl std::io::Read for Null {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }
    impl std::io::Write for Null {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Stream for Null {
        fn input_ready(&self) -> Option<bool> {
            Some(true)
        }
        fn clone_box(&self) -> Box<dyn Stream> {
            Box::new(Self)
        }
        #[cfg(unix)]
        fn try_clone_to_owned(&self) -> Result<std::os::fd::OwnedFd, error::Error> {
            Err(error::ErrorKind::CannotConvertToNativeFd.into())
        }
        #[cfg(unix)]
        fn try_borrow_as_fd(&self) -> Result<std::os::fd::BorrowedFd<'_>, error::Error> {
            Err(error::ErrorKind::CannotConvertToNativeFd.into())
        }
        fn as_any(&self) -> Option<&dyn std::any::Any> {
            Some(self)
        }
    }
    NULL_SINK_TYPE.with(|id| id.set(Some(std::any::TypeId::of::<Null>())));
    OpenFile::Stream(Box::new(Null))
}

thread_local! {
    static NULL_SINK_TYPE: std::cell::Cell<Option<std::any::TypeId>> =
        const { std::cell::Cell::new(None) };
}

/// Whether `file` is the null device made by [`null_sink`].
pub fn is_null_sink(file: &OpenFile) -> bool {
    let OpenFile::Stream(stream) = file else {
        return false;
    };
    let Some(any) = stream.as_any() else {
        return false;
    };
    NULL_SINK_TYPE.with(|id| id.get() == Some(any.type_id()))
}

/// The most a substitution (`$( )`, `<( )`, `>( )`) holds in memory: 16 MiB. Writers past it are
/// refused as a closed pipe refuses them, and whoever reads the cut-off output learns it was cut.
pub const MAX_SUBSTITUTION_BYTES: usize = 16 * 1024 * 1024;

/// The error a reader gets at the end of a process substitution's output that was cut off at
/// [`MAX_SUBSTITUTION_BYTES`].
pub const TRUNCATED_SUBSTITUTION: &str =
    "process substitution output over 16 MiB is unsupported in bash-tool";

/// Creates a read-only shared byte stream, suitable for fully staged here-documents.
/// This storage is invocation input, rather than an undrained bounded pipe.
pub fn from_bytes(bytes: Vec<u8>) -> OpenFile {
    from_bytes_then(bytes, None)
}

/// Like [`from_bytes`], but a read at the end fails with `error` instead of seeing end-of-file:
/// the bytes are a prefix of what should have been there.
pub fn from_bytes_then(bytes: Vec<u8>, error: Option<&'static str>) -> OpenFile {
    struct Bytes(
        Arc<std::sync::Mutex<std::io::Cursor<Vec<u8>>>>,
        Option<&'static str>,
    );
    impl std::io::Read for Bytes {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let read = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read(buf)?;
            match self.1 {
                Some(error) if read == 0 && !buf.is_empty() => Err(std::io::Error::other(error)),
                _ => Ok(read),
            }
        }
    }
    impl std::io::Write for Bytes {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::PermissionDenied.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Stream for Bytes {
        fn input_ready(&self) -> Option<bool> {
            Some(true)
        }
        fn clone_box(&self) -> Box<dyn Stream> {
            Box::new(Self(self.0.clone(), self.1))
        }
        #[cfg(unix)]
        fn try_clone_to_owned(&self) -> Result<std::os::fd::OwnedFd, error::Error> {
            Err(error::ErrorKind::CannotConvertToNativeFd.into())
        }
        #[cfg(unix)]
        fn try_borrow_as_fd(&self) -> Result<std::os::fd::BorrowedFd<'_>, error::Error> {
            Err(error::ErrorKind::CannotConvertToNativeFd.into())
        }
    }
    OpenFile::Stream(Box::new(Bytes(
        Arc::new(std::sync::Mutex::new(std::io::Cursor::new(bytes))),
        error,
    )))
}

/// What a [`memory_sink`] kept, and whether a writer went past its limit.
#[derive(Default)]
pub struct Captured {
    /// The bytes written, up to the limit.
    pub bytes: Vec<u8>,
    /// Whether a write was refused at the limit.
    pub truncated: bool,
}

/// A write-only stream that keeps what is written to it in memory, up to a limit (see
/// [`memory_sink`]).
struct MemorySink(Arc<std::sync::Mutex<Captured>>, usize);

impl std::io::Read for MemorySink {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::PermissionDenied.into())
    }
}

impl std::io::Write for MemorySink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut captured = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let room = self.1.saturating_sub(captured.bytes.len());
        if room == 0 && !buf.is_empty() {
            captured.truncated = true;
            drop(captured);
            #[cfg(target_arch = "wasm32")]
            crate::execution::process::record_broken_pipe();
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        let count = buf.len().min(room);
        captured.bytes.extend_from_slice(&buf[..count]);
        drop(captured);
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Stream for MemorySink {
    fn clone_box(&self) -> Box<dyn Stream> {
        Box::new(Self(self.0.clone(), self.1))
    }
    fn target_id(&self) -> Option<usize> {
        Some(Arc::as_ptr(&self.0).addr())
    }
    #[cfg(unix)]
    fn try_clone_to_owned(&self) -> Result<std::os::fd::OwnedFd, error::Error> {
        Err(error::ErrorKind::CannotConvertToNativeFd.into())
    }
    #[cfg(unix)]
    fn try_borrow_as_fd(&self) -> Result<std::os::fd::BorrowedFd<'_>, error::Error> {
        Err(error::ErrorKind::CannotConvertToNativeFd.into())
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

/// Creates a write-only stream that keeps what is written to it in memory, up to `limit` bytes,
/// and the buffer it writes to. A write past the limit fails as a write to a closed pipe does.
pub fn memory_sink(limit: usize) -> (OpenFile, Arc<std::sync::Mutex<Captured>>) {
    let buffer = Arc::new(std::sync::Mutex::new(Captured::default()));
    (
        OpenFile::Stream(Box::new(MemorySink(buffer.clone(), limit))),
        buffer,
    )
}

/// Whether `file` is a [`memory_sink`] writing to `buffer`.
#[cfg(target_arch = "wasm32")]
pub(crate) fn is_memory_sink_of(file: &OpenFile, buffer: &Arc<std::sync::Mutex<Captured>>) -> bool {
    matches!(file, OpenFile::Stream(stream) if stream
        .as_any()
        .and_then(|any| any.downcast_ref::<MemorySink>())
        .is_some_and(|sink| Arc::ptr_eq(&sink.0, buffer)))
}

impl OpenFile {
    /// Whether `self` and `other` write to the same place: clones of one open file, pipe end or
    /// stream, as `2>&1` makes them.
    pub fn same_target(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Stdout(_), Self::Stdout(_)) | (Self::Stderr(_), Self::Stderr(_)) => true,
            (Self::File(a), Self::File(b)) => Arc::ptr_eq(a, b),
            (Self::PipeWriter(a), Self::PipeWriter(b)) => Arc::ptr_eq(a, b),
            (Self::Stream(a), Self::Stream(b)) => {
                a.target_id().is_some_and(|id| Some(id) == b.target_id())
            }
            _ => false,
        }
    }
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
    /// Borrows an async view. Memory pipes suspend without blocking the executor;
    /// regular files and custom ready streams retain their existing synchronous I/O.
    pub fn async_io(&mut self) -> &mut dyn AsyncStream {
        self
    }

    /// Queries readiness without consuming bytes. Cooperative streams can answer immediately.
    pub fn input_ready(&self) -> Option<bool> {
        match self {
            Self::Stream(stream) => stream.input_ready(),
            Self::File(_) => Some(true),
            _ => None,
        }
    }

    /// Let a command explicitly handle pipe write errors instead of SIGPIPE-style stage cancellation.
    pub fn set_broken_pipe_cancellation(&mut self, enabled: bool) {
        if let Self::Stream(stream) = self {
            stream.set_broken_pipe_cancellation(enabled);
        }
    }

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
            Self::File(f) => f
                .as_ref()
                .read(buf)
                .map_err(|error| directory_read_error(f, error)),
            Self::PipeReader(reader) => reader.as_ref().read(buf),
            Self::PipeWriter(_) => Err(std::io::Error::other(
                error::ErrorKind::OpenFileNotReadable("pipe writer"),
            )),
            Self::Stream(s) => s.read(buf),
        }
    }
}

/// A failed read of a directory, as Linux reports it: WASI says "Bad file descriptor" where
/// Linux says "Is a directory".
fn directory_read_error(file: &std::fs::File, error: std::io::Error) -> std::io::Error {
    #[cfg(any(unix, target_os = "wasi"))]
    if file.metadata().is_ok_and(|metadata| metadata.is_dir()) {
        return std::io::Error::from_raw_os_error(libc::EISDIR);
    }
    #[cfg(not(any(unix, target_os = "wasi")))]
    let _ = file;
    error
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

impl futures::io::AsyncRead for OpenFile {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Stream(stream) => stream.poll_read(cx, buf),
            file => std::task::Poll::Ready(std::io::Read::read(file, buf)),
        }
    }
}

impl futures::io::AsyncWrite for OpenFile {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Stream(stream) => stream.poll_write(cx, buf),
            file => std::task::Poll::Ready(std::io::Write::write(file, buf)),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(std::io::Write::flush(&mut *self))
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // Closing this borrowed view flushes; dropping the owned OpenFile closes its handle.
        self.poll_flush(cx)
    }
}

/// Legacy readiness hints retained for source compatibility.
/// Cooperative pipes are rejected by [`wait_for_input`] regardless of these hints.
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

/// Rejects cooperative pipes before a legacy synchronous stdin command starts.
/// Native streams and regular files retain their existing behavior.
#[deprecated(note = "use command-owned async input through OpenFile::async_io()")]
#[allow(
    clippy::unused_async,
    reason = "compatibility with the former async readiness API"
)]
pub async fn wait_for_input(file: &OpenFile, _readiness: InputReadiness) -> std::io::Result<()> {
    #[cfg(target_arch = "wasm32")]
    return check_synchronous_input(file);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = file;
        Ok(())
    }
}

/// Diagnostic used when a synchronous stdin adapter is connected to a cooperative pipe.
pub const SYNCHRONOUS_PIPE_INPUT_MESSAGE: &str = "synchronous stdin commands cannot consume cooperative pipes; use an async command with OpenFile::async_io()";

#[cfg(any(target_arch = "wasm32", test))]
fn check_synchronous_input(file: &OpenFile) -> std::io::Result<()> {
    if matches!(file, OpenFile::Stream(stream) if stream.as_any().is_some_and(|s| s.is::<mem_pipe::MemPipeReader>()))
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            SYNCHRONOUS_PIPE_INPUT_MESSAGE,
        ))
    } else {
        Ok(())
    }
}

/// A bounded in-memory pipe for single-threaded WASM execution.
/// Async views park on empty/full buffers and register wakeups under the state lock.
/// A synchronous reader of an empty pipe gets `WouldBlock`, never a premature EOF. A synchronous
/// writer (a builtin such as `declare -p` or `type`) cannot wait for the reader, so what it writes
/// is kept past the capacity, up to [`super::MAX_SUBSTITUTION_BYTES`] in all.
/// Last-writer closure delivers EOF after draining; last-reader closure wakes writers with
/// `BrokenPipe`. The state machine is platform independent and tested on the host.
#[cfg(any(target_arch = "wasm32", test))]
mod mem_pipe {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::{Arc, Mutex};

    /// Maximum buffered bytes. Capacity exhaustion parks asynchronous writers.
    pub(crate) const DEFAULT_CAPACITY: usize = 64 * 1024;

    /// The buffer and the handle counts behind one pipe. End-of-stream is "buffer drained and no
    /// writers remain", so the explicit counts — not the `Arc` strong count, which every handle and
    /// other owners contribute to — are what signal EOF and a broken pipe.
    struct Inner {
        buf: VecDeque<u8>,
        writers: usize,
        readers: usize,
        capacity: usize,
        next_id: usize,
        read_wakers: BTreeMap<usize, std::task::Waker>,
        write_wakers: BTreeMap<usize, std::task::Waker>,
    }

    struct Shared {
        inner: Mutex<Inner>,
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
    pub(crate) struct MemPipeWriter(Arc<Shared>, usize, bool);

    /// The read half. Drains the shared buffer; reports EOF once the buffer is empty and every
    /// writer handle has been dropped.
    pub(crate) struct MemPipeReader(Arc<Shared>, usize);

    /// Creates a connected (reader, writer) pair sharing one in-memory buffer.
    pub(crate) fn pipe(capacity: usize) -> (MemPipeReader, MemPipeWriter) {
        assert!(capacity > 0, "pipe capacity must be positive");
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                buf: VecDeque::new(),
                writers: 1,
                readers: 1,
                capacity,
                next_id: 2,
                read_wakers: BTreeMap::new(),
                write_wakers: BTreeMap::new(),
            }),
        });
        (
            MemPipeReader(Arc::clone(&shared), 0),
            MemPipeWriter(shared, 1, true),
        )
    }

    impl Clone for MemPipeWriter {
        fn clone(&self) -> Self {
            let mut inner = lock(&self.0);
            inner.writers += 1;
            let id = inner.next_id;
            inner.next_id += 1;
            drop(inner);
            Self(Arc::clone(&self.0), id, self.2)
        }
    }

    impl Drop for MemPipeWriter {
        fn drop(&mut self) {
            let mut inner = lock(&self.0);
            inner.writers -= 1;
            inner.write_wakers.remove(&self.1);
            let closed = inner.writers == 0;
            let wake = if closed {
                std::mem::take(&mut inner.read_wakers)
            } else {
                BTreeMap::new()
            };
            drop(inner);
            for waker in wake.into_values() {
                waker.wake();
            }
        }
    }

    impl MemPipeWriter {
        fn write_with_waker(
            &self,
            data: &[u8],
            waker: Option<&std::task::Waker>,
        ) -> std::task::Poll<std::io::Result<usize>> {
            use std::task::Poll;
            if data.is_empty() {
                return Poll::Ready(Ok(0));
            }
            let mut inner = lock(&self.0);
            if inner.readers == 0 {
                drop(inner);
                if self.2 {
                    crate::execution::process::record_broken_pipe();
                }
                return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            let mut n = data
                .len()
                .min(inner.capacity.saturating_sub(inner.buf.len()));
            if n == 0 {
                if let Some(waker) = waker {
                    inner.write_wakers.insert(self.1, waker.clone());
                    return Poll::Pending;
                }
                n = data
                    .len()
                    .min(super::MAX_SUBSTITUTION_BYTES.saturating_sub(inner.buf.len()));
                if n == 0 {
                    return Poll::Ready(Err(std::io::Error::other(
                        "synchronous output to a pipe over 16 MiB is unsupported in bash-tool",
                    )));
                }
            }
            inner.buf.extend(data[..n].iter().copied());
            let wake = std::mem::take(&mut inner.read_wakers);
            drop(inner);
            for waker in wake.into_values() {
                waker.wake();
            }
            Poll::Ready(Ok(n))
        }
    }

    impl std::io::Write for MemPipeWriter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            match self.write_with_waker(data, None) {
                std::task::Poll::Ready(result) => result,
                std::task::Poll::Pending => {
                    unreachable!("synchronous writes do not register wakeups")
                }
            }
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
        fn poll_write(
            &mut self,
            cx: &mut std::task::Context<'_>,
            data: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.write_with_waker(data, Some(cx.waker()))
        }

        fn set_broken_pipe_cancellation(&mut self, enabled: bool) {
            self.2 = enabled;
        }

        fn as_any(&self) -> Option<&dyn std::any::Any> {
            Some(self)
        }

        fn target_id(&self) -> Option<usize> {
            Some(Arc::as_ptr(&self.0).addr())
        }

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
            let mut inner = lock(&self.0);
            inner.readers += 1;
            let id = inner.next_id;
            inner.next_id += 1;
            drop(inner);
            Self(Arc::clone(&self.0), id)
        }
    }

    impl Drop for MemPipeReader {
        fn drop(&mut self) {
            let mut inner = lock(&self.0);
            inner.readers -= 1;
            inner.read_wakers.remove(&self.1);
            let closed = inner.readers == 0;
            // Closure wakes writers; only a subsequent failed write reports SIGPIPE.
            let wake = if closed {
                std::mem::take(&mut inner.write_wakers)
            } else {
                BTreeMap::new()
            };
            drop(inner);
            for waker in wake.into_values() {
                waker.wake();
            }
        }
    }

    impl MemPipeReader {
        fn read_with_waker(
            &self,
            buf: &mut [u8],
            waker: Option<&std::task::Waker>,
        ) -> std::task::Poll<std::io::Result<usize>> {
            use std::task::Poll;
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            let mut inner = lock(&self.0);
            if inner.buf.is_empty() {
                if inner.writers == 0 {
                    return Poll::Ready(Ok(0));
                }
                if let Some(waker) = waker {
                    inner.read_wakers.insert(self.1, waker.clone());
                    return Poll::Pending;
                }
                return Poll::Ready(Err(std::io::ErrorKind::WouldBlock.into()));
            }
            let n = buf.len().min(inner.buf.len());
            for (slot, byte) in buf.iter_mut().zip(inner.buf.drain(..n)) {
                *slot = byte;
            }
            let wake = std::mem::take(&mut inner.write_wakers);
            drop(inner);
            for waker in wake.into_values() {
                waker.wake();
            }
            Poll::Ready(Ok(n))
        }
    }

    impl std::io::Read for MemPipeReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.read_with_waker(buf, None) {
                std::task::Poll::Ready(result) => result,
                std::task::Poll::Pending => {
                    unreachable!("synchronous reads do not register wakeups")
                }
            }
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
        fn poll_read(
            &mut self,
            cx: &mut std::task::Context<'_>,
            buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.read_with_waker(buf, Some(cx.waker()))
        }

        fn input_ready(&self) -> Option<bool> {
            let inner = lock(&self.0);
            Some(!inner.buf.is_empty() || inner.writers == 0)
        }
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

/// Creates an in-memory pipe as a connected `(reader, writer)` pair of [`OpenFile`]s for use on `wasm32` where `std::io::pipe()` is unsupported. See [`mem_pipe`].
#[cfg(target_arch = "wasm32")]
pub(crate) fn open_mem_pipe() -> (OpenFile, OpenFile) {
    let (reader, writer) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);
    (
        OpenFile::Stream(Box::new(reader)),
        OpenFile::Stream(Box::new(writer)),
    )
}

#[cfg(test)]
pub(crate) fn test_pipe(capacity: usize) -> (OpenFile, OpenFile) {
    let (reader, writer) = mem_pipe::pipe(capacity);
    (
        OpenFile::Stream(Box::new(reader)),
        OpenFile::Stream(Box::new(writer)),
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
    use super::mem_pipe;
    use futures::FutureExt as _;
    use std::io::{Read, Write};

    /// Bytes written are read back in order. With the writer still open, an empty read is an
    /// error, not end-of-stream; once the writer drops it is a clean EOF.
    #[test]
    fn descriptors_that_share_a_target_are_recognised() {
        let (_, writer) = super::test_pipe(8);
        let (_, other) = super::test_pipe(8);
        assert!(writer.same_target(&writer.clone()));
        assert!(!writer.same_target(&other));
        let (sink, _) = super::memory_sink(64);
        assert!(sink.same_target(&sink.clone()));
        assert!(!sink.same_target(&writer));
        let file = super::OpenFile::File(std::sync::Arc::new(tempfile::tempfile().unwrap()));
        let second = super::OpenFile::File(std::sync::Arc::new(tempfile::tempfile().unwrap()));
        assert!(file.same_target(&file.clone()));
        assert!(!file.same_target(&second));
    }

    #[test]
    fn round_trips_bytes_then_reports_eof_on_writer_drop() {
        let (mut reader, mut writer) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);

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
        let (mut reader, mut writer) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);

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
        let (mut reader, writer) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);
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

    /// Writing after every reader is gone fails like `EPIPE`.
    #[test]
    fn write_without_readers_breaks_the_pipe() {
        use super::Stream as _;
        let (reader, mut writer) = mem_pipe::pipe(mem_pipe::DEFAULT_CAPACITY);
        let reader2 = reader.clone_box();

        drop(reader);
        writer.write_all(b"still read").unwrap();

        drop(reader2);

        let err = writer.write_all(b"nobody").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// A synchronous writer cannot wait for the reader, so a full pipe keeps its bytes anyway, up
    /// to the hard bound.
    #[test]
    fn synchronous_writes_beyond_capacity_are_kept() {
        let (mut reader, mut writer) = mem_pipe::pipe(8);

        writer.write_all(b"12345678").unwrap();
        writer.write_all(b"9").unwrap();
        let mut out = [0; 9];
        reader.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"123456789");

        let big = vec![b'x'; super::MAX_SUBSTITUTION_BYTES];
        writer.write_all(&big).unwrap();
        assert_eq!(
            writer.write_all(b"y").unwrap_err().kind(),
            std::io::ErrorKind::Other
        );
    }

    fn open_pipe(capacity: usize) -> (super::OpenFile, super::OpenFile) {
        let (reader, writer) = mem_pipe::pipe(capacity);
        (
            super::OpenFile::Stream(Box::new(reader)),
            super::OpenFile::Stream(Box::new(writer)),
        )
    }

    #[test]
    fn oversized_async_writes_backpressure_and_drain_before_eof() {
        use futures::io::{AsyncReadExt, AsyncWriteExt};
        for capacity in [1, 8, 64 * 1024] {
            let (mut reader, mut writer) = open_pipe(capacity);
            let payload: Vec<u8> = (0..(capacity * 3 + 19))
                .map(|n| u8::try_from(n % 251).unwrap())
                .collect();
            let mut output = Vec::new();
            let (write, read) = futures::executor::block_on(async {
                futures::join!(
                    async {
                        writer.async_io().write_all(&payload).await?;
                        drop(writer);
                        Ok::<_, std::io::Error>(())
                    },
                    reader.async_io().read_to_end(&mut output)
                )
            });
            write.unwrap();
            read.unwrap();
            assert_eq!(output, payload);
        }
    }

    #[test]
    fn full_writer_wakes_when_last_reader_closes() {
        use futures::io::AsyncWriteExt;
        let (reader, mut writer) = open_pipe(1);
        std::io::Write::write_all(&mut writer, b"a").unwrap();
        let mut write = Box::pin(writer.async_io().write_all(b"b"));
        let wake = std::sync::Arc::new(CountWake::default());
        let waker = futures::task::waker(wake.clone());
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(write.poll_unpin(&mut cx).is_pending());
        drop(reader);
        assert!(wake.0.load(std::sync::atomic::Ordering::Relaxed) > 0);
        assert!(
            matches!(write.poll_unpin(&mut cx), std::task::Poll::Ready(Err(error)) if error.kind() == std::io::ErrorKind::BrokenPipe)
        );
    }

    #[test]
    fn empty_reader_wakes_when_last_writer_closes() {
        use futures::io::AsyncReadExt;
        let (mut reader, writer) = open_pipe(1);
        let writer2 = writer.clone();
        let mut buf = [0; 1];
        let mut read = Box::pin(reader.async_io().read(&mut buf));
        let wake = std::sync::Arc::new(CountWake::default());
        let waker = futures::task::waker(wake.clone());
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(read.poll_unpin(&mut cx).is_pending());
        drop(writer);
        assert!(read.poll_unpin(&mut cx).is_pending());
        drop(writer2);
        assert!(wake.0.load(std::sync::atomic::Ordering::Relaxed) > 0);
        assert!(matches!(
            read.poll_unpin(&mut cx),
            std::task::Poll::Ready(Ok(0))
        ));
    }

    #[test]
    fn synchronous_oversized_write_reports_partial_progress() {
        let (mut reader, mut writer) = mem_pipe::pipe(1);
        assert_eq!(writer.write(b"abc").unwrap(), 1);
        // Full: a synchronous write goes past the capacity rather than failing.
        assert_eq!(writer.write(b"bc").unwrap(), 2);
        let mut buf = [0; 4];
        assert_eq!(reader.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"abc");
    }

    #[test]
    fn legacy_input_rejects_empty_buffered_and_eof_pipes_without_consuming() {
        for capacity in [1, 8, 65_536] {
            let (mut reader, mut writer) = open_pipe(capacity);
            let error = super::check_synchronous_input(&reader).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
            assert_eq!(error.to_string(), super::SYNCHRONOUS_PIPE_INPUT_MESSAGE);
            writer.write_all(b"x").unwrap();
            assert!(super::check_synchronous_input(&reader).is_err());
            drop(writer);
            assert!(super::check_synchronous_input(&reader).is_err());
            let mut byte = [0];
            assert_eq!(reader.read(&mut byte).unwrap(), 1);
            assert_eq!(byte, [b'x']);
            assert_eq!(reader.read(&mut byte).unwrap(), 0);
            assert!(super::check_synchronous_input(&reader).is_err());
        }
        assert!(super::check_synchronous_input(&super::from_bytes(b"input".to_vec())).is_ok());
        let file = tempfile::tempfile().unwrap();
        assert!(super::check_synchronous_input(&super::OpenFile::from(file)).is_ok());
    }

    #[derive(Default)]
    struct CountWake(std::sync::atomic::AtomicUsize);
    impl futures::task::ArcWake for CountWake {
        fn wake_by_ref(arc_self: &std::sync::Arc<Self>) {
            arc_self
                .0
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
