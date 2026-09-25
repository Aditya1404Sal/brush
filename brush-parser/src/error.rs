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

    /// The text nests brackets or substitutions deeper than the word parser can hold.
    #[error("maximum nesting level exceeded: deeper nesting is unsupported in bash-tool")]
    NestedTooDeeply,
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
    // Bash reads the end of a command string as the end of a line, so what fails at the end of
    // input without a final newline (`echo first; fi`) fails at that newline, where bash names
    // the token it could not use.
    if matches!(error, ParseError::ParsingAtEndOfInput) && !source.ends_with('\n') {
        let with_newline = std::format!("{source}\n");
        let mut reader = std::io::BufReader::new(with_newline.as_bytes());
        if let Err(error @ ParseError::ParsingNear(_)) =
            crate::Parser::new(&mut reader, options).parse_program()
        {
            return bash_diagnostic(&error, &with_newline, options);
        }
    }
    let end_line = source.lines().count() + 1;
    // In a conditional command, bash's own parser for them words the error.
    let failed_at = match error {
        ParseError::ParsingNear(position) => Some(Some(position.index)),
        ParseError::ParsingAtEndOfInput => Some(None),
        ParseError::Tokenizing { .. } => None,
    };
    if let Some(lines) =
        failed_at.and_then(|failed_at| conditional_diagnostic(source, options, failed_at))
    {
        return lines;
    }
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
            if let Some(line) = open_compound_assignment(&tokens, tokens.len()) {
                // Bash reads the words of a compound assignment apart, to their `)`.
                vec![std::format!(
                    "line {line}: unexpected EOF while looking for matching `)'"
                )]
            } else if let Some((token, line)) = misplaced_last(&tokens) {
                near(token, line)
            } else if let Some(line) = open_function_parenthesis(&tokens) {
                near("newline", line)
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

/// Bash's diagnostic for the first conditional command (`[[ ... ]]`), up to where the parse
/// failed (`failed_at`, a character index; `None` for the end of the input), that bash's parser
/// for them rejects, or `None` when there is none.
fn conditional_diagnostic(
    source: &str,
    options: &crate::ParserOptions,
    failed_at: Option<usize>,
) -> Option<Vec<String>> {
    // The input ends with a newline, as bash reads it.
    let text = if source.ends_with('\n') {
        std::borrow::Cow::Borrowed(source)
    } else {
        std::borrow::Cow::Owned(std::format!("{source}\n"))
    };
    let tokens = tokenizer::tokenize_str_with_options(&text, &options.tokenizer_options()).ok()?;
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let mut command_start = true;
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        if failed_at.is_some_and(|at| token.location().start.index > at) {
            return None;
        }
        let word = token.to_str();
        if command_start && matches!(token, crate::Token::Word(..)) && word == "[[" {
            let mut conditional = Conditional {
                tokens: &tokens,
                lines: &lines,
                next: index + 1,
                cond_token: CondToken::Error,
                failed: CondToken::Error,
                messages: vec![],
            };
            match conditional.command(token.location().start.line) {
                Ok(next) => {
                    index = next;
                    command_start = false;
                    continue;
                }
                Err(diagnostic) => return Some(diagnostic),
            }
        }
        command_start = match token {
            crate::Token::Operator(..) => matches!(
                word,
                ";" | "\n" | "&" | "&&" | "||" | "|" | "|&" | "(" | ")" | ";;" | ";&" | ";;&"
            ),
            crate::Token::Word(..) => {
                command_start
                    && matches!(
                        word,
                        "if" | "then"
                            | "else"
                            | "elif"
                            | "while"
                            | "until"
                            | "do"
                            | "{"
                            | "!"
                            | "time"
                    )
            }
        };
        index += 1;
    }
    None
}

/// What bash's conditional parser holds as its current token.
#[derive(Clone, Copy)]
enum CondToken {
    /// The token at this index.
    At(usize),
    /// The end of the input.
    End,
    /// An error was found (bash's `COND_ERROR`).
    Error,
}

