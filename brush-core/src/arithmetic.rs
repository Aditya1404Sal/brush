//! Arithmetic evaluation

use std::borrow::Cow;

use crate::{ExecutionParameters, Shell, env, expansion, extensions, variables};
use brush_parser::ast;

mod syntax;

pub use syntax::SyntaxError;

/// Maximum recursion depth for arithmetic variable dereference chains
/// (e.g., a=b, b=c, c=a would cycle through variable dereferences). Bash allows 1024; on WASM
/// each level costs about half a KiB of Wasmtime's 512 KiB native stack, which deeply nested
/// shell code may already have used most of, so the limit there is 200.
#[cfg(not(target_arch = "wasm32"))]
const MAX_VARIABLE_DEREF_DEPTH: u32 = 1024;
#[cfg(target_arch = "wasm32")]
const MAX_VARIABLE_DEREF_DEPTH: u32 = 200;

/// Represents an error that occurs during evaluation of an arithmetic expression.
#[derive(Clone, Debug, thiserror::Error)]
pub enum EvalError {
    /// Division by zero.
    #[error("division by 0")]
    DivideByZero,

    /// Negative exponent.
    #[error("exponent less than 0")]
    NegativeExponent,

    /// Failed to tokenize an arithmetic expression.
    #[error("failed to tokenize expression")]
    FailedToTokenizeExpression,

    /// Failed to expand an arithmetic expression.
    #[error("failed to expand expression: {0}")]
    FailedToExpandExpression(String),

    /// Failed to access an element of an array.
    #[error("failed to access array")]
    FailedToAccessArray,

    /// Failed to update the shell environment in an assignment operator.
    #[error("failed to update environment")]
    FailedToUpdateEnvironment,

    /// Failed to parse an arithmetic expression.
    #[error("arithmetic syntax error: operand expected")]
    ParseError(String),

    /// A malformed expression, with the error and the token bash names.
    #[error("{0}")]
    Syntax(Box<SyntaxError>),

    /// Error expanding an unset variable.
    #[error("{0}: unbound variable")]
    ExpandingUnsetVariable(String),

    /// An error in the named expression, worded as bash reports it.
    #[error("{}", in_expression_message(.0, .1))]
    InExpression(String, Box<Self>),

    /// Expression recursion level exceeded.
    #[error("expression recursion level exceeded")]
    RecursionLimitExceeded,

    /// An assignment to a readonly variable.
    #[error("{0}: readonly variable")]
    ReadonlyVariable(String),

    /// An error in an indexed array's subscript, which ends the shell as bash's does, reported
    /// without the command's name.
    #[error("{0}")]
    InSubscript(Box<Self>),

    /// An error in the offset or length of the substring expansion of the named parameter
    /// (`${x:1+}`), worded as bash words it.
    #[error("{0}: {1}")]
    InSubstring(String, Box<Self>),

    /// An expression that nests deeper than the stack can hold.
    #[error(
        "arithmetic expression nesting level exceeded ({0}): deeper nesting is unsupported in bash-tool"
    )]
    NestedTooDeeply(usize),
}

/// Trait implemented by arithmetic expressions that can be evaluated.
pub(crate) trait ExpandAndEvaluate {
    /// Evaluate the given expression, returning the resulting numeric value.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell to use for evaluation.
    /// * `trace_if_needed` - Whether to trace the evaluation.
    async fn eval(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
        trace_if_needed: bool,
    ) -> Result<i64, EvalError>;
}

impl ExpandAndEvaluate for ast::UnexpandedArithmeticExpr {
    async fn eval(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
        trace_if_needed: bool,
    ) -> Result<i64, EvalError> {
        // The text is expanded as bash expands an arithmetic expression, not as a word.
        let expr = self.value.as_str();
        let expanded = expansion::basic_expand_arithmetic_text(shell, params, expr)
            .await
            .map_err(|_e| EvalError::FailedToExpandExpression(expr.to_owned()))?;
        eval_expanded(shell, params, expanded, trace_if_needed).await
    }
}

