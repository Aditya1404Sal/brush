use std::borrow::Cow;
use std::sync::Arc;
use utf8_chars::BufReadCharsExt;

use crate::{SourcePosition, SourceSpan};

#[derive(Clone, Debug)]
pub(crate) enum TokenEndReason {
    /// End of input was reached.
    EndOfInput,
    /// An unescaped newline char was reached.
    UnescapedNewLine,
    /// Specified terminating char.
    SpecifiedTerminatingChar,
    /// A non-newline blank char was reached.
    NonNewLineBlank,
    /// A here-document's body is starting.
    HereDocumentBodyStart,
    /// A here-document's body was terminated.
    HereDocumentBodyEnd,
    /// A here-document's end tag was reached.
    HereDocumentEndTag,
    /// An operator was started.
    OperatorStart,
    /// An operator was terminated.
    OperatorEnd,
    /// Some other condition was reached.
    Other,
}

/// Compatibility alias for `SourceSpan`.
pub type TokenLocation = SourceSpan;

/// Represents a token extracted from a shell script.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(
    any(test, feature = "serde"),
    derive(PartialEq, Eq, serde::Serialize, serde::Deserialize)
)]
pub enum Token {
    /// An operator token.
    Operator(String, SourceSpan),
    /// A word token.
    Word(String, SourceSpan),
}

impl Token {
    /// Returns the string value of the token.
    pub fn to_str(&self) -> &str {
        match self {
            Self::Operator(s, _) => s,
            Self::Word(s, _) => s,
        }
    }

    /// Returns the location of the token in the source script.
    pub const fn location(&self) -> &SourceSpan {
        match self {
            Self::Operator(_, l) => l,
            Self::Word(_, l) => l,
        }
    }
}

#[cfg(feature = "diagnostics")]
impl From<&Token> for miette::SourceSpan {
    fn from(token: &Token) -> Self {
        let start = token.location().start.as_ref();
        Self::new(start.into(), token.location().length())
    }
}

/// Encapsulates the result of tokenizing a shell script.
#[derive(Clone, Debug)]
pub(crate) struct TokenizeResult {
    /// Reason for tokenization ending.
    pub reason: TokenEndReason,
    /// The token that was extracted, if any.
    pub token: Option<Token>,
}

/// Represents an error that occurred during tokenization.
#[derive(thiserror::Error, Debug)]
pub enum TokenizerError {
    /// An unterminated escape sequence was encountered at the end of the input stream.
    #[error("unterminated escape sequence")]
    UnterminatedEscapeSequence,

    /// An unterminated single-quoted substring was encountered at the end of the input stream.
    #[error("unterminated single quote at {0}")]
    UnterminatedSingleQuote(SourcePosition),

    /// An unterminated ANSI C-quoted substring was encountered at the end of the input stream.
    #[error("unterminated ANSI C quote at {0}")]
    UnterminatedAnsiCQuote(SourcePosition),

    /// An unterminated double-quoted substring was encountered at the end of the input stream.
    #[error("unterminated double quote at {0}")]
    UnterminatedDoubleQuote(SourcePosition),

    /// An unterminated back-quoted substring was encountered at the end of the input stream.
    #[error("unterminated backquote near {0}")]
    UnterminatedBackquote(SourcePosition),

    /// An unterminated extended glob (extglob) pattern was encountered at the end of the input
    /// stream.
    #[error("unterminated extglob near {0}")]
    UnterminatedExtendedGlob(SourcePosition),

    /// An unterminated variable expression was encountered at the end of the input stream; it
    /// began at the given position.
    #[error("unterminated variable expression")]
    UnterminatedVariable(SourcePosition),

    /// An unterminated command substitiion was encountered at the end of the input stream.
    #[error("unterminated command substitution")]
    UnterminatedCommandSubstitution,

    /// An unterminated arithmetic or other expansion was encountered at the end of the input
    /// stream; it wanted the given closing character.
    #[error("unterminated expansion")]
    UnterminatedExpansion(char),

    /// An unterminated arithmetic expansion (`$((` or `$[`) was encountered at the end of the
    /// input stream; it wanted the given closing character, and began at the given position.
    #[error("unterminated arithmetic expansion")]
    UnterminatedArithmetic(char, SourcePosition),

    /// An error occurred decoding UTF-8 characters in the input stream.
    #[error("failed to decode UTF-8 characters")]
    FailedDecoding,

    /// An I/O here tag was missing.
    #[error("missing here tag for here document body")]
    MissingHereTagForDocumentBody,

    /// The indicated I/O here tag was missing.
    #[error("missing here tag '{0}'")]
    MissingHereTag(String),

    /// An unterminated here document sequence was encountered at the end of the input stream.
    #[error(
        "unterminated here document sequence; tag(s) [{}] found at: [{}]",
        .0.iter().map(|d| d.tag.as_str()).collect::<Vec<_>>().join(", "),
        .0.iter().map(|d| d.position.to_string()).collect::<Vec<_>>().join(", ")
    )]
    UnterminatedHereDocuments(Vec<UnterminatedHereDocument>),

    /// An I/O error occurred while reading from the input stream.
    #[error("failed to read input")]
    ReadError(#[from] std::io::Error),
}

/// A here-document that the input ended in before its delimiter.
#[derive(Clone, Debug)]
pub struct UnterminatedHereDocument {
    /// Its tag, as written (`'EOF'`).
    pub tag: String,
    /// The delimiter it wants: its tag without quoting.
    pub delimiter: String,
    /// Where its tag is.
    pub position: SourcePosition,
    /// The line read last before its body began, which bash names in its warning: the line
    /// that ends its command for the first here-document there, otherwise the line the previous
    /// here-document ended on, or the last line when the input ended first.
    pub line: usize,
}

impl TokenizerError {
    /// Returns true if the error represents an error that could possibly be due
    /// to an incomplete input stream.
    pub const fn is_incomplete(&self) -> bool {
        matches!(
            self,
            Self::UnterminatedEscapeSequence
                | Self::UnterminatedAnsiCQuote(..)
                | Self::UnterminatedSingleQuote(..)
                | Self::UnterminatedDoubleQuote(..)
                | Self::UnterminatedBackquote(..)
                | Self::UnterminatedCommandSubstitution
                | Self::UnterminatedExpansion(_)
                | Self::UnterminatedArithmetic(..)
                | Self::UnterminatedVariable(..)
                | Self::UnterminatedExtendedGlob(..)
                | Self::UnterminatedHereDocuments(..)
        )
    }
}

/// Encapsulates a sequence of tokens.
#[derive(Debug)]
pub(crate) struct Tokens<'a> {
    /// Sequence of tokens.
    pub tokens: &'a [Token],
    /// The text the tokens were read from, when known.
    pub source: Option<&'a str>,
}

#[derive(Clone, Debug)]
enum QuoteMode {
    None,
    AnsiC(SourcePosition),
    Single(SourcePosition),
    Double(SourcePosition),
}

#[derive(Clone, Debug, Default)]
enum HereState {
    /// In this state, we are not currently tracking any here-documents.
    #[default]
    None,
    /// In this state, we expect that the next token will be a here tag.
    NextTokenIsHereTag { remove_tabs: bool },
    /// In this state, the *current* token is a here tag.
    CurrentTokenIsHereTag {
        remove_tabs: bool,
        operator_token_result: TokenizeResult,
    },
    /// In this state, we expect that the *next line* will be the body of
    /// a here-document.
    NextLineIsHereDoc,
    /// In this state, we are in the set of lines that comprise 1 or more
    /// consecutive here-document bodies.
    InHereDocs,
}

#[derive(Clone, Debug)]
struct HereTag {
    tag: String,
    tag_was_escaped_or_quoted: bool,
    remove_tabs: bool,
    position: SourcePosition,
    tokens: Vec<TokenizeResult>,
    pending_tokens_after: Vec<TokenizeResult>,
}

#[derive(Clone, Debug)]
struct CrossTokenParseState {
    /// Cursor within the overall token stream; used for error reporting.
    cursor: SourcePosition,
    /// Current state of parsing here-documents.
    here_state: HereState,
    /// Ordered queue of here tags for which we're still looking for matching here-document bodies.
    current_here_tags: Vec<HereTag>,
    /// Tokens already tokenized that should be used first to serve requests for tokens.
    queued_tokens: Vec<TokenizeResult>,
    /// Are we in an arithmetic expansion?
    arithmetic_expansion: bool,
    /// Is the next word in a command's first words, where it can be an assignment?
    command_position: bool,
    /// How many nested constructs (`$(...)`, `${...}` and the like) are being tokenized.
    nested_constructs: u32,
    /// The line read last before the body of the here-document being read began.
    here_body_after_line: usize,
    /// Are we in the parentheses of a compound array assignment (`a=(...)`)?
    compound_assignment: bool,
    /// Does a `-` follow a `>&` or `<&` just read? Bash reads it as a word of its own (closing
    /// the descriptor), so `>&-1` is `>&-` followed by the word `1`.
    dash_follows_duplication: bool,
}

/// Options controlling how the tokenizer operates.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct TokenizerOptions {
    /// Whether or not to enable extended globbing patterns (extglob).
    pub enable_extended_globbing: bool,
    /// Whether or not to operate in POSIX compliance mode.
    pub posix_mode: bool,
    /// Whether or not we're running in SH emulation mode.
    pub sh_mode: bool,
}

impl Default for TokenizerOptions {
    fn default() -> Self {
        Self {
            enable_extended_globbing: true,
            posix_mode: false,
            sh_mode: false,
        }
    }
}

/// A tokenizer for shell scripts.
pub(crate) struct Tokenizer<'a, R: ?Sized + std::io::BufRead> {
    char_reader: std::iter::Peekable<utf8_chars::Chars<'a, R>>,
    /// A character read and put back (see `peek_second_char`), to be read again first.
    put_back: Option<char>,
    /// The text read so far.
    text: String,
    cross_state: CrossTokenParseState,
    options: TokenizerOptions,
}

