use brush_core::{ExecutionExitCode, ExecutionResult, builtins, history};
use clap::Parser;
use std::{
    io::Write,
    path::{Path, PathBuf},
};

/// Query or manipulate the shell's command history.
// TODO(history): Evaluate which of the options conflict with each other.
#[derive(Parser)]
#[expect(clippy::option_option)]
pub(crate) struct HistoryCommand {
    /// Clears all history.
    #[arg(short = 'c')]
    clear_history: bool,

    /// Deletes the history entry at the given offset. Positive offsets are relative to the
    /// beginning of the history, while negative offsets are relative to the end of the history.
    #[arg(short = 'd', value_name = "OFFSET")]
    delete_offset: Option<i64>,

    /// Appends the history from the current session to the history file.
    #[arg(short = 'a', group = "anrw", num_args = 0..=1, value_name = "HIST_FILE")]
    append_session_to_file: Option<Option<String>>,

    /// Appends any remaining history from the history file to the current session.
    #[arg(short = 'n', group = "anrw", num_args = 0..=1, value_name = "HIST_FILE")]
    append_rest_of_file_to_session: Option<Option<String>>,

    /// Appends the history from the history file to the current session.
    #[arg(short = 'r', group = "anrw", num_args = 0..=1, value_name = "HIST_FILE")]
    append_file_to_session: Option<Option<String>>,

    /// Replaces the history file with the current session history.
    #[arg(short = 'w', group = "anrw", num_args = 0..=1, value_name = "HIST_FILE")]
    write_session_to_file: Option<Option<String>>,

    /// History-expands positional arguments and displays them.
    #[arg(short = 'p', num_args = 0.., value_name = "ARG")]
    expand_args: Option<Vec<String>>,

    /// Appends positional arguments as an entry in the current session.
    #[arg(short = 's', num_args = 0.., value_name = "ARG")]
    append_args_to_session: Option<Vec<String>>,

    /// Arguments.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

struct HistoryConfig {
    default_history_file_path: Option<PathBuf>,
    time_format: Option<String>,
    diagnostic_prefix: String,
}

impl builtins::Command for HistoryCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        // Retrieve the shell's history config -- and the diagnostic prefix a numeric-argument
        // error needs -- while `context.shell` is still borrowed immutably; `history_or_init_mut()`
        // right below needs it mutably.
        let config = HistoryConfig {
            default_history_file_path: context.shell.history_file_path(),
            time_format: context.shell.history_time_format(),
            diagnostic_prefix: context.shell.diagnostic_prefix(),
        };

        let stdout = context.stdout();
        let stderr = context.stderr();

        // The builtin works on the list whether or not `set -o history` is on: that option only
        // gates automatic recording (done elsewhere, in the interactive layer), not whether the
        // list itself exists. See `Shell::history_or_init_mut`.
        let history = context.shell.history_or_init_mut();
        self.execute_with_history(history, &config, stdout, stderr)
    }
}