/// Evaluate the given arithmetic expression, returning the resulting numeric value.
///
/// # Arguments
///
/// * `shell` - The shell to use for evaluation.
/// * `expr` - The unexpanded arithmetic expression to evaluate.
/// * `trace_if_needed` - Whether to trace the evaluation.
pub(crate) async fn expand_and_eval(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    expr: &str,
    trace_if_needed: bool,
) -> Result<i64, EvalError> {
    // Per documentation, first shell-expand it.
    let options = expansion::ExpanderOptions {
        tilde_expand: false,
        ..Default::default()
    };
    let expanded_self = expansion::basic_expand_word_with_options(shell, params, expr, &options)
        .await
        .map_err(|_e| EvalError::FailedToExpandExpression(expr.to_owned()))?;

    eval_expanded(shell, params, expanded_self, trace_if_needed).await
}

/// Parses and evaluates an expanded arithmetic expression.
async fn eval_expanded(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    expanded_self: String,
    trace_if_needed: bool,
) -> Result<i64, EvalError> {
    // Now parse.
    let expr = parse(&expanded_self)?;

    // Trace if applicable: bash prints the expanded expression as written.
    if trace_if_needed && shell.options().print_commands_and_arguments {
        shell
            .trace_command(params, std::format!("(( {expanded_self} ))"))
            .await;
    }

    // Now evaluate.
    eval_reporting(&expr, shell, params)
        .map_err(|error| EvalError::in_expression(&expanded_self, error))
}

