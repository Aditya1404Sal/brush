//! Bash's reading of an arithmetic expression, for its syntax errors.
//!
//! An expression is read again the way bash's `expr.c` reads it, to find the error bash reports
//! and the token it reports it at. Only the syntax and the number literals are checked; nothing
//! is evaluated. Bash's grammar is also stricter than the parser's in places: an assignment binds
//! only at its own level, so `-a=1` and `1 + a = 2` are errors.

/// A malformed arithmetic expression, worded as bash reports it:
/// `EXPR: MESSAGE (error token is "TOKEN")`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxError {
    /// The expression as bash names it: without its leading blanks, and cut after a malformed
    /// number.
    pub expression: String,
    /// What is wrong.
    pub message: &'static str,
    /// The expression from the token where the error was found.
    pub token: String,
}

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} (error token is \"{}\")",
            self.expression, self.message, self.token
        )
    }
}

/// The expression without its leading blanks, as bash names it in a diagnostic.
pub(crate) fn without_leading_blanks(expression: &str) -> &str {
    expression.trim_start_matches([' ', '\t'])
}

/// The error bash finds in an expression, if any. A blank expression is 0.
pub(crate) fn check(expression: &str) -> Option<SyntaxError> {
    if expression.trim_matches([' ', '\t', '\n', '\r']).is_empty() {
        return None;
    }
    let failure = Reader::new(expression).check().err()?;
    let text = failure
        .number_end
        .and_then(|end| expression.get(..end))
        .unwrap_or(expression);
    Some(SyntaxError {
        expression: without_leading_blanks(text).to_owned(),
        message: failure.message,
        token: text.get(failure.at..).unwrap_or_default().to_owned(),
    })
}

/// The error in an expression that does not parse, as bash reports it.
pub(crate) fn diagnose(expression: &str) -> SyntaxError {
    check(expression).unwrap_or_else(|| SyntaxError {
        expression: without_leading_blanks(expression).to_owned(),
        message: OPERAND_EXPECTED,
        token: expression
            .trim_end()
            .chars()
            .last()
            .map(String::from)
            .unwrap_or_default(),
    })
}

const OPERAND_EXPECTED: &str = "arithmetic syntax error: operand expected";

/// How deeply the expression nests, counted as the parser and the evaluator recurse: one for
/// each parenthesis, prefix operator (`-`, `!`, `++`) and right-associative operator (`**`, `=`,
/// `?`) still open. A chain of left-associative operators (`1+2+3`) does not nest. The count is
/// read with a loop, so it costs no stack however deep the expression is.
pub(crate) fn nesting(expression: &str) -> usize {
    let mut reader = Reader::new(expression);
    // For each open parenthesis, what was open around it.
    let mut levels: Vec<usize> = vec![];
    let mut outside = 0;
    // What is open in the current parenthesis: assignments and conditionals until a comma,
    // powers until another operator, prefix operators until their operand.
    let (mut assignments, mut powers, mut prefixes) = (0, 0, 0);
    let mut deepest = 0;
    while reader.next().is_ok() {
        let previous_is_operand = matches!(
            reader.previous,
            Token::Number | Token::Name | Token::CloseParen | Token::PostStep
        );
        match reader.current {
            Token::End => break,
            Token::OpenParen => {
                let open = assignments + powers + prefixes + 1;
                levels.push(open);
                outside += open;
                (assignments, powers, prefixes) = (0, 0, 0);
            }
            Token::CloseParen => {
                outside -= levels.pop().unwrap_or(0).min(outside);
                (assignments, powers, prefixes) = (0, 0, 0);
            }
            Token::Number | Token::Name => prefixes = 0,
            Token::PreStep | Token::Not | Token::BitNot => prefixes += 1,
            Token::Plus | Token::Minus if !previous_is_operand => prefixes += 1,
            Token::Power => powers += 1,
            Token::Assign | Token::OperatorAssign | Token::Question => {
                assignments += 1;
                powers = 0;
            }
            Token::Comma => (assignments, powers) = (0, 0),
            _ => powers = 0,
        }
        deepest = deepest.max(outside + assignments + powers + prefixes);
    }
    deepest
}

/// The tokens of bash's arithmetic grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    End,
    Number,
    Name,
    Assign,
    OperatorAssign,
    Equal,
    NotEqual,
    LessEqual,
    GreaterEqual,
    Less,
    Greater,
    ShiftLeft,
    ShiftRight,
    And,
    Or,
    Power,
    PreStep,
    PostStep,
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
    Not,
    BitNot,
    OpenParen,
    CloseParen,
    BitAnd,
    BitOr,
    BitXor,
    Question,
    Colon,
    Comma,
    /// Not read, but set after a conditional expression, which cannot be assigned to.
    Conditional,
}

