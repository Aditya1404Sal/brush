use crate::{
    ExecutionParameters, error, expansion, extensions,
    shell::Shell,
    sys::{self, users},
};
use std::io::Write as _;
use std::path::Path;

const VERSION_MAJOR: &str = env!("CARGO_PKG_VERSION_MAJOR");
const VERSION_MINOR: &str = env!("CARGO_PKG_VERSION_MINOR");
const VERSION_PATCH: &str = env!("CARGO_PKG_VERSION_PATCH");

pub(crate) async fn expand_prompt(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    spec: &str,
) -> Result<String, error::Error> {
    // A prompt whose expansion expands a prompt (`x='${x@P}'`) nests like code does: one the
    // stack cannot hold ends the shell rather than trapping (bash itself overflows its stack).
    #[cfg(target_arch = "wasm32")]
    {
        if shell.nesting >= crate::shell::MAX_NESTING
            || crate::sys::wasm::stack::remaining() < crate::shell::STACK_RESERVE
        {
            return Err(error::Error::from(error::ErrorKind::NestingTooDeep).into_fatal());
        }
        shell.nesting += 1;
        let mut frame = crate::shell::FrameGuard::new(shell, leave_prompt, None);
        let result = expand_prompt_text(frame.shell(), params, spec).await;
        frame.finish()?;
        result
    }
    #[cfg(not(target_arch = "wasm32"))]
    expand_prompt_text(shell, params, spec).await
}

/// Leaves a prompt expansion entered by [`expand_prompt`].
#[cfg(target_arch = "wasm32")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "a frame guard's cleanup is fallible"
)]
const fn leave_prompt(
    shell: &mut Shell<impl extensions::ShellExtensions>,
) -> Result<(), error::Error> {
    shell.nesting = shell.nesting.saturating_sub(1);
    Ok(())
}

async fn expand_prompt_text(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    spec: &str,
) -> Result<String, error::Error> {
    // Parse the prompt spec into its pieces.
    let prompt_pieces = parse_prompt(spec)?;

    // Now, render each piece.
    let mut formatted_prompt = String::new();
    for piece in prompt_pieces {
        // Pieces that semantically represent a literal char (e.g. user wrote
        // `\$` meaning a literal `$`, not input for pass-2 expansion). We
        // prepend a `\` so pass 2 consumes it and leaves the char alone.
        let semantically_literal =
            matches!(piece, brush_parser::prompt::PromptPiece::DollarOrPound);

        let formatted_piece = format_prompt_piece(shell, piece)?;

        // Only useful when pass 2 actually consumes a `\` for that leading
        // byte; otherwise the `\` would leak through.
        if shell.options().expand_prompt_strings
            && semantically_literal
            && formatted_piece.starts_with(expansion::DOUBLE_QUOTED_ESCAPE_CHARS)
        {
            formatted_prompt.push('\\');
        }

        formatted_prompt.push_str(&formatted_piece);
    }

    if shell.options().expand_prompt_strings {
        // An embedder that checks code before the shell runs it sees the text first: a refused
        // prompt runs none of its expansions.
        if let Some(guard) = shell.prompt_guard()
            && let Err(diagnostic) = guard(&formatted_prompt)
        {
            write!(params.stderr(shell), "{diagnostic}")?;
            return Err(
                error::Error::from(error::ErrorKind::PromptRefused(diagnostic)).into_reported(),
            );
        }

        // A command substitution whose command does not parse ends the prompt, as in bash, which
        // then runs what it took for its command (see [`run_broken_substitution`]).
        let written = formatted_prompt.clone();
        let broken = broken_substitution(&formatted_prompt, &shell.parser_options())
            .map(|start| formatted_prompt.split_off(start).split_off(2));

        // Now expand any remaining escape sequences, but without tilde-expansion.
        // Use double-quote escape rules so that backslashes emitted in the
        // previous step survive intact unless they precede a character that
        // would also be escapable inside a double-quoted string.
        let options = expansion::ExpanderOptions {
            tilde_expand: false,
            brace_expand: false,
            unquoted_backslash_handling: expansion::UnquotedBackslashHandling::DoubleQuoted,
            ..Default::default()
        };
        formatted_prompt = match expansion::basic_expand_word_with_options(
            shell,
            params,
            &formatted_prompt,
            &options,
        )
        .await
        {
            Ok(expanded) => expanded,
            // A prompt whose expansion fails (an arithmetic error, a bad substitution, an unset
            // variable under `set -u`) is reported and used as it reads, as in bash: the command
            // using it goes on, and the shell does not exit.
            Err(error)
                if matches!(
                    error.kind(),
                    error::ErrorKind::ExpandingUnsetVariable(_)
                        | error::ErrorKind::CheckedExpansionError(_)
                        | error::ErrorKind::BadSubstitution(_)
                ) || !error.is_fatal()
                    && !matches!(error.kind(), error::ErrorKind::PromptRefused(_)) =>
            {
                if !error.is_reported() {
                    let _ = shell.display_error(&mut params.stderr(shell), &error);
                }
                return Ok(written);
            }
            Err(error) => return Err(error),
        };
        if let Some(rest) = broken {
            let output = run_broken_substitution(shell, params, rest).await?;
            formatted_prompt.push_str(&output);
        }
    }

    Ok(formatted_prompt)
}

