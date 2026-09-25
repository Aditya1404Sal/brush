use clap::Parser;
use std::io::Write;

#[cfg(unix)]
use brush_core::sys;
use brush_core::traps::TrapSignal;
use brush_core::{ExecutionExitCode, ExecutionResult, builtins};

/// Signal a job or process.
#[derive(Parser)]
pub(crate) struct KillCommand {
    /// Options and operands, parsed as bash's `kill` does: `-s SIG`, `-n NUM`, `-SIG`, `-l`/`-L`,
    /// `--`, then process IDs and job specs.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

const USAGE: &str =
    "kill: usage: kill [-s sigspec | -n signum | -sigspec] pid | jobspec ... or kill -l [sigspec]";

/// A signal operand as bash decodes it: a name (any case, with or without `SIG`) or a number;
/// `0` is the null signal that only checks the target exists.
fn decode_signal(spec: &str) -> Option<i32> {
    if spec == "0" {
        return Some(0);
    }
    TrapSignal::try_from(spec)
        .ok()
        .or_else(|| {
            spec.parse::<i32>()
                .ok()
                .and_then(|n| TrapSignal::try_from(n).ok())
        })
        .and_then(|signal| i32::try_from(signal).ok())
        .filter(|n| *n > 0)
}

impl builtins::Command for KillCommand {
    type Error = brush_core::Error;

    /// Takes the arguments as they are, `--` included, to parse them as bash does; only a
    /// leading `--help` goes to the usual help.
    fn new<I>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = String>,
    {
        let args: Vec<String> = args.into_iter().collect();
        if args.get(1).is_some_and(|arg| arg == "--help") {
            return Self::try_parse_from(args);
        }
        Ok(Self {
            args: args.into_iter().skip(1).collect(),
        })
    }

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // Bash's default is TERM. Unix keeps its existing default until upstream changes it.
        #[cfg(unix)]
        let mut signal: Option<i32> = Some(nix::sys::signal::Signal::SIGKILL as i32);
        #[cfg(not(unix))]
        let mut signal: Option<i32> = Some(15);
        let mut spec = String::new();
        let mut listing = false;
        let mut saw_signal = false;
        let mut args = self.args.iter().peekable();
        while let Some(word) = args.peek() {
            match word.as_str() {
                "-l" | "-L" => listing = true,
                "-s" | "-n" => {
                    let option = word.as_str();
                    args.next();
                    let Some(value) = args.peek() else {
                        context.report(format_args!("{option}: option requires an argument"))?;
                        return Ok(ExecutionResult::general_error());
                    };
                    spec.clone_from(value);
                    signal = decode_signal(value);
                    saw_signal = true;
                }
                "--" => {
                    args.next();
                    break;
                }
                word if word.starts_with('-') && word.len() > 1 && !saw_signal => {
                    // `-SIG`, `-NUM`: only the first; later ones may be process groups.
                    spec = word[1..].to_owned();
                    signal = decode_signal(&spec);
                    saw_signal = true;
                }
                _ => break,
            }
            args.next();
        }
        let operands: Vec<&String> = args.collect();

        if listing {
            return print_signals(&context, &operands);
        }
        let Some(signal) = signal else {
            context.report(format_args!("{spec}: invalid signal specification"))?;
            return Ok(ExecutionResult::general_error());
        };
        if operands.is_empty() {
            writeln!(context.stderr(), "{USAGE}")?;
            return Ok(ExecutionExitCode::InvalidUsage.into());
        }