impl Token {
    /// The single-character operators.
    const fn single(c: u8) -> Option<Self> {
        Some(match c {
            b'=' => Self::Assign,
            b'<' => Self::Less,
            b'>' => Self::Greater,
            b'+' => Self::Plus,
            b'-' => Self::Minus,
            b'*' => Self::Multiply,
            b'/' => Self::Divide,
            b'%' => Self::Modulo,
            b'!' => Self::Not,
            b'~' => Self::BitNot,
            b'(' => Self::OpenParen,
            b')' => Self::CloseParen,
            b'&' => Self::BitAnd,
            b'|' => Self::BitOr,
            b'^' => Self::BitXor,
            b'?' => Self::Question,
            b':' => Self::Colon,
            b',' => Self::Comma,
            _ => return None,
        })
    }

    /// Whether this is an operand (a number or a variable), after which an unknown character is
    /// an invalid operator rather than a missing operand.
    const fn is_operand(self) -> bool {
        matches!(self, Self::Number | Self::Name)
    }
}

/// Where and why reading stopped.
struct Failure {
    message: &'static str,
    /// The byte offset of the error token.
    at: usize,
    /// For a malformed number, where it ends: bash names the expression only up to there.
    number_end: Option<usize>,
}

/// Reads an expression as bash's `readtok` does, keeping the current and the previous token and
/// the start of the last token read (`lasttp`), which is where bash reports an error.
struct Reader<'a> {
    text: &'a [u8],
    position: usize,
    last_start: usize,
    current: Token,
    previous: Token,
}

type Checked = Result<(), Failure>;

impl<'a> Reader<'a> {
    const fn new(text: &'a str) -> Self {
        Self {
            text: text.as_bytes(),
            position: 0,
            last_start: 0,
            current: Token::End,
            previous: Token::End,
        }
    }

    const fn fail(&self, message: &'static str) -> Failure {
        Failure {
            message,
            at: self.last_start,
            number_end: None,
        }
    }

    fn byte(&self, index: usize) -> Option<u8> {
        self.text.get(index).copied()
    }

    /// Checks the whole expression.
    fn check(&mut self) -> Checked {
        self.next()?;
        self.comma()?;
        if self.current == Token::End {
            Ok(())
        } else {
            Err(self.fail("arithmetic syntax error in expression"))
        }
    }

