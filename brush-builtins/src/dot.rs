use std::io::Write as _;
use std::path::{Path, PathBuf};

use brush_core::builtins;
use clap::Parser;

/// Evaluate the provided script in the current shell environment.
#[derive(Parser)]
pub(crate) struct DotCommand {
    /// Path to the script to evaluate.
    script_path: Option<String>,

    /// Any arguments to be passed as positional parameters to the script.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    script_args: Vec<String>,
}

impl DotCommand {
    /// Resolves `script_path` the way bash's `sourcepath` option (on by default) does: a name
    /// with no path separator, not found relative to the current directory, is searched for in
    /// `$PATH`. Unlike command lookup, the file need not be executable -- only present -- so
    /// this does not reuse the executable-only path search `command`/lookup uses.
    fn resolve_path<SE: brush_core::ShellExtensions>(
        script_path: &str,
        shell: &brush_core::Shell<SE>,
    ) -> PathBuf {
        let path = Path::new(script_path);
        if !shell.options().source_builtin_searches_path || script_path.contains('/') {
            return path.to_path_buf();
        }
        if shell.absolute_path(path).is_file() {
            return path.to_path_buf();
        }
        let path_var = shell
            .env()
            .get_str("PATH", shell)
            .unwrap_or_default()
            .into_owned();
        for dir in brush_core::sys::fs::split_paths(&path_var) {
            let candidate = dir.join(script_path);
            if shell.absolute_path(&candidate).is_file() {
                return candidate;
            }
        }
        path.to_path_buf()
    }
}

impl builtins::Command for DotCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // As bash words it when no file is named.
        let Some(script_path) = &self.script_path else {
            let name = &context.command_name;
            context.report("filename argument required")?;
            writeln!(
                context.stderr(),
                "{name}: usage: {name} [-p path] filename [arguments]"
            )?;
            return Ok(brush_core::ExecutionResult::new(2));
        };

        // TODO(dot): Handle trap inheritance.
        let script_path = Self::resolve_path(script_path, context.shell);
        let result = context
            .shell
            .source_script(&script_path, self.script_args.iter(), &context.params)
            .await;
        match result {
            // Bash reports a script it cannot read without naming `source`, and carries on
            // (a POSIX-mode shell exits, as a failing special builtin ends it).
            Err(error) if matches!(error.kind(), brush_core::ErrorKind::FailedSourcingFile(..)) => {
                context.shell.display_error(&mut context.stderr(), &error)?;
                let mut result = brush_core::ExecutionResult::general_error();
                if context.shell.options().posix_mode {
                    result.next_control_flow = brush_core::ExecutionControlFlow::ExitShell;
                }
                Ok(result)
            }
            result => result,
        }
    }
}
