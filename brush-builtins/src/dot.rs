use std::io::Write as _;
use std::path::Path;

use brush_core::builtins;
use clap::Parser;

/// Evaluate the provided script in the current shell environment.
#[derive(Parser)]
pub(crate) struct DotCommand {
    /// Path to the script to evaluate.
    script_path: Option<String>,

    /// Any arguments to be passed as positional parameters to the script.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    script_args: Vec<String>,
}

impl builtins::Command for DotCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // As bash words it when no file is named.
        let Some(script_path) = &self.script_path else {
            let name = &context.command_name;
            context.report("filename argument required")?;
            writeln!(
                context.stderr(),
                "{name}: usage: {name} [-p path] filename [arguments]"
            )?;
            return Ok(brush_core::ExecutionResult::new(2));
        };

        // TODO(dot): Handle trap inheritance.
        let result = context
            .shell
            .source_script(
                Path::new(script_path),
                self.script_args.iter(),
                &context.params,
            )
            .await;
        match result {
            // Bash reports a script it cannot read without naming `source`, and carries on.
            Err(error) if matches!(error.kind(), brush_core::ErrorKind::FailedSourcingFile(..)) => {
                context.shell.display_error(&mut context.stderr(), &error)?;
                Ok(brush_core::ExecutionResult::general_error())
            }
            result => result,
        }
    }
}