    /// Reads the next token.
    fn next(&mut self) -> Checked {
        let mut cp = self.position;
        while self
            .byte(cp)
            .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r'))
        {
            cp += 1;
        }
        let Some(c) = self.byte(cp) else {
            self.position = cp;
            self.previous = self.current;
            self.current = Token::End;
            return Ok(());
        };
        let start = cp;
        self.last_start = start;
        let token = if c.is_ascii_alphabetic() || c == b'_' {
            cp += 1;
            while self
                .byte(cp)
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                cp += 1;
            }
            if self.byte(cp) == Some(b'[') {
                match self.subscript_end(cp) {
                    Some(end) => cp = end + 1,
                    None => return Err(self.fail("bad array subscript")),
                }
            }
            Token::Name
        } else if c.is_ascii_digit() {
            while self
                .byte(cp)
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'#' | b'@' | b'_'))
            {
                cp += 1;
            }
            if let Err(message) = check_number(&self.text[start..cp]) {
                return Err(Failure {
                    message,
                    at: start,
                    number_end: Some(cp),
                });
            }
            Token::Number
        } else {
            let (token, width) = self.operator(c, start)?;
            cp += width;
            token
        };
        self.position = cp;
        self.previous = self.current;
        self.current = token;
        Ok(())
    }

    /// The operator at `start`, whose first character is `c`, and its width.
    fn operator(&self, c: u8, start: usize) -> Result<(Token, usize), Failure> {
        let c1 = self.byte(start + 1);
        let c2 = self.byte(start + 2);
        Ok(match (c, c1) {
            (b'=', Some(b'=')) => (Token::Equal, 2),
            (b'!', Some(b'=')) => (Token::NotEqual, 2),
            (b'>', Some(b'=')) => (Token::GreaterEqual, 2),
            (b'<', Some(b'=')) => (Token::LessEqual, 2),
            (b'<', Some(b'<')) if c2 == Some(b'=') => (Token::OperatorAssign, 3),
            (b'<', Some(b'<')) => (Token::ShiftLeft, 2),
            (b'>', Some(b'>')) if c2 == Some(b'=') => (Token::OperatorAssign, 3),
            (b'>', Some(b'>')) => (Token::ShiftRight, 2),
            (b'&', Some(b'&')) => (Token::And, 2),
            (b'|', Some(b'|')) => (Token::Or, 2),
            (b'*', Some(b'*')) => (Token::Power, 2),
            (b'+' | b'-', Some(c1)) if c1 == c && self.current == Token::Name => {
                (Token::PostStep, 2)
            }
            // `++` or `--` before a name steps it; otherwise it is two signs.
            (b'+' | b'-', Some(c1)) if c1 == c => {
                let mut next = start + 2;
                while self
                    .byte(next)
                    .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r'))
                {
                    next += 1;
                }
                if self
                    .byte(next)
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
                {
                    (Token::PreStep, 2)
                } else if c == b'+' {
                    (Token::Plus, 1)
                } else {
                    (Token::Minus, 1)
                }
            }
            (b'*' | b'/' | b'%' | b'+' | b'-' | b'&' | b'^' | b'|', Some(b'=')) => {
                (Token::OperatorAssign, 2)
            }
            _ => match Token::single(c) {
                Some(token) => (token, 1),
                None if self.current.is_operand() => {
                    return Err(self.fail("arithmetic syntax error: invalid arithmetic operator"));
                }
                None => return Err(self.fail(OPERAND_EXPECTED)),
            },
        })
    }

    /// The index of the `]` that closes the subscript opened at `open`.
    fn subscript_end(&self, open: usize) -> Option<usize> {
        let mut depth = 0_usize;
        for (index, c) in self.text.iter().enumerate().skip(open) {
            match c {
                b'[' => depth += 1,
                b']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(index);
                    }
                }
                _ => (),
            }
        }
        None
    }

    /// `expr , expr`
    fn comma(&mut self) -> Checked {
        self.assignment()?;
        while self.current == Token::Comma {
            self.next()?;
            self.assignment()?;
        }
        Ok(())
    }

    /// `name = expr`, `name op= expr`
    fn assignment(&mut self) -> Checked {
        self.conditional()?;
        if matches!(self.current, Token::Assign | Token::OperatorAssign) {
            if self.previous != Token::Name {
                return Err(self.fail("attempted assignment to non-variable"));
            }
            self.next()?;
            self.assignment()?;
        }
        Ok(())
    }

    /// `expr ? expr : expr`
    fn conditional(&mut self) -> Checked {
        self.binary(0)?;
        if self.current == Token::Question {
            self.next()?;
            if matches!(self.current, Token::End | Token::Colon) {
                return Err(self.fail("expression expected"));
            }
            self.comma()?;
            if self.current != Token::Colon {
                return Err(self.fail("`:' expected for conditional expression"));
            }
            self.next()?;
            if self.current == Token::End {
                return Err(self.fail("expression expected"));
            }
            self.conditional()?;
            self.previous = Token::Conditional;
        }
        Ok(())
    }

    /// The binary operators, loosest first.
    const LEVELS: &'static [&'static [Token]] = &[
        &[Token::Or],
        &[Token::And],
        &[Token::BitOr],
        &[Token::BitXor],
        &[Token::BitAnd],
        &[Token::Equal, Token::NotEqual],
        &[
            Token::LessEqual,
            Token::GreaterEqual,
            Token::Less,
            Token::Greater,
        ],
        &[Token::ShiftLeft, Token::ShiftRight],
        &[Token::Plus, Token::Minus],
        &[Token::Multiply, Token::Divide, Token::Modulo],
    ];

    /// A left-associative binary operator of the given level, or a power below the last.
    fn binary(&mut self, level: usize) -> Checked {
        let Some(operators) = Self::LEVELS.get(level) else {
            return self.power();
        };
        self.binary(level + 1)?;
        while operators.contains(&self.current) {
            self.next()?;
            self.binary(level + 1)?;
            self.previous = Token::Number;
        }
        Ok(())
    }

    /// `expr ** expr`, which is right-associative.
    fn power(&mut self) -> Checked {
        self.unary()?;
        if self.current == Token::Power {
            self.next()?;
            self.power()?;
            self.previous = Token::Number;
        }
        Ok(())
    }

    /// `!expr`, `~expr`, `-expr`, `+expr`
    fn unary(&mut self) -> Checked {
        if matches!(
            self.current,
            Token::Not | Token::BitNot | Token::Minus | Token::Plus
        ) {
            self.next()?;
            self.unary()?;
            self.previous = Token::Number;
            Ok(())
        } else {
            self.operand()
        }
    }

    /// A number, a variable (maybe stepped), a stepped variable, or `( expr )`.
    fn operand(&mut self) -> Checked {
        match self.current {
            Token::PreStep => {
                self.next()?;
                if self.current != Token::Name {
                    return Err(
                        self.fail("identifier expected after pre-increment or pre-decrement")
                    );
                }
                // A stepped variable is a value, so `++x = 1` assigns to a non-variable.
                self.current = Token::Number;
                self.next()
            }
            Token::OpenParen => {
                self.next()?;
                self.comma()?;
                if self.current != Token::CloseParen {
                    return Err(self.fail("missing `)'"));
                }
                self.next()
            }
            Token::Number => self.next(),
            Token::Name => {
                self.next()?;
                if self.current == Token::PostStep {
                    self.current = Token::Number;
                    self.next()?;
                }
                Ok(())
            }
            _ => Err(self.fail(OPERAND_EXPECTED)),
        }
    }
}

