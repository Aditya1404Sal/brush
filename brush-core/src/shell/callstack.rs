//! Call stack management for the shell.

use crate::{ExecutionParameters, callstack, env, error, functions, trace_categories};

/// How many programs and lists may nest (see `Shell::nesting`). Each level costs up to about
/// 2.3 KiB of Wasmtime's 512 KiB native stack, which the component cannot observe, so this count
/// is what keeps that stack from running out; 160 leaves room for the commands the innermost
/// list runs.
#[cfg(target_arch = "wasm32")]
pub(crate) const MAX_NESTING: usize = 160;

/// Shadow stack a function call leaves for everything below it: the deepest command it may run
/// without calling further. Above [`STACK_RESERVE`], so a recursing function reports the function.
#[cfg(target_arch = "wasm32")]
const FUNCTION_STACK_RESERVE: usize = 320 * 1024;

/// Shadow stack any nested execution leaves for the commands it runs (see `sys::wasm::stack`).
#[cfg(target_arch = "wasm32")]
pub(crate) const STACK_RESERVE: usize = 256 * 1024;

#[cfg(any(target_arch = "wasm32", test))]
type FrameCleanup<SE> = fn(&mut crate::Shell<SE>) -> Result<(), error::Error>;

/// Owns a borrowed shell frame across an await, including cancellation by a logical process.
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) struct FrameGuard<'a, SE: crate::extensions::ShellExtensions> {
    shell: &'a mut crate::Shell<SE>,
    cleanup: Option<FrameCleanup<SE>>,
    restore_status: Option<u8>,
}

#[cfg(any(target_arch = "wasm32", test))]
impl<'a, SE: crate::extensions::ShellExtensions> FrameGuard<'a, SE> {
    pub(crate) fn new(
        shell: &'a mut crate::Shell<SE>,
        cleanup: FrameCleanup<SE>,
        restore_status: Option<u8>,
    ) -> Self {
        Self {
            shell,
            cleanup: Some(cleanup),
            restore_status,
        }
    }

    pub(crate) const fn shell(&mut self) -> &mut crate::Shell<SE> {
        self.shell
    }

    pub(crate) fn finish(mut self) -> Result<(), error::Error> {
        self.release()
    }

    fn release(&mut self) -> Result<(), error::Error> {
        let result = self
            .cleanup
            .take()
            .map_or(Ok(()), |cleanup| cleanup(self.shell));
        if let Some(status) = self.restore_status.take() {
            self.shell.last_exit_status = status;
        }
        result
    }
}

#[cfg(any(target_arch = "wasm32", test))]
impl<SE: crate::extensions::ShellExtensions> Drop for FrameGuard<'_, SE> {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

impl<SE: crate::extensions::ShellExtensions> crate::Shell<SE> {
    /// Returns whether or not the shell is actively executing in a sourced script.
    pub fn in_sourced_script(&self) -> bool {
        self.call_stack.in_sourced_script()
    }

    /// Returns whether or not the shell is actively executing in a shell function.
    pub fn in_function(&self) -> bool {
        self.call_stack.in_function()
    }

    /// Updates the shell's internal tracking state to reflect that a new interactive
    /// session is being started.
    pub fn start_interactive_session(&mut self) -> Result<(), error::Error> {
        self.call_stack.push_interactive_session();
        Ok(())
    }

    /// Updates the shell's internal tracking state to reflect that the current
    /// interactive session is ending.
    pub fn end_interactive_session(&mut self) -> Result<(), error::Error> {
        if self
            .call_stack
            .current_frame()
            .is_none_or(|frame| !frame.frame_type.is_interactive_session())
        {
            return Err(error::ErrorKind::NotInInteractiveSession.into());
        }

        self.call_stack.pop();

        Ok(())
    }

    /// Updates the shell's internal tracking state to reflect that command
    /// string mode is being started.
    ///
    /// A shell running a command string is a shell process of its own, as `bash -c` is: it is
    /// in no subshell (`BASH_SUBSHELL` is 0), xtrace starts at PS4's own level, and an error
    /// that ends it exits with `bash -c`'s status.
    pub fn start_command_string_mode(&mut self) {
        let name = self.name.clone().unwrap_or_else(|| "bash".to_owned());
        self.call_stack.push_command_string(&name);
        self.process_depth = self.depth;
        self.subshell_level = 0;
        self.trace_level = 0;
        self.exit_trace_level = 0;
    }

    /// Updates the shell's internal tracking state to reflect that command
    /// string mode is ending.
    pub fn end_command_string_mode(&mut self) -> Result<(), error::Error> {
        if self
            .call_stack
            .current_frame()
            .is_none_or(|frame| !frame.frame_type.is_command_string())
        {
            return Err(error::ErrorKind::NotExecutingCommandString.into());
        }

        self.call_stack.pop();

        Ok(())
    }

    pub(crate) fn enter_trap_handler(
        &mut self,
        signal: crate::traps::TrapSignal,
        handler: Option<&crate::traps::TrapHandler>,
    ) {
        self.call_stack.push_trap_handler(signal, handler);
    }

    pub(crate) fn leave_trap_handler(&mut self) {
        self.call_stack.pop();
    }

