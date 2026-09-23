use clap::Parser;

use brush_core::{ExecutionResult, builtins};

/// Remove jobs from the job table.
#[derive(Parser)]
pub(crate) struct DisownCommand {
    /// Keep the jobs in the table, only sparing them SIGHUP (which this shell never sends).
    #[arg(short = 'h')]
    keep_in_table: bool,

    /// Remove every job.
    #[arg(short = 'a')]
    all: bool,

    /// Remove only running jobs.
    #[arg(short = 'r')]
    running_only: bool,

    /// Job specs to remove; the current job when none is given.
    job_specs: Vec<String>,
}

impl builtins::Command for DisownCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let jobs = context.shell.jobs_mut();
        let ids: Vec<Result<usize, &str>> = if self.job_specs.is_empty() {
            if self.all || self.running_only {
                jobs.jobs
                    .iter()
                    .filter(|job| {
                        !self.running_only
                            || matches!(job.state, brush_core::jobs::JobState::Running)
                    })
                    .map(|job| Ok(job.id))
                    .collect()
            } else {
                vec![jobs.current_job().map(|job| job.id).ok_or("current")]
            }
        } else {
            self.job_specs
                .iter()
                .map(|spec| {
                    jobs.resolve_job_spec(spec)
                        .map(|job| job.id)
                        .ok_or(spec.as_str())
                })
                .collect()
        };

        let mut result = ExecutionResult::success();
        for id in ids {
            match id {
                Ok(id) if !self.keep_in_table => {
                    context.shell.jobs_mut().disown(id);
                }
                Ok(_) => {}
                Err(spec) => {
                    context.report(format_args!("{spec}: no such job"))?;
                    result = ExecutionResult::general_error();
                }
            }
        }
        Ok(result)
    }
}