/// Where the first `$(` in `text` starts whose command does not parse, if any: one inside
/// another expansion (`${x:-$(fi)}`) is left to that expansion.
fn broken_substitution(text: &str, options: &brush_parser::ParserOptions) -> Option<usize> {
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        let from = text.get(index..).unwrap_or_default();
        match c {
            '\\' => {
                chars.next();
            }
            '$' if from.starts_with("$((") => {
                chars.next();
                chars.next();
            }
            '$' if from.starts_with("$(") => {
                let Some(length) = substitution_length(from, options) else {
                    let prefix = text.get(..index).unwrap_or_default();
                    return (prefix.is_empty()
                        || brush_parser::word::parse(prefix, options).is_ok())
                    .then_some(index);
                };
                while chars.peek().is_some_and(|(next, _)| *next < index + length) {
                    chars.next();
                }
            }
            _ => (),
        }
    }
    None
}

/// The length of the command substitution `text` starts with, when its command parses: the
/// substitution the text is read as, or when the text as a whole is no word (an open quote after
/// it), the first `)` that ends one.
fn substitution_length(text: &str, options: &brush_parser::ParserOptions) -> Option<usize> {
    let substitution = |text: &str| match brush_parser::word::parse(text, options)
        .ok()?
        .into_iter()
        .next()
    {
        Some(brush_parser::word::WordPieceWithSource {
            piece: brush_parser::word::WordPiece::CommandSubstitution(command),
            end_index,
            ..
        }) => Some(parses(&command, options).then_some(end_index)),
        _ => None,
    };
    match substitution(text) {
        Some(length) => length,
        None => text
            .match_indices(')')
            .find_map(|(close, _)| substitution(text.get(..=close)?).flatten()),
    }
}

/// Whether `text` parses as a program.
fn parses(text: &str, options: &brush_parser::ParserOptions) -> bool {
    brush_parser::Parser::new(text.as_bytes(), options)
        .parse_program()
        .is_ok()
}

/// Runs a command substitution whose command does not parse as bash does, `rest` being the
/// prompt's text after its `$(`: bash reports the syntax error its parser meets looking for the
/// `)`, then takes the rest of the prompt but its last character for the command, and runs that
/// (reporting its own syntax error when it does not parse either). Its output ends the prompt.
async fn run_broken_substitution(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    rest: String,
) -> Result<String, error::Error> {
    let options = shell.parser_options();
    let error = error::Error::from(error::ErrorKind::SyntaxError {
        origin: "command substitution".to_owned(),
        lines: unclosed_substitution_lines(&rest, &options, shell.line_number()),
    });
    let _ = shell.display_error(&mut params.stderr(shell), &error);

    let mut command = rest;
    command.pop();
    if parses(&command, &options)
        && let Some(guard) = shell.prompt_guard()
    {
        // The embedder checks the command as the substitution it would be in a prompt; one that
        // text cannot hold (its `)` would end a comment or a here-document early) is not run.
        let text = format!("$({command})");
        let whole = brush_parser::word::parse(&text, &options).is_ok_and(|pieces| {
            matches!(pieces.as_slice(), [brush_parser::word::WordPieceWithSource {
                piece: brush_parser::word::WordPiece::CommandSubstitution(body),
                ..
            }] if *body == command)
        });
        if !whole {
            return Ok(String::new());
        }
        if let Err(diagnostic) = guard(&text) {
            write!(params.stderr(shell), "{diagnostic}")?;
            return Err(
                error::Error::from(error::ErrorKind::PromptRefused(diagnostic)).into_reported(),
            );
        }
    }
    expansion::command_substitution_output(shell, params, command, false).await
}

