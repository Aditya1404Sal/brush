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
    job_specs: Vec<String>,

    /// With `-x`: the command to run, with job specs in it replaced by process group numbers.
    #[arg(skip)]
    execute: Option<Vec<String>>,

    /// `-x` came after `-l`, `-p` or `-n`, which bash rejects.
    #[arg(skip)]
    execute_conflict: bool,
}

impl builtins::Command for JobsCommand {
    type Error = brush_core::Error;

    /// `-x` ends option parsing, as bash's `jobs` does: what follows is the command to run, its
    /// own options included.
    fn new<I>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = String>,
    {
        let args: Vec<String> = args.into_iter().collect();
        let (mut listing_form, mut conflict, mut execute) = (false, false, false);
        let mut index = 1;
        while let Some(arg) = args.get(index) {
            if arg == "--" {
                index += 1;
                break;
            }
            let Some(letters) = arg.strip_prefix('-').filter(|letters| {
                !letters.is_empty() && letters.chars().all(|c| "lpnxrs".contains(c))
            }) else {
                break;
            };
            for letter in letters.chars() {
                match letter {
                    'l' | 'p' | 'n' => listing_form = true,
                    'x' => {
                        conflict |= listing_form;
                        execute = true;
                    }
                    _ => {}
                }
            }
            index += 1;
        }
        if !execute {
            return Self::try_parse_from(args);
        }
        Ok(Self {
            also_show_pids: false,
            list_changed_only: false,
            show_pids_only: false,
            running_jobs_only: false,
            stopped_jobs_only: false,
            job_specs: Vec::new(),
            execute: Some(args[index..].to_vec()),
            execute_conflict: conflict,
        })
    }

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if let Some(words) = &self.execute {
            return execute_with_replacements(context, words, self.execute_conflict).await;
        }
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

/// `jobs -x`: runs `words` as a command, with each word that names a job replaced by the job's
/// process group, as bash does. Without job control every job is in the shell's group, `$$`. A
/// word that names no job passes through, and the words are not expanded again.
async fn execute_with_replacements(
    context: brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    words: &[String],
    conflict: bool,
) -> Result<ExecutionResult, brush_core::Error> {
    if conflict {
        context.report("no other options allowed with `-x'")?;
        return Ok(ExecutionResult::general_error());
    }
    if words.is_empty() {
        return Ok(ExecutionResult::success());
    }
    let group = context.shell.processes().shell_pid().to_string();
    let line = words
        .iter()
        .map(|word| {
            if word.starts_with('%') && context.shell.jobs().lists_job_spec(word) {
                group.clone()
            } else {
                brush_core::escape::single_quote(word).into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let source_info = context.shell.call_stack().current_pos_as_source_info();
    context
        .shell
        .run_string(line, &source_info, &context.params)
        .await
}

/// Whether a job the table just dropped as finished is the one `spec` names by number.
fn spec_matches(job: &jobs::Job, spec: &str) -> bool {
    spec.strip_prefix('%')
        .and_then(|id| id.parse::<usize>().ok())
        .is_some_and(|id| id == job.id)
}
