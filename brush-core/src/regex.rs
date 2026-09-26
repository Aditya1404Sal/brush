#![allow(clippy::needless_pass_by_value)]

use std::borrow::Cow;
use std::cell::RefCell;

use crate::{error, rawbytes};
use cached::Cached;

/// Cache mapping a (pattern, case-insensitive, multiline) key to a compiled regex.
type RegexCache = cached::LruCache<(String, bool, bool), fancy_regex::Regex>;

thread_local! {
    // Wrapped in `Option` so that if cache construction ever fails we gracefully
    // degrade to compiling regexes uncached rather than panicking. (With a fixed
    // positive `max_size` this always succeeds, but `build()` is fallible.)
    static REGEX_CACHE: RefCell<Option<RegexCache>> =
        RefCell::new(cached::LruCache::builder().max_size(64).build().ok());
}

/// Represents a piece of a regular expression.
#[derive(Clone, Debug)]
pub(crate) enum RegexPiece {
    /// A pattern that should be interpreted as a regular expression.
    Pattern(String),
    /// A literal string that should be matched exactly.
    Literal(String),
}

impl RegexPiece {
    fn to_regex_str(&self) -> Cow<'_, str> {
        match self {
            Self::Pattern(s) => Cow::Borrowed(s.as_str()),
            Self::Literal(s) => escape_literal_regex_piece(s.as_str()),
        }
    }
}

type RegexWord = Vec<RegexPiece>;

/// Encapsulates a regular expression usable in the shell.
#[derive(Clone, Debug)]
pub struct Regex {
    pieces: RegexWord,
    case_insensitive: bool,
    multiline: bool,
}

impl From<RegexWord> for Regex {
    fn from(pieces: RegexWord) -> Self {
        Self {
            pieces,
            case_insensitive: false,
            multiline: false,
        }
    }
}

impl Regex {
    /// Sets the regular expression's case sensitivity.
    ///
    /// # Arguments
    ///
    /// * `value` - The new case sensitivity value.
    pub const fn set_case_insensitive(mut self, value: bool) -> Self {
        self.case_insensitive = value;
        self
    }

    /// Enables (or disables) multiline support for this pattern: `.` then also matches
    /// newline characters. `^` and `$` always anchor to the whole string, as in POSIX
    /// regular expressions, which have no line anchors.
    ///
    /// # Arguments
    ///
    /// * `value` - The new multiline value.
    pub const fn set_multiline(mut self, value: bool) -> Self {
        self.multiline = value;
        self
    }

    /// Computes if the regular expression matches the given string.
    ///
    /// # Arguments
    ///
    /// * `value` - The string to check for a match.
    pub fn matches(&self, value: &str) -> Result<Option<Vec<Option<String>>>, error::Error> {
        let regex_pattern: String = self
            .pieces
            .iter()
            .map(|piece| piece.to_regex_str())
            .collect();

        // A pattern bash's regex library refuses is refused here too, whether or not this engine
        // would take it. That library reads the bytes the pattern stands for, in UTF-8.
        let invalid = |reason| {
            error::Error::from(error::ErrorKind::InvalidRegex(
                regex_pattern.clone(),
                reason,
            ))
        };
        if let Some(reason) = invalid_regex_reason(&rawbytes::encode(&regex_pattern)) {
            return Err(invalid(reason));
        }
        let Ok(pattern) = String::from_utf8(rawbytes::encode(&regex_pattern).into_owned()) else {
            return Err(invalid(INVALID_REGEXP));
        };
        let back_references = has_back_reference(&pattern);
        let re = compile_regex(pattern, self.case_insensitive, self.multiline)
            .map_err(|_error| invalid("Invalid regular expression"))?;

        if back_references {
            // musl matches a back-reference by backtracking, which reads a byte that is no
            // character as it reads any other.
            return Ok(re
                .captures(value)?
                .map(|captures| shell_values(captures.iter().map(|c| c.map(|m| m.as_str())))));
        }
        // musl's other matcher reads the value one character at a time, one past the character
        // it matches, until no match can go further; reaching a byte that is not UTF-8 ends it
        // with no match. So a match lies before the first such byte, and ends at least two
        // characters before it.
        let bytes = rawbytes::encode(value);
        let text = bytes.utf8_chunks().next().map_or("", |chunk| chunk.valid());
        let reaches_cut = |end: usize| {
            text.len() < bytes.len()
                && text
                    .get(end..)
                    .is_none_or(|rest| rest.chars().nth(1).is_none())
        };
        Ok(re
            .captures(text)?
            .filter(|captures| !captures.get(0).is_some_and(|m| reaches_cut(m.end())))
            .map(|captures| shell_values(captures.iter().map(|c| c.map(|m| m.as_str())))))
    }
}