/// What bash reports as it looks for the `)` of a substitution in a prompt read on `line`, the
/// prompt's text after the `$(` being `rest`: the syntax error in it, numbered on from `line`
/// and, when the error is at a token other than the `)`, saying so; or, when it ends first, the
/// `)` it did not find.
fn unclosed_substitution_lines(
    rest: &str,
    options: &brush_parser::ParserOptions,
    line: usize,
) -> Vec<String> {
    let lines = brush_parser::Parser::new(rest.as_bytes(), options)
        .parse_program()
        .err()
        .map(|error| brush_parser::bash_diagnostic(&error, rest, options))
        .filter(|lines| {
            lines
                .first()
                .is_some_and(|first| !first.contains("syntax error: unexpected end of file"))
        });
    let Some(lines) = lines else {
        let end = line + rest.matches('\n').count() + 2;
        return vec![format!(
            "line {end}: unexpected EOF while looking for matching `)'"
        )];
    };
    let mut lines = error::shift_diagnostic_lines(lines, line);
    if let Some(first) = lines.first_mut()
        && first.contains("syntax error near unexpected token")
        && !first.ends_with("token `)'")
    {
        first.push_str(" while looking for matching `)'");
    }
    lines
}

#[cached::macros::cached(max_size = 64, key = "String", convert = r#"{ spec.to_owned() }"#)]
fn parse_prompt(
    spec: &str,
) -> Result<Vec<brush_parser::prompt::PromptPiece>, brush_parser::WordParseError> {
    brush_parser::prompt::parse(spec)
}

fn format_prompt_piece(
    shell: &Shell<impl extensions::ShellExtensions>,
    piece: brush_parser::prompt::PromptPiece,
) -> Result<String, error::Error> {
    let formatted = match piece {
        brush_parser::prompt::PromptPiece::EscapedSequence(s) => s,
        brush_parser::prompt::PromptPiece::Literal(l) => l,
        brush_parser::prompt::PromptPiece::AsciiCharacter(c) => {
            char::from_u32(c).map_or_else(String::new, |c| c.to_string())
        }
        brush_parser::prompt::PromptPiece::Backslash => "\\".to_owned(),
        brush_parser::prompt::PromptPiece::BellCharacter => "\x07".to_owned(),
        brush_parser::prompt::PromptPiece::CarriageReturn => "\r".to_owned(),
        brush_parser::prompt::PromptPiece::CurrentCommandNumber => {
            return error::unimp("prompt: current command number");
        }
        brush_parser::prompt::PromptPiece::CurrentHistoryNumber => {
            return error::unimp("prompt: current history number");
        }
        brush_parser::prompt::PromptPiece::CurrentUser => users::get_current_username()?,
        brush_parser::prompt::PromptPiece::CurrentWorkingDirectory {
            tilde_replaced,
            basename,
        } => format_current_working_directory(shell, tilde_replaced, basename),
        brush_parser::prompt::PromptPiece::Date(format) => {
            format_date(&chrono::Local::now(), &format)
        }
        brush_parser::prompt::PromptPiece::DollarOrPound => {
            if users::is_root() {
                "#".to_owned()
            } else {
                "$".to_owned()
            }
        }
        // NOTE: We mimic bash and convert \[ into \001, a.k.a. RL_PROMPT_START_IGNORE.
        // It will need to get removed before it's actually displayed. While present it
        // also has the important (compatible) side effect of ensuring the text on either
        // side of it is not concatenated together, potentially resulting in incompatible
        // variable expansions. Also, we *only* do this if the shell is interactive.
        brush_parser::prompt::PromptPiece::EndNonPrintingSequence => {
            if shell.options().interactive {
                "\x02".to_owned()
            } else {
                String::new()
            }
        }
        brush_parser::prompt::PromptPiece::EscapeCharacter => "\x1b".to_owned(),
        brush_parser::prompt::PromptPiece::Hostname {
            only_up_to_first_dot,
        } => {
            let hn = sys::network::get_hostname()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if only_up_to_first_dot && let Some((first, _)) = hn.split_once('.') {
                return Ok(first.to_owned());
            }
            hn
        }
        brush_parser::prompt::PromptPiece::Newline => "\n".to_owned(),
        brush_parser::prompt::PromptPiece::NumberOfManagedJobs => {
            shell.jobs().jobs.len().to_string()
        }
        brush_parser::prompt::PromptPiece::ShellBaseName => {
            if let Some(shell_name) = shell.current_shell_name() {
                Path::new(shell_name.as_ref())
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            }
        }
        brush_parser::prompt::PromptPiece::ShellRelease => {
            std::format!("{VERSION_MAJOR}.{VERSION_MINOR}.{VERSION_PATCH}")
        }
        brush_parser::prompt::PromptPiece::ShellVersion => {
            std::format!("{VERSION_MAJOR}.{VERSION_MINOR}")
        }
        // NOTE: See above note for EndNonPrintingSequence
        brush_parser::prompt::PromptPiece::StartNonPrintingSequence => {
            if shell.options().interactive {
                "\x01".to_owned()
            } else {
                String::new()
            }
        }
        brush_parser::prompt::PromptPiece::TerminalDeviceBaseName => {
            sys::terminal::try_get_terminal_device_path()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
                .unwrap_or_default()
        }
        brush_parser::prompt::PromptPiece::Time(time_fmt) => {
            format_time(&chrono::Local::now(), &time_fmt)
        }
    };

    Ok(formatted)
}

