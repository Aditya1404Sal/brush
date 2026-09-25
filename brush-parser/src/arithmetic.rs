//! Parser for shell arithmetic expressions.

use crate::ast;
use crate::error;

/// Parses a shell arithmetic expression.
///
/// # Arguments
///
/// * `input` - The arithmetic expression to parse, in string form.
pub fn parse(input: &str) -> Result<ast::ArithmeticExpr, error::WordParseError> {
    // A long expression (`1+2+…+5000`) is parsed each time rather than cached: a cached tree is
    // cloned on every use, and cloning a deep one recurses once per level.
    if input.len() > MAX_CACHED_LEN {
        return uncached_parse(input);
    }
    cacheable_parse(input)
}

/// The longest expression the parse cache keeps.
const MAX_CACHED_LEN: usize = 1024;

#[cached::macros::cached(max_size = 64, key = "String", convert = r#"{ input.to_owned() }"#)]
fn cacheable_parse(input: &str) -> Result<ast::ArithmeticExpr, error::WordParseError> {
    uncached_parse(input)
}

fn uncached_parse(input: &str) -> Result<ast::ArithmeticExpr, error::WordParseError> {
    tracing::debug!(target: "arithmetic", "parsing arithmetic expression: '{input}'");
    arithmetic::full_expression(input)
        .map_err(|e| error::WordParseError::ArithmeticExpression(e.into()))
}

