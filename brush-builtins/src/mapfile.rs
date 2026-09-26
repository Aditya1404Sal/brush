#[cfg(not(target_arch = "wasm32"))]
use std::io::Read;

use clap::Parser;

use brush_core::{ExecutionExitCode, ExecutionResult, builtins, env, variables};

/// Read lines from standard input into an indexed array variable.
#[derive(Parser)]
pub(crate) struct MapFileCommand {
    /// Delimiter to use (defaults to newline).
    #[arg(short = 'd')]
    delimiter: Option<String>,

    /// Maximum number of entries to read (0 means no limit).
    #[arg(short = 'n', allow_hyphen_values = true)]
    max_count: Option<String>,

    /// Index into array at which to start assignment.
    #[arg(short = 'O', allow_hyphen_values = true)]
    origin: Option<String>,

    /// Number of initial entries to skip.
    #[arg(short = 's', allow_hyphen_values = true)]
    skip_count: Option<String>,

    /// Whether or not to remove the delimiter from each read line.
    #[arg(short = 't')]
    remove_delimiter: bool,

    /// File descriptor to read from (defaults to stdin).
    #[arg(short = 'u')]
    fd: Option<brush_core::ShellFd>,

    /// Name of function to call for each group of lines.
    #[arg(short = 'C')]
    callback: Option<String>,

    /// Number of lines to pass the callback for each group.
    #[arg(short = 'c', allow_hyphen_values = true)]
    callback_group_size: Option<String>,

    /// Name of array to read into.
    #[arg(default_value = "MAPFILE")]
    array_var_name: String,
}

impl builtins::Command for MapFileCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // The counts are numbers, as bash words what is not: a line count or a skip count of
        // at least 0, and a callback quantum of at least 1.
        let number = |text: &Option<String>, least: i64, default: i64| match text {
            None => Some(default),
            Some(text) => text.trim().parse::<i64>().ok().filter(|n| *n >= least),
        };
        let counts = [
            (&self.max_count, 0, 0, "invalid line count"),
            (&self.skip_count, 0, 0, "invalid line count"),
            (
                &self.callback_group_size,
                1,
                5000,
                "invalid callback quantum",
            ),
        ]
        .map(|(text, least, default, message)| (number(text, least, default), text, message));
        let mut values = [0_i64; 3];
        for (slot, (value, text, message)) in values.iter_mut().zip(counts) {
            let Some(value) = value else {
                let text = text.as_deref().unwrap_or_default();
                context.report(format_args!("{text}: {message}"))?;
                return Ok(ExecutionExitCode::GeneralError.into());
            };
            *slot = value;
        }
        let [max_count, skip_count, quantum] = values;

        if !brush_core::env::valid_variable_name(&self.array_var_name) {
            context.report(format_args!(
                "`{}': not a valid identifier",
                self.array_var_name
            ))?;
            return Ok(ExecutionExitCode::GeneralError.into());
        }

        // The origin is a number, not an expression, as in bash.
        let origin = match self
            .origin
            .as_deref()
            .map(|text| (text, text.trim().parse::<i64>()))
        {
            None => None,
            Some((_, Ok(origin))) if origin >= 0 => Some(origin),
            Some((text, _)) => {
                context.report(format_args!("{text}: invalid array origin"))?;
                return Ok(ExecutionExitCode::GeneralError.into());
            }
        };

        if let Some((_, var)) = context.shell.env().get(&self.array_var_name) {
            if var.value().is_associative_array() {
                context.report(format_args!(
                    "{}: not an indexed array",
                    self.array_var_name
                ))?;
                return Ok(ExecutionExitCode::GeneralError.into());
            }
        }

        // A descriptor named with -u must be open; a closed standard input reads as empty, as in
        // bash.
        let fd = self
            .fd
            .unwrap_or(brush_core::openfiles::OpenFiles::STDIN_FD);
        // With a callback, the elements are assigned as they are read, as in bash.
        if let Some(callback) = &self.callback
            && let Some(input_file) = context.try_fd(fd)
        {
            return self
                .read_with_callback(
                    context,
                    input_file,
                    callback,
                    origin,
                    [max_count, skip_count, quantum],
                )
                .await;
        }

        let results = match context.try_fd(fd) {
            Some(input_file) => self.read_entries(input_file, max_count, skip_count).await?,
            None if self.fd.is_some() => {
                context.report(format_args!(
                    "{fd}: invalid file descriptor: Bad file descriptor"
                ))?;
                return Ok(ExecutionExitCode::GeneralError.into());
            }
            None => variables::ArrayLiteral(vec![]),
        };

        // Bash looks the array up once, and a circular name reference warns.
        context
            .shell
            .warn_circular_nameref(&context.params, &self.array_var_name, 1, false);

        if let Some(origin) = origin {
            // -O: preserve existing array, assign at offset.
            for (elem_idx, (_key, value)) in results.0.into_iter().enumerate() {
                // If the user is getting to wraparounds in *bash*, they got bigger problems.
                #[allow(clippy::cast_possible_wrap)]
                let elem_idx = elem_idx as i64;
                context.shell.env_mut().update_or_add_array_element(
                    &self.array_var_name,
                    (elem_idx + origin).to_string(),
                    value,
                    |_| Ok(()),
                    env::EnvironmentLookup::Anywhere,
                    env::EnvironmentScope::Global,
                )?;
            }
        } else {
            // No -O: replace the entire variable (clears existing).
            context.shell.env_mut().update_or_add(
                &self.array_var_name,
                variables::ShellValueLiteral::Array(results),
                |_| Ok(()),
                env::EnvironmentLookup::Anywhere,
                env::EnvironmentScope::Global,
            )?;
        }

        Ok(ExecutionResult::success())
    }
}

