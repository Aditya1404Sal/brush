use std::io::Write;

use brush_core::{ExecutionResult, builtins};
use clap::Parser;

/// Evaluate the given string as script.
#[derive(Parser)]
pub(crate) struct EvalCommand {
    /// The script to evaluate.
    #[clap(allow_hyphen_values = true)]
    args: Vec<String>,

    /// The option given, which eval has none of.
    #[clap(skip)]
    invalid_option: Option<String>,
}

impl builtins::Command for EvalCommand {
    type Error = brush_core::Error;

    /// As bash reads them: a first word `--` ends the options, and any other that starts with
    /// `-` (but `-` itself) is an option eval does not have.
    fn new<I>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = String>,
    {
        let mut args: Vec<String> = args.into_iter().skip(1).collect();
        let mut invalid_option = None;
        match args.first() {
            Some(first) if first == "--" => {
                args.remove(0);
            }
            Some(first) if first.len() > 1 && first.starts_with('-') => {
                invalid_option = Some(first.chars().take(2).collect());
            }
            _ => (),
        }
        Ok(Self {
            args,
            invalid_option,
        })
    }

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if let Some(option) = &self.invalid_option {
            context.report(format_args!("{option}: invalid option"))?;
            writeln!(context.stderr(), "eval: usage: eval [arg ...]")?;
            return Ok(ExecutionResult::new(2));
        }
        if !self.args.is_empty() {
            let args_concatenated = self.args.join(" ");

            tracing::debug!("Applying eval to: {:?}", args_concatenated);

            // Our new source context is relative to the current position because we are only
            // providing the raw string being eval'd.
            // TODO(source-info): Provide the location of the specific tokens that make up
            // `self.args`.
            let source_info = brush_core::SourceInfo {
                // Bash names eval'd text `eval` in its diagnostics.
                source: "eval".to_owned(),
                ..context.shell.call_stack().current_pos_as_source_info()
            };

            // Return the direct result of running the string; we intentionally
            // pass through the result and honor its requested control flow. eval
            // executes in the current environment, so all control flow (return,
            // exit, break, continue) should propagate. Its lines are numbered on from the
            // eval command's, as in bash.
            let shift = context.shell.begin_nested_code();
            let result = context
                .shell
                .run_string(args_concatenated, &source_info, &context.params)
                .await;
            context.shell.end_nested_code(shift);
            result
        } else {
            Ok(ExecutionResult::success())
        }
    }
}
