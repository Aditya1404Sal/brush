//! String escaping utilities

use std::borrow::Cow;

use itertools::Itertools;

use crate::{error, int_utils};

/// Escape expansion mode.
#[derive(Clone, Copy)]
pub enum EscapeExpansionMode {
    /// echo builtin mode.
    EchoBuiltin,
    /// ANSI-C quotes.
    AnsiCQuotes,
}

/// Expands backslash escapes in the provided string.
///
/// # Arguments
///
/// * `s` - The string to expand.
/// * `mode` - The mode to use for expansion.
#[expect(clippy::too_many_lines)]
pub fn expand_backslash_escapes(
    s: &str,
    mode: EscapeExpansionMode,
) -> Result<(Vec<u8>, bool), error::Error> {
    let mut result: Vec<u8> = Vec::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            // Not a backslash, add and move on (as the byte it stands for, if it stands for one;
            // see `rawbytes`).
            crate::rawbytes::push_char(&mut result, c);
            continue;
        }

        let Some(escape_cmd) = it.next() else {
            // Trailing backslash.
            result.push(b'\\');
            continue;
        };

        match escape_cmd {
            'a' => result.push(b'\x07'),
            'b' => result.push(b'\x08'),
            'c' => {
                match mode {
                    EscapeExpansionMode::EchoBuiltin => {
                        // Stop all additional output!
                        return Ok((result, false));
                    }
                    EscapeExpansionMode::AnsiCQuotes => {
                        if let Some(char_value) = it.next() {
                            // Special case backslash. If it's immediately followed by another
                            // backslash, then we consume both; if not, we still will use the
                            // backslash character as the one to apply the control transformation
                            // to.
                            if char_value == '\\' {
                                let orig_it = it.clone();
                                if !matches!(it.next(), Some('\\')) {
                                    // Didn't find another backslash; restore iterator.
                                    it = orig_it;
                                }
                            }

                            let mut bytes: Vec<u8> = if char_value.is_ascii_lowercase() {
                                char_value
                                    .to_ascii_uppercase()
                                    .to_string()
                                    .bytes()
                                    .collect()
                            } else {
                                char_value.to_string().bytes().collect()
                            };

                            if !bytes.is_empty() {
                                if bytes[0] == b'?' {
                                    // We can't explain why this is the case, but it is.
                                    bytes[0] = 0x7f;
                                } else {
                                    bytes[0] &= 0x1f;
                                }
                            }

                            result.append(bytes.as_mut());
                        } else {
                            result.push(b'\\');
                            result.push(b'c');
                        }
                    }
                }
            }
            'e' | 'E' => result.push(b'\x1b'),
            'f' => result.push(b'\x0c'),
            'n' => result.push(b'\n'),
            'r' => result.push(b'\r'),
            't' => result.push(b'\t'),
            'v' => result.push(b'\x0b'),
            '\\' => result.push(b'\\'),
            '\'' if matches!(mode, EscapeExpansionMode::AnsiCQuotes) => result.push(b'\''),
            '\"' if matches!(mode, EscapeExpansionMode::AnsiCQuotes) => result.push(b'\"'),
            '?' if matches!(mode, EscapeExpansionMode::AnsiCQuotes) => result.push(b'?'),
            '0' => {
                // Consume 0-3 valid octal chars
                let mut taken_so_far = 0;
                let mut octal_chars: String = it
                    .take_while_ref(|c| {
                        if taken_so_far < 3 && matches!(*c, '0'..='7') {
                            taken_so_far += 1;
                            true
                        } else {
                            false
                        }
                    })
                    .collect();

                if octal_chars.is_empty() {
                    octal_chars.push('0');
                }

                // Bash keeps the low byte of a value above \377.
                let value = int_utils::parse::<u32>(octal_chars.as_str(), 8)?;
                result.push(low_byte(value));
            }
            'x' => {
                // Consume 1-2 valid hex chars (or unlimited with braces in ANSI-C mode)
                let mut hex_chars = String::new();
                let mut invalid_prefix = false;
                let mut hexits_consumed = 0;
                let mut start_brace_consumed = false;

                loop {
                    // Save the original in case we go too far and need to restore.
                    let orig_it = it.clone();

                    let Some(next_c) = it.next() else {
                        break;
                    };

                    if matches!(mode, EscapeExpansionMode::AnsiCQuotes)
                        && !start_brace_consumed
                        && next_c == '{'
                    {
                        start_brace_consumed = true;
                    } else if start_brace_consumed && next_c == '}' {
                        break;
                    } else if ((start_brace_consumed && !invalid_prefix)
                        || (!start_brace_consumed && hexits_consumed < 2))
                        && next_c.is_ascii_hexdigit()
                    {
                        hex_chars.push(next_c);
                        hexits_consumed += 1;
                    } else if start_brace_consumed && hexits_consumed == 0 {
                        invalid_prefix = true;
                    } else {
                        // Went too far; restore iterator and break.
                        it = orig_it;
                        break;
                    }
                }

                if hex_chars.is_empty() {
                    if start_brace_consumed {
                        result.push(0);
                    } else {
                        result.push(b'\\');
                        result.append(escape_cmd.to_string().into_bytes().as_mut());
                    }
                } else {
                    let value32 = int_utils::parse::<u32>(hex_chars.as_str(), 16)?;
                    let value8: u8 = (value32 & 0xFF) as u8;
                    result.push(value8);
                }
            }
            'u' => {
                // Consume 1-4 hex digits
                let mut taken_so_far = 0;
                let hex_chars: String = it
                    .take_while_ref(|next_c| {
                        if taken_so_far < 4 && next_c.is_ascii_hexdigit() {
                            taken_so_far += 1;
                            true
                        } else {
                            false
                        }
                    })
                    .collect();

                if hex_chars.is_empty() {
                    result.push(b'\\');
                    result.append(escape_cmd.to_string().into_bytes().as_mut());
                } else {
                    let value = int_utils::parse::<u16>(hex_chars.as_str(), 16)?;
                    push_code_point(&mut result, u32::from(value));
                }
            }
            'U' => {
                // Consume 1-8 hex digits
                let mut taken_so_far = 0;
                let hex_chars: String = it
                    .take_while_ref(|next_c| {
                        if taken_so_far < 8 && next_c.is_ascii_hexdigit() {
                            taken_so_far += 1;
                            true
                        } else {
                            false
                        }
                    })
                    .collect();

                if hex_chars.is_empty() {
                    result.push(b'\\');
                    result.append(escape_cmd.to_string().into_bytes().as_mut());
                } else {
                    let value = int_utils::parse::<u32>(hex_chars.as_str(), 16)?;
                    push_code_point(&mut result, value);
                }
            }
            first_octal @ '1'..='7' if matches!(mode, EscapeExpansionMode::AnsiCQuotes) => {
                // We've already consumed the first octal digit.
                let mut octal_chars = String::new();
                octal_chars.push(first_octal);

                // Consume up to 2 more valid octal chars
                let mut taken_so_far = 1;
                for next_c in it.take_while_ref(|next_c| {
                    if taken_so_far < 3 && matches!(next_c, '0'..='7') {
                        taken_so_far += 1;
                        true
                    } else {
                        false
                    }
                }) {
                    octal_chars.push(next_c);
                }

                // Bash keeps the low byte of a value above \377.
                let value = int_utils::parse::<u32>(octal_chars.as_str(), 8)?;
                result.push(low_byte(value));
            }
            unknown => {
                // Not a valid escape sequence.
                result.push(b'\\');
                result.append(unknown.to_string().into_bytes().as_mut());
            }
        }
    }

    // In ANSI-C quotes, we crop the result at the first NUL.
    if matches!(mode, EscapeExpansionMode::AnsiCQuotes) {
        if let Some(nul_index) = result.iter().position(|&b| b == 0) {
            result.truncate(nul_index);
        }
    }

    Ok((result, true))
}