/// Captured text as shell strings (see `rawbytes`).
fn shell_values<'a>(captures: impl Iterator<Item = Option<&'a str>>) -> Vec<Option<String>> {
    captures
        .map(|c| c.map(|text| rawbytes::decode(text.as_bytes()).into_owned()))
        .collect()
}

/// musl's reason for a pattern that holds a byte that is not UTF-8.
const INVALID_REGEXP: &str = "Invalid regexp";

/// Whether `pattern` refers back to a group (`\1`).
fn has_back_reference(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.next().is_some_and(|c| matches!(c, '1'..='9')) {
            return true;
        }
    }
    false
}

/// Why `pattern` is not a valid POSIX extended regular expression, as the C library bash uses
/// (musl) words it, or `None` when this check finds nothing wrong.
fn invalid_regex_reason(pattern: &[u8]) -> Option<&'static str> {
    const CLASSES: [&str; 12] = [
        "alnum", "alpha", "blank", "cntrl", "digit", "graph", "lower", "print", "punct", "space",
        "upper", "xdigit",
    ];
    // The pattern's characters, `None` for a byte that is not UTF-8. musl refuses such a byte
    // where it reads one as a character; inside a bound or a class name it reads bytes, so there
    // it is just not a digit, or not part of any class's name.
    let mut chars = pattern
        .utf8_chunks()
        .flat_map(|chunk| {
            chunk
                .valid()
                .chars()
                .map(Some)
                .chain(chunk.invalid().iter().map(|_| None))
        })
        .peekable();
    let text = |chars: Vec<Option<char>>| -> String {
        chars
            .into_iter()
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect()
    };
    let mut depth = 0usize;
    let mut first = true;
    while let Some(c) = chars.next() {
        let Some(c) = c else {
            return Some(INVALID_REGEXP);
        };
        match c {
            '\\' => match chars.next() {
                None => return Some("Trailing backslash"),
                Some(None) => return Some(INVALID_REGEXP),
                Some(Some(_)) => (),
            },
            '*' | '+' | '?' if first => {
                return Some("Repetition not preceded by valid expression");
            }
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '{' => {
                // `{m}`, `{m,}` or `{m,n}`.
                let bound = text(chars.by_ref().take_while(|c| *c != Some('}')).collect());
                let (low, high) = bound.split_once(',').unwrap_or((&bound, "0"));
                let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
                if !digits(low) || !(high.is_empty() || digits(high)) {
                    return Some("Invalid contents of {}");
                }
            }
            '[' => {
                // A `]` right after `[` or `[^` is part of the set.
                chars.next_if_eq(&Some('^'));
                chars.next_if_eq(&Some(']'));
                loop {
                    match chars.next() {
                        None => return Some("Missing ']'"),
                        Some(None) => return Some(INVALID_REGEXP),
                        Some(Some(']')) => break,
                        Some(Some('[')) if chars.next_if_eq(&Some(':')).is_some() => {
                            let class =
                                text(chars.by_ref().take_while(|c| *c != Some(':')).collect());
                            if !CLASSES.contains(&class.as_str()) {
                                return Some("Unknown character class name");
                            }
                            chars.next_if_eq(&Some(']'));
                        }
                        Some(Some(_)) => (),
                    }
                }
            }
            _ => (),
        }
        first = false;
    }
    (depth > 0).then_some("Missing ')'")
}