fn format_current_working_directory(
    shell: &Shell<impl extensions::ShellExtensions>,
    tilde_replaced: bool,
    basename: bool,
) -> String {
    let mut working_dir_str = shell.working_dir().to_string_lossy().to_string();

    if tilde_replaced {
        working_dir_str = shell.tilde_shorten(working_dir_str);
    }

    if basename && let Some(filename) = Path::new(&working_dir_str).file_name() {
        working_dir_str = filename.to_string_lossy().to_string();
    }

    if cfg!(windows) {
        working_dir_str = working_dir_str.replace('\\', "/");
    }

    working_dir_str
}

fn format_time<Tz: chrono::TimeZone>(
    datetime: &chrono::DateTime<Tz>,
    format: &brush_parser::prompt::PromptTimeFormat,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let formatted = match format {
        brush_parser::prompt::PromptTimeFormat::TwelveHourAM => datetime.format("%I:%M %p"),
        brush_parser::prompt::PromptTimeFormat::TwelveHourHHMMSS => datetime.format("%I:%M:%S"),
        brush_parser::prompt::PromptTimeFormat::TwentyFourHourHHMM => datetime.format("%H:%M"),
        brush_parser::prompt::PromptTimeFormat::TwentyFourHourHHMMSS => datetime.format("%H:%M:%S"),
    };

    formatted.to_string()
}

fn format_date<Tz: chrono::TimeZone>(
    datetime: &chrono::DateTime<Tz>,
    format: &brush_parser::prompt::PromptDateFormat,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match format {
        brush_parser::prompt::PromptDateFormat::WeekdayMonthDate => {
            datetime.format("%a %b %d").to_string()
        }
        brush_parser::prompt::PromptDateFormat::Custom(fmt) => {
            let fmt_items = chrono::format::StrftimeItems::new(fmt);
            datetime.format_with_items(fmt_items).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_time() {
        // Create a well-known test date/time.
        let dt = chrono::DateTime::parse_from_rfc3339("2024-12-25T13:34:56.789Z").unwrap();

        assert_eq!(
            format_time(&dt, &brush_parser::prompt::PromptTimeFormat::TwelveHourAM),
            "01:34 PM"
        );

        assert_eq!(
            format_time(
                &dt,
                &brush_parser::prompt::PromptTimeFormat::TwentyFourHourHHMMSS
            ),
            "13:34:56"
        );

        assert_eq!(
            format_time(
                &dt,
                &brush_parser::prompt::PromptTimeFormat::TwelveHourHHMMSS
            ),
            "01:34:56"
        );
    }

    #[test]
    fn test_format_date() {
        // Create a well-known test date/time.
        let dt = chrono::DateTime::parse_from_rfc3339("2024-12-25T12:34:56.789Z").unwrap();

        assert_eq!(
            format_date(
                &dt,
                &brush_parser::prompt::PromptDateFormat::WeekdayMonthDate
            ),
            "Wed Dec 25"
        );

        assert_eq!(
            format_date(
                &dt,
                &brush_parser::prompt::PromptDateFormat::Custom(String::from("%Y-%m-%d"))
            ),
            "2024-12-25"
        );

        assert_eq!(
            format_date(
                &dt,
                &brush_parser::prompt::PromptDateFormat::Custom(String::from(
                    "%Y-%m-%d %H:%M:%S.%f"
                ))
            ),
            "2024-12-25 12:34:56.789000000"
        );
    }
}