/// The low byte of an octal escape's value (`\777` is 0xff), as bash takes it.
const fn low_byte(value: u32) -> u8 {
    value.to_le_bytes()[0]
}

/// Appends the UTF-8 encoding of a `\u` or `\U` escape's code point. Like bash, a code point
/// that is not a character (a surrogate, or above U+10FFFF) gets the same encoding scheme's bytes,
/// up to six of them, and one above 0x7FFFFFFF gets nothing.
fn push_code_point(result: &mut Vec<u8>, value: u32) {
    if let Some(c) = char::from_u32(value) {
        let mut buffer = [0; 4];
        result.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
        return;
    }
    let (lead, count): (u8, u32) = match value {
        0..=0xFFFF => (0xE0, 2),
        0x1_0000..=0x1F_FFFF => (0xF0, 3),
        0x20_0000..=0x3FF_FFFF => (0xF8, 4),
        0x400_0000..=0x7FFF_FFFF => (0xFC, 5),
        _ => return,
    };
    result.push(lead | low_byte(value >> (6 * count)));
    for shift in (0..count).rev() {
        result.push(0x80 | (low_byte(value >> (6 * shift)) & 0x3F));
    }
}

/// Quoting mode to use for escaping.
#[derive(Clone, Copy, Default)]
pub enum QuoteMode {
    /// Single-quote.
    #[default]
    SingleQuote,
    /// Double-quote.
    DoubleQuote,
    /// Backslash-escape.
    BackslashEscape,
}

