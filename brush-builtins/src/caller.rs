use brush_core::{ExecutionResult, builtins};
use clap::Parser;
use std::io::Write;

/// Return the context of the current subroutine call.
#[derive(Parser)]
pub(crate) struct CallerCommand {
    /// The number of call frames to go back.
    expr: Option<usize>,
}

impl builtins::Command for CallerCommand {
    type Error = brush_core::Error;

    /// As bash's `caller`: from `BASH_LINENO`, `BASH_SOURCE` and `FUNCNAME`, `LINE FILE` for the
    /// current call (`NULL` for a file that is not known) or `LINE FUNCTION FILE` for frame N.
    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let element = |name: &str, index: usize| -> Option<String> {
            let (_, var) = context.shell.env().get(name)?;
            var.value()
                .get_at(&index.to_string(), context.shell)
                .ok()
                .flatten()
                .map(|value| value.into_owned())
        };

        // Outside any function or sourced file there is no caller.
        let (Some(line), Some(_)) = (element("BASH_LINENO", 0), element("BASH_SOURCE", 0)) else {
            return Ok(ExecutionResult::general_error());
        };

        let Some(frame) = self.expr else {
            let file = element("BASH_SOURCE", 1).unwrap_or_else(|| "NULL".to_owned());
            writeln!(context.stdout(), "{line} {file}")?;
            return Ok(ExecutionResult::success());
        };

        let (Some(line), Some(file), Some(function)) = (
            element("BASH_LINENO", frame),
            element("BASH_SOURCE", frame + 1),
            element("FUNCNAME", frame + 1),
        ) else {
            return Ok(ExecutionResult::general_error());
        };
        writeln!(context.stdout(), "{line} {function} {file}")?;

        Ok(ExecutionResult::success())
    }
}