/// Encapsulates the current token parsing state.
#[derive(Clone, Debug)]
struct TokenParseState {
    pub start_position: SourcePosition,
    pub token_so_far: String,
    pub token_is_operator: bool,
    pub in_escape: bool,
    pub quote_mode: QuoteMode,
}

impl TokenParseState {
    pub fn new(start_position: &SourcePosition) -> Self {
        Self {
            start_position: start_position.to_owned(),
            token_so_far: String::new(),
            token_is_operator: false,
            in_escape: false,
            quote_mode: QuoteMode::None,
        }
    }

    pub fn pop(&mut self, end_position: &SourcePosition) -> Token {
        let end = Arc::new(end_position.to_owned());
        let token_location = SourceSpan {
            start: Arc::new(std::mem::take(&mut self.start_position)),
            end,
        };

        let token = if std::mem::take(&mut self.token_is_operator) {
            Token::Operator(std::mem::take(&mut self.token_so_far), token_location)
        } else {
            Token::Word(std::mem::take(&mut self.token_so_far), token_location)
        };

        end_position.clone_into(&mut self.start_position);
        self.in_escape = false;
        self.quote_mode = QuoteMode::None;

        token
    }

    pub const fn started_token(&self) -> bool {
        !self.token_so_far.is_empty()
    }

    /// Returns true if the token so far consists only of blanks.
    ///
    /// This can only happen when tokenizing with `include_space`, where blanks are accumulated
    /// into the token so that the original text of a nested construct can be reproduced. Such
    /// blanks are not a word, so a `#` following them still begins a comment.
    pub fn only_blanks_so_far(&self) -> bool {
        !self.token_so_far.is_empty() && self.token_so_far.chars().all(is_blank)
    }

    pub fn append_char(&mut self, c: char) {
        self.token_so_far.push(c);
    }

    pub fn append_str(&mut self, s: &str) {
        self.token_so_far.push_str(s);
    }

    pub const fn unquoted(&self) -> bool {
        !self.in_escape && matches!(self.quote_mode, QuoteMode::None)
    }

    pub fn current_token(&self) -> &str {
        &self.token_so_far
    }

    pub fn is_specific_operator(&self, operator: &str) -> bool {
        self.token_is_operator && self.current_token() == operator
    }

    pub const fn in_operator(&self) -> bool {
        self.token_is_operator
    }

    fn is_newline(&self) -> bool {
        self.token_so_far == "\n"
    }

    fn replace_with_here_doc(&mut self, s: String) {
        self.token_so_far = s;
    }

    #[allow(clippy::too_many_lines)]
    pub fn delimit_current_token(
        &mut self,
        reason: TokenEndReason,
        cross_token_state: &mut CrossTokenParseState,
    ) -> Result<Option<TokenizeResult>, TokenizerError> {
        // If we don't have anything in the token, then don't yield an empty string token
        // *unless* it's the body of a here document.
        if !self.started_token() && !matches!(reason, TokenEndReason::HereDocumentBodyEnd) {
            return Ok(Some(TokenizeResult {
                reason,
                token: None,
            }));
        }

        // TODO(tokenizer): Make sure the here-tag meets criteria (and isn't a newline).
        let current_here_state = std::mem::take(&mut cross_token_state.here_state);
        if !matches!(current_here_state, HereState::InHereDocs) {
            cross_token_state.command_position = starts_command_words(
                self.current_token(),
                self.token_is_operator,
                cross_token_state.command_position,
            );
        }
        match current_here_state {
            HereState::NextTokenIsHereTag { remove_tabs } => {
                // Don't yield the operator as a token yet. We need to make sure we collect
                // up everything we need for all the here-documents with tags on this line.
                let operator_token_result = TokenizeResult {
                    reason,
                    token: Some(self.pop(&cross_token_state.cursor)),
                };

                cross_token_state.here_state = HereState::CurrentTokenIsHereTag {
                    remove_tabs,
                    operator_token_result,
                };

                return Ok(None);
            }
            HereState::CurrentTokenIsHereTag {
                remove_tabs,
                operator_token_result,
            } => {
                if self.is_newline() {
                    return Err(TokenizerError::MissingHereTag(
                        self.current_token().to_owned(),
                    ));
                }

                cross_token_state.here_state = HereState::NextLineIsHereDoc;

                // Include the trailing \n in the here tag so it's easier to check against.
                let tag = std::format!("{}\n", self.current_token().trim_ascii_start());
                let tag_was_escaped_or_quoted = tag.contains(is_quoting_char);

                let tag_token_result = TokenizeResult {
                    reason,
                    token: Some(self.pop(&cross_token_state.cursor)),
                };

                cross_token_state.current_here_tags.push(HereTag {
                    tag,
                    tag_was_escaped_or_quoted,
                    remove_tabs,
                    position: cross_token_state.cursor.clone(),
                    tokens: vec![operator_token_result, tag_token_result],
                    pending_tokens_after: vec![],
                });

                return Ok(None);
            }
            HereState::NextLineIsHereDoc => {
                if self.is_newline() {
                    cross_token_state.here_state = HereState::InHereDocs;
                    cross_token_state.here_body_after_line =
                        last_line_read(&cross_token_state.cursor);
                } else {
                    cross_token_state.here_state = HereState::NextLineIsHereDoc;
                }

                if let Some(last_here_tag) = cross_token_state.current_here_tags.last_mut() {
                    let token = self.pop(&cross_token_state.cursor);
                    let result = TokenizeResult {
                        reason,
                        token: Some(token),
                    };

                    last_here_tag.pending_tokens_after.push(result);
                } else {
                    return Err(TokenizerError::MissingHereTagForDocumentBody);
                }

                return Ok(None);
            }
            HereState::InHereDocs => {
                // We hit the end of the current here-document.
                let completed_here_tag = cross_token_state.current_here_tags.remove(0);

                // First queue the redirection operator and (start) here-tag.
                cross_token_state
                    .queued_tokens
                    .extend(completed_here_tag.tokens);

                // Leave a hint that we are about to start a here-document.
                cross_token_state.queued_tokens.push(TokenizeResult {
                    reason: TokenEndReason::HereDocumentBodyStart,
                    token: None,
                });

                // Then queue the body document we just finished.
                cross_token_state.queued_tokens.push(TokenizeResult {
                    reason,
                    token: Some(self.pop(&cross_token_state.cursor)),
                });

                // Then queue up the (end) here-tag.
                let end_tag = if completed_here_tag.tag_was_escaped_or_quoted {
                    unquote_str(&completed_here_tag.tag)
                } else {
                    completed_here_tag.tag
                };
                self.append_str(end_tag.trim_end_matches('\n'));
                cross_token_state.queued_tokens.push(TokenizeResult {
                    reason: TokenEndReason::HereDocumentEndTag,
                    token: Some(self.pop(&cross_token_state.cursor)),
                });

                // Now we're ready to queue up any tokens that came between the completed
                // here tag and the next here tag (or newline after it if it was the last).
                cross_token_state
                    .queued_tokens
                    .extend(completed_here_tag.pending_tokens_after);

                if cross_token_state.current_here_tags.is_empty() {
                    cross_token_state.here_state = HereState::None;
                } else {
                    cross_token_state.here_state = HereState::InHereDocs;
                    cross_token_state.here_body_after_line =
                        last_line_read(&cross_token_state.cursor);
                }

                return Ok(None);
            }
            HereState::None => (),
        }

        let token = self.pop(&cross_token_state.cursor);
        let result = TokenizeResult {
            reason,
            token: Some(token),
        };

        Ok(Some(result))
    }
}

/// Break the given input shell script string into tokens, returning the tokens.
///
/// # Arguments
///
/// * `input` - The shell script to tokenize.
pub fn tokenize_str(input: &str) -> Result<Vec<Token>, TokenizerError> {
    tokenize_str_with_options(input, &TokenizerOptions::default())
}

/// Break the given input shell script string into tokens, returning the tokens.
///
/// # Arguments
///
/// * `input` - The shell script to tokenize.
/// * `options` - Options controlling how the tokenizer operates.
pub fn tokenize_str_with_options(
    input: &str,
    options: &TokenizerOptions,
) -> Result<Vec<Token>, TokenizerError> {
    uncached_tokenize_string(input, options)
}

#[cached::macros::cached(
    name = "TOKENIZE_CACHE",
    max_size = 64,
    key = "(String, TokenizerOptions)",
    convert = r#"{ (input.to_owned(), options.to_owned()) }"#
)]
fn uncached_tokenize_string(
    input: &str,
    options: &TokenizerOptions,
) -> Result<Vec<Token>, TokenizerError> {
    uncached_tokenize_str(input, options)
}

/// How many bytes of `text` a command substitution takes whose `$(` came just before it, through
/// its closing `)`, as the tokenizer reads one: bash parses a here-document body's command
/// substitutions only when it expands the body, but finds where each ends the same way.
pub(crate) fn command_substitution_len(
    text: &str,
    options: &TokenizerOptions,
) -> Result<usize, TokenizerError> {
    let mut reader = std::io::BufReader::new(text.as_bytes());
    let mut tokenizer = Tokenizer::new(&mut reader, options);
    let mut state = TokenParseState::new(&tokenizer.cross_state.cursor);
    let pending = tokenizer.set_aside_pending_here_docs();
    tokenizer.consume_nested_construct(&mut state, ')', "(", 1)?;
    tokenizer.restore_pending_here_docs(pending);
    let chars = tokenizer.cross_state.cursor.index;
    Ok(text
        .char_indices()
        .nth(chars)
        .map_or(text.len(), |(index, _)| index))
}

