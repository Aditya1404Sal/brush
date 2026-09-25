use clap::Parser;
use itertools::Itertools;
use std::collections::VecDeque;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use brush_core::{ErrorKind, builtins, env, error, variables};

#[cfg(target_arch = "wasm32")]
use futures::{
    FutureExt,
    io::{AsyncReadExt, AsyncWriteExt},
};
#[cfg(not(target_arch = "wasm32"))]
use std::io::Read;
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use utf8_chars::BufReadCharsExt;

/// Exit code returned when `read` times out.
/// This is 128 + SIGALRM (14) = 142, matching bash behavior.
const TIMEOUT_EXIT_CODE: u8 = 142;

/// ASCII control character for Ctrl+C (ETX - End of Text).
const CTRL_C: char = '\x03';
/// ASCII control character for Ctrl+D (EOT - End of Transmission).
const CTRL_D: char = '\x04';
/// Backslash character used for escape processing.
const BACKSLASH: char = '\\';
/// Default line delimiter (newline).
const DEFAULT_DELIMITER: char = '\n';
/// NUL character used as delimiter when `-d ''` is specified.
const NUL_DELIMITER: char = '\0';

/// Parse standard input.
#[derive(Parser)]
pub(crate) struct ReadCommand {
    /// Optionally, name of an array variable to receive read words
    /// of input.
    #[clap(short = 'a', value_name = "VAR_NAME")]
    array_variable: Option<String>,

    /// Optionally, a delimiter to use other than a newline character.
    #[clap(short = 'd')]
    delimiter: Option<String>,

    /// Use readline-like input.
    #[clap(short = 'e')]
    use_readline: bool,

    /// Provide text to use as initial input for readline.
    #[clap(short = 'i', value_name = "STR")]
    initial_text: Option<String>,

    /// Read only the first N characters or until a specified
    /// delimiter is reached, whichever happens first.
    #[clap(short = 'n', value_name = "COUNT")]
    return_after_n_chars: Option<usize>,

    /// Read exactly N characters, ignoring any specified delimiter.
    #[clap(short = 'N', value_name = "COUNT")]
    return_after_n_chars_no_delimiter: Option<usize>,

    /// Prompt to display before reading.
    #[clap(short = 'p')]
    prompt: Option<String>,

    /// Read input in raw mode; no escape sequences.
    #[clap(short = 'r')]
    raw_mode: bool,

    /// Do not echo input.
    #[clap(short = 's')]
    silent: bool,

    /// Specify timeout in seconds; fail if the timeout elapses before
    /// input is completed.
    #[clap(short = 't', value_name = "SECONDS", allow_hyphen_values = true)]
    timeout_in_seconds: Option<f64>,

    /// File descriptor to read from instead of stdin.
    #[clap(short = 'u', name = "FD")]
    fd_num_to_read: Option<u8>,

    /// Optionally, names of variables to receive read input.
    variable_names: Vec<String>,
}

impl builtins::Command for ReadCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if self.use_readline {
            return error::unimp("read -e");
        }
        if self.initial_text.is_some() {
            return error::unimp("read -i");
        }

        // Validate timeout value if provided.
        if let Some(result) = self.validate_timeout(&context).await? {
            return Ok(result);
        }

        // Find the input stream to use.
        let fd_num = self.fd_num_to_read.map_or(
            brush_core::openfiles::OpenFiles::STDIN_FD,
            brush_core::ShellFd::from,
        );

        // Retrieve the file.
        let input_stream = context
            .try_fd(fd_num)
            .ok_or_else(|| ErrorKind::BadFileDescriptor(fd_num))?;

        // Retrieve effective value of IFS for splitting.
        // We convert to owned String to release the borrow before the mutable borrow
        // needed for variable assignment.
        let ifs = context.shell.ifs().into_owned();

        // Convert timeout to Duration.
        let timeout = self.timeout_in_seconds.map(Duration::from_secs_f64);

        let read_result = self
            .read_line(
                input_stream,
                context.stderr(),
                timeout,
                #[cfg(target_arch = "wasm32")]
                context.shell.execution_services(),
            )
            .await?;

        // Determine whether to skip IFS splitting (for -N option).
        let skip_ifs_splitting = self.return_after_n_chars_no_delimiter.is_some();

        // Extract the input line and determine exit code based on result.
        let (input_line, result) = match &read_result {
            ReadResult::Line(line) => (Some(line.clone()), brush_core::ExecutionResult::success()),
            ReadResult::Eof(Some(line)) => (
                Some(line.clone()),
                brush_core::ExecutionResult::general_error(),
            ),
            ReadResult::Eof(None) | ReadResult::Interrupted | ReadResult::InputNotReady => {
                (None, brush_core::ExecutionResult::general_error())
            }
            ReadResult::TimedOut(partial) => (
                partial.clone(),
                brush_core::ExecutionResult::new(TIMEOUT_EXIT_CODE),
            ),
            ReadResult::InputReady => (None, brush_core::ExecutionResult::success()),
        };

        // Assign input to variables based on options.
        assign_input_to_variables(
            context.shell,
            input_line.as_deref(),
            &ifs,
            !self.raw_mode,
            skip_ifs_splitting,
            self.array_variable.as_deref(),
            &self.variable_names,
        )?;

        Ok(result)
    }
}

