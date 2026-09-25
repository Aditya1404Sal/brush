//! Command execution

use std::{
    borrow::Cow,
    ffi::OsStr,
    fmt::Display,
    path::{Path, PathBuf},
    process::Stdio,
};

use brush_parser::ast;
use itertools::Itertools;
use sys::commands::{CommandExt, CommandFdInjectionExt, CommandFgControlExt};

#[cfg(not(target_arch = "wasm32"))]
use crate::ExecutionExitCode;

use crate::{
    ErrorKind, ExecutionControlFlow, ExecutionParameters, ExecutionResult, Shell, ShellFd,
    builtins, error, escape,
    extensions::{self, ShellExtensions},
    functions,
    interp::{self, Execute, ProcessGroupPolicy},
    openfiles::{self, OpenFile, OpenFiles},
    pathsearch, processes,
    results::ExecutionSpawnResult,
    sys, trace_categories, traps,
};

/// Encapsulates the result of waiting for a command to complete.
pub enum CommandWaitResult {
    /// The command completed.
    CommandCompleted(ExecutionResult),
    /// The command was stopped before it completed.
    CommandStopped(ExecutionResult, processes::ChildProcess),
}

/// Represents the context for executing a command.
pub struct ExecutionContext<'a, SE: ShellExtensions = extensions::DefaultShellExtensions> {
    /// The shell in which the command is being executed.
    pub shell: &'a mut Shell<SE>,
    /// The name of the command being executed.
    pub command_name: String,
    /// The parameters for the execution.
    pub params: ExecutionParameters,
}

impl<SE: ShellExtensions> ExecutionContext<'_, SE> {
    /// Returns the standard input file; usable with `write!` et al.
    pub fn stdin(&self) -> openfiles::OpenFile {
        self.params.stdin(self.shell)
    }

    /// Returns the standard output file; usable with `write!` et al.
    pub fn stdout(&self) -> openfiles::OpenFile {
        self.params.stdout(self.shell)
    }

    /// Returns the standard error file; usable with `write!` et al.
    pub fn stderr(&self) -> openfiles::OpenFile {
        self.params.stderr(self.shell)
    }

    /// Writes a builtin's diagnostic as bash does: `NAME: line N: BUILTIN: message`.
    pub fn report(&self, message: impl std::fmt::Display) -> std::io::Result<()> {
        use std::io::Write as _;
        let prefix = self.shell.diagnostic_prefix();
        writeln!(self.stderr(), "{prefix}{}: {message}", self.command_name)
    }

    /// Returns the file descriptor with the given number. Returns `None`
    /// if the file descriptor is not open.
    ///
    /// # Arguments
    ///
    /// * `fd` - The file descriptor number to retrieve.
    pub fn try_fd(&self, fd: ShellFd) -> Option<openfiles::OpenFile> {
        self.params.try_fd(self.shell, fd)
    }

    /// Iterates over all open file descriptors.
    pub fn iter_fds(&self) -> impl Iterator<Item = (ShellFd, openfiles::OpenFile)> {
        self.params.iter_fds(self.shell)
    }
}

/// An argument to a command.
#[derive(Clone, Debug)]
pub enum CommandArg {
    /// A simple string argument.
    String(String),
    /// An assignment/declaration; typically treated as a string, but will
    /// be specially handled by a limited set of built-in commands.
    Assignment(ast::Assignment),
}

impl Display for CommandArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String(s) => f.write_str(s),
            Self::Assignment(a) => write!(f, "{a}"),
        }
    }
}

impl From<String> for CommandArg {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&String> for CommandArg {
    fn from(value: &String) -> Self {
        Self::String(value.clone())
    }
}

impl CommandArg {
    pub(crate) fn quote_for_tracing(&self) -> Cow<'_, str> {
        match self {
            Self::String(s) => escape::quote_if_needed(s, escape::QuoteMode::SingleQuote),
            // Bash prints the word `name=value` as it prints any word, quoted whole when it
            // needs to be: `e=`, `'a=x y'`.
            Self::Assignment(a) => {
                let op = if a.append { "+=" } else { "=" };
                escape::quote_if_needed(
                    format!("{}{op}{}", a.name, a.value).as_str(),
                    escape::QuoteMode::SingleQuote,
                )
                .into_owned()
                .into()
            }
        }
    }
}

/// Encapsulates a possibly-owned reference to a `Shell` for command execution.
pub enum ShellForCommand<'a, SE: extensions::ShellExtensions> {
    /// The command is run in the same shell as its parent; the provided
    /// mutable reference allows modifying the parent shell.
    ParentShell(&'a mut Shell<SE>),
    /// The command is run in its own owned shell (which is also provided).
    OwnedShell {
        /// The owned shell.
        target: Box<Shell<SE>>,
        /// The parent shell.
        parent: &'a mut Shell<SE>,
    },
}

impl<SE: extensions::ShellExtensions> std::ops::Deref for ShellForCommand<'_, SE> {
    type Target = Shell<SE>;

    fn deref(&self) -> &Self::Target {
        match self {
            ShellForCommand::ParentShell(shell) => shell,
            ShellForCommand::OwnedShell { target, .. } => target,
        }
    }
}

impl<SE: extensions::ShellExtensions> std::ops::DerefMut for ShellForCommand<'_, SE> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            ShellForCommand::ParentShell(shell) => shell,
            ShellForCommand::OwnedShell { target, .. } => target,
        }
    }
}