/// Break the given input shell script string into tokens, returning the tokens.
/// No caching is performed.
///
/// # Arguments
///
/// * `input` - The shell script to tokenize.
pub fn uncached_tokenize_str(
    input: &str,
    options: &TokenizerOptions,
) -> Result<Vec<Token>, TokenizerError> {
    let mut reader = std::io::BufReader::new(input.as_bytes());
    let mut tokenizer = crate::tokenizer::Tokenizer::new(&mut reader, options);

    let mut tokens = vec![];
    loop {
        match tokenizer.next_token()? {
            TokenizeResult {
                token: Some(token), ..
            } => tokens.push(token),
            TokenizeResult {
                reason: TokenEndReason::EndOfInput,
                ..
            } => break,
            _ => (),
        }
    }

    Ok(tokens)
}

impl<'a, R: ?Sized + std::io::BufRead> Tokenizer<'a, R> {
    pub fn new(reader: &'a mut R, options: &TokenizerOptions) -> Self {
        Tokenizer {
            options: options.clone(),
            char_reader: reader.chars().peekable(),
            put_back: None,
            text: String::new(),
            cross_state: CrossTokenParseState {
                cursor: SourcePosition {
                    index: 0,
                    line: 1,
                    column: 1,
                },
                here_state: HereState::None,
                current_here_tags: vec![],
                queued_tokens: vec![],
                dash_follows_duplication: false,
                arithmetic_expansion: false,
                command_position: true,
                nested_constructs: 0,
                here_body_after_line: 0,
                compound_assignment: false,
            },
        }
    }

    #[expect(clippy::unnecessary_wraps)]
    pub fn current_location(&self) -> Option<SourcePosition> {
        Some(self.cross_state.cursor.clone())
    }

    fn next_char(&mut self) -> Result<Option<char>, TokenizerError> {
        let c = if let Some(c) = self.put_back.take() {
            Some(c)
        } else {
            let c = self
                .char_reader
                .next()
                .transpose()
                .map_err(TokenizerError::ReadError)?;
            self.text.extend(c);
            c
        };

        if let Some(ch) = c {
            if ch == '\n' {
                self.cross_state.cursor.line += 1;
                self.cross_state.cursor.column = 1;
            } else {
                self.cross_state.cursor.column += 1;
            }
            self.cross_state.cursor.index += 1;
        }

        Ok(c)
    }

    fn consume_char(&mut self) -> Result<(), TokenizerError> {
        let _ = self.next_char()?;
        Ok(())
    }

    fn peek_char(&mut self) -> Result<Option<char>, TokenizerError> {
        if let Some(c) = self.put_back {
            return Ok(Some(c));
        }
        match self.char_reader.peek() {
            Some(result) => match result {
                Ok(c) => Ok(Some(*c)),
                Err(_) => Err(TokenizerError::FailedDecoding),
            },
            None => Ok(None),
        }
    }

    /// Returns the character after the next one (not a newline), consuming neither.
    fn peek_second_char(&mut self) -> Result<Option<char>, TokenizerError> {
        let Some(first) = self.next_char()? else {
            return Ok(None);
        };
        let second = self.peek_char();
        self.put_back = Some(first);
        self.cross_state.cursor.column -= 1;
        self.cross_state.cursor.index -= 1;
        second
    }

    /// Takes the text read so far.
    pub fn take_text(&mut self) -> String {
        std::mem::take(&mut self.text)
    }

    pub fn next_token(&mut self) -> Result<TokenizeResult, TokenizerError> {
        self.next_token_until(None, false /* include space? */)
    }

    /// Sets aside the here-documents pending on the current line while a nested construct
    /// (`$(...)`, `$((...))`, `$[...]` or `${...}`) is tokenized.
    ///
    /// The construct's tokens belong to it, not to the tokens queued up after a pending here tag,
    /// and the pending bodies start on the line after the one the construct ends on, as in bash.
    /// A here-document opened inside the construct is tokenized there.
    fn set_aside_pending_here_docs(&mut self) -> (HereState, Vec<HereTag>, bool, bool) {
        self.cross_state.nested_constructs += 1;
        (
            std::mem::take(&mut self.cross_state.here_state),
            std::mem::take(&mut self.cross_state.current_here_tags),
            std::mem::replace(&mut self.cross_state.command_position, false),
            std::mem::replace(&mut self.cross_state.compound_assignment, false),
        )
    }

    /// Restores here-documents set aside by `set_aside_pending_here_docs`, keeping any the
    /// nested construct left pending after them.
    fn restore_pending_here_docs(
        &mut self,
        (here_state, mut here_tags, command_position, compound_assignment): (
            HereState,
            Vec<HereTag>,
            bool,
            bool,
        ),
    ) {
        self.cross_state.nested_constructs -= 1;
        self.cross_state.command_position = command_position;
        self.cross_state.compound_assignment = compound_assignment;
        if here_tags.is_empty() && matches!(here_state, HereState::None) {
            return;
        }

        here_tags.append(&mut self.cross_state.current_here_tags);
        self.cross_state.current_here_tags = here_tags;
        self.cross_state.here_state = here_state;
    }

    /// Consumes a nested construct (e.g., `$((...))` or `$[...]`), handling nested delimiters
    /// and here-documents.
    ///
    /// # Arguments
    ///
    /// * `state` - The current token parse state to append characters to.
    /// * `terminating_char` - The character that terminates the construct (e.g., `)` or `]`).
    /// * `nesting_open` - The character that increases nesting depth when encountered (e.g., `(` or
    ///   `[`).
    /// * `initial_nesting` - The initial nesting count (e.g., 2 for `$((`, 1 for `$[`).
    fn consume_nested_construct(
        &mut self,
        state: &mut TokenParseState,
        terminating_char: char,
        nesting_open: &str,
        mut nesting_count: u32,
    ) -> Result<(), TokenizerError> {
        let mut pending_here_doc_tokens = vec![];
        let mut drain_here_doc_tokens = false;
        // In a command substitution, the `)` that ends a case pattern does not close it.
        let mut cases =
            (nesting_open == "(" && !self.cross_state.arithmetic_expansion).then(CaseTracker::new);

        loop {
            let cur_token = if drain_here_doc_tokens && !pending_here_doc_tokens.is_empty() {
                if pending_here_doc_tokens.len() == 1 {
                    drain_here_doc_tokens = false;
                }
                pending_here_doc_tokens.remove(0)
            } else {
                let cur_token = self.next_token_until(Some(terminating_char), true)?;

                if matches!(
                    cur_token.reason,
                    TokenEndReason::HereDocumentBodyStart
                        | TokenEndReason::HereDocumentBodyEnd
                        | TokenEndReason::HereDocumentEndTag
                ) {
                    pending_here_doc_tokens.push(cur_token);
                    continue;
                }
                cur_token
            };

            if matches!(cur_token.reason, TokenEndReason::UnescapedNewLine)
                && !pending_here_doc_tokens.is_empty()
            {
                pending_here_doc_tokens.push(cur_token);
                drain_here_doc_tokens = true;
                continue;
            }

            if let Some(cur_token_value) = cur_token.token {
                state.append_str(cur_token_value.to_str());
                if let Some(cases) = &mut cases {
                    cases.note(&cur_token_value);
                }

                match &cur_token_value {
                    Token::Operator(o, _) if o == nesting_open => nesting_count += 1,
                    // `[` is not an operator, so a subscript's opening bracket is inside a word
                    // (`$[a[0] < 9]`); each one left open needs its own closing bracket. A word
                    // can also hold both (`+(a[1])`, read whole as a pattern).
                    Token::Word(w, _) if nesting_open == "[" || nesting_open == "{" => {
                        let (open, close) = if nesting_open == "[" {
                            ('[', ']')
                        } else {
                            ('{', '}')
                        };
                        let open = w.matches(open).count();
                        let closed = w.matches(close).count();
                        nesting_count +=
                            u32::try_from(open.saturating_sub(closed)).unwrap_or(u32::MAX);
                    }
                    _ => (),
                }
            }

            match cur_token.reason {
                TokenEndReason::HereDocumentBodyStart => {
                    state.append_char('\n');
                }
                TokenEndReason::NonNewLineBlank => state.append_char(' '),
                TokenEndReason::SpecifiedTerminatingChar => {
                    if cases.as_mut().is_some_and(CaseTracker::closes_pattern) {
                        state.append_char(self.next_char()?.unwrap());
                        continue;
                    }
                    nesting_count -= 1;
                    if nesting_count == 0 {
                        break;
                    }
                    state.append_char(self.next_char()?.unwrap());
                }
                TokenEndReason::EndOfInput => {
                    return Err(TokenizerError::UnterminatedExpansion(terminating_char));
                }
                _ => (),
            }
        }

        state.append_char(self.next_char()?.unwrap());
        Ok(())
    }

