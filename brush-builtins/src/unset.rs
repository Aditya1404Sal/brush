use std::borrow::Cow;

use clap::Parser;

use brush_core::{
    ExecutionResult, Shell, builtins,
    variables::{ArrayLiteral, ShellValue, ShellValueLiteral, ShellValueUnsetType},
};

/// Unset a variable.
#[derive(Parser)]
pub(crate) struct UnsetCommand {
    #[clap(flatten)]
    name_interpretation: UnsetNameInterpretation,

    /// Names of variables to unset.
    names: Vec<String>,
}

#[derive(Parser)]
#[clap(group = clap::ArgGroup::new("name-interpretation").multiple(false).required(false))]
pub(crate) struct UnsetNameInterpretation {
    /// Treat each name as a shell function.
    #[arg(short = 'f', group = "name-interpretation")]
    shell_functions: bool,

    /// Treat each name as a shell variable.
    #[arg(short = 'v', group = "name-interpretation")]
    shell_variables: bool,

    /// Treat each name as a name reference.
    #[arg(short = 'n', group = "name-interpretation")]
    name_references: bool,
}

impl UnsetNameInterpretation {
    pub const fn unspecified(&self) -> bool {
        !self.shell_functions && !self.shell_variables && !self.name_references
    }
}

impl builtins::Command for UnsetCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        mut context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // `unset -n` unsets a nameref itself; plain `unset` unsets the variable it names.
        if self.name_interpretation.name_references {
            for name in &self.names {
                context.shell.env_mut().unset_raw(name)?;
            }
            return Ok(ExecutionResult::success());
        }

        let unspecified = self.name_interpretation.unspecified();
        let mut result = ExecutionResult::success();

        for name in &self.names {
            if unspecified || self.name_interpretation.shell_variables {
                // Try to parse the name as a parameter. If we can't, don't bail; it may not be a
                // valid variable name/parameter but could still be a function name.
                if let Ok(parameter) =
                    brush_parser::word::parse_parameter(name, &context.shell.parser_options())
                {
                    let (base, outcome) = match parameter {
                        brush_parser::word::Parameter::Positional(_)
                        | brush_parser::word::Parameter::Special(_) => continue,
                        brush_parser::word::Parameter::Named(name) => {
                            let outcome = context.shell.env_mut().unset(name.as_str());
                            (name, outcome.map(|unset| unset.is_some()))
                        }
                        brush_parser::word::Parameter::NamedWithIndex { name, index } => {
                            let outcome =
                                unset_array_index(context.shell, &context.params, &name, &index)
                                    .await;
                            (name, outcome)
                        }
                        // `unset 'a[@]'` empties an indexed array; an associative array's `@` and
                        // `*` are ordinary keys.
                        brush_parser::word::Parameter::NamedWithAllIndices {
                            name,
                            concatenate,
                        } => {
                            let outcome = unset_all_elements(&mut context, &name, concatenate);
                            (name, outcome)
                        }
                    };

                    match outcome {
                        Ok(true) => continue,
                        Ok(false) => (),
                        Err(error) => {
                            let message = match error.kind() {
                                brush_core::ErrorKind::ReadonlyVariable
                                | brush_core::ErrorKind::ReadonlyVariableNamed(_) => {
                                    "cannot unset: readonly variable"
                                }
                                brush_core::ErrorKind::NotArray => "not an array variable",
                                _ => return Err(error),
                            };
                            context.report(format_args!("{base}: {message}"))?;
                            result = ExecutionResult::general_error();
                            continue;
                        }
                    }
                }
            }

            if unspecified || self.name_interpretation.shell_functions {
                if context
                    .shell
                    .funcs()
                    .get(name)
                    .is_some_and(|f| f.is_readonly())
                {
                    context.report(format_args!("{name}: cannot unset: readonly function"))?;
                    result = ExecutionResult::general_error();
                    continue;
                }
                context.shell.undefine_func(name);
            }
        }

        Ok(result)
    }
}

/// Unsets every element of an indexed array, leaving it declared and empty, as
/// `unset 'a[@]'` does in bash. For an associative array `@` and `*` are keys like any other.
fn unset_all_elements(
    context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    name: &str,
    concatenate: bool,
) -> Result<bool, brush_core::Error> {
    let Some((_, var)) = context.shell.env_mut().get_mut(name) else {
        return Ok(false);
    };
    if var.is_readonly() {
        return Err(brush_core::ErrorKind::ReadonlyVariable.into());
    }
    match var.value() {
        ShellValue::AssociativeArray(_) => var.unset_index(if concatenate { "*" } else { "@" }),
        ShellValue::IndexedArray(_) | ShellValue::Unset(ShellValueUnsetType::IndexedArray) => {
            var.assign(ShellValueLiteral::Array(ArrayLiteral(vec![])), false)?;
            Ok(true)
        }
        _ => Err(brush_core::ErrorKind::NotArray.into()),
    }
}

async fn unset_array_index(
    shell: &mut Shell<impl brush_core::ShellExtensions>,
    params: &brush_core::ExecutionParameters,
    name: &str,
    index: &str,
) -> Result<bool, brush_core::Error> {
    let Some((_, var)) = shell.env().get(name) else {
        return Ok(false);
    };
    if var.is_readonly() {
        return Err(brush_core::ErrorKind::ReadonlyVariable.into());
    }
    let is_assoc_array = var.value().is_associative_array();
    let is_scalar = matches!(var.value(), ShellValue::String(_));

    // Compute which index we should actually use. An associative array's key is expanded as a
    // word, so quotes around it are removed; an indexed array's is evaluated arithmetically.
    let index_to_use: Cow<'_, str> = if is_assoc_array {
        shell.basic_expand_string(params, index).await?.into()
    } else {
        // Expanded, then evaluated; an error ends the shell, as in bash.
        let index = shell.basic_expand_string(params, index).await?;
        brush_core::arithmetic::eval_subscript(shell, &index)?
            .to_string()
            .into()
    };

    // A scalar is element 0 of itself.
    if is_scalar {
        if index_to_use == "0" {
            return Ok(shell.env_mut().unset(name)?.is_some());
        }
        return Err(brush_core::ErrorKind::NotArray.into());
    }

    // Now we can try to unset, and return the result.
    shell.env_mut().unset_index(name, index_to_use.as_ref())
}
