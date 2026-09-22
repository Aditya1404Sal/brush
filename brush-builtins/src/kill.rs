use clap::Parser;
use std::io::Write;

#[cfg(unix)]
use brush_core::sys;
use brush_core::traps::TrapSignal;
use brush_core::{ExecutionExitCode, ExecutionResult, builtins};

/// Signal a job or process.
#[derive(Parser)]
pub(crate) struct KillCommand {
    /// Name of the signal to send.
    #[arg(short = 's', value_name = "SIG_NAME")]
    signal_name: Option<String>,

    /// Number of the signal to send.
    #[arg(short = 'n', value_name = "SIG_NUM")]
    signal_number: Option<usize>,

    //
    // TODO(kill): implement -sigspec syntax
    /// List known signal names.
    #[arg(short = 'l', short_alias = 'L')]
    list_signals: bool,

    // Interpretation of these depends on whether -l is present.
    #[arg(allow_hyphen_values = true)]
    args: Vec<String>,
}

impl builtins::Command for KillCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let mut signal_zero = false;

        // Bash's default is TERM. Unix keeps its existing default until upstream changes it.
        #[cfg(unix)]
        let mut trap_signal = TrapSignal::Signal(nix::sys::signal::Signal::SIGKILL);
        #[cfg(target_arch = "wasm32")]
        let mut trap_signal: TrapSignal = "TERM".parse()?;

        // Try parsing the signal name (if specified).
        if let Some(signal_name) = &self.signal_name {
            if let Ok(parsed_trap_signal) = TrapSignal::try_from(signal_name.as_str()) {
                trap_signal = parsed_trap_signal;
            } else {
                writeln!(
                    context.stderr(),
                    "{}: invalid signal name: {}",
                    context.command_name,
                    signal_name
                )?;
                return Ok(ExecutionExitCode::InvalidUsage.into());
            }
        }

        // Try parsing the signal number (if specified).
        if let Some(signal_number) = &self.signal_number {
            if *signal_number == 0 {
                signal_zero = true;
            } else {
                #[expect(clippy::cast_possible_truncation)]
                #[expect(clippy::cast_possible_wrap)]
                if let Ok(parsed_trap_signal) = TrapSignal::try_from(*signal_number as i32) {
                    trap_signal = parsed_trap_signal;
                } else {
                    writeln!(
                        context.stderr(),
                        "{}: invalid signal number: {}",
                        context.command_name,
                        signal_number
                    )?;
                    return Ok(ExecutionExitCode::InvalidUsage.into());
                }
            }
        }

        // Look through the remaining args for a pid/job spec or a -sigspec style option.
        let mut pid_or_job_spec = None;
        for arg in &self.args {
            // See if this is -sigspec syntax. The sigspec may be a signal name
            // (e.g., -TERM) or a signal number (e.g., -9, including -0).
            if let Some(possible_sigspec) = arg.strip_prefix("-") {
                if Ok(0) == possible_sigspec.parse::<i32>() {
                    signal_zero = true;
                } else if let Ok(parsed_trap_signal) = possible_sigspec.parse::<TrapSignal>() {
                    signal_zero = false;
                    trap_signal = parsed_trap_signal;
                } else {
                    writeln!(
                        context.stderr(),
                        "{}: {}: invalid signal specification",
                        context.command_name,
                        possible_sigspec
                    )?;
                    return Ok(ExecutionResult::general_error());
                }
            } else if pid_or_job_spec.is_none() {
                pid_or_job_spec = Some(arg);
            } else {
                writeln!(
                    context.stderr(),
                    "{}: too many jobs or processes specified",
                    context.command_name
                )?;
                return Ok(ExecutionExitCode::InvalidUsage.into());
            }
        }

        if self.list_signals {
            return print_signals(&context, self.args.as_ref());
        } else {
            let Some(pid_or_job_spec) = pid_or_job_spec else {
                writeln!(context.stderr(), "{}: invalid usage", context.command_name)?;
                return Ok(ExecutionExitCode::InvalidUsage.into());
            };

            let mut stderr = context.stderr();
            if let Some(failure) = signal_target(
                &mut *context.shell,
                &context.command_name,
                &mut stderr,
                pid_or_job_spec,
                trap_signal,
                signal_zero,
            )? {
                return Ok(failure);
            }
        }
        Ok(ExecutionResult::success())
    }
}