    /// Returns the next token from the input stream, optionally stopping early when a specified
    /// terminating character is encountered.
    ///
    /// # Arguments
    ///
    /// * `terminating_char` - An optional character that, if encountered, will stop the
    ///   tokenization process and return the token up to that character.
    /// * `include_space` - If true, include spaces in the tokenization process. This is not
    ///   typically the case, but can be helpful when needing to preserve the original source text
    ///   embedded within a command substitution or similar construct.
    #[expect(clippy::cognitive_complexity)]
    #[expect(clippy::if_same_then_else)]
    #[expect(clippy::panic_in_result_fn)]
    #[expect(clippy::too_many_lines)]
    #[allow(clippy::unwrap_in_result)]
    fn next_token_until(
        &mut self,
        terminating_char: Option<char>,
        include_space: bool,
    ) -> Result<TokenizeResult, TokenizerError> {
        let mut state = TokenParseState::new(&self.cross_state.cursor);
        let mut result: Option<TokenizeResult> = None;

        while result.is_none() {
            // First satisfy token results from our queue. Once we exhaust the queue then
            // we'll look at the input stream.
            if !self.cross_state.queued_tokens.is_empty() {
                return Ok(self.cross_state.queued_tokens.remove(0));
            }

            let next = self.peek_char()?;
            let c = next.unwrap_or('\0');

            if std::mem::take(&mut self.cross_state.dash_follows_duplication) && c == '-' {
                self.consume_char()?;
                state.append_char(c);
                result =
                    state.delimit_current_token(TokenEndReason::Other, &mut self.cross_state)?;
                continue;
            }

            // When we hit the end of the input, then we're done with the current token (if there is
            // one).
            if next.is_none() {
                // TODO(tokenizer): Verify we're not waiting on some terminating character?
                // Verify we're out of all quotes.
                if state.in_escape {
                    if matches!(state.quote_mode, QuoteMode::None) {
                        // A backslash that ends the input is an ordinary character, as in
                        // bash (`echo \` prints it); it is already in the token.
                        state.in_escape = false;
                    } else {
                        return Err(TokenizerError::UnterminatedEscapeSequence);
                    }
                }
                match state.quote_mode {
                    QuoteMode::None => (),
                    QuoteMode::AnsiC(pos) => {
                        return Err(TokenizerError::UnterminatedAnsiCQuote(pos));
                    }
                    QuoteMode::Single(pos) => {
                        return Err(TokenizerError::UnterminatedSingleQuote(pos));
                    }
                    QuoteMode::Double(pos) => {
                        return Err(TokenizerError::UnterminatedDoubleQuote(pos));
                    }
                }

                // Verify we're not in a here document.
                if !matches!(self.cross_state.here_state, HereState::None) {
                    if self.remove_here_end_tag(&mut state, &mut result, false)? {
                        // If we hit end tag without a trailing newline, try to get next token.
                        continue;
                    }

                    // The input can end on a here-document's tag (`cat <<EOF`).
                    if matches!(
                        self.cross_state.here_state,
                        HereState::CurrentTokenIsHereTag { .. }
                    ) && state.started_token()
                    {
                        state.delimit_current_token(
                            TokenEndReason::EndOfInput,
                            &mut self.cross_state,
                        )?;
                    }

                    // The body being read began after `here_body_after_line`; any other starts
                    // at the end of the input.
                    let last_line = last_line_read(&self.cross_state.cursor);
                    let reading_body = matches!(self.cross_state.here_state, HereState::InHereDocs);
                    let documents = self
                        .cross_state
                        .current_here_tags
                        .iter()
                        .enumerate()
                        .map(|(i, tag)| {
                            let tag_text = tag.tag.trim();
                            UnterminatedHereDocument {
                                tag: tag_text.to_owned(),
                                delimiter: if tag.tag_was_escaped_or_quoted {
                                    unquote_str(tag_text)
                                } else {
                                    tag_text.to_owned()
                                },
                                position: tag.position.clone(),
                                line: if i == 0 && reading_body {
                                    self.cross_state.here_body_after_line
                                } else {
                                    last_line
                                },
                            }
                        })
                        .collect();
                    return Err(TokenizerError::UnterminatedHereDocuments(documents));
                }

                result = state
                    .delimit_current_token(TokenEndReason::EndOfInput, &mut self.cross_state)?;
            //
            // Handle being in a here document.
            //
            } else if matches!(self.cross_state.here_state, HereState::InHereDocs) {
                //
                // For now, just include the character in the current token. We also check
                // if there are leading tabs to be removed.
                //
                if !self.cross_state.current_here_tags.is_empty()
                    && self.cross_state.current_here_tags[0].remove_tabs
                    && (!state.started_token() || state.current_token().ends_with('\n'))
                    && c == '\t'
                {
                    // Consume it but don't include it.
                    self.consume_char()?;
                } else if c == '\\'
                    && self
                        .cross_state
                        .current_here_tags
                        .first()
                        .is_some_and(|tag| !tag.tag_was_escaped_or_quoted)
                    && state
                        .current_token()
                        .chars()
                        .rev()
                        .take_while(|c| *c == '\\')
                        .count()
                        % 2
                        == 0
                {
                    // In an unquoted here-document, an unescaped backslash-newline joins the
                    // lines, as bash reads it (a delimiter joined to the line before it no
                    // longer ends the document).
                    self.consume_char()?;
                    if matches!(self.peek_char()?, Some('\n')) {
                        self.consume_char()?;
                    } else {
                        state.append_char(c);
                    }
                } else {
                    self.consume_char()?;
                    state.append_char(c);

                    // See if this was a newline character following the terminating here tag.
                    if c == '\n' {
                        self.remove_here_end_tag(&mut state, &mut result, true)?;
                    }
                }
            //
            // Look for the specially specified terminating char. An operator being read ends
            // first, for its own reason: a newline's is what starts a pending here-document.
            //
            } else if state.unquoted() && terminating_char == Some(c) && !state.in_operator() {
                result = state.delimit_current_token(
                    TokenEndReason::SpecifiedTerminatingChar,
                    &mut self.cross_state,
                )?;
            } else if state.in_operator() {
                //
                // We're in an operator. See if this character continues an operator, or if it
                // must be a separate token (because it wouldn't make a prefix of an operator).
                //

                let mut hypothetical_token = state.current_token().to_owned();
                hypothetical_token.push(c);

                if state.unquoted() && self.is_operator(hypothetical_token.as_ref()) {
                    self.consume_char()?;
                    state.append_char(c);
                } else {
                    assert!(state.started_token());

                    //
                    // N.B. If the completed operator indicates a here-document, then keep
                    // track that the *next* token should be the here-tag.
                    //
                    if self.cross_state.arithmetic_expansion {
                        //
                        // We're in an arithmetic context; don't consider << and <<-
                        // special. They're not here-docs, they're either a left-shift
                        // operator or a left-shift operator followed by a unary
                        // minus operator.
                        //

                        if state.is_specific_operator(")") && c == ')' {
                            self.cross_state.arithmetic_expansion = false;
                        }
                    } else if state.is_specific_operator("<<") {
                        self.cross_state.here_state =
                            HereState::NextTokenIsHereTag { remove_tabs: false };
                    } else if state.is_specific_operator("<<-") {
                        self.cross_state.here_state =
                            HereState::NextTokenIsHereTag { remove_tabs: true };
                    } else if state.is_specific_operator("(") && c == '(' {
                        self.cross_state.arithmetic_expansion = true;
                    }

                    let reason = if state.current_token() == "\n" {
                        TokenEndReason::UnescapedNewLine
                    } else {
                        TokenEndReason::OperatorEnd
                    };

                    let dash_follows = c == '-'
                        && (state.is_specific_operator(">&") || state.is_specific_operator("<&"));
                    result = state.delimit_current_token(reason, &mut self.cross_state)?;
                    self.cross_state.dash_follows_duplication = dash_follows;
                }
            //
            // See if this is a character that changes the current escaping/quoting state.
            //
            } else if does_char_newly_affect_quoting(&state, c) {
                if c == '\\' {
                    // Consume the backslash ourselves so we can peek past it.
                    self.consume_char()?;

                    if matches!(self.peek_char()?, Some('\n')) {
                        // Make sure the newline char gets consumed too.
                        self.consume_char()?;

                        // Make sure to include neither the backslash nor the newline character.
                    } else {
                        state.in_escape = true;
                        state.append_char(c);
                    }
                } else if c == '\'' {
                    if state.token_so_far.ends_with('$') {
                        state.quote_mode = QuoteMode::AnsiC(self.cross_state.cursor.clone());
                    } else {
                        state.quote_mode = QuoteMode::Single(self.cross_state.cursor.clone());
                    }

                    self.consume_char()?;
                    state.append_char(c);
                } else if c == '\"' {
                    state.quote_mode = QuoteMode::Double(self.cross_state.cursor.clone());
                    self.consume_char()?;
                    state.append_char(c);
                }
            }
            //
            // Handle end of single-quote, double-quote, or ANSI-C quote.
            else if !state.in_escape
                && matches!(
                    state.quote_mode,
                    QuoteMode::Single(..) | QuoteMode::AnsiC(..)
                )
                && c == '\''
            {
                state.quote_mode = QuoteMode::None;
                self.consume_char()?;
                state.append_char(c);
            } else if !state.in_escape
                && matches!(state.quote_mode, QuoteMode::Double(..))
                && c == '\"'
            {
                state.quote_mode = QuoteMode::None;
                self.consume_char()?;
                state.append_char(c);
            }
            //
            // Handle end of escape sequence.
            // TODO(tokenizer): Handle double-quote specific escape sequences.
            else if state.in_escape {
                state.in_escape = false;
                self.consume_char()?;
                state.append_char(c);
            } else if (state.unquoted()
                || (matches!(state.quote_mode, QuoteMode::Double(_)) && !state.in_escape))
                && (c == '$' || c == '`')
            {
                // TODO(tokenizer): handle quoted $ or ` in a double quote
                if c == '$' {
                    // Consume the '$' so we can peek beyond.
                    self.consume_char()?;

                    // Now peek beyond to see what we have.
                    let char_after_dollar_sign = self.peek_char()?;
                    match char_after_dollar_sign {
                        Some('(') => {
                            // Add the '$' we already consumed to the token.
                            state.append_char('$');

                            // Consume the '(' and add it to the token.
                            state.append_char(self.next_char()?.unwrap());

                            // Check to see if this is possibly an arithmetic expression
                            // (i.e., one that starts with `$((`).
                            let (initial_nesting, is_arithmetic) =
                                if matches!(self.peek_char()?, Some('(')) {
                                    // Consume the second '(' and add it to the token.
                                    state.append_char(self.next_char()?.unwrap());
                                    (2, true)
                                } else {
                                    (1, false)
                                };

                            if is_arithmetic {
                                self.cross_state.arithmetic_expansion = true;
                            }

                            // Bash names the line an arithmetic expansion left open began on.
                            let start = self.cross_state.cursor.clone();
                            let pending = self.set_aside_pending_here_docs();
                            self.consume_nested_construct(&mut state, ')', "(", initial_nesting)
                                .map_err(|error| match error {
                                    TokenizerError::UnterminatedExpansion(closing)
                                        if is_arithmetic =>
                                    {
                                        TokenizerError::UnterminatedArithmetic(closing, start)
                                    }
                                    error => error,
                                })?;
                            self.restore_pending_here_docs(pending);

                            if is_arithmetic {
                                self.cross_state.arithmetic_expansion = false;
                            }
                        }

                        Some('[') => {
                            // Add the '$' we already consumed to the token.
                            state.append_char('$');

                            // Consume the '[' and add it to the token.
                            state.append_char(self.next_char()?.unwrap());

                            // Keep track that we're in an arithmetic expression, since
                            // some text will be interpreted differently as a result.
                            self.cross_state.arithmetic_expansion = true;

                            let start = self.cross_state.cursor.clone();
                            let pending = self.set_aside_pending_here_docs();
                            self.consume_nested_construct(&mut state, ']', "[", 1)
                                .map_err(|error| match error {
                                    TokenizerError::UnterminatedExpansion(closing) => {
                                        TokenizerError::UnterminatedArithmetic(closing, start)
                                    }
                                    error => error,
                                })?;
                            self.restore_pending_here_docs(pending);

                            self.cross_state.arithmetic_expansion = false;
                        }

                        Some('{') => {
                            // Add the '$' we already consumed to the token.
                            state.append_char('$');

                            // Consume the '{' and add it to the token.
                            state.append_char(self.next_char()?.unwrap());

                            // `${ command; }` and `${| command; }` (bash 5.3) hold a command,
                            // read like the one in `$(...)`.
                            if matches!(self.peek_char()?, Some(' ' | '\t' | '\n' | '|')) {
                                let pending = self.set_aside_pending_here_docs();
                                self.consume_nested_construct(&mut state, '}', "{", 1)?;
                                self.restore_pending_here_docs(pending);
                                continue;
                            }

                            // Bash names the line a parameter expansion left open began on.
                            let start = self.cross_state.cursor.clone();
                            let pending = self.set_aside_pending_here_docs();
                            let mut pending_here_doc_tokens = vec![];
                            let mut drain_here_doc_tokens = false;

                            loop {
                                let cur_token = if drain_here_doc_tokens
                                    && !pending_here_doc_tokens.is_empty()
                                {
                                    if pending_here_doc_tokens.len() == 1 {
                                        drain_here_doc_tokens = false;
                                    }

                                    pending_here_doc_tokens.remove(0)
                                } else {
                                    let cur_token = self.next_token_until(
                                        Some('}'),
                                        false, /* include space? */
                                    )?;

                                    // See if this is a here-document-related token we need to hold
                                    // onto until after we've seen all the tokens that need to show
                                    // up before we get to the body.
                                    if matches!(
                                        cur_token.reason,
                                        TokenEndReason::HereDocumentBodyStart
                                            | TokenEndReason::HereDocumentBodyEnd
                                            | TokenEndReason::HereDocumentEndTag
                                    ) {
                                        pending_here_doc_tokens.push(cur_token);
                                        continue;
                                    }

                                    cur_token
                                };

                                if matches!(cur_token.reason, TokenEndReason::UnescapedNewLine)
                                    && !pending_here_doc_tokens.is_empty()
                                {
                                    pending_here_doc_tokens.push(cur_token);
                                    drain_here_doc_tokens = true;
                                    continue;
                                }

                                if let Some(cur_token_value) = cur_token.token {
                                    state.append_str(cur_token_value.to_str());
                                }

                                match cur_token.reason {
                                    TokenEndReason::HereDocumentBodyStart => {
                                        state.append_char('\n');
                                    }
                                    TokenEndReason::NonNewLineBlank => state.append_char(' '),
                                    TokenEndReason::SpecifiedTerminatingChar => {
                                        // We hit the end brace we were looking for but did not
                                        // yet consume it. Do so now.
                                        state.append_char(self.next_char()?.unwrap());
                                        break;
                                    }
                                    TokenEndReason::EndOfInput => {
                                        return Err(TokenizerError::UnterminatedVariable(start));
                                    }
                                    _ => (),
                                }
                            }
                            self.restore_pending_here_docs(pending);
                        }
                        _ => {
                            // This is either a different character, or else the end of the string.
                            // Either way, add the '$' we already consumed to the token.
                            state.append_char('$');
                        }
                    }
                } else {
                    // We look for the terminating backquote. First disable normal consumption and
                    // consume the starting backquote.
                    let backquote_pos = self.cross_state.cursor.clone();
                    self.consume_char()?;

                    // Add the opening backquote to the token.
                    state.append_char(c);

                    // Now continue until we see an unescaped backquote.
                    let mut escaping_enabled = false;
                    let mut done = false;
                    while !done {
                        // Read (and consume) the next char.
                        let next_char_in_backquote = self.next_char()?;
                        if let Some(cib) = next_char_in_backquote {
                            // Include it in the token no matter what.
                            state.append_char(cib);

                            // Watch out for escaping.
                            if !escaping_enabled && cib == '\\' {
                                escaping_enabled = true;
                            } else {
                                // Look for an unescaped backquote to terminate.
                                if !escaping_enabled && cib == '`' {
                                    done = true;
                                }
                                escaping_enabled = false;
                            }
                        } else {
                            return Err(TokenizerError::UnterminatedBackquote(backquote_pos));
                        }
                    }
                }
            }
            //
            // In a command's first words, `NAME[` starts an array element to assign to. As bash
            // does, read through the matching `]` as part of the word, blanks and operators
            // included (`b[x > 2]=y`).
            else if c == '['
                && self.cross_state.command_position
                && self.cross_state.nested_constructs == 0
                && state.unquoted()
                && !state.in_operator()
                && is_valid_name(state.current_token())
            {
                self.consume_char()?;
                state.append_char(c);
                self.consume_array_subscript(&mut state)?;
            }
            //
            // A word of a compound array assignment that starts with `[` starts with a subscript
            // (`a=([ 1 ]=x)`): as in bash, read through the matching `]` as part of the word,
            // blanks included.
            else if c == '['
                && self.cross_state.compound_assignment
                && self.cross_state.nested_constructs == 0
                && !state.started_token()
            {
                self.consume_char()?;
                state.append_char(c);
                self.consume_array_subscript(&mut state)?;
            }
            //
            // [Extension]
            // If extended globbing is enabled, the last consumed character is an
            // unquoted start of an extglob pattern, *and* if the current character
            // is an open parenthesis, then this begins an extglob pattern.
            else if c == '('
                && self.options.enable_extended_globbing
                && state.unquoted()
                && !state.in_operator()
                && state
                    .current_token()
                    .ends_with(|x| Self::can_start_extglob(x))
            {
                // Consume the '(' and append it.
                self.consume_char()?;
                state.append_char(c);

                let mut paren_depth = 1;
                let mut in_escape = false;

                // Keep consuming until we see the matching end ')'.
                while paren_depth > 0 {
                    if let Some(extglob_char) = self.next_char()? {
                        // Include it in the token.
                        state.append_char(extglob_char);

                        match extglob_char {
                            _ if in_escape => in_escape = false,
                            '\\' => in_escape = true,
                            '(' => paren_depth += 1,
                            ')' => paren_depth -= 1,
                            _ => (),
                        }
                    } else {
                        return Err(TokenizerError::UnterminatedExtendedGlob(
                            self.cross_state.cursor.clone(),
                        ));
                    }
                }
            //
            // As in bash, `<(` or `>(` inside a word (`--file=<(list)`), or starting an element
            // of a compound array assignment (`a=(<(list))`), is a process substitution read as
            // part of the word.
            //
            } else if matches!(c, '<' | '>')
                && !self.options.sh_mode
                && state.unquoted()
                && !state.in_operator()
                && !self.cross_state.arithmetic_expansion
                && if state.started_token() {
                    !state.only_blanks_so_far()
                } else {
                    self.cross_state.compound_assignment
                }
                && matches!(self.peek_second_char(), Ok(Some('(')))
            {
                self.consume_char()?;
                state.append_char(c);
                self.consume_char()?;
                state.append_char('(');

                let pending = self.set_aside_pending_here_docs();
                self.consume_nested_construct(&mut state, ')', "(", 1)?;
                self.restore_pending_here_docs(pending);
            //
            // If the character *can* start an operator, then it will.
            //
            } else if state.unquoted() && Self::can_start_operator(c) {
                // `NAME=(` (or `NAME+=(`) opens a compound array assignment, and its `)` closes
                // it.
                if c == '(' && state.started_token() {
                    self.cross_state.compound_assignment =
                        is_assignment_word(state.current_token())
                            && state.current_token().ends_with('=');
                } else if c == ')' {
                    self.cross_state.compound_assignment = false;
                }
                if state.started_token() {
                    result = state.delimit_current_token(
                        TokenEndReason::OperatorStart,
                        &mut self.cross_state,
                    )?;
                } else {
                    state.token_is_operator = true;
                    self.consume_char()?;
                    state.append_char(c);
                }
            //
            // Whitespace gets discarded (and delimits tokens).
            //
            } else if state.unquoted() && is_blank(c) {
                if state.started_token() {
                    result = state.delimit_current_token(
                        TokenEndReason::NonNewLineBlank,
                        &mut self.cross_state,
                    )?;
                } else if include_space {
                    state.append_char(c);
                } else {
                    // Make sure we don't include this char in the token range.
                    state.start_position.column += 1;
                    state.start_position.index += 1;
                }

                self.consume_char()?;
            }
            //
            // N.B. We need to remember if we were recursively called in a variable
            // expansion expression; in that case we won't think a token was started but...
            // we'd be wrong.
            //
            // The `!only_blanks_so_far` clause keeps a comment recognizable inside a nested
            // construct. With `include_space`, a blank is appended to the token when none is
            // started and delimits the token when one is, so the blanks before a `#` alternate
            // between the two; after an odd number of them a token is "in progress" and the `#`
            // was appended to it rather than starting a comment. `$( #'<newline>)` then failed to
            // tokenize with an unterminated single quote, while `$(  #'<newline>)` — two blanks —
            // was fine.
            //
            else if !state.token_is_operator
                && (state.started_token() || matches!(terminating_char, Some('}')))
                && !(c == '#' && state.only_blanks_so_far())
            {
                self.consume_char()?;
                state.append_char(c);
            } else if c == '#' && !self.cross_state.arithmetic_expansion {
                // Consume the '#'.
                self.consume_char()?;

                let mut done = false;
                while !done {
                    done = match self.peek_char()? {
                        Some('\n') => true,
                        None => true,
                        _ => {
                            // Consume the peeked char; it's part of the comment.
                            self.consume_char()?;
                            false
                        }
                    };
                }
                // Re-start loop as if the comment never happened.
            } else if state.started_token() {
                // In all other cases where we have an in-progress token, we delimit here.
                result =
                    state.delimit_current_token(TokenEndReason::Other, &mut self.cross_state)?;
            } else {
                // If we got here, then we don't have a token in progress and we're not starting an
                // operator. Add the character to a new token.
                self.consume_char()?;
                state.append_char(c);
            }
        }

        let result = result.unwrap();

        Ok(result)
    }