/// Evaluates a parsed expression, writing the warnings a circular name reference gets from it
/// (see `Shell::note_circular_nameref`) to the standard error of `params`.
pub fn eval_reporting(
    expr: &ast::ArithmeticExpr,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<i64, EvalError> {
    let outer = shell.nameref_warnings.replace(String::new());
    let result = expr.eval(shell);
    let warnings = std::mem::replace(&mut shell.nameref_warnings, outer).unwrap_or_default();
    if !warnings.is_empty() {
        use std::io::Write as _;
        let _ = params.stderr(shell).write_all(warnings.as_bytes());
    }
    result
}

/// How deeply an expression may nest (see `syntax::nesting`): the parser and the evaluator
/// recurse at each level, and a hundred levels fit in the stack left at the shell's deepest
/// nesting. A chain of left-associative operators (`1+2+…`) does not nest, and may be as long as
/// memory allows.
const MAX_EXPRESSION_NESTING: usize = 100;

/// A parsed arithmetic expression. A long chain of operators (`1+2+…+5000`) makes a deep tree,
/// so it is taken apart with a loop rather than dropped recursively.
pub struct ParsedExpression(ast::ArithmeticExpr);

impl std::ops::Deref for ParsedExpression {
    type Target = ast::ArithmeticExpr;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for ParsedExpression {
    fn drop(&mut self) {
        let mut parts = vec![std::mem::replace(
            &mut self.0,
            ast::ArithmeticExpr::Literal(0),
        )];
        while let Some(expr) = parts.pop() {
            match expr {
                ast::ArithmeticExpr::UnaryOp(_, operand)
                | ast::ArithmeticExpr::Assignment(_, operand)
                | ast::ArithmeticExpr::BinaryAssignment(_, _, operand) => parts.push(*operand),
                ast::ArithmeticExpr::BinaryOp(_, left, right) => {
                    parts.push(*left);
                    parts.push(*right);
                }
                ast::ArithmeticExpr::Conditional(condition, then_expr, else_expr) => {
                    parts.push(*condition);
                    parts.push(*then_expr);
                    parts.push(*else_expr);
                }
                ast::ArithmeticExpr::Literal(_)
                | ast::ArithmeticExpr::Reference(_)
                | ast::ArithmeticExpr::UnaryAssignment(..) => (),
            }
        }
    }
}

/// Parses an arithmetic expression, failing as bash reports a malformed one, or as one that
/// nests too deeply to evaluate.
///
/// # Arguments
///
/// * `text` - The (already expanded) expression.
pub fn parse(text: &str) -> Result<ParsedExpression, EvalError> {
    let nesting = syntax::nesting(text);
    if nesting > MAX_EXPRESSION_NESTING {
        return Err(EvalError::NestedTooDeeply(nesting));
    }
    match brush_parser::arithmetic::parse(text).map(ParsedExpression) {
        // Bash's grammar is stricter than the parser's in places (`-a=1`), so an expression bash
        // rejects fails even when it parses.
        Ok(expr) => match syntax::check(text) {
            Some(error) => Err(EvalError::Syntax(Box::new(error))),
            None => Ok(expr),
        },
        Err(_) => Err(EvalError::Syntax(Box::new(syntax::diagnose(text)))),
    }
}

/// Bash's wording for an arithmetic error: `EXPR: message (error token is "TOKEN")`, where the
/// token is the part of the expression where evaluation failed.
fn in_expression_message(expr: &str, error: &EvalError) -> String {
    let token = match error {
        // Bash names the text after a `/` or `%`, and after `/=` or `%=` the last token it read.
        EvalError::DivideByZero => expr.rsplit_once(['/', '%']).map(|(_, rest)| {
            if rest.starts_with('=') {
                last_token(expr).to_owned()
            } else {
                rest.to_owned()
            }
        }),
        EvalError::NegativeExponent => expr
            .rsplit_once("**")
            .map(|(_, rest)| rest.trim_start().trim_start_matches('-').to_owned()),
        EvalError::ParseError(_) => expr.trim_end().chars().last().map(String::from),
        EvalError::RecursionLimitExceeded => Some(expr.to_owned()),
        _ => None,
    };
    match (error, token) {
        (EvalError::ExpandingUnsetVariable(_) | EvalError::ReadonlyVariable(_), _) => {
            error.to_string()
        }
        // Bash keeps the whitespace that follows the failing token.
        (_, Some(token)) => format!(
            "{expr}: {error} (error token is \"{}\")",
            token.trim_start()
        ),
        (_, None) => format!("{expr}: {error}"),
    }
}

/// The last token of `expr` and the whitespace after it, where bash's reader stops at its end.
fn last_token(expr: &str) -> &str {
    let trimmed = expr.trim_end();
    let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '#' | '@');
    let mut start = trimmed.len();
    for (at, c) in trimmed.char_indices().rev() {
        if word(c) {
            start = at;
        } else {
            // An operator is a token of its own.
            if start == trimmed.len() {
                start = at;
            }
            break;
        }
    }
    expr.get(start..).unwrap_or(expr)
}

/// Evaluates a value assigned to an integer (`declare -i`) variable, as bash does: the already
/// expanded text is an arithmetic expression, and an empty value is 0.
///
/// # Arguments
///
/// * `shell` - The shell to use for evaluation.
/// * `value` - The expanded value being assigned.
pub fn eval_integer_assignment(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    value: &str,
) -> Result<i64, EvalError> {
    if value.trim().is_empty() {
        return Ok(0);
    }
    parse(value)?
        .eval(shell)
        .map_err(|error| EvalError::in_expression(value, error))
}

/// Evaluates every value in an assignment to an integer variable (see
/// [`eval_integer_assignment`]).
pub fn eval_integer_literal(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    literal: crate::variables::ShellValueLiteral,
) -> Result<crate::variables::ShellValueLiteral, EvalError> {
    use crate::variables::{ArrayLiteral, ShellValueLiteral};
    Ok(match literal {
        ShellValueLiteral::Scalar(value) => {
            ShellValueLiteral::Scalar(eval_integer_assignment(shell, &value)?.to_string())
        }
        ShellValueLiteral::Array(ArrayLiteral(elements)) => {
            let mut evaluated = Vec::with_capacity(elements.len());
            for (key, value) in elements {
                evaluated.push((key, eval_integer_assignment(shell, &value)?.to_string()));
            }
            ShellValueLiteral::Array(ArrayLiteral(evaluated))
        }
    })
}

/// Resolves the subscripts of a compound assignment to an indexed array.
///
/// As bash's `assign_compound_array_list` does once every element is expanded, each subscript
/// is expanded again and evaluated arithmetically, in order, and placed by
/// [`variables::IndexedLiteralKeys`]: a negative one counts back from one past the highest index
/// so far. An error in a subscript ends the shell, as in bash. An empty subscript, or one that
/// counts back past the start, stops there: the elements before it are returned with the error
/// to report once they are assigned (`[KEY]=VALUE: bad array subscript`, which abandons the
/// command).
///
/// # Arguments
///
/// * `shell` - The shell to use for evaluation.
/// * `params` - The execution parameters to use.
/// * `keys` - Where the elements go: after the array's elements when appending.
/// * `literal` - The expanded elements.
pub async fn resolve_indexed_array_literal(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    mut keys: variables::IndexedLiteralKeys,
    literal: variables::ArrayLiteral,
) -> Result<(variables::ArrayLiteral, Option<crate::error::Error>), crate::error::Error> {
    let mut placed = Vec::with_capacity(literal.0.len());
    for (key, value) in literal.0 {
        let key = match key {
            Some(key) => {
                let index = if key.is_empty() {
                    None
                } else {
                    let index = expand_and_eval(shell, params, &key, false)
                        .await
                        .map_err(EvalError::in_subscript)?;
                    keys.key(index)
                };
                let Some(index) = index else {
                    let element =
                        crate::error::ErrorKind::BadArrayElement(format!("[{key}]={value}"));
                    return Ok((variables::ArrayLiteral(placed), Some(element.into())));
                };
                Some(index.to_string())
            }
            None => None,
        };
        placed.push((key, value));
        keys.placed();
    }
    Ok((variables::ArrayLiteral(placed), None))
}

/// Trait implemented by evaluatable arithmetic expressions.
pub trait Evaluatable {
    /// Evaluate the given arithmetic expression, returning the resulting numeric value.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell to use for evaluation.
    fn eval(&self, shell: &mut Shell<impl extensions::ShellExtensions>) -> Result<i64, EvalError>;
}

impl Evaluatable for ast::ArithmeticExpr {
    fn eval(&self, shell: &mut Shell<impl extensions::ShellExtensions>) -> Result<i64, EvalError> {
        eval_expr_impl(self, shell, 0)
    }
}

fn eval_expr_impl(
    expr: &ast::ArithmeticExpr,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    depth: u32,
) -> Result<i64, EvalError> {
    let value = match expr {
        ast::ArithmeticExpr::Literal(l) => *l,
        ast::ArithmeticExpr::Reference(lvalue) => deref_lvalue(shell, lvalue, depth)?,
        ast::ArithmeticExpr::UnaryOp(op, operand) => apply_unary_op(shell, *op, operand, depth)?,
        ast::ArithmeticExpr::BinaryOp(..) => eval_binary_chain(expr, shell, depth)?,
        ast::ArithmeticExpr::Conditional(condition, then_expr, else_expr) => {
            let conditional_eval = eval_expr_impl(condition, shell, depth)?;

            // Ensure we only evaluate the branch indicated by the condition.
            if conditional_eval != 0 {
                eval_expr_impl(then_expr, shell, depth)?
            } else {
                eval_expr_impl(else_expr, shell, depth)?
            }
        }
        ast::ArithmeticExpr::Assignment(lvalue, rhs) => {
            let expr_eval = eval_expr_impl(rhs, shell, depth)?;
            assign(shell, lvalue, expr_eval, depth)?
        }
        ast::ArithmeticExpr::UnaryAssignment(op, lvalue) => {
            apply_unary_assignment_op(shell, lvalue, *op, depth)?
        }
        ast::ArithmeticExpr::BinaryAssignment(op, lvalue, operand) => {
            let current = deref_lvalue(shell, lvalue, depth)?;
            let value = apply_binary_op(shell, *op, current, operand, depth)?;
            assign(shell, lvalue, value, depth)?
        }
    };

    Ok(value)
}

fn get_var_value<'a>(
    shell: &'a Shell<impl extensions::ShellExtensions>,
    name: &str,
) -> Result<Cow<'a, str>, EvalError> {
    // A nameref to an array element (`declare -n ref='arr[1]'`) reads that element.
    let target = shell.env().resolve_nameref(name);
    if let Some((base, index)) = target
        .strip_suffix(']')
        .and_then(|target| target.split_once('['))
    {
        let element = shell.env().get(base).and_then(|(_, var)| {
            var.value()
                .get_at(index, shell)
                .ok()
                .flatten()
                .map(|value| value.to_string())
        });
        if let Some(element) = element {
            return Ok(element.into());
        }
    }

    let value = shell.env_var(name).map(|var| var.resolve_value(shell));

    if let Some(value) = value
        && value.is_set()
    {
        return Ok(value.to_cow_str(shell).to_string().into());
    }

    if shell.options().treat_unset_variables_as_error {
        return Err(EvalError::ExpandingUnsetVariable(name.into()));
    }

    Ok("".into())
}

