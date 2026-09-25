use clap::Parser;
use std::{io::Write, path::PathBuf};

use brush_core::{ExecutionResult, builtins};

#[derive(Parser)]
pub(crate) struct HashCommand {
    /// Remove entries associated with the given names.
    #[arg(short = 'd')]
    remove: bool,

    /// Display paths in a format usable for input.
    #[arg(short = 'l')]
    display_as_usable_input: bool,

    /// The path to associate with the names.
    #[arg(short = 'p', value_name = "PATH")]
    path_to_use: Option<PathBuf>,

    /// Remove all entries.
    #[arg(short = 'r')]
    remove_all: bool,

    /// Display the paths associated with the names.
    #[arg(short = 't')]
    display_paths: bool,

    /// Names to process.
    names: Vec<String>,
}

impl builtins::Command for HashCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let mut result = ExecutionResult::success();

        if self.remove_all {
            context.shell.program_location_cache_mut().reset();
        } else if self.remove {
            for name in &self.names {
                if !context.shell.program_location_cache_mut().unset(name) {
                    context.report(format_args!("{name}: not found"))?;
                    result = ExecutionResult::general_error();
                }
            }
        } else if self.display_paths {
            for name in &self.names {
                if let Some(path) = context.shell.program_location_cache().get(name) {
                    if self.display_as_usable_input {
                        writeln!(
                            context.stdout(),
                            "builtin hash -p {} {name}",
                            path.to_string_lossy()
                        )?;
                    } else {
                        let mut prefix = String::new();

                        if self.names.len() > 1 {
                            prefix.push_str(name.as_str());
                            prefix.push('\t');
                        }

                        writeln!(
                            context.stdout(),
                            "{prefix}{}",
                            path.to_string_lossy().as_ref()
                        )?;
                    }
                } else {
                    context.report(format_args!("{name}: not found"))?;
                    result = ExecutionResult::general_error();
                }
            }
        } else if let Some(path) = &self.path_to_use {
            // The shell's working directory is its own state, not the process's, so a
            // relative path is resolved against it before it's inspected -- but reported as given.
            let is_dir = context.shell.absolute_path(path).is_dir();

            for name in &self.names {
                if is_dir {
                    context.report(format_args!("{}: Is a directory", path.display()))?;
                    result = ExecutionResult::general_error();
                    continue;
                }

                context
                    .shell
                    .program_location_cache_mut()
                    .set(name, path.clone());
            }
        } else if self.names.is_empty() {
            // With no names, bash lists the table. Its hit counts start at zero here: commands
            // hashed without being run from a path have none.
            let cache = context.shell.program_location_cache();
            if cache.is_empty() {
                writeln!(context.stdout(), "hash: hash table empty")?;
            } else {
                writeln!(context.stdout(), "hits\tcommand")?;
                for (_, path) in cache.entries() {
                    writeln!(context.stdout(), "   0\t{}", path.display())?;
                }
            }
        } else {
            for name in &self.names {
                // Remove from the cache if already hashed.
                let _ = context.shell.program_location_cache_mut().unset(name);

                // Names with slashes are accepted silently
                if name.contains('/') {
                    continue;
                }

                // As in bash, a function or a builtin is not hashed, and not an error; a builtin
                // that stands for a program bash has no builtin for is hashed at its file.
                if context.shell.funcs().get(name).is_some() {
                    continue;
                }
                if let Some(path) = context.shell.program_file(name) {
                    context.shell.program_location_cache_mut().set(name, path);
                    continue;
                }
                if context
                    .shell
                    .builtins()
                    .get(name)
                    .is_some_and(|registration| !registration.disabled)
                {
                    continue;
                }

                // Hash the path
                if context
                    .shell
                    .find_first_executable_in_path_using_cache(name)
                    .is_none()
                {
                    context.report(format_args!("{name}: not found"))?;
                    result = ExecutionResult::general_error();
                }
            }
        }

        Ok(result)
    }
}