    /// Consumes an array subscript through its matching `]` (the `[` already consumed), quotes
    /// and nested brackets included, appending it to the token.
    fn consume_array_subscript(
        &mut self,
        state: &mut TokenParseState,
    ) -> Result<(), TokenizerError> {
        let mut depth = 1;
        let mut quote = None;
        while depth > 0 {
            let Some(c) = self.next_char()? else {
                return Err(TokenizerError::UnterminatedExpansion(']'));
            };
            state.append_char(c);
            match (quote, c) {
                (Some('\''), '\'') | (Some('"'), '"') => quote = None,
                (Some('\''), _) => (),
                (_, '\\') => {
                    if let Some(escaped) = self.next_char()? {
                        state.append_char(escaped);
                    }
                }
                (Some(_), _) => (),
                (None, '\'' | '"') => quote = Some(c),
                (None, '[') => depth += 1,
                (None, ']') => depth -= 1,
                _ => (),
            }
        }
        Ok(())
    }

    fn remove_here_end_tag(
        &mut self,
        state: &mut TokenParseState,
        result: &mut Option<TokenizeResult>,
        ends_with_newline: bool,
    ) -> Result<bool, TokenizerError> {
        // Bail immediately if we don't even have a *starting* here tag.
        if self.cross_state.current_here_tags.is_empty() {
            return Ok(false);
        }

        let next_here_tag = &self.cross_state.current_here_tags[0];

        let tag_str: Cow<'_, str> = if next_here_tag.tag_was_escaped_or_quoted {
            unquote_str(next_here_tag.tag.as_str()).into()
        } else {
            next_here_tag.tag.as_str().into()
        };

