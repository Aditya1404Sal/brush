use clap::Parser;
use std::io::Write;

use brush_core::{ExecutionControlFlow, ExecutionExitCode, ExecutionResult, builtins};

/// Return from the current function.
#[derive(Parser)]
pub(crate) struct ReturnCommand {
    /// The exit code to return.
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    code: Vec<String>,
}

impl builtins::Command for ReturnCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if context.shell.in_function() || context.shell.in_sourced_script() {
            let code_8bit = match crate::exit::status_operand(&context, &self.code)? {
                crate::exit::StatusOperand::Status(code) => code,
                crate::exit::StatusOperand::None => context.shell.last_exit_status(),
                crate::exit::StatusOperand::NotANumber => 2,
                // As in bash, more than one operand ends the shell, with status 1.
                crate::exit::StatusOperand::TooMany => {
                    let mut result = ExecutionResult::new(1);
                    result.next_control_flow = ExecutionControlFlow::ExitShell;
                    return Ok(result);
                }
            };
            context.shell.note_status_before_return();
            let mut result = ExecutionResult::new(code_8bit);
            result.next_control_flow = ExecutionControlFlow::ReturnFromFunctionOrScript;
            Ok(result)
        } else {
            let _ = writeln!(
                context.stderr(),
                "{}return: can only `return' from a function or sourced script",
                context.shell.diagnostic_prefix()
            );
            Ok(ExecutionExitCode::InvalidUsage.into())
        }
    }
}
