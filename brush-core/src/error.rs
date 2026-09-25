//! Error facilities

use std::path::PathBuf;

use crate::{Shell, ShellFd, extensions, results, sys};

/// Unified error type for this crate. Contains just a kind for now,
/// but will be extended later with additional context.
#[derive(thiserror::Error, Debug)]
#[error("{kind}")]
pub struct Error {
    /// The kind of error.
    #[source]
    kind: ErrorKind,

    /// Whether or not the error should be considered a "fatal" error that would
    /// result in abnormal exit of a non-interactive shell.
    fatal: bool,

    /// Whether the error was already shown where it happened, so that whatever handles it
    /// later does not show it again.
    reported: bool,
}

/// Monolithic error type for the shell
#[derive(thiserror::Error, Debug)]
pub enum ErrorKind {
    /// An attempt was made to assign a list to an array member
    #[error("cannot assign list to array member")]
    AssigningListToArrayMember,

    /// A regular expression (`[[ =~ ]]`) that does not compile, with the reason as bash's
    /// regex library words it.
    #[error("invalid regular expression `{0}': {1}")]
    InvalidRegex(String, &'static str),

    /// An attempt was made to convert an associative array to an indexed array.
    #[error("cannot convert associative array to indexed array")]
    ConvertingAssociativeArrayToIndexedArray,

    /// An attempt was made to convert an indexed array to an associative array.
    #[error("cannot convert indexed array to associative array")]
    ConvertingIndexedArrayToAssociativeArray,

    /// An error occurred while sourcing the indicated script file.
    #[error("{}: {}", .0.display(), io_message(.1))]
    FailedSourcingFile(PathBuf, #[source] std::io::Error),

    /// The process or process group does not exist.
    #[error("no such process")]
    NoSuchProcess,

    /// The process or process group exists, but cannot be signaled.
    #[error("operation not permitted")]
    PermissionDenied,

    /// The shell failed to send a signal to a process.
    #[error("failed to send signal to process")]
    FailedToSendSignal,

    /// An attempt was made to assign a value to a special parameter.
    #[error("{0}: cannot assign in this way")]
    CannotAssignToSpecialParameter(String),

    /// Checked expansion error.
    #[error("{0}")]
    CheckedExpansionError(String),

    /// A reference was made to an unknown shell function.
    #[error("function not found: {0}")]
    FunctionNotFound(String),

    /// Command was not found.
    #[error("{0}: command not found")]
    CommandNotFound(String),

    /// Not a builtin.
    #[error("{0}: not a shell builtin")]
    BuiltinNotFound(String),

    /// The working directory does not exist.
    #[error("working directory does not exist: {0}")]
    WorkingDirMissing(PathBuf),

    /// Failed to execute command.
    #[error("failed to execute command '{0}': {1}")]
    FailedToExecuteCommand(String, #[source] std::io::Error),

    /// A command named by a path cannot run, for the reason given (`No such file or directory`,
    /// `Is a directory`), as the kernel's `execve` would say.
    #[error("{0}: {1}")]
    CannotExecutePath(String, &'static str),

    /// A command resolved to a file, which this platform cannot run as a process.
    #[error("{0}: executing files is unsupported in bash-tool")]
    ExecutingFilesUnsupported(String),

    /// History item was not found.
    #[error("history item not found")]
    HistoryItemNotFound,

    /// The requested functionality has not yet been implemented in this shell.
    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),

    /// The requested functionality has not yet been implemented in this shell; it is tracked in a
    /// GitHub issue.
    #[error("not yet implemented: {0}; see https://github.com/reubeno/brush/issues/{1}")]
    UnimplementedAndTracked(&'static str, u32),

    /// An expected environment scope could not be found.
    #[error("missing environment scope")]
    MissingScope,

    /// The environment scope required for a new variable is not available.
    #[error("environment scope required for new variable is not available")]
    MissingScopeForNewVariable,

    /// An unexpected environment scope type was encountered.
    #[error("unexpected environment scope type: expected '{expected}', found '{actual}'")]
    UnexpectedScopeType {
        /// The expected scope type.
        expected: crate::env::EnvironmentScope,
        /// The actual scope type.
        actual: crate::env::EnvironmentScope,
    },

    /// The given path is not a directory.
    #[error("not a directory: {0}")]
    NotADirectory(PathBuf),

    /// The given path is a directory.
    #[error("path is a directory")]
    IsADirectory,

    /// The given variable is not an array.
    #[error("variable is not an array")]
    NotArray,

    /// The current user could not be determined.
    #[error("no current user")]
    NoCurrentUser,

    /// The requested input or output redirection is invalid.
    #[error("invalid redirection target")]
    InvalidRedirection,

    /// A redirection target expanded to no word or to several (`> $empty`).
    #[error("{0}: ambiguous redirect")]
    AmbiguousRedirect(String),

    /// An error occurred while redirecting input or output with the given file.
    #[error("{0}: {1}")]
    RedirectionFailure(String, String),

    /// An error occurred evaluating an arithmetic expression.
    #[error("{0}")]
    EvalError(crate::arithmetic::EvalError),

    /// An error in an indexed array's subscript: bash reports it without the command's name and
    /// ends the shell.
    #[error("{0}")]
    ArithmeticSubscript(crate::arithmetic::EvalError),

    /// The given string could not be parsed as an integer.
    #[error("failed to parse '{s}' as a {int_type_name}, base-{radix} integer: {inner}")]
    IntParseError {
        /// The string that failed to parse.
        s: String,
        /// The integer type being parsed.
        int_type_name: &'static str,
        /// The radix (base) used for parsing.
        radix: u32,
        /// The underlying parse error.
        inner: std::num::ParseIntError,
    },

    /// The given integer could not be converted to the target type.
    #[error("integer conversion error")]
    TryIntParseError(#[from] std::num::TryFromIntError),

    /// A byte sequence could not be decoded as a valid UTF-8 string.
    #[error("failed to decode utf-8")]
    FromUtf8Error(#[from] std::string::FromUtf8Error),

    /// A byte sequence could not be decoded as a valid UTF-8 string.
    #[error("failed to decode utf-8")]
    Utf8Error(#[from] std::str::Utf8Error),

    /// An attempt was made to modify a readonly variable.
    #[error("cannot mutate readonly variable")]
    ReadonlyVariable,

    /// An attempt was made to assign to the named readonly variable.
    #[error("{0}: readonly variable")]
    ReadonlyVariableNamed(String),

    /// The indicated pattern is invalid.
    #[error("invalid pattern: '{0}'")]
    InvalidPattern(String),

    /// A regular expression error occurred
    #[error("regex error: {0}")]
    RegexError(#[from] fancy_regex::Error),

    /// An invalid regular expression was provided.
    #[error("invalid regex: {0}; expression: '{1}'")]
    InvalidRegexError(fancy_regex::Error, String),

    /// An I/O error occurred.
    #[error("{}", io_message(.0))]
    IoError(#[from] std::io::Error),

    /// Invalid substitution syntax; holds bash's message (`${v:}: bad substitution`).
    #[error("{0}")]
    BadSubstitution(String),

    /// An indirect expansion's value is not a name to expand.
    #[error("{0}: invalid variable name")]
    InvalidVariableName(String),

    /// `${!ref}` of a name reference that cannot be followed (`local -n v=v`).
    #[error("{0}: invalid indirect expansion")]
    InvalidIndirectExpansion(String),

    /// An error occurred while creating a child process.
    #[error("failed to create child process")]
    ChildCreationFailure,

    /// An error occurred while formatting a string.
    #[error(transparent)]
    FormattingError(#[from] std::fmt::Error),

    /// An error occurred while parsing.
    #[error("{1}: {0}")]
    ParseError(crate::parser::ParseError, crate::SourceInfo),

    /// A syntax error in bash's words (see [`brush_parser::bash_diagnostic`]): each line follows
    /// the `NAME: ORIGIN: ` prefix, where ORIGIN names what was being parsed (`-c`, `eval`, a
    /// script path, `exit trap`).
    #[error("{origin}: {}", .lines.join("\n"))]
    SyntaxError {
        /// What was being parsed.
        origin: String,
        /// The diagnostic lines, without prefix.
        lines: Vec<String>,
    },

    /// An error occurred while parsing a function body.
    #[error("{0}: {1}")]
    FunctionParseError(String, crate::parser::ParseError),

    /// An error occurred while parsing a word.
    #[error(transparent)]
    WordParseError(#[from] crate::parser::WordParseError),

    /// Unable to parse a test command.
    #[error("invalid test command")]
    TestCommandParseError(#[from] crate::parser::TestCommandParseError),

    /// Unable to parse a key binding specification.
    #[error(transparent)]
    BindingParseError(#[from] crate::parser::BindingParseError),

    /// A threading error occurred.
    #[error("threading error")]
    ThreadingError(#[from] tokio::task::JoinError),

    /// An invalid signal was referenced.
    #[error("{0}: invalid signal specification")]
    InvalidSignal(String),

    /// A platform error occurred.
    #[error("platform error: {0}")]
    PlatformError(#[from] sys::PlatformError),

    /// An invalid umask was provided.
    #[error("invalid umask value")]
    InvalidUmask,

    /// The given open file cannot be read from.
    #[error("cannot read from {0}")]
    OpenFileNotReadable(&'static str),

    /// The given open file cannot be written to.
    #[error("cannot write to {0}")]
    OpenFileNotWritable(&'static str),

    /// Bad file descriptor.
    #[error("{0}: Bad file descriptor")]
    BadFileDescriptor(ShellFd),

    /// Printf failure
    #[error("printf failure: {0}")]
    PrintfFailure(i32),

    /// Printf invalid usage
    #[error("printf: {0}")]
    PrintfInvalidUsage(String),

    /// Interrupted
    #[error("interrupted")]
    Interrupted,

    /// A function call would nest deeper than `FUNCNEST` (or the embedder's limit) allows: the
    /// function's name and the nesting level reached.
    #[error("{0}: maximum function nesting level exceeded ({1})")]
    MaxFunctionCallDepthExceeded(String, usize),

    /// A function call would nest deeper than the stack can hold: the function's name and the
    /// nesting level reached.
    #[error(
        "{0}: maximum function nesting level exceeded ({1}): deeper nesting is unsupported in bash-tool"
    )]
    FunctionNestingTooDeep(String, usize),

    /// A command substitution's output exceeded what the shell holds in memory.
    #[error(
        "command substitution: output over {} MiB is unsupported in bash-tool",
        crate::openfiles::MAX_SUBSTITUTION_BYTES >> 20
    )]
    SubstitutionTooLarge,

    /// Execution would nest deeper than the stack can hold.
    #[error("maximum nesting level exceeded: deeper nesting is unsupported in bash-tool")]
    NestingTooDeep,

    /// System time error.
    #[error("system time error: {0}")]
    TimeError(#[from] std::time::SystemTimeError),

    /// A `test` operand that must be an integer is not one.
    #[error("{0}: integer expected")]
    IntegerExpressionExpected(String),

    /// Array index out of range.
    #[error("{0}: bad array subscript")]
    ArrayIndexOutOfRange(String),

    /// An element of an array literal whose negative key counts back past the start
    /// (`a=([-1]=x)`): it abandons the top-level command, reported without the command's name.
    #[error("{0}: bad array subscript")]
    BadArrayElement(String),

    /// Unhandled key code.
    #[error("unhandled key code: {0:?}")]
    UnhandledKeyCode(Vec<u8>),

    /// An error occurred in a built-in command.
    #[error("{1}: {0}")]
    BuiltinError(Box<dyn BuiltinError>, String),

    /// The embedder refused to run a prompt string's expansions (see
    /// [`crate::Shell::set_prompt_guard`]); the diagnostic is its own.
    #[error("{0}")]
    PromptRefused(String),

    /// Operation not supported on this platform.
    #[error("operation not supported on this platform: {0}")]
    NotSupportedOnThisPlatform(&'static str),

    /// Command history is not enabled in this shell.
    #[error("command history is not enabled in this shell")]
    HistoryNotEnabled,

    /// Expanding an unset variable.
    #[error("{0}: unbound variable")]
    ExpandingUnsetVariable(String),

    /// An internal error occurred.
    #[error("internal shell error: {0}")]
    InternalError(String),

    /// Attempted to perform an operation that requires an interactive session.
    #[error("operation requires an interactive session")]
    NotInInteractiveSession,

    /// Attempted to perform an operation that requires command-string mode.
    #[error("operation requires command-string mode")]
    NotExecutingCommandString,

    /// Too much data was provided to an operation.
    #[error("too much data")]
    TooMuchData,

    /// Cannot convert open file to native file descriptor.
    #[error("cannot convert open file to native file descriptor")]
    CannotConvertToNativeFd,

    /// History file is too large to import.
    #[error("history file is too large to import")]
    HistoryFileTooLargeToImport,

    /// Too many open files.
    #[error("too many open files")]
    TooManyOpenFiles,

    /// The function name shadows a special built-in command.
    #[error("function name '{}' shadows a special built-in command", .name)]
    FunctionNameShadowsSpecialBuiltin {
        /// Name of the function.
        name: String,
    },

    /// A glob pattern failed to match any files (failglob).
    #[error("no match: {0}")]
    NoMatch(String),
}

/// Trait implementable by built-in commands to represent errors.
pub trait BuiltinError: std::error::Error + ConvertibleToExitCode + Send + Sync {
    /// Try to extract a reference to the underlying `std::io::Error`, if any.
    /// Implementations should return `None` if there is no inner I/O error.
    /// They should not attempt to *synthesize* an I/O error if one does not
    /// naturally exist.
    fn as_io_error(&self) -> Option<&std::io::Error> {
        None
    }
}

impl BuiltinError for Error {
    fn as_io_error(&self) -> Option<&std::io::Error> {
        self.as_io_error()
    }
}

/// Helper trait for converting values to exit codes.
pub trait ConvertibleToExitCode {
    /// Converts to an exit code.
    fn as_exit_code(&self) -> results::ExecutionExitCode;
}

impl<T> ConvertibleToExitCode for T
where
    results::ExecutionExitCode: for<'a> From<&'a T>,
{
    fn as_exit_code(&self) -> results::ExecutionExitCode {
        self.into()
    }
}

/// `execve`'s reason for a path that names nothing.
pub(crate) const NO_SUCH_FILE: &str = "No such file or directory";

impl From<&ErrorKind> for results::ExecutionExitCode {
    fn from(value: &ErrorKind) -> Self {
        match value {
            ErrorKind::CommandNotFound(..) => Self::NotFound,
            ErrorKind::Unimplemented(..) | ErrorKind::UnimplementedAndTracked(..) => {
                Self::Unimplemented
            }
            ErrorKind::ParseError(..) | ErrorKind::SyntaxError { .. } => Self::InvalidUsage,
            ErrorKind::FunctionParseError(..) => Self::InvalidUsage,
            ErrorKind::TestCommandParseError(..) => Self::InvalidUsage,
            ErrorKind::IntegerExpressionExpected(..) => Self::InvalidUsage,
            ErrorKind::PromptRefused(..) => Self::InvalidUsage,
            ErrorKind::FailedToExecuteCommand(..) => Self::CannotExecute,
            ErrorKind::CannotExecutePath(_, reason) if *reason == NO_SUCH_FILE => Self::NotFound,
            ErrorKind::CannotExecutePath(..) => Self::CannotExecute,
            // Found but not run: bash's status for a file it cannot execute.
            ErrorKind::ExecutingFilesUnsupported(..) => Self::CannotExecute,
            ErrorKind::FunctionNameShadowsSpecialBuiltin { .. } => Self::InvalidUsage,
            ErrorKind::IoError(io_err) => io_err.into(),
            ErrorKind::BuiltinError(inner, ..) => inner.as_exit_code(),
            _ => Self::GeneralError,
        }
    }
}

impl From<&std::io::Error> for results::ExecutionExitCode {
    fn from(io_err: &std::io::Error) -> Self {
        if io_err.kind() == std::io::ErrorKind::BrokenPipe {
            Self::BrokenPipe
        } else {
            Self::GeneralError
        }
    }
}

impl From<&Error> for results::ExecutionExitCode {
    fn from(error: &Error) -> Self {
        Self::from(&error.kind)
    }
}

impl From<crate::arithmetic::EvalError> for ErrorKind {
    fn from(error: crate::arithmetic::EvalError) -> Self {
        match error {
            crate::arithmetic::EvalError::InSubscript(inner) => Self::ArithmeticSubscript(*inner),
            error => Self::EvalError(error),
        }
    }
}

impl<T> From<T> for Error
where
    ErrorKind: From<T>,
{
    fn from(convertible_to_kind: T) -> Self {
        Self {
            kind: convertible_to_kind.into(),
            fatal: false,
            reported: false,
        }
    }
}

impl Error {
    /// Marks this error as fatal.
    #[must_use]
    pub const fn into_fatal(mut self) -> Self {
        self.fatal = true;
        self
    }

    /// Marks the error as already shown where it happened.
    #[must_use]
    pub const fn into_reported(mut self) -> Self {
        self.reported = true;
        self
    }

    /// Whether the error was already shown where it happened.
    pub const fn is_reported(&self) -> bool {
        self.reported
    }

    /// Whether the error abandons the rest of the top-level command, as assigning to a
    /// readonly variable does in bash: it passes through function calls rather than becoming
    /// the call's status. Only an assignment statement does this, and it reports the error
    /// where it happens; a builtin or a loop that cannot assign the variable just fails.
    pub const fn abandons_command(&self) -> bool {
        // A failed glob under failglob, and an indirect expansion of a value that is no name, do
        // the same, reported where the top-level command ends.
        (matches!(self.kind, ErrorKind::ReadonlyVariableNamed(_)) && self.reported)
            || matches!(
                self.kind,
                ErrorKind::NoMatch(_)
                    | ErrorKind::InvalidVariableName(_)
                    | ErrorKind::InvalidIndirectExpansion(_)
                    | ErrorKind::BadArrayElement(_)
            )
    }

    /// The reason a path could not be used, as the system words it ("No such file or
    /// directory", "Not a directory"), for a diagnostic that names the path itself.
    pub fn path_reason(&self) -> String {
        match &self.kind {
            ErrorKind::IoError(error) => io_message(error),
            ErrorKind::NotADirectory(_) => "Not a directory".to_owned(),
            ErrorKind::WorkingDirMissing(_) => "No such file or directory".to_owned(),
            kind => kind.to_string(),
        }
    }

    /// The arithmetic error this error carries, or the error itself when it is not one.
    ///
    /// # Errors
    ///
    /// Returns the error itself when it is not an arithmetic error.
    pub fn into_eval_error(self) -> Result<crate::arithmetic::EvalError, Self> {
        match self.kind {
            ErrorKind::EvalError(error) => Ok(error),
            _ => Err(self),
        }
    }

    /// Returns whether or not this error is fatal.
    pub const fn is_fatal(&self) -> bool {
        self.fatal || matches!(self.kind, ErrorKind::ArithmeticSubscript(_))
    }

    /// Returns a reference to the error kind.
    pub const fn kind(&self) -> &ErrorKind {
        &self.kind
    }

    /// Try to extract a reference to the underlying `std::io::Error`, if any.
    pub fn as_io_error(&self) -> Option<&std::io::Error> {
        match &self.kind {
            ErrorKind::IoError(io_err) => Some(io_err),
            ErrorKind::BuiltinError(inner, _) => inner.as_io_error(),
            _ => None,
        }
    }

    /// Converts this error into the appropriate control flow based on the shell's current state.
    /// This centralizes the logic for determining how fatal errors should affect execution flow.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell instance, used to check interactive mode and script call stack.
    pub fn to_control_flow(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> results::ExecutionControlFlow {
        if self.is_fatal() && !shell.options().interactive {
            results::ExecutionControlFlow::ExitShell
        } else {
            results::ExecutionControlFlow::Normal
        }
    }

    /// Converts this error into an execution result for the shell.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell instance, used to determine control flow.
    pub fn into_result(
        self,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> results::ExecutionResult {
        let next_control_flow = self.to_control_flow(shell);
        // An unset variable (`set -u`, `${x?}`) ends `bash -c` with 127, as in bash; a script
        // read from a file or standard input, `set -e`, or a subshell ended by it gives 1.
        let exit_code = if matches!(next_control_flow, results::ExecutionControlFlow::ExitShell)
            && shell.depth() == shell.process_depth
            && shell.options().command_string_mode
            && !shell.options().exit_on_nonzero_command_exit
            && matches!(
                self.kind,
                ErrorKind::ExpandingUnsetVariable(..) | ErrorKind::CheckedExpansionError(..)
            ) {
            results::ExecutionExitCode::NotFound
        } else {
            results::ExecutionExitCode::from(&self)
        };

        results::ExecutionResult {
            next_control_flow,
            exit_code,
            terminating_signal: None,
        }
    }
}

/// An I/O error's message as bash words it: the system's description, without the
/// ` (os error N)` Rust appends.
pub fn io_message(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.rsplit_once(" (os error ") {
        Some((message, _)) if text.ends_with(')') => message.to_owned(),
        _ => text,
    }
}

/// Convenience function for returning an error for unimplemented functionality.
///
/// # Arguments
///
/// * `msg` - The message to include in the error
pub fn unimp<T>(msg: &'static str) -> Result<T, Error> {
    Err(ErrorKind::Unimplemented(msg).into())
}

/// Convenience function for returning an error for *tracked*, unimplemented functionality.
///
/// # Arguments
///
/// * `msg` - The message to include in the error
/// * `project_issue_id` - The GitHub issue ID where the implementation is tracked.
pub fn unimp_with_issue<T>(msg: &'static str, project_issue_id: u32) -> Result<T, Error> {
    Err(ErrorKind::UnimplementedAndTracked(msg, project_issue_id).into())
}
