use clap::Parser;

use std::io::Write as _;

use brush_core::{ExecutionExitCode, ExecutionResult, arithmetic::EvalError, builtins};

/// Evaluate arithmetic expressions.
#[derive(Parser)]
pub(crate) struct LetCommand {
    /// Arithmetic expressions to evaluate.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    exprs: Vec<String>,
}

impl builtins::Command for LetCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let mut result = ExecutionExitCode::InvalidUsage.into();

        if self.exprs.is_empty() {
            context.report("expression expected")?;
            return Ok(ExecutionResult::general_error());
        }

        for expr in &self.exprs {
            let evaluated = match brush_core::arithmetic::parse(expr.as_str()).and_then(|parsed| {
                brush_core::arithmetic::eval_reporting(&parsed, context.shell, &context.params)
            }) {
                Ok(evaluated) => evaluated,
                Err(error) => return report(&context, expr, error),
            };

            if evaluated == 0 {
                result = ExecutionResult::general_error();
            } else {
                result = ExecutionResult::success();
            }
        }

        Ok(result)
    }
}

/// Fails `let` on an expression it could not evaluate, reported as bash words it
/// (`let: EXPR: message`, or `NAME: readonly variable`). An unset variable under `set -u` ends
/// the shell.
fn report(
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    expr: &str,
    error: EvalError,
) -> Result<ExecutionResult, brush_core::Error> {
    if let Some(name) = error.unset_variable() {
        return Err(
            brush_core::Error::from(brush_core::ErrorKind::ExpandingUnsetVariable(
                name.to_owned(),
            ))
            .into_fatal(),
        );
    }
    // An error in an array subscript ends the shell, reported without `let`.
    if error.is_in_subscript() {
        return Err(brush_core::Error::from(error));
    }
    if error.is_readonly_variable() {
        writeln!(
            context.stderr(),
            "{}{error}",
            context.shell.diagnostic_prefix()
        )?;
    } else {
        context.report(EvalError::in_expression(expr, error))?;
    }
    Ok(ExecutionResult::general_error())
}