/// Assigns read input to shell variables based on the specified options.
///
/// This handles three modes:
/// - Array mode (`-a`): Split input by IFS and assign to array elements
/// - Named variables: Split input by IFS and assign to each variable, with remainder to last
/// - Default (`REPLY`): Assign entire input line to the `REPLY` variable
fn assign_input_to_variables(
    shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>,
    input_line: Option<&str>,
    ifs: &str,
    escapes: bool,
    skip_ifs_splitting: bool,
    array_variable: Option<&str>,
    variable_names: &[String],
) -> Result<(), brush_core::Error> {
    if let Some(array_variable) = array_variable {
        let literal_fields = build_array_fields(input_line, ifs, escapes, skip_ifs_splitting);
        shell.env_mut().update_or_add(
            array_variable,
            variables::ShellValueLiteral::Array(variables::ArrayLiteral(literal_fields)),
            |_| Ok(()),
            env::EnvironmentLookup::Anywhere,
            env::EnvironmentScope::Global,
        )?;
    } else if !variable_names.is_empty() {
        assign_to_named_variables(
            shell,
            input_line,
            ifs,
            escapes,
            skip_ifs_splitting,
            variable_names,
        )?;
    } else {
        shell.env_mut().update_or_add(
            "REPLY",
            variables::ShellValueLiteral::Scalar(unescape_input(
                input_line.unwrap_or_default(),
                escapes,
            )),
            |_| Ok(()),
            env::EnvironmentLookup::Anywhere,
            env::EnvironmentScope::Global,
        )?;
    }
    Ok(())
}

/// Assigns split fields to named variables.
///
/// Fields are assigned one per variable, with any remaining fields joined by space
/// and assigned to the last variable. If there are more variables than fields,
/// the extra variables are set to empty strings.
fn assign_to_named_variables(
    shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>,
    input_line: Option<&str>,
    ifs: &str,
    escapes: bool,
    skip_ifs_splitting: bool,
    variable_names: &[String],
) -> Result<(), brush_core::Error> {
    let mut fields = build_variable_fields(
        input_line,
        ifs,
        escapes,
        skip_ifs_splitting,
        variable_names.len(),
    );

    for (i, name) in variable_names.iter().enumerate() {
        let is_last = i == variable_names.len() - 1;

        let value = if fields.is_empty() {
            String::new()
        } else if is_last {
            // Last variable gets all remaining fields joined by space.
            std::mem::take(&mut fields).into_iter().join(" ")
        } else {
            fields.pop_front().unwrap_or_default()
        };

        shell.env_mut().update_or_add(
            name,
            variables::ShellValueLiteral::Scalar(value),
            |_| Ok(()),
            env::EnvironmentLookup::Anywhere,
            env::EnvironmentScope::Global,
        )?;

        if is_last {
            break;
        }
    }
    Ok(())
}

/// Builds array field values from input, optionally splitting by IFS.
fn build_array_fields(
    input_line: Option<&str>,
    ifs: &str,
    escapes: bool,
    skip_ifs_splitting: bool,
) -> Vec<(Option<String>, String)> {
    match input_line {
        Some(line) if skip_ifs_splitting => {
            // With -N, don't split - put entire input as single element.
            vec![(None, unescape_input(line, escapes))]
        }
        Some(line) => {
            let fields: VecDeque<_> =
                split_line_by_ifs(ifs, line, None /* max_fields */, escapes);
            fields.into_iter().map(|f| (None, f)).collect()
        }
        None => vec![],
    }
}

/// Builds field values from input for assignment to named variables.
fn build_variable_fields(
    input_line: Option<&str>,
    ifs: &str,
    escapes: bool,
    skip_ifs_splitting: bool,
    num_variables: usize,
) -> VecDeque<String> {
    match input_line {
        Some(line) if skip_ifs_splitting => {
            // With -N, don't split - put entire input in first variable.
            VecDeque::from([unescape_input(line, escapes)])
        }
        Some(line) => split_line_by_ifs(ifs, line, Some(num_variables), escapes),
        None => VecDeque::new(),
    }
}

/// Result of a `read` operation.
///
/// This enum clearly represents all possible outcomes of `read_line()`,
/// making the contract with callers explicit.
enum ReadResult {
    /// Successfully read a complete line (delimiter or char limit reached).
    Line(String),
    /// Reached end of input. Contains any partial content read before EOF.
    Eof(Option<String>),
    /// Input was interrupted (e.g., Ctrl+C). No content is returned.
    Interrupted,
    /// The operation timed out. Contains any partial content read before timeout.
    TimedOut(Option<String>),
    /// For `-t 0`: input is immediately available (exit 0).
    InputReady,
    /// For `-t 0`: no input immediately available (exit 1).
    InputNotReady,
}

