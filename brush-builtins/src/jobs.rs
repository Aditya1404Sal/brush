use clap::Parser;
use std::io::Write;

use brush_core::{ExecutionResult, builtins, error, jobs};

/// Manage jobs.
#[derive(Parser)]
pub(crate) struct JobsCommand {
    /// Also show process IDs.
    #[arg(short = 'l')]
    also_show_pids: bool,

    /// List only jobs that have changed status since the last notification.
    #[arg(short = 'n')]
    list_changed_only: bool,

    /// Show only process IDs.
    #[arg(short = 'p')]
    show_pids_only: bool,

    /// Show only running jobs.
    #[arg(short = 'r')]
    running_jobs_only: bool,

    /// Show only stopped jobs.
    #[arg(short = 's')]
    stopped_jobs_only: bool,

    /// Job specs to list.
    // TODO(jobs): Add -x option
    job_specs: Vec<String>,
}

impl builtins::Command for JobsCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // As bash does, notice the jobs that have finished: they are listed once, as Done.
        let finished = context.shell.jobs_mut().poll()?;
        let mut listed: Vec<&jobs::Job> = context
            .shell
            .jobs()
            .jobs
            .iter()
            .chain(finished.iter().map(|(job, _)| job))
            .collect();
        listed.sort_by_key(|job| job.id);

        if self.also_show_pids {
            #[cfg(target_arch = "wasm32")]
            {
                // As bash lays it out: `[N]+  1234 Running                    cmd &`.
                for job in &listed {
                    let pid = job
                        .pids()
                        .first()
                        .map_or_else(String::new, ToString::to_string);
                    writeln!(
                        context.stdout(),
                        "[{}]{} {pid:>5} {:<27}{}",
                        job.id,
                        job.annotation(),
                        job.state.to_string(),
                        job.display_command()
                    )?;
                }
                return Ok(ExecutionResult::success());
            }
            #[cfg(not(target_arch = "wasm32"))]
            return error::unimp("jobs -l");
        }
        if self.list_changed_only {
            return error::unimp("jobs -n");
        }

        if self.job_specs.is_empty() {
            for job in listed {
                self.display_job(&context, job)?;
            }
        } else {
            return error::unimp("jobs with job specs");
        }

        Ok(ExecutionResult::success())
    }
}

impl JobsCommand {
    fn display_job(
        &self,
        context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        job: &jobs::Job,
    ) -> Result<(), brush_core::Error> {
        if self.running_jobs_only && !matches!(job.state, jobs::JobState::Running) {
            return Ok(());
        }
        if self.stopped_jobs_only && !matches!(job.state, jobs::JobState::Stopped) {
            return Ok(());
        }

        if self.show_pids_only {
            if let Some(pid) = job.representative_pid() {
                writeln!(context.stdout(), "{pid}")?;
            }
        } else {
            writeln!(context.stdout(), "{job}")?;
        }

        Ok(())
    }
}
