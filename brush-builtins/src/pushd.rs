use clap::Parser;

use brush_core::{ExecutionResult, builtins};

/// Push a path onto the current directory stack.
#[derive(Parser)]
pub(crate) struct PushdCommand {
    /// Push the path without changing the current working directory.
    #[clap(short = 'n')]
    no_directory_change: bool,

    /// Directory to push on the directory stack, or `+N`/`-N` to rotate the Nth entry, counting
    /// from the left or the right, to the top. Without it, the top two entries are exchanged.
    #[arg(allow_hyphen_values = true)]
    dir: Option<String>,
}

impl builtins::Command for PushdCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let dirs = crate::dirs::listed_dirs(context.shell);
        let rotation = match &self.dir {
            // Exchange the top two entries.
            None if dirs.len() < 2 => {
                context.report("no other directory")?;
                return Ok(ExecutionResult::general_error());
            }
            None => Some([vec![dirs[1].clone(), dirs[0].clone()], dirs[2..].to_vec()].concat()),
            Some(dir) => match crate::dirs::stack_position(dir, dirs.len()) {
                crate::dirs::StackPosition::At(position) => {
                    Some([&dirs[position..], &dirs[..position]].concat())
                }
                crate::dirs::StackPosition::OutOfRange if dirs.len() == 1 => {
                    context.report("directory stack empty")?;
                    return Ok(ExecutionResult::general_error());
                }
                crate::dirs::StackPosition::OutOfRange => {
                    context.report(format_args!("{dir}: directory stack index out of range"))?;
                    return Ok(ExecutionResult::general_error());
                }
                crate::dirs::StackPosition::NotAnIndex => None,
            },
        };

        if let Some(rotated) = rotation {
            if !self.no_directory_change
                && let Err(error) = context.shell.set_working_dir(&rotated[0])
            {
                context.report(format_args!(
                    "{}: {}",
                    rotated[0].display(),
                    error.path_reason()
                ))?;
                return Ok(ExecutionResult::general_error());
            }
            crate::dirs::set_stack(context.shell, &rotated[1..]);
        } else if let Some(dir) = &self.dir {
            if self.no_directory_change {
                context
                    .shell
                    .directory_stack_mut()
                    .push(std::path::PathBuf::from(dir));
            } else {
                let prev_working_dir = context.shell.working_dir().to_path_buf();
                if let Err(error) = context.shell.set_working_dir(std::path::Path::new(dir)) {
                    context.report(format_args!("{dir}: {}", error.path_reason()))?;
                    return Ok(ExecutionResult::general_error());
                }
                context.shell.directory_stack_mut().push(prev_working_dir);
            }
        }

        // Display dirs.
        let dirs_cmd = crate::dirs::DirsCommand::default();
        dirs_cmd.execute(context).await?;

        Ok(ExecutionResult::success())
    }
}
