use clap::Parser;

use brush_core::{ExecutionResult, builtins};

/// Pop a path from the current directory stack.
#[derive(Parser)]
pub(crate) struct PopdCommand {
    /// Pop the path without changing the current working directory.
    #[clap(short = 'n')]
    no_directory_change: bool,

    /// Remove the Nth entry, counting from the left (`+N`) or the right (`-N`).
    #[arg(allow_hyphen_values = true)]
    entry: Option<String>,
}

impl builtins::Command for PopdCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let mut dirs = crate::dirs::listed_dirs(context.shell);
        if dirs.len() == 1 {
            context.report("directory stack empty")?;
            return Ok(ExecutionResult::general_error());
        }
        let position = match &self.entry {
            None => 0,
            Some(entry) => match crate::dirs::stack_position(entry, dirs.len()) {
                crate::dirs::StackPosition::At(position) => position,
                crate::dirs::StackPosition::OutOfRange => {
                    context.report(format_args!("{entry}: directory stack index out of range"))?;
                    return Ok(ExecutionResult::general_error());
                }
                crate::dirs::StackPosition::NotAnIndex => {
                    context.report(format_args!("{entry}: invalid argument"))?;
                    return Ok(ExecutionResult::general_error());
                }
            },
        };

        // Removing the current directory changes to the next one, unless -n.
        dirs.remove(position);
        if position == 0 && !self.no_directory_change {
            context.shell.set_working_dir(&dirs[0])?;
        }
        let rest = if position == 0 && self.no_directory_change {
            &dirs[..]
        } else {
            &dirs[1..]
        };
        crate::dirs::set_stack(context.shell, rest);

        // Display dirs.
        let dirs_cmd = crate::dirs::DirsCommand::default();
        dirs_cmd.execute(context).await?;
        Ok(ExecutionResult::success())
    }
}
