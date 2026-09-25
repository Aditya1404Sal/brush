use clap::Parser;
use std::io::Write;
#[cfg(target_arch = "wasm32")]
use std::task::Poll;

use brush_core::{ExecutionExitCode, ExecutionResult, builtins, error};

/// Wait for jobs to terminate.
#[derive(Parser)]
pub(crate) struct WaitCommand {
    /// Wait for specified job to terminate (instead of change status).
    #[arg(short = 'f')]
    wait_for_terminate: bool,

    /// Wait for a single job to change status; if jobs are specified, waits for
    /// the first to change status, and otherwise waits for the next change.
    #[arg(short = 'n')]
    wait_for_first_or_next: bool,

    /// Name of variable to receive the job ID of the job whose status is indicated.
    #[arg(short = 'p', value_name = "VAR_NAME")]
    variable_to_receive_id: Option<String>,

    /// Process IDs or job specs to wait for.
    ids: Vec<String>,
}

/// Awaits `wait`, unless a signal with a trap arrives first: then `wait` returns `128 + n` at
/// once and the trap runs, as in bash. `Err` carries that status.
async fn interruptible<T>(
    wait: impl std::future::Future<Output = Result<T, brush_core::Error>>,
) -> Result<Result<T, ExecutionResult>, brush_core::Error> {
    #[cfg(target_arch = "wasm32")]
    {
        let trapped = std::pin::pin!(brush_core::execution::process::trapped_signal());
        match futures::future::select(trapped, std::pin::pin!(wait)).await {
            futures::future::Either::Left((signal, _)) => {
                Ok(Err(ExecutionExitCode::from(128 + signal).into()))
            }
            futures::future::Either::Right((result, _)) => result.map(Ok),
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    wait.await.map(Ok)
}

impl builtins::Command for WaitCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        if self.wait_for_terminate {
            return error::unimp("wait -f");
        }
        if self.variable_to_receive_id.is_some() {
            return error::unimp("wait -p");
        }
        if self.wait_for_first_or_next {
            #[cfg(target_arch = "wasm32")]
            return wait_next(context, &self.ids).await;
            #[cfg(not(target_arch = "wasm32"))]
            return error::unimp("wait -n");
        }

        if self.ids.is_empty() {
            // Wait for every job of this shell; as in bash, their statuses are not kept.
            let serials: Vec<u64> = context
                .shell
                .jobs()
                .jobs
                .iter()
                .filter(|job| !job.is_listing_copy())
                .map(|job| job.serial)
                .collect();
            for serial in serials {
                let jobs = context.shell.jobs_mut();
                let Some(job) = jobs.jobs.iter_mut().find(|job| job.serial == serial) else {
                    continue;
                };
                let fresh = !job.is_reaped();
                match interruptible(job.wait()).await? {
                    Ok(status) => {
                        let notice = fresh.then(|| death_notice(job, &status)).flatten();
                        jobs.remove_finished(serial, None);
                        report_death(&context, notice)?;
                    }
                    Err(interrupted) => return Ok(interrupted),
                }
            }
            return Ok(ExecutionResult::success());
        }

        let mut result = ExecutionResult::success();
        for id in &self.ids {
            let jobs = context.shell.jobs_mut();
            let job = if id.starts_with('%') {
                match jobs.find_job_spec(id) {
                    Ok(job) => job,
                    Err(brush_core::jobs::JobSpecError::Ambiguous) => {
                        let name = id.trim_start_matches(['%', '?']);
                        context.report(format_args!("{name}: ambiguous job spec"))?;
                        result = ExecutionExitCode::from(127).into();
                        continue;
                    }
                    Err(brush_core::jobs::JobSpecError::NoSuchJob) => {
                        context.report(format_args!("{id}: no such job"))?;
                        result = ExecutionExitCode::from(127).into();
                        continue;
                    }
                }
            } else {
                // A process ID: a synthetic process number on WASM.
                #[cfg(target_arch = "wasm32")]
                {
                    let Ok(pid) = id.parse::<brush_core::process_table::Pid>() else {
                        context.report(format_args!("`{id}': not a pid or valid job spec"))?;
                        result = ExecutionResult::general_error();
                        continue;
                    };
                    // `disown` takes a job out of the table, but it is still a child to wait for.
                    match jobs.job_with_pid_mut(pid) {
                        Some(job) => job,
                        None => {
                            // A job reaped earlier still has its status, as in bash.
                            if let Some(status) = jobs.reaped_status(pid) {
                                result = status;
                            } else {
                                context.report(format_args!(
                                    "pid {pid} is not a child of this shell"
                                ))?;
                                result = ExecutionExitCode::from(127).into();
                            }
                            continue;
                        }
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                return error::unimp("wait with process IDs");
            };
            let serial = job.serial;
            let fresh = !job.is_reaped();
            match interruptible(job.wait()).await? {
                Ok(status) => {
                    let notice = fresh.then(|| death_notice(job, &status)).flatten();
                    context.shell.jobs_mut().mark_reaped(serial, &status);
                    report_death(&context, notice)?;
                    result = status;
                }
                Err(interrupted) => return Ok(interrupted),
            }
        }
        // `wait` returns a status; the job's signal did not end `wait` itself.
        result.terminating_signal = None;
        Ok(result)
    }
}

/// What bash reports about a job a signal ended, when it waits for one: every signal but INT,
/// TERM and PIPE, with the process number and the command. Returns the signal and that text.
fn death_notice(job: &brush_core::jobs::Job, status: &ExecutionResult) -> Option<(u8, String)> {
    let signal = status
        .terminating_signal
        .filter(|signal| !matches!(signal, 2 | 13 | 15))?;
    let pid = job
        .pids()
        .first()
        .map_or_else(String::new, ToString::to_string);
    let description = brush_core::traps::signal_description(signal);
    Some((
        signal,
        format!("{pid:>5} {description:<27}{}", job.command_line),
    ))
}

/// Writes a [`death_notice`], unless the shell traps the signal.
fn report_death(
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    notice: Option<(u8, String)>,
) -> Result<(), brush_core::Error> {
    let Some((signal, text)) = notice else {
        return Ok(());
    };
    if brush_core::traps::TrapSignal::try_from(i32::from(signal))
        .is_ok_and(|trapped| context.shell.traps().handles(trapped))
    {
        return Ok(());
    }
    let prefix = context.shell.diagnostic_prefix();
    writeln!(context.stderr(), "{prefix}{text}")?;
    Ok(())
}

/// `wait -n`: waits for the next of this shell's jobs to finish, among those `ids` name if any,
/// and returns its status; 127 if there is none to wait for.
#[cfg(target_arch = "wasm32")]
async fn wait_next(
    context: brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    ids: &[String],
) -> Result<ExecutionResult, brush_core::Error> {
    let mut wanted = Vec::new();
    for id in ids {
        let jobs = context.shell.jobs_mut();
        let job = if id.starts_with('%') {
            jobs.find_job_spec(id).ok()
        } else {
            id.parse::<brush_core::process_table::Pid>()
                .ok()
                .and_then(|pid| jobs.job_with_pid_mut(pid))
        };
        match job {
            Some(job) => wanted.push(job.serial),
            None => context.report(format_args!("`{id}': not a pid or valid job spec"))?,
        }
    }
    if !ids.is_empty() && wanted.is_empty() {
        return Ok(ExecutionExitCode::from(127).into());
    }
    // Report exactly one finished job per call, oldest first, so simultaneous completions are not
    // lost between successive `wait -n` calls. Polling registers this task to be woken when a
    // job ends or a trapped signal arrives, so `wait -n` sleeps instead of polling in a loop.
    let next = std::future::poll_fn(|cx| {
        let jobs = context.shell.jobs_mut();
        let mut any = false;
        for job in &mut jobs.jobs {
            if job.is_listing_copy()
                || job.is_reaped()
                || !(wanted.is_empty() || wanted.contains(&job.serial))
            {
                continue;
            }
            any = true;
            match job.poll_done_cx(cx) {
                Ok(Some(result)) => {
                    return Poll::Ready(result.map(|status| Next::Finished(job.serial, status)));
                }
                Ok(None) => {}
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
        if !any {
            return Poll::Ready(Ok(Next::NoJob));
        }
        match brush_core::execution::process::pending_trapped_signal() {
            Some(signal) => Poll::Ready(Ok(Next::Trapped(signal))),
            None => Poll::Pending,
        }
    })
    .await?;
    match next {
        Next::Finished(serial, status) => {
            let jobs = context.shell.jobs_mut();
            let notice = jobs
                .jobs
                .iter()
                .find(|job| job.serial == serial)
                .and_then(|job| death_notice(job, &status));
            jobs.remove_finished(serial, Some(&status));
            report_death(&context, notice)?;
            Ok(ExecutionResult::new(u8::from(status.exit_code)))
        }
        Next::NoJob => Ok(ExecutionExitCode::from(127).into()),
        Next::Trapped(signal) => Ok(ExecutionExitCode::from(128 + signal).into()),
    }
}

/// What ends a `wait -n`.
#[cfg(target_arch = "wasm32")]
enum Next {
    /// This job, by serial, finished with this status.
    Finished(u64, ExecutionResult),
    /// There is no job to wait for.
    NoJob,
    /// A signal with a trap arrived.
    Trapped(u8),
}