fn deref_lvalue(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    lvalue: &ast::ArithmeticTarget,
    depth: u32,
) -> Result<i64, EvalError> {
    // Bash looks a variable up once to read it, an element twice.
    match lvalue {
        ast::ArithmeticTarget::Variable(name) => shell.note_circular_nameref(name, 1, false),
        ast::ArithmeticTarget::ArrayElement(name, _) => shell.note_circular_nameref(name, 2, false),
    }
    let value_str: Cow<'_, str> = match lvalue {
        ast::ArithmeticTarget::Variable(name) => get_var_value(shell, name.as_str())?,
        ast::ArithmeticTarget::ArrayElement(name, index) => {
            let index_str = element_key(shell, name, index, depth)?;

            shell
                .env()
                .get(name)
                .map_or_else(
                    || Ok(None),
                    |(_, v)| v.value().get_at(index_str.as_str(), shell),
                )
                .map_err(|_err| EvalError::FailedToAccessArray)?
                .unwrap_or(Cow::Borrowed(""))
        }
    };

    let value_str = value_str.into_owned();
    let parsed_value = parse(&value_str)?;

    // Literals don't need depth tracking — they can't cause recursion.
    // Only increment depth when the parsed value requires further evaluation
    // (i.e., it references other variables), matching bash's behavior.
    if matches!(*parsed_value, ast::ArithmeticExpr::Literal(_)) {
        return eval_expr_impl(&parsed_value, shell, depth);
    }

    let new_depth = depth + 1;
    if new_depth > MAX_VARIABLE_DEREF_DEPTH {
        // Bash names the expression it was about to evaluate, and all of it as the token.
        return Err(EvalError::in_expression(
            &value_str,
            EvalError::RecursionLimitExceeded,
        ));
    }

    // An error in the value names the value, as bash evaluates it as an expression of its own.
    eval_expr_impl(&parsed_value, shell, new_depth)
        .map_err(|error| EvalError::in_expression(&value_str, error))
}