/// Helper struct that encapsulates the state for reading input character by character.
///
/// This separates the concerns of character-level I/O with timeout handling from the
/// higher-level logic of line building and escape processing.
#[cfg(not(target_arch = "wasm32"))]
struct InputReader {
    /// The input source.
    input: std::io::BufReader<PolledInput>,
    /// Bytes from a malformed UTF-8 sequence, still owed to the caller one at a time.
    pending: VecDeque<char>,
    /// Whether the input is a terminal, where Ctrl+C and Ctrl+D are keys, not characters.
    terminal: bool,
    /// Terminal mode guard - kept alive for RAII cleanup on drop.
    /// The guard restores original terminal settings when dropped, even though
    /// we don't access the field directly after construction.
    ///
    /// The leading underscore suppresses the "unused field" warning while making
    /// it explicit this field exists solely for its `Drop` implementation.
    _term_mode: Option<brush_core::terminal::AutoModeGuard>,
}

/// Events that can occur when reading input.
enum InputEvent {
    /// A regular character was read.
    Char(char),
    /// End of file was reached.
    Eof,
    /// The read operation timed out.
    Timeout,
    /// Ctrl+C was pressed.
    CtrlC,
    /// Ctrl+D was pressed.
    CtrlD,
}

#[cfg(not(target_arch = "wasm32"))]
impl InputReader {
    /// Creates a new input reader with optional timeout.
    fn new(
        input: brush_core::openfiles::OpenFile,
        timeout: Option<Duration>,
        term_mode: Option<brush_core::terminal::AutoModeGuard>,
    ) -> Self {
        let terminal = input.is_terminal();
        Self {
            input: std::io::BufReader::with_capacity(
                1,
                PolledInput {
                    input,
                    deadline: timeout.map(|t| Instant::now() + t),
                },
            ),
            pending: VecDeque::new(),
            terminal,
            _term_mode: term_mode,
        }
    }

    /// Checks if input is immediately available (for `-t 0`). Returns `false` if an error
    /// occurs while checking for available input.
    fn check_input_available(&self) -> bool {
        brush_core::sys::poll::poll_for_input(&self.input.get_ref().input, Duration::ZERO)
            .unwrap_or(false)
    }

    /// Reads the next input event, handling timeout and control characters.
    #[allow(
        clippy::unused_async,
        reason = "WASM awaits incremental input through the same interface"
    )]
    async fn read_event(&mut self) -> Result<InputEvent, brush_core::Error> {
        let ch = loop {
            if let Some(ch) = self.pending.pop_front() {
                break ch;
            }

            match self.input.read_char_raw() {
                Ok(Some(ch)) => break stand_for_own_bytes(ch, &mut self.pending),
                Ok(None) => return Ok(InputEvent::Eof),
                Err(e) => {
                    // Hand back the bytes it consumed, one at a time, each as the character
                    // that stands for it (see `rawbytes`); the byte that broke the sequence was
                    // left unconsumed, for the next call to re-read.
                    if e.as_bytes().is_empty() {
                        if e.as_io_error().kind() == std::io::ErrorKind::TimedOut {
                            return Ok(InputEvent::Timeout);
                        }
                        return Err(e.into_io_error().into());
                    }
                    self.pending.extend(
                        e.as_bytes()
                            .iter()
                            .copied()
                            .map(brush_core::rawbytes::byte_char),
                    );
                }
            }
        };

        Ok(input_event(ch, self.terminal))
    }
}

/// Reads the input file one byte at a time, enforcing the deadline on every byte.
///
/// Wrapped in a 1-byte `BufReader`, which gives us both the pushback a byte that turns
/// out not to belong to the current UTF-8 sequence needs, and the guarantee that `read`
/// never consumes more of the descriptor than it asked for -- whatever runs next may
/// want the rest.
#[cfg(not(target_arch = "wasm32"))]
struct PolledInput {
    /// The input source.
    input: brush_core::openfiles::OpenFile,
    /// Optional deadline for timeout.
    deadline: Option<Instant>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Read for PolledInput {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(deadline) = self.deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero()
                || !brush_core::sys::poll::poll_for_input(&self.input, remaining)?
            {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
        }
        self.input.read(buf)
    }
}

#[cfg(target_arch = "wasm32")]
struct InputReader {
    input: brush_core::openfiles::OpenFile,
    timer: Option<futures::future::LocalBoxFuture<'static, ()>>,
    pending: VecDeque<char>,
    bytes: VecDeque<u8>,
    /// Whether the input is a terminal, where Ctrl+C and Ctrl+D are keys, not characters.
    terminal: bool,
}