impl HistoryCommand {
    #[expect(clippy::cast_possible_wrap)]
    #[expect(clippy::cast_possible_truncation)]
    #[expect(clippy::cast_sign_loss)]
    fn execute_with_history(
        &self,
        history: &mut history::History,
        config: &HistoryConfig,
        mut stdout: impl Write,
        mut stderr: impl Write,
    ) -> Result<ExecutionResult, brush_core::Error> {
        if self.clear_history {
            history.clear()?;
        }

        if let Some(offset) = self.delete_offset {
            if offset == 0 {
                writeln!(stderr, "cannot delete history item at offset 0")?;
                return Ok(ExecutionExitCode::InvalidUsage.into());
            }

            if offset > 0 {
                // Convert to 0-based index.
                let index = (offset - 1) as usize;
                if !history.remove_nth_item(index) {
                    writeln!(stderr, "index past end of history")?;
                    return Ok(ExecutionExitCode::InvalidUsage.into());
                }
            } else {
                let count = history.count() as i64;
                let index = count + offset;
                if index < 0 {
                    writeln!(stderr, "index before beginning of history")?;
                    return Ok(ExecutionExitCode::InvalidUsage.into());
                }

                let _ = history.remove_nth_item(index as usize);
            }

            return Ok(ExecutionResult::success());
        }

        if let Some(append_option) = &self.append_session_to_file {
            if let Some(file_path) = get_effective_history_file_path(
                config.default_history_file_path.as_deref(),
                append_option.as_deref(),
            ) {
                history.flush(
                    file_path,
                    true,                         /* append? */
                    true,                         /* unsaved items only */
                    config.time_format.is_some(), /* write timestamps? */
                )?;
            }

            return Ok(ExecutionResult::success());
        }

        // `-n` ("remaining" entries not yet read into this session) and `-r` (the whole file)
        // differ, in real bash, only from what an earlier read of the same file in this session
        // (typically the interactive startup load) already brought in. This shell never
        // auto-loads `HISTFILE` at startup -- there is no interactive session to do it for -- so
        // nothing has been read from any file yet the first time either is called; the two are
        // the same operation here. (A script that called `-r FILE` and then `-n FILE` would see
        // FILE's lines twice, unlike real bash's cross-call tracking; that's the one corner this
        // doesn't cover.)
        if let Some(option) = &self.append_rest_of_file_to_session {
            if let Some(file_path) = get_effective_history_file_path(
                config.default_history_file_path.as_deref(),
                option.as_deref(),
            ) {
                append_file_to_session(history, file_path)?;
            }
            return Ok(ExecutionResult::success());
        }

        if let Some(option) = &self.append_file_to_session {
            if let Some(file_path) = get_effective_history_file_path(
                config.default_history_file_path.as_deref(),
                option.as_deref(),
            ) {
                append_file_to_session(history, file_path)?;
            }
            return Ok(ExecutionResult::success());
        }

        if let Some(write_option) = &self.write_session_to_file {
            if let Some(file_path) = get_effective_history_file_path(
                config.default_history_file_path.as_deref(),
                write_option.as_deref(),
            ) {
                history.flush(
                    file_path,
                    false,                        /* append? */
                    false,                        /* unsaved items only? */
                    config.time_format.is_some(), /* write timestamps? */
                )?;
            }

            return Ok(ExecutionResult::success());
        }

        if let Some(args) = &self.expand_args {
            return expand_and_print(
                args,
                history,
                &config.diagnostic_prefix,
                &mut stdout,
                &mut stderr,
            );
        }

        if let Some(args) = &self.append_args_to_session {
            history.add(history::Item::new(args.join(" ")))?;
            return Ok(ExecutionResult::success());
        }

        let max_entries: Option<usize> = if let Some(arg) = self.args.first() {
            match parse_count_arg(arg, &config.diagnostic_prefix, &mut stderr)? {
                Ok(value) => Some(value),
                Err(result) => return Ok(result),
            }
        } else {
            None
        };

        display_history(history, config, max_entries, stdout, stderr)?;

        Ok(ExecutionResult::success())
    }
}

fn display_history(
    history: &history::History,
    config: &HistoryConfig,
    max_entries: Option<usize>,
    mut stdout: impl Write,
    _stderr: impl Write,
) -> Result<(), brush_core::Error> {
    let item_count = history.count();
    let skip_count = item_count - max_entries.unwrap_or(item_count);

    for (i, item) in history.iter().skip(skip_count).enumerate() {
        let mut formatted_timestamp = String::new();

        if let Some(timestamp) = item.timestamp {
            let local_timestamp = timestamp.with_timezone(&chrono::Local);
            if let Some(time_format) = &config.time_format {
                let fmt_items = chrono::format::StrftimeItems::new(time_format);
                formatted_timestamp = local_timestamp.format_with_items(fmt_items).to_string();
            }
        }

        // Output format is something like:
        //     1  echo hello world
        std::writeln!(
            stdout,
            "{:>5}  {formatted_timestamp}{}",
            skip_count + i + 1,
            item.command_line
        )?;
    }

    Ok(())
}

/// Parses `history`'s own trailing `N` argument (how many recent entries to show); on failure,
/// writes bash's own wording for it (with its usual `NAME: line N: ` diagnostic prefix) and
/// returns the exit code to report instead.
fn parse_count_arg(
    arg: &str,
    diagnostic_prefix: &str,
    mut stderr: impl Write,
) -> Result<Result<usize, ExecutionResult>, brush_core::Error> {
    if let Ok(value) = arg.parse::<usize>() {
        Ok(Ok(value))
    } else {
        writeln!(
            stderr,
            "{diagnostic_prefix}history: {arg}: numeric argument required"
        )?;
        Ok(Err(ExecutionExitCode::InvalidUsage.into()))
    }
}

/// Implements `history -p`: expands each argument's `!`-event designators and prints the
/// result, or reports bash's own "history expansion failed" wording for one that doesn't
/// resolve -- matching bash's own per-argument handling (one bad reference doesn't stop the
/// rest from being printed).
fn expand_and_print(
    args: &[String],
    history: &history::History,
    diagnostic_prefix: &str,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> Result<ExecutionResult, brush_core::Error> {
    let mut failed = false;
    for arg in args {
        match expand_history_references(arg, history) {
            Ok(expanded) => writeln!(stdout, "{expanded}")?,
            Err(event) => {
                writeln!(
                    stderr,
                    "{diagnostic_prefix}history: {event}: history expansion failed"
                )?;
                failed = true;
            }
        }
    }
    Ok(if failed {
        ExecutionExitCode::GeneralError.into()
    } else {
        ExecutionResult::success()
    })
}

fn get_effective_history_file_path<'a>(
    default_history_file_path: Option<&'a Path>,
    option: Option<&'a str>,
) -> Option<&'a Path> {
    option.map(Path::new).or(default_history_file_path)
}

