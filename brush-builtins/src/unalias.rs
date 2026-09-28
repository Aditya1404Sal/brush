use clap::Parser;
use std::io::Write;

use brush_core::{ExecutionResult, builtins};

/// Unset a shell alias.
#[derive(Parser)]
pub(crate) struct UnaliasCommand {
    /// Remove all aliases.
    #[arg(short = 'a')]
    remove_all: bool,

    /// Names of aliases to operate on.
    aliases: Vec<String>,
}

impl builtins::Command for UnaliasCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let mut exit_code = ExecutionResult::success();

        // Nothing to remove is a usage error, which bash reports with its usage line alone.
        if !self.remove_all && self.aliases.is_empty() {
            writeln!(
                context.stderr(),
                "unalias: usage: unalias [-a] name [name ...]"
            )?;
            return Ok(ExecutionResult::new(2));
        }

        if self.remove_all {
            context.shell.aliases_mut().clear();
        } else {
            for alias in &self.aliases {
                if context.shell.aliases_mut().remove(alias).is_none() {
                    context.report(format_args!("{alias}: not found"))?;
                    exit_code = ExecutionResult::general_error();
                }
            }
        }

        Ok(exit_code)
    }
}