#[cfg(target_arch = "wasm32")]
impl InputReader {
    fn new(
        input: brush_core::openfiles::OpenFile,
        timeout: Option<Duration>,
        _term_mode: Option<brush_core::terminal::AutoModeGuard>,
        services: brush_core::execution::ExecutionServices,
    ) -> Self {
        Self {
            terminal: input.is_terminal(),
            input,
            timer: timeout
                .filter(|time| !time.is_zero())
                .map(|time| (services.sleep)(time)),
            pending: VecDeque::new(),
            bytes: VecDeque::new(),
        }
    }
    fn check_input_available(&self) -> bool {
        self.input.input_ready().unwrap_or(false)
    }

    async fn byte(&mut self) -> Result<Option<u8>, std::io::Error> {
        if let Some(byte) = self.bytes.pop_front() {
            return Ok(Some(byte));
        }
        let mut byte = [0];
        let read = self.input.async_io().read(&mut byte);
        let count = if let Some(timer) = &mut self.timer {
            match futures::future::select(read.boxed_local(), timer.as_mut()).await {
                futures::future::Either::Left((result, _)) => result?,
                futures::future::Either::Right(_) => {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
            }
        } else {
            read.await?
        };
        Ok((count != 0).then_some(byte[0]))
    }

    async fn read_event(&mut self) -> Result<InputEvent, brush_core::Error> {
        let ch = if let Some(ch) = self.pending.pop_front() {
            ch
        } else {
            let first = match self.byte().await {
                Ok(Some(byte)) => byte,
                Ok(None) => return Ok(InputEvent::Eof),
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                    return Ok(InputEvent::Timeout);
                }
                Err(error) => return Err(error.into()),
            };
            let width = match first {
                0..=127 => 1,
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => 1,
            };
            let mut encoded = vec![first];
            while encoded.len() < width {
                match self.byte().await {
                    Ok(Some(byte)) if byte & 0xc0 == 0x80 => encoded.push(byte),
                    Ok(Some(byte)) => {
                        self.bytes.push_front(byte);
                        break;
                    }
                    Ok(None) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::TimedOut => break,
                    Err(error) => return Err(error.into()),
                }
            }
            if let Ok(text) = std::str::from_utf8(&encoded) {
                let ch = text.chars().next().ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "empty UTF-8 sequence")
                })?;
                stand_for_own_bytes(ch, &mut self.pending)
            } else {
                // Each byte of a malformed sequence is a character of its own, standing for
                // that byte (see `rawbytes`).
                self.pending.extend(
                    encoded
                        .into_iter()
                        .skip(1)
                        .map(brush_core::rawbytes::byte_char),
                );
                brush_core::rawbytes::byte_char(first)
            }
        };
        Ok(input_event(ch, self.terminal))
    }
}

/// A character decoded from valid UTF-8, as the shell holds it: a character in the range that
/// stands for bytes that are not UTF-8 (see `rawbytes`) stands for its own bytes instead, so the
/// first is returned and the rest queued ahead of any pending characters.
fn stand_for_own_bytes(ch: char, pending: &mut VecDeque<char>) -> char {
    if brush_core::rawbytes::char_byte(ch).is_none() {
        return ch;
    }
    let mut buffer = [0; 4];
    let mut bytes = ch
        .encode_utf8(&mut buffer)
        .bytes()
        .map(brush_core::rawbytes::byte_char);
    let first = bytes.next().unwrap_or(ch);
    for (index, byte) in bytes.enumerate() {
        pending.insert(index, byte);
    }
    first
}

/// The input event for a character read. Ctrl+C and Ctrl+D are keys only on a terminal; any
/// other input holds them as ordinary characters, as bash reads them.
const fn input_event(ch: char, terminal: bool) -> InputEvent {
    match ch {
        CTRL_C if terminal => InputEvent::CtrlC,
        CTRL_D if terminal => InputEvent::CtrlD,
        _ => InputEvent::Char(ch),
    }
}

/// Configuration for line reading behavior.
struct LineReaderConfig {
    /// Character that terminates input (None for -N mode).
    delimiter: Option<char>,
    /// Maximum characters to read (for -n or -N).
    char_limit: Option<usize>,
    /// Whether to process backslash escapes (false for -r mode).
    process_escapes: bool,
    /// Whether the input is a terminal, whose other control characters are keys too.
    terminal: bool,
}

