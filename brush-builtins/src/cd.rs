use std::io::Write;
use std::path::PathBuf;

use clap::Parser;

use brush_core::{ExecutionResult, builtins, error};

/// Change the current shell working directory.
#[derive(Parser)]
pub(crate) struct CdCommand {
    /// Force following symlinks.
    #[arg(short = 'L', overrides_with = "use_physical_dir")]
    force_follow_symlinks: bool,

    /// Use physical dir structure without following symlinks.
    #[arg(short = 'P', overrides_with = "force_follow_symlinks")]
    use_physical_dir: bool,

    /// Exit with non zero exit status if current working directory resolution fails.
    #[arg(short = 'e')]
    exit_on_failed_cwd_resolution: bool,

    /// Show file with extended attributes as a dir with extended
    /// attributes.
    #[arg(short = '@')]
    file_with_xattr_as_dir: bool,

    /// By default it is the value of the HOME shell variable. If `TARGET_DIR` is "-", it is
    /// converted to $OLDPWD.
    #[arg(allow_hyphen_values = true)]
    target_dir: Option<String>,
}

impl builtins::Command for CdCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        // TODO(cd): implement 'cd -@'
        if self.file_with_xattr_as_dir {
            return error::unimp("cd -@");
        }

        let mut should_print = false;
        let mut target_dir = if let Some(target_dir) = &self.target_dir {
            // `cd -', equivalent to `cd $OLDPWD'
            if target_dir == "-" {
                should_print = true;
                let oldpwd = context
                    .shell
                    .env()
                    .get("OLDPWD")
                    .filter(|(_, var)| var.value().is_set())
                    .map(|(_, var)| var.value().to_cow_str(context.shell).to_string());
                match oldpwd {
                    // An empty OLDPWD names the current directory.
                    Some(oldpwd) if oldpwd.is_empty() => {
                        writeln!(context.stdout())?;
                        return Ok(ExecutionResult::success());
                    }
                    Some(oldpwd) => PathBuf::from(oldpwd),
                    None => {
                        context.report("OLDPWD not set")?;
                        return Ok(ExecutionResult::general_error());
                    }
                }
            } else if target_dir.is_empty() {
                context.report("null directory")?;
                return Ok(ExecutionResult::general_error());
            } else if let Some(found) = cdpath_directory(&context, target_dir) {
                // A directory found through a non-empty CDPATH entry is printed, as in bash.
                should_print = found.1;
                found.0
            } else {
                PathBuf::from(target_dir)
            }
        // `cd' without arguments is equivalent to `cd $HOME'
        } else {
            if let Some(home_var) = context.shell.env_str("HOME") {
                PathBuf::from(home_var.to_string())
            } else {
                context.report("HOME not set")?;
                return Ok(ExecutionResult::general_error());
            }
        };

        if self.use_physical_dir
            || context
                .shell
                .options()
                .do_not_resolve_symlinks_when_changing_dir
        {
            // -e is only relevant in physical mode.
            if self.exit_on_failed_cwd_resolution {
                return error::unimp("cd -e");
            }

            target_dir = context.shell.absolute_path(target_dir).canonicalize()?;
        }

        if let Err(error) = context.shell.set_working_dir(&target_dir) {
            // As bash words it: the operand as given, then the reason.
            let shown = self
                .target_dir
                .clone()
                .unwrap_or_else(|| target_dir.to_string_lossy().to_string());
            context.report(format_args!("{shown}: {}", error.path_reason()))?;
            return Ok(ExecutionResult::general_error());
        }

        // Bash compatibility
        // https://www.gnu.org/software/bash/manual/bash.html#index-cd
        // If a non-empty directory name from CDPATH is used, or if '-' is the first argument, and
        // the directory change is successful, the absolute pathname of the new working
        // directory is written to the standard output.
        if should_print {
            writeln!(
                context.stdout(),
                "{}",
                context.shell.working_dir().display()
            )?;
        }

        Ok(ExecutionResult::success())
    }
}

/// The directory `target` names through CDPATH, as bash searches it: only for a relative name
/// that does not start with `.` or `..`, each entry in turn (an empty one is the current
/// directory). Returns it, and whether a non-empty entry found it.
fn cdpath_directory(
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    target: &str,
) -> Option<(PathBuf, bool)> {
    let first = target.split('/').next().unwrap_or_default();
    if target.starts_with('/') || first == "." || first == ".." {
        return None;
    }
    let cdpath = context.shell.env_str("CDPATH")?;
    cdpath.split(':').find_map(|entry| {
        let candidate = if entry.is_empty() {
            PathBuf::from(target)
        } else {
            PathBuf::from(entry).join(target)
        };
        context
            .shell
            .absolute_path(&candidate)
            .is_dir()
            .then_some((candidate, !entry.is_empty()))
    })
}