/// Options influencing how to escape/quote an input string.
#[derive(Default)]
pub(crate) struct QuoteOptions {
    /// Whether or not to *always* escape or quote the input; if false, then escaping/quoting
    /// will only be applied if the input contains characters that *require* it.
    pub always_quote: bool,
    /// Preferred mode for quoting/escaping. Quoting may be "upgraded" to a more expressive
    /// format if the input is not expressible otherwise.
    pub preferred_mode: QuoteMode,
    /// Whether or not to *avoid* using ANSI C quoting just for the benefit of newline characters.
    /// Default is for newline characters to require upgrading the string's quoting to
    /// ANSI C quoting.
    pub avoid_ansi_c_quoting_newline: bool,
}

pub(crate) fn quote<'a>(s: &'a str, options: &QuoteOptions) -> Cow<'a, str> {
    let use_ansi_c_quotes = s.contains(|c| {
        needs_ansi_c_quoting(c) && (!options.avoid_ansi_c_quoting_newline || c != '\n')
    });

    if use_ansi_c_quotes {
        return ansi_c_quote(s).into();
    }

    let use_default_quotes =
        !use_ansi_c_quotes && (options.always_quote || s.is_empty() || s.contains(needs_escaping));

    if !use_default_quotes {
        return s.into();
    }

    match options.preferred_mode {
        QuoteMode::BackslashEscape => backslash_escape(s),
        QuoteMode::SingleQuote => single_quote(s),
        QuoteMode::DoubleQuote => double_quote(s).into(),
    }
}

/// Escape the given string, forcing quoting.
///
/// # Arguments
///
/// * `s` - The string to escape.
/// * `mode` - The quoting mode to use.
pub fn force_quote(s: &str, mode: QuoteMode) -> String {
    let options = QuoteOptions {
        always_quote: true,
        preferred_mode: mode,
        ..Default::default()
    };

    quote(s, &options).to_string()
}

/// Applies the given quoting mode to the provided string, only changing it if required.
///
/// # Arguments
///
/// * `s` - The string to escape.
/// * `mode` - The quoting mode to use.
pub fn quote_if_needed(s: &str, mode: QuoteMode) -> Cow<'_, str> {
    let options = QuoteOptions {
        always_quote: false,
        preferred_mode: mode,
        ..Default::default()
    };

    quote(s, &options)
}