/// Reads a complete line of input using the given reader and configuration.
///
/// Returns a `ReadResult` indicating success, EOF, timeout, or interruption.
///
/// Note on character counting for `-n` limit:
/// Bash counts OUTPUT characters (after escape processing) toward the limit.
/// For example, with `-n 3` and input `a\bc` (4 bytes):
/// - Bash processes: 'a' (output 1), '\b' → 'b' (output 2), 'c' (output 3) → "abc"
/// - The backslash is consumed but doesn't count toward the limit
async fn read_line_with_reader(
    reader: &mut InputReader,
    config: &LineReaderConfig,
) -> Result<ReadResult, brush_core::Error> {
    let mut line = String::new();
    // Tracked alongside `line`, which is a `String`: the limit counts characters, and
    // recounting a growing string on every one of them is quadratic.
    let mut output_chars = 0usize;
    let mut pending_backslash = false;

    loop {
        let event = reader.read_event().await?;

        match event {
            InputEvent::Eof => {
                // Bash discards pending backslash on EOF.
                return Ok(ReadResult::Eof(if line.is_empty() {
                    None
                } else {
                    Some(line)
                }));
            }

            InputEvent::Timeout => {
                // Include pending backslash on timeout (different from EOF).
                if pending_backslash {
                    line.push(BACKSLASH);
                }
                return Ok(ReadResult::TimedOut(if line.is_empty() {
                    None
                } else {
                    Some(line)
                }));
            }

            InputEvent::CtrlC => {
                return Ok(ReadResult::Interrupted);
            }

            InputEvent::CtrlD => {
                // At line start = EOF, mid-input = flush current input.
                // Bash discards pending backslash here too.
                return Ok(if line.is_empty() && !pending_backslash {
                    ReadResult::Eof(None)
                } else {
                    ReadResult::Line(line)
                });
            }

            InputEvent::Char(ch) => {
                // Handle backslash escape processing (when enabled).
                if config.process_escapes {
                    if pending_backslash {
                        pending_backslash = false;

                        // Backslash-delimiter is line continuation.
                        if let Some(delim) = config.delimiter
                            && ch == delim
                        {
                            continue; // Line continuation.
                        }

                        // For other chars, keep the escape: the char is literal, so field
                        // splitting must not split on it; the backslash is removed when the
                        // line is split or assigned (see `input_units`).
                        line.push(BACKSLASH);
                        line.push(ch);
                        output_chars += 1;

                        // Check character limit (based on output length).
                        if let Some(limit) = config.char_limit
                            && output_chars >= limit
                        {
                            return Ok(ReadResult::Line(line));
                        }
                        continue;
                    }

                    if ch == BACKSLASH {
                        pending_backslash = true;
                        continue;
                    }
                }

                // Check for delimiter.
                if let Some(delim) = config.delimiter
                    && ch == delim
                {
                    return Ok(ReadResult::Line(line));
                }

                // A NUL byte is skipped, as bash skips it; on a terminal, so are the other
                // non-whitespace control characters (keys with no meaning here).
                if ch == '\0'
                    || (config.terminal && ch.is_ascii_control() && !ch.is_ascii_whitespace())
                {
                    continue;
                }

                line.push(ch);
                output_chars += 1;

                // Check character limit (based on output length).
                if let Some(limit) = config.char_limit
                    && output_chars >= limit
                {
                    return Ok(ReadResult::Line(line));
                }
            }
        }
    }
}

impl ReadCommand {
    /// Reads a line of input, optionally with a timeout.
    ///
    /// Handles backslash escape processing:
    /// - Without `-r`: backslash-newline is line continuation, other backslashes escape the next
    ///   char
    /// - With `-r`: backslash is treated as a literal character
    async fn read_line(
        &self,
        input_file: brush_core::openfiles::OpenFile,
        mut stderr_file: brush_core::openfiles::OpenFile,
        timeout: Option<Duration>,
        #[cfg(target_arch = "wasm32")] services: brush_core::execution::ExecutionServices,
    ) -> Result<ReadResult, brush_core::Error> {
        let term_mode = self.setup_terminal_settings(&input_file)?;

        // Display prompt on stderr, but only if input is from a terminal (per bash behavior).
        if let Some(prompt) = &self.prompt {
            if input_file.is_terminal() {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    write!(stderr_file, "{prompt}")?;
                    stderr_file.flush()?;
                }
                #[cfg(target_arch = "wasm32")]
                {
                    stderr_file.async_io().write_all(prompt.as_bytes()).await?;
                    stderr_file.async_io().flush().await?;
                }
            }
        }

        // Determine delimiter based on options.
        let delimiter = if self.return_after_n_chars_no_delimiter.is_some() {
            None
        } else if let Some(delimiter_str) = &self.delimiter {
            if delimiter_str.is_empty() {
                Some(NUL_DELIMITER)
            } else {
                delimiter_str.chars().next()
            }
        } else {
            Some(DEFAULT_DELIMITER)
        };

        let char_limit = self
            .return_after_n_chars_no_delimiter
            .or(self.return_after_n_chars);

        // Create the input reader.
        let mut reader = InputReader::new(
            input_file,
            timeout,
            term_mode,
            #[cfg(target_arch = "wasm32")]
            services,
        );