pub(crate) fn compile_regex(
    regex_str: String,
    case_insensitive: bool,
    multiline: bool,
) -> Result<fancy_regex::Regex, error::Error> {
    // Move regex_str into the key to avoid cloning on cache-hit path.
    let key = (regex_str, case_insensitive, multiline);

    let cached_regex = REGEX_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .as_mut()
            .and_then(|c| c.cache_get(&key).cloned())
    });
    if let Some(re) = cached_regex {
        return Ok(re);
    }

    // Handle identified cases where a shell-supported regex isn't supported directly by
    // `fancy_regex` -- specifically, adding missing escape characters.
    let mut regex_str = add_missing_escape_chars_to_regex(key.0.as_str());

    // Handle multiline enablement.
    if multiline {
        // The fancy_regex crate internally seems to have flags that can be used
        // to enable multiline support, but they're not exposed via its
        // RegexBuilder. We instead just prefix with the right flags.
        let updated_str = std::format!("(?s){regex_str}");
        regex_str = updated_str.into();
    }

    let mut builder = fancy_regex::RegexBuilder::new(regex_str.as_ref());
    builder.case_insensitive(case_insensitive);

    let re = match builder.build() {
        Ok(re) => re,
        Err(e) => return Err(error::ErrorKind::InvalidRegexError(e, regex_str.to_string()).into()),
    };

    // Release borrow on key.0 before moving key into cache_set.
    drop(regex_str);

    REGEX_CACHE.with(|cache| {
        if let Some(c) = cache.borrow_mut().as_mut() {
            c.cache_set(key, re.clone());
        }
    });

    Ok(re)
}

fn add_missing_escape_chars_to_regex(s: &str) -> Cow<'_, str> {
    // We may see a character class with an unescaped '[' (open bracket) character. We need
    // to escape that character.
    let mut in_escape = false;
    let mut in_brackets = false;
    // A ']' in the first member position of a bracket expression (immediately after the
    // opening '[' or after the leading '^' that negates it) is a member of the
    // expression, not its terminator.
    let mut at_first_member = false;
    let mut insertion_positions = vec![];
    // Character classes (`[:alpha:]`), each with its text's byte range and the members that
    // replace it.
    let mut classes = vec![];

    let mut peekable = s.char_indices().peekable();
    while let Some((byte_offset, c)) = peekable.next() {
        let next_is_colon = peekable.peek().is_some_and(|(_, c)| *c == ':');
        let was_at_first_member = at_first_member;
        at_first_member = false;

        match c {
            '[' if !in_escape && in_brackets && next_is_colon => {
                // A class, read whole so its `]` does not end the expression.
                if let Some(end) = s
                    .get(byte_offset..)
                    .and_then(|rest| rest.find(":]"))
                    .map(|at| byte_offset + at + 2)
                    && let Some(members) = s
                        .get(byte_offset + 2..end - 2)
                        .and_then(posix_class_members)
                {
                    classes.push((byte_offset, end, members));
                    while peekable.next_if(|(at, _)| *at < end).is_some() {}
                }
            }
            '[' if !in_escape && !in_brackets => {
                in_brackets = true;
                // A '^' here negates the expression; it isn't a member itself, so the
                // first member position is the one after it. Any later '^' is an
                // ordinary member.
                let _ = peekable.next_if(|(_, c)| *c == '^');
                at_first_member = true;
            }
            '[' if !in_escape && in_brackets && !next_is_colon => {
                // Need to escape.
                insertion_positions.push(byte_offset);
            }
            ']' if !in_escape && in_brackets && was_at_first_member => {
                // `fancy_regex` doesn't implement the rule that this ']' is a member;
                // escape it so it's a member there too, and so it can be the low end
                // of a range (e.g. `[]-a]`).
                insertion_positions.push(byte_offset);
            }
            ']' if !in_escape && in_brackets => {
                in_brackets = false;
            }
            _ => (),
        }

        in_escape = !in_escape && c == '\\';
    }

    if insertion_positions.is_empty() && classes.is_empty() {
        return s.into();
    }

    let mut edits: Vec<(usize, usize, &str)> = insertion_positions
        .iter()
        .map(|pos| (*pos, *pos, "\\"))
        .chain(
            classes
                .iter()
                .map(|(start, end, members)| (*start, *end, *members)),
        )
        .collect();
    edits.sort_by_key(|(start, _, _)| *start);
    let mut updated = s.to_owned();
    for (start, end, text) in edits.iter().rev() {
        updated.replace_range(*start..*end, text);
    }

    updated.into()
}

