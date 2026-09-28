//! Shell strings that hold bytes that are not UTF-8.
//!
//! Bash's strings are byte strings: `$'\xff'`, a command substitution, `read` or `mapfile` can
//! produce a value holding bytes that are not UTF-8, and writing the value back out reproduces
//! them. The shell's values are Rust strings, so each such byte is stored as a character of its
//! own in a private-use range (U+10FF80 to U+10FFFF), and [`encode`] turns it back into the byte
//! wherever the value leaves the shell as bytes: output, here-documents, and the arguments given to
//! commands. Such a byte counts as one character, as it does in bash.
//!
//! Valid UTF-8 for a character in that range is stored byte by byte as well, so [`decode`]
//! followed by [`encode`] reproduces any input exactly.

use std::borrow::Cow;

/// The characters standing for bytes 0x80 to 0xFF are this plus the byte.
const BASE: u32 = 0x10_FF00;

/// Every character standing for a byte is encoded in UTF-8 starting with this byte.
const LEAD_BYTE: u8 = 0xF4;

/// Returns the character that stands for `byte` when it is not part of valid UTF-8 (an ASCII
/// byte stands for itself).
///
/// # Arguments
///
/// * `byte` - The byte.
pub fn byte_char(byte: u8) -> char {
    if byte.is_ascii() {
        return char::from(byte);
    }
    char::from_u32(BASE + u32::from(byte)).unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// Returns the byte `c` stands for, if it is one of the characters [`decode`] uses for bytes.
///
/// # Arguments
///
/// * `c` - The character to check.
pub const fn char_byte(c: char) -> Option<u8> {
    let value = c as u32;
    if value >= BASE + 0x80 && value <= BASE + 0xFF {
        #[expect(clippy::cast_possible_truncation)]
        Some((value - BASE) as u8)
    } else {
        None
    }
}

/// Converts bytes to a shell string, keeping each byte that is not part of valid UTF-8.
///
/// # Arguments
///
/// * `bytes` - The bytes to convert.
pub fn decode(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(text) if !bytes.contains(&LEAD_BYTE) => Cow::Borrowed(text),
        _ => Cow::Owned(decode_escaping(bytes)),
    }
}

/// Converts bytes to a shell string, as [`decode`] does, reusing the buffer when it can.
///
/// # Arguments
///
/// * `bytes` - The bytes to convert.
pub fn decode_vec(bytes: Vec<u8>) -> String {
    if bytes.contains(&LEAD_BYTE) {
        return decode_escaping(&bytes);
    }
    String::from_utf8(bytes).unwrap_or_else(|error| decode_escaping(error.as_bytes()))
}

fn decode_escaping(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if char_byte(c).is_some() {
                let mut buffer = [0; 4];
                text.extend(c.encode_utf8(&mut buffer).bytes().map(byte_char));
            } else {
                text.push(c);
            }
        }
        text.extend(chunk.invalid().iter().copied().map(byte_char));
    }
    text
}

/// Converts a shell string to the bytes it stands for.
///
/// # Arguments
///
/// * `text` - The string to convert.
pub fn encode(text: &str) -> Cow<'_, [u8]> {
    if !text.as_bytes().contains(&LEAD_BYTE) {
        return Cow::Borrowed(text.as_bytes());
    }
    let mut bytes = Vec::with_capacity(text.len());
    for c in text.chars() {
        if let Some(byte) = char_byte(c) {
            bytes.push(byte);
        } else {
            let mut buffer = [0; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
        }
    }
    Cow::Owned(bytes)
}

/// Appends the bytes a single character of a shell string stands for.
///
/// # Arguments
///
/// * `bytes` - The buffer to append to.
/// * `c` - The character.
pub fn push_char(bytes: &mut Vec<u8>, c: char) {
    if let Some(byte) = char_byte(c) {
        bytes.push(byte);
    } else {
        let mut buffer = [0; 4];
        bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
    }
}

/// Converts a shell string to an OS string holding the bytes it stands for, for a command's
/// arguments.
///
/// # Arguments
///
/// * `text` - The string to convert.
pub fn to_os_string(text: &str) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(encode(text).into_owned())
    }
    #[cfg(target_os = "wasi")]
    {
        // `std::os::wasi::ffi` is not stable on every WASI target.
        let bytes = encode(text).into_owned();
        // SAFETY: WASI's `OsStr` is a plain byte sequence, as Unix's is (it is not WTF-8), so
        // every byte sequence is a valid encoding.
        unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(bytes) }
    }
    #[cfg(not(any(unix, target_os = "wasi")))]
    {
        std::ffi::OsString::from(String::from_utf8_lossy(&encode(text)).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_utf8_is_unchanged() {
        assert!(matches!(decode("héllo".as_bytes()), Cow::Borrowed("héllo")));
        assert!(matches!(encode("héllo"), Cow::Borrowed(b) if b == "héllo".as_bytes()));
    }

    #[test]
    fn invalid_bytes_round_trip() {
        for input in [
            &b"a\xffb"[..],
            b"caf\xe9",
            b"\xc3",
            b"\xed\xa0\x80",
            b"\xf4\x90\x80\x80",
            b"\x80\x80\xbf",
            // Valid UTF-8 for the characters that stand for bytes.
            "x\u{10FF80}\u{10FFFF}y".as_bytes(),
        ] {
            let text = decode(input);
            assert_eq!(encode(&text).as_ref(), input, "round trip of {input:?}");
            assert_eq!(decode_vec(input.to_vec()), text);
        }
    }

    #[test]
    fn each_invalid_byte_is_one_character() {
        assert_eq!(decode(b"a\xffb").chars().count(), 3);
        assert_eq!(decode(b"caf\xe9").chars().count(), 4);
        assert_eq!(decode("\u{10FF80}".as_bytes()).chars().count(), 4);
    }

    #[test]
    fn push_char_matches_encode() {
        let text = decode(b"\xfe\xc3\xa9z");
        let mut bytes = Vec::new();
        for c in text.chars() {
            push_char(&mut bytes, c);
        }
        assert_eq!(bytes, b"\xfe\xc3\xa9z");
    }
}