        let tag_str = if !ends_with_newline {
            tag_str
                .strip_suffix('\n')
                .unwrap_or_else(|| tag_str.as_ref())
        } else {
            tag_str.as_ref()
        };

        if let Some(current_token_without_here_tag) = state.current_token().strip_suffix(tag_str) {
            // Make sure that was either the start of the here document, or there
            // was a newline between the preceding part
            // and the tag.
            if current_token_without_here_tag.is_empty()
                || current_token_without_here_tag.ends_with('\n')
            {
                state.replace_with_here_doc(current_token_without_here_tag.to_owned());

                // Delimit the end of the here-document body.
                *result = state.delimit_current_token(
                    TokenEndReason::HereDocumentBodyEnd,
                    &mut self.cross_state,
                )?;

                return Ok(true);
            }
        }
        Ok(false)
    }

    const fn can_start_extglob(c: char) -> bool {
        matches!(c, '@' | '!' | '?' | '+' | '*')
    }

    const fn can_start_operator(c: char) -> bool {
        matches!(c, '&' | '(' | ')' | ';' | '\n' | '|' | '<' | '>')
    }

    fn is_operator(&self, s: &str) -> bool {
        // Handle non-POSIX operators.
        if !self.options.sh_mode && matches!(s, "<<<" | "&>" | "&>>" | ";;&" | ";&" | "|&") {
            return true;
        }

        matches!(
            s,
            "&" | "&&"
                | "("
                | ")"
                | ";"
                | ";;"
                | "\n"
                | "|"
                | "||"
                | "<"
                | ">"
                | ">|"
                | "<<"
                | ">>"
                | "<&"
                | ">&"
                | "<<-"
                | "<>"
        )
    }
}

impl<R: ?Sized + std::io::BufRead> Iterator for Tokenizer<'_, R> {
    type Item = Result<TokenizeResult, TokenizerError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_token() {
            #[expect(clippy::manual_map)]
            Ok(result) => match result.token {
                Some(_) => Some(Ok(result)),
                None => None,
            },
            Err(e) => Some(Err(e)),
        }
    }
}

const fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Where the tokens of a command substitution are in the `case` commands open in it, so that the
/// `)` ending a pattern is not taken for the one closing the substitution (bash reads the
/// substitution as a command).
struct CaseTracker {
    /// For each open `case`, where its tokens are.
    open: Vec<CaseState>,
    /// Whether the next word starts a command, so a `case` there opens one.
    command_start: bool,
    /// Whether the current pattern began with its optional `(`.
    pattern_paren: bool,
}

#[derive(PartialEq, Eq)]
enum CaseState {
    /// After `case WORD`, before `in`.
    ExpectIn,
    /// In a pattern list, before its `)`.
    Pattern,
    /// In the commands after a pattern list.
    Body,
}

impl CaseTracker {
    const fn new() -> Self {
        Self {
            open: vec![],
            command_start: true,
            pattern_paren: false,
        }
    }

    /// Notes a token of the substitution.
    fn note(&mut self, token: &Token) {
        match token {
            Token::Word(word, _) => {
                let word = word.trim_matches(is_blank);
                match (self.open.last(), word) {
                    (_, "case") if self.command_start => self.open.push(CaseState::ExpectIn),
                    (Some(CaseState::ExpectIn), "in") => {
                        self.open.pop();
                        self.open.push(CaseState::Pattern);
                    }
                    (Some(CaseState::Pattern), "esac") => {
                        self.open.pop();
                    }
                    (Some(CaseState::Body), "esac") if self.command_start => {
                        self.open.pop();
                    }
                    _ => (),
                }
                self.command_start = matches!(
                    word,
                    "then" | "do" | "else" | "elif" | "if" | "while" | "until" | "{" | "!" | "time"
                );
            }
            Token::Operator(operator, _) => {
                let operator = operator.trim_matches(is_blank);
                if matches!(operator, ";;" | ";&" | ";;&")
                    && self.open.last() == Some(&CaseState::Body)
                {
                    self.open.pop();
                    self.open.push(CaseState::Pattern);
                }
                if operator == "(" && self.open.last() == Some(&CaseState::Pattern) {
                    self.pattern_paren = true;
                }
                self.command_start = matches!(
                    operator,
                    ";" | "&" | "&&" | "||" | "|" | "|&" | "(" | "\n" | ";;" | ";&" | ";;&"
                );
            }
        }
    }

    /// Whether a `)` ends a case pattern rather than closing a parenthesis; it moves past the
    /// pattern either way. A pattern's optional `(` is closed by the `)` too, so that `)` still
    /// balances it.
    fn closes_pattern(&mut self) -> bool {
        if self.open.last() != Some(&CaseState::Pattern) {
            return false;
        }
        self.open.pop();
        self.open.push(CaseState::Body);
        self.command_start = true;
        !std::mem::take(&mut self.pattern_paren)
    }
}

/// Whether `s` is a valid variable name.
/// The line of the last character read, given the position after it: the line before when that
/// character was a newline.
const fn last_line_read(cursor: &SourcePosition) -> usize {
    if cursor.column == 1 && cursor.line > 1 {
        cursor.line - 1
    } else {
        cursor.line
    }
}

fn is_valid_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether the word after a token is still among a command's first words, where it can be an
/// assignment: after a control operator or a reserved word that starts a command, or after an
/// assignment in that position.
fn starts_command_words(token: &str, is_operator: bool, command_position: bool) -> bool {
    if is_operator {
        return matches!(
            token,
            ";" | "&" | "&&" | "||" | "|" | "|&" | "(" | ")" | "\n" | ";;" | ";&" | ";;&"
        );
    }
    if matches!(
        token,
        "if" | "then" | "else" | "elif" | "do" | "while" | "until" | "!" | "{" | "time"
    ) {
        return true;
    }
    command_position && is_assignment_word(token)
}

/// Whether `token` looks like an assignment: a name, an optional subscript, then `=` or `+=`.
pub(crate) fn is_assignment_word(token: &str) -> bool {
    let name_len = token
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(token.len());
    let (name, mut rest) = token.split_at(name_len);
    if !is_valid_name(name) {
        return false;
    }
    if rest.starts_with('[') {
        let mut depth = 0;
        let Some(end) = rest.find(|c| {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => (),
            }
            depth == 0
        }) else {
            return false;
        };
        rest = rest.get(end + 1..).unwrap_or_default();
    }
    rest.starts_with('=') || rest.starts_with("+=")
}

const fn does_char_newly_affect_quoting(state: &TokenParseState, c: char) -> bool {
    // If we're currently escaped, then nothing affects quoting.
    if state.in_escape {
        return false;
    }

    match state.quote_mode {
        // When we're in a double quote or ANSI-C quote, only a subset of escape
        // sequences are recognized.
        QuoteMode::Double(_) | QuoteMode::AnsiC(_) => {
            if c == '\\' {
                // TODO(tokenizer): handle backslash in double quote
                true
            } else {
                false
            }
        }
        // When we're in a single quote, nothing affects quoting.
        QuoteMode::Single(_) => false,
        // When we're not already in a quote, then we can straightforwardly look for a
        // quote mark or backslash.
        QuoteMode::None => is_quoting_char(c),
    }
}

