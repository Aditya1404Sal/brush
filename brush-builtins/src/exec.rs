use clap::Parser;
#[cfg(unix)]
use std::{borrow::Cow, os::unix::process::CommandExt};

use brush_core::{ExecutionResult, builtins};
#[cfg(unix)]
use brush_core::{ErrorKind, ExecutionExitCode, commands};

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

        // wasm32: there is no execve and no real process image to replace. Emulate the observable
        // semantics — run the command in-shell, then exit the shell with its status — via the same
        // `command` delegation the subshell path uses, followed by an ExitShell control flow.
        #[cfg(target_arch = "wasm32")]
        {
            if self.empty_environment || self.exec_as_login || self.name_for_argv0.is_some() {
                return brush_core::error::unimp("exec options are not supported on this platform");
            }

            let cmd_cmd = crate::command::CommandCommand {
                command_and_args: self.args.clone(),
                ..Default::default()
            };

            let mut result = cmd_cmd.execute(context).await?;
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
