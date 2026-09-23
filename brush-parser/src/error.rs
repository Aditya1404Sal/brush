use crate::tokenizer;

/// Represents an error that occurred while parsing tokens.
#[derive(thiserror::Error, Debug)]
pub enum ParseError {
    /// A parsing error occurred near the given position.
    #[error("syntax error at line {} col {}", .0.line, .0.column)]
    ParsingNear(crate::SourcePosition),

    /// A parsing error occurred at the end of the input.
    #[error("syntax error at end of input")]
    ParsingAtEndOfInput,

    /// An error occurred while tokenizing the input stream.
    #[error("{} (detected near {})", .inner, .position.as_ref().map_or_else(|| String::from("<unknown position>"), |p| std::format!("line {} col {}", p.line, p.column)))]
    Tokenizing {
        /// The inner error.
        inner: tokenizer::TokenizerError,
        /// Optionally provides the position of the error.
        position: Option<crate::SourcePosition>,
    },
}

#[cfg(feature = "diagnostics")]
#[allow(clippy::cast_sign_loss)]
#[allow(unused)] // Workaround unused warnings in nightly versions of the compiler
pub mod miette {
    use super::ParseError;
    use miette::SourceOffset;

    impl ParseError {
        /// Convert the original error to one miette can pretty print
        pub fn to_pretty_error(self, input: impl Into<String>) -> PrettyError {
            let input = input.into();
            let location = match self {
                Self::ParsingNear(ref pos) => {
                    Some(SourceOffset::from_location(&input, pos.line, pos.column))
                }
                Self::Tokenizing { ref position, .. } => position
                    .as_ref()
                    .map(|p| SourceOffset::from_location(&input, p.line, p.column)),
                Self::ParsingAtEndOfInput => {
                    Some(SourceOffset::from_location(&input, usize::MAX, usize::MAX))
                }
            };

            PrettyError {
                cause: self,
                input,
                location,
            }
        }
    }

    /// Represents an error that occurred while parsing tokens.
    #[derive(thiserror::Error, Debug, miette::Diagnostic)]
    #[error("Cannot parse the input script")]
    pub struct PrettyError {
        cause: ParseError,
        #[source_code]
        input: String,
        #[label("{cause}")]
        location: Option<SourceOffset>,
    }
}

/// Represents a parsing error with its location information
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct ParseErrorLocation {
    #[from]
    inner: peg::error::ParseError<peg::str::LineCol>,
}

/// Represents an error that occurred while parsing a word.
#[derive(Debug, thiserror::Error)]
pub enum WordParseError {
    /// An error occurred while parsing an arithmetic expression.
    #[error("failed to parse arithmetic expression")]
    ArithmeticExpression(ParseErrorLocation),

    /// An error occurred while parsing a shell pattern.
    #[error("failed to parse pattern")]
    Pattern(ParseErrorLocation),

    /// An error occurred while parsing a prompt string.
    #[error("failed to parse prompt string")]
    Prompt(ParseErrorLocation),

    /// An error occurred while parsing a parameter.
    #[error("failed to parse parameter '{0}'")]
    Parameter(String, ParseErrorLocation),

    /// An error occurred while parsing for brace expansion.
    #[error("failed to parse for brace expansion: '{0}'")]
    BraceExpansion(String, ParseErrorLocation),

    /// An error occurred while parsing a word.
    #[error("failed to parse word '{0}'")]
    Word(String, ParseErrorLocation),
}

/// Represents an error that occurred while parsing a (non-extended) test command.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct TestCommandParseError(#[from] peg::error::ParseError<usize>);

/// Represents an error that occurred while parsing a key-binding specification.
#[derive(Debug, thiserror::Error)]
pub enum BindingParseError {
    /// An unknown error occurred while parsing a key-binding specification.
    #[error("unknown error while parsing key-binding: '{0}'")]
    Unknown(String),

    /// A key code was missing from the key-binding specification.
    #[error("missing key code in key-binding")]
    MissingKeyCode,
}

