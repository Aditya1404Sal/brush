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

    /// Command and args. An option before the command that is none of the above is an invalid
    /// one, as in bash.
    #[arg(trailing_var_arg = true)]
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

        // Native: if we know we're already running in a subshell, then `exec`ing is actually
        // unsafe, since it would also replace the *parent* shell instance (a real, separate OS
        // process on Unix). We instead delegate to the `command` builtin to perform the
        // execution, with an expectation of returning. This concern does not apply on wasm32
        // (see below): there is no real subshell process to protect either way, so ending the
        // subshell's own call frame is exactly what a real exec would have done to it too.
        #[cfg(not(target_arch = "wasm32"))]
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

        // wasm32: there is no execve and no process image to replace, in a subshell or not. The
        // command runs, and this call frame -- the whole shell, or just the subshell running it,
        // which is its own clone (`shell.clone()` in interp.rs) and so has its own trap set --
        // ends with its status, as replacing it would. The EXIT trap does not run: it belonged
        // to the frame that is gone.
        #[cfg(target_arch = "wasm32")]
        {
            if self.empty_environment || self.exec_as_login || self.name_for_argv0.is_some() {
                return brush_core::error::unimp("exec -a, -c and -l");
            }
            let command = crate::command::CommandCommand {
                command_and_args: self.args.clone(),
                ..Default::default()
            };
            // Bash's own wording for a target `exec` cannot find ("exec: NAME: not found")
            // differs from the generic "command not found" the `command` builtin below produces
            // for an ordinary lookup failure; a path it cannot run is named alone ("PATH: No such
            // file or directory"). A failed `exec` ends a non-interactive shell, unless `execfail`
            // is set and the shell is no subshell, as in bash.
            let diagnostic_prefix = context.shell.diagnostic_prefix();
            let mut stderr = context.params.stderr(context.shell);
            let shell = context.shell;
            // The program replaces the shell, which leaves its level as bash does.
            shell.exec_shell_level(&context.params, &self.args[0]);
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
                            std::format!("exec: {name}: not found"),
                            brush_core::ExecutionExitCode::NotFound,
                        ),
                        _ => (
                            error.to_string(),
                            brush_core::ExecutionExitCode::from(&error),
                        ),
                    };
                    let _ = writeln!(stderr, "{diagnostic_prefix}{message}");
                    let result = brush_core::ExecutionResult::new(exit_code.into());
                    if shell.options().exit_on_exec_fail && !shell.in_subshell_environment() {
                        return Ok(result);
                    }
                    result
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