/// Bash's parser for conditional commands (`cond_expr`, `cond_term` and the rest in bash's
/// `parse.y`), run over the tokens after a `[[` to find the error bash reports and word it.
struct Conditional<'a> {
    tokens: &'a [crate::Token],
    /// The lines of the input, each with its newline.
    lines: &'a [&'a str],
    next: usize,
    cond_token: CondToken,
    /// The token read last when the error was found.
    failed: CondToken,
    messages: Vec<String>,
}

impl Conditional<'_> {
    /// Parses the command through its `]]`, returning the index after it, or bash's diagnostic.
    fn command(&mut self, cond_line: usize) -> Result<usize, Vec<String>> {
        self.expr();
        match self.cond_token {
            CondToken::At(i) if self.is_word(i, "]]") => return Ok(i + 1),
            CondToken::Error => {}
            CondToken::End => {
                self.messages.push(std::format!(
                    "line {cond_line}: unexpected EOF while looking for `]]'"
                ));
                self.failed = CondToken::End;
            }
            token @ CondToken::At(_) => {
                let text = self.text(token);
                self.messages.push(std::format!(
                    "line {cond_line}: syntax error in conditional expression: unexpected token `{text}'"
                ));
                self.failed = token;
            }
        }
        let mut diagnostic = std::mem::take(&mut self.messages);
        if let CondToken::At(i) = self.failed {
            diagnostic.extend(self.near(i));
        } else {
            diagnostic.push(std::format!(
                "line {}: syntax error: unexpected end of file from `[[' command on line {cond_line}",
                self.lines.len() + 1
            ));
        }
        Err(diagnostic)
    }

    fn expr(&mut self) {
        self.and();
        if matches!(self.cond_token, CondToken::At(i) if self.is_operator(i, "||")) {
            self.expr();
        }
    }

    fn and(&mut self) {
        self.term();
        if matches!(self.cond_token, CondToken::At(i) if self.is_operator(i, "&&")) {
            self.and();
        }
    }

    fn term(&mut self) {
        let token = self.skip_newlines();
        let CondToken::At(i) = token else {
            return self.fail(token, "unexpected token `EOF' in conditional command");
        };
        if self.is_word(i, "]]") {
            self.failed = token;
            self.cond_token = CondToken::Error;
        } else if self.is_operator(i, "(") {
            self.group(token);
        } else if self.is_word(i, "!") {
            self.term();
        } else if self.is_operand(i) && is_unary_operator(self.tokens[i].to_str()) {
            let argument = self.read();
            if matches!(argument, CondToken::At(j) if self.is_operand(j)) {
                self.cond_token = self.skip_newlines();
            } else {
                let text = self.text(argument);
                self.fail(
                    argument,
                    &std::format!("unexpected argument `{text}' to conditional unary operator"),
                );
            }
        } else if self.is_operand(i) {
            self.binary();
        } else {
            let text = self.text(token);
            self.fail(
                token,
                &std::format!("unexpected token `{text}' in conditional command"),
            );
        }
    }

    /// The rest of a `( ... )` group, after its `(`.
    fn group(&mut self, open: CondToken) {
        let line = self.line(open);
        self.expr();
        match self.cond_token {
            CondToken::At(j) if self.is_operator(j, ")") => {
                self.cond_token = self.skip_newlines();
            }
            CondToken::Error => self
                .messages
                .push(std::format!("line {line}: expected `)'")),
            other => {
                let text = self.text(other);
                self.messages.push(std::format!(
                    "line {line}: unexpected token `{text}', expected `)'"
                ));
                self.failed = other;
                self.cond_token = CondToken::Error;
            }
        }
    }

    /// The rest of a binary expression, or of a word tested alone, after its first word.
    fn binary(&mut self) {
        let operator = self.read();
        let CondToken::At(j) = operator else {
            return self.fail(
                operator,
                "unexpected token `EOF', conditional binary operator expected",
            );
        };
        let binary = self.is_operator(j, "<")
            || self.is_operator(j, ">")
            || (self.is_operand(j) && is_binary_operator(self.tokens[j].to_str()));
        if !binary {
            // `[[ word ]]` tests that the word is not empty.
            if self.is_word(j, "]]")
                || self.is_operator(j, "&&")
                || self.is_operator(j, "||")
                || self.is_operator(j, ")")
            {
                self.cond_token = operator;
            } else {
                let text = self.text(operator);
                self.fail(
                    operator,
                    &std::format!(
                        "unexpected token `{text}', conditional binary operator expected"
                    ),
                );
            }
            return;
        }
        let regex = self.is_word(j, "=~");
        let argument = self.read();
        match argument {
            CondToken::At(k) if self.is_operand(k) || (regex && self.is_operator(k, "(")) => {
                if regex {
                    self.skip_regex(k);
                }
                self.cond_token = self.skip_newlines();
            }
            _ => {
                let text = self.text(argument);
                self.fail(
                    argument,
                    &std::format!("unexpected argument `{text}' to conditional binary operator"),
                );
            }
        }
    }

    /// Reads the rest of a regular expression that starts with the token at `first`: the tokens
    /// that follow it with no blank between, and anything inside parentheses.
    fn skip_regex(&mut self, first: usize) {
        let depth_change = |token: &crate::Token| match token {
            crate::Token::Operator(o, _) if o == "(" => 1,
            crate::Token::Operator(o, _) if o == ")" => -1,
            _ => 0,
        };
        let mut depth = depth_change(&self.tokens[first]);
        let mut last = first;
        while let Some(token) = self.tokens.get(self.next) {
            let adjacent = token.location().start.index == self.tokens[last].location().end.index;
            if depth <= 0 && (!adjacent || token.to_str() == "\n") {
                break;
            }
            depth += depth_change(token);
            last = self.next;
            self.next += 1;
        }
    }

    const fn read(&mut self) -> CondToken {
        if self.next < self.tokens.len() {
            self.next += 1;
            CondToken::At(self.next - 1)
        } else {
            CondToken::End
        }
    }

    fn skip_newlines(&mut self) -> CondToken {
        loop {
            match self.read() {
                CondToken::At(i) if self.is_operator(i, "\n") => {}
                token => return token,
            }
        }
    }

    /// Reports `message` for an error found after reading `token`.
    fn fail(&mut self, token: CondToken, message: &str) {
        let line = self.line(token);
        self.messages.push(std::format!("line {line}: {message}"));
        self.failed = token;
        self.cond_token = CondToken::Error;
    }

    fn is_word(&self, i: usize, word: &str) -> bool {
        matches!(&self.tokens[i], crate::Token::Word(w, _) if w == word)
    }

    fn is_operator(&self, i: usize, operator: &str) -> bool {
        matches!(&self.tokens[i], crate::Token::Operator(o, _) if o == operator)
    }

    /// A word other than `]]`, which ends the command.
    fn is_operand(&self, i: usize) -> bool {
        matches!(&self.tokens[i], crate::Token::Word(w, _) if w != "]]")
    }

    /// A token as bash names it in a diagnostic.
    fn text(&self, token: CondToken) -> String {
        match token {
            CondToken::At(i) if self.is_operator(i, "\n") => "newline".to_owned(),
            CondToken::At(i) => self.tokens[i].to_str().to_owned(),
            CondToken::End | CondToken::Error => "EOF".to_owned(),
        }
    }

    /// The line bash is on once it has read `token`.
    fn line(&self, token: CondToken) -> usize {
        match token {
            CondToken::At(i) if self.is_operator(i, "\n") => self.tokens[i].location().start.line,
            CondToken::At(i) => self.tokens[i].location().end.line,
            CondToken::End | CondToken::Error => self.lines.len() + 1,
        }
    }

    /// Bash's `syntax error near` for an error found after reading the token at `i`: the text
    /// bash finds before that point, and the line it is on.
    fn near(&self, i: usize) -> [String; 2] {
        let line = self.line(CondToken::At(i));
        let text = self
            .lines
            .get(line.saturating_sub(1))
            .copied()
            .unwrap_or_default();
        let chars: Vec<char> = text.chars().collect();
        let read = if self.is_operator(i, "\n") {
            chars.len()
        } else {
            self.tokens[i].location().end.column.saturating_sub(1)
        };
        let word = error_token_from_text(&chars, read.min(chars.len()));
        [
            std::format!("line {line}: syntax error near `{word}'"),
            std::format!("line {line}: `{}'", text.trim_end_matches('\n')),
        ]
    }
}