/// The members of a bracket expression that make up the POSIX character class `name` in the
/// C.UTF-8 locale bash runs in (musl's `iswalpha` and the rest): letters of every script, but
/// only ASCII digits. `None` when there is no such class.
pub(crate) fn posix_class_members(name: &str) -> Option<&'static str> {
    // musl's `iswspace`, and the characters neither printable nor graphic.
    macro_rules! space {
        () => {
            r"\t\n\x0B\x0C\r \x{85}\x{2000}-\x{2006}\x{2008}-\x{200A}\x{2028}\x{2029}\x{205F}\x{3000}"
        };
    }
    macro_rules! control {
        () => {
            r"\p{Cc}\x{2028}\x{2029}"
        };
    }
    Some(match name {
        "alpha" => r"\p{Alphabetic}",
        "alnum" => r"\p{Alphabetic}0-9",
        "upper" => r"\p{Uppercase}",
        "lower" => r"\p{Lowercase}",
        "digit" => "0-9",
        "xdigit" => "0-9A-Fa-f",
        "blank" => r" \t",
        "space" => space!(),
        "cntrl" => control!(),
        "print" => concat!("[^", control!(), "]"),
        "graph" => concat!("[^", control!(), space!(), "]"),
        "punct" => concat!(r"[^\p{Alphabetic}0-9", control!(), space!(), "]"),
        _ => return None,
    })
}

fn escape_literal_regex_piece(s: &str) -> Cow<'_, str> {
    let mut result = String::new();

    for c in s.chars() {
        match c {
            c if regex_char_is_special(c) => {
                result.push('\\');
                result.push(c);
            }
            c => result.push(c),
        }
    }

    result.into()
}