        // Handle -t 0 special case: just check if input is available without reading.
        if timeout == Some(Duration::ZERO) {
            return Ok(if reader.check_input_available() {
                ReadResult::InputReady
            } else {
                ReadResult::InputNotReady
            });
        }

        // Configure and perform the read.
        let config = LineReaderConfig {
            delimiter,
            char_limit,
            process_escapes: !self.raw_mode,
            terminal: reader.terminal,
        };

        read_line_with_reader(&mut reader, &config).await
    }

    fn setup_terminal_settings(
        &self,
        file: &brush_core::openfiles::OpenFile,
    ) -> Result<Option<brush_core::terminal::AutoModeGuard>, brush_core::Error> {
        let mode = brush_core::terminal::AutoModeGuard::new(file.to_owned()).ok();
        if let Some(mode) = &mode {
            let config = brush_core::terminal::Settings::builder()
                .line_input(false)
                .interrupt_signals(false)
                .echo_input(!self.silent)
                .build();

            mode.apply_settings(&config)?;
        }

        Ok(mode)
    }

    /// Validates the timeout value and returns an error result if invalid.
    ///
    /// Returns `Ok(Some(result))` if the timeout is invalid (caller should return early),
    /// `Ok(None)` if the timeout is valid or not specified.
    ///
    /// TODO(read): Bash uses $TMOUT as a default timeout for `read` when -t is not specified.
    #[cfg_attr(not(target_arch = "wasm32"), allow(clippy::unused_async))]
    async fn validate_timeout(
        &self,
        context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    ) -> Result<Option<brush_core::ExecutionResult>, brush_core::Error> {
        if let Some(timeout) = self.timeout_in_seconds {
            if Duration::try_from_secs_f64(timeout).is_err() {
                let message = format!(
                    "{}: -t: invalid timeout specification\n",
                    context.command_name
                );
                #[cfg(target_arch = "wasm32")]
                {
                    use futures::io::AsyncWriteExt;
                    context
                        .stderr()
                        .async_io()
                        .write_all(message.as_bytes())
                        .await?;
                }
                #[cfg(not(target_arch = "wasm32"))]
                context.stderr().write_all(message.as_bytes())?;
                return Ok(Some(brush_core::ExecutionResult::general_error()));
            }
        }
        Ok(None)
    }
}

/// Splits a line by IFS (Internal Field Separator) according to shell rules.
///
/// Shell IFS splitting has special rules:
/// - Whitespace IFS chars (space, tab, newline) are "IFS whitespace"
/// - Leading/trailing IFS whitespace is trimmed from the input
/// - Consecutive IFS whitespace chars act as a single delimiter
/// - Non-whitespace IFS chars each act as individual delimiters
/// - Trailing non-whitespace delimiter does NOT create an empty final field
///
/// # Arguments
/// * `ifs` - The IFS string (typically " \t\n")
/// * `line` - The input line to split
/// * `max_fields` - Optional limit on number of fields (for `read var1 var2`)
/// * `escapes` - Whether the line holds backslash escapes (see `input_units`)
fn split_line_by_ifs(
    ifs: &str,
    line: &str,
    max_fields: Option<usize>,
    escapes: bool,
) -> VecDeque<String> {
    let ifs_chars: Vec<char> = ifs.chars().collect();

    // Helper to check if a char is IFS whitespace (space, tab, or newline AND in IFS). An
    // escaped char is never a delimiter.
    let is_ifs_whitespace = |(c, escaped): &(char, bool)| -> bool {
        !escaped && (*c == ' ' || *c == '\t' || *c == '\n') && ifs_chars.contains(c)
    };

    // Trim leading/trailing IFS whitespace from the input.
    let units = input_units(line, escapes);
    let start = units
        .iter()
        .position(|unit| !is_ifs_whitespace(unit))
        .unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|unit| !is_ifs_whitespace(unit))
        .map_or(start, |last| last + 1);
    let trimmed_line = units.get(start..end).unwrap_or_default();
    if trimmed_line.is_empty() {
        return VecDeque::new();
    }

    let max_fields = max_fields.unwrap_or(usize::MAX);

    // State machine for splitting:
    // - `consuming_whitespace_run`: Currently skipping consecutive IFS whitespace
    // - `prev_was_non_ws_delim`: Previous char was a non-whitespace delimiter
    // - `collecting_remainder`: We've hit max_fields, collect everything into last field
    let mut fields = VecDeque::new();
    let mut current_field = String::new();
    let mut consuming_whitespace_run = false;
    let mut prev_was_non_ws_delim = false;
    let mut collecting_remainder = false;

    for &(c, escaped) in trimmed_line {
        // Skip consecutive IFS whitespace (they act as single delimiter).
        if consuming_whitespace_run && is_ifs_whitespace(&(c, escaped)) {
            continue;
        }
        consuming_whitespace_run = false;

        let is_delimiter = !escaped && ifs_chars.contains(&c);
        let at_field_limit = fields.len() + 1 >= max_fields;

        if !at_field_limit && is_delimiter {
            // Normal case: delimiter ends current field, start new one.
            fields.push_back(std::mem::take(&mut current_field));
            consuming_whitespace_run = is_ifs_whitespace(&(c, escaped));
            prev_was_non_ws_delim = !consuming_whitespace_run;
        } else if at_field_limit && !collecting_remainder && is_delimiter {
            // At field limit but haven't started last field content yet.
            // Skip leading IFS whitespace for the final field.
            if is_ifs_whitespace(&(c, escaped)) {
                consuming_whitespace_run = true;
            } else {
                // Non-whitespace delimiters at boundary: include in remainder.
                // e.g., "x::y" with IFS=":" and 2 vars gives ["x", ":y"]
                collecting_remainder = true;
                current_field.push(c);
            }
        } else {
            // Regular character: add to current field.
            collecting_remainder = at_field_limit;
            current_field.push(c);
            prev_was_non_ws_delim = false;
        }
    }

    // Finalize: push last field unless it's empty AND we ended with non-ws delimiter.
    // e.g., "a,b,c," with IFS="," gives ["a", "b", "c"], not ["a", "b", "c", ""].
    if !current_field.is_empty() || !prev_was_non_ws_delim {
        fields.push_back(current_field);
    }

    fields
}