/// Whether bash's `[[` takes `word` as a unary operator (`-f`).
fn is_unary_operator(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next() == Some('-')
        && chars
            .next()
            .is_some_and(|c| "abcdefghknoprstuvwxzGLOSNR".contains(c))
        && chars.next().is_none()
}

/// Whether bash's `[[` takes `word` as a binary operator (`==`, `-lt`).
fn is_binary_operator(word: &str) -> bool {
    matches!(
        word,
        "=" | "=="
            | "!="
            | "=~"
            | "-nt"
            | "-ot"
            | "-ef"
            | "-eq"
            | "-ne"
            | "-lt"
            | "-le"
            | "-gt"
            | "-ge"
    )
}

/// The token bash names from the text of the line it is reading, `read` characters in
/// (`error_token_from_text` in bash's `parse.y`): the last run of characters before there that
/// are not blanks or `;|&`, or else the one such character there.
fn error_token_from_text(line: &[char], read: usize) -> String {
    let at = |i: usize| line.get(i).copied().unwrap_or('\0');
    let blank = |c: char| matches!(c, ' ' | '\t' | '\n');
    let mut i = read;
    if i > 0 && at(i) == '\0' {
        i -= 1;
    }
    while i > 0 && blank(at(i)) {
        i -= 1;
    }
    let token_end = if i > 0 { i + 1 } else { 0 };
    while i > 0 && !matches!(at(i), ' ' | '\n' | '\t' | ';' | '|' | '&') {
        i -= 1;
    }
    while i != token_end && blank(at(i)) {
        i += 1;
    }
    if token_end > 0 {
        line.get(i..token_end).unwrap_or_default().iter().collect()
    } else {
        at(i).to_string()
    }
}