pub(crate) const fn regex_char_is_special(c: char) -> bool {
    matches!(
        c,
        '\\' | '^' | '$' | '.' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn character_classes_are_unicode() {
        let matches = |pattern: &str, value: &str, case_insensitive: bool| {
            compile_regex(pattern.to_owned(), case_insensitive, false)
                .unwrap()
                .is_match(value)
                .unwrap()
        };
        assert!(matches("^[[:alpha:]]$", "é", false));
        assert!(matches("^[[:upper:]]$", "É", false));
        assert!(!matches("^[[:alpha:]]$", "1", false));
        assert!(matches("^[[:punct:]]$", "€", false));
        assert!(matches("^[[:punct:]]$", "\u{a0}", false));
        assert!(!matches("^[[:space:]]$", "\u{a0}", false));
        assert!(matches("^[^[:space:][:digit:]]$", "x", false));
        assert!(!matches("^[^[:space:][:digit:]]$", "7", false));
        // A class in a pattern keeps its case where case does not otherwise count.
        assert!(!matches("^(?-i:[[:upper:]])$", "q", true));
        assert!(matches("^[[:upper:]]$", "q", true));
    }

    #[test]
    fn a_byte_that_is_no_character_ends_the_match_where_musl_reads_it() {
        let matched = |pattern: &str, bytes: &[u8]| {
            Regex::from(vec![RegexPiece::Pattern(pattern.to_owned())])
                .set_multiline(true)
                .matches(&rawbytes::decode(bytes))
                .unwrap()
                .map(|captures| captures[0].clone().unwrap_or_default())
        };
        assert_eq!(matched("^a.b$", b"a\xffb"), None);
        assert_eq!(matched("[^a]", b"a\xffb"), None);
        assert_eq!(matched("ab", b"\xffab"), None);
        assert_eq!(matched("a.*", b"abc\xff"), None);
        // The matcher reads one character past the match.
        assert_eq!(matched("a", b"ab\xff"), None);
        assert_eq!(matched("a", b"abc\xff"), Some("a".to_owned()));
        assert_eq!(matched("^$", b"\xff"), None);
        // A back-reference is matched by backtracking, which reads such a byte as any other.
        assert_eq!(matched(r"(a)\1", b"\xffaa"), Some("aa".to_owned()));
        // Valid UTF-8 for a character in the range that stands for bytes is that character.
        let private = "\u{10FF80}".as_bytes();
        assert_eq!(
            matched("^.$", private),
            Some(rawbytes::decode(private).into_owned())
        );
    }

    #[test]
    fn a_pattern_byte_that_is_no_character_is_an_invalid_regexp() {
        assert_eq!(invalid_regex_reason(b"a\xffb"), Some(INVALID_REGEXP));
        assert_eq!(invalid_regex_reason(b"[\xff]"), Some(INVALID_REGEXP));
        assert_eq!(invalid_regex_reason(b"\\\xff"), Some(INVALID_REGEXP));
        assert_eq!(invalid_regex_reason(b"(\xff"), Some(INVALID_REGEXP));
        // An error found before it, or where musl reads bytes, keeps its own words.
        assert_eq!(
            invalid_regex_reason(b"*\xff"),
            Some("Repetition not preceded by valid expression")
        );
        assert_eq!(
            invalid_regex_reason(b"a{x}\xff"),
            Some("Invalid contents of {}")
        );
        assert_eq!(
            invalid_regex_reason(b"[[:al\xff:]]"),
            Some("Unknown character class name")
        );
        assert_eq!(invalid_regex_reason("^\u{10FF80}$".as_bytes()), None);
    }

    #[test]
    fn test_add_missing_escape_chars_to_regex() {
        // Negative cases -- where we don't need to escape.
        assert_eq!(add_missing_escape_chars_to_regex("a[b]"), "a[b]");
        assert_eq!(add_missing_escape_chars_to_regex(r"a\[b\]"), r"a\[b\]");
        assert_eq!(add_missing_escape_chars_to_regex(r"a[b\[]"), r"a[b\[]");

        // Positive case -- where we need to escape.
        assert_eq!(add_missing_escape_chars_to_regex(r"a[b[]"), r"a[b\[]");
        assert_eq!(add_missing_escape_chars_to_regex(r"a[[]"), r"a[\[]");
    }

    #[test]
    fn test_leading_close_bracket_in_bracket_expression() {
        // A ']' in the first member position (after any '^') is a member of the bracket
        // expression, not its terminator, so the expression is still open after it.
        // It's escaped as well, since `fancy_regex` doesn't implement that rule; that
        // also makes it the low end of a range in `[]-a]`, as it is in a shell pattern.
        assert_eq!(add_missing_escape_chars_to_regex("[]]"), r"[\]]");
        assert_eq!(add_missing_escape_chars_to_regex("[^]]"), r"[^\]]");
        assert_eq!(add_missing_escape_chars_to_regex("[][]"), r"[\]\[]");
        assert_eq!(add_missing_escape_chars_to_regex("[^][]"), r"[^\]\[]");
        assert_eq!(add_missing_escape_chars_to_regex("[]a[]"), r"[\]a\[]");
        assert_eq!(add_missing_escape_chars_to_regex("[]-a]"), r"[\]-a]");

        // The rule only applies in the first member position: the ']' in `[a]` and the
        // already-escaped one in `[\]]` both close their expressions.
        assert_eq!(add_missing_escape_chars_to_regex("[a][]x]"), r"[a][\]x]");
        assert_eq!(add_missing_escape_chars_to_regex(r"[\]][]x]"), r"[\]][\]x]");

        // Only the '^' that negates the expression is skipped over; a later one is an
        // ordinary member, so the ']' after it terminates the expression.
        assert_eq!(add_missing_escape_chars_to_regex("[^^][ab]"), "[^^][ab]");
        assert_eq!(add_missing_escape_chars_to_regex("[^^][b[]"), r"[^^][b\[]");

        // A '^' outside a bracket expression doesn't open one.
        assert_eq!(add_missing_escape_chars_to_regex("^[a[]"), r"^[a\[]");
    }
}
