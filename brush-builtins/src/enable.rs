use brush_core::ExecutionResult;
use clap::Parser;
use itertools::Itertools;
use std::io::Write;

use brush_core::builtins;
use brush_core::error;

/// Enable, disable, or display built-in commands.
#[derive(Parser)]
pub(crate) struct EnableCommand {
    /// Print a list of built-in commands.
    #[arg(short = 'a')]
    print_list: bool,

    /// Disables the specified built-in commands.
    #[arg(short = 'n')]
    disable: bool,

    /// Print a list of built-in commands with reusable output.
    #[arg(short = 'p')]
    print_reusably: bool,

    /// Only operate on special built-in commands.
    #[arg(short = 's')]
    special_only: bool,

    /// Path to a shared object from which built-in commands will be loaded.
    #[arg(short = 'f', value_name = "PATH")]
    shared_object_path: Option<String>,

    /// Remove the built-in commands loaded from the indicated object path.
    #[arg(short = 'd')]
    remove_loaded_builtin: bool,

    /// Names of built-in commands to operate on.
    names: Vec<String>,
}

impl builtins::Command for EnableCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let mut result = ExecutionResult::success();

        if self.shared_object_path.is_some() {
            return error::unimp("enable -f");
        }
        // No builtin here was loaded from a shared object, so none can be unloaded; bash says
        // which of the names is a builtin at all.
        if self.remove_loaded_builtin && !self.names.is_empty() {
            for name in &self.names {
                let builtin = !context.shell.is_file_program(name)
                    && context.shell.builtins().contains_key(name);
                if builtin {
                    context.report(format_args!("{name}: not dynamically loaded"))?;
                } else {
                    context.report(format_args!("{name}: not a shell builtin"))?;
                }
            }
            return Ok(ExecutionResult::general_error());
        }

        // With -p, bash lists the builtins and does not act on the names.
        if !self.names.is_empty() && !self.print_reusably {
            for name in &self.names {
                // A builtin that stands for a program bash has no builtin for (`cat`) is not a
                // shell builtin to enable or disable.
                if context.shell.is_file_program(name) {
                    context.report(format_args!("{name}: not a shell builtin"))?;
                    result = ExecutionResult::general_error();
                } else if let Some(builtin) = context.shell.builtin_mut(name) {
                    builtin.disabled = self.disable;
                } else {
                    context.report(format_args!("{name}: not a shell builtin"))?;
                    result = ExecutionResult::general_error();
                }
            }
        } else {
            let builtins: Vec<_> = context
                .shell
                .builtins()
                .iter()
                .filter(|(name, _reg)| !context.shell.is_file_program(name))
                .sorted_by_key(|(name, _reg)| *name)
                .collect();

            for (builtin_name, builtin) in builtins {
                if self.disable {
                    if !builtin.disabled {
                        continue;
                    }
                } else if self.print_list {
                    if builtin.disabled {
                        continue;
                    }
                }

                if self.special_only && !builtin.special_builtin {
                    continue;
                }

                let prefix = if builtin.disabled { "-n " } else { "" };

                writeln!(context.stdout(), "enable {prefix}{builtin_name}")?;
            }
        }

        Ok(result)
    }
}
