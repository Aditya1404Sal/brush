use clap::Parser;
use std::io::Write;

use brush_core::{ExecutionControlFlow, ExecutionResult, builtins};

/// Breaks out of a control-flow loop.
#[derive(Parser)]
pub(crate) struct BreakCommand {
    /// If specified, indicates which enclosing loop to break out of.
    #[arg(allow_hyphen_values = true)]
    which_loop: Option<String>,
}

impl builtins::Command for BreakCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let prefix = context.shell.diagnostic_prefix();
        let name = &context.command_name;
        // As in bash: outside a loop this is a diagnostic, not an error, and nothing changes.
        if context.shell.loop_depth() == 0 {
            writeln!(
                context.stderr(),
                "{prefix}{name}: only meaningful in a `for', `while', or `until' loop"
            )?;
            return Ok(ExecutionResult::success());
        }
        let mut result = ExecutionResult::success();
        let levels = match self.which_loop.as_deref().map(str::parse::<i64>) {
            None => 1,
            Some(Ok(n)) if n > 0 => n,
            Some(Ok(_)) => {
                // Bash reports the count and still leaves (or continues) the innermost loop.
                let arg = self.which_loop.as_deref().unwrap_or_default();
                writeln!(
                    context.stderr(),
                    "{prefix}{name}: {arg}: loop count out of range"
                )?;
                result = ExecutionResult::general_error();
                1
            }
            Some(Err(_)) => {
                let arg = self.which_loop.as_deref().unwrap_or_default();
                writeln!(
                    context.stderr(),
                    "{prefix}{name}: {arg}: numeric argument required"
                )?;
                let mut result = ExecutionResult::new(2);
                result.next_control_flow = ExecutionControlFlow::ExitShell;
                return Ok(result);
            }
        };
        // Leaving more loops than enclose the command leaves them all.
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let levels = (levels as usize).min(context.shell.loop_depth()) - 1;
        result.next_control_flow = ExecutionControlFlow::BreakLoop { levels };
        Ok(result)
    }
}