/// Composes a `std::process::Command` to execute the given command. Appropriately
/// configures the command name and arguments, redirections, injected file
/// descriptors, environment variables, etc.
///
/// # Arguments
///
/// * `context` - The execution context in which the command is being composed.
/// * `command_name` - The name of the command to execute.
/// * `argv0` - The value to use for `argv[0]` (may be different from the command).
/// * `args` - The arguments to pass to the command.
/// * `empty_env` - If true, the command will be executed with an empty environment; if false, the
///   command will inherit environment variables marked as exported in the provided `Shell`.
#[allow(unused_variables, reason = "argv0 is only used on unix platforms")]
pub fn compose_std_command<S: AsRef<OsStr>, SE: extensions::ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    command_name: &str,
    argv0: &str,
    args: &[S],
    empty_env: bool,
) -> Result<std::process::Command, error::Error> {
    let mut cmd = std::process::Command::new(command_name);

    // Override argv[0].
    // NOTE: Not supported on all platforms.
    cmd.arg0(argv0);

    // Pass through args.
    cmd.args(args);

    // Use the shell's current working dir.
    cmd.current_dir(context.shell.working_dir());

    // Start with a clear environment.
    cmd.env_clear();

    // Add in exported variables.
    if !empty_env {
        for (k, v) in context.shell.env().iter_exported() {
            // NOTE: To match bash behavior, we only include exported variables
            // that are set (i.e., have a value). This means a variable that
            // shows up in `declare -p` but has no *set* value will be omitted.
            if v.value().is_set() {
                cmd.env(k.as_str(), v.value().to_cow_str(context.shell).as_ref());
            }
        }
        // Set _ to the resolved command path for external commands.
        cmd.env("_", command_name);
    }

    // Add in exported functions.
    if !empty_env {
        for (func_name, registration) in context.shell.funcs().iter() {
            if registration.is_exported() {
                let var_name = std::format!("BASH_FUNC_{func_name}%%");
                let value = std::format!("() {}", registration.definition().body);
                cmd.env(var_name, value);
            }
        }
    }

    // Redirect stdin, if applicable.
    match context.try_fd(OpenFiles::STDIN_FD) {
        Some(OpenFile::Stdin(_)) | None => (),
        Some(stdin_file) => {
            let as_stdio: Stdio = stdin_file.try_into()?;
            cmd.stdin(as_stdio);
        }
    }

    // Redirect stdout, if applicable.
    match context.try_fd(OpenFiles::STDOUT_FD) {
        Some(OpenFile::Stdout(_)) | None => (),
        Some(stdout_file) => {
            let as_stdio: Stdio = stdout_file.try_into()?;
            cmd.stdout(as_stdio);
        }
    }

    // Redirect stderr, if applicable.
    match context.try_fd(OpenFiles::STDERR_FD) {
        Some(OpenFile::Stderr(_)) | None => {}
        Some(stderr_file) => {
            let as_stdio: Stdio = stderr_file.try_into()?;
            cmd.stderr(as_stdio);
        }
    }

    // Inject any other fds.
    let other_files = context.iter_fds().filter(|(fd, _)| {
        *fd != OpenFiles::STDIN_FD && *fd != OpenFiles::STDOUT_FD && *fd != OpenFiles::STDERR_FD
    });
    cmd.inject_fds(other_files)?;

    Ok(cmd)
}

/// Represents a simple command to be executed.
pub struct SimpleCommand<'a, SE: extensions::ShellExtensions> {
    /// The shell to run the command in.
    shell: ShellForCommand<'a, SE>,

    /// The execution parameters for the command.
    pub params: ExecutionParameters,

    /// The name of the command to execute.
    pub command_name: String,

    /// The arguments to the command, including the command itself.
    pub args: Vec<CommandArg>,

    /// Whether to consider shell functions when looking up the command name.
    /// If true, shell functions will be checked; if false, they will be ignored.
    pub use_functions: bool,

    /// Optional list of directories to search for external commands. If left
    /// `None`, the default search logic will be used.
    pub path_dirs: Option<Vec<PathBuf>>,

    /// The process group ID to use for externally executed commands. This may be
    /// `None`, in which case the default behavior will be used.
    pub process_group_id: Option<i32>,

    /// Optional override for the `argv[0]` value presented to an externally
    /// spawned process. When `None`, `command_name` is used.
    pub argv0: Option<String>,

    /// Optionally provides a function that can run after execution occurs. Note
    /// that it is *not* invoked if the shell is discarded during the execution
    /// process.
    #[allow(clippy::type_complexity)]
    pub post_execute: Option<fn(&mut Shell<SE>) -> Result<(), error::Error>>,
}

impl<'a, SE: extensions::ShellExtensions> SimpleCommand<'a, SE> {
    /// Creates a new `SimpleCommand` instance.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell in which to execute the command.
    /// * `params` - The execution parameters for the command.
    /// * `command_name` - The name of the command to execute.
    /// * `args` - The arguments to the command, including the command itself.
    pub fn new<I>(
        shell: ShellForCommand<'a, SE>,
        params: ExecutionParameters,
        command_name: String,
        args: I,
    ) -> Self
    where
        I: IntoIterator<Item = CommandArg>,
    {
        Self {
            shell,
            params,
            command_name,
            args: args.into_iter().collect(),
            use_functions: true,
            path_dirs: None,
            process_group_id: None,
            argv0: None,
            post_execute: None,
        }
    }