/// A parse error as bash words it: the lines bash writes after its `NAME: ORIGIN: ` prefix.
///
/// `source` is the text that failed to parse. A misplaced token is named along with the line that
/// holds it; an unclosed compound command is named with the line where it began; an unterminated
/// quote is reported at its opening line and an unterminated substitution at the end of the input.
pub fn bash_diagnostic(
    error: &ParseError,
    source: &str,
    options: &crate::ParserOptions,
) -> Vec<String> {
    let end_line = source.lines().count() + 1;
    let tokens = || {
        tokenizer::tokenize_str_with_options(source, &options.tokenizer_options())
            .unwrap_or_default()
    };
    let near = |token: &str, line: usize| {
        let text = source
            .lines()
            .nth(line.saturating_sub(1))
            .unwrap_or_default();
        vec![
            std::format!("line {line}: syntax error near unexpected token `{token}'"),
            std::format!("line {line}: `{text}'"),
        ]
    };
    match error {
        ParseError::ParsingNear(position) => {
            let (token, line) = unexpected_token(&tokens(), position, source);
            near(&token, line)
        }
        ParseError::ParsingAtEndOfInput => {
            let tokens = tokens();
            if let Some((token, line)) = misplaced_last(&tokens) {
                near(token, line)
            } else if let Some((keyword, line)) = unclosed_command(&tokens) {
                vec![std::format!(
                    "line {end_line}: syntax error: unexpected end of file from `{keyword}' command on line {line}"
                )]
            } else {
                vec![std::format!(
                    "line {end_line}: syntax error: unexpected end of file"
                )]
            }
        }
        ParseError::Tokenizing { inner, .. } => {
            use tokenizer::TokenizerError as T;
            let (line, closing) = match inner {
                T::UnterminatedDoubleQuote(position) => (position.line, '"'),
                T::UnterminatedSingleQuote(position) | T::UnterminatedAnsiCQuote(position) => {
                    (position.line, '\'')
                }
                T::UnterminatedBackquote(position) => (position.line, '`'),
                T::UnterminatedExtendedGlob(_) | T::UnterminatedCommandSubstitution => {
                    (end_line, ')')
                }
                T::UnterminatedExpansion(closing) => (end_line, *closing),
                T::UnterminatedVariable => (end_line, '}'),
                _ => return vec![std::format!("line {end_line}: syntax error: {inner}")],
            };
            vec![std::format!(
                "line {line}: unexpected EOF while looking for matching `{closing}'"
            )]
        }
    }
}

/// A reserved word that ends the input where a command was expected (`if then`): bash names it
/// rather than the end of the input.
fn misplaced_last(tokens: &[crate::Token]) -> Option<(&str, usize)> {
    const RESERVED: [&str; 8] = ["then", "else", "elif", "fi", "do", "done", "esac", "}"];
    const WANT_COMMAND: [&str; 9] = [
        "if", "elif", "while", "until", "then", "else", "do", "{", "!",
    ];
    let [.., before, last] = tokens else {
        return None;
    };
    (RESERVED.contains(&last.to_str()) && WANT_COMMAND.contains(&before.to_str()))
        .then(|| (last.to_str(), last.location().start.line))
}

/// The token bash would name for a parse failure at `position`, and its line.
fn unexpected_token(
    tokens: &[crate::Token],
    position: &crate::SourcePosition,
    source: &str,
) -> (String, usize) {
    // A reserved word out of place parses as an ordinary word, so the parser fails one token
    // later, at the separator after it; bash names the reserved word.
    const RESERVED: [&str; 8] = ["then", "else", "elif", "fi", "do", "done", "esac", "}"];
    let Some(mut index) = tokens
        .iter()
        .position(|token| token.location().start.index == position.index)
    else {
        let rest = source.get(position.index..).unwrap_or_default();
        let word = rest.split_whitespace().next().unwrap_or("newline");
        return (word.to_owned(), position.line);
    };
    if index > 0
        && matches!(tokens[index].to_str(), ";" | "\n" | "&")
        && RESERVED.contains(&tokens[index - 1].to_str())
    {
        index -= 1;
    }
    let token = &tokens[index];
    let text = match token.to_str() {
        "\n" => "newline",
        text => text,
    };
    (text.to_owned(), token.location().start.line)
}