const fn is_quoting_char(c: char) -> bool {
    matches!(c, '\\' | '\'' | '\"')
}

/// Return a string with all the quoting removed, honoring POSIX quote semantics.
///
/// Inside single quotes every character (including backslash) is literal; inside
/// double quotes backslash escapes only `$`, `` ` ``, `"`, `\`, and newline (line
/// continuation) and is otherwise retained; outside quotes backslash escapes the
/// next character.
///
/// # Arguments
///
/// * `s` - The string to unquote.
pub fn unquote_str(s: &str) -> String {
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut result = String::new();
    let mut quote = Quote::None;
    let mut in_escape = false;

    for c in s.chars() {
        match quote {
            Quote::None => {
                if in_escape {
                    result.push(c);
                    in_escape = false;
                } else {
                    match c {
                        '\\' => in_escape = true,
                        '\'' => quote = Quote::Single,
                        '"' => quote = Quote::Double,
                        c => result.push(c),
                    }
                }
            }
            Quote::Single => {
                if c == '\'' {
                    quote = Quote::None;
                } else {
                    result.push(c);
                }
            }
            Quote::Double => {
                if in_escape {
                    in_escape = false;
                    match c {
                        '$' | '`' | '"' | '\\' => result.push(c),
                        '\n' => (),
                        c => {
                            result.push('\\');
                            result.push(c);
                        }
                    }
                } else {
                    match c {
                        '\\' => in_escape = true,
                        '"' => quote = Quote::None,
                        c => result.push(c),
                    }
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {

    use super::*;
    use anyhow::Result;
    use insta::assert_ron_snapshot;
    use pretty_assertions::{assert_eq, assert_matches};

    #[derive(serde::Serialize, serde::Deserialize)]
    struct TokenizerResult<'a> {
        input: &'a str,
        result: Vec<Token>,
    }

    fn test_tokenizer(input: &str) -> Result<TokenizerResult<'_>> {
        Ok(TokenizerResult {
            input,
            result: tokenize_str(input)?,
        })
    }

    #[test]
    fn tokenize_empty() -> Result<()> {
        let tokens = tokenize_str("")?;
        assert_eq!(tokens.len(), 0);
        Ok(())
    }

    #[test]
    fn tokenize_line_continuation() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(
            r"a\
bc"
        )?);
        Ok(())
    }

    #[test]
    fn tokenize_operators() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer("a>>b")?);
        Ok(())
    }

    #[test]
    fn tokenize_dash_after_duplication_as_its_own_word() -> Result<()> {
        // As bash reads it: `>&-1` closes stdout and passes `1` on as a word.
        let words = |input: &str| -> Result<Vec<String>> {
            Ok(tokenize_str(input)?
                .iter()
                .map(|token| token.to_str().to_owned())
                .collect())
        };
        assert_eq!(words("echo x >&-1")?, ["echo", "x", ">&", "-", "1"]);
        assert_eq!(words("cat <&-x")?, ["cat", "<&", "-", "x"]);
        assert_eq!(words("echo 2>&-")?, ["echo", "2", ">&", "-"]);
        assert_eq!(words("echo >&1-")?, ["echo", ">&", "1-"]);
        Ok(())
    }

    #[test]
    fn tokenize_comment() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(
            r"a #comment
"
        )?);
        Ok(())
    }

    #[test]
    fn tokenize_comment_in_command_substitution() {
        // A comment inside `$( )` is a comment however many blanks precede its `#`. An odd number
        // of them used to leave a token in progress, so the `#` was appended to that token instead
        // of starting a comment, and the apostrophe in the comment's text then opened a quote that
        // was never closed.
        //
        // The comment is dropped from the text reconstructed for the substitution, leaving just
        // the blanks that preceded it. A blank that delimits an in-progress token comes back as a
        // single space, so the blanks aren't always reproduced verbatim; that's insignificant
        // inside `$( )`, where the text gets re-parsed as a program.
        for (prefix, reconstructed_blanks) in [
            ("", ""),
            (" ", " "),
            ("  ", "  "),
            ("   ", "   "),
            ("\t", "\t"),
            ("\t\t", "\t "),
            (" \t", "  "),
            ("\t ", "\t "),
        ] {
            let input = format!("$({prefix}# it's a comment\n)\n");
            let tokens = tokenize_str(input.as_str()).unwrap();
            let token_strs: Vec<_> = tokens.iter().map(Token::to_str).collect();
            assert_eq!(
                token_strs,
                [format!("$({reconstructed_blanks}\n)").as_str(), "\n"],
                "tokenizing {input:?}"
            );
        }
    }

    #[test]
    fn tokenize_comment_at_eof() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(r"a #comment")?);
        Ok(())
    }

    #[test]
    fn tokenize_empty_here_doc() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(
            r"cat <<HERE
HERE
"
        )?);
        Ok(())
    }

    #[test]
    fn tokenize_here_doc() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(
            r"cat <<HERE
SOMETHING
HERE
echo after
"
        )?);
        assert_ron_snapshot!(test_tokenizer(
            r"cat <<HERE
SOMETHING
HERE
"
        )?);
        assert_ron_snapshot!(test_tokenizer(
            r"cat <<HERE
SOMETHING
HERE

"
        )?);
        assert_ron_snapshot!(test_tokenizer(
            r"cat <<HERE
SOMETHING
HERE"
        )?);
        Ok(())
    }

    #[test]
    fn tokenize_here_doc_with_tab_removal() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(
            r"cat <<-HERE
	SOMETHING
	HERE
"
        )?);
        Ok(())
    }

    #[test]
    fn tokenize_here_doc_with_other_tokens() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(
            r"cat <<EOF | wc -l
A B C
1 2 3
D E F
EOF
"
        )?);
        Ok(())
    }

    #[test]
    fn tokenize_multiple_here_docs() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(
            r"cat <<HERE1 <<HERE2
SOMETHING
HERE1
OTHER
HERE2
echo after
"
        )?);
        Ok(())
    }

    #[test]
    fn tokenize_expansions_after_here_doc_operator() -> Result<()> {
        // Tokens after a here tag wait for the here-document's body, but the pieces of a nested
        // construct are not tokens of that line: `${f}` stays whole instead of losing its `f`.
        let strs = |input: &str| -> Result<Vec<String>> {
            Ok(tokenize_str(input)?
                .iter()
                .map(|t| t.to_str().to_owned())
                .collect())
        };
        assert_eq!(
            strs("cat <<EOF > \"${f}\" $(echo x) $((1+2)) $[3]\nhello\nEOF\n")?,
            [
                "cat",
                "<<",
                "EOF",
                "hello\n",
                "EOF",
                ">",
                "\"${f}\"",
                "$(echo x)",
                "$((1+2))",
                "$[3]",
                "\n"
            ]
        );
        // A substitution spanning lines ends before the pending body starts.
        assert_eq!(
            strs("cat <<EOF; x=$(\necho hi\n)\nbody\nEOF\n")?,
            [
                "cat",
                "<<",
                "EOF",
                "body\n",
                "EOF",
                ";",
                "x=$(\necho hi\n)",
                "\n"
            ]
        );
        // An unquoted here-document joins backslash-newline, unless the backslash is escaped;
        // a quoted one keeps it.
        assert_eq!(
            strs("cat <<EOF\na \\\nb \\\\\nc\nEOF\n")?,
            ["cat", "<<", "EOF", "a b \\\\\nc\n", "EOF", "\n"]
        );
        assert_eq!(
            strs("cat <<'EOF'\na \\\nb\nEOF\n")?,
            ["cat", "<<", "'EOF'", "a \\\nb\n", "EOF", "\n"]
        );
        // A here tag spelled with an expansion is the tag, literally.
        assert_eq!(
            strs("cat <<${x}\nbody\n${x}\n")?,
            ["cat", "<<", "${x}", "body\n", "${x}", "\n"]
        );
        Ok(())
    }

    #[test]
    fn tokenize_here_documents_in_command_substitutions() -> Result<()> {
        // A `)` in the body of a here-document inside `$( )` does not close it.
        assert_eq!(
            tokenize_str("x=$(cat <<EOF\n)\nEOF\n); echo")?
                .iter()
                .map(|t| t.to_str().to_owned())
                .collect::<Vec<_>>(),
            ["x=$(cat <<EOF\n)\nEOF\n)", ";", "echo"]
        );
        Ok(())
    }

    #[test]
    #[allow(clippy::panic)]
    fn tokenize_unterminated_here_documents() {
        let open = |input: &str| -> Vec<(String, String, usize)> {
            match tokenize_str(input) {
                Err(TokenizerError::UnterminatedHereDocuments(documents)) => documents
                    .into_iter()
                    .map(|d| (d.tag, d.delimiter, d.line))
                    .collect(),
                other => panic!("{input:?}: {other:?}"),
            }
        };
        let doc = |tag: &str, delimiter: &str, line| (tag.to_owned(), delimiter.to_owned(), line);
        // Bash names the line read last before each body began.
        assert_eq!(
            open("cat <<\"A, B\" <<'C'\nhello\n"),
            [doc("\"A, B\"", "A, B", 1), doc("'C'", "C", 2)]
        );
        assert_eq!(
            open("echo\ncat <<A <<B <<C\na\nA\nb"),
            [doc("B", "B", 4), doc("C", "C", 5)]
        );
        assert_eq!(open("cat <<A; x=$(\necho)\nb"), [doc("A", "A", 2)]);
        // The input can end on the tag itself.
        assert_eq!(open("echo a; cat <<A"), [doc("A", "A", 1)]);
    }

    #[test]
    fn tokenize_process_substitutions_in_words() -> Result<()> {
        let strs = |input: &str| -> Result<Vec<String>> {
            Ok(tokenize_str(input)?
                .iter()
                .map(|t| t.to_str().to_owned())
                .collect())
        };
        // Inside a word, or starting an element of a compound array assignment, `<(` and `>(`
        // are read with the word; at a word's start they are operators, and `<` before anything
        // else still ends the word.
        assert_eq!(
            strs("x=<(echo a) cmd --f=>(cat; echo)z b<c <(d)")?,
            [
                "x=<(echo a)",
                "cmd",
                "--f=>(cat; echo)z",
                "b",
                "<",
                "c",
                "<",
                "(",
                "d",
                ")"
            ]
        );
        assert_eq!(
            strs("a=(<(true) x >(y)) b+=(<(z)); c=( <(w))")?,
            [
                "a=", "(", "<(true)", "x", ">(y)", ")", "b+=", "(", "<(z)", ")", ";", "c=", "(",
                "<(w)", ")"
            ]
        );
        assert_eq!(
            strs("f (<(x)); (( 1<(2) )); echo $(( 3>(2) ))")?,
            [
                "f",
                "(",
                "<",
                "(",
                "x",
                ")",
                ")",
                ";",
                "(",
                "(",
                "1",
                "<",
                "(",
                "2",
                ")",
                ")",
                ")",
                ";",
                "echo",
                "$(( 3>(2) ))"
            ]
        );
        Ok(())
    }

    #[test]
    fn tokenize_array_element_assignments() -> Result<()> {
        let strs = |input: &str| -> Result<Vec<String>> {
            Ok(tokenize_str(input)?
                .iter()
                .map(|t| t.to_str().to_owned())
                .collect())
        };
        // In a command's first words, an element's subscript is part of the word.
        assert_eq!(strs("b[x>2]=y")?, ["b[x>2]=y"]);
        assert_eq!(
            strs("a=1 b[1 + (2)]+=x c[\"]\" d]=z cmd a[1 + 1]=w")?,
            [
                "a=1",
                "b[1 + (2)]+=x",
                "c[\"]\" d]=z",
                "cmd",
                "a[1",
                "+",
                "1]=w"
            ]
        );
        assert_eq!(
            strs("if true; then e[a[1] > 0]=v; fi")?,
            ["if", "true", ";", "then", "e[a[1] > 0]=v", ";", "fi"]
        );
        // So is a leading subscript in a compound assignment's elements, blanks and all.
        assert_eq!(
            strs("b=([ 1 ]=x [2]=y [ a[0] ]=z [ 1 ] =w c) m+=(\n[ k\t]=v)")?,
            [
                "b=",
                "(",
                "[ 1 ]=x",
                "[2]=y",
                "[ a[0] ]=z",
                "[ 1 ]",
                "=w",
                "c",
                ")",
                "m+=",
                "(",
                "\n",
                "[ k\t]=v",
                ")"
            ]
        );
        // Not elsewhere.
        assert_eq!(strs("echo [ 1 ]")?, ["echo", "[", "1", "]"]);
        // A case pattern's `)` inside a substitution does not close it.
        assert_eq!(
            strs("x=$(case a in a) echo m;; (b|c) echo n;; esac); y=$( (echo s) )")?,
            [
                "x=$(case a in a) echo m;; (b|c) echo n;; esac)",
                ";",
                "y=$( (echo s) )"
            ]
        );
        // A backslash that ends the input is an ordinary character.
        assert_eq!(strs("echo a \\")?, ["echo", "a", "\\"]);
        // Legacy arithmetic ends at the bracket matching its own.
        assert_eq!(strs("echo $[a[0] < 9]")?, ["echo", "$[a[0] < 9]"]);
        assert_eq!(
            strs("echo $[+(a[x-4]) + b[1]]")?,
            ["echo", "$[+(a[x-4]) + b[1]]"]
        );
        // In arithmetic, `#` is an operator (a base), not a comment.
        assert_eq!(
            strs("(( 2#1 # 2 ))")?,
            ["(", "(", "2#1", "#", "2", ")", ")"]
        );
        Ok(())
    }

    #[test]
    fn tokenize_unterminated_here_doc() {
        let result = tokenize_str(
            r"cat <<HERE
SOMETHING
",
        );
        assert!(result.is_err());
    }

    #[test]
    fn tokenize_missing_here_tag() {
        let result = tokenize_str(
            r"cat <<
",
        );
        assert!(result.is_err());
    }

    #[test]
    fn tokenize_here_doc_in_command_substitution() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(
            r"echo $(cat <<HERE
TEXT
HERE
)"
        )?);
        Ok(())
    }

    #[test]
    fn tokenize_here_doc_in_double_quoted_command_substitution() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(
            r#"echo "$(cat <<HERE
TEXT
HERE
)""#
        )?);
        Ok(())
    }

    #[test]
    fn tokenize_here_doc_in_double_quoted_command_substitution_with_space() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(
            r#"echo "$(cat << HERE
TEXT
HERE
)""#
        )?);
        Ok(())
    }

    #[test]
    fn tokenize_complex_here_docs_in_command_substitution() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(
            r"echo $(cat <<HERE1 <<HERE2 | wc -l
TEXT
HERE1
OTHER
HERE2
)"
        )?);
        Ok(())
    }

    #[test]
    fn tokenize_simple_backquote() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(r"echo `echo hi`")?);
        Ok(())
    }

    #[test]
    fn tokenize_backquote_with_escape() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(r"echo `echo\`hi`")?);
        Ok(())
    }

    #[test]
    fn tokenize_unterminated_backquote() {
        assert_matches!(
            tokenize_str("`"),
            Err(TokenizerError::UnterminatedBackquote(_))
        );
    }

    #[test]
    fn tokenize_unterminated_command_substitution() {
        // $( is consumed before the tokenizer knows whether it's $( or $((,
        // so it goes through consume_nested_construct and yields UnterminatedExpansion.
        assert_matches!(
            tokenize_str("$("),
            Err(TokenizerError::UnterminatedExpansion(_))
        );
    }

    #[test]
    fn tokenize_unterminated_arithmetic_expansion() {
        assert_matches!(
            tokenize_str("$(("),
            Err(TokenizerError::UnterminatedArithmetic(')', _))
        );
    }

    #[test]
    fn tokenize_unterminated_legacy_arithmetic_expansion() {
        assert_matches!(
            tokenize_str("$["),
            Err(TokenizerError::UnterminatedArithmetic(']', _))
        );
    }

    #[test]
    fn tokenize_command_substitution() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer("a$(echo hi)b c")?);
        Ok(())
    }

    #[test]
    fn tokenize_command_substitution_with_subshell() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer("$( (:) )")?);
        Ok(())
    }

    #[test]
    fn tokenize_command_substitution_containing_extglob() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer("echo $(echo !(x))")?);
        Ok(())
    }

    #[test]
    fn tokenize_arithmetic_expression() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer("a$((1+2))b c")?);
        Ok(())
    }

    #[test]
    fn tokenize_arithmetic_expression_with_space() -> Result<()> {
        // N.B. The spacing comes out a bit odd, but it gets processed okay
        // by later stages.
        assert_ron_snapshot!(test_tokenizer("$(( 1 ))")?);
        Ok(())
    }
    #[test]
    fn tokenize_arithmetic_expression_with_parens() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer("$(( (0) ))")?);
        Ok(())
    }

    #[test]
    fn tokenize_special_parameters() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer("$$")?);
        assert_ron_snapshot!(test_tokenizer("$@")?);
        assert_ron_snapshot!(test_tokenizer("$!")?);
        assert_ron_snapshot!(test_tokenizer("$?")?);
        assert_ron_snapshot!(test_tokenizer("$*")?);
        Ok(())
    }

    #[test]
    fn tokenize_unbraced_parameter_expansion() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer("$x")?);
        assert_ron_snapshot!(test_tokenizer("a$x")?);
        Ok(())
    }

    #[test]
    fn tokenize_unterminated_parameter_expansion() {
        assert_matches!(
            tokenize_str("${x"),
            Err(TokenizerError::UnterminatedVariable(_))
        );
    }

    #[test]
    fn tokenize_braced_parameter_expansion() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer("${x}")?);
        assert_ron_snapshot!(test_tokenizer("a${x}b")?);
        Ok(())
    }

    #[test]
    fn tokenize_braced_parameter_expansion_with_escaping() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(r"a${x\}}b")?);
        Ok(())
    }

    #[test]
    fn tokenize_whitespace() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer("1 2 3")?);
        Ok(())
    }

    #[test]
    fn tokenize_escaped_whitespace() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(r"1\ 2 3")?);
        Ok(())
    }

    #[test]
    fn tokenize_single_quote() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(r"x'a b'y")?);
        Ok(())
    }

    #[test]
    fn tokenize_double_quote() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(r#"x"a b"y"#)?);
        Ok(())
    }

    #[test]
    fn tokenize_double_quoted_command_substitution() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(r#"x"$(echo hi)"y"#)?);
        Ok(())
    }

    #[test]
    fn tokenize_double_quoted_arithmetic_expression() -> Result<()> {
        assert_ron_snapshot!(test_tokenizer(r#"x"$((1+2))"y"#)?);
        Ok(())
    }

    #[test]
    fn test_quote_removal() {
        assert_eq!(unquote_str(r#""hello""#), "hello");
        assert_eq!(unquote_str(r"'hello'"), "hello");
        assert_eq!(unquote_str(r#""hel\"lo""#), r#"hel"lo"#);
        // Single quotes are fully literal (POSIX): backslash is an ordinary character.
        assert_eq!(unquote_str(r"'\n'"), r"\n");
        assert_eq!(unquote_str(r"'hel\lo'"), r"hel\lo");
        // Double quotes: backslash escapes only $ ` " \ and newline; otherwise retained.
        assert_eq!(unquote_str(r#""a\nb""#), r"a\nb");
        assert_eq!(unquote_str(r#""a\\b""#), r"a\b");
        assert_eq!(unquote_str(r#""a\$b""#), "a$b");
        // Outside quotes, backslash escapes the next character.
        assert_eq!(unquote_str(r"hel\'lo"), "hel'lo");
    }
}