peg::parser! {
    grammar arithmetic() for str {
        pub(crate) rule full_expression() -> ast::ArithmeticExpr =
            _ ![_] { ast::ArithmeticExpr::Literal(0) } /
            _ e:expression() _ { e }

        pub(crate) rule expression() -> ast::ArithmeticExpr = precedence!{
            x:(@) _ "," _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::Comma, Box::new(x), Box::new(y)) }
            --
            x:lvalue() _ "*=" _ y:(@) { ast::ArithmeticExpr::BinaryAssignment(ast::BinaryOperator::Multiply, x, Box::new(y)) }
            x:lvalue() _ "/=" _ y:(@) { ast::ArithmeticExpr::BinaryAssignment(ast::BinaryOperator::Divide, x, Box::new(y)) }
            x:lvalue() _ "%=" _ y:(@) { ast::ArithmeticExpr::BinaryAssignment(ast::BinaryOperator::Modulo, x, Box::new(y)) }
            x:lvalue() _ "+=" _ y:(@) { ast::ArithmeticExpr::BinaryAssignment(ast::BinaryOperator::Add, x, Box::new(y)) }
            x:lvalue() _ "-=" _ y:(@) { ast::ArithmeticExpr::BinaryAssignment(ast::BinaryOperator::Subtract, x, Box::new(y)) }
            x:lvalue() _ "<<=" _ y:(@) { ast::ArithmeticExpr::BinaryAssignment(ast::BinaryOperator::ShiftLeft, x, Box::new(y)) }
            x:lvalue() _ ">>=" _ y:(@) { ast::ArithmeticExpr::BinaryAssignment(ast::BinaryOperator::ShiftRight, x, Box::new(y)) }
            x:lvalue() _ "&=" _ y:(@) { ast::ArithmeticExpr::BinaryAssignment(ast::BinaryOperator::BitwiseAnd, x, Box::new(y)) }
            x:lvalue() _ "|=" _ y:(@) { ast::ArithmeticExpr::BinaryAssignment(ast::BinaryOperator::BitwiseOr, x, Box::new(y)) }
            x:lvalue() _ "^=" _ y:(@) { ast::ArithmeticExpr::BinaryAssignment(ast::BinaryOperator::BitwiseXor, x, Box::new(y)) }
            x:lvalue() _ "=" _ y:(@) { ast::ArithmeticExpr::Assignment(x, Box::new(y)) }
            --
            x:@ _ "?" _ y:expression() _ ":" _ z:(@) { ast::ArithmeticExpr::Conditional(Box::new(x), Box::new(y), Box::new(z)) }
            --
            x:(@) _ "||" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::LogicalOr, Box::new(x), Box::new(y)) }
            --
            x:(@) _ "&&" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::LogicalAnd, Box::new(x), Box::new(y)) }
            --
            x:(@) _ "|" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::BitwiseOr, Box::new(x), Box::new(y)) }
            --
            x:(@) _ "^" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::BitwiseXor, Box::new(x), Box::new(y)) }
            --
            x:(@) _ "&" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::BitwiseAnd, Box::new(x), Box::new(y)) }
            --
            x:(@) _ "==" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::Equals, Box::new(x), Box::new(y)) }
            x:(@) _ "!=" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::NotEquals, Box::new(x), Box::new(y)) }
            --
            x:(@) _ "<" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::LessThan, Box::new(x), Box::new(y)) }
            x:(@) _ ">" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::GreaterThan, Box::new(x), Box::new(y)) }
            x:(@) _ "<=" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::LessThanOrEqualTo, Box::new(x), Box::new(y)) }
            x:(@) _ ">=" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::GreaterThanOrEqualTo, Box::new(x), Box::new(y)) }
            --
            x:(@) _ "<<" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::ShiftLeft, Box::new(x), Box::new(y)) }
            x:(@) _ ">>" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::ShiftRight, Box::new(x), Box::new(y)) }
            --
            x:(@) _ "+" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::Add, Box::new(x), Box::new(y)) }
            x:(@) _ "-" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::Subtract, Box::new(x), Box::new(y)) }
            --
            x:(@) _ "*" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::Multiply, Box::new(x), Box::new(y)) }
            x:(@) _ "%" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::Modulo, Box::new(x), Box::new(y)) }
            x:(@) _ "/" _ y:@ { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::Divide, Box::new(x), Box::new(y)) }
            --
            x:@ _ "**" _ y:(@) { ast::ArithmeticExpr::BinaryOp(ast::BinaryOperator::Power, Box::new(x), Box::new(y)) }
            --
            "!" _ x:(@) { ast::ArithmeticExpr::UnaryOp(ast::UnaryOperator::LogicalNot, Box::new(x)) }
            "~" _ x:(@) { ast::ArithmeticExpr::UnaryOp(ast::UnaryOperator::BitwiseNot, Box::new(x)) }
            --
            // NOTE: `++` and `--` step a variable that follows them; before anything else they are
            // two signs, as in bash.
            "+" !("+" _ ['a'..='z' | 'A'..='Z' | '_']) _ x:(@) { ast::ArithmeticExpr::UnaryOp(ast::UnaryOperator::UnaryPlus, Box::new(x)) }
            "-" !("-" _ ['a'..='z' | 'A'..='Z' | '_']) _ x:(@) { ast::ArithmeticExpr::UnaryOp(ast::UnaryOperator::UnaryMinus, Box::new(x)) }
            --
            "++" _ x:lvalue() { ast::ArithmeticExpr::UnaryAssignment(ast::UnaryAssignmentOperator::PrefixIncrement, x) }
            "--" _ x:lvalue() { ast::ArithmeticExpr::UnaryAssignment(ast::UnaryAssignmentOperator::PrefixDecrement, x) }
            --
            x:lvalue() _ "++" { ast::ArithmeticExpr::UnaryAssignment(ast::UnaryAssignmentOperator::PostfixIncrement, x) }
            x:lvalue() _ "--" { ast::ArithmeticExpr::UnaryAssignment(ast::UnaryAssignmentOperator::PostfixDecrement, x) }
            --
            n:literal_number() { ast::ArithmeticExpr::Literal(n) }
            l:lvalue() { ast::ArithmeticExpr::Reference(l) }
            "(" _ expr:expression() _ ")" { expr }
        }

        // The subscript is kept as written: an associative array uses it as its key, and an
        // indexed array evaluates it, as bash does.
        rule lvalue() -> ast::ArithmeticTarget =
            name:variable_name() "[" index:$(subscript()) "]" {
                ast::ArithmeticTarget::ArrayElement(name.to_owned(), index.to_owned())
            } /
            name:variable_name() {
                ast::ArithmeticTarget::Variable(name.to_owned())
            }

        rule subscript() = ([^ '[' | ']'] / "[" subscript() "]")*

        rule variable_name() -> &'input str =
            $(['a'..='z' | 'A'..='Z' | '_'](['a'..='z' | 'A'..='Z' | '_' | '0'..='9']*))

        rule _() -> () = quiet!{[' ' | '\t' | '\n' | '\r']*} {}

        rule literal_number() -> i64 =
            // Literal with explicit radix (format: <base>#<literal>)
            radix:decimal_literal() "#" s:$(['0'..='9' | 'a'..='z' | 'A'..='Z' | '@' | '_']+) {?
                parse_shell_literal_number(s, radix.cast_unsigned())
            } /
            // Hex literal (a bare `0x` is 0, as in bash)
            "0" ['x' | 'X'] s:$(['0'..='9' | 'a'..='f' | 'A'..='F']*) { wrapping_literal(s, 16) } /
            // Octal literal
            s:$("0" ['0'..='7']*) { wrapping_literal(s, 8) } /
            // Decimal literal
            decimal_literal()

        // A literal too large for 64 bits wraps, as in bash. This also gives INT64_MIN for
        // -9223372036854775808.
        rule decimal_literal() -> i64 =
            s:$(['1'..='9'] ['0'..='9']*) { wrapping_literal(s, 10) }
    }
}

