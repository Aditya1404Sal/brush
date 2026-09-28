use clap::Parser;
use std::io::Write;

use brush_core::{ExecutionResult, builtins};

/// Shift positional arguments.
#[derive(Parser)]
pub(crate) struct ShiftCommand {
    /// Number of positions to shift the arguments by (defaults to 1).
    #[arg(allow_hyphen_values = true)]
    n: Option<i32>,

    /// Operands after the first: too many, which ends the shell, as in bash.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    extra: Vec<String>,
}

impl builtins::Command for ShiftCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // More than one operand ends the shell, as bash's special builtins do.
        if !self.extra.is_empty() {
            context.report("too many arguments")?;
            let mut result = ExecutionResult::general_error();
            result.next_control_flow = brush_core::ExecutionControlFlow::ExitShell;
            return Ok(result);
        }
        let n = self.n.unwrap_or(1);

        if n < 0 {
            let prefix = context.shell.diagnostic_prefix();
            writeln!(
                context.stderr(),
                "{prefix}shift: {n}: shift count out of range"
            )?;
            return Ok(ExecutionResult::general_error());
        }

        #[expect(clippy::cast_sign_loss)]
        let n = n as usize;

        let args = context.shell.current_shell_args_mut();

        // Shifting past the last parameter changes nothing and fails, quietly unless
        // `shift_verbose` is on, as in bash (which names the count only when one was given).
        if n > args.len() {
            if context.shell.options().shift_verbose {
                match self.n {
                    Some(n) => context.report(format_args!("{n}: shift count out of range"))?,
                    None => context.report("shift count out of range")?,
                }
            }
            return Ok(ExecutionResult::general_error());
        }

        args.drain(0..n);

        Ok(ExecutionResult::success())
    }
}