fn backslash_escape(s: &str) -> Cow<'_, str> {
    if s.is_empty() {
        // An empty string must be represented as '' to be a valid shell word.
        Cow::Owned("''".to_string())
    } else if !s.chars().any(needs_escaping) {
        Cow::Borrowed(s)
    } else {
        let mut output = String::with_capacity(s.len());
        for c in s.chars() {
            if needs_escaping(c) {
                output.push('\\');
            }
            output.push(c);
        }
        Cow::Owned(output)
    }
}

/// Plain single-quoting, as bash's `sh_single_quote` does it.
///
/// Unlike [`force_quote`] and [`quote_if_needed`], this never upgrades to ANSI-C quoting;
/// control characters are emitted literally. Use it where bash prints values verbatim,
/// such as `alias` output.
///
/// # Arguments
///
/// * `s` - The string to quote.
pub fn single_quote(s: &str) -> Cow<'_, str> {
    // Special-case the empty string and a lone single quote; both are rendered
    // without any empty quoted runs.
    match s {
        "" => return Cow::Borrowed("''"),
        "'" => return Cow::Borrowed(r"\'"),
        _ => (),
    }

    let mut result = String::with_capacity(s.len() + 2);

    // Go through the string; put everything in single quotes except for
    // the single quote character itself. It will get escaped outside
    // all quoting, with the quoted runs on either side of it kept even
    // when they're empty.
    for (i, part) in s.split('\'').enumerate() {
        if i > 0 {
            result.push_str(r"\'");
        }

        result.push('\'');
        result.push_str(part);
        result.push('\'');
    }

    Cow::Owned(result)
}

fn double_quote(s: &str) -> String {
    let mut result = String::with_capacity(s.len());

    result.push('"');

    for c in s.chars() {
        if matches!(c, '$' | '`' | '"' | '\\') {
            result.push('\\');
        }

        result.push(c);
    }

    result.push('"');

    result
}

