use clap::Parser;

use brush_core::{ExecutionControlFlow, ExecutionResult, builtins};

/// Exit the shell.
#[derive(Parser)]
pub(crate) struct ExitCommand {
    /// The exit code to return.
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    code: Vec<String>,
}

impl builtins::Command for ExitCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let code_8bit = match status_operand(&context, &self.code)? {
            StatusOperand::Status(code) => code,
            StatusOperand::None => context.shell.last_exit_status(),
            // As in bash: a status that is not a number fails the command; more than one
            // operand still ends the shell, with status 1.
            StatusOperand::NotANumber => return Ok(ExecutionResult::new(2)),
            StatusOperand::TooMany => 1,
        };

        let mut result = ExecutionResult::new(code_8bit);
        result.next_control_flow = ExecutionControlFlow::ExitShell;

        Ok(result)
    }
}

/// Exit a login shell.
#[derive(Parser)]
pub(crate) struct LogoutCommand {
    /// The exit code to return.
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    code: Vec<String>,
}

impl builtins::Command for LogoutCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // As bash words it, `logout` only ends a login shell.
        if !context.shell.options().login_shell {
            context.report("not login shell: use `exit'")?;
            return Ok(ExecutionResult::general_error());
        }
        let code_8bit = match status_operand(&context, &self.code)? {
            StatusOperand::Status(code) => code,
            StatusOperand::None => context.shell.last_exit_status(),
            StatusOperand::NotANumber => return Ok(ExecutionResult::new(2)),
            StatusOperand::TooMany => 1,
        };
        let mut result = ExecutionResult::new(code_8bit);
        result.next_control_flow = ExecutionControlFlow::ExitShell;
        Ok(result)
    }
}

/// The status operand `exit` and `return` were given, read as bash reads it.
pub(crate) enum StatusOperand {
    /// None was given.
    None,
    /// A number, taken modulo 256.
    Status(u8),
    /// One that is not a number (reported).
    NotANumber,
    /// More than one (reported).
    TooMany,
}

/// Reads the status operand of `exit` or `return`, reporting it in bash's words when it is not a
/// number or there is more than one.
pub(crate) fn status_operand(
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    operands: &[String],
) -> Result<StatusOperand, brush_core::Error> {
    match operands {
        [] => Ok(StatusOperand::None),
        [operand] => {
            if let Ok(code) = operand.trim().parse::<i64>() {
                // The status is the number modulo 256.
                Ok(StatusOperand::Status(code.to_le_bytes()[0]))
            } else {
                context.report(format_args!("{operand}: numeric argument required"))?;
                Ok(StatusOperand::NotANumber)
            }
        }
        _ => {
            context.report("too many arguments")?;
            Ok(StatusOperand::TooMany)
        }
    }
}
