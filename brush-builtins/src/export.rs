use clap::Parser;
use itertools::Itertools;
use std::io::Write;

use brush_core::{
    ExecutionExitCode, ExecutionResult, builtins,
    env::{EnvironmentLookup, EnvironmentScope},
    parser::ast,
    variables,
};

/// Add or update exported shell variables.
#[derive(Parser)]
pub(crate) struct ExportCommand {
    /// Names are treated as function names.
    #[arg(short = 'f')]
    names_are_functions: bool,

    /// Un-export the names.
    #[arg(short = 'n')]
    unexport: bool,

    /// Display all exported names.
    #[arg(short = 'p')]
    display_exported_names: bool,

    //
    // Declarations
    //
    // N.B. These are skipped by clap, but filled in by the BuiltinDeclarationCommand trait.
    #[clap(skip)]
    declarations: Vec<brush_core::CommandArg>,
}

impl builtins::DeclarationCommand for ExportCommand {
    fn set_declarations(&mut self, declarations: Vec<brush_core::CommandArg>) {
        self.declarations = declarations;
    }
}

impl builtins::Command for ExportCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        mut context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if self.declarations.is_empty() {
            display_all_exported_vars(&context)?;
            return Ok(ExecutionResult::success());
        }

        let mut result = ExecutionResult::success();
        for decl in &self.declarations {
            let current_result = self.process_decl(&mut context, decl)?;
            if !current_result.is_success() {
                result = current_result;
            }
        }

        Ok(result)
    }
}

impl ExportCommand {
    fn process_decl(
        &self,
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        decl: &brush_core::CommandArg,
    ) -> Result<ExecutionResult, brush_core::Error> {
        match decl {
            // A word that expanded to `name=value` (`export $v`) assigns, as in bash.
            brush_core::CommandArg::String(s)
                if !self.names_are_functions
                    && let Some(word) = crate::declare::AssignmentText::parse(s) =>
            {
                if let Some(index) = &word.index {
                    context.report(format_args!(
                        "`{}[{index}]': not a valid identifier",
                        word.name
                    ))?;
                    return Ok(ExecutionExitCode::GeneralError.into());
                }
                self.assign(
                    context,
                    &word.name,
                    variables::ShellValueLiteral::Scalar(word.value),
                    word.append,
                )?;
            }
            brush_core::CommandArg::String(s) => {
                // See if this is supposed to be a function name.
                if self.names_are_functions {
                    // Try to find the function already present; if we find it, then mark it
                    // exported.
                    if let Some(func) = context.shell.func_mut(s) {
                        if self.unexport {
                            func.unexport();
                        } else {
                            func.export();
                        }
                    } else {
                        context.report(format_args!("{s}: not a function"))?;
                        return Ok(ExecutionExitCode::InvalidUsage.into());
                    }
                }
                // Try to find the variable already present; if we find it, then mark it
                // exported. Bash looks the name up once; a circular name reference warns.
                else if let Some((_, variable)) = {
                    context
                        .shell
                        .warn_circular_nameref(&context.params, s, 1, false);
                    context.shell.env_mut().get_mut(s)
                } {
                    if self.unexport {
                        variable.unexport();
                    } else {
                        variable.export();
                    }
                }
                // Otherwise the name is exported before it has a value, as in bash: a later
                // assignment reaches the environment.
                else if !brush_core::env::valid_variable_name(s) {
                    context.report(format_args!("`{s}': not a valid identifier"))?;
                    return Ok(ExecutionExitCode::GeneralError.into());
                } else if !self.unexport {
                    let mut variable = brush_core::ShellVariable::new(
                        brush_core::ShellValue::Unset(variables::ShellValueUnsetType::Untyped),
                    );
                    variable.export();
                    context
                        .shell
                        .env_mut()
                        .add(s.as_str(), variable, EnvironmentScope::Global)?;
                }
            }
            brush_core::CommandArg::Assignment(assignment) => {
                let name = match &assignment.name {
                    ast::AssignmentName::VariableName(name) => name,
                    ast::AssignmentName::ArrayElementName(name, index) => {
                        context
                            .report(format_args!("`{name}[{index}]': not a valid identifier"))?;
                        return Ok(ExecutionExitCode::InvalidUsage.into());
                    }
                };

                let value = match &assignment.value {
                    ast::AssignmentValue::Scalar(s) => {
                        variables::ShellValueLiteral::Scalar(s.flatten())
                    }
                    ast::AssignmentValue::Array(a) => {
                        variables::ShellValueLiteral::Array(variables::ArrayLiteral(
                            a.iter()
                                .map(|(k, v)| (k.as_ref().map(|k| k.flatten()), v.flatten()))
                                .collect(),
                        ))
                    }
                };

                self.assign(context, name, value, assignment.append)?;
            }
        }

        Ok(ExecutionResult::success())
    }

    /// Assigns `value` to `name` and marks it exported (or not, for `-n`).
    fn assign(
        &self,
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        name: &str,
        value: variables::ShellValueLiteral,
        append: bool,
    ) -> Result<(), brush_core::Error> {
        // `export name+=value` appends to the existing value, exactly like a
        // bare `name+=value`. update_or_add always replaces, so when the
        // variable already exists honor the append here. A missing variable
        // falls through: appending to nothing is a plain assignment.
        if append && let Some((_, variable)) = context.shell.env_mut().get_mut(name) {
            variable.assign(value, true)?;
            if self.unexport {
                variable.unexport();
            } else {
                variable.export();
            }
            return Ok(());
        }

        // Update the variable with the provided value and then mark it exported.
        context.shell.env_mut().update_or_add(
            name,
            value,
            |var| {
                if self.unexport {
                    var.unexport();
                } else {
                    var.export();
                }
                Ok(())
            },
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        )
    }
}

fn display_all_exported_vars(
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
) -> Result<(), brush_core::Error> {
    // Enumerate variables, sorted by key.
    for (name, variable) in context.shell.env().iter().sorted_by_key(|v| v.0) {
        if variable.is_exported() {
            let value = variable.value().try_get_cow_str(context.shell);
            if let Some(value) = value {
                writeln!(context.stdout(), "declare -x {name}=\"{value}\"")?;
            } else {
                writeln!(context.stdout(), "declare -x {name}")?;
            }
        }
    }

    Ok(())
}