/// Checks a number literal as bash's `strlong` does: `0` starts an octal and `0x` a hexadecimal
/// number, and `BASE#DIGITS` gives a base from 2 to 64.
fn check_number(text: &[u8]) -> Result<(), &'static str> {
    let mut digits = text.iter().copied().peekable();
    let (mut base, mut found_base): (u64, bool) = if digits.next_if_eq(&b'0').is_some() {
        if digits.peek().is_none() {
            return Ok(());
        }
        if digits.next_if(|c| matches!(c, b'x' | b'X')).is_some() {
            (16, true)
        } else {
            (8, true)
        }
    } else {
        (10, false)
    };
    let mut value: u64 = 0;
    while let Some(c) = digits.next() {
        if c == b'#' {
            if found_base {
                return Err("invalid number");
            }
            if !(2..=64).contains(&value) {
                return Err("invalid arithmetic base");
            }
            base = value;
            value = 0;
            found_base = true;
            if !digits
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'@' | b'_'))
            {
                return Err("invalid integer constant");
            }
            continue;
        }
        let digit = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'z' => c - b'a' + 10,
            b'A'..=b'Z' if base <= 36 => c - b'A' + 10,
            b'A'..=b'Z' => c - b'A' + 36,
            b'@' => 62,
            _ => 63,
        };
        if u64::from(digit) >= base {
            return Err("value too great for base");
        }
        value = value.wrapping_mul(base).wrapping_add(u64::from(digit));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(expression: &str) -> String {
        diagnose(expression).to_string()
    }

    #[test]
    fn numbers_are_checked_as_bash_checks_them() {
        for (expression, expected) in [
            ("08", "08: value too great for base (error token is \"08\")"),
            (
                "1 + 08 + 2",
                "1 + 08: value too great for base (error token is \"08\")",
            ),
            (
                "65#1",
                "65#1: invalid arithmetic base (error token is \"65#1\")",
            ),
            ("0#1", "0#1: invalid number (error token is \"0#1\")"),
            ("2#", "2#: invalid integer constant (error token is \"2#\")"),
            (
                "37#Z",
                "37#Z: value too great for base (error token is \"37#Z\")",
            ),
        ] {
            assert_eq!(message(expression), expected, "{expression}");
        }
    }

    #[test]
    fn nesting_counts_what_the_parser_recurses_on() {
        for (expression, depth) in [
            ("1+2+3+4", 0),
            ("((1))", 2),
            ("- - -1", 3),
            ("2**3**4", 2),
            ("a=b=c=1", 3),
            ("a=1, b=2, c=3", 1),
            ("1+(2+(3+(4)))", 3),
            ("(1)+(2)+(3)", 1),
            ("!(a ? b : c ? d : e)", 4),
        ] {
            assert_eq!(nesting(expression), depth, "{expression}");
        }
        let deep = format!("{}1{}", "(".repeat(10_000), ")".repeat(10_000));
        assert_eq!(nesting(&deep), 10_000);
    }

    #[test]
    fn assignments_bind_only_at_their_own_level() {
        for expression in ["-a=1", "1 + a = 2", "1 ? a : b = 2", "(a)=1", "++a = 1"] {
            assert_eq!(
                check(expression).map(|error| error.message),
                Some("attempted assignment to non-variable"),
                "{expression}"
            );
        }
        for expression in [
            "a = b ? c : d",
            "a ? b = 1 : c",
            "a[i+1] += 2, b = c = 3",
            "x++ + ++y",
            "- -5 ** 2",
            " ",
            "m[foo.txt]++",
        ] {
            assert_eq!(check(expression), None, "{expression}");
        }
    }

    #[test]
    fn syntax_errors_name_the_token_bash_stops_at() {
        for (expression, expected) in [
            (
                "1+",
                "1+: arithmetic syntax error: operand expected (error token is \"+\")",
            ),
            (
                " 1 @ 2 ",
                "1 @ 2 : arithmetic syntax error: invalid arithmetic operator (error token is \"@ 2 \")",
            ),
            (
                "a b c",
                "a b c: arithmetic syntax error in expression (error token is \"b c\")",
            ),
            (
                "x++=7",
                "x++=7: attempted assignment to non-variable (error token is \"=7\")",
            ),
            (
                "1 ? 2",
                "1 ? 2: `:' expected for conditional expression (error token is \"2\")",
            ),
            ("(1", "(1: missing `)' (error token is \"1\")"),
            ("a[1", "a[1: bad array subscript (error token is \"a[1\")"),
        ] {
            assert_eq!(message(expression), expected, "{expression}");
        }
    }
}