    /// Executes the simple command.
    ///
    /// The command may be a builtin, a shell function, or an externally
    /// executed command. This function's implementation is responsible for
    /// dispatching it appropriately according to the context provided.
    #[allow(
        clippy::missing_panics_doc,
        reason = "these unwrap calls should not panic"
    )]
    pub async fn execute(mut self) -> Result<ExecutionSpawnResult, error::Error> {
        // First see if it's the name of a builtin.
        let builtin = self.shell.builtins().get(&self.command_name).cloned();

        // If we're in POSIX mode and found a special builtin (that's not disabled), then invoke it
        // without considering functions.
        if self.shell.options().posix_mode
            && builtin
                .as_ref()
                .is_some_and(|r| !r.disabled && r.special_builtin)
        {
            #[allow(clippy::unwrap_used, reason = "we just checked that builtin is Some")]
            let builtin = builtin.unwrap();
            return self.execute_via_builtin(builtin).await;
        }

        // Assuming we weren't requested not to do so, check if it's the name of
        // a shell function.
        if self.use_functions {
            if let Some(func_registration) =
                self.shell.funcs().get(self.command_name.as_str()).cloned()
            {
                return self.execute_via_function(func_registration).await;
            }
        }

        // If we haven't yet resolved the command name and found a builtin that's not disabled,
        // then invoke it.
        if let Some(builtin) = builtin {
            if !builtin.disabled {
                return self.execute_via_builtin(builtin).await;
            }
        }

        // We still haven't found a command to invoke. We'll need to look for an external command.
        if !sys::fs::contains_path_separator(&self.command_name) {
            // All else failed; if we were given path directories to search, look through them
            // for a match. Otherwise, use our default search logic.
            let path = if let Some(path_dirs) = &self.path_dirs {
                pathsearch::resolve_command(path_dirs, self.command_name.as_str())
            } else {
                self.shell
                    .resolve_command_in_path_using_cache(&self.command_name)
            };

            if let Some(path) = path {
                self.execute_via_external(&path)
            } else {
                // Bash updates $_ even when the command is not found, so mirror
                // that here before reporting the error.
                let last_arg = Self::take_last_arg(&self.args);
                self.shell.update_last_arg_variable(last_arg);

                if let Some(post_execute) = self.post_execute {
                    let _ = post_execute(&mut self.shell);
                }

                // Bash hands a command it cannot find to `command_not_found_handle`, when one is
                // defined, run in a subshell with the command and its arguments; its status is
                // the command's.
                if let Some(handler) = self.shell.funcs().get(NOT_FOUND_HANDLER).cloned() {
                    let mut subshell = self.shell.clone();
                    // The handler does not handle the commands it cannot find itself.
                    subshell.undefine_func(NOT_FOUND_HANDLER);
                    let context = ExecutionContext {
                        shell: &mut subshell,
                        command_name: NOT_FOUND_HANDLER.to_owned(),
                        params: self.params,
                    };
                    let status = match invoke_shell_function(handler, context, &self.args).await {
                        Ok(spawned) => match spawned.wait().await? {
                            crate::results::ExecutionWaitResult::Completed(result) => {
                                result.exit_code
                            }
                            crate::results::ExecutionWaitResult::Stopped(..) => {
                                ExecutionResult::stopped().exit_code
                            }
                        },
                        Err(error) => {
                            let _ = subshell.display_error(&mut subshell.stderr(), &error);
                            error.into_result(&subshell).exit_code
                        }
                    };
                    return Ok(ExecutionResult::from(status).into());
                }

                Err(ErrorKind::CommandNotFound(self.command_name).into())
            }
        } else {
            let command_name = PathBuf::from(self.command_name.clone());
            self.execute_via_external(command_name.as_path())
        }
    }

    /// Extracts the owned string representation of the last argument of a
    /// command, suitable for recording into `$_`.
    fn take_last_arg(args: &[CommandArg]) -> Option<String> {
        args.last().map(ToString::to_string)
    }

    async fn execute_via_builtin(
        self,
        builtin: builtins::Registration<SE>,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        match self.shell {
            ShellForCommand::OwnedShell { target, .. } => {
                #[cfg(not(target_arch = "wasm32"))]
                let spawn_result = Self::execute_via_builtin_in_owned_shell(
                    *target,
                    self.params,
                    builtin,
                    self.command_name,
                    self.args,
                );
                #[cfg(target_arch = "wasm32")]
                let spawn_result = Self::execute_via_builtin_in_owned_shell(
                    *target,
                    self.params,
                    builtin,
                    self.command_name,
                    self.args,
                )
                .await?;
                Ok(spawn_result)
            }
            ShellForCommand::ParentShell(..) => {
                self.execute_via_builtin_in_parent_shell(builtin).await
            }
        }
    }

    /// Runs an owned-shell builtin stage. Natively this offloads to a blocking task and returns a
    /// `StartedTask` so pipeline stages run concurrently on real threads. On `wasm32` there is no
    /// thread pool. The pipeline owns local stage tasks; each task awaits its builtin and
    /// cooperative I/O directly, returning `Completed` to that stage's execution wrapper.
    #[cfg(not(target_arch = "wasm32"))]
    fn execute_via_builtin_in_owned_shell(
        mut shell: Shell<SE>,
        params: ExecutionParameters,
        builtin: builtins::Registration<SE>,
        command_name: String,
        args: Vec<CommandArg>,
    ) -> ExecutionSpawnResult {
        let last_arg = Self::take_last_arg(&args);
        let join_handle = tokio::task::spawn_blocking(move || {
            let cmd_context = ExecutionContext {
                shell: &mut shell,
                command_name,
                params,
            };

            let rt = tokio::runtime::Handle::current();
            let result = rt.block_on(execute_builtin_command(&builtin, cmd_context, args));

            // Update $_ after command execution.
            shell.update_last_arg_variable(last_arg);

            result
        });

        ExecutionSpawnResult::StartedTask(join_handle)
    }

    #[cfg(target_arch = "wasm32")]
    async fn execute_via_builtin_in_owned_shell(
        mut shell: Shell<SE>,
        params: ExecutionParameters,
        builtin: builtins::Registration<SE>,
        command_name: String,
        args: Vec<CommandArg>,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        let last_arg = Self::take_last_arg(&args);
        let cmd_context = ExecutionContext {
            shell: &mut shell,
            command_name,
            params,
        };

        let result = execute_builtin_command(&builtin, cmd_context, args).await;

        // Update $_ after command execution (mirrors the native path, which does this regardless of
        // the builtin's success).
        shell.update_last_arg_variable(last_arg);

        // Propagate errors the same way the native `StartedTask` does: the error surfaces when the
        // pipeline waits on this stage, not swallowed into a `Completed` result.
        Ok(ExecutionSpawnResult::Completed(result?))
    }

    async fn execute_via_builtin_in_parent_shell(
        self,
        builtin: builtins::Registration<SE>,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        let mut shell = self.shell;
        let last_arg = Self::take_last_arg(&self.args);
        #[cfg(any(target_arch = "wasm32", test))]
        let mut cleanup = crate::shell::FrameGuard::new(
            &mut shell,
            self.post_execute.unwrap_or(|_| Ok(())),
            None,
        );
        #[cfg(any(target_arch = "wasm32", test))]
        let shell = cleanup.shell();
        #[cfg(not(any(target_arch = "wasm32", test)))]
        let shell = &mut *shell;

        let cmd_context = ExecutionContext {
            shell: &mut *shell,
            command_name: self.command_name,
            params: self.params,
        };

        let result = execute_builtin_command(&builtin, cmd_context, self.args).await;

        // Update $_ after command execution.
        shell.update_last_arg_variable(last_arg);

        #[cfg(not(any(target_arch = "wasm32", test)))]
        if let Some(post_execute) = self.post_execute {
            let _ = post_execute(shell);
        }

        let result = result?;

        Ok(result.into())
    }

    async fn execute_via_function(
        self,
        func_registration: functions::Registration,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        let mut shell = self.shell;
        let last_arg = Self::take_last_arg(&self.args);
        #[cfg(any(target_arch = "wasm32", test))]
        let mut cleanup = crate::shell::FrameGuard::new(
            &mut shell,
            self.post_execute.unwrap_or(|_| Ok(())),
            None,
        );
        #[cfg(any(target_arch = "wasm32", test))]
        let shell = cleanup.shell();
        #[cfg(not(any(target_arch = "wasm32", test)))]
        let shell = &mut *shell;

        let cmd_context = ExecutionContext {
            shell: &mut *shell,
            command_name: self.command_name,
            params: self.params,
        };

        // Strip the function name off args.
        let result = invoke_shell_function(func_registration, cmd_context, &self.args[1..]).await;

        // $_ is reset *after* the function body runs, to the last argument of
        // the invocation (or the function name itself if zero args). Any
        // mutations made inside the body are overwritten — this matches bash,
        // where the caller observes only the invocation's last argument.
        shell.update_last_arg_variable(last_arg);

        #[cfg(not(any(target_arch = "wasm32", test)))]
        if let Some(post_execute) = self.post_execute {
            let _ = post_execute(shell);
        }

        result
    }

    fn execute_via_external(self, path: &Path) -> Result<ExecutionSpawnResult, error::Error> {
        let mut shell = self.shell;
        let last_arg = Self::take_last_arg(&self.args);

        let cmd_context = ExecutionContext {
            shell: &mut shell,
            command_name: self.command_name,
            params: self.params,
        };

        let resolved_path = path.to_string_lossy();
        let result = execute_external_command(
            cmd_context,
            resolved_path.as_ref(),
            self.process_group_id,
            self.argv0.as_deref(),
            &self.args[1..],
        );

        // Update $_ after command execution.
        shell.update_last_arg_variable(last_arg);

        if let Some(post_execute) = self.post_execute {
            let _ = post_execute(&mut shell);
        }

        result
    }
}