impl MapFileCommand {
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(clippy::unused_async, reason = "WASM pipe input is awaited")
    )]
    async fn read_entries(
        &self,
        mut input_file: brush_core::openfiles::OpenFile,
        max_count: i64,
        skip_count: i64,
    ) -> Result<variables::ArrayLiteral, brush_core::Error> {
        let _term_mode = setup_terminal_settings(&input_file)?;
        let mut entries = vec![];
        let mut read_count = 0;
        let max_count = usize::try_from(max_count)?;
        while max_count == 0 || entries.len() < max_count {
            let Some(line) = self.read_line(&mut input_file).await? else {
                break;
            };
            if read_count < skip_count {
                read_count += 1;
                continue;
            }
            entries.push((None, line));
        }

        Ok(variables::ArrayLiteral(entries))
    }

    /// The delimiter that ends each line.
    fn delimiter(&self) -> u8 {
        match &self.delimiter {
            Some(d) if d.is_empty() => b'\0',
            Some(d) => brush_core::rawbytes::encode(d)
                .first()
                .copied()
                .unwrap_or(b'\n'),
            None => b'\n',
        }
    }

    /// Reads the next line, without its delimiter with `-t`; `None` at the end of the input.
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(clippy::unused_async, reason = "WASM pipe input is awaited")
    )]
    async fn read_line(
        &self,
        input_file: &mut brush_core::openfiles::OpenFile,
    ) -> Result<Option<String>, brush_core::Error> {
        // Ctrl+C and Ctrl+D are keys only on a terminal; other input holds them as characters.
        let terminal = input_file.is_terminal();
        let delimiter = self.delimiter();
        let mut buf = [0u8; 1];
        let mut line = vec![];
        let mut saw_delimiter = false;

        loop {
            #[cfg(target_arch = "wasm32")]
            let read = futures::io::AsyncReadExt::read(input_file.async_io(), &mut buf).await;
            #[cfg(not(target_arch = "wasm32"))]
            let read = input_file.read(&mut buf);
            match read {
                Ok(0) => break,                                                     // End of input
                Ok(1) if terminal && buf[0] == b'\x03' => break,                    // Ctrl+C
                Ok(1) if terminal && buf[0] == b'\x04' && line.is_empty() => break, // Ctrl+D
                Ok(1) => {
                    let byte = buf[0];
                    line.push(byte);
                    if byte == delimiter {
                        saw_delimiter = true;
                        break;
                    }
                }
                Ok(_) => unreachable!("input can only be 0, 1, or error"),
                Err(e) => return Err(e.into()),
            }
        }

        if line.is_empty() && !saw_delimiter {
            return Ok(None);
        }

        if self.remove_delimiter && line.ends_with(&[delimiter]) {
            line.pop();
        }

        // A bash string ends at a NUL byte.
        if let Some(nul) = line.iter().position(|byte| *byte == 0) {
            line.truncate(nul);
        }

        // Bytes that are not UTF-8 are kept (see `rawbytes`).
        Ok(Some(brush_core::rawbytes::decode_vec(line)))
    }

    /// Reads lines into the array one at a time, evaluating `callback` with the index and the
    /// line (quoted) before every `quantum`th line is assigned, as bash does.
    async fn read_with_callback<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
        mut input_file: brush_core::openfiles::OpenFile,
        callback: &str,
        origin: Option<i64>,
        [max_count, skip_count, quantum]: [i64; 3],
    ) -> Result<ExecutionResult, brush_core::Error> {
        let _term_mode = setup_terminal_settings(&input_file)?;
        for _ in 0..skip_count {
            if self.read_line(&mut input_file).await?.is_none() {
                break;
            }
        }

        context
            .shell
            .warn_circular_nameref(&context.params, &self.array_var_name, 1, false);
        // Without -O the array starts empty.
        if origin.is_none() {
            context.shell.env_mut().update_or_add(
                &self.array_var_name,
                variables::ShellValueLiteral::Array(variables::ArrayLiteral(vec![])),
                |_| Ok(()),
                env::EnvironmentLookup::Anywhere,
                env::EnvironmentScope::Global,
            )?;
        }

        let mut index = origin.unwrap_or(0);
        let mut line_count = 1;
        while let Some(line) = self.read_line(&mut input_file).await? {
            if line_count % quantum == 0 {
                let command = format!("{callback} {index} '{}'", line.replace('\'', "'\\''"));
                let source_info = context.shell.call_stack().current_pos_as_source_info();
                context
                    .shell
                    .run_string(command, &source_info, &context.params)
                    .await?;
            }
            context.shell.env_mut().update_or_add_array_element(
                &self.array_var_name,
                index.to_string(),
                line,
                |_| Ok(()),
                env::EnvironmentLookup::Anywhere,
                env::EnvironmentScope::Global,
            )?;
            index += 1;
            line_count += 1;
            if max_count != 0 && line_count > max_count {
                break;
            }
        }

        Ok(ExecutionResult::success())
    }
}

fn setup_terminal_settings(
    file: &brush_core::openfiles::OpenFile,
) -> Result<Option<brush_core::terminal::AutoModeGuard>, brush_core::Error> {
    let mode = brush_core::terminal::AutoModeGuard::new(file.to_owned()).ok();
    if let Some(mode) = &mode {
        let config = brush_core::terminal::Settings::builder()
            .line_input(false)
            .interrupt_signals(false)
            .build();

        mode.apply_settings(&config)?;
    }

    Ok(mode)
}
