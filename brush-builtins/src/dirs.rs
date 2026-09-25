use clap::Parser;
use std::io::Write;

use brush_core::{ExecutionResult, builtins};

/// Manage the current directory stack.
#[derive(Default, Parser)]
pub(crate) struct DirsCommand {
    /// Clear the directory stack.
    #[arg(short = 'c')]
    clear: bool,

    /// Don't tilde-shorten paths.
    #[arg(short = 'l')]
    tilde_long: bool,

    /// Print one directory per line instead of all on one line.
    #[arg(short = 'p')]
    print_one_per_line: bool,

    /// Print one directory per line with its index.
    #[arg(short = 'v')]
    print_one_per_line_with_index: bool,

    /// Show only the Nth entry, counting from the left (`+N`) or the right (`-N`).
    #[arg(allow_hyphen_values = true)]
    entry: Option<String>,

    /// More operands, each of which bash also checks.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    extra: Vec<String>,
}

/// The directories as `dirs` lists them: the current directory, then the stack, newest first.
pub(crate) fn listed_dirs(
    shell: &brush_core::Shell<impl brush_core::ShellExtensions>,
) -> Vec<std::path::PathBuf> {
    std::iter::once(shell.working_dir().to_path_buf())
        .chain(shell.directory_stack().iter().rev().cloned())
        .collect()
}

/// Where `+N` or `-N` points in a list of directories.
pub(crate) enum StackPosition {
    /// The operand is not `+N` or `-N`.
    NotAnIndex,
    /// It points past the list.
    OutOfRange,
    /// It points at this entry.
    At(usize),
}

/// Where `+N` or `-N` points in a list of `len` directories, counting from the left or the
/// right as bash does.
pub(crate) fn stack_position(operand: &str, len: usize) -> StackPosition {
    let (from_right, digits) = match operand.split_at_checked(1) {
        Some(("+", digits)) => (false, digits),
        Some(("-", digits)) => (true, digits),
        _ => return StackPosition::NotAnIndex,
    };
    let Ok(n) = digits.parse::<usize>() else {
        return StackPosition::NotAnIndex;
    };
    if n >= len {
        StackPosition::OutOfRange
    } else if from_right {
        StackPosition::At(len - 1 - n)
    } else {
        StackPosition::At(n)
    }
}

/// Makes the stack hold `dirs` after the current directory (listed newest first).
pub(crate) fn set_stack(
    shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>,
    dirs: &[std::path::PathBuf],
) {
    let stack = shell.directory_stack_mut();
    stack.clear();
    stack.extend(dirs.iter().rev().cloned());
}

impl builtins::Command for DirsCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // As in bash: an operand that is not `+N` or `-N` is an invalid option.
        if let Some(entry) =
            self.entry.iter().chain(&self.extra).find(|entry| {
                matches!(stack_position(entry, usize::MAX), StackPosition::NotAnIndex)
            })
        {
            context.report(format_args!("{entry}: invalid option"))?;
            writeln!(context.stderr(), "dirs: usage: dirs [-clpv] [+N] [-N]")?;
            return Ok(ExecutionResult::new(2));
        }
        if self.clear {
            context.shell.directory_stack_mut().clear();
        } else if let Some(entry) = &self.entry {
            let dirs = listed_dirs(context.shell);
            match stack_position(entry, dirs.len()) {
                StackPosition::At(position) => {
                    let mut dir_str = dirs[position].to_string_lossy().to_string();
                    if !self.tilde_long {
                        dir_str = context.shell.tilde_shorten(dir_str);
                    }
                    writeln!(context.stdout(), "{dir_str}")?;
                }
                _ if dirs.len() == 1 => {
                    context.report("directory stack empty")?;
                    return Ok(ExecutionResult::general_error());
                }
                StackPosition::OutOfRange => {
                    let shown = entry.strip_prefix('+').unwrap_or(entry);
                    context.report(format_args!("{shown}: directory stack index out of range"))?;
                    return Ok(ExecutionResult::general_error());
                }
                StackPosition::NotAnIndex => {
                    context.report(format_args!("{entry}: invalid argument"))?;
                    return Ok(ExecutionResult::general_error());
                }
            }
        } else {
            let dirs = vec![context.shell.working_dir()]
                .into_iter()
                .chain(
                    context
                        .shell
                        .directory_stack()
                        .iter()
                        .rev()
                        .map(|p| p.as_path()),
                )
                .collect::<Vec<_>>();

            let one_per_line = self.print_one_per_line || self.print_one_per_line_with_index;

            for (i, dir) in dirs.iter().enumerate() {
                if !one_per_line && i > 0 {
                    write!(context.stdout(), " ")?;
                }

                if self.print_one_per_line_with_index {
                    write!(context.stdout(), "{i:2}  ")?;
                }

                let mut dir_str = dir.to_string_lossy().to_string();

                if !self.tilde_long {
                    dir_str = context.shell.tilde_shorten(dir_str);
                }

                write!(context.stdout(), "{dir_str}")?;

                if one_per_line || i == dirs.len() - 1 {
                    writeln!(context.stdout())?;
                }
            }

            return Ok(ExecutionResult::success());
        }

        Ok(ExecutionResult::success())
    }
}
