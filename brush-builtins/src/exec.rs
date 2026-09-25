use clap::Parser;
#[cfg(unix)]
use std::{borrow::Cow, os::unix::process::CommandExt};

#[cfg(unix)]
use brush_core::{ErrorKind, ExecutionExitCode, commands};
use brush_core::{ExecutionResult, builtins};

/// Exec the provided command.
#[derive(Parser)]
pub(crate) struct ExecCommand {
    /// Pass given name as zeroth argument to command.
    #[arg(short = 'a', value_name = "NAME")]
    name_for_argv0: Option<String>,

    /// Exec command with an empty environment.
    #[arg(short = 'c')]
    empty_environment: bool,

    /// Exec command as a login shell.
    #[arg(short = 'l')]
    exec_as_login: bool,

    /// Command and args.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

impl builtins::Command for ExecCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        if self.args.is_empty() {
            // When no arguments are present, then there's nothing for us to execute -- but we need
            // to ensure that any redirections setup for this builtin get applied to the calling
            // shell instance.
            #[allow(clippy::needless_collect)]
            let fds: Vec<_> = context.iter_fds().collect();

            context.shell.replace_open_files(fds.into_iter());
            return Ok(ExecutionResult::success());
        }

        // If we know we're already running in a subshell, then `exec`ing is actually
        // unsafe, since it would also replace the *parent* shell instance. We instead
        // delegate to the `command` builtin to perform the execution, with an expectation
        // of returning.
        if context.shell.is_subshell() {
            if self.empty_environment || self.exec_as_login || self.name_for_argv0.is_some() {
                return brush_core::error::unimp("exec with options in subshell not yet supported");
            }

            let cmd_cmd = crate::command::CommandCommand {
                command_and_args: self.args.clone(),
                ..Default::default()
            };

            return cmd_cmd.execute(context).await;
        }

        // wasm32: there is no execve and no process image to replace. The command runs, and the
        // shell then ends with its status, as it would once replaced. The EXIT trap does not run:
        // it belonged to the shell that is gone.
        #[cfg(target_arch = "wasm32")]
        {
            if self.empty_environment || self.exec_as_login || self.name_for_argv0.is_some() {
                return brush_core::error::unimp("exec -a, -c and -l");
            }
            let command = crate::command::CommandCommand {
                command_and_args: self.args.clone(),
                ..Default::default()
            };
            // Bash's own wording for a target `exec` cannot find ("NAME: not found") differs
            // from the generic "command not found" the `command` builtin below produces for an
            // ordinary lookup failure. And unlike an ordinary command, a failed `exec` always
            // ends a non-interactive shell: there is no process image left for the rest of the
            // script to run in, exactly as there would be none after a successful one.
            let diagnostic_prefix = context.shell.diagnostic_prefix();
            let mut stderr = context.params.stderr(context.shell);
            let shell = context.shell;
            let inner = brush_core::ExecutionContext {
                shell: &mut *shell,
                command_name: context.command_name,
                params: context.params,
            };
            let mut result = match command.execute(inner).await {
                Ok(result) => result,
                Err(error) => {
                    use std::io::Write as _;
                    let (message, exit_code) = match error.kind() {
                        brush_core::ErrorKind::CommandNotFound(name) => (
                            std::format!("{name}: not found"),
                            brush_core::ExecutionExitCode::NotFound,
                        ),
                        _ => (
                            error.to_string(),
                            brush_core::ExecutionExitCode::from(&error),
                        ),
                    };
                    let _ = writeln!(stderr, "{diagnostic_prefix}exec: {message}");
                    brush_core::ExecutionResult::new(exit_code.into())
                }
            };
            shell
                .traps_mut()
                .remove_handlers(brush_core::traps::TrapSignal::Exit);
            result.next_control_flow = brush_core::ExecutionControlFlow::ExitShell;
            Ok(result)
        }

        #[cfg(unix)]
        {
            let mut argv0 = Cow::Borrowed(self.name_for_argv0.as_ref().unwrap_or(&self.args[0]));

            if self.exec_as_login {
                argv0 = Cow::Owned(std::format!("-{argv0}"));
            }

            let mut cmd = commands::compose_std_command(
                &context,
                &self.args[0],
                argv0.as_str(),
                &self.args[1..],
                self.empty_environment,
            )?;

            let exec_error = cmd.exec();

            if exec_error.kind() == std::io::ErrorKind::NotFound {
                Ok(ExecutionExitCode::NotFound.into())
            } else {
                Err(ErrorKind::from(exec_error).into())
            }
        }
    }
}