    /// Acquires a block on trap delivery, preventing traps from being delivered until
    /// the block is released. Multiple blocks may be acquired, and trap delivery will
    /// remain suppressed until all blocks have been released.
    pub(crate) const fn acquire_trap_delivery_block(&mut self) {
        self.call_stack.acquire_trap_delivery_block();
    }

    /// Releases a block on trap delivery; note that trap delivery will remain
    /// suppressed until all blocks have been released.
    pub(crate) const fn release_trap_delivery_block(&mut self) {
        self.call_stack.release_trap_delivery_block();
    }

    /// Updates the shell's internal tracking state to reflect that a new shell
    /// function is being entered.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the function being entered.
    /// * `function` - The function being entered.
    /// * `args` - The arguments being passed to the function.
    /// * `_params` - Current execution parameters.
    pub(crate) fn enter_function(
        &mut self,
        name: &str,
        function: &functions::Registration,
        args: impl IntoIterator<Item = String>,
        _params: &ExecutionParameters,
    ) -> Result<(), error::Error> {
        // As bash: `FUNCNEST`, when a positive number, limits the nesting, and exceeding it ends
        // a non-interactive shell.
        let depth = self.call_stack.function_call_depth();
        let funcnest = self
            .env
            .get_str("FUNCNEST", self)
            .and_then(|value| value.trim().parse::<i64>().ok())
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0);
        if let Some(max_call_depth) = funcnest.or(self.options.max_function_call_depth)
            && depth >= max_call_depth
        {
            let kind = error::ErrorKind::MaxFunctionCallDepthExceeded(name.to_owned(), depth);
            return Err(error::Error::from(kind).into_fatal());
        }
        // A call the stack cannot hold ends the shell the same way, rather than trapping.
        // A few lists short of the limit, so a recursing function is reported by name before its
        // body's compound commands reach it.
        #[cfg(target_arch = "wasm32")]
        if self.nesting + 8 >= MAX_NESTING
            || crate::sys::wasm::stack::remaining() < FUNCTION_STACK_RESERVE
        {
            let kind = error::ErrorKind::FunctionNestingTooDeep(name.to_owned(), depth);
            return Err(error::Error::from(kind).into_fatal());
        }

        if tracing::enabled!(target: trace_categories::FUNCTIONS, tracing::Level::DEBUG) {
            let depth = self.call_stack.function_call_depth();
            let prefix = repeated_char_str(' ', depth);
            tracing::debug!(target: trace_categories::FUNCTIONS, "Entering func [depth={depth}]: {prefix}{name}");
        }

        self.call_stack.push_function(name, function, args);
        self.env.push_scope(env::EnvironmentScope::Local);

        Ok(())
    }

    /// Updates the shell's internal tracking state to reflect that the shell
    /// has exited the top-most function on its call stack.
    pub(crate) fn leave_function(&mut self) -> Result<(), error::Error> {
        self.env.pop_scope(env::EnvironmentScope::Local)?;

        if let Some(exited_call) = self.call_stack.pop() {
            if let callstack::FrameType::Function(func_call) = exited_call.frame_type {
                if tracing::enabled!(target: trace_categories::FUNCTIONS, tracing::Level::DEBUG) {
                    let depth = self.call_stack.function_call_depth();
                    let prefix = repeated_char_str(' ', depth);
                    tracing::debug!(target: trace_categories::FUNCTIONS, "Exiting func  [depth={depth}]: {prefix}{}", func_call.function_name);
                }
            } else {
                let err: error::Error =
                    error::ErrorKind::InternalError("mismatched call stack state".to_owned())
                        .into();
                return Err(err.into_fatal());
            }
        }

        Ok(())
    }

    /// Returns the *current* positional arguments for the shell ($1 and beyond).
    /// Influenced by the current call stack.
    pub fn current_shell_args(&self) -> &[String] {
        for frame in self.call_stack.iter() {
            match frame.frame_type {
                // Function calls always shadow positional parameters.
                crate::callstack::FrameType::Function(..) => return &frame.args,
                // Executed scripts always shadow positional parameters.
                _ if frame.frame_type.is_run_script() => return &frame.args,
                // Sourced scripts shadow positional parameters if they have arguments.
                _ if frame.frame_type.is_sourced_script() && !frame.args.is_empty() => {
                    return &frame.args;
                }
                _ => (),
            }
        }

        self.args.as_slice()
    }

    /// Returns a mutable reference to *current* positional parameters for the shell
    /// ($1 and beyond).
    pub fn current_shell_args_mut(&mut self) -> &mut Vec<String> {
        for frame in self.call_stack.iter_mut() {
            match frame.frame_type {
                // Function calls always shadow positional parameters.
                crate::callstack::FrameType::Function(..) => return &mut frame.args,
                // Executed scripts always shadow positional parameters.
                _ if frame.frame_type.is_run_script() => return &mut frame.args,
                // Sourced scripts shadow positional parameters if they have arguments.
                _ if frame.frame_type.is_sourced_script() && !frame.args.is_empty() => {
                    return &mut frame.args;
                }
                _ => (),
            }
        }

        &mut self.args
    }
}

fn repeated_char_str(c: char, count: usize) -> String {
    (0..count).map(|_| c).collect()
}