fn ansi_c_quote(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    result.push_str("$'");

    for c in s.chars() {
        match c {
            '\x07' => result.push_str("\\a"),
            '\x08' => result.push_str("\\b"),
            '\x1b' => result.push_str("\\E"),
            '\x0c' => result.push_str("\\f"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\x0b' => result.push_str("\\v"),
            '\\' => result.push_str("\\\\"),
            '\'' => result.push_str("\\'"),
            c if needs_ansi_c_quoting(c) => {
                let byte = crate::rawbytes::char_byte(c).unwrap_or(c as u8);
                result.push_str(std::format!("\\{byte:03o}").as_str());
            }
            _ => result.push(c),
        }
    }

    result.push('\'');

    result
}

// Returns whether or not the given character needs to be escaped (or quoted) if outside
// quotes.
const fn needs_escaping(c: char) -> bool {
    matches!(
        c,
        '(' | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '$'
            | '*'
            | '?'
            | '|'
            | '&'
            | ';'
            | '<'
            | '>'
            | '`'
            | '\\'
            | '"'
            | '!'
            | '^'
            | ','
            | ' '
            | '\''
    )
}

// A byte that is not UTF-8 (see `rawbytes`) is written as its octal escape, as bash writes it.
const fn needs_ansi_c_quoting(c: char) -> bool {
    c.is_ascii_control() || crate::rawbytes::char_byte(c).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backslash_escape() {
        assert_eq!(quote_if_needed("a", QuoteMode::BackslashEscape), "a");
        assert_eq!(quote_if_needed("a b", QuoteMode::BackslashEscape), r"a\ b");
        assert_eq!(quote_if_needed("", QuoteMode::BackslashEscape), "''");
    }

    #[test]
    fn test_single_quote_escape() {
        assert_eq!(quote_if_needed("a", QuoteMode::SingleQuote), "a");
        assert_eq!(quote_if_needed("a b", QuoteMode::SingleQuote), "'a b'");
        assert_eq!(quote_if_needed("", QuoteMode::SingleQuote), "''");
        assert_eq!(quote_if_needed("'", QuoteMode::SingleQuote), r"\'");

        // The quoted run on either side of an escaped quote is kept even when it's empty;
        // only a string that is *nothing but* a single quote loses its surrounding quotes.
        assert_eq!(quote_if_needed("a'b", QuoteMode::SingleQuote), r"'a'\''b'");
        assert_eq!(
            quote_if_needed("a''b", QuoteMode::SingleQuote),
            r"'a'\'''\''b'"
        );
        assert_eq!(quote_if_needed("'a", QuoteMode::SingleQuote), r"''\''a'");
        assert_eq!(quote_if_needed("a'", QuoteMode::SingleQuote), r"'a'\'''");
        assert_eq!(quote_if_needed("''", QuoteMode::SingleQuote), r"''\'''\'''");
    }

    #[test]
    fn test_single_quote_never_upgrades() {
        // Unlike `quote`, this never reaches for ANSI-C quoting; control characters are
        // emitted literally.
        assert_eq!(single_quote("a\tb"), "'a\tb'");
        assert_eq!(single_quote("a\nb"), "'a\nb'");
        assert_eq!(single_quote("a"), "'a'");

        // ...whereas `quote` does upgrade for the same input.
        assert_eq!(quote_if_needed("a\tb", QuoteMode::SingleQuote), r"$'a\tb'");
    }

    fn assert_echo_expands_to(unexpanded: &str, expected: &str) {
        assert_eq!(
            String::from_utf8(
                expand_backslash_escapes(unexpanded, EscapeExpansionMode::EchoBuiltin)
                    .unwrap()
                    .0
            )
            .unwrap(),
            expected
        );
    }

    #[test]
    fn test_ansi_c_bytes_beyond_characters() {
        let expand = |s: &str| {
            expand_backslash_escapes(s, EscapeExpansionMode::AnsiCQuotes)
                .unwrap()
                .0
        };
        // An octal value above \377 keeps its low byte.
        assert_eq!(expand(r"\777"), [0xff]);
        assert_eq!(expand(r"a\400b"), b"a");
        // Code points that are not characters get their encoding's bytes, as in bash.
        assert_eq!(expand(r"\ud800"), [0xed, 0xa0, 0x80]);
        assert_eq!(expand(r"\U00110000"), [0xf4, 0x90, 0x80, 0x80]);
        assert_eq!(expand(r"\U00200000"), [0xf8, 0x88, 0x80, 0x80, 0x80]);
        assert_eq!(expand(r"\U7fffffff"), [0xfd, 0xbf, 0xbf, 0xbf, 0xbf, 0xbf]);
        assert_eq!(expand(r"x\UFFFFFFFFy"), b"xy");
    }

    #[test]
    fn test_echo_expansion() {
        assert_echo_expands_to("a", "a");
        assert_echo_expands_to(r"\M", "\\M");
        assert_echo_expands_to(r"a\nb", "a\nb");
        assert_echo_expands_to(r"\a", "\x07");
        assert_echo_expands_to(r"\b", "\x08");
        assert_echo_expands_to(r"\e", "\x1b");
        assert_echo_expands_to(r"\f", "\x0c");
        assert_echo_expands_to(r"\n", "\n");
        assert_echo_expands_to(r"\r", "\r");
        assert_echo_expands_to(r"\t", "\t");
        assert_echo_expands_to(r"\v", "\x0b");
        assert_echo_expands_to(r"\\", "\\");
        assert_echo_expands_to(r"\'", "\\'");
        assert_echo_expands_to(r#"\""#, r#"\""#);
        assert_echo_expands_to(r"\?", "\\?");
        assert_echo_expands_to(r"\0", "\0");
        assert_echo_expands_to(r"\00", "\0");
        assert_echo_expands_to(r"\000", "\0");
        assert_echo_expands_to(r"\081", "\081");
        assert_echo_expands_to(r"\0101", "A");
        assert_echo_expands_to(r"abc\", "abc\\");
        assert_echo_expands_to(r"\x41", "A");
        assert_echo_expands_to(r"\xf0\x9f\x90\x8d", "🐍");
        assert_echo_expands_to(r"\u2620", "☠");
        assert_echo_expands_to(r"\U0001f602", "😂");
    }
}