/// Reads `file_path` and appends each command line in it as a new history item, matching
/// `history::History::import`'s own timestamp-comment handling (`#SECONDS` lines set the
/// timestamp of the line that follows) and its tolerance of unreadable lines (skipped, with a
/// warning logged, rather than failing the whole read).
fn append_file_to_session(
    history: &mut history::History,
    file_path: impl AsRef<Path>,
) -> Result<(), brush_core::Error> {
    let file = std::fs::File::open(file_path.as_ref())?;
    let reader = std::io::BufReader::new(file);

    let mut next_timestamp = None;
    for line_result in std::io::BufRead::lines(reader) {
        let line = match line_result {
            Ok(line) => line,
            Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
                tracing::warn!("unreadable history line; {err}");
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        if let Some(comment) = line.strip_prefix('#') {
            next_timestamp = comment
                .trim()
                .parse()
                .ok()
                .and_then(|seconds| history::ItemTimestamp::from_timestamp(seconds, 0));
            continue;
        }

        history.add(history::Item {
            id: 0, // `History::add` assigns the real ID.
            command_line: line,
            timestamp: next_timestamp.take(),
            dirty: false,
        })?;
    }

    Ok(())
}

/// The most recent history item whose command line satisfies `matches`, searching from the
/// newest item backward (`History`'s iterator is oldest-first and not double-ended, so this
/// goes through `History::search` with `Direction::Backward` instead of reversing it).
fn most_recent_matching(
    history: &history::History,
    matches: impl Fn(&str) -> bool,
) -> Option<String> {
    let query = history::Query {
        direction: history::Direction::Backward,
        ..Default::default()
    };
    history
        .search(query)
        .ok()?
        .find(|item| matches(item.command_line.as_str()))
        .map(|item| item.command_line.clone())
}

/// Applies bash's `!`-event designators to `text` and returns the expanded result, or the
/// unresolved token (for a "history expansion failed" diagnostic) if one doesn't match anything.
///
/// Covers the designators bash documents as selecting an event: `!!` (the previous command),
/// `!N` (absolute item N), `!-N` (N items back), `!string` (the most recent item starting with
/// `string`) and `!?string?` (the most recent item containing `string`). Word designators and
/// modifiers (`:0`, `:$`, `:s/old/new/`, ...) are not covered: a `!` this doesn't recognize as
/// the start of one of these forms (including one followed by whitespace, `=` or `(`, which
/// bash itself never treats as the start of an event) is left in the output unexpanded, exactly
/// as bash leaves it.
fn expand_history_references(text: &str, history: &history::History) -> Result<String, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '!' || i + 1 >= chars.len() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let next = chars[i + 1];
        if next.is_whitespace() || next == '=' || next == '(' {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        let is_word_char = |c: char| !c.is_whitespace();
        let start = i;
        let event_end;
        let resolved: Option<String> = if next == '!' {
            event_end = i + 2;
            history
                .count()
                .checked_sub(1)
                .and_then(|index| history.get(index))
                .map(|item| item.command_line.clone())
        } else if next == '-' || next.is_ascii_digit() {
            let digits_start = if next == '-' { i + 2 } else { i + 1 };
            let mut end = digits_start;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            event_end = end;
            if end == digits_start {
                None
            } else {
                let digits: String = chars[digits_start..end].iter().collect();
                digits.parse::<usize>().ok().and_then(|n| {
                    let index = if next == '-' {
                        history.count().checked_sub(n)
                    } else {
                        n.checked_sub(1)
                    };
                    index
                        .and_then(|index| history.get(index))
                        .map(|item| item.command_line.clone())
                })
            }
        } else if next == '?' {
            let mut end = i + 2;
            while end < chars.len() && chars[end] != '?' {
                end += 1;
            }
            let needle: String = chars[i + 2..end].iter().collect();
            event_end = if end < chars.len() { end + 1 } else { end };
            most_recent_matching(history, |line| line.contains(needle.as_str()))
        } else {
            let mut end = i + 1;
            while end < chars.len() && is_word_char(chars[end]) {
                end += 1;
            }
            let prefix: String = chars[i + 1..end].iter().collect();
            event_end = end;
            most_recent_matching(history, |line| line.starts_with(prefix.as_str()))
        };

        match resolved {
            Some(command_line) => {
                out.push_str(&command_line);
                i = event_end;
            }
            None => return Err(chars[start..event_end].iter().collect()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use pretty_assertions::{assert_eq, assert_matches};

    #[test]
    fn test_parse_dash_a() -> Result<()> {
        let cmd = HistoryCommand::try_parse_from(["history", "5"])?;
        assert_matches!(cmd.append_session_to_file, None);

        let cmd = HistoryCommand::try_parse_from(["history", "-a"])?;
        assert_matches!(cmd.append_session_to_file, Some(None));

        let cmd = HistoryCommand::try_parse_from(["history", "-a", "token"])?;
        assert_eq!(
            cmd.append_session_to_file,
            Some(Some(String::from("token")))
        );

        Ok(())
    }
}