/// The line of a command's first word followed by `(` and then a newline or the end of the
/// input (`f (`): bash takes it for a function definition wanting its `)` there.
fn open_function_parenthesis(tokens: &[crate::Token]) -> Option<usize> {
    let mut command_start = true;
    for (i, token) in tokens.iter().enumerate() {
        let text = token.to_str();
        if command_start
            && matches!(token, crate::Token::Word(..))
            && !matches!(
                text,
                "if" | "then"
                    | "else"
                    | "elif"
                    | "fi"
                    | "do"
                    | "done"
                    | "case"
                    | "esac"
                    | "while"
                    | "until"
                    | "for"
                    | "select"
                    | "{"
                    | "}"
                    | "!"
                    | "[["
                    | "]]"
                    | "time"
                    | "function"
                    | "in"
                    | "coproc"
            )
            && tokens.get(i + 1).is_some_and(|t| t.to_str() == "(")
            && tokens.get(i + 2).is_none_or(|t| t.to_str() == "\n")
        {
            return Some(tokens[i + 1].location().start.line);
        }
        command_start = match token {
            crate::Token::Operator(..) => matches!(
                text,
                ";" | "\n" | "&" | "&&" | "||" | "|" | "|&" | "(" | ")" | ";;" | ";&" | ";;&"
            ),
            crate::Token::Word(..) => {
                command_start
                    && matches!(
                        text,
                        "if" | "then"
                            | "else"
                            | "elif"
                            | "while"
                            | "until"
                            | "do"
                            | "{"
                            | "!"
                            | "time"
                    )
            }
        };
    }
    None
}

