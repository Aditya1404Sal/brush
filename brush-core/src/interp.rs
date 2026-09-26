use brush_parser::ast::{self, CommandPrefixOrSuffixItem};
use itertools::Itertools;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::arithmetic::{self, ExpandAndEvaluate};
use crate::commands::{self, CommandArg};
use crate::env::{EnvironmentLookup, EnvironmentScope, valid_variable_name};
use crate::openfiles::{OpenFile, OpenFiles};
use crate::results::{
    ExecutionExitCode, ExecutionResult, ExecutionSpawnResult, ExecutionWaitResult,
};
use crate::shell::Shell;
use crate::variables::{
    ArrayLiteral, ShellValue, ShellValueLiteral, ShellValueUnsetType, ShellVariable,
};
use crate::{
    ShellFd, error, expansion, extendedtests, extensions, ioutils, jobs, openfiles, sys, timing,
    traps,
};

/// A simple command bash would run without forking, as `exec` does, so that a program it names
/// replaces the shell and sees `SHLVL` one lower: the last command of a command string
/// (`bash -c`, a substitution), the body of a `( list )` subshell, or a background command, and
/// in a substitution or a background command the last command of a function they call.
#[derive(Clone, Copy, Default)]
pub(crate) struct NoFork {
    /// The command's address in the program that holds it, or 0 for none. The program outlives
    /// any shell that names one of its commands here.
    command: usize,
    /// Whether it also needs no trap to be set or running, as bash's `should_suppress_fork` and
    /// `should_optimize_fork` do.
    checked: bool,
    /// Whether a function it calls runs its own last command this way, as bash's
    /// `optimize_shell_function` does in a substitution or a background command.
    into_function: bool,
}

/// How a command string ends the shell that runs it (see [`Shell::exec_last_command`]).
#[derive(Clone, Copy)]
pub(crate) enum CommandString {
    /// A `-c` string: its last command is the end of the input only when nothing but blanks, a
    /// comment and one newline follow it.
    Script,
    /// A `$( )` substitution, which bash runs from its command printed back, so it always ends
    /// with its last command.
    Substitution,
    /// A backquoted substitution, which bash runs as written.
    Backquoted,
}

impl NoFork {
    /// The last command of a command string, `program`, whose text is `input` when known.
    fn for_command_string(
        program: &ast::Program,
        kind: CommandString,
        input: Option<&str>,
    ) -> Self {
        let Some(list) = program.complete_commands.last() else {
            return Self::default();
        };
        if let (CommandString::Script, Some(input)) = (kind, input) {
            let end = ast::SourceLocation::location(list).map(|span| span.end.index);
            if !end.is_some_and(|end| ends_input(input, end)) {
                return Self::default();
            }
        }
        Self::for_last_command(list, !matches!(kind, CommandString::Script))
    }

    /// The last command of `list`, a command string's or a function body's, as bash's
    /// `should_suppress_fork` finds it: it has no redirections of its own and needs no trap.
    fn for_last_command(list: &ast::CompoundList, into_function: bool) -> Self {
        match last_simple_command(list) {
            Some((command, _)) if !has_redirects(command) => Self {
                command: std::ptr::from_ref(command) as usize,
                checked: true,
                into_function,
            },
            _ => Self::default(),
        }
    }

    /// The last command of a `( list )` subshell's body: its only command, whatever its
    /// redirections and the traps, or the last of a `;`, `&&` or `||` connection as a command
    /// string's.
    fn for_subshell(list: &ast::CompoundList) -> Self {
        match last_simple_command(list) {
            Some((command, false)) => Self {
                command: std::ptr::from_ref(command) as usize,
                checked: false,
                into_function: false,
            },
            Some((_, true)) => Self::for_last_command(list, false),
            None => Self::default(),
        }
    }

    /// A background command that is a single simple command, forked as it starts.
    fn for_background(ao_list: &ast::AndOrList) -> Self {
        match (ao_list.additional.is_empty(), &ao_list.first) {
            (
                true,
                ast::Pipeline {
                    timed: None, seq, ..
                },
            ) => match seq.as_slice() {
                [ast::Command::Simple(command)] => Self {
                    command: std::ptr::from_ref(command) as usize,
                    checked: false,
                    into_function: true,
                },
                _ => Self::default(),
            },
            _ => Self::default(),
        }
    }

    /// The last command of a function body, when the call runs without forking.
    pub(crate) fn for_function(body: &ast::CompoundCommand) -> Self {
        match body {
            ast::CompoundCommand::BraceGroup(ast::BraceGroupCommand { list, .. }) => {
                Self::for_last_command(list, true)
            }
            _ => Self::default(),
        }
    }

    /// Whether `command` is the command this names.
    fn names(&self, command: &ast::SimpleCommand) -> bool {
        self.command != 0 && self.command == std::ptr::from_ref(command) as usize
    }
}

/// The simple command bash runs last in `list` when that is a whole command of its own: the
/// list's only pipeline, or the second of its last `;`, `&&` or `||` (`a; b && c` ends with an
/// `&&`, and `a & b` with a `&`), and whether it is such a second.
fn last_simple_command(list: &ast::CompoundList) -> Option<(&ast::SimpleCommand, bool)> {
    let (ast::CompoundListItem(and_or, separator), before) = list.0.split_last()?;
    if matches!(separator, ast::SeparatorOperator::Async) {
        return None;
    }
    let (pipeline, connection) = match before.last() {
        Some(ast::CompoundListItem(_, ast::SeparatorOperator::Async)) => return None,
        Some(_) if !and_or.additional.is_empty() => return None,
        Some(_) => (&and_or.first, true),
        None => match and_or.additional.last() {
            Some(ast::AndOr::And(pipeline) | ast::AndOr::Or(pipeline)) => (pipeline, true),
            None => (&and_or.first, false),
        },
    };
    if pipeline.bang || pipeline.timed.is_some() {
        return None;
    }
    match pipeline.seq.as_slice() {
        [ast::Command::Simple(command)] => Some((command, connection)),
        _ => None,
    }
}

/// Whether a simple command has redirections of its own.
fn has_redirects(command: &ast::SimpleCommand) -> bool {
    command
        .prefix
        .iter()
        .flat_map(|prefix| &prefix.0)
        .chain(command.suffix.iter().flat_map(|suffix| &suffix.0))
        .any(|item| matches!(item, CommandPrefixOrSuffixItem::IoRedirect(_)))
}

/// Whether only blanks, a `;`, a comment and one newline follow character `end` of a command
/// string's `input`: bash, which reads the string a line at a time, is then at its end.
fn ends_input(input: &str, end: usize) -> bool {
    fn skip_blanks(mut text: &str) -> &str {
        loop {
            text = text.trim_start_matches([' ', '\t']);
            match text.strip_prefix("\\\n") {
                Some(after) => text = after,
                None => return text,
            }
        }
    }
    let rest: String = input.chars().skip(end).collect();
    let mut rest = skip_blanks(rest.as_str());
    if let Some(after) = rest.strip_prefix(';') {
        rest = skip_blanks(after);
    }
    if rest.starts_with('#') {
        return rest
            .split_once('\n')
            .is_none_or(|(_, after)| after.is_empty());
    }
    rest.is_empty() || rest == "\n"
}

/// How many lines `to` is past `from` (negative when before it).
fn line_delta(to: usize, from: usize) -> isize {
    let magnitude = isize::try_from(to.abs_diff(from)).unwrap_or(isize::MAX);
    if to >= from { magnitude } else { -magnitude }
}

/// A process substitution's list as bash runs it: printed back from its parse (see
/// `brush_parser::print_comsub_list`) and read again, so its commands are numbered as that text
/// has them; `None` when that text does not parse.
fn reprinted_list(
    shell: &Shell<impl extensions::ShellExtensions>,
    list: &ast::CompoundList,
) -> Option<ast::CompoundList> {
    let text = brush_parser::print_comsub_list(list, &shell.parser_options());
    let program = shell.parse_string(text).ok()?;
    Some(ast::CompoundList(
        program
            .complete_commands
            .into_iter()
            .flat_map(|list| list.0)
            .collect(),
    ))
}

/// Numbers a process substitution's commands on from the line of the command it belongs to, as
/// bash numbers those of its text printed back; its first command is on that line.
fn number_substitution_list(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    list: &ast::CompoundList,
) {
    let base = shell.current_position().map(|position| position.line);
    let first = ast::SourceLocation::location(list).map(|span| span.start.line);
    if let (Some(base), Some(first)) = (base, first) {
        shell.shift_lines(line_delta(base, first));
    }
}

/// Whether the traps allow bash to run a command without forking: none of `EXIT`, `ERR` or a
/// caught signal is set, and no trap handler is running.
fn traps_allow_exec(shell: &Shell<impl extensions::ShellExtensions>) -> bool {
    let traps = shell.traps();
    traps
        .get_effective_handler(traps::TrapSignal::Exit)
        .is_none()
        && traps
            .get_effective_handler(traps::TrapSignal::Err)
            .is_none()
        && traps
            .signal_dispositions()
            .all(|(_, disposition)| disposition != traps::PipeDisposition::Caught)
        && !shell
            .call_stack()
            .iter()
            .any(|frame| frame.frame_type.is_trap_handler())
}

/// Encapsulates the context of execution in a command pipeline.
struct PipelineExecutionContext<'a, SE: extensions::ShellExtensions> {
    /// The shell in which the command should be executed.
    shell: commands::ShellForCommand<'a, SE>,
    /// Process group ID for spawned processes.
    process_group_id: Option<i32>,
}

/// Parameters for execution.
#[derive(Clone, Default)]
pub struct ExecutionParameters {
    /// The open files tracked by the current context.
    open_files: openfiles::OpenFiles,
    /// Output process substitutions (`>(list)`) set up for the command these parameters are
    /// for, waiting for it to finish (see `run_pending_output_substitutions`).
    #[cfg(target_arch = "wasm32")]
    pub(crate) output_substitutions: PendingOutputSubstitutions,
    /// Process substitutions inside the words of the command these parameters are for, set up
    /// as its words are expanded and waiting to be given to it (see
    /// `setup_word_process_substitution`).
    word_process_substitutions: WordProcessSubstitutions,
    /// Policy for how to manage spawned external processes.
    pub process_group_policy: ProcessGroupPolicy,
    /// Whether `errexit` (exit on error) behavior should be
    /// suppressed in this execution context. Defaults to `false`.
    pub suppress_errexit: bool,
    /// Embedder context, cloned into stages and substitutions rather than installed globally.
    context: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    /// Whether the command these parameters are for reads a pipe or has its own redirection of
    /// standard input; the commands inside it then keep that input when run in the background.
    pub(crate) stdin_redirected: bool,
    /// Whether a background command keeps standard input instead of reading /dev/null: the
    /// subshell it runs in reads a pipe or redirected input, or a compound command around it
    /// does (bash's `stdin_redir`).
    pub(crate) async_stdin_kept: bool,
}

impl ExecutionParameters {
    /// Attaches invocation-owned embedder context inherited by cloned execution parameters.
    /// The context is not shell state and is never serialized into a shell snapshot.
    pub fn set_context<T: std::any::Any + Send + Sync>(&mut self, context: std::sync::Arc<T>) {
        self.context = Some(context);
    }

    /// Retrieves this invocation's context when its type matches, without retaining a borrow.
    pub fn context<T: std::any::Any + Send + Sync>(&self) -> Option<std::sync::Arc<T>> {
        self.context.clone()?.downcast().ok()
    }

    /// Returns the standard input file; usable with `write!` et al.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn stdin(&self, shell: &Shell<impl extensions::ShellExtensions>) -> OpenFile {
        self.try_stdin(shell)
            .unwrap_or_else(|| ioutils::FailingReaderWriter::new("Bad file descriptor").into())
    }

    /// Tries to retrieve the standard input file. Returns `None` if not set.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn try_stdin(&self, shell: &Shell<impl extensions::ShellExtensions>) -> Option<OpenFile> {
        self.try_fd(shell, openfiles::OpenFiles::STDIN_FD)
    }

    /// Returns the standard output file; usable with `write!` et al. In the event that
    /// no such file is available, returns a valid implementation of `std::io::Write`
    /// that fails all I/O requests.
    ///
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn stdout(&self, shell: &Shell<impl extensions::ShellExtensions>) -> OpenFile {
        self.try_stdout(shell)
            .unwrap_or_else(|| ioutils::FailingReaderWriter::new("Bad file descriptor").into())
    }

    /// Tries to retrieve the standard output file. Returns `None` if not set.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn try_stdout(&self, shell: &Shell<impl extensions::ShellExtensions>) -> Option<OpenFile> {
        self.try_fd(shell, openfiles::OpenFiles::STDOUT_FD)
    }

    /// Returns the standard error file; usable with `write!` et al. In the event that
    /// no such file is available, returns a valid implementation of `std::io::Write`
    /// that fails all I/O requests.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn stderr(&self, shell: &Shell<impl extensions::ShellExtensions>) -> OpenFile {
        self.try_stderr(shell)
            .unwrap_or_else(|| ioutils::FailingReaderWriter::new("Bad file descriptor").into())
    }

    /// Tries to retrieve the standard error file. Returns `None` if not set.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn try_stderr(&self, shell: &Shell<impl extensions::ShellExtensions>) -> Option<OpenFile> {
        self.try_fd(shell, openfiles::OpenFiles::STDERR_FD)
    }

    /// Returns the file descriptor with the given number. Returns `None`
    /// if the file descriptor is not open.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    /// * `fd` - The file descriptor number to retrieve.
    pub fn try_fd(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
        fd: ShellFd,
    ) -> Option<openfiles::OpenFile> {
        match self.open_files.fd_entry(fd) {
            openfiles::OpenFileEntry::Open(f) => Some(f.clone()),
            openfiles::OpenFileEntry::NotPresent => None,
            openfiles::OpenFileEntry::NotSpecified => {
                // We didn't have this fd specified one way or the other; we fallback
                // to what's represented in the shell's open files.
                shell.persistent_open_files().try_fd(fd).cloned()
            }
        }
    }

    /// Sets the given file descriptor to the provided open file.
    ///
    /// # Arguments
    ///
    /// * `fd` - The file descriptor number to set.
    /// * `file` - The open file to set.
    pub fn set_fd(&mut self, fd: ShellFd, file: openfiles::OpenFile) {
        self.open_files.set_fd(fd, file);
    }

    /// Iterates over all open file descriptors in this context.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn iter_fds(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> impl Iterator<Item = (ShellFd, openfiles::OpenFile)> {
        let our_fds = self.open_files.iter_fds();
        let shell_fds = shell
            .persistent_open_files()
            .iter_fds()
            .filter(|(fd, _)| !self.open_files.contains_fd(*fd));

        #[allow(clippy::needless_collect)]
        let all_fds: Vec<_> = our_fds
            .chain(shell_fds)
            .map(|(fd, file)| (fd, file.clone()))
            .collect();

        all_fds.into_iter()
    }
}