/// The innermost compound command still open at the end of `tokens`, and the line it began on.
fn unclosed_command(tokens: &[crate::Token]) -> Option<(String, usize)> {
    let mut open: Vec<(&str, usize)> = Vec::new();
    let mut command_start = true;
    for token in tokens {
        let text = token.to_str();
        let line = token.location().start.line;
        let close = |open: &mut Vec<(&str, usize)>, openers: &[&str]| {
            if open
                .last()
                .is_some_and(|(keyword, _)| openers.contains(keyword))
            {
                open.pop();
            }
        };
        match token {
            crate::Token::Operator(..) => {
                match text {
                    "(" if command_start => open.push(("(", line)),
                    ")" => close(&mut open, &["("]),
                    _ => {}
                }
                command_start = matches!(
                    text,
                    ";" | "\n" | "&" | "&&" | "||" | "|" | "|&" | "(" | ")" | ";;" | ";&" | ";;&"
                );
            }
            crate::Token::Word(..) if command_start => {
                match text {
                    "if" | "for" | "while" | "until" | "case" | "select" | "{" => {
                        open.push((text, line));
                    }
                    "fi" => close(&mut open, &["if"]),
                    "done" => close(&mut open, &["for", "while", "until", "select"]),
                    "esac" => close(&mut open, &["case"]),
                    "}" => close(&mut open, &["{"]),
                    _ => {}
                }
                command_start = matches!(
                    text,
                    "if" | "then" | "else" | "elif" | "while" | "until" | "do" | "{" | "!" | "time"
                );
            }
            crate::Token::Word(..) => command_start = false,
        }
    }
    open.last()
        .map(|(keyword, line)| ((*keyword).to_owned(), *line))
}

pub(crate) fn convert_peg_parse_error(
    err: &peg::error::ParseError<usize>,
    tokens: &[crate::Token],
) -> ParseError {
    let approx_token_index = err.location;

    if approx_token_index < tokens.len() {
        let token = &tokens[approx_token_index];
        ParseError::ParsingNear((*token.location().start).clone())
    } else {
        ParseError::ParsingAtEndOfInput
    }
}

#[cfg(test)]
mod tests {
    use super::bash_diagnostic;

    fn diagnose(source: &str) -> Vec<String> {
        let options = crate::ParserOptions::default();
        let mut reader = std::io::BufReader::new(source.as_bytes());
        crate::Parser::new(&mut reader, &options)
            .parse_program()
            .err()
            .map(|error| bash_diagnostic(&error, source, &options))
            .unwrap_or_default()
    }

    #[test]
    fn names_the_unexpected_token_and_its_line() {
        assert_eq!(
            diagnose("echo first; if then; echo after"),
            [
                "line 1: syntax error near unexpected token `then'",
                "line 1: `echo first; if then; echo after'",
            ]
        );
        assert_eq!(
            diagnose("echo a\nfi\necho b"),
            [
                "line 2: syntax error near unexpected token `fi'",
                "line 2: `fi'"
            ]
        );
        assert_eq!(
            diagnose("f() { echo in; }\nf\ng() { if; }\necho after")[0],
            "line 3: syntax error near unexpected token `;'"
        );
        assert_eq!(
            diagnose("echo a;echo b\n\n\necho c ;; echo d")[0],
            "line 4: syntax error near unexpected token `;;'"
        );
        assert_eq!(
            diagnose("if then"),
            [
                "line 1: syntax error near unexpected token `then'",
                "line 1: `if then'"
            ]
        );
    }

    #[test]
    fn names_the_unclosed_command_at_end_of_input() {
        assert_eq!(
            diagnose("for i in 1; do echo $i"),
            ["line 2: syntax error: unexpected end of file from `for' command on line 1"]
        );
        assert_eq!(
            diagnose("echo ok; if"),
            ["line 2: syntax error: unexpected end of file from `if' command on line 1"]
        );
        assert_eq!(
            diagnose("if true; then\n  while x; do\n    echo if done\n")[0],
            "line 4: syntax error: unexpected end of file from `while' command on line 2"
        );
    }

    #[test]
    fn reports_unterminated_quotes_and_substitutions() {
        assert_eq!(
            diagnose("echo first\necho \"open"),
            ["line 2: unexpected EOF while looking for matching `\"'"]
        );
        assert_eq!(
            diagnose("echo first\necho $(echo x"),
            ["line 3: unexpected EOF while looking for matching `)'"]
        );
    }
}
