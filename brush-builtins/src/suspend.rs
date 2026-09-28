use clap::Parser;
#[cfg(unix)]
use std::io::Write;

#[cfg(unix)]
use brush_core::ExecutionExitCode;
use brush_core::{ExecutionResult, builtins};

/// Suspend the shell.
#[derive(Parser)]
pub(crate) struct SuspendCommand {
    /// Force suspend login shells.
    #[arg(short = 'f')]
    force: bool,

    /// Operands: too many, which ends the shell, as in bash.
    #[arg(trailing_var_arg = true, hide = true)]
    extra: Vec<String>,
}

impl builtins::Command for SuspendCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        // An operand ends the shell, as in bash.
        if !self.extra.is_empty() {
            context.report("too many arguments")?;
            let mut result = ExecutionResult::general_error();
            result.next_control_flow = brush_core::ExecutionControlFlow::ExitShell;
            return Ok(result);
        }
        // WASM shells have no job control: as bash does then, only `-f` suspends.
        #[cfg(target_arch = "wasm32")]
        {
            use brush_core::execution::process;
            if !self.force {
                context.report("cannot suspend: no job control")?;
                return Ok(ExecutionResult::general_error());
            }
            // Stop every process of the shell's group, until something sends CONT.
            let table = context.shell.processes().clone();
            process::signal_process_group(&table, table.shell_pid(), process::signals::STOP);
            Ok(ExecutionResult::success())
        }

        #[cfg(unix)]
        {
            if context.shell.options().login_shell && !self.force {
                writeln!(context.stderr(), "login shell cannot be suspended")?;
                return Ok(ExecutionExitCode::InvalidUsage.into());
            }

            #[expect(clippy::cast_possible_wrap)]
            brush_core::sys::signal::kill_process(
                std::process::id() as i32,
                brush_core::traps::TrapSignal::Signal(nix::sys::signal::SIGSTOP),
            )?;

            Ok(ExecutionResult::success())
        }
    }
}