/// Bash's diagnostic for a here-document body's command substitution that does not parse.
///
/// `text` is the body after the substitution's `$(`, and its first line is line `first_line`.
/// Bash reads it as it reads any `$( )`: to a `)` that closes it, or to the end.
pub fn command_substitution_diagnostic(
    text: &str,
    first_line: usize,
    options: &crate::ParserOptions,
) -> Vec<String> {
    let lines = match tokenizer::command_substitution_len(text, &options.tokenizer_options()) {
        Err(inner) => bash_diagnostic(
            &ParseError::Tokenizing {
                inner,
                position: None,
            },
            text,
            options,
        ),
        Ok(len) => {
            let parse = |command: &str| {
                let mut reader = std::io::BufReader::new(command.as_bytes());
                crate::Parser::new(&mut reader, options).parse_program()
            };
            // A command that does not parse is diagnosed with its `)`, which bash reads as a
            // token: `$(if)` fails at it.
            let command = text.get(..len).unwrap_or(text);
            let without_close = command
                .get(..command.len().saturating_sub(1))
                .unwrap_or(command);
            let error = match parse(without_close) {
                Ok(_) => None,
                Err(_) => parse(command).err(),
            };
            match error {
                None => vec![],
                Some(error) => {
                    let mut lines = bash_diagnostic(&error, text, options);
                    // Bash names what it was looking for when the token is not the `)`.
                    if let Some(first) = lines.first_mut() {
                        if first.contains("syntax error near unexpected token `")
                            && !first.ends_with("`)'")
                        {
                            first.push_str(" while looking for matching `)'");
                        }
                    }
                    lines
                }
            }
        }
    };
    // The lines are numbered from the substitution's first line.
    lines
        .into_iter()
        .map(|line| {
            let Some(rest) = line.strip_prefix("line ") else {
                return line;
            };
            let after = rest.trim_start_matches(|c: char| c.is_ascii_digit());
            let number = rest
                .strip_suffix(after)
                .and_then(|n| n.parse::<usize>().ok());
            match number {
                Some(number) => {
                    std::format!("line {}{after}", first_line + number.saturating_sub(1))
                }
                None => line,
            }
        })
        .collect()
}

/// The exit status bash gives a syntax error: 1 for one inside the parentheses of a compound array
/// assignment (`a=(x | y)`, `a=(x` at the end), whose words bash reads apart, otherwise 2.
pub fn syntax_error_status(error: &ParseError, source: &str, options: &crate::ParserOptions) -> u8 {
    let tokenize = |text: &str| {
        tokenizer::tokenize_str_with_options(text, &options.tokenizer_options()).unwrap_or_default()
    };
    let open = match error {
        ParseError::ParsingNear(position) => {
            let tokens = tokenize(source);
            tokens
                .iter()
                .position(|token| token.location().start.index >= position.index)
                .and_then(|at| open_compound_assignment(&tokens, at))
        }
        ParseError::ParsingAtEndOfInput => {
            let tokens = tokenize(source);
            open_compound_assignment(&tokens, tokens.len())
        }
        // A quote left open: what came before it.
        ParseError::Tokenizing { inner, .. } => {
            use tokenizer::TokenizerError as T;
            match inner {
                T::UnterminatedDoubleQuote(position)
                | T::UnterminatedSingleQuote(position)
                | T::UnterminatedAnsiCQuote(position)
                | T::UnterminatedBackquote(position) => {
                    let before: String = source.chars().take(position.index).collect();
                    let tokens = tokenize(&before);
                    open_compound_assignment(&tokens, tokens.len())
                }
                _ => None,
            }
        }
    };
    if open.is_some() { 1 } else { 2 }
}

