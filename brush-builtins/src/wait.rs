use clap::Parser;
use std::io::Write;

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

impl builtins::Command for WaitCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        if self.wait_for_terminate {
            return error::unimp("wait -f");
        }
        if self.wait_for_first_or_next {
            // Report exactly one finished job per call, oldest first, so simultaneous
            // completions are not lost between successive `wait -n` calls.
            #[cfg(target_arch = "wasm32")]
            loop {
                let jobs = &mut context.shell.jobs_mut().jobs;
                if jobs.is_empty() {
                    return Ok(ExecutionExitCode::from(127).into());
                }
                for index in 0..jobs.len() {
                    if let Some(result) = jobs[index].poll_done()? {
                        jobs.remove(index);
                        return result;
                    }
                }
                (context.shell.execution_services().yield_now)().await;
            }
            #[cfg(not(target_arch = "wasm32"))]
            return error::unimp("wait -n");
        }
        if self.variable_to_receive_id.is_some() {
            return error::unimp("wait -p");
        }

        let mut result = ExecutionResult::success();

        if !self.ids.is_empty() {
            for id in &self.ids {
                if id.starts_with('%') {
                    // It's a job spec.
                    if let Some(job) = context.shell.jobs_mut().resolve_job_spec(id) {
                        result = job.wait().await?;
                    } else {
                        context.report(format_args!("{id}: no such job"))?;

                        result = ExecutionExitCode::GeneralError.into();
                    }
                } else {
                    // It's a process ID: a synthetic job number on WASM.
                    #[cfg(target_arch = "wasm32")]
                    {
                        let Ok(pid) = brush_core::int_utils::parse::<brush_core::process_table::Pid>(
                            id.as_str(),
                            10,
                        ) else {
                            context.report(format_args!("`{id}': not a pid or valid job spec"))?;
                            result = ExecutionExitCode::GeneralError.into();
                            continue;
                        };
                        let job = context
                            .shell
                            .jobs_mut()
                            .jobs
                            .iter_mut()
                            .find(|job| job.pids().contains(&pid) || job.leader() == Some(pid));
                        if let Some(job) = job {
                            result = job.wait().await?;
                        } else {
                            context
                                .report(format_args!("pid {pid} is not a child of this shell"))?;
                            result = ExecutionExitCode::from(127).into();
                        }
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    return error::unimp("wait with process IDs");
                }
            }
        } else {
            // Wait for all jobs.
            let jobs = context.shell.jobs_mut().wait_all().await?;

            if context.shell.options().enable_job_control {
                for job in jobs {
                    writeln!(context.stdout(), "{job}")?;
                }
            }
        }

        Ok(result)
    }
}
