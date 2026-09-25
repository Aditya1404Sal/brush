use clap::Parser;
use std::fmt::Write as _;
use std::io::Write;

use brush_core::{ExecutionResult, builtins, jobs};

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
        let mut finished = context.shell.jobs_mut().poll()?;
        let mut result = ExecutionResult::success();

        // The jobs to list: those the specs name, or all of them.
        let selected: Option<Vec<u64>> = if self.job_specs.is_empty() {
            None
        } else {
            let mut serials = Vec::new();
            for spec in &self.job_specs {
                let found = finished
                    .iter()
                    .find(|(job, _)| spec_matches(job, spec))
                    .map(|(job, _)| job.serial)
                    .or_else(|| {
                        context
                            .shell
                            .jobs_mut()
                            .find_job_spec(spec)
                            .ok()
                            .map(|job| job.serial)
                    });
                if let Some(serial) = found {
                    serials.push(serial);
                } else {
                    context.report(format_args!("{spec}: no such job"))?;
                    result = ExecutionResult::general_error();
                }
            }
            Some(serials)
        };

        let mut listed: Vec<&mut jobs::Job> = context
            .shell
            .jobs_mut()
            .jobs
            .iter_mut()
            .chain(finished.iter_mut().map(|(job, _)| job))
            .filter(|job| {
                selected
                    .as_ref()
                    .is_none_or(|serials| serials.contains(&job.serial))
            })
            .collect();
        listed.sort_by_key(|job| job.id);

        let mut out = String::new();
        for job in listed {
            if self.running_jobs_only && !matches!(job.state, jobs::JobState::Running) {
                continue;
            }
            if self.stopped_jobs_only && !matches!(job.state, jobs::JobState::Stopped) {
                continue;
            }
            if self.list_changed_only && !job.changed_since_listed() {
                continue;
            }
            job.mark_listed();
            if self.show_pids_only {
                // A pipeline's first process leads its group.
                if let Some(pid) = job
                    .pids()
                    .first()
                    .copied()
                    .or_else(|| job.representative_pid())
                {
                    let _ = writeln!(out, "{pid}");
                }
            } else if self.also_show_pids {
                #[cfg(target_arch = "wasm32")]
                for line in job.long_lines() {
                    out.push_str(&line);
                    out.push('\n');
                }
                #[cfg(not(target_arch = "wasm32"))]
                return brush_core::error::unimp("jobs -l");
            } else {
                let _ = writeln!(out, "{job}");
            }
        }
        context.stdout().write_all(out.as_bytes())?;
        Ok(result)
    }
}

/// Whether a job the table just dropped as finished is the one `spec` names by number.
fn spec_matches(job: &jobs::Job, spec: &str) -> bool {
    spec.strip_prefix('%')
        .and_then(|id| id.parse::<usize>().ok())
        .is_some_and(|id| id == job.id)
}