/// The characters of a line read without `-r`, where a backslash escape (kept by
/// `read_line_with_reader`) marks the char after it as literal, each with whether it was
/// escaped; with `escapes` false, every char stands for itself.
fn input_units(line: &str, escapes: bool) -> Vec<(char, bool)> {
    let mut units = Vec::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if escapes && c == BACKSLASH {
            if let Some(escaped) = chars.next() {
                units.push((escaped, true));
                continue;
            }
        }
        units.push((c, false));
    }
    units
}

/// A line read without `-r` with its backslash escapes removed.
fn unescape_input(line: &str, escapes: bool) -> String {
    input_units(line, escapes)
        .into_iter()
        .map(|(c, _)| c)
        .collect()
}

#[cfg(test)]
mod tests {
    use itertools::assert_equal;

    use super::*;

    // ==================== UTF-8 decoding tests ====================

    // Decoding is otherwise covered end-to-end by the compat suite; what can't be
    // expressed there is a partial sequence that never completes, since it needs a
    // writer held open while `read` gives up on it.
    //
    // `-t` needs `poll_for_input`, which only the unix backend implements; elsewhere it
    // reports `Unsupported`, so there's no timeout to observe.
    #[cfg(unix)]
    #[test]
    fn test_read_times_out_mid_sequence_without_losing_the_partial_bytes() {
        let (rx, mut tx) = std::io::pipe().unwrap();
        tx.write_all(b"\xc3").unwrap();

        let mut reader = InputReader::new(rx.into(), Some(Duration::from_millis(100)), None);
        let config = LineReaderConfig {
            delimiter: Some(DEFAULT_DELIMITER),
            char_limit: None,
            process_escapes: false,
            terminal: false,
        };

        // Holding `tx` means the continuation byte never arrives, so the deadline has to
        // be enforced on it and not just on the byte that started the sequence.
        let result = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(read_line_with_reader(&mut reader, &config))
            .unwrap();
        drop(tx);

        // The partial sequence's byte is kept as the byte it is (see `rawbytes`).
        assert!(
            matches!(result, ReadResult::TimedOut(Some(line)) if line == brush_core::rawbytes::decode(b"\xc3"))
        );
    }

    // ==================== split_line_by_ifs tests ====================

    #[test]
    fn test_split_line_by_ifs_keeps_escaped_delimiters() {
        // Without -r, an escaped IFS character is literal, and the escapes are removed.
        let result = split_line_by_ifs(":", "a\\:b:c", Some(2), true);
        assert_equal(result, VecDeque::from(vec!["a:b", "c"]));
        let result = split_line_by_ifs(" ", "\\ a\\  b\\ ", None, true);
        assert_equal(result, VecDeque::from(vec![" a ", "b "]));
        // With -r, a backslash is an ordinary character.
        let result = split_line_by_ifs(":", "a\\:b", None, false);
        assert_equal(result, VecDeque::from(vec!["a\\", "b"]));
    }

    #[test]
    fn test_split_line_by_ifs_basic() {
        let result = split_line_by_ifs(",", "a,b,c", None, false);
        assert_equal(result, VecDeque::from(vec!["a", "b", "c"]));
    }

    #[test]
    fn test_split_line_by_ifs_leading_or_trailing_space() {
        let result = split_line_by_ifs(" ", "  a b c ", None, false);
        assert_equal(result, VecDeque::from(vec!["a", "b", "c"]));
    }

    #[test]
    fn test_split_line_by_ifs_extra_interior_space() {
        let result = split_line_by_ifs(" ", "a  b c", None, false);
        assert_equal(result, VecDeque::from(vec!["a", "b", "c"]));
    }

