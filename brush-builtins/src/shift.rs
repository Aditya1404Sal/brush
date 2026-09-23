use clap::Parser;
use std::io::Write;

use brush_core::{ExecutionResult, builtins};

/// Shift positional arguments.
#[derive(Parser)]
pub(crate) struct ShiftCommand {
    /// Number of positions to shift the arguments by (defaults to 1).
    #[arg(allow_hyphen_values = true)]
    n: Option<i32>,
}

impl builtins::Command for ShiftCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
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

        // Shifting past the last parameter changes nothing and fails quietly, as in bash.
        if n > args.len() {
            return Ok(ExecutionResult::general_error());
        }

        args.drain(0..n);

        Ok(ExecutionResult::success())
    }
}
