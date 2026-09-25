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
    /// Policy for how to manage spawned external processes.
    pub process_group_policy: ProcessGroupPolicy,
    /// Whether `errexit` (exit on error) behavior should be
    /// suppressed in this execution context. Defaults to `false`.
    pub suppress_errexit: bool,
    /// Embedder context, cloned into stages and substitutions rather than installed globally.
    context: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
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
        let mut result = ExecutionResult::success();
        let (program, interrupted) = shell.begin_program();

        for (index, command) in self.complete_commands.iter().enumerate() {
            shell.begin_command_unit(program, index);
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
        let mut result = ExecutionResult::success();

        for ast::CompoundListItem(ao_list, sep) in &self.0 {
            let run_async = matches!(sep, ast::SeparatorOperator::Async);

            if run_async {
                #[cfg(target_arch = "wasm32")]
                if shell.jobs().running_count(shell.processes()) >= jobs::MAX_RUNNING_JOBS {
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

                if shell.options().interactive && !shell.is_subshell() {
                    writeln!(params.stderr(shell), "{job_formatted}")?;
                }

                result = ExecutionResult::success();
            } else {
                result = ao_list.execute(shell, params).await?;

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

#[cfg(target_arch = "wasm32")]
fn spawn_async_ao_list_in_task<'a, SE: extensions::ShellExtensions>(
    ao_list: &ast::AndOrList,
    shell: &'a mut Shell<SE>,
    params: &ExecutionParameters,
) -> &'a jobs::Job {
    use crate::execution::process;
    let table = shell.processes().clone();
    let command_line = ao_list.to_string();
    let leader = table.allocate(shell.own_pid(), command_line.clone());

    let mut cloned_shell = shell.clone();
    let mut cloned_params = params.clone();
    let cloned_ao_list = ao_list.clone();
    cloned_shell.options_mut().interactive = false;
    cloned_shell.set_own_pid(leader);
    // Bash resets caught handlers in asynchronous subshells; ignored signals stay ignored.
    cloned_shell.traps_mut().reset_caught_for_subshell();
    if let Ok(null) = openfiles::null() {
        cloned_params.set_fd(openfiles::OpenFiles::STDIN_FD, null);
    }

    let mut dispositions = process::Dispositions::from_traps(cloned_shell.traps());
    // Without job control, asynchronous commands ignore SIGINT.
    dispositions.int = crate::traps::PipeDisposition::Ignored;
    // Register now, so `kill $!` and `kill %1` reach the job before its task first runs.
    let leader_process = process::NumberedProcess::register(&table, leader, dispositions);

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
                leader_process.register_child(pid, dispositions.for_exec())
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
    let subshell_body = sole_subshell_body(ao_list).cloned();
    let join_handle = spawn_command_task(shell.execution_services(), async move {
        leader_process
            .run(async move {
                let result = match subshell_body {
                    Some(list) => match list.execute(&mut cloned_shell, &cloned_params).await {
                        Ok(result) => result,
                        Err(error) => {
                            let mut stderr = cloned_params.stderr(&cloned_shell);
                            let _ = cloned_shell.display_error(&mut stderr, &error);
                            error.into_result(&cloned_shell)
                        }
                    },
                    None => {
                        cloned_ao_list
                            .execute(&mut cloned_shell, &cloned_params)
                            .await?
                    }
                };
                // A job reports only its status: its `exit`, `break` or `return` must never
                // act on the shell that later waits for it.
                Ok(ExecutionResult::from(result.exit_code))
            })
            .await
    });

    let pids = if stage_pids.is_empty() {
        vec![leader]
    } else {
        stage_pids
    };
    shell.jobs_mut().add_as_current(jobs::Job::new_numbered(
        [jobs::JobTask::Internal(join_handle)],
        command_line,
        leader,
        pids,
    ))
}

/// The body of a background list that is exactly one plain `( list )`.
#[cfg(target_arch = "wasm32")]
const fn sole_subshell_body(ao_list: &ast::AndOrList) -> Option<&ast::CompoundList> {
    let pipeline = &ao_list.first;
    if !ao_list.additional.is_empty() || pipeline.bang || pipeline.timed.is_some() {
        return None;
    }
    match pipeline.seq.as_slice() {
        [
            ast::Command::Compound(
                ast::CompoundCommand::Subshell(ast::SubshellCommand { list, .. }),
                None,
            ),
        ] => Some(list),
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
        cloned_ao_list
            .execute(&mut cloned_shell, &cloned_params)
            .await
    });

    shell.jobs_mut().add_as_current(jobs::Job::new(
        [jobs::JobTask::Internal(join_handle)],
        ao_list.to_string(),
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
        let mut result = wait_result?;

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
        if !result.is_success() && !params.suppress_errexit && !self.bang {
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

        // If requested, report timing.
        if let (Some(timed), Some(stopwatch)) = (&self.timed, &stopwatch)
            && let Some(mut stderr) = params.try_fd(shell, openfiles::OpenFiles::STDERR_FD)
        {
            let timing = stopwatch.stop()?;
            if timed.is_posix_output() {
                std::write!(
                    stderr,
                    "real {}\nuser {}\nsys {}\n",
                    timing::format_duration_posixly(&timing.wall),
                    timing::format_duration_posixly(&timing.user),
                    timing::format_duration_posixly(&timing.system),
                )?;
            } else {
                std::write!(
                    stderr,
                    "\nreal\t{}\nuser\t{}\nsys\t{}\n",
                    timing::format_duration_non_posixly(&timing.wall),
                    timing::format_duration_non_posixly(&timing.user),
                    timing::format_duration_non_posixly(&timing.system),
                )?;
            }
        }

        Ok(result)
    }
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
        if let Some(Some(reader)) = pipe_readers.pop() {
            cmd_params.open_files.set_fd(OpenFiles::STDIN_FD, reader);
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
                let mut stage_shell = shell.clone();
                if let Some(stage_process) = &stage_process {
                    stage_shell.set_own_pid(stage_process.pid());
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

            PipelineExecutionContext {
                shell: commands::ShellForCommand::OwnedShell {
                    target: Box::new(shell.clone()),
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
    use crate::execution::process;
    let services = shell.execution_services();
    shell.traps_mut().reset_pipe_for_subshell();
    let disposition = shell.traps().pipe_disposition();
    services.spawn(async move {
        let body = async move {
            let context = PipelineExecutionContext {
                shell: commands::ShellForCommand::ParentShell(&mut shell),
                process_group_id: None,
            };
            match command
                .execute_in_pipeline(context, params)
                .await?
                .wait()
                .await?
            {
                ExecutionWaitResult::Completed(result) => Ok(result),
                ExecutionWaitResult::Stopped(_) => Ok(ExecutionResult::stopped()),
            }
        };
        match numbered {
            Some(numbered) => numbered.run(body).await,
            None => process::run_process(disposition, body).await,
        }
    })
}

async fn wait_for_pipeline_processes_and_update_status(
    pipeline: &ast::Pipeline,
    mut process_spawn_results: VecDeque<ExecutionSpawnResult>,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<ExecutionResult, error::Error> {
    let mut result = ExecutionResult::success();
    let mut stopped_children = vec![];
    let mut last_failure_exit_code: Option<(ExecutionExitCode, Option<u8>)> = None;

    // A compound command or function definition run on its own in this shell leaves PIPESTATUS
    // as the last pipeline it ran set it, as in bash; `(( ))`, `[[ ]]` and `( )` set it.
    let keeps_statuses = matches!(
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
    );

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

    Ok(result)
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

        // Updates the shell with information about the currently executing command.
        pipeline_context.shell.set_current_cmd(self);

        match self {
            Self::Simple(simple) => simple.execute_in_pipeline(pipeline_context, params).await,
            Self::Compound(compound, redirects) => {
                // `>(list)` substitutions in these redirects run once the command has finished.
                #[cfg(target_arch = "wasm32")]
                let pending = params.own_output_substitutions();

                // Set up any additional redirects.
                if let Some(redirects) = redirects {
                    for redirect in &redirects.0 {
                        setup_redirect(&mut pipeline_context.shell, &mut params, redirect).await?;
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
        match self {
            Self::BraceGroup(ast::BraceGroupCommand { list, .. }) => {
                list.execute(shell, params).await
            }
            Self::Subshell(ast::SubshellCommand { list, .. }) => {
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
                #[cfg(target_arch = "wasm32")]
                let disposition = subshell.traps().pipe_disposition();
                let body = async {
                    let result = list.execute(&mut subshell, params).await;
                    subshell.exit_with_trap(result).await
                };

                // Handle errors within the subshell context to prevent fatal errors
                // from propagating to the parent shell.
                #[cfg(target_arch = "wasm32")]
                let execution = crate::execution::process::run_process(disposition, body).await;
                #[cfg(not(target_arch = "wasm32"))]
                let execution = body.await;
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
                // the shell, break out of loops, etc.
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
                let result =
                    if extendedtests::eval_extended_test_expr(&e.expr, shell, params).await? {
                        0
                    } else {
                        1
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

        // If we were given explicit words to iterate over, then expand them all, with splitting
        // enabled.
        let expanded_values = if let Some(unexpanded_values) = &self.values {
            expand_words(shell, params, unexpanded_values).await?
        } else {
            // Otherwise, we use the current positional parameters.
            shell.current_shell_args().to_vec()
        };

        for value in expanded_values {
            if shell.options().print_commands_and_arguments {
                if let Some(unexpanded_values) = &self.values {
                    shell
                        .trace_command(
                            params,
                            std::format!(
                                "for {} in {}",
                                self.variable_name,
                                unexpanded_values.iter().join(" ")
                            ),
                        )
                        .await;
                } else {
                    shell
                        .trace_command(params, std::format!("for {}", self.variable_name))
                        .await;
                }
            }

            // Update the variable. A nameref control variable is pointed at each word in turn
            // rather than assigned through, as bash does.
            let nameref = shell
                .env()
                .get_raw(&self.variable_name)
                .is_some_and(|(_, var)| var.is_treated_as_nameref());
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
        let value = self.expr.eval(shell, params, true).await?;
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
            initializer.eval(shell, params, true).await?;
        }

        loop {
            if let Some(condition) = &self.condition {
                // An empty condition (e.g., `for (( ; ; ))`) means "always true".
                if !condition.value.is_empty() && condition.eval(shell, params, true).await? == 0 {
                    break;
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
                updater.eval(shell, params, true).await?;
            }
        }

        shell.set_last_exit_status(result.exit_code.into());
        Ok(result)
    }
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
        shell.define_func(func_name, self.clone(), &source_info);

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

        // Before its words are expanded, the command's text becomes BASH_COMMAND (unless a trap
        // handler is running, whose commands leave it alone) and the DEBUG trap runs, as in bash.
        if !context.shell.running_trap_handler() {
            context.shell.env_mut().update_or_add(
                "BASH_COMMAND",
                ShellValueLiteral::Scalar(self.to_string()),
                |_| Ok(()),
                EnvironmentLookup::Anywhere,
                EnvironmentScope::Global,
            )?;
        }
        if context.shell.traps().handles(traps::TrapSignal::Debug) {
            let _ = context
                .shell
                .invoke_trap_handler(traps::TrapSignal::Debug, &params)
                .await?;
        }

        let mut assignments = vec![];
        let mut args: Vec<CommandArg> = vec![];
        let mut command_takes_assignments = false;

        // Capture the status change count before expansion, so we can detect
        // if expansion (e.g., command substitution) set an exit status.
        let status_change_count_before_expansion = context.shell.last_exit_status_change_count();

        for item in prefix_iter.chain(cmd_name_items.iter()).chain(suffix_iter) {
            match item {
                CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                    if let Err(e) = setup_redirect(&mut context.shell, &mut params, redirect).await
                    {
                        let _ = context
                            .shell
                            .display_error(&mut params.stderr(&context.shell), &e);
                        return Ok(ExecutionResult::general_error().into());
                    }
                }
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
                    if args.is_empty() {
                        // If we haven't yet seen any arguments, then this must be a proper
                        // scoped assignment. Add it to the list we're accumulating.
                        assignments.push(assignment);
                    } else {
                        if command_takes_assignments {
                            // This looks like an assignment, and the command being invoked is a
                            // well-known builtin that takes arguments that need to function like
                            // assignments (but which are processed by the builtin).
                            let expanded =
                                expand_assignment(&mut context.shell, &params, assignment).await?;
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
                    let mut next_args =
                        expansion::full_expand_and_split_word(&mut context.shell, &params, arg)
                            .await?;

                    if args.is_empty() {
                        if let Some(cmd_name) = next_args.first() {
                            // Aliases are only expanded when `expand_aliases` is enabled; it's
                            // enabled by default for interactive shells.
                            if context.shell.options().expand_aliases
                                && context.shell.alias_in_effect(cmd_name)
                                && let Some(alias_value) =
                                    context.shell.aliases().get(cmd_name.as_str())
                            {
                                //
                                // TODO(#57): This is a total hack; aliases are supposed to be
                                // handled much earlier in the process.
                                //
                                // N.B. Tokenizing first releases our borrow of the shell's aliases,
                                // so we can take a mutable borrow of the shell to expand the words.
                                let alias_words = tokenize_alias_body(&context.shell, alias_value);
                                let mut alias_pieces =
                                    expand_words(&mut context.shell, &params, alias_words).await?;

                                next_args.remove(0);
                                alias_pieces.append(&mut next_args);

                                next_args = alias_pieces;
                            }

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
                    }

                    let mut next_args = next_args.into_iter().map(CommandArg::String).collect();
                    args.append(&mut next_args);
                }
            }
        }

        // If we have a command, then execute it.
        if let Some(CommandArg::String(cmd_name)) = args.first() {
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

            let result = execute_command(context, params, cmd_name, &assignments, &args).await;
            #[cfg(target_arch = "wasm32")]
            let result = match result {
                Ok(spawned) if !pending.is_empty() => {
                    // The command must finish writing before the substitutions read it.
                    let completed = ExecutionResult::from(spawned.wait().await?);
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
                    false,
                    None,
                    EnvironmentScope::Global,
                )
                .await?;
            }

            // Assignment-only statements clear $_ (set to empty string).
            // This matches bash behavior where assignments don't have a "last
            // argument".
            context.shell.update_last_arg_variable(None);

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

async fn execute_command<T: Into<String>>(
    mut context: PipelineExecutionContext<'_, impl extensions::ShellExtensions>,
    params: ExecutionParameters,
    cmd_name: T,
    assignments: &[&ast::Assignment],
    args: &[CommandArg],
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
            true,
            Some(EnvironmentScope::Command),
            EnvironmentScope::Command,
        )
        .await
        {
            // A readonly variable keeps its value: bash reports it and still runs the command.
            Err(error) if error.abandons_command() => (),
            result => result?,
        }
    }

    if guard.shell().options().print_commands_and_arguments {
        guard
            .shell()
            .trace_command(
                &params,
                args.iter().map(|arg| arg.quote_for_tracing()).join(" "),
            )
            .await;
    }

    guard.detach();
    drop(guard);

    // Construct the command struct.
    let mut cmd =
        commands::SimpleCommand::new(context.shell, params, cmd_name.into(), args.iter().cloned());
    cmd.process_group_id = context.process_group_id;

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
        // Bash names the element: `a[-3]: bad array subscript`.
        error::ErrorKind::ArrayIndexOutOfRange(index) => error::ErrorKind::ArrayIndexOutOfRange(
            format!("{}[{index}]", assignment.name.base_name()),
        )
        .into(),
        _ => error,
    })
}

#[expect(clippy::too_many_lines)]
async fn apply_assignment_unchecked(
    assignment: &ast::Assignment,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
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
    // Assigning through a nameref assigns to the variable it names, and a nameref to an array
    // element (`declare -n ref='arr[1]'`) to that element, just as `arr[1]=value` would.
    let mut resolved_name = shell
        .env()
        .resolve_nameref(variable_name.as_str())
        .into_owned();
    if array_index.is_none()
        && let Some((array, index)) = shell.env().resolve_nameref_element(variable_name.as_str())
    {
        resolved_name = array;
        array_index = Some(index);
    }
    let variable_name = &resolved_name;

    // Expand the values.
    let new_value = match &assignment.value {
        ast::AssignmentValue::Scalar(unexpanded_value) => {
            let value =
                expansion::basic_expand_assignment_word(shell, params, unexpanded_value).await?;
            ShellValueLiteral::Scalar(value)
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

    // Assigning to an integer variable evaluates the value arithmetically.
    let new_value = if shell
        .env()
        .get(variable_name)
        .is_some_and(|(_, existing)| existing.is_treated_as_integer())
    {
        arithmetic::eval_integer_literal(shell, new_value)?
    } else {
        new_value
    };

    if shell.options().print_commands_and_arguments {
        let op = if assignment.append { "+=" } else { "=" };
        shell
            .trace_command(params, std::format!("{}{op}{new_value}", assignment.name))
            .await;
    }

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
                    .await?
                    .to_string(),
            );
        }
    }

    // Read option before taking mutable borrow on env.
    let export_variables_on_modification = shell.options().export_variables_on_modification;

    // See if we can find an existing value associated with the variable.
    if let Some((existing_value_scope, existing_value)) =
        shell.env_mut().get_mut(variable_name.as_str())
    {
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
            return Ok(());
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

    shell.env_mut().add(variable_name, new_var, creation_scope)
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
            // `{fd}>&-` closes the descriptor the variable holds.
            if matches!(target, ast::IoFileRedirectTarget::Duplicate(word) if word.value == "-") {
                let fd = shell
                    .env_str(variable)
                    .and_then(|value| value.parse::<ShellFd>().ok())
                    .ok_or_else(|| {
                        error::ErrorKind::AmbiguousRedirect(format!("{{{variable}}}"))
                    })?;
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
                    let expanded_file_path: PathBuf =
                        shell.absolute_path(Path::new(written_path.as_str()));

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
                        let Some(target_file) = params.try_fd(shell, source_fd_num) else {
                            return Err(error::ErrorKind::BadFileDescriptor(source_fd_num).into());
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

                    if dash {
                        // Ignore a descriptor that is not open.
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
    let abs_file_path: PathBuf = shell.absolute_path(Path::new(file_path));

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
    // Execute in a subshell.
    let mut subshell = shell.clone();

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
    let subshell_cmd = subshell_cmd.to_owned();
    tokio::spawn(async move {
        // Intentionally ignore the result of the subshell command.
        let _ = subshell_cmd
            .list
            .execute(&mut subshell, &child_params)
            .await;
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
    let target_file = match kind {
        ast::ProcessSubstitutionKind::Read => {
            let (sink, output) = openfiles::memory_sink();
            child_params.open_files.set_fd(OpenFiles::STDOUT_FD, sink);
            run_substitution_list(shell, &subshell_cmd.list, &child_params).await;
            let bytes = std::mem::take(
                &mut *output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            openfiles::from_bytes(bytes)
        }
        ast::ProcessSubstitutionKind::Write => {
            let (sink, input) = openfiles::memory_sink();
            params.output_substitutions.push(PendingOutputSubstitution {
                list: subshell_cmd.list.clone(),
                params: child_params,
                input,
            });
            sink
        }
    };

    // As bash does, count down from 63 for a free descriptor.
    let fd = (1..=63)
        .rev()
        .find(|fd| !params.open_files.contains_fd(*fd))
        .ok_or_else(|| error::ErrorKind::Unimplemented("no available file descriptors"))?;
    Ok((fd, target_file))
}

/// An output process substitution waiting for the command that writes to it to finish.
#[cfg(target_arch = "wasm32")]
pub(crate) struct PendingOutputSubstitution {
    list: ast::CompoundList,
    params: ExecutionParameters,
    input: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
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
            .set_fd(OpenFiles::STDIN_FD, openfiles::from_bytes(input));
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
    let bytes = contents.as_bytes();

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