        // Each target is signalled; the status is a failure if any of them failed.
        let mut stderr = context.stderr();
        let mut result = ExecutionResult::success();
        for target in operands {
            if let Some(failure) = signal_target(
                &mut *context.shell,
                &context.command_name,
                &mut stderr,
                target,
                signal,
            )? {
                result = failure;
            }
        }
        Ok(result)
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
    signal: i32,
) -> Result<Option<ExecutionResult>, brush_core::Error> {
    let probe_only = signal == 0;
    let signal = TrapSignal::try_from(signal.max(1))?;
    if target.starts_with('%') {
        // It's a job spec.
        if let Some(job) = shell.jobs_mut().resolve_job_spec(target) {
            if probe_only {
                job.check_signalable()?;
            } else {
                job.kill(signal)?;
            }
        } else {
            let prefix = shell.diagnostic_prefix();
            writeln!(stderr, "{prefix}{command_name}: {target}: no such job")?;
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
/// or `None` when the target exists. Signal 0 only checks that it does.
#[cfg(target_arch = "wasm32")]
fn signal_target<SE: brush_core::ShellExtensions>(
    shell: &mut brush_core::Shell<SE>,
    command_name: &str,
    stderr: &mut impl Write,
    target: &str,
    signal: i32,
) -> Result<Option<ExecutionResult>, brush_core::Error> {
    use brush_core::execution::process;
    let table = shell.processes().clone();
    let probe_only = signal == 0;
    let number = u8::try_from(signal).unwrap_or(process::signals::TERM);
    let prefix = shell.diagnostic_prefix();
    if target.starts_with('%') {
        let leader = match shell.jobs_mut().find_job_spec(target) {
            Ok(job) => job.leader(),
            Err(brush_core::jobs::JobSpecError::Ambiguous) => {
                let name = target.trim_start_matches(['%', '?']);
                writeln!(stderr, "{prefix}{command_name}: {name}: ambiguous job spec")?;
                return Ok(Some(ExecutionResult::general_error()));
            }
            Err(brush_core::jobs::JobSpecError::NoSuchJob) => None,
        }
        .filter(|pid| process::process_exists(&table, *pid));
        let Some(leader) = leader else {
            writeln!(stderr, "{prefix}{command_name}: {target}: no such job")?;
            return Ok(Some(ExecutionResult::general_error()));
        };
        if !probe_only {
            process::signal_process_group(&table, leader, number);
        }
        return Ok(None);
    }
    let Ok(pid) = target.parse::<brush_core::process_table::Pid>() else {
        writeln!(
            stderr,
            "{prefix}{command_name}: `{target}': not a pid or valid job spec"
        )?;
        return Ok(Some(ExecutionResult::general_error()));
    };
    // 0 is this shell's process group: without job control, every process of the call. A
    // negative number is the group that process leads.
    let (pid, group) = match pid {
        0 => (table.shell_pid(), true),
        pid if pid < 0 => (-pid, true),
        pid => (pid, false),
    };
    let delivered = if probe_only {
        process::process_exists(&table, pid)
    } else if group {
        process::signal_process_group(&table, pid, number)
    } else {
        process::signal_process(&table, pid, number)
    };
    if !delivered {
        writeln!(stderr, "{prefix}{command_name}: ({pid}) - No such process")?;
        return Ok(Some(ExecutionResult::general_error()));
    }
    Ok(None)
}

fn print_signals(
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    signals: &[&String],
) -> Result<ExecutionResult, brush_core::Error> {
    if signals.is_empty() {
        return brush_core::traps::format_signals(
            context.stdout(),
            TrapSignal::iterator().filter(|s| !matches!(s, TrapSignal::Exit)),
        )
        .map(|()| ExecutionResult::success());
    }
    let mut result = ExecutionResult::success();
    for spec in signals {
        // A number names its signal, as does an exit status above 128; a name gives its number.
        if let Ok(mut number) = spec.parse::<i32>() {
            if number > 128 {
                number -= 128;
            }
            match TrapSignal::try_from(number) {
                // The C library's reserved numbers have no name to print.
                Ok(TrapSignal::Signal(_)) if (32..=34).contains(&number) => {}
                Ok(signal) => {
                    let name = signal.as_str();
                    writeln!(
                        context.stdout(),
                        "{}",
                        name.strip_prefix("SIG").unwrap_or(name)
                    )?;
                }
                Err(_) => {
                    context.report(format_args!("{spec}: invalid signal specification"))?;
                    result = ExecutionResult::general_error();
                }
            }
        } else if let Some(number) = decode_signal(spec) {
            writeln!(context.stdout(), "{number}")?;
        } else {
            context.report(format_args!("{spec}: invalid signal specification"))?;
            result = ExecutionResult::general_error();
        }
    }
    Ok(result)
}