fn apply_unary_op(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    op: ast::UnaryOperator,
    operand: &ast::ArithmeticExpr,
    depth: u32,
) -> Result<i64, EvalError> {
    let operand_eval = eval_expr_impl(operand, shell, depth)?;

    match op {
        ast::UnaryOperator::UnaryPlus => Ok(operand_eval),
        ast::UnaryOperator::UnaryMinus => Ok(operand_eval.wrapping_neg()),
        ast::UnaryOperator::BitwiseNot => Ok(!operand_eval),
        ast::UnaryOperator::LogicalNot => Ok(bool_to_i64(operand_eval == 0)),
    }
}

/// Evaluates a binary operation. A chain of left-associative operators (`1+2+3+…`) nests down its
/// left side, so it is evaluated along that side with a loop: its length costs no stack.
fn eval_binary_chain(
    expr: &ast::ArithmeticExpr,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    depth: u32,
) -> Result<i64, EvalError> {
    let mut rights = vec![];
    let mut leftmost = expr;
    while let ast::ArithmeticExpr::BinaryOp(op, left, right) = leftmost {
        rights.push((*op, right.as_ref()));
        leftmost = left;
    }
    let mut value = eval_expr_impl(leftmost, shell, depth)?;
    while let Some((op, right)) = rights.pop() {
        value = apply_binary_op(shell, op, value, right, depth)?;
    }
    Ok(value)
}

