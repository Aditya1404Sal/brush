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

    /// Print only the action of each named signal's trap (bash 5.3).
    #[arg(short = 'P')]
    print_actions: bool,

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
        } else if self.print_actions {
            if self.print_trap_commands {
                context.report("cannot specify both -p and -P")?;
                return Ok(ExecutionResult::new(2));
            }
            if self.args.is_empty() {
                context.report("-P requires at least one signal name")?;
                return Ok(ExecutionResult::new(2));
            }
            let (signals, result) = Self::parse_signals(&context, &self.args)?;
            for signal in signals {
                if let Some(handler) = context.shell.traps().get_handler(signal) {
                    writeln!(stdout, "{}", handler.command)?;
                }
            }
            Ok(result)
        } else if self.print_trap_commands || self.args.is_empty() {
            if !self.args.is_empty() {
                let (signals, result) = Self::parse_signals(&context, &self.args)?;
                for signal in signals {
                    Self::display_handlers_for(&context, signal, stdout)?;
                }
                Ok(result)
            } else {
                Self::display_all_handlers(&context, stdout)?;
                Ok(ExecutionResult::success())
            }
        } else if self.args.len() == 1 || self.args[0] == "-" {
            // A single argument is a signal whose handlers to remove; after "-", every argument
            // is.
            let names = if self.args.len() == 1 {
                &self.args[..]
            } else {
                &self.args[1..]
            };
            let (signals, result) = Self::parse_signals(&context, names)?;
            for signal in signals {
                Self::remove_all_handlers(&mut context, signal);
            }
            Ok(result)
        } else {
            let handler = &self.args[0];
            let (signals, result) = Self::parse_signals(&context, &self.args[1..])?;
            Self::register_handler(&mut context, signals, handler.as_str());
            Ok(result)
        }
    }

    /// Parses signal names and numbers. As bash does, an invalid one is reported and fails the
    /// command, and the valid ones still take effect.
    fn parse_signals(
        context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        names: &[String],
    ) -> Result<(Vec<TrapSignal>, ExecutionResult), brush_core::Error> {
        let mut signals = Vec::with_capacity(names.len());
        let mut result = ExecutionResult::success();
        for name in names {
            if let Ok(signal) = name.parse() {
                signals.push(signal);
            } else {
                context.report(format_args!("{name}: invalid signal specification"))?;
                result = ExecutionResult::general_error();
            }
        }
        Ok((signals, result))
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
        brush_core::execution::process::apply_trap_dispositions(context.shell.traps());
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
            brush_core::execution::process::apply_trap_dispositions(context.shell.traps());
        }
    }
}
