use clap::Parser;

use brush_core::traps::TrapSignal;
use brush_core::{ExecutionResult, builtins};

/// Manage signal traps.
#[derive(Parser)]
pub(crate) struct TrapCommand {
    /// List all signal names.
    #[arg(short = 'l')]
    list_signals: bool,

    /// Print registered trap commands.
    #[arg(short = 'p')]
    print_trap_commands: bool,

    args: Vec<String>,
}

impl builtins::Command for TrapCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        #[cfg(target_arch = "wasm32")]
        {
            use futures::io::AsyncWriteExt;
            let mut stdout = context.stdout();
            let mut bytes = Vec::new();
            let result = self.execute_inner(context, &mut bytes);
            stdout.async_io().write_all(&bytes).await?;
            result
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut stdout = context.stdout();
            self.execute_inner(context, &mut stdout)
        }
    }
}

impl TrapCommand {
    fn execute_inner(
        &self,
        mut context: brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        stdout: &mut dyn std::io::Write,
    ) -> Result<ExecutionResult, brush_core::Error> {
        if self.list_signals {
            brush_core::traps::format_signals(&mut *stdout, TrapSignal::iterator())
                .map(|()| ExecutionResult::success())
        } else if self.print_trap_commands || self.args.is_empty() {
            if !self.args.is_empty() {
                for signal_type in &self.args {
                    Self::display_handlers_for(&context, signal_type.parse()?, stdout)?;
                }
            } else {
                Self::display_all_handlers(&context, stdout)?;
            }
            Ok(ExecutionResult::success())
        } else if self.args.len() == 1 {
            // When only a single argument is given, it is assumed to be a signal name
            // and an indication to remove the handlers for that signal.
            let signal = self.args[0].as_str();
            Self::remove_all_handlers(&mut context, signal.parse()?);
            Ok(ExecutionResult::success())
        } else if self.args[0] == "-" {
            // "-" as the first argument indicates that the remaining
            // arguments are signal names and we need to remove the handlers for them.
            for signal in &self.args[1..] {
                Self::remove_all_handlers(&mut context, signal.parse()?);
            }
            Ok(ExecutionResult::success())
        } else {
            let handler = &self.args[0];

            let mut signal_types = Vec::with_capacity(self.args.len() - 1);
            for signal in &self.args[1..] {
                signal_types.push(signal.parse()?);
            }

            Self::register_handler(&mut context, signal_types, handler.as_str());
            Ok(ExecutionResult::success())
        }
    }

    fn display_all_handlers(
        context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        stdout: &mut dyn std::io::Write,
    ) -> Result<(), brush_core::Error> {
        for (signal, _) in context.shell.traps().iter_handlers() {
            Self::display_handlers_for(context, signal, stdout)?;
        }
        Ok(())
    }

    fn display_handlers_for(
        context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        signal_type: TrapSignal,
        stdout: &mut dyn std::io::Write,
    ) -> Result<(), brush_core::Error> {
        if let Some(handler) = context.shell.traps().get_handler(signal_type) {
            #[cfg(target_arch = "wasm32")]
            writeln!(
                stdout,
                "trap -- {} {signal_type}",
                brush_core::escape::single_quote(&handler.command)
            )?;
            #[cfg(not(target_arch = "wasm32"))]
            writeln!(stdout, "trap -- '{}' {signal_type}", handler.command)?;
        }
        Ok(())
    }

    fn remove_all_handlers(
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        signal: TrapSignal,
    ) {
        context.shell.traps_mut().remove_handlers(signal);
        #[cfg(target_arch = "wasm32")]
        brush_core::execution::process::set_pipe_disposition(
            context.shell.traps().pipe_disposition(),
        );
    }

    fn register_handler<I>(
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        signals: I,
        handler: &str,
    ) where
        I: IntoIterator<Item = TrapSignal>,
    {
        // Our new source context is relative to the current position.
        // TODO(source-info): Provide the location of the specific token that makes up
        // `self.args[0]`.
        let source_info = context.shell.call_stack().current_pos_as_source_info();

        for signal in signals {
            context.shell.traps_mut().register_handler(
                signal,
                handler.to_owned(),
                source_info.clone(),
            );
            #[cfg(target_arch = "wasm32")]
            brush_core::execution::process::set_pipe_disposition(
                context.shell.traps().pipe_disposition(),
            );
        }
    }
}
