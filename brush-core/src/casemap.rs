//! Case conversion one character at a time, as bash converts case.
//!
//! Bash maps each character to one character with the C library's `towupper` and `towlower`
//! (Unicode's simple case mappings), so `ß` becomes `ẞ` rather than `SS`, `ﬁ` stays as it is,
//! and a string keeps its length. Rust's `char::to_uppercase` and `str::to_uppercase` apply
//! the full mappings instead, which can turn one character into several.

/// The upper-case form of `c`, one character for one, as `towupper` gives it.
///
/// # Arguments
///
/// * `c` - The character to convert.
pub fn to_upper(c: char) -> char {
    let mut upper = c.to_uppercase();
    if let (Some(single), None) = (upper.next(), upper.next()) {
        return single;
    }
    // The characters whose full mapping is several characters but whose simple one is one.
    match c {
        'ß' => 'ẞ',
        '\u{1F80}'..='\u{1F87}' | '\u{1F90}'..='\u{1F97}' | '\u{1FA0}'..='\u{1FA7}' => {
            char::from_u32(u32::from(c) + 8).unwrap_or(c)
        }
        '\u{1FB3}' => '\u{1FBC}',
        '\u{1FC3}' => '\u{1FCC}',
        '\u{1FF3}' => '\u{1FFC}',
        _ => c,
    }
}

/// The lower-case form of `c`, one character for one, as `towlower` gives it.
///
/// # Arguments
///
/// * `c` - The character to convert.
pub fn to_lower(c: char) -> char {
    let mut lower = c.to_lowercase();
    if let (Some(single), None) = (lower.next(), lower.next()) {
        return single;
    }
    match c {
        'İ' => 'i',
        _ => c,
    }
}

/// `c` with its case toggled: an upper-case (or title-case) character becomes lower case,
/// and a lower-case one upper case.
///
/// # Arguments
///
/// * `c` - The character to convert.
pub fn toggle(c: char) -> char {
    let lower = to_lower(c);
    if lower == c { to_upper(c) } else { lower }
}

/// `s` in upper case, character by character.
///
/// # Arguments
///
/// * `s` - The string to convert.
pub fn upper(s: &str) -> String {
    s.chars().map(to_upper).collect()
}

/// `s` in lower case, character by character.
///
/// # Arguments
///
/// * `s` - The string to convert.
pub fn lower(s: &str) -> String {
    s.chars().map(to_lower).collect()
}

/// `s` with each character's case toggled.
///
/// # Arguments
///
/// * `s` - The string to convert.
pub fn toggled(s: &str) -> String {
    s.chars().map(toggle).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_character_for_one() {
        assert_eq!(upper("straße ﬁx ŉ İi ǅ σς ı ᾳ"), "STRAẞE ﬁX ŉ İI Ǆ ΣΣ I ᾼ");
        assert_eq!(lower("STRAẞE İ ǅ ΣΑΣ"), "straße i ǆ σασ");
        assert_eq!(toggled("İi ǅ ẞ σς ı"), "iI ǆ ß ΣΣ I");
    }
}