/// Applies a binary operator to a left operand already evaluated and a right one to evaluate
/// (unless the operator short-circuits).
fn apply_binary_op(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    op: ast::BinaryOperator,
    left: i64,
    right: &ast::ArithmeticExpr,
    depth: u32,
) -> Result<i64, EvalError> {
    // First, special-case short-circuiting operators: the right operand is evaluated only
    // when it decides the result.
    match op {
        ast::BinaryOperator::LogicalAnd => {
            if left == 0 {
                return Ok(bool_to_i64(false));
            }

            let right = eval_expr_impl(right, shell, depth)?;
            return Ok(bool_to_i64(right != 0));
        }
        ast::BinaryOperator::LogicalOr => {
            if left != 0 {
                return Ok(bool_to_i64(true));
            }

            let right = eval_expr_impl(right, shell, depth)?;
            return Ok(bool_to_i64(right != 0));
        }
        _ => (),
    }

    // The remaining operators unconditionally operate both operands.
    let right = eval_expr_impl(right, shell, depth)?;

    #[expect(clippy::cast_possible_truncation)]
    #[expect(clippy::cast_sign_loss)]
    match op {
        ast::BinaryOperator::Power => {
            if right >= 0 {
                Ok(wrapping_pow_u64(left, right as u64))
            } else {
                Err(EvalError::NegativeExponent)
            }
        }
        ast::BinaryOperator::Multiply => Ok(left.wrapping_mul(right)),
        ast::BinaryOperator::Divide => {
            if right == 0 {
                Err(EvalError::DivideByZero)
            } else {
                Ok(left.wrapping_div(right))
            }
        }
        ast::BinaryOperator::Modulo => {
            if right == 0 {
                Err(EvalError::DivideByZero)
            } else {
                Ok(left.wrapping_rem(right))
            }
        }
        ast::BinaryOperator::Comma => Ok(right),
        ast::BinaryOperator::Add => Ok(left.wrapping_add(right)),
        ast::BinaryOperator::Subtract => Ok(left.wrapping_sub(right)),
        ast::BinaryOperator::ShiftLeft => Ok(left.wrapping_shl(right as u32)),
        ast::BinaryOperator::ShiftRight => Ok(left.wrapping_shr(right as u32)),
        ast::BinaryOperator::LessThan => Ok(bool_to_i64(left < right)),
        ast::BinaryOperator::LessThanOrEqualTo => Ok(bool_to_i64(left <= right)),
        ast::BinaryOperator::GreaterThan => Ok(bool_to_i64(left > right)),
        ast::BinaryOperator::GreaterThanOrEqualTo => Ok(bool_to_i64(left >= right)),
        ast::BinaryOperator::Equals => Ok(bool_to_i64(left == right)),
        ast::BinaryOperator::NotEquals => Ok(bool_to_i64(left != right)),
        ast::BinaryOperator::BitwiseAnd => Ok(left & right),
        ast::BinaryOperator::BitwiseXor => Ok(left ^ right),
        ast::BinaryOperator::BitwiseOr => Ok(left | right),
        ast::BinaryOperator::LogicalAnd => unreachable!("LogicalAnd covered above"),
        ast::BinaryOperator::LogicalOr => unreachable!("LogicalOr covered above"),
    }
}

fn apply_unary_assignment_op(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    lvalue: &ast::ArithmeticTarget,
    op: ast::UnaryAssignmentOperator,
    depth: u32,
) -> Result<i64, EvalError> {
    let value = deref_lvalue(shell, lvalue, depth)?;

    match op {
        ast::UnaryAssignmentOperator::PrefixIncrement => {
            let new_value = value.wrapping_add(1);
            assign(shell, lvalue, new_value, depth)?;
            Ok(new_value)
        }
        ast::UnaryAssignmentOperator::PrefixDecrement => {
            let new_value = value.wrapping_sub(1);
            assign(shell, lvalue, new_value, depth)?;
            Ok(new_value)
        }
        ast::UnaryAssignmentOperator::PostfixIncrement => {
            let new_value = value.wrapping_add(1);
            assign(shell, lvalue, new_value, depth)?;
            Ok(value)
        }
        ast::UnaryAssignmentOperator::PostfixDecrement => {
            let new_value = value.wrapping_sub(1);
            assign(shell, lvalue, new_value, depth)?;
            Ok(value)
        }
    }
}

fn assign(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    lvalue: &ast::ArithmeticTarget,
    value: i64,
    depth: u32,
) -> Result<i64, EvalError> {
    // Bash looks a variable up once and binds it to assign it (an element: three lookups).
    match lvalue {
        ast::ArithmeticTarget::Variable(name) => shell.note_circular_nameref(name, 1, true),
        ast::ArithmeticTarget::ArrayElement(name, _) => shell.note_circular_nameref(name, 3, false),
    }
    match lvalue {
        ast::ArithmeticTarget::Variable(name) => {
            shell
                .env_mut()
                .update_or_add(
                    name.as_str(),
                    variables::ShellValueLiteral::Scalar(value.to_string()),
                    |_| Ok(()),
                    env::EnvironmentLookup::Anywhere,
                    env::EnvironmentScope::Global,
                )
                .map_err(|error| assignment_error(&error))?;
        }
        ast::ArithmeticTarget::ArrayElement(name, index) => {
            let index_str = element_key(shell, name, index, depth)?;

            shell
                .env_mut()
                .update_or_add_array_element(
                    name.as_str(),
                    index_str,
                    value.to_string(),
                    |_| Ok(()),
                    env::EnvironmentLookup::Anywhere,
                    env::EnvironmentScope::Global,
                )
                .map_err(|error| assignment_error(&error))?;
        }
    }

    Ok(value)
}

