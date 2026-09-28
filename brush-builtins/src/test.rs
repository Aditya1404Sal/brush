use clap::Parser;

use brush_core::{ExecutionExitCode, ExecutionResult, builtins, tests};

/// Evaluate test expression.
#[derive(Parser)]
#[clap(disable_help_flag = true, disable_version_flag = true)]
pub(crate) struct TestCommand {
    #[clap(allow_hyphen_values = true)]
    args: Vec<String>,
}

impl builtins::Command for TestCommand {
    type Error = brush_core::Error;

    /// Override the default [`builtins::Command::new`] function to handle clap's limitation related
    /// to `--`. See [`builtins::parse_known`] for more information
    /// TODO(test): we can safely remove this after the issue is resolved
    fn new<I>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = String>,
    {
        let (mut this, rest_args) = brush_core::builtins::try_parse_known::<Self>(args)?;
        if let Some(args) = rest_args {
            this.args.extend(args);
        }
        Ok(this)
    }

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let mut args = self.args.as_slice();

        if context.command_name == "[" {
            match args.last() {
                Some(s) if s == "]" => (),
                None | Some(_) => {
                    context.report("missing `]'")?;
                    return Ok(ExecutionExitCode::InvalidUsage.into());
                }
            }

            args = &args[0..args.len() - 1];
        }

        let Ok(test_command) = brush_parser::test_command::parse(args) else {
            context.report(syntax_error(args))?;
            return Ok(ExecutionExitCode::InvalidUsage.into());
        };
        if tests::eval_expr(&test_command, context.shell, &context.params)? {
            Ok(ExecutionResult::success())
        } else {
            Ok(ExecutionResult::general_error())
        }
    }
}

/// Why `test` could not read its arguments, as bash words it: with two arguments the first must
/// be a unary operator, with three the second must be a binary operator.
fn syntax_error(args: &[String]) -> String {
    match args {
        [first, _] => std::format!("{first}: unary operator expected"),
        [_, second, _] => std::format!("{second}: binary operator expected"),
        _ => "too many arguments".to_owned(),
    }
}