/// The value of a literal's digits (all valid in the radix), wrapped to 64 bits.
fn wrapping_literal(digits: &str, radix: u32) -> i64 {
    digits
        .chars()
        .filter_map(|c| c.to_digit(radix))
        .fold(0_u64, |value, digit| {
            value
                .wrapping_mul(u64::from(radix))
                .wrapping_add(u64::from(digit))
        })
        .cast_signed()
}

fn parse_shell_literal_number(s: &str, radix: u64) -> Result<i64, &'static str> {
    if !(2..=64).contains(&radix) {
        return Err("invalid base");
    }

    // For bases <= 36: case-insensitive (a-z and A-Z both map to 10-35)
    // For bases > 36 (bash extension):
    //   0-9 = 0-9, a-z = 10-35, A-Z = 36-61, @ = 62, _ = 63
    let mut result: i64 = 0;

    for ch in s.chars() {
        let digit_val = if radix <= 36 {
            match ch {
                '0'..='9' => (ch as u64) - ('0' as u64),
                'a'..='z' => (ch as u64) - ('a' as u64) + 10,
                'A'..='Z' => (ch as u64) - ('A' as u64) + 10,
                _ => return Err("invalid digit"),
            }
        } else {
            match ch {
                '0'..='9' => (ch as u64) - ('0' as u64),
                'a'..='z' => (ch as u64) - ('a' as u64) + 10,
                'A'..='Z' => (ch as u64) - ('A' as u64) + 36,
                '@' => 62,
                '_' => 63,
                _ => return Err("invalid digit"),
            }
        };

        if digit_val >= radix {
            return Err("value too great for base");
        }

        result = result
            .wrapping_mul(radix.cast_signed())
            .wrapping_add(digit_val.cast_signed());
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(input: &str) -> Option<(String, String)> {
        match parse(input).ok()? {
            ast::ArithmeticExpr::Reference(ast::ArithmeticTarget::ArrayElement(name, index))
            | ast::ArithmeticExpr::UnaryAssignment(
                _,
                ast::ArithmeticTarget::ArrayElement(name, index),
            ) => Some((name, index)),
            _ => None,
        }
    }

    #[test]
    fn subscripts_are_kept_as_written() {
        for (input, array, subscript) in [
            ("m[k]", "m", "k"),
            ("m[ k ]", "m", " k "),
            ("m[foo.txt]++", "m", "foo.txt"),
            ("m[a-b]", "m", "a-b"),
            ("a[i+1]", "a", "i+1"),
            ("a[b[1]]", "a", "b[1]"),
        ] {
            assert_eq!(
                element(input),
                Some((array.to_owned(), subscript.to_owned())),
                "{input}"
            );
        }
    }

    #[test]
    fn literals_wrap_to_64_bits() {
        for (input, value) in [
            ("99999999999999999999", 7_766_279_631_452_241_919),
            ("18446744073709551616", 0),
            ("0777777777777777777777777", -1),
            ("0xffffffffffffffffff", -1),
            ("0x", 0),
        ] {
            assert!(
                matches!(parse(input), Ok(ast::ArithmeticExpr::Literal(v)) if v == value),
                "{input}"
            );
        }
    }

    #[test]
    fn steps_before_a_number_are_signs() {
        for input in ["++1", "-- 1"] {
            assert!(
                matches!(
                    parse(input),
                    Ok(ast::ArithmeticExpr::UnaryOp(_, ref operand))
                        if matches!(**operand, ast::ArithmeticExpr::UnaryOp(_, _))
                ),
                "{input}"
            );
        }
        assert!(matches!(
            parse("++ a"),
            Ok(ast::ArithmeticExpr::UnaryAssignment(
                ast::UnaryAssignmentOperator::PrefixIncrement,
                _
            ))
        ));
    }

    #[test]
    fn blank_expressions_are_zero() {
        for input in ["", " ", "  \t "] {
            assert!(
                matches!(parse(input), Ok(ast::ArithmeticExpr::Literal(0))),
                "{input:?}"
            );
        }
    }
}
