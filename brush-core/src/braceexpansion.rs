use brush_parser::word;
use itertools::Itertools;

pub(crate) fn generate_and_combine_brace_expansions(
    pieces: Vec<brush_parser::word::BraceExpressionOrText>,
) -> impl IntoIterator<Item = String> {
    let expansions: Vec<Vec<String>> = pieces
        .into_iter()
        .map(|piece| expand_brace_expr_or_text(piece).collect())
        .collect();

    expansions
        .into_iter()
        .multi_cartesian_product()
        .map(|v| v.join(""))
}

fn expand_brace_expr_or_text(
    beot: word::BraceExpressionOrText,
) -> Box<dyn Iterator<Item = String>> {
    match beot {
        word::BraceExpressionOrText::Expr(members) => {
            // Chain all member iterators together
            Box::new(members.into_iter().flat_map(expand_brace_expr_member))
        }
        word::BraceExpressionOrText::Text(text) => Box::new(std::iter::once(text)),
    }
}

#[expect(clippy::cast_possible_truncation)]
fn expand_brace_expr_member(bem: word::BraceExpressionMember) -> Box<dyn Iterator<Item = String>> {
    match bem {
        word::BraceExpressionMember::NumberSequence {
            start,
            end,
            increment,
            zero_padded_width,
        } => {
            // Bash counts by the size of the increment toward `end`, and takes 0 as 1. The
            // steps are checked, so a sequence ends at either end of i64 instead of wrapping,
            // and stay in i64, since a usize is 32 bits on WASM.
            let step = increment.unsigned_abs().max(1);
            let up = start <= end;

            // A bound written with a leading zero asks for every member to be padded
            // out to the width of the longer bound, so `{01..10}` counts `01 02 ...`
            // and not `1 2 ...`. The sign takes one of those columns.
            let format = move |n: i64| match zero_padded_width {
                Some(width) => std::format!("{n:0width$}"),
                None => n.to_string(),
            };

            Box::new(
                std::iter::successors(Some(start), move |&n| {
                    let next = if up {
                        n.checked_add_unsigned(step)?
                    } else {
                        n.checked_sub_unsigned(step)?
                    };
                    let within = if up { next <= end } else { next >= end };
                    within.then_some(next)
                })
                .map(format),
            )
        }

        word::BraceExpressionMember::CharSequence {
            start,
            end,
            increment,
        } => {
            let mut increment = increment.unsigned_abs() as usize;
            if increment == 0 {
                increment = 1;
            }

            if start <= end {
                Box::new((start..=end).step_by(increment).map(sequence_char))
            } else {
                // Iterate from start down to end by decrementing.
                let increment = increment as u32;
                Box::new(
                    std::iter::successors(Some(start), move |&c| {
                        let next = char::from_u32((c as u32).checked_sub(increment)?)?;
                        (next >= end).then_some(next)
                    })
                    .map(sequence_char),
                )
            }
        }

        word::BraceExpressionMember::Child(elements) => {
            // Chain all element iterators together
            Box::new(generate_and_combine_brace_expansions(elements).into_iter())
        }
    }
}

/// A member of a letter sequence, as word text. A range from an upper-case to a lower-case letter
/// (`{Z..a}`) passes the punctuation between them; bash's quote removal takes the backslash
/// away, and keeps a backquote with nothing after it literal.
fn sequence_char(c: char) -> String {
    match c {
        '\\' => String::from("''"),
        '`' => String::from("\\`"),
        c => c.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbers(start: i64, end: i64, increment: i64) -> Vec<String> {
        let member = word::BraceExpressionMember::NumberSequence {
            start,
            end,
            increment,
            zero_padded_width: None,
        };
        // At most one more than bash gives, so a sequence that never ends fails instead of hangs.
        expand_brace_expr_member(member).take(3).collect()
    }

    #[test]
    fn number_sequences_stop_at_the_ends_of_i64() {
        assert_eq!(
            numbers(i64::MIN + 1, i64::MIN, 1),
            ["-9223372036854775807", "-9223372036854775808"]
        );
        assert_eq!(
            numbers(i64::MAX - 1, i64::MAX, 1),
            ["9223372036854775806", "9223372036854775807"]
        );
        assert_eq!(
            numbers(i64::MAX - 5, i64::MAX, 4),
            ["9223372036854775802", "9223372036854775806"]
        );
    }

    #[test]
    fn number_sequence_increments_are_not_truncated() {
        // 2^32 would truncate to 0 in a 32-bit usize, and then count by 1.
        assert_eq!(numbers(1, 3, 1 << 32), ["1"]);
        assert_eq!(numbers(3, 1, 1 << 32), ["3"]);
        assert_eq!(numbers(1, 3, 0), ["1", "2", "3"]);
    }
}