/// Delivers a signal to a host process or job. Returns the failure status to report, or `None`
/// when the target exists.
#[cfg(unix)]
fn signal_target<SE: brush_core::ShellExtensions>(
    shell: &mut brush_core::Shell<SE>,
    command_name: &str,
    stderr: &mut impl Write,
    target: &str,
    signal: TrapSignal,
    probe_only: bool,
) -> Result<Option<ExecutionResult>, brush_core::Error> {
    if target.starts_with('%') {
        // It's a job spec.
        if let Some(job) = shell.jobs_mut().resolve_job_spec(target) {
            if probe_only {
                job.check_signalable()?;
            } else {
                job.kill(signal)?;
            }
        } else {
            writeln!(stderr, "{command_name}: {target}: no such job")?;
            return Ok(Some(ExecutionResult::general_error()));
        }
    } else {
        // It's a pid.
        let pid = brush_core::int_utils::parse(target, 10)?;
        if probe_only {
            sys::signal::check_signalable(pid)?;
        } else {
            sys::signal::kill_process(pid, signal)?;
        }
    }
    Ok(None)
}

/// Delivers a signal to a synthetic WASM process or job. Returns the failure status to report,
/// or `None` when the target exists.
#[cfg(target_arch = "wasm32")]
fn signal_target<SE: brush_core::ShellExtensions>(
    shell: &mut brush_core::Shell<SE>,
    command_name: &str,
    stderr: &mut impl Write,
    target: &str,
    signal: TrapSignal,
    probe_only: bool,
) -> Result<Option<ExecutionResult>, brush_core::Error> {
    use brush_core::execution::process;
    let table = shell.processes().clone();
    let number = i32::try_from(signal)
        .ok()
        .and_then(|number| u8::try_from(number).ok())
        .unwrap_or(process::signals::TERM);
    if target.starts_with('%') {
        let leader = shell
            .jobs_mut()
            .resolve_job_spec(target)
            .and_then(|job| job.leader())
            .filter(|pid| process::process_exists(&table, *pid));
        let Some(leader) = leader else {
            writeln!(stderr, "{command_name}: {target}: no such job")?;
            return Ok(Some(ExecutionResult::general_error()));
        };
        if !probe_only {
            process::signal_process_group(&table, leader, number);
        }
    } else {
        let pid: brush_core::process_table::Pid = brush_core::int_utils::parse(target, 10)?;
        let delivered = if probe_only {
            process::process_exists(&table, pid)
        } else {
            process::signal_process(&table, pid, number)
        };
        if !delivered {
            writeln!(stderr, "{command_name}: ({pid}) - No such process")?;
            return Ok(Some(ExecutionResult::general_error()));
        }
    }
    Ok(None)
}

fn print_signals(
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    signals: &[String],
) -> Result<ExecutionResult, brush_core::Error> {
    let mut exit_code = ExecutionResult::success();
    if !signals.is_empty() {
        for s in signals {
            // If the user gives us a code, we print the name; if they give a name, we print its
            // code.
            enum PrintSignal {
                Name(&'static str),
                Num(i32),
            }

            let signal = if let Ok(n) = s.parse::<i32>() {
                // bash compatibility. `SIGHUP` -> `HUP`
                TrapSignal::try_from(n).map(|s| {
                    PrintSignal::Name(s.as_str().strip_prefix("SIG").unwrap_or(s.as_str()))
                })
            } else {
                TrapSignal::try_from(s.as_str()).map(|sig| {
                    i32::try_from(sig).map_or(PrintSignal::Name(sig.as_str()), PrintSignal::Num)
                })
            };

            match signal {
                Ok(PrintSignal::Num(n)) => {
                    writeln!(context.stdout(), "{n}")?;
                }
                Ok(PrintSignal::Name(s)) => {
                    writeln!(context.stdout(), "{s}")?;
                }
                Err(e) => {
                    writeln!(context.stderr(), "{e}")?;
                    exit_code = ExecutionResult::general_error();
                }
            }
        }
    } else {
        return brush_core::traps::format_signals(
            context.stdout(),
            TrapSignal::iterator().filter(|s| !matches!(s, TrapSignal::Exit)),
        )
        .map(|()| ExecutionResult::success());
    }

    Ok(exit_code)
}
