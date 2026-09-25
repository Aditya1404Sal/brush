//! I/O support for shell instances.

use std::io::Write;

use crate::{error, extensions, ioutils};

impl<SE: extensions::ShellExtensions> crate::Shell<SE> {
    /// Returns a value that can be used to write to the shell's currently configured
    /// standard output stream using `write!` et al.
    pub fn stdout(&self) -> impl std::io::Write + 'static {
        self.open_files
            .try_stdout()
            .cloned()
            .unwrap_or_else(|| ioutils::FailingReaderWriter::new("Bad file descriptor").into())
    }

    /// Returns a value that can be used to write to the shell's currently configured
    /// standard error stream using `write!` et al.
    pub fn stderr(&self) -> impl std::io::Write + 'static {
        self.open_files
            .try_stderr()
            .cloned()
            .unwrap_or_else(|| ioutils::FailingReaderWriter::new("Bad file descriptor").into())
    }

    /// Outputs `set -x` style trace output for a command. Intentionally does not return
    /// a result or error to avoid risk that a caller treats an error as fatal. Tracing
    /// failure should generally always be ignored to avoid interfering with execution
    /// flows.
    ///
    /// # Arguments
    ///
    /// * `command` - The command to trace.
    pub(crate) async fn trace_command<S: AsRef<str>>(
        &mut self,
        params: &crate::interp::ExecutionParameters,
        command: S,
    ) {
        // Expand the PS4 prompt variable to get our prefix, with xtrace off as bash does, so the
        // commands PS4 runs are not traced (each of which would expand PS4 again). A PS4 that
        // cannot be expanded is reported and used as written, as in bash.
        let xtrace = std::mem::replace(&mut self.options.print_commands_and_arguments, false);
        let expanded = self.as_mut().expand_prompt_var("PS4", "").await;
        self.options.print_commands_and_arguments = xtrace;
        let mut prefix = match expanded {
            Ok(prefix) => prefix,
            Err(error) => {
                let _ = self.display_error(&mut params.stderr(self), &error);
                self.env_str("PS4")
                    .map(|ps4| ps4.into_owned())
                    .unwrap_or_default()
            }
        };

        // Add additional depth-based prefixes using the first character of PS4.
        let additional_depth = self.call_stack.script_source_depth() + self.trace_level;
        if let Some(c) = prefix.chars().next() {
            for _ in 0..additional_depth {
                prefix.insert(0, c);
            }
        }

        // Resolve which file descriptor to use for tracing. We default to stderr,
        // but if BASH_XTRACEFD is set and refers to a valid file descriptor, use that instead.
        let trace_file = if let Some((_, xtracefd_var)) = self.env.get("BASH_XTRACEFD")
            && let Ok(fd) = xtracefd_var
                .value()
                .to_cow_str(self)
                .parse::<super::ShellFd>()
            && let Some(file) = self.open_files.try_fd(fd)
        {
            Some(file.clone())
        } else {
            params.try_stderr(self)
        };

        // If we have a valid trace file, write to it.
        if let Some(mut trace_file) = trace_file {
            let _ = writeln!(trace_file, "{prefix}{}", command.as_ref());
        }
    }

    /// Displays the given error to the user, using the shell's error display mechanisms.
    ///
    /// # Arguments
    ///
    /// * `file_table` - The open file table to use for any file descriptor references.
    /// * `err` - The error to display.
    pub fn display_error(
        &self,
        file: &mut impl std::io::Write,
        err: &error::Error,
    ) -> Result<(), error::Error> {
        use crate::extensions::ErrorFormatter as _;
        if err.is_reported() {
            return Ok(());
        }
        let str = self.error_formatter.format_error(err, self);
        write!(file, "{str}")?;

        Ok(())
    }
}