pub(crate) fn execute_external_command(
    context: ExecutionContext<'_, impl extensions::ShellExtensions>,
    executable_path: &str,
    process_group_id: Option<i32>,
    argv0_override: Option<&str>,
    args: &[CommandArg],
) -> Result<ExecutionSpawnResult, error::Error> {
    // Filter out the args; we only want strings.
    let cmd_args = args
        .iter()
        .filter_map(|e| {
            if let CommandArg::String(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    // Before we lose ownership of the open files, figure out if stdin will be a terminal.
    let child_stdin_is_terminal = context
        .try_fd(openfiles::OpenFiles::STDIN_FD)
        .is_some_and(|f| f.is_terminal());

    // Figure out if we should be setting up a new process group.
    let new_pg = matches!(
        context.params.process_group_policy,
        ProcessGroupPolicy::NewProcessGroup
    );

    // Compose the std::process::Command that encapsulates what we want to launch.
    // argv[0] defaults to context.command_name (the user-facing name of the
    // command) unless the caller specified an explicit override.
    let argv0 = argv0_override.unwrap_or(context.command_name.as_str());
    #[allow(unused_mut, reason = "only mutated on unix platforms")]
    let mut cmd = compose_std_command(
        &context,
        executable_path,
        argv0,
        cmd_args.as_slice(),
        false, /* empty environment? */
    )?;

    // Set up process group state.
    if new_pg {
        // Check if we'll be doing terminal control setup (which includes setsid)
        if child_stdin_is_terminal && context.shell.options().external_cmd_leads_session {
            // Don't set process_group(0) - setsid() in pre_exec will handle it
            cmd.lead_session();
        } else {
            // Normal case: create new process group in current session
            cmd.process_group(0);
            if child_stdin_is_terminal {
                cmd.take_foreground();
            }
        }
    } else {
        // We need to join an established process group.
        if let Some(pgid) = process_group_id {
            cmd.process_group(pgid);
        }
    }

    // When tracing is enabled, report.
    tracing::debug!(
        target: trace_categories::COMMANDS,
        "Spawning: cmd='{} {}'",
        cmd.get_program().to_string_lossy().to_string(),
        cmd.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .join(" ")
    );

    match sys::process::spawn(cmd, context.shell.options().kill_external_commands_on_drop) {
        Ok(child) => {
            // Retrieve the pid.
            #[expect(clippy::cast_possible_wrap)]
            let pid = child.id().map(|id| id as i32);
            let mut actual_pgid = process_group_id;
            if let Some(pid) = &pid {
                if new_pg {
                    actual_pgid = Some(*pid);
                }
            } else {
                tracing::warn!("could not retrieve pid for child process");
            }

            Ok(ExecutionSpawnResult::StartedProcess(
                processes::ChildProcess::new(child, pid, actual_pgid),
            ))
        }
        Err(spawn_err) => {
            if context.shell.options().interactive {
                sys::terminal::move_self_to_foreground()?;
            }

            #[cfg(target_arch = "wasm32")]
            if spawn_err.kind() == std::io::ErrorKind::Unsupported {
                return Err(unexecutable(context.shell, context.command_name));
            }

            if spawn_err.kind() == std::io::ErrorKind::NotFound {
                if !context.shell.working_dir().exists() {
                    Err(
                        error::ErrorKind::WorkingDirMissing(context.shell.working_dir().to_owned())
                            .into(),
                    )
                } else {
                    Err(error::ErrorKind::CommandNotFound(context.command_name).into())
                }
            } else {
                Err(
                    error::ErrorKind::FailedToExecuteCommand(context.command_name, spawn_err)
                        .into(),
                )
            }
        }
    }
}

/// Why command `name` cannot run on WASI, which cannot start processes: a path that names
/// nothing or a directory fails as `execve` fails on it; a file is refused.
#[cfg(target_arch = "wasm32")]
fn unexecutable(shell: &Shell<impl extensions::ShellExtensions>, name: String) -> error::Error {
    if !name.contains('/') {
        return error::ErrorKind::ExecutingFilesUnsupported(name).into();
    }
    let path = shell.absolute_path(std::path::Path::new(&name));
    match std::fs::metadata(&path) {
        Err(_) => error::ErrorKind::CannotExecutePath(name, error::NO_SUCH_FILE),
        Ok(metadata) if metadata.is_dir() => {
            error::ErrorKind::CannotExecutePath(name, "Is a directory")
        }
        Ok(_) => error::ErrorKind::ExecutingFilesUnsupported(name),
    }
    .into()
}

#[cfg(not(target_arch = "wasm32"))]
async fn execute_builtin_command<SE: extensions::ShellExtensions>(
    builtin: &builtins::Registration<SE>,
    context: ExecutionContext<'_, SE>,
    args: Vec<CommandArg>,
) -> Result<ExecutionResult, error::Error> {
    // In POSIX mode, special builtins that return errors are to be treated as fatal.
    let mark_errors_fatal = builtin.special_builtin && context.shell.options().posix_mode;
    #[cfg(target_arch = "wasm32")]
    let services = context.shell.execution_services();

    let result = (builtin.execute_func)(context, args).await;

    // On wasm32, pipeline stages share one thread; give a stage waiting on this one's output — or
    // watching a pipe this builtin just found broken — its turn.
    #[cfg(target_arch = "wasm32")]
    (services.yield_now)().await;

    match result {
        Ok(result) => Ok(result),
        Err(e) => {
            // Broken pipe errors should silently return the appropriate exit code
            if let Some(io_err) = e.as_io_error() {
                if io_err.kind() == std::io::ErrorKind::BrokenPipe {
                    return Ok(ExecutionExitCode::from(io_err).into());
                }
            }

            Err(if mark_errors_fatal { e.into_fatal() } else { e })
        }
    }
}

#[cfg(target_arch = "wasm32")]
async fn execute_builtin_command<SE: extensions::ShellExtensions>(
    builtin: &builtins::Registration<SE>,
    context: ExecutionContext<'_, SE>,
    args: Vec<CommandArg>,
) -> Result<ExecutionResult, error::Error> {
    use crate::execution::process;
    if builtin.execution_boundary == builtins::ExecutionBoundary::Command {
        let disposition = process::pipe_disposition().for_exec();
        let ExecutionContext {
            shell,
            command_name,
            params,
        } = context;
        let mut command_shell = shell.clone();
        command_shell.traps_mut().reset_pipe_for_subshell();
        let command_context = ExecutionContext {
            shell: &mut command_shell,
            command_name,
            params: params.clone(),
        };
        let result = process::run_process(
            disposition,
            execute_wasm_builtin(builtin, command_context, args, false),
        )
        .await;
        // Signals that reached this (calling) process while the command ran.
        let triggering_status = result
            .as_ref()
            .map_or(1, |result| u8::from(result.exit_code));
        if let Some(outcome) = deliver_pending_traps(shell, &params, triggering_status).await? {
            return Ok(outcome);
        }
        result
    } else {
        process::apply_trap_dispositions(context.shell.traps());
        execute_wasm_builtin(builtin, context, args, true).await
    }
}

#[cfg(target_arch = "wasm32")]
async fn execute_wasm_builtin<SE: extensions::ShellExtensions>(
    builtin: &builtins::Registration<SE>,
    context: ExecutionContext<'_, SE>,
    args: Vec<CommandArg>,
    deliver_traps: bool,
) -> Result<ExecutionResult, error::Error> {
    use crate::{execution::process, traps::PipeDisposition};
    use futures::io::AsyncWriteExt;
    let ExecutionContext {
        shell,
        command_name,
        params,
    } = context;
    let services = shell.execution_services();
    let mark_errors_fatal = builtin.special_builtin && shell.options().posix_mode;
    let mut result = (builtin.execute_func)(
        ExecutionContext {
            shell: &mut *shell,
            command_name: command_name.clone(),
            params: params.clone(),
        },
        args,
    )
    .await;

    // Default SIGPIPE is observed by the enclosing process before anything else executes.
    (services.yield_now)().await;
    if let Err(error) = &result {
        if error
            .as_io_error()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe)
        {
            if process::pipe_disposition() != PipeDisposition::Default {
                let mut stderr = params.stderr(shell);
                let prefix = shell.diagnostic_prefix();
                let diagnostic = stderr
                    .async_io()
                    .write_all(
                        format!("{prefix}{command_name}: write error: Broken pipe\n").as_bytes(),
                    )
                    .await;
                if let Err(error) = diagnostic {
                    if error.kind() != std::io::ErrorKind::BrokenPipe {
                        return Err(error.into());
                    }
                }
                result = Ok(ExecutionResult::new(1));
            } else {
                result = Ok(ExecutionResult::terminated_by_signal(13));
            }
        } else if error.as_io_error().is_some_and(|error| {
            error.kind() == std::io::ErrorKind::Unsupported
                && error.to_string() == crate::openfiles::SYNCHRONOUS_PIPE_INPUT_MESSAGE
        }) {
            params
                .stderr(shell)
                .async_io()
                .write_all(
                    format!("{}\n", crate::openfiles::SYNCHRONOUS_PIPE_INPUT_MESSAGE).as_bytes(),
                )
                .await?;
            result = Ok(ExecutionResult::new(1));
        }
    }

    if deliver_traps {
        let triggering_status = result
            .as_ref()
            .map_or(1, |result| u8::from(result.exit_code));
        if let Some(outcome) = deliver_pending_traps(shell, &params, triggering_status).await? {
            return Ok(outcome);
        }
    }
    result.map_err(|error| {
        if mark_errors_fatal {
            error.into_fatal()
        } else {
            error
        }
    })
}

/// Runs handlers for caught signals that arrived while the current command ran. Returns the
/// handler's result when it changes control flow (for example `exit`).
#[cfg(target_arch = "wasm32")]
async fn deliver_pending_traps<SE: extensions::ShellExtensions>(
    shell: &mut Shell<SE>,
    params: &ExecutionParameters,
    triggering_status: u8,
) -> Result<Option<ExecutionResult>, error::Error> {
    use crate::execution::process::{self, signals};
    while let Some(signal_number) = process::take_pending_trap() {
        shell.set_last_exit_status(triggering_status);
        let _handling = (signal_number == signals::PIPE).then(process::handling_pipe);
        let signal: crate::traps::TrapSignal = i32::from(signal_number).try_into()?;
        let handler_result = shell.invoke_trap_handler(signal, params).await?;
        process::apply_trap_dispositions(shell.traps());
        if !handler_result.is_normal_flow() {
            return Ok(Some(handler_result));
        }
    }
    Ok(None)
}

/// The function bash runs for a command it cannot find.
const NOT_FOUND_HANDLER: &str = "command_not_found_handle";

pub(crate) async fn invoke_shell_function(
    function: functions::Registration,
    mut context: ExecutionContext<'_, impl extensions::ShellExtensions>,
    args: &[CommandArg],
) -> Result<ExecutionSpawnResult, error::Error> {
    let ast::FunctionBody(body, redirects) = &function.definition().body;

    // Apply any redirects specified at function definition-time.
    if let Some(redirects) = redirects {
        for redirect in &redirects.0 {
            interp::setup_redirect(context.shell, &mut context.params, redirect).await?;
        }
        if redirects.0.iter().any(interp::redirects_stdin) {
            context.params.stdin_redirected = true;
        }
    }

    let positional_args = args.iter().map(|a| a.to_string());

    // Note that we're going deeper. Once we do this, we need to make sure we don't bail early
    // before "exiting" the function.
    context.shell.enter_function(
        context.command_name.as_str(),
        &function,
        positional_args,
        &context.params,
    )?;

    // A function executes within the current shell process and shares its caller's open files,
    // so the parameters are passed through by shared reference rather than cloned. This prevents
    // direct mutation of the caller's `ExecutionParameters` open-file table, though the function
    // may still change the shell's persistent open files via builtins (e.g. `exec`).
    // `break` in a function body does not reach the caller's loops.
    let caller_loop_depth = std::mem::take(&mut context.shell.loop_depth);
    let return_trap = context
        .shell
        .traps()
        .get_handler(traps::TrapSignal::Return)
        .map(|handler| handler.command.clone());
    // `local -` in the body saves the options, which come back when it returns.
    let option_saves = context.shell.local_option_saves.len();
    // The body expands the aliases in effect where the function was defined.
    let caller_aliases = context.shell.alias_scope.replace(function.aliases());
    #[cfg(any(target_arch = "wasm32", test))]
    let result = {
        let mut frame = crate::shell::FrameGuard::new(context.shell, Shell::leave_function, None);
        let result = body.execute(frame.shell(), &context.params).await;
        frame.finish()?;
        result
    };
    #[cfg(not(any(target_arch = "wasm32", test)))]
    let result = {
        let result = body.execute(context.shell, &context.params).await;
        context.shell.leave_function()?;
        result
    };
    context.shell.alias_scope = caller_aliases;
    context.shell.loop_depth = caller_loop_depth;
    context.shell.restore_local_options(option_saves);

    // The RETURN trap runs as the function returns when the function set it (or, with
    // functrace, inherited it), as in bash.
    let now = context
        .shell
        .traps()
        .get_handler(traps::TrapSignal::Return)
        .map(|handler| handler.command.clone());
    if now.is_some()
        && (now != return_trap
            || context
                .shell
                .options()
                .shell_functions_inherit_debug_and_return_traps)
    {
        let _ = context.shell.run_return_trap(&context.params).await;
    }
    context.shell.status_before_return = None;

    // Get the actual execution result from the body of the function.
    let mut result = result?;

    // Handle control-flow.
    match result.next_control_flow {
        ExecutionControlFlow::BreakLoop { .. } | ExecutionControlFlow::ContinueLoop { .. } => {
            return error::unimp("break or continue returned from function invocation");
        }
        ExecutionControlFlow::ReturnFromFunctionOrScript => {
            // It's now been handled.
            result.next_control_flow = ExecutionControlFlow::Normal;
        }
        _ => {}
    }

    Ok(result.into())
}

pub(crate) async fn invoke_command_in_subshell_and_get_output(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    s: String,
) -> Result<String, error::Error> {
    // Instantiate a subshell to run the command in.
    let mut subshell = shell.clone();
    #[cfg(target_arch = "wasm32")]
    subshell.traps_mut().reset_pipe_for_subshell();
    // It runs only an EXIT trap it sets itself, when it ends.
    #[cfg(target_arch = "wasm32")]
    subshell.traps_mut().reset_exit_for_subshell();

    // Command substitutions don't inherit errexit by default. Only inherit it when
    // command_subst_inherits_errexit is enabled, otherwise disable errexit in the subshell.
    if !shell.options().command_subst_inherits_errexit {
        subshell.options_mut().exit_on_nonzero_command_exit = false;
    }

    // Get our own set of parameters we can customize and use.
    let mut params = params.clone();
    params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;

    // On wasm32 (wasip2): `std::io::pipe()` is unsupported and the single-threaded runtime has no
    // blocking pool for the concurrent spawn-then-read below. Poll the producer and bounded-pipe
    // drain together, so substitutions larger than capacity do not fill an undrained pipe.
    // Expansion still receives the captured output only after the producer completes.
    #[cfg(target_arch = "wasm32")]
    {
        let (mut reader, writer) = openfiles::open_mem_pipe();
        params.set_fd(OpenFiles::STDOUT_FD, writer);

        // It is a process of its own, with its own `$BASHPID`.
        let numbered = interp::subshell_process(&mut subshell);
        // The substitution's output ends when the subshell, its EXIT trap and every job still
        // holding its output are done.
        let command = async move {
            let completed = std::cell::Cell::new(false);
            let result = numbered
                .run(async {
                    let result = run_wasm_substitution_command(&mut subshell, &mut params, s).await;
                    let result = subshell.exit_with_trap_in(result, &params).await;
                    completed.set(true);
                    result
                })
                .await;
            if !completed.get() {
                subshell.exit_trap_after_signal(&result, &params).await;
            }
            result
        };
        // At most `MAX_SUBSTITUTION_BYTES` are kept; past that the reader closes, so the
        // substitution's writers get SIGPIPE, and the substitution fails.
        let read = async move {
            use futures::io::AsyncReadExt;
            let mut output = Vec::new();
            let mut chunk = vec![0; 64 * 1024];
            loop {
                let count = reader.async_io().read(&mut chunk).await?;
                if count == 0 {
                    return Ok::<_, std::io::Error>((output, false));
                }
                if output.len() + count > openfiles::MAX_SUBSTITUTION_BYTES {
                    drop(reader);
                    return Ok((output, true));
                }
                output.extend_from_slice(&chunk[..count]);
            }
        };
        let (cmd_result, output_result) = futures::join!(command, read);
        let (output, truncated) = output_result?;
        let status = cmd_result?.exit_code.into();
        if truncated {
            shell.set_last_exit_status(1);
            return Err(error::ErrorKind::SubstitutionTooLarge.into());
        }
        shell.set_last_exit_status(status);
        // The output is kept byte for byte, not required to be UTF-8 (see `rawbytes`).
        Ok(crate::rawbytes::decode_vec(output))
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        // Set up pipe so we can read the output.
        let (reader, writer) = std::io::pipe()?;
        params.set_fd(OpenFiles::STDOUT_FD, writer.into());

        let mut async_reader = sys::async_pipe::AsyncPipeReader::new(reader)?;

        let cmd_join_handle = tokio::spawn(run_substitution_command(subshell, params, s));

        let output_str = async_reader.read_to_string().await?;

        // Now observe the command's completion.
        let run_result = cmd_join_handle.await?;
        let cmd_result = run_result?;

        // Store the status.
        shell.set_last_exit_status(cmd_result.exit_code.into());

        // Note: $_ is naturally isolated from the parent because we cloned the
        // shell to run the substitution.

        Ok(output_str)
    }
}

/// Runs a command string in the shell itself rather than a copy of it, as bash runs a
/// `${ command; }` substitution, and returns what it wrote to its standard output.
pub(crate) async fn invoke_command_in_current_shell_and_get_output(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    command: String,
) -> Result<String, error::Error> {
    let mut params = params.clone();

    // The output is kept byte for byte, not required to be UTF-8 (see `rawbytes`). The command
    // and the read of its output run together, so output larger than the pipe cannot stall it.
    // As for `$( )`, at most `MAX_SUBSTITUTION_BYTES` are kept; past that the reader closes, and
    // the substitution fails.
    #[cfg(target_arch = "wasm32")]
    let (cmd_result, output) = {
        let (mut reader, writer) = openfiles::open_mem_pipe();
        params.set_fd(OpenFiles::STDOUT_FD, writer);
        let read = async move {
            use futures::io::AsyncReadExt;
            let mut output = Vec::new();
            let mut chunk = vec![0; 64 * 1024];
            loop {
                let count = reader.async_io().read(&mut chunk).await?;
                if count == 0 {
                    return Ok::<_, std::io::Error>((output, false));
                }
                if output.len() + count > openfiles::MAX_SUBSTITUTION_BYTES {
                    drop(reader);
                    return Ok((output, true));
                }
                output.extend_from_slice(&chunk[..count]);
            }
        };
        let (cmd_result, output_result) =
            futures::join!(run_command_string(shell, params, command), read);
        let (output, truncated) = output_result?;
        if truncated {
            shell.set_last_exit_status(1);
            return Err(error::ErrorKind::SubstitutionTooLarge.into());
        }
        (cmd_result, crate::rawbytes::decode_vec(output))
    };

    #[cfg(not(target_arch = "wasm32"))]
    let (cmd_result, output) = {
        let (reader, writer) = std::io::pipe()?;
        params.set_fd(OpenFiles::STDOUT_FD, writer.into());
        let mut async_reader = sys::async_pipe::AsyncPipeReader::new(reader)?;
        let (cmd_result, output) = futures::join!(
            run_command_string(shell, params, command),
            async_reader.read_to_string()
        );
        (cmd_result, output?)
    };

    shell.set_last_exit_status(cmd_result?.exit_code.into());
    Ok(output)
}

/// Runs a command string in the shell itself, as a `${| command; }` substitution does, and
/// returns the value it left in `REPLY`, which is local to it.
pub(crate) async fn invoke_command_in_current_shell_for_reply(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    command: String,
) -> Result<String, error::Error> {
    const REPLY: &str = "REPLY";
    let saved = shell
        .env()
        .get(REPLY)
        .map(|(scope, var)| (scope, var.clone()));
    shell.env_mut().unset(REPLY)?;

    let result = run_command_string(shell, params.clone(), command).await;

    let value = shell
        .env_str(REPLY)
        .map(|v| v.into_owned())
        .unwrap_or_default();
    shell.env_mut().unset(REPLY)?;
    if let Some((scope, var)) = saved {
        shell.env_mut().add(REPLY, var, scope)?;
    }

    shell.set_last_exit_status(result?.exit_code.into());
    Ok(value)
}

/// Runs a command string with the given parameters, which it owns, so its output closes when it
/// finishes.
async fn run_command_string(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: ExecutionParameters,
    command: String,
) -> Result<ExecutionResult, error::Error> {
    let parse_result = shell.parse_string(command.as_str());
    let source_info = crate::SourceInfo::from("main");
    shell
        .run_parsed_result(parse_result, Some(&command), &source_info, &params)
        .await
}

#[cfg(target_arch = "wasm32")]
async fn run_wasm_substitution_command(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &mut ExecutionParameters,
    command: String,
) -> Result<ExecutionResult, error::Error> {
    run_substitution_command_in(shell, params, command).await
}

#[cfg(not(target_arch = "wasm32"))]
async fn run_substitution_command(
    mut shell: Shell<impl extensions::ShellExtensions>,
    mut params: ExecutionParameters,
    command: String,
) -> Result<ExecutionResult, error::Error> {
    run_substitution_command_in(&mut shell, &mut params, command).await
}

async fn run_substitution_command_in(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &mut ExecutionParameters,
    command: String,
) -> Result<ExecutionResult, error::Error> {
    // Parse the string into a whole shell program.
    let parse_result = shell.parse_string(command.as_str());

    // Check for a command that is only an input redirection ("< file").
    // If detected, emulate `cat file` to stdout and return immediately.
    // If we failed to parse, then we'll fall below and handle it there.
    if let Ok(program) = &parse_result {
        if let Some(redir) = try_unwrap_bare_input_redir_program(program) {
            // A file that cannot be read is reported and fails the substitution (status 1), as
            // in bash; it does not end the command using the substitution.
            if let Err(error) = interp::setup_redirect(shell, params, redir).await {
                let _ = shell.display_error(&mut params.stderr(shell), &error);
                return Ok(ExecutionResult::general_error());
            }
            #[cfg(target_arch = "wasm32")]
            futures::io::copy(&mut params.stdin(shell), &mut params.stdout(shell)).await?;
            #[cfg(not(target_arch = "wasm32"))]
            std::io::copy(&mut params.stdin(shell), &mut params.stdout(shell))?;
            return Ok(ExecutionResult::new(0));
        }
    }

    // TODO(source-info): review this
    let source_info = crate::SourceInfo::from("main");

    // The substitution's lines are numbered on from the command it is part of.
    shell.begin_nested_code();

    // Handle the parse result using default shell behavior.
    shell
        .run_parsed_result(parse_result, Some(&command), &source_info, params)
        .await
}

// Detects a subshell command that consists solely of a single input redirection
// (e.g., "< file"), returning the IoRedirect when present.
fn try_unwrap_bare_input_redir_program(program: &ast::Program) -> Option<&ast::IoRedirect> {
    // We're looking for exactly one complete command...
    let [complete] = program.complete_commands.as_slice() else {
        return None;
    };

    // ...a single list item...
    let ast::CompoundList(items) = complete;
    let [item] = items.as_slice() else {
        return None;
    };

    // ...with a single pipeline (no && or || chaining)...
    let and_or = &item.0;
    if !and_or.additional.is_empty() {
        return None;
    }

    // ...not negated...
    let pipeline = &and_or.first;
    if pipeline.bang {
        return None;
    }

    // ...with a single command in the pipeline...
    let [ast::Command::Simple(simple_cmd)] = pipeline.seq.as_slice() else {
        return None;
    };

    // ...with no program word/name and no suffix...
    if simple_cmd.word_or_name.is_some() || simple_cmd.suffix.is_some() {
        return None;
    }

    // ...and exactly one prefix containing an I/O redirect...
    let prefix = simple_cmd.prefix.as_ref()?;
    let [ast::CommandPrefixOrSuffixItem::IoRedirect(redir)] = prefix.0.as_slice() else {
        return None;
    };

    // ...that is a file input redirection to a filename, targeting stdin.
    match redir {
        ast::IoRedirect::File(
            fd,
            ast::IoFileRedirectKind::Read,
            ast::IoFileRedirectTarget::Filename(..),
        ) if fd.is_none_or(|fd| fd == openfiles::OpenFiles::STDIN_FD) => Some(redir),
        _ => None,
    }
}
