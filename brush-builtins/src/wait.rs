use clap::Parser;

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
                match interruptible(job.wait()).await? {
                    Ok(_) => jobs.remove_finished(serial, None),
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
            match interruptible(job.wait()).await? {
                Ok(status) => {
                    context.shell.jobs_mut().mark_reaped(serial, &status);
                    result = status;
                }
                Err(interrupted) => return Ok(interrupted),
            }
        }
        Ok(result)
    }
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
    // lost between successive `wait -n` calls.
    loop {
        let jobs = context.shell.jobs_mut();
        let candidates: Vec<u64> = jobs
            .jobs
            .iter()
            .filter(|job| !job.is_listing_copy() && !job.is_reaped())
            .filter(|job| wanted.is_empty() || wanted.contains(&job.serial))
            .map(|job| job.serial)
            .collect();
        if candidates.is_empty() {
            return Ok(ExecutionExitCode::from(127).into());
        }
        for serial in candidates {
            let Some(job) = jobs.jobs.iter_mut().find(|job| job.serial == serial) else {
                continue;
            };
            if let Some(result) = job.poll_done()? {
                let status = result?;
                jobs.remove_finished(serial, Some(&status));
                return Ok(status);
            }
        }
        if let Some(signal) = brush_core::execution::process::pending_trapped_signal() {
            return Ok(ExecutionExitCode::from(128 + signal).into());
        }
        (context.shell.execution_services().yield_now)().await;
    }
}