#[derive(Clone, Debug, Default)]
/// Policy for how to manage spawned external processes.
pub enum ProcessGroupPolicy {
    /// Place the process in a new process group.
    #[default]
    NewProcessGroup,
    /// Place the process in the same process group as its parent.
    SameProcessGroup,
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait Execute {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error>;
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
trait ExecuteInPipeline<SE: extensions::ShellExtensions> {
    async fn execute_in_pipeline(
        &self,
        context: PipelineExecutionContext<'_, SE>,
        params: ExecutionParameters,
    ) -> Result<ExecutionSpawnResult, error::Error>;
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::Program {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // A parsed program (`eval`, `source`, a substitution, `sh -c`, a trap handler) costs more
        // stack than a list, so it counts as one more level of nesting (see `MAX_NESTING`).
        #[cfg(target_arch = "wasm32")]
        {
            shell.nesting += 1;
            let mut frame = crate::shell::FrameGuard::new(shell, leave_list, None);
            let result = execute_program(self, frame.shell(), params).await;
            frame.finish()?;
            result
        }
        #[cfg(not(target_arch = "wasm32"))]
        execute_program(self, shell, params).await
    }
}

async fn execute_program(
    program_ast: &ast::Program,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<ExecutionResult, error::Error> {
    {
        let mut result = ExecutionResult::success();
        let (program, interrupted) = shell.begin_program();
        let input = shell.pending_input.take();
        // A command string's last command may run in place of the shell, as bash's does.
        let outer_no_fork = shell.no_fork;
        if let Some(kind) = shell.exec_last.take() {
            shell.no_fork = NoFork::for_command_string(program_ast, kind, input.as_deref());
        }
        let mut next_input_line = 1;
        let mut quiet_lines: Option<SubstitutionLines> = None;

        for (index, command) in program_ast.complete_commands.iter().enumerate() {
            shell.begin_command_unit(program, index);
            // `set -v` echoes the input lines of each command as bash reads them, before it runs.
            if let Some(input) = &input {
                let end = ast::SourceLocation::location(command)
                    .map_or(next_input_line, |span| span.end.line);
                if shell.options().print_shell_input_lines && end >= next_input_line {
                    let quiet = quiet_lines.get_or_insert_with(|| {
                        lines_inside_substitutions(input, &shell.parser_options())
                    });
                    let mut stderr = params.stderr(shell);
                    for (number, line) in input
                        .lines()
                        .enumerate()
                        .skip(next_input_line - 1)
                        .take(end + 1 - next_input_line)
                    {
                        // A here-document's lines in a substitution are echoed as bash reads
                        // them while it parses the substitution: once, or twice in double
                        // quotes. (It echoes them again each time the substitution runs.)
                        if let Some((_, last, times)) = quiet
                            .here_documents
                            .iter()
                            .find(|(first, _, _)| *first == number + 1)
                        {
                            let block: Vec<&str> =
                                input.lines().skip(number).take(last - number).collect();
                            for _ in 0..*times {
                                for line in &block {
                                    let _ = writeln!(stderr, "{line}");
                                }
                            }
                        }
                        if !quiet.inside.contains(&(number + 1)) {
                            let _ = writeln!(stderr, "{line}");
                        }
                    }
                }
                next_input_line = next_input_line.max(end + 1);
            }
            // Execute the command and handle any errors without immediately propagating them.
            // This allows interactive shells to continue executing subsequent commands even after
            // errors.
            match command.execute(shell, params).await {
                Ok(exec_result) => result = exec_result,
                Err(err) => {
                    // Display the error and convert to an execution result.
                    let _ = shell.display_error(&mut params.stderr(shell), &err);
                    result = err.into_result(shell);
                }
            }

            // Update status
            shell.set_last_exit_status(result.exit_code.into());

            // Check if we should stop executing subsequent commands
            if !result.is_normal_flow() {
                break;
            }
        }

        shell.end_program(interrupted);
        shell.no_fork = outer_no_fork;
        Ok(result)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::CompoundList {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // Every nested execution (a function body, a subshell, a substitution, `eval`, `source`)
        // runs a list; one the stack cannot hold ends the shell rather than trapping.
        #[cfg(target_arch = "wasm32")]
        {
            if shell.nesting >= crate::shell::MAX_NESTING
                || crate::sys::wasm::stack::remaining() < crate::shell::STACK_RESERVE
            {
                return Err(error::Error::from(error::ErrorKind::NestingTooDeep).into_fatal());
            }
            shell.nesting += 1;
            let mut frame = crate::shell::FrameGuard::new(shell, leave_list, None);
            let result = execute_list(self, frame.shell(), params).await;
            frame.finish()?;
            result
        }
        #[cfg(not(target_arch = "wasm32"))]
        execute_list(self, shell, params).await
    }
}

/// Leaves a program or list entered by its `execute`.
#[cfg(target_arch = "wasm32")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "a frame guard's cleanup is fallible"
)]
const fn leave_list(
    shell: &mut Shell<impl extensions::ShellExtensions>,
) -> Result<(), error::Error> {
    shell.nesting = shell.nesting.saturating_sub(1);
    Ok(())
}

async fn execute_list(
    list: &ast::CompoundList,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<ExecutionResult, error::Error> {
    {
        let mut result = ExecutionResult::success();

        for ast::CompoundListItem(ao_list, sep) in &list.0 {
            let run_async = matches!(sep, ast::SeparatorOperator::Async);

            if run_async {
                #[cfg(target_arch = "wasm32")]
                if !job_slot_available(shell).await {
                    writeln!(
                        params.stderr(shell),
                        "bash: fork: retry: Resource temporarily unavailable"
                    )?;
                    result = ExecutionResult::new(1);
                    shell.set_last_exit_status(1);
                    continue;
                }
                let job = spawn_async_ao_list_in_task(ao_list, shell, params);
                let job_formatted = job.to_pid_style_string();
                let last_pid = job.representative_pid();
                if let Some(pid) = last_pid {
                    shell.set_last_background_pid(pid);
                }

                if shell.options().interactive && !shell.is_subshell() {
                    writeln!(params.stderr(shell), "{job_formatted}")?;
                }

                result = ExecutionResult::success();
            } else {
                #[cfg(target_arch = "wasm32")]
                give_other_tasks_a_turn(shell).await;
                result = match ao_list.execute(shell, params).await {
                    Ok(result) => result,
                    // An error that ends the shell is reported where it happened, while LINENO
                    // still names that line (in a function body, not the call).
                    Err(error) if error.is_fatal() && !error.is_reported() => {
                        let _ = shell.display_error(&mut params.stderr(shell), &error);
                        return Err(error.into_reported());
                    }
                    Err(error) => return Err(error),
                };

                // Update status
                shell.set_last_exit_status(result.exit_code.into());
            }

            if !result.is_normal_flow() {
                break;
            }
        }

        Ok(result)
    }
}

/// Commands a script runs between turns for the other tasks of the call.
#[cfg(target_arch = "wasm32")]
const COMMANDS_PER_TURN: u32 = 64;

#[cfg(target_arch = "wasm32")]
thread_local! {
    /// Commands run since the running script last gave the other tasks a turn.
    static COMMANDS_SINCE_TURN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Every [`COMMANDS_PER_TURN`] commands, lets the other tasks of the call run. On wasm32 every
/// task shares one thread, so a loop of commands that never wait (`while :; do x=1; done`)
/// would otherwise keep a timer from firing and a signal from being sent, and the signal from
/// ending it: a signal ends a process only when its body next waits.
#[cfg(target_arch = "wasm32")]
async fn give_other_tasks_a_turn(shell: &Shell<impl extensions::ShellExtensions>) {
    let due = COMMANDS_SINCE_TURN.with(|count| {
        let next = count.get() + 1;
        count.set(if next >= COMMANDS_PER_TURN { 0 } else { next });
        next >= COMMANDS_PER_TURN
    });
    if due {
        (shell.execution_services().yield_now)().await;
    }
}

#[cfg(target_arch = "wasm32")]
fn spawn_async_ao_list_in_task<'a, SE: extensions::ShellExtensions>(
    ao_list: &ast::AndOrList,
    shell: &'a mut Shell<SE>,
    params: &ExecutionParameters,
) -> &'a jobs::Job {
    use crate::execution::process;
    let table = shell.processes().clone();
    let command_line = jobs::job_text(ao_list);
    let leader = table.allocate_job(shell.own_pid(), command_line.clone());

    let mut cloned_shell = shell.clone();
    let mut cloned_params = params.clone();
    let cloned_ao_list = ao_list.clone();
    cloned_shell.options_mut().interactive = false;
    cloned_shell.set_own_pid(leader);
    // Bash resets caught handlers in asynchronous subshells; ignored signals stay ignored.
    cloned_shell.traps_mut().reset_caught_for_subshell();
    // Its input is /dev/null, unless a pipe or redirection around it provides one.
    if !params.async_stdin_kept
        && let Ok(null) = openfiles::null()
    {
        cloned_params.set_fd(openfiles::OpenFiles::STDIN_FD, null);
    }

    let dispositions = process::Dispositions::from_traps(cloned_shell.traps());
    // Register now, so `kill $!` and `kill %1` reach the job before its task first runs. Without
    // job control, it ignores INT and QUIT once it runs.
    let leader_process =
        process::NumberedProcess::register(&table, leader, dispositions).in_background();

    // A single background pipeline reports one number per stage; `$!` is the last stage.
    let stage_processes: VecDeque<_> = if ao_list.additional.is_empty()
        && ao_list.first.seq.len() > 1
        && !cloned_shell
            .options()
            .run_last_pipeline_cmd_in_current_shell
    {
        ao_list
            .first
            .seq
            .iter()
            .map(|command| {
                let pid = table.allocate(leader, command.to_string());
                leader_process
                    .register_child(pid, dispositions.for_exec())
                    .in_background()
            })
            .collect()
    } else {
        VecDeque::new()
    };
    let stage_pids: Vec<_> = stage_processes
        .iter()
        .map(process::NumberedProcess::pid)
        .collect();
    cloned_shell.set_stage_processes(stage_processes);

    // Bash forks once for `( list ) &`: the subshell is the job itself, so its traps are the job's.
    let subshell_body =
        sole_subshell_body(ao_list).map(|(list, redirects)| (list.clone(), redirects.cloned()));
    // The job outlives the subshell, stage or child shell that starts it, as an orphan does.
    // The job is a subshell: it runs only an EXIT trap it sets itself, when it ends.
    cloned_shell.traps_mut().reset_exit_for_subshell();
    let join_handle = process::spawn_job(&shell.execution_services(), &table, async move {
        let completed = std::cell::Cell::new(false);
        let result = leader_process
            .run(async {
                // A background command is forked as it starts, so a program it names runs in
                // place of that process; `( list ) &` is a subshell whose body runs as one.
                cloned_shell.stage_subshell = false;
                cloned_shell.paren_subshell = subshell_body.is_some();
                cloned_shell.no_fork = match &subshell_body {
                    Some((list, _)) => NoFork::for_subshell(list),
                    None => NoFork::for_background(&cloned_ao_list),
                };
                let result = match subshell_body {
                    Some((list, redirects)) => {
                        // `( list ) >log &` is the same one process, its output redirected. Its
                        // own background commands keep an input it redirects.
                        let mut redirected = Ok(());
                        if redirects
                            .iter()
                            .flat_map(|redirects| &redirects.0)
                            .any(redirects_stdin)
                        {
                            cloned_params.async_stdin_kept = true;
                        }
                        for redirect in redirects.iter().flat_map(|redirects| &redirects.0) {
                            redirected =
                                setup_redirect(&mut cloned_shell, &mut cloned_params, redirect)
                                    .await;
                            if redirected.is_err() {
                                break;
                            }
                        }
                        let executed = match redirected {
                            Ok(()) => list.execute(&mut cloned_shell, &cloned_params).await,
                            Err(error) => Err(error),
                        };
                        match executed {
                            Ok(result) => Ok(result),
                            Err(error) => {
                                let mut stderr = cloned_params.stderr(&cloned_shell);
                                let _ = cloned_shell.display_error(&mut stderr, &error);
                                Ok(error.into_result(&cloned_shell))
                            }
                        }
                    }
                    None => {
                        cloned_ao_list
                            .execute(&mut cloned_shell, &cloned_params)
                            .await
                    }
                };
                let result = cloned_shell
                    .exit_with_trap_in(result, &cloned_params)
                    .await?;
                completed.set(true);
                // A job reports only its status: its `exit`, `break` or `return` must never
                // act on the shell that later waits for it.
                Ok(ExecutionResult::from(result.exit_code))
            })
            .await;
        if !completed.get() {
            cloned_shell
                .exit_trap_after_signal(&result, &cloned_params)
                .await;
        }
        result
    });

    let stage_pids_len = stage_pids.len();
    let pids = if stage_pids.is_empty() {
        vec![leader]
    } else {
        stage_pids
    };
    if let Some(last) = pids.last() {
        table.set_shown_pid(leader, *last);
    }
    let stages = if stage_pids_len > 1 {
        jobs::stage_texts(&ao_list.first)
    } else {
        Vec::new()
    };
    shell.jobs_mut().add_as_current(jobs::Job::new_numbered(
        [jobs::JobTask::Internal(join_handle)],
        command_line,
        leader,
        pids,
        stages,
    ))
}

/// Whether a pipeline stage adds no subshell level (`BASH_SUBSHELL`) of its own, as in bash: a
/// simple command runs in the stage's process as it is, and a `( list )` is the stage's subshell.
const fn stage_adds_no_subshell(command: &ast::Command) -> bool {
    matches!(
        command,
        ast::Command::Simple(_) | ast::Command::Compound(ast::CompoundCommand::Subshell(_), _)
    )
}

/// Turns the jobs already started get to finish before the job cap refuses another one.
#[cfg(target_arch = "wasm32")]
const JOB_SLOT_TURNS: usize = 64;

/// Whether another background job may start: fewer than [`jobs::MAX_RUNNING_JOBS`] run in the
/// whole session. Starting a job does not run it, so a tight `&` loop reaches the cap with jobs
/// that have not had a turn yet; they get a few turns to finish before the cap refuses a job.
#[cfg(target_arch = "wasm32")]
async fn job_slot_available(shell: &Shell<impl extensions::ShellExtensions>) -> bool {
    let services = shell.execution_services();
    for _ in 0..JOB_SLOT_TURNS {
        if shell.processes().running_jobs() < jobs::MAX_RUNNING_JOBS {
            return true;
        }
        (services.yield_now)().await;
    }
    shell.processes().running_jobs() < jobs::MAX_RUNNING_JOBS
}

/// The body of a background list that is exactly one `( list )`, and its redirections. A
/// redirection to an output process substitution runs only after its command, so a subshell
/// with one keeps its own process.
#[cfg(target_arch = "wasm32")]
fn sole_subshell_body(
    ao_list: &ast::AndOrList,
) -> Option<(&ast::CompoundList, Option<&ast::RedirectList>)> {
    let pipeline = &ao_list.first;
    if !ao_list.additional.is_empty() || pipeline.bang || pipeline.timed.is_some() {
        return None;
    }
    match pipeline.seq.as_slice() {
        [
            ast::Command::Compound(
                ast::CompoundCommand::Subshell(ast::SubshellCommand { list, .. }),
                redirects,
            ),
        ] if redirects.as_ref().is_none_or(|redirects| {
            !redirects.0.iter().any(|redirect| {
                matches!(
                    redirect,
                    ast::IoRedirect::File(_, _, ast::IoFileRedirectTarget::ProcessSubstitution(..))
                        | ast::IoRedirect::NamedFd(
                            _,
                            _,
                            ast::IoFileRedirectTarget::ProcessSubstitution(..)
                        )
                )
            })
        }) =>
        {
            Some((list, redirects.as_ref()))
        }
        _ => None,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_async_ao_list_in_task<'a, SE: extensions::ShellExtensions>(
    ao_list: &ast::AndOrList,
    shell: &'a mut Shell<SE>,
    params: &ExecutionParameters,
) -> &'a jobs::Job {
    // Clone the inputs.
    let mut cloned_shell = shell.clone();
    let mut cloned_params = params.clone();
    let cloned_ao_list = ao_list.clone();

    // Mark the child shell as not interactive; we don't want it messing with the terminal too much.
    cloned_shell.options_mut().interactive = false;

    // Redirect stdin to null, per spec.
    if let Ok(null) = openfiles::null() {
        cloned_params.set_fd(openfiles::OpenFiles::STDIN_FD, null);
    }

    let join_handle = spawn_command_task(shell.execution_services(), async move {
        cloned_shell.stage_subshell = false;
        cloned_shell.paren_subshell = false;
        cloned_shell.no_fork = NoFork::for_background(&cloned_ao_list);
        cloned_ao_list
            .execute(&mut cloned_shell, &cloned_params)
            .await
    });

    shell.jobs_mut().add_as_current(jobs::Job::new(
        [jobs::JobTask::Internal(join_handle)],
        jobs::job_text(ao_list),
        jobs::JobState::Running,
    ))
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::AndOrList {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let has_operators = !self.additional.is_empty();

        // For the first command, suppress errexit if there are more commands after it
        let mut first_params = params.clone();
        if has_operators {
            first_params.suppress_errexit = true;
        }

        let mut result = self.first.execute(shell, &first_params).await?;

        for (index, next_ao) in self.additional.iter().enumerate() {
            // Check for non-normal control flow.
            if !result.is_normal_flow() {
                break;
            }

            let (is_and, pipeline) = match next_ao {
                ast::AndOr::And(p) => (true, p),
                ast::AndOr::Or(p) => (false, p),
            };

            // If we short-circuit, then we don't break out of the whole loop
            // but we skip evaluating the current pipeline. We'll then continue
            // on and possibly evaluate a subsequent one (depending on the
            // operator before it).
            if is_and {
                if !result.is_success() {
                    continue;
                }
            } else if result.is_success() {
                continue;
            }

            // For the last command in the chain, use original params (errexit not suppressed)
            // For earlier commands, suppress errexit
            let mut params = params.clone();

            let is_last = index == self.additional.len() - 1;
            if !is_last {
                params.suppress_errexit = true;
            }

            result = pipeline.execute(shell, &params).await?;
        }

        Ok(result)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::Pipeline {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // Capture current timing if so requested.
        let stopwatch = self
            .timed
            .is_some()
            .then(timing::start_timing)
            .transpose()?;

        let mut params = params.clone();

        // If this pipeline is negated, suppress errexit for commands within it
        if self.bang {
            params.suppress_errexit = true;
        }

        // Spawn all the processes required for the pipeline, connecting outputs/inputs with pipes
        // as needed.
        // `spawned` stays alive until this function returns: on wasm32 it owns the stage tasks, and
        // dropping it aborts any still running.
        let spawned = spawn_pipeline_processes(self, shell, &params).await?;

        // Wait for the processes. This also has a side effect of updating pipeline status.
        let wait_result =
            wait_for_pipeline_processes_and_update_status(self, spawned.results, shell, &params)
                .await;
        #[cfg(target_arch = "wasm32")]
        spawned._stage_tasks.cancel_and_join().await;
        let (mut result, last_signal) = wait_result?;
        // A foreground command a signal ended is reported as bash reports it. Its signal is not
        // reported again by the commands around it.
        #[cfg(target_arch = "wasm32")]
        if let Some(signal) = last_signal {
            report_signal_death(shell, &params, self, signal)?;
            result.terminating_signal = None;
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = last_signal;

        // Invert the exit code if requested.
        if self.bang {
            result.exit_code = ExecutionExitCode::from(if result.is_success() { 1 } else { 0 });
            result.terminating_signal = None;
        }

        // Update exit status.
        shell.set_last_exit_status(result.exit_code.into());

        // Fire the ERR trap if the pipeline failed in a non-conditional context.
        // We reuse `suppress_errexit` here because bash suppresses the ERR trap in
        // exactly the same contexts it suppresses errexit (conditionals, `!`-prefixed
        // pipelines, etc.).
        if !result.is_success()
            && !params.suppress_errexit
            && !self.bang
            && !runs_its_own_commands(self)
        {
            if shell.traps().handles(crate::traps::TrapSignal::Err) {
                shell
                    .invoke_trap_handler(crate::traps::TrapSignal::Err, &params)
                    .await?;
            }
        }

        // Apply errexit if not suppressed (and not negated)
        if !params.suppress_errexit && !self.bang {
            shell.apply_errexit_if_enabled(&mut result);
        }

        // If requested, report timing, in TIMEFORMAT (bash's default when it is unset; nothing
        // when it is empty) or, for `time -p`, the POSIX format.
        if let (Some(timed), Some(stopwatch)) = (&self.timed, &stopwatch)
            && let Some(mut stderr) = params.try_fd(shell, openfiles::OpenFiles::STDERR_FD)
        {
            let timing = stopwatch.stop()?;
            let format = if timed.is_posix_output() {
                timing::POSIX_TIMEFORMAT.to_owned()
            } else {
                shell
                    .env()
                    .get("TIMEFORMAT")
                    .filter(|(_, var)| var.value().is_set())
                    .map_or_else(
                        || timing::BASH_TIMEFORMAT.to_owned(),
                        |(_, var)| var.value().to_cow_str(shell).into_owned(),
                    )
            };
            if !format.is_empty() {
                match timing::format_timing(&format, &timing) {
                    Ok(text) => std::writeln!(stderr, "{text}")?,
                    Err(message) => {
                        std::writeln!(stderr, "{}{message}", shell.diagnostic_prefix())?;
                    }
                }
            }
        }

        Ok(result)
    }
}

/// Whether the pipeline is one compound command that runs its commands in this shell (a group,
/// a loop, `if` or `case`), or a function definition. The commands inside set PIPESTATUS and run
/// the ERR trap themselves, as in bash; `(( ))`, `[[ ]]` and `( )` do both as commands.
const fn runs_its_own_commands(pipeline: &ast::Pipeline) -> bool {
    matches!(
        pipeline.seq.as_slice(),
        [ast::Command::Function(_)
            | ast::Command::Compound(
                ast::CompoundCommand::BraceGroup(_)
                    | ast::CompoundCommand::ForClause(_)
                    | ast::CompoundCommand::ArithmeticForClause(_)
                    | ast::CompoundCommand::SelectClause(_)
                    | ast::CompoundCommand::CaseClause(_)
                    | ast::CompoundCommand::IfClause(_)
                    | ast::CompoundCommand::WhileClause(_)
                    | ast::CompoundCommand::UntilClause(_),
                _,
            )]
    )
}

async fn spawn_pipeline_processes(
    pipeline: &ast::Pipeline,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<SpawnedPipeline, error::Error> {
    let pipeline_len = pipeline.seq.len();
    let mut pipe_readers = vec![];
    let mut pipe_writers = vec![];
    let mut spawn_results = VecDeque::new();
    let mut process_group_id: Option<i32> = None;

    // On wasm32, the stage tasks spawned so far. Held here from the first spawn, so an early return
    // below aborts them rather than leaving them running.
    #[cfg(target_arch = "wasm32")]
    let mut stage_tasks = StageTasks::default();

    // Create pipes to use between commands, but only bother doing so if there's more than one
    // command.
    if pipeline_len > 1 {
        pipe_readers.reserve_exact(pipeline_len - 1);
        pipe_writers.reserve_exact(pipeline_len - 1);

        for _ in 0..(pipeline_len - 1) {
            // On wasm32 there are no OS pipes (`std::io::pipe()` errors) and one thread, so stages
            // connect through in-memory pipes and run as cooperating tasks (see
            // `openfiles::open_mem_pipe` and `spawn_pipeline_stage`).
            #[cfg(target_arch = "wasm32")]
            let (reader, writer) = {
                let (reader, writer) = openfiles::open_mem_pipe();
                (reader, writer)
            };
            #[cfg(not(target_arch = "wasm32"))]
            let (reader, writer) = {
                let (r, w) = std::io::pipe()?;
                (openfiles::OpenFile::from(r), openfiles::OpenFile::from(w))
            };
            pipe_readers.push(Some(reader));
            pipe_writers.push(Some(writer));
        }
        // Push `None` to the readers; it will be popped off by the *first* command, which will
        // mean that command gets its stdin from the execution parameters' current stdin.
        pipe_readers.push(None);
    }

    for (current_pipeline_index, command) in pipeline.seq.iter().enumerate() {
        //
        // We run a command directly in the current shell if either of the following is true:
        //     * There's only one command in the pipeline.
        //     * This is the *last* command in the pipeline, the lastpipe option is enabled, and job
        //       monitoring is disabled.
        // Otherwise, we spawn a separate subshell for each command in the pipeline.
        //

        let run_in_current_shell = pipeline_len == 1
            || (current_pipeline_index == pipeline_len - 1
                && shell.options().run_last_pipeline_cmd_in_current_shell
                && !shell.options().enable_job_control);

        // Set up parameters appropriate for this command.
        let mut cmd_params = params.clone();

        // Install pipes.
        cmd_params.stdin_redirected = false;
        if let Some(Some(reader)) = pipe_readers.pop() {
            cmd_params.open_files.set_fd(OpenFiles::STDIN_FD, reader);
            cmd_params.stdin_redirected = true;
        }
        if let Some(Some(writer)) = pipe_writers.pop() {
            cmd_params.open_files.set_fd(OpenFiles::STDOUT_FD, writer);
        }

        // On wasm32, every stage that does not run in the current shell becomes its own task, so
        // stages interleave instead of each running to completion before the next starts.
        #[cfg(target_arch = "wasm32")]
        {
            if !run_in_current_shell {
                let stage_process = shell.take_stage_process();
                let debug_trap_ran = stage_debug_trap(shell, params, command).await?;
                let mut stage_shell = shell.clone();
                stage_shell.debug_trap_ran = debug_trap_ran;
                stage_shell.stage_command = matches!(command, ast::Command::Simple(_));
                stage_shell.stage_subshell = true;
                stage_shell.paren_subshell = false;
                if stage_adds_no_subshell(command) {
                    stage_shell.subshell_level = shell.subshell_level;
                }
                let join_handle =
                    spawn_pipeline_stage(stage_shell, command.clone(), cmd_params, stage_process);
                stage_tasks.0.push(join_handle.abort_handle());
                spawn_results.push_back(ExecutionSpawnResult::StartedTask(join_handle));
                continue;
            }
        }

        let pipeline_context = if !run_in_current_shell {
            // Make sure that all commands in the pipeline are in the same process group.
            if current_pipeline_index > 0 {
                cmd_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;
            }

            let debug_trap_ran = stage_debug_trap(shell, params, command).await?;
            let mut stage_shell = shell.clone();
            stage_shell.debug_trap_ran = debug_trap_ran;
            stage_shell.stage_command = matches!(command, ast::Command::Simple(_));
            stage_shell.stage_subshell = true;
            stage_shell.paren_subshell = false;
            if stage_adds_no_subshell(command) {
                stage_shell.subshell_level = shell.subshell_level;
            }
            PipelineExecutionContext {
                shell: commands::ShellForCommand::OwnedShell {
                    target: Box::new(stage_shell),
                    parent: shell,
                },
                process_group_id,
            }
        } else {
            PipelineExecutionContext {
                shell: commands::ShellForCommand::ParentShell(shell),
                process_group_id,
            }
        };

        let spawn_result = command
            .execute_in_pipeline(pipeline_context, cmd_params)
            .await;
        #[cfg(target_arch = "wasm32")]
        if spawn_result.is_err() {
            stage_tasks.cancel_and_join().await;
        }
        let spawn_result = spawn_result?;

        // Update the process group ID if something was spawned.
        if let ExecutionSpawnResult::StartedProcess(child) = &spawn_result {
            if process_group_id.is_none() {
                process_group_id = child.pgid();
            }
        }

        spawn_results.push_back(spawn_result);
    }

    Ok(SpawnedPipeline {
        results: spawn_results,
        #[cfg(target_arch = "wasm32")]
        _stage_tasks: stage_tasks,
    })
}

/// The spawned stages of a pipeline, in pipeline order.
struct SpawnedPipeline {
    results: VecDeque<ExecutionSpawnResult>,
    /// On `wasm32`, the stages running as tasks; held only so that dropping it aborts them. See
    /// [`StageTasks`].
    #[cfg(target_arch = "wasm32")]
    _stage_tasks: StageTasks,
}

/// Aborts a pipeline's stage tasks that are still running when the pipeline itself is dropped.
///
/// A pipeline future can be dropped when its containing process terminates or a stage fails to
/// start. These controls signal cancellation immediately; the owning execution scope retains
/// completion observers and joins cleanup before returning. Finished tasks are unaffected.
#[cfg(target_arch = "wasm32")]
#[derive(Default)]
struct StageTasks(Vec<crate::execution::TaskControl>);

#[cfg(target_arch = "wasm32")]
impl StageTasks {
    async fn cancel_and_join(&self) {
        for task in &self.0 {
            task.abort();
        }
        for task in &self.0 {
            task.clone().join().await;
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for StageTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Registers a new numbered process for `subshell`, a copy of the shell about to run as a
/// subshell, and makes it the subshell's `$BASHPID`. It inherits the running process's
/// dispositions (caught handlers reset) and the subshell's own PIPE disposition.
#[cfg(target_arch = "wasm32")]
pub(crate) fn subshell_process(
    subshell: &mut Shell<impl extensions::ShellExtensions>,
) -> crate::execution::process::NumberedProcess {
    use crate::execution::process;
    let table = subshell.processes().clone();
    let pid = table.allocate(subshell.own_pid(), String::new());
    subshell.set_own_pid(pid);
    let dispositions = process::inherited_dispositions(subshell.traps().pipe_disposition());
    process::NumberedProcess::register(&table, pid, dispositions)
}

/// Runs one pipeline stage as its own task on `wasm32`, in a copy of the shell (as a subshell).
///
/// Stages share one thread and hand control to each other at the yield points in the in-memory
/// pipes. If this stage writes into a pipe after every reader is gone — the downstream stage has
/// finished — default SIGPIPE ends the writing logical process with status 141. An ignored or
/// caught SIGPIPE instead leaves error handling to the command and its shell trap safe point.
#[cfg(target_arch = "wasm32")]
fn spawn_pipeline_stage<SE: extensions::ShellExtensions>(
    mut shell: Shell<SE>,
    command: ast::Command,
    params: ExecutionParameters,
    numbered: Option<crate::execution::process::NumberedProcess>,
) -> crate::execution::CommandTask {
    let services = shell.execution_services();
    shell.traps_mut().reset_pipe_for_subshell();
    // A stage is a subshell: it runs only an EXIT trap it sets itself, when it ends.
    shell.traps_mut().reset_exit_for_subshell();
    // It is a process of its own, with its own `$BASHPID`: a background pipeline registered its
    // stages already.
    let numbered = match numbered {
        Some(numbered) => {
            shell.set_own_pid(numbered.pid());
            numbered
        }
        None => subshell_process(&mut shell),
    };
    services.spawn(async move {
        let completed = std::cell::Cell::new(false);
        let body = async {
            let context = PipelineExecutionContext {
                shell: commands::ShellForCommand::ParentShell(&mut shell),
                process_group_id: None,
            };
            let result = match command.execute_in_pipeline(context, params.clone()).await {
                Ok(spawned) => match spawned.wait().await {
                    Ok(ExecutionWaitResult::Completed(result)) => Ok(result),
                    Ok(ExecutionWaitResult::Stopped(_)) => Ok(ExecutionResult::stopped()),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            // An error that ends the stage (`${x:?}`) ends only the stage, which exits with the
            // error's status, as a bash subshell does.
            let result = result.or_else(|error: error::Error| {
                let _ = shell.display_error(&mut params.stderr(&shell), &error);
                Ok(ExecutionResult::from(error.into_result(&shell).exit_code))
            });
            let result = shell.exit_with_trap_in(result, &params).await;
            completed.set(true);
            // A stage is a subshell: its `exit`, `break` or `return` ends only the stage.
            result.map(|result| ExecutionResult {
                terminating_signal: result.terminating_signal,
                ..ExecutionResult::from(result.exit_code)
            })
        };
        let result = numbered.run(body).await;
        if !completed.get() {
            shell.exit_trap_after_signal(&result, &params).await;
        }
        result
    })
}

/// Reports a foreground command that `signal` ended, as bash does: TERM with its description
/// and the command, other signals (but INT and PIPE) with the line and the process number too,
/// unless the shell traps them.
#[cfg(target_arch = "wasm32")]
fn report_signal_death(
    shell: &Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    pipeline: &ast::Pipeline,
    signal: u8,
) -> Result<(), error::Error> {
    use crate::execution::process::signals;
    if matches!(signal, signals::INT | signals::PIPE) {
        return Ok(());
    }
    let description = traps::signal_description(signal);
    let text = jobs::pipeline_text(pipeline);
    if signal == signals::TERM {
        writeln!(params.stderr(shell), "{description:<27}{text}")?;
    } else if !traps::TrapSignal::try_from(i32::from(signal))
        .is_ok_and(|trapped| shell.traps().handles(trapped))
    {
        let pid = shell
            .processes()
            .last_signaled_child(shell.own_pid())
            .map_or_else(String::new, |pid| pid.to_string());
        let prefix = shell.diagnostic_prefix();
        writeln!(
            params.stderr(shell),
            "{prefix}{pid:>5} {description:<27}{text}"
        )?;
    }
    Ok(())
}

/// Waits for a pipeline's processes and records their statuses. Also returns the signal that
/// ended the last one, if one did.
async fn wait_for_pipeline_processes_and_update_status(
    pipeline: &ast::Pipeline,
    mut process_spawn_results: VecDeque<ExecutionSpawnResult>,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<(ExecutionResult, Option<u8>), error::Error> {
    let mut result = ExecutionResult::success();
    let mut stopped_children = vec![];
    let mut last_failure_exit_code: Option<(ExecutionExitCode, Option<u8>)> = None;

    // A compound command or function definition run on its own in this shell leaves PIPESTATUS
    // as the last pipeline it ran set it, as in bash; `(( ))`, `[[ ]]` and `( )` set it.
    let keeps_statuses = runs_its_own_commands(pipeline);

    // Clear our the pipeline status so we can start filling it out.
    if !keeps_statuses {
        shell.last_pipeline_statuses_mut().clear();
    }

    while let Some(child) = process_spawn_results.pop_front() {
        let wait_result = if !stopped_children.is_empty() {
            child.poll().await?
        } else {
            child.wait().await?
        };

        match wait_result {
            ExecutionWaitResult::Completed(current_result) => {
                result = current_result;
                shell.set_last_exit_status(result.exit_code.into());
                if !keeps_statuses {
                    shell
                        .last_pipeline_statuses_mut()
                        .push(result.exit_code.into());
                }

                // Track the last failure for pipefail option
                if !result.is_success() {
                    last_failure_exit_code = Some((result.exit_code, result.terminating_signal));
                }
            }
            ExecutionWaitResult::Stopped(child) => {
                result = ExecutionResult::stopped();
                shell.set_last_exit_status(result.exit_code.into());
                shell
                    .last_pipeline_statuses_mut()
                    .push(result.exit_code.into());

                stopped_children.push(jobs::JobTask::External(child));
            }
        }
    }

    let last_signal = result.terminating_signal;

    // Apply pipefail semantics if enabled
    if shell.options().return_last_failure_from_pipeline {
        if let Some((failure_exit_code, terminating_signal)) = last_failure_exit_code {
            result.exit_code = failure_exit_code;
            result.terminating_signal = terminating_signal;
        }
    }

    if shell.options().interactive {
        sys::terminal::move_self_to_foreground()?;
    }

    // If there were stopped jobs, then encapsulate the pipeline as a managed job and hand it
    // off to the job manager.
    if !stopped_children.is_empty() {
        let job = shell.jobs_mut().add_as_current(jobs::Job::new(
            stopped_children,
            pipeline.to_string(),
            jobs::JobState::Stopped,
        ));

        let formatted = job.to_string();

        // N.B. We use the '\r' to overwrite any ^Z output.
        writeln!(params.stderr(shell), "\r{formatted}")?;
    }

    Ok((result, last_signal))
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl<SE: extensions::ShellExtensions> ExecuteInPipeline<SE> for ast::Command {
    async fn execute_in_pipeline(
        &self,
        mut pipeline_context: PipelineExecutionContext<'_, SE>,
        mut params: ExecutionParameters,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        if pipeline_context.shell.options().do_not_execute_commands {
            return Ok(ExecutionSpawnResult::Completed(ExecutionResult::success()));
        }

        // Updates the shell with information about the currently executing command. Bash numbers
        // a simple command by the line its first word (or assignment, or redirection) ends on,
        // and `(( ))` and `[[ ]]` by their last line.
        pipeline_context.shell.set_current_cmd(self);
        match self {
            Self::Simple(simple) => {
                let first = simple
                    .prefix
                    .as_ref()
                    .and_then(|prefix| prefix.0.first())
                    .and_then(ast::SourceLocation::location)
                    .or_else(|| {
                        simple
                            .word_or_name
                            .as_ref()
                            .and_then(ast::SourceLocation::location)
                    });
                pipeline_context
                    .shell
                    .set_current_position(first.map(|span| span.end));
            }
            Self::Compound(
                compound @ (ast::CompoundCommand::Arithmetic(_)
                | ast::CompoundCommand::ExtendedTest(_)),
                _,
            ) => {
                pipeline_context.shell.set_current_position(
                    ast::SourceLocation::location(compound).map(|span| span.end),
                );
            }
            _ => {}
        }

        match self {
            Self::Simple(simple) => simple.execute_in_pipeline(pipeline_context, params).await,
            Self::Compound(compound, redirects) => {
                // `>(list)` substitutions in these redirects run once the command has finished.
                #[cfg(target_arch = "wasm32")]
                let pending = params.own_output_substitutions();

                // `(( ))`, `[[ ]]` and `( )` are commands in their own right: they become
                // BASH_COMMAND, and the DEBUG trap runs before `(( ))` and `[[ ]]`, as in bash.
                let text = match compound {
                    // The expression as written, blanks and all (`((  1  ))`).
                    ast::CompoundCommand::Arithmetic(arithmetic) => {
                        Some((arithmetic.to_string(), true))
                    }
                    ast::CompoundCommand::ExtendedTest(test) => {
                        Some((format!("[[ {test} ]]"), true))
                    }
                    ast::CompoundCommand::Subshell(subshell) => Some((subshell.to_string(), false)),
                    _ => None,
                };
                if let Some((text, debug)) = text {
                    let shell = &mut pipeline_context.shell;
                    if !shell.running_trap_handler() {
                        shell.env_mut().update_or_add(
                            "BASH_COMMAND",
                            ShellValueLiteral::Scalar(text),
                            |_| Ok(()),
                            EnvironmentLookup::Anywhere,
                            EnvironmentScope::Global,
                        )?;
                    }
                    if debug && shell.traps().handles(traps::TrapSignal::Debug) {
                        shell
                            .invoke_trap_handler(traps::TrapSignal::Debug, &params)
                            .await?;
                    }
                }

                // Set up any additional redirects. One that fails fails the command, and the
                // list goes on. Bash names the line the command ends on.
                if let Some(redirects) = redirects {
                    let position = pipeline_context.shell.current_position();
                    pipeline_context.shell.set_current_position(
                        ast::SourceLocation::location(redirects).map(|span| span.end),
                    );
                    for redirect in &redirects.0 {
                        if let Err(error) =
                            setup_redirect(&mut pipeline_context.shell, &mut params, redirect).await
                        {
                            let shell = &mut pipeline_context.shell;
                            let _ = shell.display_error(&mut params.stderr(shell), &error);
                            return Ok(ExecutionResult::general_error().into());
                        }
                    }
                    pipeline_context.shell.set_current_position(position);
                    if redirects.0.iter().any(redirects_stdin) {
                        params.stdin_redirected = true;
                    }
                }

                let result = compound.execute(&mut pipeline_context.shell, &params).await;
                #[cfg(target_arch = "wasm32")]
                run_pending_output_substitutions(&pipeline_context.shell, &pending).await;
                Ok(result?.into())
            }
            Self::Function(func) => Ok(func
                .execute(&mut pipeline_context.shell, &params)
                .await?
                .into()),
        }
    }
}

enum WhileOrUntil {
    While,
    Until,
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::CompoundCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // As in bash, a background command reads /dev/null, unless the subshell it runs in, or a
        // compound command around it, reads a pipe or redirects standard input.
        let stdin_redirected = params.stdin_redirected;
        let inner_params;
        let params = if stdin_redirected {
            inner_params = ExecutionParameters {
                stdin_redirected: false,
                async_stdin_kept: true,
                ..params.clone()
            };
            &inner_params
        } else {
            params
        };
        match self {
            Self::BraceGroup(ast::BraceGroupCommand { list, .. }) => {
                list.execute(shell, params).await
            }
            Self::Subshell(ast::SubshellCommand { list, .. }) => {
                // A new subshell keeps input for background commands only if it reads a pipe or
                // redirected input itself.
                let subshell_params;
                let params = if params.async_stdin_kept == stdin_redirected {
                    params
                } else {
                    subshell_params = ExecutionParameters {
                        async_stdin_kept: stdin_redirected,
                        ..params.clone()
                    };
                    &subshell_params
                };
                // Clone off a new subshell, and run the body of the subshell there.
                // TODO(source-info): Do we need to reset the line number?
                let mut subshell = shell.clone();
                #[cfg(target_arch = "wasm32")]
                subshell.traps_mut().reset_pipe_for_subshell();
                // The subshell runs only an EXIT trap it sets itself, when its body ends.
                subshell.traps_mut().reset_exit_for_subshell();
                // `break` in a `( ... )` does not reach the loops around it.
                subshell.loop_depth = 0;
                // Nor does it list the jobs around it, as a pipeline stage does.
                subshell.jobs_mut().jobs.clear();
                // Its last command may run in place of the subshell's process.
                subshell.paren_subshell = true;
                subshell.stage_subshell = false;
                subshell.no_fork = NoFork::for_subshell(list);
                // It is a process of its own, with its own `$BASHPID`.
                #[cfg(target_arch = "wasm32")]
                let numbered_subshell = subshell_process(&mut subshell);
                #[cfg(target_arch = "wasm32")]
                let completed = std::cell::Cell::new(false);
                let body = async {
                    let result = list.execute(&mut subshell, params).await;
                    let result = subshell.exit_with_trap_in(result, params).await;
                    #[cfg(target_arch = "wasm32")]
                    completed.set(true);
                    result
                };

                // Handle errors within the subshell context to prevent fatal errors
                // from propagating to the parent shell.
                #[cfg(target_arch = "wasm32")]
                let execution = numbered_subshell.run(body).await;
                #[cfg(not(target_arch = "wasm32"))]
                let execution = body.await;
                // A signal ended the subshell before its commands finished.
                #[cfg(target_arch = "wasm32")]
                if !completed.get() {
                    subshell.exit_trap_after_signal(&execution, params).await;
                }
                let subshell_result = match execution {
                    Ok(result) => result,
                    Err(error) => {
                        // Display the error to stderr, but prevent fatal error propagation
                        let mut stderr = params.stderr(shell);
                        let _ = shell.display_error(&mut stderr, &error);

                        // Convert error to result in subshell context
                        error.into_result(&subshell)
                    }
                };

                // Preserve the subshell's exit code, but don't honor any of its requests to exit
                // the shell, break out of loops, etc. A signal that ended it is reported by the
                // pipeline it belongs to.
                #[cfg(target_arch = "wasm32")]
                if !completed.get() {
                    return Ok(ExecutionResult {
                        terminating_signal: subshell_result.terminating_signal,
                        ..ExecutionResult::from(subshell_result.exit_code)
                    });
                }
                Ok(ExecutionResult::from(subshell_result.exit_code))
            }
            Self::ForClause(f) => f.execute(shell, params).await,
            Self::SelectClause(s) => s.execute(shell, params).await,
            Self::CaseClause(c) => c.execute(shell, params).await,
            Self::IfClause(i) => i.execute(shell, params).await,
            Self::WhileClause(w) => (WhileOrUntil::While, w).execute(shell, params).await,
            Self::UntilClause(u) => (WhileOrUntil::Until, u).execute(shell, params).await,
            Self::Arithmetic(a) => a.execute(shell, params).await,
            Self::ArithmeticForClause(a) => a.execute(shell, params).await,
            Self::Coprocess(c) => c.execute(shell, params).await,
            Self::ExtendedTest(e) => {
                let result = match extendedtests::eval_extended_test_expr(&e.expr, shell, params)
                    .await
                {
                    Ok(true) => 0,
                    Ok(false) => 1,
                    // A regular expression that does not compile fails the test with status 2.
                    Err(error) if matches!(error.kind(), error::ErrorKind::InvalidRegex(..)) => {
                        writeln!(
                            params.stderr(shell),
                            "{}[[: {error}",
                            shell.diagnostic_prefix()
                        )?;
                        return Ok(ExecutionResult::new(2));
                    }
                    Err(error) => {
                        return match error.into_eval_error() {
                            Ok(error) => arithmetic_command_error(shell, params, "[[", &error),
                            Err(error) => Err(error),
                        };
                    }
                };
                Ok(ExecutionResult::new(result))
            }
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::CoprocessCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        if shell.options().do_not_execute_commands {
            return Ok(ExecutionResult::success());
        }

        // Resolve the name of the variable that will receive the coprocess's file descriptors.
        let name = self
            .name
            .as_ref()
            .map_or(Cow::Borrowed("COPROC"), |w| Cow::Owned(w.to_string()));

        if !valid_variable_name(&name) {
            writeln!(
                params.stderr(shell),
                "coproc {name}: not a valid identifier"
            )?;
            return Ok(ExecutionExitCode::GeneralError.into());
        }

        // Set up the pipes that we'll use to communicate with the coprocess.
        let (stdin_reader, stdin_writer) = std::io::pipe()?;
        let (stdout_reader, stdout_writer) = std::io::pipe()?;

        // Allocate new fds in the (parent) shell for the read end of the coprocess's stdout
        // and the write end of the coprocess's stdin.
        let stdout_fd = shell.open_files_mut().add(stdout_reader.into())?;
        let stdin_fd = shell.open_files_mut().add(stdin_writer.into())?;

        // Crete a subshell that the coprocess will own and run in.
        let mut child_shell = shell.clone();
        child_shell.options_mut().interactive = false;

        // Setup redirection for the coprocess's shell's stdin/stdout.
        let mut child_params = params.clone();
        child_params
            .open_files
            .set_fd(OpenFiles::STDIN_FD, stdin_reader.into());
        child_params
            .open_files
            .set_fd(OpenFiles::STDOUT_FD, stdout_writer.into());

        let body = self.body.clone();
        let join_handle = spawn_command_task(shell.execution_services(), async move {
            let pipeline_context = PipelineExecutionContext {
                shell: commands::ShellForCommand::ParentShell(&mut child_shell),
                process_group_id: None,
            };
            let spawn_result = body
                .execute_in_pipeline(pipeline_context, child_params)
                .await?;
            match spawn_result.wait().await? {
                ExecutionWaitResult::Completed(result) => Ok(result),
                ExecutionWaitResult::Stopped(_) => Ok(ExecutionResult::stopped()),
            }
        });

        let job = shell.jobs_mut().add_as_current(jobs::Job::new(
            [jobs::JobTask::Internal(join_handle)],
            format!("coproc {name}"),
            jobs::JobState::Running,
        ));
        let job_id = job.id;

        // Fill out the fd variable.
        let arr_value = ShellValue::from(vec![stdout_fd.to_string(), stdin_fd.to_string()]);
        shell
            .env_mut()
            .set_global(name.clone(), ShellVariable::new(arr_value))?;

        // Set the job ID for the coprocess in a separate variable with the _PID suffix.
        let pid_name = format!("{name}_PID");
        shell
            .env_mut()
            .set_global(pid_name, ShellVariable::new(job_id.to_string()))?;

        Ok(ExecutionResult::success())
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::ForClauseCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let mut result = ExecutionResult::success();

        // A loop variable that is not a name fails the loop before it runs, as in bash.
        if !valid_variable_name(&self.variable_name) {
            writeln!(
                params.stderr(shell),
                "{}`{}': not a valid identifier",
                shell.diagnostic_prefix(),
                self.variable_name
            )?;
            return Ok(ExecutionExitCode::GeneralError.into());
        }

        // If we were given explicit words to iterate over, then expand them all, with splitting
        // enabled.
        let expanded_values = if let Some(unexpanded_values) = &self.values {
            expand_words(shell, params, unexpanded_values).await?
        } else {
            // Otherwise, we use the current positional parameters.
            shell.current_shell_args().to_vec()
        };

        let header = if let Some(unexpanded_values) = &self.values {
            std::format!(
                "for {} in {}",
                self.variable_name,
                unexpanded_values.iter().join(" ")
            )
        } else {
            std::format!("for {}", self.variable_name)
        };

        for value in expanded_values {
            // Each iteration runs the DEBUG trap with the header as BASH_COMMAND, as in bash.
            if !shell.running_trap_handler() {
                shell.env_mut().update_or_add(
                    "BASH_COMMAND",
                    ShellValueLiteral::Scalar(header.clone()),
                    |_| Ok(()),
                    EnvironmentLookup::Anywhere,
                    EnvironmentScope::Global,
                )?;
            }
            if shell.traps().handles(traps::TrapSignal::Debug) {
                shell
                    .invoke_trap_handler(traps::TrapSignal::Debug, params)
                    .await?;
            }

            if shell.options().print_commands_and_arguments {
                shell.trace_command(params, header.as_str()).await;
            }

            // Update the variable. A nameref control variable is pointed at each word in turn
            // rather than assigned through, as bash does; a circular one (`local -n v=v`) cannot
            // be followed, so the word is assigned to the global variable, with bash's warning.
            shell.warn_circular_nameref(params, &self.variable_name, 0, true);
            let nameref = shell
                .env()
                .get_raw(&self.variable_name)
                .is_some_and(|(_, var)| var.is_treated_as_nameref())
                && shell.env().circular_nameref(&self.variable_name).is_none();
            if nameref {
                if let Some(var) = shell
                    .env_mut()
                    .get_mut_using_policy_raw(&self.variable_name, EnvironmentLookup::Anywhere)
                {
                    var.assign(ShellValueLiteral::Scalar(value), false)?;
                }
            } else if let Err(error) = shell.env_mut().update_or_add(
                &self.variable_name,
                ShellValueLiteral::Scalar(value),
                |_| Ok(()),
                EnvironmentLookup::Anywhere,
                EnvironmentScope::Global,
            ) {
                // A readonly control variable fails the loop, and the list goes on.
                if !matches!(error.kind(), error::ErrorKind::ReadonlyVariableNamed(_)) {
                    return Err(error);
                }
                let _ = shell.display_error(&mut params.stderr(shell), &error);
                result = ExecutionResult::general_error();
                break;
            }

            shell.loop_depth += 1;
            let body_result = self.body.list.execute(shell, params).await;
            shell.loop_depth -= 1;
            result = body_result?;
            if result.is_return_or_exit() {
                break;
            }

            let is_break = result.is_break();

            result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();

            if is_break || result.is_continue() {
                break;
            }
        }

        shell.set_last_exit_status(result.exit_code.into());
        Ok(result)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::SelectClauseCommand {
    /// As bash runs `select`: the menu and the `PS3` prompt go to standard error, a line read
    /// from standard input sets `REPLY`, and the variable gets the chosen value (empty for a
    /// line that names none). An empty line shows the menu again; the end of input ends the
    /// loop with status 1 after printing a newline.
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        use futures::io::{AsyncReadExt, AsyncWriteExt};

        let values = if let Some(unexpanded_values) = &self.values {
            expand_words(shell, params, unexpanded_values).await?
        } else {
            shell.current_shell_args().to_vec()
        };
        let mut result = ExecutionResult::success();
        if values.is_empty() {
            return Ok(result);
        }
        let columns = shell
            .env_str("COLUMNS")
            .and_then(|columns| columns.parse::<usize>().ok())
            .filter(|columns| *columns > 0)
            .unwrap_or(80);
        let menu = select_menu(&values, columns);
        let mut show_menu = true;
        loop {
            let prompt = shell
                .env_str("PS3")
                .map_or_else(|| "#? ".to_owned(), |prompt| prompt.into_owned());
            let mut stderr = params.stderr(shell);
            if show_menu {
                stderr.async_io().write_all(menu.as_bytes()).await?;
            }
            stderr.async_io().write_all(prompt.as_bytes()).await?;
            stderr.async_io().flush().await?;

            // One line, a byte at a time, so later readers see the rest of the input.
            let mut stdin = params.stdin(shell);
            let mut line = Vec::new();
            let mut ended = true;
            let mut byte = [0];
            while stdin.async_io().read(&mut byte).await? == 1 {
                ended = false;
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            if ended {
                params.stdout(shell).async_io().write_all(b"\n").await?;
                result = ExecutionResult::general_error();
                break;
            }
            let reply = String::from_utf8_lossy(&line).into_owned();
            shell.env_mut().update_or_add(
                "REPLY",
                ShellValueLiteral::Scalar(reply.clone()),
                |_| Ok(()),
                EnvironmentLookup::Anywhere,
                EnvironmentScope::Global,
            )?;
            if reply.is_empty() {
                show_menu = true;
                continue;
            }
            let chosen = reply
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|choice| values.get(choice.checked_sub(1)?))
                .cloned()
                .unwrap_or_default();
            shell.warn_circular_nameref(params, &self.variable_name, 0, true);
            shell.env_mut().update_or_add(
                &self.variable_name,
                ShellValueLiteral::Scalar(chosen),
                |_| Ok(()),
                EnvironmentLookup::Anywhere,
                EnvironmentScope::Global,
            )?;

            shell.loop_depth += 1;
            let body_result = self.body.list.execute(shell, params).await;
            shell.loop_depth -= 1;
            result = body_result?;
            if result.is_return_or_exit() {
                break;
            }
            let is_break = result.is_break();
            result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();
            if is_break || result.is_continue() {
                break;
            }
            show_menu = false;
        }

        shell.set_last_exit_status(result.exit_code.into());
        Ok(result)
    }
}

/// The menu `select` shows, laid out as bash lays it out: numbered entries in as many columns as
/// fit, filled down each column, padded with tabs and spaces.
fn select_menu(values: &[String], columns: usize) -> String {
    let count = values.len();
    let digits = count.to_string().len();
    let widest = values
        .iter()
        .map(|value| value.chars().count())
        .max()
        .unwrap_or(0);
    // Each entry is `N) value`, and columns are two spaces apart.
    let width = widest + digits + 2 + 2;
    let mut cols = (columns / width).max(1);
    let mut rows = count.div_ceil(cols);
    cols = count.div_ceil(rows);
    if rows == 1 {
        rows = cols;
    }
    let first_digits = rows.to_string().len();
    let mut menu = String::new();
    for row in 0..rows {
        let mut index = row;
        let mut position = 0;
        loop {
            let digits = if position == 0 { first_digits } else { digits };
            let entry = std::format!("{:>digits$}) {}", index + 1, values[index]);
            let length = entry.chars().count();
            menu.push_str(&entry);
            index += rows;
            if index >= count {
                break;
            }
            // bash's `indent`: tabs to each tab stop within reach, then spaces.
            let (mut from, to) = (position + length, position + width);
            while from < to {
                if to / 8 > from / 8 {
                    menu.push('\t');
                    from += 8 - from % 8;
                } else {
                    menu.push(' ');
                    from += 1;
                }
            }
            position += width;
        }
        menu.push('\n');
    }
    menu
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::CaseClauseCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // N.B. One would think it makes sense to trace the expanded value being switched
        // on, but that's not it.
        if shell.options().print_commands_and_arguments {
            shell
                .trace_command(params, std::format!("case {} in", self.value))
                .await;
        }

        let expanded_value = expansion::basic_expand_word(shell, params, &self.value).await?;
        let mut result: ExecutionResult = ExecutionResult::success();
        let mut force_execute_next_case = false;

        for case in &self.cases {
            if force_execute_next_case {
                force_execute_next_case = false;
            } else {
                let mut matches = false;
                for pattern in &case.patterns {
                    let expanded_pattern = expansion::basic_expand_pattern(shell, params, pattern)
                        .await?
                        .set_extended_globbing(shell.options().extended_globbing)
                        .set_case_insensitive(shell.options().case_insensitive_conditionals);

                    if expanded_pattern.exactly_matches(expanded_value.as_str())? {
                        matches = true;
                        break;
                    }
                }

                if !matches {
                    continue;
                }
            }

            result = if let Some(case_cmd) = &case.cmd {
                case_cmd.execute(shell, params).await?
            } else {
                ExecutionResult::success()
            };

            // Check for early return (return/exit) or loop control flow (break/continue)
            if !result.is_normal_flow() {
                break;
            }

            match case.post_action {
                ast::CaseItemPostAction::ExitCase => break,
                ast::CaseItemPostAction::UnconditionallyExecuteNextCaseItem => {
                    force_execute_next_case = true;
                }
                ast::CaseItemPostAction::ContinueEvaluatingCases => (),
            }
        }

        shell.set_last_exit_status(result.exit_code.into());

        Ok(result)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::IfClauseCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // Execute condition with errexit suppressed
        let mut condition_params = params.clone();
        condition_params.suppress_errexit = true;
        let condition = self.condition.execute(shell, &condition_params).await?;

        // Check if the condition itself resulted in non-normal control flow.
        if !condition.is_normal_flow() {
            return Ok(condition);
        }

        if condition.is_success() {
            return self.then.execute(shell, params).await;
        }

        if let Some(elses) = &self.elses {
            for else_clause in elses {
                match &else_clause.condition {
                    Some(else_condition) => {
                        let else_condition_result =
                            else_condition.execute(shell, &condition_params).await?;

                        // Check if the elif condition caused non-normal control flow.
                        if !else_condition_result.is_normal_flow() {
                            return Ok(else_condition_result);
                        }

                        if else_condition_result.is_success() {
                            return else_clause.body.execute(shell, params).await;
                        }
                    }
                    None => {
                        return else_clause.body.execute(shell, params).await;
                    }
                }
            }
        }

        // If we got down here, then no branch was taken; we make sure to
        // reset the last exit status to success and then return success.
        let result = ExecutionResult::success();
        shell.set_last_exit_status(result.exit_code.into());

        Ok(result)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for (WhileOrUntil, &ast::WhileOrUntilClauseCommand) {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let is_while = match self.0 {
            WhileOrUntil::While => true,
            WhileOrUntil::Until => false,
        };
        let test_condition = &self.1.0;
        let body = &self.1.1;

        let mut result = ExecutionResult::success();

        // Execute loop condition with errexit suppressed
        let mut condition_params = params.clone();
        condition_params.suppress_errexit = true;

        loop {
            shell.loop_depth += 1;
            let condition_result = test_condition.execute(shell, &condition_params).await;
            shell.loop_depth -= 1;
            let condition_result = condition_result?;

            // Update status for condition
            shell.set_last_exit_status(condition_result.exit_code.into());

            if !condition_result.is_normal_flow() {
                result = condition_result;

                // If the condition has break/continue, the while/until loop itself
                // consumes one level. We need to decrement the level before returning.
                result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();
                break;
            }

            if condition_result.is_success() != is_while {
                break;
            }

            shell.loop_depth += 1;
            let body_result = body.list.execute(shell, params).await;
            shell.loop_depth -= 1;
            result = body_result?;
            if result.is_return_or_exit() {
                break;
            }

            let is_break = result.is_break();

            result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();

            if is_break || result.is_continue() {
                break;
            }
        }

        shell.set_last_exit_status(result.exit_code.into());
        Ok(result)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::ArithmeticCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let value = match self.expr.eval(shell, params, true).await {
            Ok(value) => value,
            Err(error) => return arithmetic_command_error(shell, params, "((", &error),
        };
        let result = if value != 0 {
            ExecutionResult::success()
        } else {
            ExecutionResult::general_error()
        };

        shell.set_last_exit_status(result.exit_code.into());

        Ok(result)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::ArithmeticForClauseCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let mut result = ExecutionResult::success();
        if let Some(initializer) = &self.initializer {
            arithmetic_for_debug_trap(shell, params, initializer).await?;
            if let Err(error) = initializer.eval(shell, params, true).await {
                return arithmetic_command_error(shell, params, "((", &error);
            }
        }

        loop {
            if let Some(condition) = &self.condition {
                arithmetic_for_debug_trap(shell, params, condition).await?;
                // An empty condition (e.g., `for (( ; ; ))`) means "always true".
                if !condition.value.is_empty() {
                    match condition.eval(shell, params, true).await {
                        Ok(0) => break,
                        Ok(_) => (),
                        Err(error) => return arithmetic_command_error(shell, params, "((", &error),
                    }
                }
            }

            shell.loop_depth += 1;
            let body_result = self.body.list.execute(shell, params).await;
            shell.loop_depth -= 1;
            result = body_result?;
            if result.is_return_or_exit() {
                break;
            }

            let is_break = result.is_break();

            result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();

            if is_break || result.is_continue() {
                break;
            }

            if let Some(updater) = &self.updater {
                arithmetic_for_debug_trap(shell, params, updater).await?;
                if let Err(error) = updater.eval(shell, params, true).await {
                    return arithmetic_command_error(shell, params, "((", &error);
                }
            }
        }

        shell.set_last_exit_status(result.exit_code.into());
        Ok(result)
    }
}

/// Before an arithmetic `for` evaluates one of its clauses, the clause becomes `BASH_COMMAND` as
/// bash prints it (`((i=0 ))`) and the DEBUG trap runs, as in bash.
async fn arithmetic_for_debug_trap(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    clause: &ast::UnexpandedArithmeticExpr,
) -> Result<(), error::Error> {
    if !shell.running_trap_handler() {
        shell.env_mut().update_or_add(
            "BASH_COMMAND",
            ShellValueLiteral::Scalar(format!("(({}))", clause.value.trim_start())),
            |_| Ok(()),
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        )?;
    }
    if shell.traps().handles(traps::TrapSignal::Debug) {
        shell
            .invoke_trap_handler(traps::TrapSignal::Debug, params)
            .await?;
    }
    Ok(())
}

/// An arithmetic error in `(( ))`, `for (( ))` or `[[ ]]` fails that command with status 1,
/// reported as bash words it (`((: EXPR: message`), and the list goes on. An unset variable under
/// `set -u` ends the shell instead, as it does anywhere else.
fn arithmetic_command_error(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    command: &str,
    error: &arithmetic::EvalError,
) -> Result<ExecutionResult, error::Error> {
    if let Some(name) = error.unset_variable() {
        return Err(
            error::Error::from(error::ErrorKind::ExpandingUnsetVariable(name.to_owned()))
                .into_fatal(),
        );
    }
    // An error in an array subscript ends the shell, reported without the command.
    if error.is_in_subscript() {
        return Err(error::Error::from(error.clone()));
    }
    // A readonly variable is named on its own, without the command.
    let message = if error.is_readonly_variable() {
        error.to_string()
    } else {
        format!("{command}: {error}")
    };
    writeln!(
        params.stderr(shell),
        "{}{message}",
        shell.diagnostic_prefix()
    )?;
    let result = ExecutionResult::general_error();
    shell.set_last_exit_status(result.exit_code.into());
    Ok(result)
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Execute for ast::FunctionDefinition {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let func_name = self.fname.value.clone();

        // A readonly function (`readonly -f`) keeps its definition.
        if shell
            .funcs()
            .get(&func_name)
            .is_some_and(|f| f.is_readonly())
        {
            writeln!(
                params.stderr(shell),
                "{}{func_name}: readonly function",
                shell.diagnostic_prefix()
            )?;
            let result = ExecutionResult::general_error();
            shell.set_last_exit_status(result.exit_code.into());
            return Ok(result);
        }

        // In POSIX mode, function names can't shadow special builtins.
        if shell.options().posix_mode
            && shell
                .builtins()
                .get(&func_name)
                .is_some_and(|r| r.special_builtin)
        {
            return Err(
                error::Error::from(error::ErrorKind::FunctionNameShadowsSpecialBuiltin {
                    name: func_name,
                })
                .into_fatal(),
            );
        }

        // The function definition's source context should be the same as the current frame
        // so we directly pass that through.
        let source_info = shell
            .call_stack()
            .current_frame()
            .map_or_else(crate::SourceInfo::default, |frame| {
                frame.adjusted_source_info()
            });
        let aliases = shell.aliases_for_definition();
        shell.define_func(func_name.clone(), self.clone(), &source_info);
        if let Some(registration) = shell.func_mut(&func_name) {
            registration.set_aliases(aliases);
        }

        let result = ExecutionResult::success();
        shell.set_last_exit_status(result.exit_code.into());

        Ok(result)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[allow(clippy::too_many_lines)]
impl<SE: extensions::ShellExtensions> ExecuteInPipeline<SE> for ast::SimpleCommand {
    async fn execute_in_pipeline(
        &self,
        mut context: PipelineExecutionContext<'_, SE>,
        mut params: ExecutionParameters,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        let prefix_iter = self.prefix.as_ref().map(|s| s.0.iter()).unwrap_or_default();
        let suffix_iter = self.suffix.as_ref().map(|s| s.0.iter()).unwrap_or_default();
        let cmd_name_items = self
            .word_or_name
            .as_ref()
            .map(|won| CommandPrefixOrSuffixItem::Word(won.clone()));

        // `>(list)` substitutions among the arguments and redirects run once the command has
        // finished.
        #[cfg(target_arch = "wasm32")]
        let pending = params.own_output_substitutions();
        params.word_process_substitutions = WordProcessSubstitutions::default();

        // A pipeline stage's DEBUG trap already ran in the shell that started the stage.
        if !std::mem::take(&mut context.shell.debug_trap_ran) {
            before_simple_command(&mut context.shell, &params, self).await?;
        }
        let record_last_arg = !std::mem::take(&mut context.shell.stage_command);

        let mut assignments = vec![];
        let mut args: Vec<CommandArg> = vec![];
        let mut command_takes_assignments = false;
        let mut redirects = vec![];
        let mut alias_follows = false;

        // `set -x` traces a simple command to the standard error it had before its own
        // redirections, as bash does.
        let trace_params = context
            .shell
            .options()
            .print_commands_and_arguments
            .then(|| params.clone());

        // Capture the status change count before expansion, so we can detect
        // if expansion (e.g., command substitution) set an exit status.
        let status_change_count_before_expansion = context.shell.last_exit_status_change_count();

        for item in prefix_iter.chain(cmd_name_items.iter()).chain(suffix_iter) {
            params.install_word_process_substitutions();
            match item {
                // Bash expands the command's words first, then its assignments, and makes its
                // redirections last.
                CommandPrefixOrSuffixItem::IoRedirect(redirect) => redirects.push(redirect),
                CommandPrefixOrSuffixItem::ProcessSubstitution(kind, subshell_command) => {
                    let (installed_fd_num, substitution_file) =
                        setup_process_substitution(&context.shell, &params, kind, subshell_command)
                            .await?;

                    params
                        .open_files
                        .set_fd(installed_fd_num, substitution_file);

                    args.push(CommandArg::String(std::format!(
                        "/dev/fd/{installed_fd_num}"
                    )));
                }
                CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) => {
                    // With `set -k`, an assignment anywhere among the words is one for the
                    // command's environment, as in bash.
                    if args.is_empty()
                        || (!command_takes_assignments
                            && context
                                .shell
                                .options()
                                .place_all_assignment_args_in_command_env)
                    {
                        // If we haven't yet seen any arguments, then this must be a proper
                        // scoped assignment. Add it to the list we're accumulating.
                        assignments.push(assignment);
                    } else {
                        if command_takes_assignments {
                            // This looks like an assignment, and the command being invoked is a
                            // well-known builtin that takes arguments that need to function like
                            // assignments (but which are processed by the builtin).
                            check_declared_assoc_elements(&context.shell, &args, assignment)?;
                            let expanded =
                                expand_assignment(&mut context.shell, &params, assignment).await?;
                            // Bash traces a compound array assignment to a declaration as it
                            // expands it, with every element quoted: `a=('1' '2')`.
                            if let Some(trace_params) = &trace_params
                                && let ast::AssignmentValue::Array(elements) = &expanded.value
                            {
                                let op = if expanded.append { "+=" } else { "=" };
                                let text = format!(
                                    "{}{op}({})",
                                    expanded.name,
                                    elements
                                        .iter()
                                        .map(|(key, value)| match key {
                                            Some(key) => format!(
                                                "[{}]={}",
                                                single_quoted(&key.value),
                                                single_quoted(&value.value)
                                            ),
                                            None => single_quoted(&value.value),
                                        })
                                        .join(" ")
                                );
                                context.shell.trace_command(trace_params, text).await;
                            }
                            args.push(CommandArg::Assignment(expanded));
                        } else {
                            // This *looks* like an assignment, but it's really a string we should
                            // fully treat as a regular looking
                            // argument.
                            let mut next_args = expansion::full_expand_and_split_word(
                                &mut context.shell,
                                &params,
                                word,
                            )
                            .await?
                            .into_iter()
                            .map(CommandArg::String)
                            .collect();
                            args.append(&mut next_args);
                        }
                    }
                }
                CommandPrefixOrSuffixItem::Word(arg) => {
                    // The command word, and a word after an alias ending in a blank, may be
                    // aliases (when `expand_aliases` is on, as it is in interactive shells).
                    //
                    // TODO(#57): aliases are supposed to be expanded as the command is read, not
                    // as it runs; this handles bodies that amount to a sequence of words.
                    let alias_position = args.is_empty() || alias_follows;
                    alias_follows = false;
                    let next_args = if alias_position
                        && let Some((fields, trailing_blank)) =
                            expand_alias(&mut context.shell, &params, &arg.value).await?
                    {
                        alias_follows = trailing_blank;
                        fields
                    } else {
                        expansion::full_expand_and_split_word(&mut context.shell, &params, arg)
                            .await?
                    };

                    if args.is_empty() {
                        // Check if we're going to be invoking a special declaration builtin.
                        // That will change how we parse and process args. (An alias with an
                        // empty body leaves us with no words at all.)
                        if let Some(first_arg) = next_args.first()
                            && context
                                .shell
                                .builtins()
                                .get(first_arg.as_str())
                                .is_some_and(|r| !r.disabled && r.declaration_builtin)
                        {
                            command_takes_assignments = true;
                        }
                    }

                    let mut next_args = next_args.into_iter().map(CommandArg::String).collect();
                    args.append(&mut next_args);
                }
            }
        }

        params.install_word_process_substitutions();

        // If we have a command, then execute it.
        if let Some(CommandArg::String(cmd_name)) = args.first() {
            let cmd_name = cmd_name.clone();
            let mut stderr = params.stderr(&context.shell);

            let (owned_shell, parent_shell) = match context.shell {
                commands::ShellForCommand::ParentShell(shell) => (None, shell),
                commands::ShellForCommand::OwnedShell { target, parent } => (Some(target), parent),
            };

            let shell = if let Some(owned_shell) = owned_shell {
                commands::ShellForCommand::OwnedShell {
                    target: owned_shell,
                    parent: parent_shell,
                }
            } else {
                commands::ShellForCommand::ParentShell(parent_shell)
            };

            let context = PipelineExecutionContext {
                shell,
                process_group_id: context.process_group_id,
            };

            let no_fork = context.shell.no_fork;
            let result = execute_command(
                context,
                params,
                trace_params.as_ref(),
                cmd_name,
                &assignments,
                args,
                &redirects,
                &mut stderr,
                no_fork.names(self).then_some(no_fork),
                record_last_arg,
            )
            .await;
            #[cfg(target_arch = "wasm32")]
            let result = match result {
                Ok(spawned) if !pending.is_empty() => {
                    // The command must finish writing before the substitutions read it.
                    let completed = ExecutionResult::from(spawned.wait().await?);
                    start_persistent_output_substitutions(parent_shell, &pending);
                    run_pending_output_substitutions(parent_shell, &pending).await;
                    Ok(completed.into())
                }
                other => other,
            };
            match result {
                Ok(result) => Ok(result),
                Err(err) if err.abandons_command() => Err(err),
                Err(err) => {
                    let _ = parent_shell.display_error(&mut stderr, &err);

                    let result = err.into_result(parent_shell);
                    Ok(result.into())
                }
            }
        } else {
            // No command to run; assignments must be applied to this shell.
            for assignment in assignments {
                // Apply the assignment. Don't mark as fatal - let errors be handled
                // at the program level so multiple complete_commands can execute independently.
                apply_assignment(
                    assignment,
                    &mut context.shell,
                    &params,
                    trace_params.as_ref().unwrap_or(&params),
                    false,
                    None,
                    EnvironmentScope::Global,
                )
                .await?;
            }

            // Assignment-only statements clear $_ (set to empty string).
            // This matches bash behavior where assignments don't have a "last
            // argument".
            if record_last_arg {
                context.shell.update_last_arg_variable(None);
            }

            // Then the redirections; one that fails fails the statement, but the assignments
            // still took effect, as in bash.
            let redirect_failed =
                !setup_command_redirects(&mut context.shell, &mut params, &redirects, &args)
                    .await?;
            if redirect_failed {
                context.shell.set_last_exit_status(1);
                return Ok(ExecutionResult::general_error().into());
            }

            // We need to set the last exit status to indicate assignment success,
            // but only if there was no status set during expansion. We use the
            // status count captured before expansion to detect if command
            // substitution (or other expansion) set an exit status.
            if status_change_count_before_expansion == context.shell.last_exit_status_change_count()
            {
                context.shell.set_last_exit_status(0);
            }

            // Return the last exit status we have; in some cases, an expansion
            // might result in a non-zero exit status stored in the shell.
            Ok(ExecutionResult::new(context.shell.last_exit_status()).into())
        }
    }
}

/// Sets up a simple command's redirections, which bash makes after it expands the command's
/// words and assignments. Returns whether they all succeeded: one that fails is reported and
/// fails the command, unless it ends the shell or abandons the top-level command.
async fn setup_command_redirects(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &mut ExecutionParameters,
    redirects: &[&ast::IoRedirect],
    args: &[CommandArg],
) -> Result<bool, error::Error> {
    for redirect in redirects {
        params.install_word_process_substitutions();
        let here = matches!(
            redirect,
            ast::IoRedirect::HereDocument(..) | ast::IoRedirect::HereString(..)
        );
        // Bash expands a here-document or here-string given to a program it runs in a child
        // process there, so the expansion's side effects are lost.
        let result = if here && runs_as_process(shell, args) {
            let mut child = Shell::clone(shell);
            setup_redirect(&mut child, params, redirect).await
        } else {
            setup_redirect(shell, params, redirect).await
        };
        if let Err(error) = result {
            // An expansion error that ends the shell (a bad substitution in a file name) or
            // abandons the top-level command (failglob) still does, as in bash; in a
            // here-document or here-string, or any other failed redirection, it fails the
            // command.
            if (error.is_fatal() || error.abandons_command()) && !here {
                return Err(error);
            }
            let _ = shell.display_error(&mut params.stderr(shell), &error);
            // A special builtin's failed redirection ends a non-interactive POSIX-mode shell.
            if shell.options().posix_mode
                && !shell.options().interactive
                && runs_special_builtin(shell, args)
            {
                return Err(error.into_reported().into_fatal());
            }
            return Ok(false);
        }
    }
    params.install_word_process_substitutions();
    Ok(true)
}

/// The input lines of multi-line `$( )`s, as `set -v` echoes them.
#[derive(Default)]
struct SubstitutionLines {
    /// The lines after the first of each: bash reads them while it parses the substitution, and
    /// echoes none of them.
    inside: std::collections::HashSet<usize>,
    /// The first and last lines of the here-documents among them (the body and the delimiter),
    /// which bash echoes as it reads them, and how many times: once, or twice for a substitution
    /// in double quotes, which bash parses twice.
    here_documents: Vec<(usize, usize, usize)>,
}

/// The input lines of multi-line `$( )`s (see [`SubstitutionLines`]).
fn lines_inside_substitutions(
    input: &str,
    options: &brush_parser::ParserOptions,
) -> SubstitutionLines {
    fn substitutions(
        pieces: &[brush_parser::word::WordPieceWithSource],
        quoted: bool,
        found: &mut Vec<(usize, usize, bool)>,
    ) {
        for piece in pieces {
            match &piece.piece {
                brush_parser::word::WordPiece::CommandSubstitution(_) => {
                    found.push((piece.start_index, piece.end_index, quoted));
                }
                brush_parser::word::WordPiece::DoubleQuotedSequence(inner)
                | brush_parser::word::WordPiece::GettextDoubleQuotedSequence(inner) => {
                    substitutions(inner, true, found);
                }
                _ => (),
            }
        }
    }

    let mut lines = SubstitutionLines::default();
    let Ok(tokens) = brush_parser::tokenize_str(input) else {
        return lines;
    };
    for token in tokens {
        let brush_parser::Token::Word(text, span) = token else {
            continue;
        };
        if !text.contains('\n') {
            continue;
        }
        let Ok(pieces) = brush_parser::word::parse(&text, options) else {
            continue;
        };
        let mut found = vec![];
        substitutions(&pieces, false, &mut found);
        for (start, end, quoted) in found {
            let (Some(before), Some(inside)) = (text.get(..start), text.get(start..end)) else {
                continue;
            };
            let first_line = span.start.line + before.matches('\n').count();
            lines
                .inside
                .extend(first_line + 1..=first_line + inside.matches('\n').count());
            // Line 1 of the substitution's command is `first_line`.
            let command = inside
                .strip_prefix("$(")
                .and_then(|command| command.strip_suffix(')'))
                .unwrap_or_default();
            let times = if quoted { 2 } else { 1 };
            lines.here_documents.extend(
                here_document_lines(command)
                    .into_iter()
                    .map(|(first, last)| (first_line + first - 1, first_line + last - 1, times)),
            );
        }
    }
    lines
}

/// The first and last lines of each here-document's body and delimiter in `command`, counted
/// from its first line: the tokenizer gives a here-document's operator and tag, then its body,
/// whose lines are followed by the delimiter's.
pub(crate) fn here_document_lines(command: &str) -> Vec<(usize, usize)> {
    let Ok(tokens) = brush_parser::tokenize_str(command) else {
        return vec![];
    };
    let mut found = vec![];
    let mut tokens = tokens.iter();
    while let Some(token) = tokens.next() {
        if !matches!(token, brush_parser::Token::Operator(op, _) if op == "<<" || op == "<<-") {
            continue;
        }
        let (Some(_tag), Some(body)) = (tokens.next(), tokens.next()) else {
            break;
        };
        let first = body.location().start.line;
        found.push((first, first + body.to_str().matches('\n').count()));
    }
    found
}

/// What happens before a simple command's words are expanded: its text becomes `BASH_COMMAND`
/// (unless a trap handler is running, whose commands leave it alone) and the DEBUG trap runs, as
/// in bash.
async fn before_simple_command(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    command: &ast::SimpleCommand,
) -> Result<(), error::Error> {
    if !shell.running_trap_handler() {
        shell.env_mut().update_or_add(
            "BASH_COMMAND",
            ShellValueLiteral::Scalar(command.to_string()),
            |_| Ok(()),
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        )?;
    }
    if shell.traps().handles(traps::TrapSignal::Debug) {
        let _ = shell
            .invoke_trap_handler(traps::TrapSignal::Debug, params)
            .await?;
    }
    Ok(())
}

/// Runs a simple command's DEBUG trap in this shell before the pipeline stage that runs it
/// starts, as bash does before it forks the stage, and returns whether it did.
async fn stage_debug_trap(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    command: &ast::Command,
) -> Result<bool, error::Error> {
    match command {
        ast::Command::Simple(simple) => {
            before_simple_command(shell, params, simple).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// A word in single quotes, as bash's xtrace quotes an array element: `'it'\''s'`.
fn single_quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Whether the command `args` names is a special builtin (not a function of that name).
fn runs_special_builtin(
    shell: &Shell<impl extensions::ShellExtensions>,
    args: &[CommandArg],
) -> bool {
    let Some(CommandArg::String(name)) = args.first() else {
        return false;
    };
    shell.funcs().get(name).is_none()
        && shell
            .builtins()
            .get(name)
            .is_some_and(|builtin| !builtin.disabled && builtin.special_builtin)
}

/// Runs the command `args` names as bash runs a command without forking: a program replaces the
/// shell, as `exec` does, and sees `SHLVL` one lower; a function called from a substitution or a
/// background command runs its own last command this way. Not in a pipeline stage (bash's
/// substitution there is a process of its own), nor while a trap needs the shell afterwards.
fn exec_in_place(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    no_fork: NoFork,
    args: &[CommandArg],
    params: &ExecutionParameters,
) {
    if shell.stage_subshell || (no_fork.checked && !traps_allow_exec(shell)) {
        return;
    }
    let mut words = args.iter().map_while(|arg| match arg {
        CommandArg::String(word) => Some(word.as_str()),
        CommandArg::Assignment(_) => None,
    });
    let Some(mut name) = words.next() else {
        return;
    };
    // `command NAME` runs the program in place too; it skips functions.
    let mut function_allowed = true;
    if name == "command" && shell.funcs().get(name).is_none() {
        function_allowed = false;
        match words.find(|word| !matches!(*word, "-p" | "--")) {
            Some(word) if !word.starts_with('-') => name = word,
            _ => return,
        }
    }
    if function_allowed && shell.funcs().get(name).is_some() {
        shell.no_fork_call = no_fork.into_function;
    } else if shell.builtins().get(name).is_some_and(|builtin| {
        !builtin.disabled
            && builtin.execution_boundary == crate::builtins::ExecutionBoundary::Command
    }) {
        // Bash sees the command's own `SHLVL=` assignment only in a subshell.
        let in_subshell = shell.depth() > shell.process_depth;
        shell.adjust_shell_level(-1, params, in_subshell);
    }
}

/// Whether bash would run the command `args` names as a program in a child process: not a
/// function, and not a builtin other than a utility that stands for a program.
fn runs_as_process(shell: &Shell<impl extensions::ShellExtensions>, args: &[CommandArg]) -> bool {
    let Some(CommandArg::String(name)) = args.first() else {
        return false;
    };
    if shell.funcs().get(name).is_some() {
        return false;
    }
    match shell.builtins().get(name) {
        Some(registration) if !registration.disabled => {
            registration.execution_boundary == crate::builtins::ExecutionBoundary::Command
        }
        _ => true,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the command's expanded parts and where its errors go, as the caller holds them"
)]
async fn execute_command<T: Into<String>>(
    mut context: PipelineExecutionContext<'_, impl extensions::ShellExtensions>,
    mut params: ExecutionParameters,
    trace_params: Option<&ExecutionParameters>,
    cmd_name: T,
    assignments: &[&ast::Assignment],
    args: Vec<CommandArg>,
    redirects: &[&ast::IoRedirect],
    stderr: &mut OpenFile,
    no_fork: Option<NoFork>,
    record_last_arg: bool,
) -> Result<ExecutionSpawnResult, error::Error> {
    // Push a new ephemeral environment scope for the duration of the command. We'll
    // set command-scoped variable assignments after doing so, and revert them before
    // returning.
    let mut guard = crate::env::ScopeGuard::new(&mut context.shell, EnvironmentScope::Command);

    for assignment in assignments {
        // Ensure it's tagged as exported and created in the command scope.
        match apply_assignment(
            assignment,
            guard.shell(),
            &params,
            trace_params.unwrap_or(&params),
            true,
            Some(EnvironmentScope::Command),
            EnvironmentScope::Command,
        )
        .await
        {
            // A readonly variable keeps its value: bash reports it and still runs the command,
            // unless it is a special builtin in a non-interactive POSIX-mode shell, which ends.
            Err(error)
                if error.abandons_command()
                    && matches!(error.kind(), error::ErrorKind::ReadonlyVariableNamed(_)) =>
            {
                let shell = guard.shell();
                if shell.options().posix_mode
                    && !shell.options().interactive
                    && runs_special_builtin(shell, &args)
                {
                    return Err(error.into_fatal());
                }
            }
            result => result?,
        }
    }

    if guard.shell().options().print_commands_and_arguments {
        let trace_params = trace_params.unwrap_or(&params);
        guard
            .shell()
            .trace_command(
                trace_params,
                args.iter().map(|arg| arg.quote_for_tracing()).join(" "),
            )
            .await;
        // `export` and `readonly` make their scalar assignments as ordinary assignments, which
        // bash traces as it makes them: `+ a=1`.
        let name = match args.first() {
            Some(CommandArg::String(name)) => name.as_str(),
            _ => "",
        };
        if matches!(name, "export" | "readonly")
            && guard.shell().funcs().get(name).is_none()
            && guard
                .shell()
                .builtins()
                .get(name)
                .is_some_and(|builtin| !builtin.disabled)
        {
            for arg in args.iter().skip(1) {
                if let CommandArg::Assignment(assignment) = arg
                    && let ast::AssignmentValue::Scalar(value) = &assignment.value
                {
                    let op = if assignment.append { "+=" } else { "=" };
                    let value = if value.value.is_empty() {
                        Cow::Borrowed("")
                    } else {
                        crate::escape::quote_if_needed(
                            &value.value,
                            crate::escape::QuoteMode::SingleQuote,
                        )
                    };
                    guard
                        .shell()
                        .trace_command(trace_params, format!("{}{op}{value}", assignment.name))
                        .await;
                }
            }
        }
    }

    // The redirections come last, after the words and assignments, and without the command's
    // own variables, as in bash.
    let scope = guard
        .shell()
        .env_mut()
        .take_scope(EnvironmentScope::Command)?;
    let redirected = setup_command_redirects(guard.shell(), &mut params, redirects, &args).await;
    guard
        .shell()
        .env_mut()
        .restore_scope(EnvironmentScope::Command, scope);
    if !redirected? {
        return Ok(ExecutionResult::general_error().into());
    }
    // An error the command fails with is reported where its standard error now goes.
    *stderr = params.stderr(guard.shell());

    guard.detach();
    drop(guard);

    if let Some(no_fork) = no_fork {
        exec_in_place(&mut context.shell, no_fork, &args, &params);
    }

    // Construct the command struct.
    let mut cmd = commands::SimpleCommand::new(context.shell, params, cmd_name.into(), args);
    cmd.process_group_id = context.process_group_id;
    cmd.record_last_arg = record_last_arg;

    // Arrange to pop off that ephemeral environment scope.
    cmd.post_execute = Some(|shell| shell.env_mut().pop_scope(EnvironmentScope::Command));

    // Execute
    // TODO(jobs): do we need to move self back to foreground on error here?
    cmd.execute().await
}

/// Tokenizes the body of an alias into the unexpanded words that should replace the aliased command
/// name.
///
/// This only handles alias bodies that amount to a simple sequence of words; bodies containing
/// operators (pipes, redirections, `&&`, ...) and recursive expansion of chained aliases require
/// alias substitution to happen in the tokenizer instead (see issue #57).
fn tokenize_alias_body(
    shell: &Shell<impl extensions::ShellExtensions>,
    alias_value: &str,
) -> Vec<String> {
    // Tokenize the body the same way the shell would tokenize any other input it's given.
    let options = shell.parser_options().tokenizer_options();

    // If we can't tokenize the body, fall back to naively splitting it on whitespace.
    brush_parser::tokenize_str_with_options(alias_value, &options).map_or_else(
        |_| {
            alias_value
                .split_ascii_whitespace()
                .map(str::to_owned)
                .collect()
        },
        |tokens| tokens.iter().map(|t| t.to_str().to_owned()).collect(),
    )
}

/// Expands `word` as the alias it names, as bash does: an alias whose body starts with another
/// alias expands that one too, though never one already expanded here. Returns the fields and
/// whether the last alias's body ends in a blank (so the next word may be an alias too), or `None`
/// when `word` names no alias in effect.
async fn expand_alias(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    word: &str,
) -> Result<Option<(Vec<String>, bool)>, error::Error> {
    let mut words: VecDeque<String> = VecDeque::from([word.to_owned()]);
    let mut expanded: Vec<String> = vec![];
    let mut trailing_blank = false;
    while let Some(first) = words.front()
        && !expanded.contains(first)
        && let Some(value) = shell.alias_for_expansion(first)
    {
        trailing_blank = value.ends_with([' ', '\t']);
        let body = tokenize_alias_body(shell, value);
        expanded.push(words.pop_front().unwrap_or_default());
        for body_word in body.into_iter().rev() {
            words.push_front(body_word);
        }
    }
    if expanded.is_empty() {
        return Ok(None);
    }
    Ok(Some((
        expand_words(shell, params, words).await?,
        trailing_blank,
    )))
}

/// Expands the given words, with splitting enabled, yielding the fields they expand to.
async fn expand_words(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    words: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<Vec<String>, error::Error> {
    // N.B. Expansion needs `&mut shell`, so the words have to be expanded in sequence.
    let mut fields = vec![];
    for word in words {
        fields.extend(expansion::full_expand_and_split_word(shell, params, word).await?);
    }
    Ok(fields)
}

async fn expand_assignment(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    assignment: &ast::Assignment,
) -> Result<ast::Assignment, error::Error> {
    let value = expand_assignment_value(shell, params, &assignment.value).await?;
    Ok(ast::Assignment {
        name: basic_expand_assignment_name(shell, params, &assignment.name).await?,
        value,
        append: assignment.append,
        loc: assignment.loc.clone(),
    })
}

async fn basic_expand_assignment_name(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    name: &ast::AssignmentName,
) -> Result<ast::AssignmentName, error::Error> {
    match name {
        ast::AssignmentName::VariableName(name) => {
            let expanded = expansion::basic_expand_word(shell, params, name).await?;
            Ok(ast::AssignmentName::VariableName(expanded))
        }
        ast::AssignmentName::ArrayElementName(name, index) => {
            let expanded_name = expansion::basic_expand_word(shell, params, name).await?;
            let expanded_index = expansion::basic_expand_word(shell, params, index).await?;
            Ok(ast::AssignmentName::ArrayElementName(
                expanded_name,
                expanded_index,
            ))
        }
    }
}

async fn expand_assignment_value(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    value: &ast::AssignmentValue,
) -> Result<ast::AssignmentValue, error::Error> {
    let expanded = match value {
        ast::AssignmentValue::Scalar(s) => {
            let expanded_word = expansion::basic_expand_assignment_word(shell, params, s).await?;
            ast::AssignmentValue::Scalar(ast::Word::from(expanded_word))
        }
        ast::AssignmentValue::Array(arr) => {
            let mut expanded_values = vec![];
            for (key, value) in arr {
                if let Some(k) = key {
                    let expanded_key = expansion::basic_expand_assignment_word(shell, params, k)
                        .await?
                        .into();
                    let expanded_value =
                        expansion::basic_expand_assignment_word(shell, params, value)
                            .await?
                            .into();
                    expanded_values.push((Some(expanded_key), expanded_value));
                } else {
                    // Array elements are treated as regular words, not assignments
                    let split_expanded_value =
                        expansion::full_expand_and_split_array_element(shell, params, value)
                            .await?;
                    for expanded_value in split_expanded_value {
                        expanded_values.push((None, expanded_value.into()));
                    }
                }
            }

            ast::AssignmentValue::Array(expanded_values)
        }
    };

    Ok(expanded)
}

async fn apply_assignment(
    assignment: &ast::Assignment,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    trace_params: &ExecutionParameters,
    export: bool,
    required_scope: Option<EnvironmentScope>,
    creation_scope: EnvironmentScope,
) -> Result<(), error::Error> {
    // Assigning to a readonly variable abandons the rest of the top-level command, as in bash:
    // the error propagates to the program, which reports it and carries on with the next one.
    apply_assignment_unchecked(
        assignment,
        shell,
        params,
        trace_params,
        export,
        required_scope,
        creation_scope,
    )
    .await
    .map_err(|error| match error.kind() {
        // Reported here, where LINENO names the assignment, even inside a function.
        error::ErrorKind::ReadonlyVariable => {
            let error = error::Error::from(error::ErrorKind::ReadonlyVariableNamed(
                assignment.name.base_name().to_owned(),
            ));
            let _ = shell.display_error(&mut params.stderr(shell), &error);
            error.into_reported()
        }
        // Bash names the element as written: `a[-(1)]: bad array subscript`. (An error in the
        // value's expansion is fatal, and names what it expanded.)
        error::ErrorKind::ArrayIndexOutOfRange(evaluated) if !error.is_fatal() => {
            let name = match &assignment.name {
                ast::AssignmentName::ArrayElementName(name, index) => format!("{name}[{index}]"),
                ast::AssignmentName::VariableName(name) => format!("{name}[{evaluated}]"),
            };
            error::ErrorKind::ArrayIndexOutOfRange(name).into()
        }
        _ => error,
    })
}

#[expect(clippy::too_many_lines)]
async fn apply_assignment_unchecked(
    assignment: &ast::Assignment,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    trace_params: &ExecutionParameters,
    mut export: bool,
    required_scope: Option<EnvironmentScope>,
    creation_scope: EnvironmentScope,
) -> Result<(), error::Error> {
    // Figure out if we are trying to assign to a variable or assign to an element of an existing
    // array.
    let mut array_index;
    let variable_name = match &assignment.name {
        ast::AssignmentName::VariableName(name) => {
            array_index = None;
            name
        }
        ast::AssignmentName::ArrayElementName(name, index) => {
            let expanded = expansion::basic_expand_word(shell, params, index).await?;
            array_index = Some(expanded);
            name
        }
    };
    // Assigning through a nameref that comes back to itself from a function's local
    // (`local -n v=v`) assigns the global variable it closes on, as bash does after following
    // the reference as far as it goes.
    let circular = shell.env().circular_nameref(variable_name);
    let global_only = if let Some((closing, true)) = &circular {
        // Bash's lookups on the way say so as they go: once for a list, twice for an element.
        let prefix = shell.diagnostic_prefix();
        let warning = match (&array_index, &assignment.value) {
            (Some(_), _) => format!(
                "{prefix}warning: {variable_name}: circular name reference\n\
                 {prefix}warning: {variable_name}: circular name reference"
            ),
            (None, ast::AssignmentValue::Array(_)) => {
                format!("{prefix}warning: {variable_name}: circular name reference")
            }
            (None, ast::AssignmentValue::Scalar(_)) => {
                format!("{prefix}warning: {variable_name}: maximum nameref depth (8) exceeded")
            }
        };
        writeln!(params.stderr(shell), "{warning}")?;
        Some(closing.clone())
    } else {
        None
    };
    // Assigning through any other circular nameref fails, as a readonly variable does in bash.
    if global_only.is_none()
        && (circular.is_some() || shell.env().is_circular_nameref(variable_name))
    {
        writeln!(
            params.stderr(shell),
            "{}warning: {variable_name}: circular name reference",
            shell.diagnostic_prefix()
        )?;
        return Err(error::Error::from(error::ErrorKind::ReadonlyVariableNamed(
            variable_name.clone(),
        ))
        .into_reported());
    }

    // Assigning through a nameref assigns to the variable it names, and a nameref to an array
    // element (`declare -n ref='arr[1]'`) to that element, just as `arr[1]=value` would.
    let mut resolved_name = match &global_only {
        Some(closing) => closing.clone(),
        None => shell
            .env()
            .resolve_nameref(variable_name.as_str())
            .into_owned(),
    };
    if array_index.is_none()
        && global_only.is_none()
        && let Some((array, index)) = shell.env().resolve_nameref_element(variable_name.as_str())
    {
        resolved_name = array;
        array_index = Some(index);
    }
    let variable_name = &resolved_name;

    let associative = shell.env().get(variable_name).is_some_and(|(_, var)| {
        matches!(
            var.value(),
            ShellValue::AssociativeArray(_)
                | ShellValue::Unset(ShellValueUnsetType::AssociativeArray)
        )
    });
    if let ast::AssignmentValue::Array(elements) = &assignment.value {
        // Bash traces a compound assignment as written, before expanding its elements; one
        // written before a command is a string, traced as its value is.
        if shell.options().print_commands_and_arguments
            && creation_scope != EnvironmentScope::Command
        {
            let op = if assignment.append { "+=" } else { "=" };
            let written = elements
                .iter()
                .map(|(key, value)| match key {
                    Some(key) => std::format!("[{}]={}", key.value, value.value),
                    None => value.value.clone(),
                })
                .join(" ");
            shell
                .trace_command(
                    trace_params,
                    std::format!("{}{op}({written})", assignment.name),
                )
                .await;
        }
        // Once an associative array's first element has a subscript, every element needs one.
        if associative && elements.first().is_some_and(|(key, _)| key.is_some()) {
            if let Some((_, word)) = elements.iter().find(|(key, _)| key.is_none()) {
                let kind = error::ErrorKind::AssocSubscriptRequired(
                    variable_name.clone(),
                    word.value.clone(),
                );
                return Err(error::Error::from(kind).into_fatal());
            }
        }
    }

    // An element of an array literal whose subscript is empty or counts back past the start
    // fails the assignment once the elements before it are assigned, as in bash.
    let mut failed_element = None;

    // Expand the values.
    let new_value = match &assignment.value {
        ast::AssignmentValue::Scalar(unexpanded_value) => {
            let value =
                expansion::basic_expand_assignment_word(shell, params, unexpanded_value).await?;
            ShellValueLiteral::Scalar(value)
        }
        // A command's own variable is a string, as in bash: an array written before a command
        // is the text of its elements, `(1 2)`, expanded as one assignment word.
        ast::AssignmentValue::Array(unexpanded_values)
            if creation_scope == EnvironmentScope::Command =>
        {
            let text = unexpanded_values
                .iter()
                .map(|(key, value)| match key {
                    Some(key) => format!("[{}]={}", key.value, value.value),
                    None => value.value.clone(),
                })
                .join(" ");
            let word = ast::Word::from(format!("({text})"));
            ShellValueLiteral::Scalar(
                expansion::basic_expand_assignment_word(shell, params, &word).await?,
            )
        }
        ast::AssignmentValue::Array(unexpanded_values) => {
            let mut elements = vec![];
            for (unexpanded_key, unexpanded_value) in unexpanded_values {
                let key = match unexpanded_key {
                    Some(unexpanded_key) => Some(
                        expansion::basic_expand_assignment_word(shell, params, unexpanded_key)
                            .await?,
                    ),
                    None => None,
                };

                if key.is_some() {
                    let value =
                        expansion::basic_expand_assignment_word(shell, params, unexpanded_value)
                            .await?;
                    elements.push((key, value));
                } else {
                    // Array elements are treated as regular words, not assignments
                    let values = expansion::full_expand_and_split_array_element(
                        shell,
                        params,
                        unexpanded_value,
                    )
                    .await?;
                    for value in values {
                        elements.push((None, value));
                    }
                }
            }
            ShellValueLiteral::Array(ArrayLiteral(elements))
        }
    };

    // Assigning to an integer variable evaluates the value arithmetically, and a value that does
    // not evaluate ends the shell, as in bash. A prefix assignment (`n=1+2 cmd`) makes a new
    // variable for the command, without the attribute, so it keeps its text.
    let new_value = if creation_scope != EnvironmentScope::Command
        && shell
            .env()
            .get(variable_name)
            .is_some_and(|(_, existing)| existing.is_treated_as_integer())
    {
        arithmetic::eval_integer_literal(shell, new_value)
            .map_err(|error| error::Error::from(error).into_fatal())?
    } else {
        new_value
    };

    if shell.options().print_commands_and_arguments
        && let ShellValueLiteral::Scalar(value) = &new_value
    {
        let op = if assignment.append { "+=" } else { "=" };
        // Bash prints an empty value as nothing: `a=`.
        let traced = if value.is_empty() {
            String::new()
        } else {
            new_value.to_string()
        };
        shell
            .trace_command(
                trace_params,
                std::format!("{}{op}{traced}", assignment.name),
            )
            .await;
    }

    // A compound assignment to an indexed array evaluates its subscripts arithmetically; an
    // associative array's are words.
    let new_value = match new_value {
        ShellValueLiteral::Array(literal) if !associative => {
            let existing = shell.env().get(variable_name).map(|(_, var)| var.value());
            let keys = crate::variables::IndexedLiteralKeys::new(existing, assignment.append);
            let (literal, failed) =
                arithmetic::resolve_indexed_array_literal(shell, params, keys, literal).await?;
            failed_element = failed;
            ShellValueLiteral::Array(literal)
        }
        value => value,
    };

    // See if we need to eval an array index.
    if let Some(idx) = &array_index {
        // An array subscript is arithmetically evaluated unless the target is an
        // associative array (in which case the subscript is used as a literal key).
        // A scalar or unset/untyped variable becomes an indexed array, so its
        // subscript still needs to be evaluated.
        let will_be_indexed_array =
            if let Some((_, existing_value)) = shell.env().get(variable_name) {
                !matches!(
                    existing_value.value(),
                    ShellValue::AssociativeArray(_)
                        | ShellValue::Unset(ShellValueUnsetType::AssociativeArray)
                )
            } else {
                true
            };

        // An empty subscript names no element (`a[]=v`, or `m[""]=v` in an associative array).
        if let ast::AssignmentName::ArrayElementName(_, written) = &assignment.name
            && (written.is_empty() || (idx.is_empty() && !will_be_indexed_array))
        {
            return Err(error::ErrorKind::ArrayIndexOutOfRange(written.clone()).into());
        }

        if will_be_indexed_array {
            array_index = Some(
                arithmetic::expand_and_eval(shell, params, idx.as_str(), false)
                    .await
                    .map_err(arithmetic::EvalError::in_subscript)?
                    .to_string(),
            );
        }
    }

    // Read option before taking mutable borrow on env.
    let export_variables_on_modification = shell.options().export_variables_on_modification;

    // See if we can find an existing value associated with the variable: the global one, for a
    // circular reference.
    let existing = if global_only.is_some() {
        shell
            .env_mut()
            .get_mut_using_policy_raw(variable_name.as_str(), EnvironmentLookup::OnlyInGlobal)
            .map(|var| (EnvironmentScope::Global, var))
    } else {
        shell.env_mut().get_mut(variable_name.as_str())
    };
    if let Some((existing_value_scope, existing_value)) = existing {
        if required_scope.is_none() || Some(existing_value_scope) == required_scope {
            if let Some(array_index) = array_index {
                match new_value {
                    ShellValueLiteral::Scalar(s) => {
                        existing_value.assign_at_index(array_index, s, assignment.append)?;
                    }
                    ShellValueLiteral::Array(_) => {
                        return error::unimp("replacing an array item with an array");
                    }
                }
            } else {
                if !export
                    && export_variables_on_modification
                    && !matches!(new_value, ShellValueLiteral::Array(_))
                {
                    export = true;
                }

                existing_value.assign(new_value, assignment.append)?;
            }

            if export {
                existing_value.export();
            }

            // That's it!
            return failed_element.map_or(Ok(()), Err);
        }

        // A command's own assignment cannot shadow a readonly variable either.
        if existing_value.is_readonly() {
            return Err(error::ErrorKind::ReadonlyVariable.into());
        }
    }

    // If we fell down here, then we need to add it.
    let new_value = if let Some(array_index) = array_index {
        match new_value {
            ShellValueLiteral::Scalar(s) => {
                ShellValue::indexed_array_from_literals(ArrayLiteral(vec![(Some(array_index), s)]))
            }
            ShellValueLiteral::Array(_) => {
                return error::unimp("cannot assign list to array member");
            }
        }
    } else {
        match new_value {
            ShellValueLiteral::Scalar(s) => {
                export = export || shell.options().export_variables_on_modification;
                ShellValue::String(s)
            }
            ShellValueLiteral::Array(values) => ShellValue::indexed_array_from_literals(values),
        }
    };

    let mut new_var = ShellVariable::new(new_value);

    if export {
        new_var.export();
    }

    let creation_scope = if global_only.is_some() {
        EnvironmentScope::Global
    } else {
        creation_scope
    };
    shell
        .env_mut()
        .add(variable_name, new_var, creation_scope)?;
    failed_element.map_or(Ok(()), Err)
}

#[expect(clippy::too_many_lines)]
pub(crate) async fn setup_redirect(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &'_ mut ExecutionParameters,
    redirect: &ast::IoRedirect,
) -> Result<(), error::Error> {
    match redirect {
        ast::IoRedirect::OutputAndError(f, append) => {
            let mut expanded_fields =
                expansion::full_expand_and_split_word(shell, params, f).await?;
            if expanded_fields.len() != 1 {
                return Err(error::ErrorKind::AmbiguousRedirect(f.value.clone()).into());
            }

            let expanded_file_path = expanded_fields.remove(0);
            setup_redirect_output_and_error_to(shell, params, &expanded_file_path, *append)?;
        }

        ast::IoRedirect::NamedFd(variable, kind, target) => {
            // `{fd}>&-` closes the descriptor the variable holds; bash names the variable when it
            // holds none.
            if matches!(target, ast::IoFileRedirectTarget::Duplicate(word) if word.value == "-") {
                let fd = shell
                    .env_str(variable)
                    .and_then(|value| value.parse::<ShellFd>().ok())
                    .ok_or_else(|| error::ErrorKind::AmbiguousRedirect(variable.clone()))?;
                params.open_files.remove_fd(fd);
                return Ok(());
            }
            // Otherwise the lowest free descriptor from 10 up, as bash allocates.
            let fd = (10..ShellFd::MAX)
                .find(|fd| params.try_fd(shell, *fd).is_none())
                .ok_or(error::ErrorKind::InvalidRedirection)?;
            let redirect = ast::IoRedirect::File(Some(fd), kind.clone(), target.clone());
            Box::pin(setup_redirect(shell, params, &redirect)).await?;
            shell.env_mut().update_or_add(
                variable,
                ShellValueLiteral::Scalar(fd.to_string()),
                |_| Ok(()),
                EnvironmentLookup::Anywhere,
                EnvironmentScope::Global,
            )?;
        }

        ast::IoRedirect::File(specified_fd_num, kind, target) => {
            match target {
                ast::IoFileRedirectTarget::Filename(f) => {
                    let mut options = std::fs::File::options();

                    let mut expanded_fields =
                        expansion::full_expand_and_split_word(shell, params, f).await?;

                    if expanded_fields.len() != 1 {
                        return Err(error::ErrorKind::AmbiguousRedirect(f.value.clone()).into());
                    }

                    // Diagnostics name the file as the script did, not its absolute path.
                    let written_path = expanded_fields.remove(0);
                    // An empty name names no file (not the working directory).
                    if written_path.is_empty() {
                        return Err(error::ErrorKind::RedirectionFailure(
                            written_path,
                            "No such file or directory".to_owned(),
                        )
                        .into());
                    }
                    // The name's bytes, including any that are not UTF-8 (see `rawbytes`).
                    let expanded_file_path: PathBuf = shell.absolute_path(Path::new(
                        &crate::rawbytes::to_os_string(written_path.as_str()),
                    ));

                    let default_fd_if_unspecified = get_default_fd_for_redirect_kind(kind);
                    match kind {
                        ast::IoFileRedirectKind::Read => {
                            options.read(true);
                        }
                        ast::IoFileRedirectKind::Write => {
                            if shell
                                .options()
                                .disallow_overwriting_regular_files_via_output_redirection
                            {
                                // First check to see if the path points to an existing regular
                                // file.
                                if !expanded_file_path.is_file() {
                                    options.create(true);
                                } else {
                                    options.create_new(true);
                                }
                                options.write(true);
                            } else {
                                options.create(true);
                                options.write(true);
                                options.truncate(true);
                            }
                        }
                        ast::IoFileRedirectKind::Append => {
                            options.create(true);
                            options.append(true);
                        }
                        ast::IoFileRedirectKind::ReadAndWrite => {
                            options.create(true);
                            options.read(true);
                            options.write(true);
                        }
                        ast::IoFileRedirectKind::Clobber => {
                            options.create(true);
                            options.write(true);
                            options.truncate(true);
                        }
                        ast::IoFileRedirectKind::DuplicateInput => {
                            options.read(true);
                        }
                        ast::IoFileRedirectKind::DuplicateOutput => {
                            options.create(true);
                            options.write(true);
                        }
                    }

                    let fd_num = specified_fd_num.unwrap_or(default_fd_if_unspecified);

                    let opened_file = shell
                        .open_file(&options, &expanded_file_path, params)
                        .map_err(|err| {
                            let message = if err.kind() == std::io::ErrorKind::AlreadyExists
                                && shell
                                    .options()
                                    .disallow_overwriting_regular_files_via_output_redirection
                            {
                                "cannot overwrite existing file".to_owned()
                            } else {
                                error::io_message(&err)
                            };
                            error::ErrorKind::RedirectionFailure(written_path.clone(), message)
                        })?;

                    params.open_files.set_fd(fd_num, opened_file);
                }

                ast::IoFileRedirectTarget::Fd(fd) => {
                    let default_fd_if_unspecified = match kind {
                        ast::IoFileRedirectKind::DuplicateInput => 0,
                        ast::IoFileRedirectKind::DuplicateOutput => 1,
                        _ => {
                            return error::unimp("unexpected redirect kind");
                        }
                    };

                    let fd_num = specified_fd_num.unwrap_or(default_fd_if_unspecified);

                    if let Some(target_file) = params.try_fd(shell, *fd) {
                        params.open_files.set_fd(fd_num, target_file);
                    } else {
                        return Err(error::ErrorKind::BadFileDescriptor(*fd).into());
                    }
                }

                ast::IoFileRedirectTarget::Duplicate(word) => {
                    let default_fd_if_unspecified = match kind {
                        ast::IoFileRedirectKind::DuplicateInput => 0,
                        ast::IoFileRedirectKind::DuplicateOutput => 1,
                        _ => {
                            return error::unimp("unexpected redirect kind");
                        }
                    };

                    let fd_num = specified_fd_num.unwrap_or(default_fd_if_unspecified);

                    let mut expanded_fields =
                        expansion::full_expand_and_split_word(shell, params, word).await?;

                    if expanded_fields.len() != 1 {
                        return Err(error::ErrorKind::AmbiguousRedirect(word.value.clone()).into());
                    }

                    let mut expanded = expanded_fields.remove(0);

                    let dash = if expanded.ends_with('-') {
                        expanded.pop();
                        true
                    } else {
                        false
                    };

                    // `N>&-` closes N; `N>&M-` moves M to N, closing M.
                    let mut closed_fd = fd_num;
                    if expanded.is_empty() {
                        // Nothing to do
                    } else if expanded.chars().all(|c: char| c.is_ascii_digit()) {
                        let source_fd_num = expanded
                            .parse::<ShellFd>()
                            .map_err(|_| error::ErrorKind::InvalidRedirection)?;

                        // Reference the same open file as the source fd (shared handle; no OS-level duplication).
                        // Bash names a descriptor that is not open as the script wrote it (`$fd`).
                        let Some(target_file) = params.try_fd(shell, source_fd_num) else {
                            return Err(error::ErrorKind::RedirectionFailure(
                                word.value.trim_end_matches('-').to_owned(),
                                "Bad file descriptor".to_owned(),
                            )
                            .into());
                        };

                        params.open_files.set_fd(fd_num, target_file);
                        closed_fd = source_fd_num;
                    } else if fd_num == 1 && !dash {
                        // Special case for compatibility: redirect stdout and stderr to the file
                        // given by `expanded`.
                        setup_redirect_output_and_error_to(
                            shell, params, &expanded, false, /* append? */
                        )?;
                    } else {
                        return Err(error::ErrorKind::InvalidRedirection.into());
                    }

                    // Ignore a descriptor that is not open; moving one onto itself (`>&1-`) keeps it.
                    let moved_onto_itself = closed_fd == fd_num && !expanded.is_empty();
                    if dash && !moved_onto_itself {
                        params.open_files.remove_fd(closed_fd);
                    }
                }

                ast::IoFileRedirectTarget::ProcessSubstitution(substitution_kind, subshell_cmd) => {
                    match kind {
                        ast::IoFileRedirectKind::Read
                        | ast::IoFileRedirectKind::Write
                        | ast::IoFileRedirectKind::Append
                        | ast::IoFileRedirectKind::ReadAndWrite
                        | ast::IoFileRedirectKind::Clobber => {
                            let (substitution_fd, substitution_file) = setup_process_substitution(
                                shell,
                                params,
                                substitution_kind,
                                subshell_cmd,
                            )
                            .await?;

                            let target_file = substitution_file.clone();
                            params.open_files.set_fd(substitution_fd, substitution_file);

                            let fd_num = specified_fd_num
                                .unwrap_or_else(|| get_default_fd_for_redirect_kind(kind));

                            params.open_files.set_fd(fd_num, target_file);
                        }
                        _ => return error::unimp("invalid process substitution"),
                    }
                }
            }
        }

        ast::IoRedirect::HereDocument(fd_num, io_here) => {
            // If not specified, default to stdin (fd 0).
            let fd_num = fd_num.unwrap_or(0);

            // Expand if required.
            let io_here_doc = if io_here.requires_expansion {
                expansion::basic_expand_heredoc_word(shell, params, &io_here.doc).await?
            } else {
                io_here.doc.flatten()
            };

            let f = setup_open_file_with_contents(io_here_doc.as_str())?;

            params.open_files.set_fd(fd_num, f);
        }

        ast::IoRedirect::HereString(fd_num, word) => {
            // If not specified, default to stdin (fd 0).
            let fd_num = fd_num.unwrap_or(0);

            let mut expanded_word =
                expansion::basic_expand_here_string(shell, params, word).await?;
            expanded_word.push('\n');

            let f = setup_open_file_with_contents(expanded_word.as_str())?;

            params.open_files.set_fd(fd_num, f);
        }
    }

    Ok(())
}

/// Sets up redirection of both stdout and stderr to the same file, given by `file_path`.
///
/// # Arguments
///
/// * `shell` - The shell instance.
/// * `params` - The execution parameters to modify.
/// * `file_path` - The path to the file to redirect output and error to.
/// * `append` - Whether to append. If `false`, the file will be truncated.
fn setup_redirect_output_and_error_to(
    shell: &Shell<impl extensions::ShellExtensions>,
    params: &mut ExecutionParameters,
    file_path: &str,
    append: bool,
) -> Result<(), error::Error> {
    // An empty name names no file (not the working directory).
    if file_path.is_empty() {
        return Err(error::ErrorKind::RedirectionFailure(
            String::new(),
            "No such file or directory".to_owned(),
        )
        .into());
    }
    // The name's bytes, including any that are not UTF-8 (see `rawbytes`).
    let abs_file_path: PathBuf =
        shell.absolute_path(Path::new(&crate::rawbytes::to_os_string(file_path)));

    // `set -C` guards `&>` and `>&word` as it guards `>`: an existing regular file is refused.
    let noclobber = !append
        && shell
            .options()
            .disallow_overwriting_regular_files_via_output_redirection;

    let mut file_options = std::fs::File::options();
    file_options.write(true);
    if noclobber && abs_file_path.is_file() {
        file_options.create_new(true);
    } else {
        file_options
            .create(true)
            .truncate(!append && !noclobber)
            .append(append);
    }

    // Diagnostics name the file as the script did, not its absolute path.
    let stdout_file = shell
        .open_file(&file_options, &abs_file_path, params)
        .map_err(|err| {
            let message = if noclobber && err.kind() == std::io::ErrorKind::AlreadyExists {
                "cannot overwrite existing file".to_owned()
            } else {
                error::io_message(&err)
            };
            error::ErrorKind::RedirectionFailure(file_path.to_owned(), message)
        })?;

    let stderr_file = stdout_file.clone();

    params.open_files.set_fd(OpenFiles::STDOUT_FD, stdout_file);
    params.open_files.set_fd(OpenFiles::STDERR_FD, stderr_file);

    Ok(())
}

/// Whether `redirect` redirects standard input.
pub(crate) fn redirects_stdin(redirect: &ast::IoRedirect) -> bool {
    match redirect {
        ast::IoRedirect::File(fd, kind, _) => {
            fd.unwrap_or_else(|| get_default_fd_for_redirect_kind(kind)) == OpenFiles::STDIN_FD
        }
        ast::IoRedirect::HereDocument(fd, _) | ast::IoRedirect::HereString(fd, _) => {
            fd.unwrap_or(OpenFiles::STDIN_FD) == OpenFiles::STDIN_FD
        }
        ast::IoRedirect::OutputAndError(..) | ast::IoRedirect::NamedFd(..) => false,
    }
}

const fn get_default_fd_for_redirect_kind(kind: &ast::IoFileRedirectKind) -> ShellFd {
    match kind {
        ast::IoFileRedirectKind::Read => 0,
        ast::IoFileRedirectKind::Write => 1,
        ast::IoFileRedirectKind::Append => 1,
        ast::IoFileRedirectKind::ReadAndWrite => 0,
        ast::IoFileRedirectKind::Clobber => 1,
        ast::IoFileRedirectKind::DuplicateInput => 0,
        ast::IoFileRedirectKind::DuplicateOutput => 1,
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[expect(
    clippy::unused_async,
    reason = "shares its signature with the wasm version, which runs the substitution"
)]
async fn setup_process_substitution(
    shell: &Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    kind: &ast::ProcessSubstitutionKind,
    subshell_cmd: &ast::SubshellCommand,
) -> Result<(ShellFd, OpenFile), error::Error> {
    // TODO(execute): Don't execute synchronously!
    // Execute in a subshell, read one xtrace level deeper, as bash does.
    let mut subshell = shell.clone();
    subshell.trace_level += 1;
    number_substitution_list(&mut subshell, &subshell_cmd.list);

    // Set up execution parameters for the child execution.
    let mut child_params = params.clone();
    child_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;

    // Set up pipe so we can connect to the command.
    let (reader, writer) = std::io::pipe()?;
    let (reader, writer) = (reader.into(), writer.into());

    let target_file = match kind {
        ast::ProcessSubstitutionKind::Read => {
            child_params.open_files.set_fd(OpenFiles::STDOUT_FD, writer);
            reader
        }
        ast::ProcessSubstitutionKind::Write => {
            child_params.open_files.set_fd(OpenFiles::STDIN_FD, reader);
            writer
        }
    };

    // Asynchronously spawn off the subshell; we intentionally don't block on its
    // completion.
    let list =
        reprinted_list(shell, &subshell_cmd.list).unwrap_or_else(|| subshell_cmd.list.clone());
    tokio::spawn(async move {
        subshell.no_fork = NoFork::for_last_command(&list, true);
        number_substitution_list(&mut subshell, &list);
        // Intentionally ignore the result of the subshell command.
        let _ = list.execute(&mut subshell, &child_params).await;
    });

    // Starting at 63 (a.k.a. 64-1)--and decrementing--look for an
    // available fd.
    let mut candidate_fd_num = 63;
    while params.open_files.contains_fd(candidate_fd_num) {
        candidate_fd_num -= 1;
        if candidate_fd_num == 0 {
            return error::unimp("no available file descriptors");
        }
    }

    Ok((candidate_fd_num, target_file))
}

#[cfg(target_arch = "wasm32")]
async fn setup_process_substitution(
    shell: &Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    kind: &ast::ProcessSubstitutionKind,
    subshell_cmd: &ast::SubshellCommand,
) -> Result<(ShellFd, OpenFile), error::Error> {
    // WASI has no pipes between processes, so a substitution is buffered: `<(list)` runs to
    // completion first and the command reads what it wrote; `>(list)` keeps what the command
    // writes and runs, with that as its input, once the command has finished.
    let mut child_params = params.clone();
    child_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;
    // The list's standard descriptors are the command's as they are now, even if the command
    // (`exec`) then changes the shell's.
    for fd in [
        OpenFiles::STDIN_FD,
        OpenFiles::STDOUT_FD,
        OpenFiles::STDERR_FD,
    ] {
        if matches!(
            child_params.open_files.fd_entry(fd),
            openfiles::OpenFileEntry::NotSpecified
        ) && let Some(file) = params.try_fd(shell, fd)
        {
            child_params.open_files.set_fd(fd, file);
        }
    }
    // As bash does, count down from 63 for a free descriptor.
    let fd = (1..=63)
        .rev()
        .find(|fd| !params.open_files.contains_fd(*fd))
        .ok_or_else(|| error::ErrorKind::Unimplemented("no available file descriptors"))?;
    let target_file = match kind {
        ast::ProcessSubstitutionKind::Read => {
            let (sink, output) = openfiles::memory_sink(openfiles::MAX_SUBSTITUTION_BYTES);
            child_params.open_files.set_fd(OpenFiles::STDOUT_FD, sink);
            run_substitution_list(shell, &subshell_cmd.list, &child_params).await;
            let captured = std::mem::take(
                &mut *output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            truncated_input(captured)
        }
        ast::ProcessSubstitutionKind::Write => {
            let (sink, input) = openfiles::memory_sink(openfiles::MAX_SUBSTITUTION_BYTES);
            params.output_substitutions.push(PendingOutputSubstitution {
                list: reprinted_list(shell, &subshell_cmd.list)
                    .unwrap_or_else(|| subshell_cmd.list.clone()),
                params: child_params,
                input,
                fd,
            });
            sink
        }
    };
    Ok((fd, target_file))
}

/// Sets up a process substitution found inside a word (`--file=<(list)`, `x=<(list)`) and
/// returns the `/dev/fd/N` path that takes its place, as bash does. The descriptor waits in
/// `params` until the command whose words are being expanded is given it, so it is open for that
/// command alone; a word expanded for anything else (an assignment by itself, a `for` list) gets
/// only the path.
pub(crate) async fn setup_word_process_substitution(
    shell: &Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    kind: &ast::ProcessSubstitutionKind,
    command: &str,
) -> Result<String, error::Error> {
    let program = shell.parse_string(command).map_err(|e| {
        error::Error::from(error::ErrorKind::ParseError(
            e,
            crate::SourceInfo::from("main"),
        ))
    })?;
    let subshell = ast::SubshellCommand {
        list: ast::CompoundList(
            program
                .complete_commands
                .into_iter()
                .flat_map(|list| list.0)
                .collect(),
        ),
        loc: brush_parser::SourceSpan::default(),
    };
    let (_, file) = setup_process_substitution(shell, params, kind, &subshell).await?;

    // As bash does, count down from 63 for a descriptor free in the command and among the
    // substitutions already waiting for it.
    let mut waiting = params
        .word_process_substitutions
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fd = (1..=63)
        .rev()
        .find(|fd| {
            !params.open_files.contains_fd(*fd) && waiting.iter().all(|(other, _)| other != fd)
        })
        .ok_or_else(|| error::ErrorKind::Unimplemented("no available file descriptors"))?;
    waiting.push((fd, file));
    drop(waiting);
    Ok(std::format!("/dev/fd/{fd}"))
}

/// The descriptors of the process substitutions inside one command's words, shared by the
/// clones of its parameters.
#[derive(Clone, Default)]
struct WordProcessSubstitutions(std::sync::Arc<std::sync::Mutex<Vec<(ShellFd, OpenFile)>>>);

impl ExecutionParameters {
    /// Gives the command these parameters are for the descriptors of the process
    /// substitutions set up while its words were expanded.
    fn install_word_process_substitutions(&mut self) {
        let waiting = std::mem::take(
            &mut *self
                .word_process_substitutions
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (fd, file) in waiting {
            self.open_files.set_fd(fd, file);
        }
    }
}

/// A process substitution's buffered output as input: when it was cut off at the limit, a read
/// at the end fails rather than seeing end-of-file.
#[cfg(target_arch = "wasm32")]
fn truncated_input(captured: openfiles::Captured) -> OpenFile {
    let error = captured
        .truncated
        .then_some(openfiles::TRUNCATED_SUBSTITUTION);
    openfiles::from_bytes_then(captured.bytes, error)
}

/// An output process substitution waiting for the command that writes to it to finish.
#[cfg(target_arch = "wasm32")]
pub(crate) struct PendingOutputSubstitution {
    list: ast::CompoundList,
    params: ExecutionParameters,
    input: std::sync::Arc<std::sync::Mutex<openfiles::Captured>>,
    /// The descriptor the command sees it as (`/dev/fd/63`).
    fd: ShellFd,
}

/// The output substitutions one command has set up, shared by the clones of its parameters.
#[cfg(target_arch = "wasm32")]
#[derive(Clone, Default)]
pub(crate) struct PendingOutputSubstitutions(
    std::sync::Arc<std::sync::Mutex<Vec<PendingOutputSubstitution>>>,
);

#[cfg(target_arch = "wasm32")]
impl PendingOutputSubstitutions {
    fn push(&self, substitution: PendingOutputSubstitution) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(substitution);
    }

    fn is_empty(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Takes out the substitutions `keep` selects.
    fn take_where(
        &self,
        mut keep: impl FnMut(&PendingOutputSubstitution) -> bool,
    ) -> Vec<PendingOutputSubstitution> {
        let mut pending = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (taken, left) = std::mem::take(&mut *pending)
            .into_iter()
            .partition(|s| keep(s));
        *pending = left;
        taken
    }

    fn take(&self) -> Vec<PendingOutputSubstitution> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

#[cfg(target_arch = "wasm32")]
impl ExecutionParameters {
    /// Gives these parameters a list of their own for `>(list)` substitutions, so the command
    /// they are for runs only its own, and returns it.
    fn own_output_substitutions(&mut self) -> PendingOutputSubstitutions {
        self.output_substitutions = PendingOutputSubstitutions::default();
        self.output_substitutions.clone()
    }
}

/// `exec` made these substitutions' sinks descriptors of the shell itself: what writes to them
/// is the rest of the shell. Each such list starts now, as a process alongside the shell reading
/// through a pipe what is written, as bash's does, and ends once every descriptor writing to it
/// is closed. Like a background job, it runs in the session's job scope.
#[cfg(target_arch = "wasm32")]
fn start_persistent_output_substitutions(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    pending: &PendingOutputSubstitutions,
) {
    use crate::execution::process;
    let held = |shell: &Shell<_>, substitution: &PendingOutputSubstitution| {
        shell
            .open_files()
            .iter_fds()
            .any(|(_, file)| openfiles::is_memory_sink_of(file, &substitution.input))
    };
    for substitution in pending.take_where(|substitution| held(shell, substitution)) {
        let (reader, writer) = openfiles::open_mem_pipe();
        let fds: Vec<ShellFd> = shell
            .open_files()
            .iter_fds()
            .filter(|(_, file)| openfiles::is_memory_sink_of(file, &substitution.input))
            .map(|(fd, _)| fd)
            .collect();
        // The list's own shell must not hold the pipe it reads, or its input never ends.
        let mut subshell = shell.clone();
        for fd in &fds {
            subshell.open_files_mut().remove_fd(*fd);
        }
        for fd in fds {
            // The descriptor the substitution was opened on closes with the command, as in bash.
            if fd == substitution.fd {
                shell.open_files_mut().remove_fd(fd);
            } else {
                shell.open_files_mut().set_fd(fd, writer.clone());
            }
        }
        drop(writer);
        let mut params = substitution.params;
        params.open_files.set_fd(OpenFiles::STDIN_FD, reader);
        let table = shell.processes().clone();
        let pid = table.allocate_substitution(shell.own_pid(), format!(">({})", substitution.list));
        shell.set_last_background_pid(pid);
        let dispositions = process::Dispositions::from_traps(shell.traps()).for_exec();
        let numbered =
            process::NumberedProcess::register(&table, pid, dispositions).in_background();
        subshell.set_own_pid(pid);
        subshell.traps_mut().reset_pipe_for_subshell();
        subshell.traps_mut().reset_exit_for_subshell();
        subshell.loop_depth = 0;
        let list = substitution.list;
        drop(process::spawn_job(
            &shell.execution_services(),
            &table,
            async move {
                numbered
                    .run(async {
                        subshell.no_fork = NoFork::for_last_command(&list, true);
                        let result = list.execute(&mut subshell, &params).await;
                        subshell.exit_with_trap_in(result, &params).await
                    })
                    .await
            },
        ));
    }
}

/// Runs the pending output substitutions, in order, each with what was written to it as its
/// input.
#[cfg(target_arch = "wasm32")]
async fn run_pending_output_substitutions(
    shell: &Shell<impl extensions::ShellExtensions>,
    pending: &PendingOutputSubstitutions,
) {
    for substitution in pending.take() {
        let input = std::mem::take(
            &mut *substitution
                .input
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut params = substitution.params;
        params
            .open_files
            .set_fd(OpenFiles::STDIN_FD, truncated_input(input));
        run_substitution_list(shell, &substitution.list, &params).await;
    }
}

/// Runs a process substitution's list as a `( list )` subshell runs, reporting its errors.
#[cfg(target_arch = "wasm32")]
async fn run_substitution_list(
    shell: &Shell<impl extensions::ShellExtensions>,
    list: &ast::CompoundList,
    params: &ExecutionParameters,
) {
    let mut subshell = shell.clone();
    subshell.traps_mut().reset_pipe_for_subshell();
    subshell.traps_mut().reset_exit_for_subshell();
    subshell.loop_depth = 0;
    // Bash reads the list one xtrace level deeper, as a command string whose last command may
    // run in place of the substitution's process.
    subshell.trace_level += 1;
    let reprinted = reprinted_list(shell, list);
    let list = reprinted.as_ref().unwrap_or(list);
    subshell.no_fork = NoFork::for_last_command(list, true);
    number_substitution_list(&mut subshell, list);
    let disposition = subshell.traps().pipe_disposition();
    let body = async {
        let result = list.execute(&mut subshell, params).await;
        subshell.exit_with_trap(result).await
    };
    if let Err(error) = crate::execution::process::run_process(disposition, body).await {
        let mut stderr = params.stderr(shell);
        let _ = shell.display_error(&mut stderr, &error);
    }
}

fn spawn_command_task(
    services: crate::execution::ExecutionServices,
    future: impl std::future::Future<Output = Result<ExecutionResult, error::Error>>
    + crate::execution::MaybeSend
    + 'static,
) -> crate::execution::CommandTask {
    #[cfg(target_arch = "wasm32")]
    {
        services.spawn(future)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = services;
        tokio::spawn(future)
    }
}

#[cfg_attr(
    target_arch = "wasm32",
    allow(
        clippy::unnecessary_wraps,
        reason = "the native implementation performs fallible file I/O"
    )
)]
fn setup_open_file_with_contents(contents: &str) -> Result<OpenFile, error::Error> {
    // The body's bytes, including any that are not UTF-8 (see `rawbytes`).
    let bytes = crate::rawbytes::encode(contents);
    let bytes = bytes.as_ref();

    // wasm32-wasip2 has no OS pipes (`std::io::pipe()` errors "operation not supported on this
    // platform"). A here-document / here-string body is fully known up front, so stage it through the
    // a shared read-only byte stream. It is staged invocation input, not a pipe waiting for a
    // consumer, so here-documents larger than the pipe capacity cannot deadlock initialization.
    #[cfg(target_arch = "wasm32")]
    {
        Ok(openfiles::from_bytes(bytes.to_vec()))
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        let (reader, mut writer) = std::io::pipe()?;

        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::os::fd::AsFd as _;

            let len = i32::try_from(bytes.len())
                .map_err(|_err| error::Error::from(error::ErrorKind::TooMuchData))?;
            nix::fcntl::fcntl(reader.as_fd(), nix::fcntl::FcntlArg::F_SETPIPE_SZ(len))?;
        }

        writer.write_all(bytes)?;
        drop(writer);

        Ok(reader.into())
    }
}

/// Bash checks a declaration builtin's compound assignment to an associative array (`-A`, or
/// one that already is) before expanding it: once the first element has a subscript, every
/// element needs one, and the error quotes the word as written.
fn check_declared_assoc_elements(
    shell: &Shell<impl extensions::ShellExtensions>,
    args: &[CommandArg],
    assignment: &ast::Assignment,
) -> Result<(), error::Error> {
    let ast::AssignmentValue::Array(elements) = &assignment.value else {
        return Ok(());
    };
    if !elements.first().is_some_and(|(key, _)| key.is_some()) {
        return Ok(());
    }
    let Some((_, word)) = elements.iter().find(|(key, _)| key.is_none()) else {
        return Ok(());
    };
    let name = assignment.name.base_name();
    // Options come before the first name, as the builtin reads them.
    let makes_associative = args
        .iter()
        .skip(1)
        .map_while(|arg| match arg {
            CommandArg::String(s) if s.len() > 1 && s.starts_with('-') && s != "--" => Some(s),
            _ => None,
        })
        .any(|option| option.contains('A'));
    let is_associative = shell.env().get(name).is_some_and(|(_, var)| {
        matches!(
            var.value(),
            ShellValue::AssociativeArray(_)
                | ShellValue::Unset(ShellValueUnsetType::AssociativeArray)
        )
    });
    if makes_associative || is_associative {
        let kind =
            error::ErrorKind::AssocSubscriptRequired(name.to_owned(), single_quoted(&word.value));
        return Err(error::Error::from(kind).into_fatal());
    }
    Ok(())
}

#[cfg(test)]
mod execution_context_tests {
    use super::ExecutionParameters;
    use std::sync::Arc;

    #[tokio::test]
    async fn cloned_parameters_retain_context_without_cross_invocation_leakage() {
        let mut first = ExecutionParameters::default();
        let mut second = ExecutionParameters::default();
        first.set_context(Arc::new(String::from("first")));
        second.set_context(Arc::new(String::from("second")));
        let child = first.clone();
        first.set_context(Arc::new(String::from("replacement")));
        tokio::task::yield_now().await;
        assert_eq!(&*child.context::<String>().unwrap(), "first");
        assert_eq!(&*second.context::<String>().unwrap(), "second");
        assert!(child.context::<usize>().is_none());
        assert!(ExecutionParameters::default().context::<String>().is_none());
    }
}

#[cfg(test)]
mod no_fork_tests {
    use super::ends_input;

    #[test]
    fn a_command_string_ends_where_bash_reads_its_end() {
        assert!(ends_input("env", 3));
        assert!(ends_input("env\n", 3));
        assert!(ends_input("env ;\n", 3));
        assert!(ends_input("env # comment", 3));
        assert!(ends_input("env # comment\n", 3));
        assert!(ends_input("env \\\n", 3));
        assert!(ends_input("é; env", 6));
        assert!(!ends_input("env\n\n", 3));
        assert!(!ends_input("env\n ", 3));
        assert!(!ends_input("env\n# comment", 3));
        assert!(!ends_input("env # comment\ntrue", 3));
        assert!(!ends_input("env; true", 3));
    }
}
