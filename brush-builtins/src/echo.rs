use clap::Parser;
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;

use brush_core::{ExecutionResult, builtins, escape};

/// Echo text to standard output.
#[derive(Parser)]
#[clap(disable_help_flag = true, disable_version_flag = true)]
pub(crate) struct EchoCommand {
    /// Suppress the trailing newline from the output.
    #[arg(short = 'n')]
    no_trailing_newline: bool,

    /// Interpret backslash escapes in the provided text.
    #[arg(short = 'e')]
    interpret_backslash_escapes: bool,

    /// Do not interpret backslash escapes in the provided text.
    #[arg(short = 'E')]
    no_interpret_backslash_escapes: bool,

    /// Tokens to echo to standard output.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,

    /// Whether `-e` or `-E` came last, as bash reads them; `None` for neither, where the
    /// `xpg_echo` option decides.
    #[clap(skip)]
    escapes: Option<bool>,
}

impl builtins::Command for EchoCommand {
    type Error = brush_core::Error;

    /// Override the default [`builtins::Command::new`] function to handle clap's limitation related
    /// to `--`. See [`builtins::parse_known`] for more information
    /// TODO(echo): we can safely remove this after the issue is resolved
    fn new<I>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = String>,
    {
        // As bash reads them: leading words made only of `n`, `e` and `E` after a dash are
        // options, the last of `-e` and `-E` wins, and everything else is text (`--` too).
        let mut this = Self {
            no_trailing_newline: false,
            interpret_backslash_escapes: false,
            no_interpret_backslash_escapes: false,
            args: vec![],
            escapes: None,
        };
        let mut options = true;
        for arg in args.into_iter().skip(1) {
            let letters = arg.strip_prefix('-').unwrap_or_default();
            if options
                && !letters.is_empty()
                && letters.chars().all(|c| matches!(c, 'n' | 'e' | 'E'))
            {
                for letter in letters.chars() {
                    match letter {
                        'n' => this.no_trailing_newline = true,
                        'e' => this.escapes = Some(true),
                        _ => this.escapes = Some(false),
                    }
                }
            } else {
                options = false;
                this.args.push(arg);
            }
        }
        this.interpret_backslash_escapes = this.escapes == Some(true);
        this.no_interpret_backslash_escapes = this.escapes == Some(false);
        Ok(this)
    }

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let mut trailing_newline = !self.no_trailing_newline;
        let mut s;
        let interpret = self.escapes.unwrap_or_else(|| {
            context
                .shell
                .options()
                .echo_builtin_expands_escape_sequences
        });
        if interpret {
            s = String::new();
            for (i, arg) in self.args.iter().enumerate() {
                if i > 0 {
                    s.push(' ');
                }

                let (expanded_arg, keep_going) = escape::expand_backslash_escapes(
                    arg.as_str(),
                    escape::EscapeExpansionMode::EchoBuiltin,
                )?;
                s.push_str(&String::from_utf8_lossy(expanded_arg.as_slice()));

                if !keep_going {
                    trailing_newline = false;
                    break;
                }
            }
        } else {
            s = self.args.join(" ");
        }

        if trailing_newline {
            s.push('\n');
        }

        #[cfg(target_arch = "wasm32")]
        {
            use futures::io::AsyncWriteExt;
            let mut stdout = context.stdout();
            stdout.async_io().write_all(s.as_bytes()).await?;
            stdout.async_io().flush().await?;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            write!(context.stdout(), "{s}")?;
            context.stdout().flush()?;
        }

        Ok(ExecutionResult::success())
    }
}
