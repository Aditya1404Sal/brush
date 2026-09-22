//! Trap handling for the shell.

use crate::{ExecutionParameters, ExecutionResult, ProcessGroupPolicy, error, traps::TrapSignal};

impl<SE: crate::extensions::ShellExtensions> crate::Shell<SE> {
    /// Runs any exit steps for the shell.
    ///
    /// This currently includes invoking the `EXIT` trap handler, if any.
    pub async fn on_exit(&mut self) -> Result<(), error::Error> {
        self.run_exit_trap().await.map(|_| ())
    }

    /// Runs the `EXIT` trap handler, if any, and returns its result.
    pub async fn run_exit_trap(&mut self) -> Result<ExecutionResult, error::Error> {
        if self.traps.handles(TrapSignal::Exit) {
            self.invoke_trap_handler(TrapSignal::Exit, &self.default_exec_params())
                .await
        } else {
            Ok(ExecutionResult::success())
        }
    }

    /// Runs the `EXIT` trap as the shell ends with `result`. As in bash, an `exit` inside the
    /// trap sets the final status; otherwise `result` stands.
    ///
    /// # Arguments
    ///
    /// * `result`: The result of the commands the shell ran before ending.
    pub async fn exit_with_trap(
        &mut self,
        result: Result<ExecutionResult, error::Error>,
    ) -> Result<ExecutionResult, error::Error> {
        match self.run_exit_trap().await {
            Ok(trap) if trap.is_exit() => Ok(trap),
            _ => result,
        }
    }

    /// Invokes the handler registered for `signal`, if any.
    ///
    /// Behavior varies by signal type:
    ///
    /// * **Per-signal recursion guard** — each trap guards against its own self-recursion, but
    ///   different traps *can* fire from within each other's handlers (matching bash semantics).
    ///
    /// * **Inheritance** — in functions and subshells, some traps are only inherited when the
    ///   corresponding shell option is enabled (e.g. `errtrace` / `set -E` for `ERR`, `functrace` /
    ///   `set -T` for `DEBUG`/`RETURN`).
    ///
    /// * **`$?` preservation** — `last_exit_status` is saved before and restored after the handler
    ///   runs so the trap does not clobber the status that triggered it.
    ///
    /// # Arguments
    ///
    /// * `signal`: Signal to run handler for.
    ///
    /// * `params`: Execution parameters to use for handler.
    pub(crate) async fn invoke_trap_handler(
        &mut self,
        signal: TrapSignal,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // Per-signal self-recursion guard: don't re-enter a trap that is
        // already being handled. Different traps *can* fire from each
        // other's handlers (e.g. ERR inside EXIT, EXIT inside ERR).
        if self.call_stack().is_trap_signal_active(signal) {
            return Ok(ExecutionResult::success());
        }

        // Don't fire traps that have been explicitly suppressed (e.g. DEBUG
        // during programmable completion).
        if self.call_stack().is_trap_delivery_suppressed() {
            return Ok(ExecutionResult::success());
        }

        // In functions and subshells, some traps are only inherited when the
        // corresponding option is enabled.
        if (self.in_function() || self.is_subshell())
            && !self.is_trap_inherited_in_current_scope(signal)
        {
            return Ok(ExecutionResult::success());
        }

        let Some(handler) = self.traps.get_effective_handler(signal).cloned() else {
            return Ok(ExecutionResult::success());
        };

        let mut params = params.clone();
        params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;

        // Preserve $? across trap handler execution so the handler doesn't
        // clobber the status that triggered it.
        let orig_last_exit_status = self.last_exit_status;

        self.enter_trap_handler(signal, Some(&handler));
        #[cfg(any(target_arch = "wasm32", test))]
        {
            let mut frame = super::callstack::FrameGuard::new(
                self,
                |shell| {
                    shell.leave_trap_handler();
                    Ok(())
                },
                Some(orig_last_exit_status),
            );
            frame
                .shell()
                .run_string(&handler.command, &handler.source_info, &params)
                .await
        }
        #[cfg(not(any(target_arch = "wasm32", test)))]
        {
            let result = self
                .run_string(&handler.command, &handler.source_info, &params)
                .await;
            self.leave_trap_handler();
            self.last_exit_status = orig_last_exit_status;
            result
        }
    }