/// The key of `name[index]`: an associative array's subscript is its key as written, and an
/// indexed array's is evaluated arithmetically.
fn element_key(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    name: &str,
    index: &str,
    depth: u32,
) -> Result<String, EvalError> {
    let associative = shell.env().get(name).is_some_and(|(_, var)| {
        matches!(
            var.value(),
            variables::ShellValue::AssociativeArray(_)
                | variables::ShellValue::Unset(variables::ShellValueUnsetType::AssociativeArray)
        )
    });
    if associative {
        return Ok(index.to_owned());
    }
    // An error in the subscript ends the shell, as bash's does.
    let index_expr = parse(index).map_err(EvalError::in_subscript)?;
    Ok(eval_expr_impl(&index_expr, shell, depth)
        .map_err(|error| EvalError::in_expression(index, error).in_subscript())?
        .to_string())
}

/// Evaluates an indexed array's subscript, already expanded, as bash does: arithmetically, with
/// an error ending the shell.
///
/// # Arguments
///
/// * `shell` - The shell to evaluate in.
/// * `index` - The subscript's text.
///
/// # Errors
///
/// Returns an error, which ends the shell, if the subscript does not evaluate.
pub fn eval_subscript(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    index: &str,
) -> Result<i64, crate::error::Error> {
    let evaluated = parse(index).and_then(|expr| {
        expr.eval(shell)
            .map_err(|error| EvalError::in_expression(index, error))
    });
    evaluated.map_err(|error| crate::error::Error::from(error.in_subscript()))
}

/// The error for an assignment the environment refused: bash names a readonly variable.
fn assignment_error(error: &crate::error::Error) -> EvalError {
    match error.kind() {
        crate::error::ErrorKind::ReadonlyVariableNamed(name) => {
            EvalError::ReadonlyVariable(name.clone())
        }
        _ => EvalError::FailedToUpdateEnvironment,
    }
}

impl EvalError {
    /// The error in the named expression, which bash reports with the expression (without its
    /// leading blanks). An error already located in an expression, such as one in a variable's
    /// value, keeps that one.
    #[must_use]
    pub fn in_expression(expression: &str, error: Self) -> Self {
        match error {
            // Too deep an expression is not named: it would be as long.
            Self::Syntax(_)
            | Self::InExpression(..)
            | Self::NestedTooDeeply(_)
            | Self::InSubscript(_) => error,
            error => Self::InExpression(
                syntax::without_leading_blanks(expression).to_owned(),
                Box::new(error),
            ),
        }
    }

    /// The error as one in an indexed array's subscript (see [`Self::InSubscript`]). An unset
    /// variable under `set -u` stays what it is: it ends the shell anyway.
    #[must_use]
    pub fn in_subscript(self) -> Self {
        match self {
            error if error.unset_variable().is_some() => error,
            error @ Self::InSubscript(_) => error,
            error => Self::InSubscript(Box::new(error)),
        }
    }

    /// Whether the error is one in an indexed array's subscript, which ends the shell.
    pub const fn is_in_subscript(&self) -> bool {
        matches!(self, Self::InSubscript(_))
    }

    /// The variable, if the error is an unset variable under `set -u`, which ends the shell
    /// rather than failing only the command.
    pub fn unset_variable(&self) -> Option<&str> {
        match self {
            Self::ExpandingUnsetVariable(name) => Some(name),
            Self::InExpression(_, inner) | Self::InSubscript(inner) => inner.unset_variable(),
            _ => None,
        }
    }

    /// Whether the error is an assignment to a readonly variable, which bash reports on its own
    /// (`NAME: readonly variable`) rather than with the expression.
    pub fn is_readonly_variable(&self) -> bool {
        match self {
            Self::ReadonlyVariable(_) => true,
            Self::InExpression(_, inner) => inner.is_readonly_variable(),
            _ => false,
        }
    }
}

const fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

// N.B. We implement our own version of wrapping_pow that takes a 64-bit exponent.
// This seems to be the best way to guarantee that we handle overflow cases
// with exponents correctly.
const fn wrapping_pow_u64(mut base: i64, mut exponent: u64) -> i64 {
    let mut result: i64 = 1;

    while exponent > 0 {
        if exponent % 2 == 1 {
            result = result.wrapping_mul(base);
        }

        base = base.wrapping_mul(base);
        exponent /= 2;
    }

    result
}