/// The line of the `(` of a compound array assignment (`a=(`, `a+=(`) still open before token
/// `at`, if one is.
fn open_compound_assignment(tokens: &[crate::Token], at: usize) -> Option<usize> {
    let mut open = None;
    let mut previous: Option<&crate::Token> = None;
    for token in tokens.iter().take(at) {
        match token {
            crate::Token::Operator(operator, location) if operator == "(" && open.is_none() => {
                let assigns = previous.is_some_and(|word| {
                    matches!(word, crate::Token::Word(..))
                        && word.to_str().ends_with('=')
                        && tokenizer::is_assignment_word(word.to_str())
                        && word.location().end.index == location.start.index
                });
                if assigns {
                    open = Some(location.start.line);
                }
            }
            crate::Token::Operator(operator, _) if operator == ")" => open = None,
            _ => {}
        }
        previous = Some(token);
    }
    open
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
        && matches!(tokens[index].to_str(), ";" | "\n" | "&" | ")")
        && RESERVED.contains(&tokens[index - 1].to_str())
    {
        index -= 1;
    }
    // A `!` starts a pipeline, not a command within one (`a | ! b`); the parser fails at the
    // command after it, and bash names the `!`.
    if index > 1
        && tokens[index - 1].to_str() == "!"
        && matches!(tokens[index - 2].to_str(), "|" | "|&")
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
    fn names_the_token_on_a_last_line_without_a_newline() {
        assert_eq!(
            diagnose("echo first; fi"),
            [
                "line 1: syntax error near unexpected token `fi'",
                "line 1: `echo first; fi'"
            ]
        );
        assert_eq!(
            diagnose("echo ("),
            [
                "line 1: syntax error near unexpected token `newline'",
                "line 1: `echo ('"
            ]
        );
        assert_eq!(
            diagnose("if ("),
            ["line 2: syntax error: unexpected end of file from `(' command on line 1"]
        );
        assert_eq!(
            diagnose("! true | ! true"),
            [
                "line 1: syntax error near unexpected token `!'",
                "line 1: `! true | ! true'"
            ]
        );
    }

    #[test]
    fn words_conditional_command_errors_as_bash() {
        assert_eq!(
            diagnose("echo x; [[ ( a\n&& b ) ]]"),
            [
                "line 1: unexpected token `newline', conditional binary operator expected",
                "line 1: expected `)'",
                "line 1: syntax error near `a'",
                "line 1: `echo x; [[ ( a'",
            ]
        );
        assert_eq!(
            diagnose("[[ a && && b ]]"),
            [
                "line 1: unexpected token `&&' in conditional command",
                "line 1: syntax error near `&'",
                "line 1: `[[ a && && b ]]'",
            ]
        );
        assert_eq!(
            diagnose("if [[ -n x ]]; then [[ ( a ) b ]]; fi"),
            [
                "line 1: syntax error in conditional expression: unexpected token `b'",
                "line 1: syntax error near `b'",
                "line 1: `if [[ -n x ]]; then [[ ( a ) b ]]; fi'",
            ]
        );
        assert_eq!(
            diagnose("[[ a =~ (x|y)z && -f ]]"),
            [
                "line 1: unexpected argument `]]' to conditional unary operator",
                "line 1: syntax error near `]]'",
                "line 1: `[[ a =~ (x|y)z && -f ]]'",
            ]
        );
        assert_eq!(
            diagnose("[[ a == b"),
            [
                "line 1: unexpected EOF while looking for `]]'",
                "line 2: syntax error: unexpected end of file from `[[' command on line 1",
            ]
        );
        assert_eq!(
            diagnose("[["),
            [
                "line 2: unexpected token `EOF' in conditional command",
                "line 2: syntax error: unexpected end of file from `[[' command on line 1",
            ]
        );
    }

    #[test]
    fn compound_assignment_errors_exit_one() {
        let status = |source: &str| {
            let options = crate::ParserOptions::default();
            let mut reader = std::io::BufReader::new(source.as_bytes());
            crate::Parser::new(&mut reader, &options)
                .parse_program()
                .err()
                .map(|error| super::syntax_error_status(&error, source, &options))
        };
        for source in [
            "x=(a b",
            "if true; then\nx=(a\n",
            "x=(a | b)",
            "a[1]+=(x \"y",
            "x=(a (b) c)",
        ] {
            assert_eq!(status(source), Some(1), "{source:?}");
        }
        for source in [
            "x=(a b)); echo c",
            "echo a; fi",
            "echo \"a",
            "if true; then",
        ] {
            assert_eq!(status(source), Some(2), "{source:?}");
        }
        assert_eq!(
            diagnose("if true; then\nx=(a\nb"),
            ["line 2: unexpected EOF while looking for matching `)'"]
        );
    }

    #[test]
    fn words_command_substitution_errors_from_their_first_line() {
        let diagnose = |text: &str| {
            super::command_substitution_diagnostic(text, 2, &crate::ParserOptions::default())
        };
        assert_eq!(
            diagnose("echo hi\n"),
            ["line 3: unexpected EOF while looking for matching `)'"]
        );
        assert_eq!(
            diagnose("if) more\n"),
            [
                "line 2: syntax error near unexpected token `)'",
                "line 2: `if) more'"
            ]
        );
        assert_eq!(
            diagnose("echo a; fi) x\n"),
            [
                "line 2: syntax error near unexpected token `fi' while looking for matching `)'",
                "line 2: `echo a; fi) x'"
            ]
        );
        assert_eq!(
            diagnose("echo 'a\n"),
            ["line 2: unexpected EOF while looking for matching `''"]
        );
        assert_eq!(
            diagnose("case x in x) echo y\nz\n"),
            ["line 4: unexpected EOF while looking for matching `)'"]
        );
        assert!(diagnose("echo fine) rest").is_empty());
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