    /// Returns whether the given trap signal is inherited in the current
    /// function or subshell scope.
    fn is_trap_inherited_in_current_scope(&self, signal: TrapSignal) -> bool {
        match signal {
            TrapSignal::Err => self.options().shell_functions_inherit_err_trap,
            TrapSignal::Debug | TrapSignal::Return => {
                self.options()
                    .shell_functions_inherit_debug_and_return_traps
            }
            // EXIT and system signals are always inherited — i.e. their visibility is
            // not gated by errtrace/functrace options. (The actual trap *state* for
            // subshells is managed separately via `Shell::clone`.)
            TrapSignal::Exit | TrapSignal::Signal(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{ExecutionResult, Shell, builtins, execution::process, traps::PipeDisposition};

    #[test]
    fn terminating_pipe_handler_releases_frame_before_shell_reuse() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokio::task::LocalSet::new().run_until(async {
                let mut shell: Shell = Shell::new(crate::CreateOptions::default()).unwrap();
                shell.register_builtin(
                    "reset-and-fail",
                    builtins::Registration {
                        execute_func: |context, _| {
                            Box::pin(async move {
                                if context.shell.in_function() {
                                    context.shell.env_mut().add(
                                        "local_marker",
                                        crate::ShellVariable::new("must not leak"),
                                        crate::env::EnvironmentScope::Local,
                                    )?;
                                }
                                context.shell.traps_mut().remove_handlers("PIPE".parse()?);
                                process::set_pipe_disposition(PipeDisposition::Default);
                                let (reader, mut writer) = crate::openfiles::test_pipe(1);
                                drop(reader);
                                assert_eq!(
                                    std::io::Write::write(&mut writer, b"x").unwrap_err().kind(),
                                    std::io::ErrorKind::BrokenPipe,
                                );
                                futures::future::pending::<()>().await;
                                Ok(ExecutionResult::success())
                            })
                        },
                        content_func: |_, _, _| Ok(String::new()),
                        disabled: false,
                        special_builtin: false,
                        declaration_builtin: false,
                        execution_boundary: builtins::ExecutionBoundary::Caller,
                    },
                );
                let signal = "PIPE".parse().unwrap();
                shell.traps_mut().register_handler(
                    signal,
                    "reset-and-fail".to_owned(),
                    "test".into(),
                );
                let params = shell.default_exec_params();
                shell.set_last_exit_status(1);
                let result = process::run_process(
                    PipeDisposition::Caught,
                    shell.invoke_trap_handler(signal, &params),
                )
                .await
                .unwrap();
                assert_eq!(result.terminating_signal, Some(13));
                assert!(!shell.call_stack().is_trap_signal_active(signal));
                assert!(shell.call_stack().is_empty());
                assert_eq!(shell.last_exit_status(), 1);

                // Reuse the same shell through a function boundary. Cancellation must also
                // remove its local environment and positional arguments.
                let result = process::run_process(
                    PipeDisposition::Default,
                    shell.run_string("f() { reset-and-fail; }; f nested", &"test".into(), &params),
                )
                .await
                .unwrap();
                assert_eq!(result.terminating_signal, Some(13));
                assert!(!shell.in_function());
                assert!(shell.env().get("local_marker").is_none());
                assert!(shell.current_shell_args().is_empty());
                assert!(shell.call_stack().is_empty());

                let script = tempfile::NamedTempFile::new().unwrap();
                std::fs::write(script.path(), b"reset-and-fail\n").unwrap();
                let result = process::run_process(
                    PipeDisposition::Default,
                    shell.source_script(script.path(), std::iter::once("nested"), &params),
                )
                .await
                .unwrap();
                assert_eq!(result.terminating_signal, Some(13));
                assert!(!shell.in_sourced_script());
                assert!(shell.current_shell_args().is_empty());
                assert!(shell.call_stack().is_empty());
            }));
    }
}