    #[test]
    fn test_split_line_by_ifs_leading_non_space_delimiter() {
        let result = split_line_by_ifs(",", ",a,b,c", None, false);
        assert_equal(result, VecDeque::from(vec!["", "a", "b", "c"]));
    }

    #[test]
    fn test_split_line_by_ifs_trailing_non_space_delimiter() {
        // Bash does NOT include empty trailing field when input ends with non-ws delimiter.
        let result = split_line_by_ifs(",", "a,b,c,", None, false);
        assert_equal(result, VecDeque::from(vec!["a", "b", "c"]));
    }

    #[test]
    fn test_split_line_by_ifs_max_fields() {
        // With max_fields=2, remainder goes into second field.
        let result = split_line_by_ifs(" ", "a b c d", Some(2), false);
        assert_equal(result, VecDeque::from(vec!["a", "b c d"]));
    }

    #[test]
    fn test_split_line_by_ifs_max_fields_with_non_ws_delimiter() {
        // With max_fields and non-whitespace delimiter.
        let result = split_line_by_ifs(",", "a,b,c,d", Some(2), false);
        assert_equal(result, VecDeque::from(vec!["a", "b,c,d"]));
    }

    #[test]
    fn test_split_line_by_ifs_consecutive_delimiters_at_boundary() {
        // Consecutive non-whitespace delimiters at field boundary should be preserved.
        // e.g., "x::y" with IFS=":" and 2 vars gives ["x", ":y"]
        let result = split_line_by_ifs(":", "x::y", Some(2), false);
        assert_equal(result, VecDeque::from(vec!["x", ":y"]));

        // Triple delimiter at boundary.
        let result = split_line_by_ifs(":", "x:::y", Some(2), false);
        assert_equal(result, VecDeque::from(vec!["x", "::y"]));

        // Delimiter in middle of remainder is also preserved.
        let result = split_line_by_ifs(":", "x:y:z:w", Some(2), false);
        assert_equal(result, VecDeque::from(vec!["x", "y:z:w"]));
    }

    #[test]
    fn test_split_line_by_ifs_mixed_delimiters() {
        // Mixed whitespace and non-whitespace in IFS.
        let result = split_line_by_ifs(": ", "a:b  c:d", None, false);
        assert_equal(result, VecDeque::from(vec!["a", "b", "c", "d"]));
    }

    #[test]
    fn test_split_line_by_ifs_empty_input() {
        let result = split_line_by_ifs(" ", "", None, false);
        assert_equal(result, VecDeque::<String>::new());
    }

    #[test]
    fn test_split_line_by_ifs_whitespace_only() {
        let result = split_line_by_ifs(" ", "   ", None, false);
        assert_equal(result, VecDeque::<String>::new());
    }

    #[test]
    fn test_split_line_by_ifs_consecutive_non_ws_delimiters() {
        // Consecutive non-whitespace delimiters create empty fields.
        let result = split_line_by_ifs(",", "a,,b", None, false);
        assert_equal(result, VecDeque::from(vec!["a", "", "b"]));
    }

    // ==================== build_array_fields tests ====================

    #[test]
    fn test_build_array_fields_basic() {
        let result = build_array_fields(Some("a b c"), " ", false, false);
        assert_eq!(
            result,
            vec![
                (None, "a".to_string()),
                (None, "b".to_string()),
                (None, "c".to_string())
            ]
        );
    }

    #[test]
    fn test_build_array_fields_skip_splitting() {
        // With -N option, entire input goes as single element.
        let result = build_array_fields(Some("a b c"), " ", false, true);
        assert_eq!(result, vec![(None, "a b c".to_string())]);
    }

    #[test]
    fn test_build_array_fields_none_input() {
        let result = build_array_fields(None, " ", false, false);
        assert!(result.is_empty());
    }

    // ==================== build_variable_fields tests ====================

    #[test]
    fn test_build_variable_fields_basic() {
        let result = build_variable_fields(Some("a b c"), " ", false, false, 3);
        assert_equal(result, VecDeque::from(vec!["a", "b", "c"]));
    }

    #[test]
    fn test_build_variable_fields_fewer_vars_than_fields() {
        // Last variable gets remainder.
        let result = build_variable_fields(Some("a b c d"), " ", false, false, 2);
        assert_equal(result, VecDeque::from(vec!["a", "b c d"]));
    }

    #[test]
    fn test_build_variable_fields_skip_splitting() {
        // With -N option, entire input goes to first variable.
        let result = build_variable_fields(Some("a b c"), " ", false, true, 3);
        assert_equal(result, VecDeque::from(vec!["a b c"]));
    }

    #[test]
    fn test_build_variable_fields_none_input() {
        let result = build_variable_fields(None, " ", false, false, 3);
        assert!(result.is_empty());
    }
}
