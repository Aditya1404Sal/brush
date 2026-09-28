use clap::Parser;
use itertools::Itertools;
use std::{io::Write, sync::LazyLock};

use brush_core::{
    ErrorKind, ExecutionResult, builtins,
    env::{self, EnvironmentLookup, EnvironmentScope},
    parser::ast,
    variables::{
        self, ArrayLiteral, ShellValue, ShellValueLiteral, ShellValueUnsetType, ShellVariable,
        ShellVariableUpdateTransform,
    },
};

crate::minus_or_plus_flag_arg!(
    MakeIndexedArrayFlag,
    'a',
    "Make the variable an indexed array."
);
crate::minus_or_plus_flag_arg!(
    MakeAssociativeArrayFlag,
    'A',
    "Make the variable an associative array."
);
crate::minus_or_plus_flag_arg!(
    CapitalizeValueOnAssignmentFlag,
    'c',
    "Enable capitalize-on-assignment for the variable."
);
crate::minus_or_plus_flag_arg!(MakeIntegerFlag, 'i', "Mark the variable as integer-typed");
crate::minus_or_plus_flag_arg!(
    LowercaseValueOnAssignmentFlag,
    'l',
    "Enable lowercase-on-assignment for the variable."
);
crate::minus_or_plus_flag_arg!(
    MakeNameRefFlag,
    'n',
    "Mark the variable as a name reference"
);
crate::minus_or_plus_flag_arg!(MakeReadonlyFlag, 'r', "Mark the variable as read-only.");
crate::minus_or_plus_flag_arg!(MakeTracedFlag, 't', "Enable tracing for the variable.");
crate::minus_or_plus_flag_arg!(
    UppercaseValueOnAssignmentFlag,
    'u',
    "Enable uppercase-on-assignment for the variable."
);
crate::minus_or_plus_flag_arg!(MakeExportedFlag, 'x', "Mark the variable for export.");

/// Display or update variables and their attributes.
#[derive(Parser)]
#[clap(override_usage = "declare [OPTIONS] [DECLARATIONS]...")]
pub(crate) struct DeclareCommand {
    /// Constrain to function names or definitions.
    #[arg(short = 'f')]
    function_names_or_defs_only: bool,

    /// Constrain to function names only.
    #[arg(short = 'F')]
    function_names_only: bool,

    /// Create global variable, if applicable.
    #[arg(short = 'g')]
    create_global: bool,

    /// When creating a local variable that shadows another variable of the same name,
    /// then initialize it with the contents and attributes of the variable being shadowed.
    #[arg(short = 'I')]
    locals_inherit_from_prev_scope: bool,

    /// Display each item's attributes and values.
    #[arg(short = 'p')]
    print: bool,

    //
    // Attribute options
    #[clap(flatten)] // -a
    make_indexed_array: MakeIndexedArrayFlag,
    #[clap(flatten)] // -A
    make_associative_array: MakeAssociativeArrayFlag,
    #[clap(flatten)] // -c
    capitalize_value_on_assignment: CapitalizeValueOnAssignmentFlag,
    #[clap(flatten)] // -i
    make_integer: MakeIntegerFlag,
    #[clap(flatten)] // -l
    lowercase_value_on_assignment: LowercaseValueOnAssignmentFlag,
    #[clap(flatten)] // -n
    make_nameref: MakeNameRefFlag,
    #[clap(flatten)] // -r
    make_readonly: MakeReadonlyFlag,
    #[clap(flatten)] // -t
    make_traced: MakeTracedFlag,
    #[clap(flatten)] // -u
    uppercase_value_on_assignment: UppercaseValueOnAssignmentFlag,
    #[clap(flatten)] // -x
    make_exported: MakeExportedFlag,

    //
    // Declarations
    //
    // N.B. These are skipped by clap, but filled in by the BuiltinDeclarationCommand trait.
    #[clap(skip)]
    declarations: Vec<brush_core::CommandArg>,
}

#[derive(Clone, Copy)]
enum DeclareVerb {
    Declare,
    Local,
    Readonly,
}

impl builtins::DeclarationCommand for DeclareCommand {
    fn set_declarations(&mut self, declarations: Vec<brush_core::CommandArg>) {
        self.declarations = declarations;
    }
}

impl builtins::Command for DeclareCommand {
    fn takes_plus_options() -> bool {
        true
    }

    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        mut context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let verb = match context.command_name.as_str() {
            "local" => DeclareVerb::Local,
            "readonly" => DeclareVerb::Readonly,
            _ => DeclareVerb::Declare,
        };

        if matches!(verb, DeclareVerb::Local) && !context.shell.in_function() {
            context.report("can only be used in a function")?;
            return Ok(ExecutionResult::general_error());
        }

        let mut result = ExecutionResult::success();
        // A variable cannot be both an indexed and an associative array; bash refuses `-a`.
        if !self.declarations.is_empty()
            && self.make_indexed_array.to_bool() == Some(true)
            && self.make_associative_array.to_bool() == Some(true)
        {
            context.report("-a: invalid option")?;
            return Ok(ExecutionResult::new(2));
        }
        if !self.declarations.is_empty() {
            for declaration in &self.declarations {
                if self.print && !matches!(verb, DeclareVerb::Readonly) {
                    if !self.try_display_declaration(&context, declaration, verb)? {
                        result = ExecutionResult::general_error();
                    }
                } else {
                    match self
                        .process_declaration(&mut context, declaration, verb)
                        .await
                    {
                        Ok(true) => (),
                        Ok(false) => result = ExecutionResult::general_error(),
                        // Bash names the readonly variable a declaration could not change;
                        // `readonly` itself reports it without its own name.
                        // Bash words a refused conversion with the variable's name.
                        Err(error)
                            if matches!(
                                error.kind(),
                                ErrorKind::ConvertingAssociativeArrayToIndexedArray
                                    | ErrorKind::ConvertingIndexedArrayToAssociativeArray
                            ) =>
                        {
                            let name = Self::declaration_to_name_and_value(declaration)?.0;
                            let what = if matches!(
                                error.kind(),
                                ErrorKind::ConvertingAssociativeArrayToIndexedArray
                            ) {
                                "associative to indexed"
                            } else {
                                "indexed to associative"
                            };
                            context.report(format_args!("{name}: cannot convert {what} array"))?;
                            result = ExecutionResult::general_error();
                        }
                        Err(error) if is_readonly_error(&error) => {
                            let name = Self::declaration_to_name_and_value(declaration)?.0;
                            if matches!(verb, DeclareVerb::Readonly) {
                                return Err(ErrorKind::ReadonlyVariableNamed(name).into());
                            }
                            context.report(format_args!("{name}: readonly variable"))?;
                            result = ExecutionResult::general_error();
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        } else {
            // Display matching declarations from the variable environment.
            if !self.function_names_only && !self.function_names_or_defs_only {
                self.display_matching_env_declarations(&context, verb)?;
            }

            // Do the same for functions (`readonly -f` lists the readonly ones).
            let functions_listed = if matches!(verb, DeclareVerb::Readonly) {
                self.function_names_only || self.function_names_or_defs_only
            } else {
                !self.print || self.function_names_only || self.function_names_or_defs_only
            };
            if !matches!(verb, DeclareVerb::Local) && functions_listed {
                self.display_matching_functions(&context, verb)?;
            }
        }

        Ok(result)
    }
}

impl DeclareCommand {
    fn try_display_declaration(
        &self,
        context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        declaration: &brush_core::CommandArg,
        verb: DeclareVerb,
    ) -> Result<bool, brush_core::Error> {
        let name = match declaration {
            brush_core::CommandArg::String(s) => s,
            brush_core::CommandArg::Assignment(_) => {
                context.report(format_args!("{declaration}: not found"))?;
                return Ok(false);
            }
        };

        let lookup = if matches!(verb, DeclareVerb::Local) {
            EnvironmentLookup::OnlyInCurrentLocal
        } else {
            EnvironmentLookup::Anywhere
        };

        if self.function_names_only || self.function_names_or_defs_only {
            if let Some(func_registration) = context.shell.funcs().get(name) {
                let flags = func_registration.attribute_flags();
                if self.function_names_only {
                    if self.print {
                        writeln!(context.stdout(), "declare -f{flags} {name}")?;
                    } else {
                        writeln!(context.stdout(), "{name}")?;
                    }
                } else {
                    writeln!(context.stdout(), "{}", func_registration.definition())?;
                    if self.print && !flags.is_empty() {
                        writeln!(context.stdout(), "declare -f{flags} {name}")?;
                    }
                }
                Ok(true)
            } else {
                // For some reason, bash does not print an error message in this case.
                Ok(false)
            }
        } else if let Some(variable) = context.shell.env().get_using_policy_raw(name, lookup) {
            let mut cs = variable.attribute_flags(context.shell);
            if cs.is_empty() {
                cs.push('-');
            }

            let resolved_value = variable.resolve_value(context.shell);
            let separator_str = if matches!(resolved_value, ShellValue::Unset(_)) {
                ""
            } else {
                "="
            };

            writeln!(
                context.stdout(),
                "declare -{cs} {name}{separator_str}{}",
                resolved_value.format(variables::FormatStyle::DeclarePrint, context.shell)?
            )?;

            Ok(true)
        } else {
            context.report(format_args!("{name}: not found"))?;
            Ok(false)
        }
    }

    /// `declare -f` with attribute flags applies them to the named function rather than
    /// displaying it (e.g. `declare -ft name`, `declare -fx name`).
    fn apply_function_attributes(
        &self,
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        declaration: &brush_core::CommandArg,
        verb: DeclareVerb,
    ) -> bool {
        let func = match declaration {
            brush_core::CommandArg::String(name) => context.shell.func_mut(name),
            brush_core::CommandArg::Assignment(_) => None,
        };

        // As with display, bash reports failure without printing an error message here, except
        // for `readonly -f`, which names the missing function.
        let Some(func) = func else {
            if matches!(verb, DeclareVerb::Readonly)
                && let brush_core::CommandArg::String(name) = declaration
            {
                let _ = context.report(format_args!("{name}: not a function"));
            }
            return false;
        };

        match self.make_exported.to_bool() {
            Some(true) => func.export(),
            Some(false) => func.unexport(),
            None => (),
        }
        if matches!(verb, DeclareVerb::Readonly) || self.make_readonly.to_bool() == Some(true) {
            func.set_readonly();
        }

        // TODO(declare): function tracing (-t) isn't tracked; it's accepted silently.
        true
    }

    async fn process_declaration(
        &self,
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        declaration: &brush_core::CommandArg,
        verb: DeclareVerb,
    ) -> Result<bool, brush_core::Error> {
        let create_var_local = matches!(verb, DeclareVerb::Local)
            || (matches!(verb, DeclareVerb::Declare)
                && context.shell.in_function()
                && !self.create_global);

        if (self.function_names_or_defs_only || self.function_names_only)
            && (self.make_traced.to_bool().is_some()
                || self.make_exported.to_bool().is_some()
                || self.make_readonly.to_bool() == Some(true)
                || matches!(verb, DeclareVerb::Readonly))
        {
            return Ok(self.apply_function_attributes(context, declaration, verb));
        }

        if self.function_names_or_defs_only || self.function_names_only {
            return self.try_display_declaration(context, declaration, verb);
        }

        // Extract the variable name and the initial value being assigned (if any).
        let (mut name, assigned_index, initial_value, name_is_array, append) =
            Self::declaration_to_name_and_value(declaration)?;

        // `local r=value` for a name reference already local here declares the variable it
        // refers to (`local -n r=x; local r=2` makes a local `x`), as bash does; one that comes
        // back to itself takes the value as the name it should refer to, which is refused.
        if create_var_local
            && self.make_nameref.to_bool().is_none()
            && assigned_index.is_none()
            && context
                .shell
                .env()
                .get_using_policy_raw(&name, EnvironmentLookup::OnlyInCurrentLocal)
                .is_some_and(ShellVariable::is_treated_as_nameref)
            && let Some(value) = &initial_value
        {
            if context.shell.env().circular_nameref(&name).is_some() {
                context
                    .shell
                    .warn_circular_nameref(&context.params, &name, 2, false);
                if let ShellValueLiteral::Scalar(value) = value {
                    context.report(format_args!(
                        "`{value}': invalid variable name for name reference"
                    ))?;
                }
                return Ok(false);
            }
            let target = context.shell.env().resolve_nameref(&name).into_owned();
            if env::valid_variable_name(&target) {
                name = target;
            }
        }

        // `readonly` takes names, not array elements, and an element named without a value
        // needs a subscript, as in bash.
        match (verb, &assigned_index) {
            (DeclareVerb::Readonly, Some(index)) => {
                context.report(format_args!("`{name}[{index}]': not a valid identifier"))?;
                return Ok(false);
            }
            (_, Some(index)) if index.is_empty() && initial_value.is_none() => {
                context.report(format_args!("`{name}[]': not a valid identifier"))?;
                return Ok(false);
            }
            _ => (),
        }

        // A readonly variable is refused before its new value is evaluated, and an array cannot
        // lose its array attribute (`declare +a`), as in bash.
        let current_lookup = if create_var_local {
            EnvironmentLookup::OnlyInCurrentLocal
        } else {
            EnvironmentLookup::Anywhere
        };
        if let Some(var) = self.existing_variable(context.shell, name.as_str(), current_lookup) {
            if var.is_readonly() && initial_value.is_some() {
                return Err(ErrorKind::ReadonlyVariable.into());
            }
            let removes_array = self.make_indexed_array.to_bool() == Some(false)
                || self.make_associative_array.to_bool() == Some(false);
            if removes_array && var.value().is_array() {
                context.report(format_args!(
                    "{name}: cannot destroy array variables in this way"
                ))?;
                return Ok(false);
            }
        }

        // An indexed array's subscripts, in a compound assignment or an element's
        // (`declare 'a[i+1]=v'`), are evaluated arithmetically (an associative array's are
        // words). In a compound assignment, one that is empty or counts back past the start
        // fails the declaration once the elements before it are assigned, abandoning the
        // command. An element's that names no element (`declare 'a[-5]=v'`, or an empty one,
        // `declare 'm[]=v'`, for an associative array too) fails the declaration as `a[-5]=v`
        // would, and the variable is still declared, with its attributes: kept if it exists,
        // otherwise a new empty array (unset for a local), as in bash.
        let mut outcome = Ok(true);
        let associative = self.assigns_associative(context, &name, current_lookup);
        let mut bad_element = false;
        // A new global array is made empty; an existing variable or a new local is left as is.
        let no_element = if create_var_local
            || self
                .existing_variable(context.shell, name.as_str(), current_lookup)
                .is_some()
        {
            None
        } else {
            Some(ShellValueLiteral::Array(ArrayLiteral(vec![])))
        };
        // An associative array has no element with an empty key: bash names the element with
        // its key and value quoted, and abandons the command.
        if associative
            && assigned_index.is_none()
            && let Some(ShellValueLiteral::Array(ArrayLiteral(elements))) = &initial_value
            && let Some((_, value)) = elements.iter().find(|(key, _)| key.as_deref() == Some(""))
        {
            let value = value.replace('\'', "'\\''");
            return Err(ErrorKind::BadArrayElement(std::format!("['']='{value}'")).into());
        }
        let initial_value = match initial_value {
            Some(ShellValueLiteral::Array(_))
                if associative && assigned_index.as_deref() == Some("") =>
            {
                writeln!(
                    context.stderr(),
                    "{}{name}[]: bad array subscript",
                    context.shell.diagnostic_prefix()
                )?;
                outcome = Ok(false);
                bad_element = true;
                no_element
            }
            Some(ShellValueLiteral::Array(literal)) if !associative => {
                // An element is assigned into the array as it stands.
                let existing = self
                    .existing_variable(context.shell, name.as_str(), current_lookup)
                    .map(|var| var.value());
                let keys = brush_core::variables::IndexedLiteralKeys::new(
                    existing,
                    append || assigned_index.is_some(),
                );
                let (literal, failed) = brush_core::arithmetic::resolve_indexed_array_literal(
                    context.shell,
                    &context.params,
                    keys,
                    literal,
                )
                .await?;
                match (failed, &assigned_index) {
                    (Some(_), Some(index)) => {
                        writeln!(
                            context.stderr(),
                            "{}{name}[{index}]: bad array subscript",
                            context.shell.diagnostic_prefix()
                        )?;
                        outcome = Ok(false);
                        bad_element = true;
                        no_element
                    }
                    (failed, _) => {
                        if let Some(failed) = failed {
                            outcome = Err(failed);
                        }
                        Some(ShellValueLiteral::Array(literal))
                    }
                }
            }
            // Once an associative array's first element has a subscript, every element needs one.
            Some(ShellValueLiteral::Array(ArrayLiteral(elements)))
                if assigned_index.is_none()
                    && elements.first().is_some_and(|(key, _)| key.is_some())
                    && elements.iter().any(|(key, _)| key.is_none()) =>
            {
                let word = elements
                    .iter()
                    .find(|(key, _)| key.is_none())
                    .map(|(_, word)| std::format!("'{}'", word.replace('\'', "'\\''")))
                    .unwrap_or_default();
                let kind = ErrorKind::AssocSubscriptRequired(name.clone(), word);
                return Err(brush_core::Error::from(kind).into_fatal());
            }
            value => value,
        };

        // What bash refuses to make a name reference, before it evaluates an integer's value.
        if self.make_nameref.to_bool() == Some(true) && assigned_index.is_none() {
            // With `-i`, bash makes nothing, and says nothing.
            if self.make_integer.to_bool() == Some(true) {
                return Ok(false);
            }
            // An array cannot be one: a new value is assigned as an indexed array's.
            let existing_array = self
                .existing_variable(context.shell, name.as_str(), current_lookup)
                .is_some_and(|var| var.value().is_array());
            if existing_array || matches!(initial_value, Some(ShellValueLiteral::Array(_))) {
                context.report(format_args!(
                    "{name}: reference variable cannot be an array"
                ))?;
                if let Some(value @ ShellValueLiteral::Array(_)) = initial_value {
                    let scope = if create_var_local {
                        EnvironmentScope::Local
                    } else {
                        EnvironmentScope::Global
                    };
                    context.shell.env_mut().update_or_add(
                        name.as_str(),
                        value,
                        |_| Ok(()),
                        current_lookup,
                        scope,
                    )?;
                }
                return Ok(false);
            }
            // Nor can it refer to no name; a new local is still declared, plainly.
            if matches!(&initial_value, Some(ShellValueLiteral::Scalar(target)) if target.is_empty())
            {
                context.report("`': not a valid identifier")?;
                if create_var_local
                    && self
                        .existing_variable(context.shell, name.as_str(), current_lookup)
                        .is_none()
                {
                    let var = ShellVariable::new(ShellValue::Unset(ShellValueUnsetType::Untyped));
                    context
                        .shell
                        .env_mut()
                        .add(name, var, EnvironmentScope::Local)?;
                }
                return Ok(false);
            }
        }
        let initial_value = self.evaluate_if_integer(context, &name, initial_value)?;

        // Special-case: `local -` saves the `set` options, to restore when the function returns.
        if name == "-" && matches!(verb, DeclareVerb::Local) {
            context.shell.save_options_locally();
            return Ok(true);
        }

        // Make sure it's a valid name.
        if !env::valid_variable_name(name.as_str()) {
            context.report(format_args!("`{name}': not a valid identifier"))?;
            return Ok(false);
        }

        // `declare -r 'a[i]=v'`: bash makes the array readonly before it assigns the element, so
        // the assignment fails ("a: readonly variable") and the declaration still succeeds: the
        // array keeps what it held, or is a new empty one (declared but unset for a local).
        if assigned_index.is_some()
            && !bad_element
            && initial_value.is_some()
            && self.make_readonly.to_bool() == Some(true)
        {
            writeln!(
                context.stderr(),
                "{}{name}: readonly variable",
                context.shell.diagnostic_prefix()
            )?;
            if let Some(var) = self.existing_variable(context.shell, name.as_str(), current_lookup)
            {
                self.apply_attributes_before_update(var)?;
                self.apply_attributes_after_update(var, verb)?;
            } else {
                let (value, scope) = if create_var_local {
                    (
                        ShellValue::Unset(ShellValueUnsetType::IndexedArray),
                        EnvironmentScope::Local,
                    )
                } else {
                    (
                        ShellValue::indexed_array_from_literals(ArrayLiteral(vec![])),
                        EnvironmentScope::Global,
                    )
                };
                let mut var = ShellVariable::new(value);
                self.apply_attributes_before_update(&mut var)?;
                self.apply_attributes_after_update(&mut var, verb)?;
                context.shell.env_mut().add(name, var, scope)?;
            }
            return Ok(true);
        }

        // A nameref must name a variable or an element of one.
        if self.make_nameref.to_bool() == Some(true)
            && let Some(ShellValueLiteral::Scalar(target)) = &initial_value
        {
            let base = target
                .split_once('[')
                .filter(|(_, rest)| rest.ends_with(']'))
                .map_or(target.as_str(), |(base, _)| base);
            if !target.is_empty() && !env::valid_variable_name(base) {
                context.report(format_args!(
                    "`{target}': invalid variable name for name reference"
                ))?;
                return Ok(false);
            }
        }

        // A nameref to itself is refused at the top level; in a function bash only warns.
        if self.make_nameref.to_bool() == Some(true)
            && matches!(&initial_value, Some(ShellValueLiteral::Scalar(target)) if *target == name)
        {
            if !create_var_local {
                context.report(format_args!(
                    "{name}: nameref variable self references not allowed"
                ))?;
                return Ok(false);
            }
            context.report(format_args!("warning: {name}: circular name reference"))?;
            writeln!(
                context.stderr(),
                "{}warning: {name}: circular name reference",
                context.shell.diagnostic_prefix()
            )?;
        }

        // Figure out where we should look.
        let lookup = if create_var_local {
            EnvironmentLookup::OnlyInCurrentLocal
        } else {
            EnvironmentLookup::Anywhere
        };

        // `local -I x[=v]` / `declare -I` (bash 5.0+): the new local inherits
        // value and attributes from the nearest same-name variable in an
        // enclosing scope instead of starting unset; `+=` appends to the
        // inherited value. With no same-name variable anywhere, fall through
        // to ordinary creation.
        // `shopt -s localvar_inherit` makes every new local do the same.
        if (self.locals_inherit_from_prev_scope
            || context.shell.options().local_vars_inherit_value_and_attrs)
            && create_var_local
        {
            let inherited = context
                .shell
                .env()
                .get_using_policy(name.as_str(), EnvironmentLookup::Anywhere)
                .cloned();

            if let Some(mut var) = inherited {
                self.apply_attributes_before_update(&mut var)?;

                if let Some(initial_value) = initial_value {
                    assign_declared(&mut var, initial_value, assigned_index.is_some(), append)?;
                }

                if context.shell.options().export_variables_on_modification
                    && !var.value().is_array()
                {
                    var.export();
                }

                self.apply_attributes_after_update(&mut var, verb)?;

                context
                    .shell
                    .env_mut()
                    .add(name, var, EnvironmentScope::Local)?;
                return outcome;
            }
        }

        // A local cannot shadow a readonly global, as bash refuses.
        if create_var_local
            && self
                .existing_variable(context.shell, name.as_str(), lookup)
                .is_none()
            && context
                .shell
                .env()
                .get_using_policy(name.as_str(), EnvironmentLookup::OnlyInGlobal)
                .is_some_and(ShellVariable::is_readonly)
        {
            return Err(ErrorKind::ReadonlyVariable.into());
        }

        // Bash looks a name given no value up once for `readonly` and twice for `declare -x`; a
        // circular name reference warns at each.
        if initial_value.is_none() {
            let lookups = match verb {
                DeclareVerb::Readonly => 1,
                _ if self.make_exported.to_bool() == Some(true) => 2,
                _ => 0,
            };
            context
                .shell
                .warn_circular_nameref(&context.params, &name, lookups, false);
        }

        // Look up the variable.
        if let Some(var) = self.existing_variable(context.shell, name.as_str(), lookup) {
            // A readonly variable keeps its value and its type.
            if var.is_readonly() && (initial_value.is_some() || self.changes_type()) {
                return Err(ErrorKind::ReadonlyVariable.into());
            }
            // `+a` and `+A` convert nothing.
            if self.make_associative_array.to_bool() == Some(true) {
                var.convert_to_associative_array()?;
            }
            if self.make_indexed_array.to_bool() == Some(true) {
                var.convert_to_indexed_array()?;
            }

            self.apply_attributes_before_update(var)?;

            if let Some(initial_value) = initial_value {
                assign_declared(var, initial_value, assigned_index.is_some(), append)?;
            }

            self.apply_attributes_after_update(var, verb)?;
        } else {
            let unset_type = if self.make_indexed_array.to_bool() == Some(true) {
                ShellValueUnsetType::IndexedArray
            } else if self.make_associative_array.to_bool() == Some(true) {
                ShellValueUnsetType::AssociativeArray
            } else if name_is_array {
                ShellValueUnsetType::IndexedArray
            } else {
                ShellValueUnsetType::Untyped
            };

            let mut var = ShellVariable::new(ShellValue::Unset(unset_type));

            self.apply_attributes_before_update(&mut var)?;

            if let Some(initial_value) = initial_value {
                assign_declared(&mut var, initial_value, assigned_index.is_some(), append)?;
            }

            if context.shell.options().export_variables_on_modification && !var.value().is_array() {
                var.export();
            }

            self.apply_attributes_after_update(&mut var, verb)?;

            let scope = if create_var_local {
                EnvironmentScope::Local
            } else {
                EnvironmentScope::Global
            };

            context.shell.env_mut().add(name, var, scope)?;
        }

        outcome
    }

    /// Whether the declaration changes what the variable holds (`-aAilu` and `-c`, or their `+`
    /// forms), which bash refuses for a readonly variable.
    const fn changes_type(&self) -> bool {
        self.make_indexed_array.is_some()
            || self.make_associative_array.is_some()
            || self.make_integer.to_bool().is_some()
            || self.lowercase_value_on_assignment.to_bool().is_some()
            || self.uppercase_value_on_assignment.to_bool().is_some()
            || self.capitalize_value_on_assignment.to_bool().is_some()
    }

    /// The variable a declaration updates. `-n` and `+n` change a nameref itself, not the
    /// variable it names.
    fn existing_variable<'a>(
        &self,
        shell: &'a mut brush_core::Shell<impl brush_core::ShellExtensions>,
        name: &str,
        lookup: EnvironmentLookup,
    ) -> Option<&'a mut ShellVariable> {
        if self.make_nameref.to_bool().is_some() {
            shell.env_mut().get_mut_using_policy_raw(name, lookup)
        } else {
            shell.env_mut().get_mut_using_policy(name, lookup)
        }
    }

    /// Whether this declaration assigns to an associative array: one it makes (`-A`), or one
    /// that already is.
    fn assigns_associative(
        &self,
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        name: &str,
        lookup: EnvironmentLookup,
    ) -> bool {
        match self.make_associative_array.to_bool() {
            Some(associative) => associative,
            None => self
                .existing_variable(context.shell, name, lookup)
                .is_some_and(|var| {
                    matches!(
                        var.value(),
                        ShellValue::AssociativeArray(_)
                            | ShellValue::Unset(ShellValueUnsetType::AssociativeArray)
                    )
                }),
        }
    }

    /// A value assigned to an integer variable (already one, or made one by this declaration)
    /// is evaluated arithmetically, as in bash, where a value that does not evaluate ends the
    /// shell.
    fn evaluate_if_integer(
        &self,
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        name: &str,
        value: Option<ShellValueLiteral>,
    ) -> Result<Option<ShellValueLiteral>, brush_core::Error> {
        let becomes_integer = match self.make_integer.to_bool() {
            Some(integer) => integer,
            None => context
                .shell
                .env()
                .get(name)
                .is_some_and(|(_, var)| var.is_treated_as_integer()),
        };
        match value {
            Some(value) if becomes_integer => Ok(Some(
                brush_core::arithmetic::eval_integer_literal(context.shell, &context.params, value)
                    .map_err(|error| brush_core::Error::from(error).into_fatal())?,
            )),
            value => Ok(value),
        }
    }

    #[expect(clippy::type_complexity)]
    fn declaration_to_name_and_value(
        declaration: &brush_core::CommandArg,
    ) -> Result<
        (
            String,
            Option<String>,
            Option<ShellValueLiteral>,
            bool,
            bool,
        ),
        brush_core::Error,
    > {
        let name;
        let assigned_index;
        let initial_value;
        let name_is_array;
        let append;

        match declaration {
            // A word that expanded to `name=value` (`declare $v`, `declare v$i=x`) assigns, as
            // in bash.
            brush_core::CommandArg::String(s) if let Some(word) = AssignmentText::parse(s) => {
                name = word.name;
                append = word.append;
                if let Some(index) = word.index {
                    initial_value = Some(ShellValueLiteral::Array(ArrayLiteral(vec![(
                        Some(index.clone()),
                        word.value,
                    )])));
                    assigned_index = Some(index);
                    name_is_array = true;
                } else {
                    initial_value = Some(ShellValueLiteral::Scalar(word.value));
                    assigned_index = None;
                    name_is_array = false;
                }
            }
            brush_core::CommandArg::String(s) => {
                // We need to handle the case of someone invoking `declare array[index]`.
                // In such case, we ignore the index and treat it as a declaration of
                // the array.
                #[allow(
                    clippy::unwrap_in_result,
                    clippy::unwrap_used,
                    reason = "regex is valid and should not fail"
                )]
                static ARRAY_AND_INDEX_RE: LazyLock<fancy_regex::Regex> =
                    LazyLock::new(|| fancy_regex::Regex::new(r"^(.*?)\[(.*?)\]$").unwrap());

                if let Some(captures) = ARRAY_AND_INDEX_RE.captures(s)? {
                    name = captures
                        .get(1)
                        .ok_or_else(|| {
                            brush_core::ErrorKind::InternalError("declaration parse error".into())
                        })?
                        .as_str()
                        .to_owned();

                    assigned_index = captures.get(2).map(|m| m.as_str().to_owned());
                    name_is_array = true;
                } else {
                    name = s.clone();
                    assigned_index = None;
                    name_is_array = false;
                }
                initial_value = None;
                append = false;
            }
            brush_core::CommandArg::Assignment(assignment) => {
                match &assignment.name {
                    ast::AssignmentName::VariableName(var_name) => {
                        name = var_name.to_owned();
                        assigned_index = None;
                    }
                    ast::AssignmentName::ArrayElementName(var_name, index) => {
                        if matches!(assignment.value, ast::AssignmentValue::Array(_)) {
                            return Err(ErrorKind::AssigningListToArrayMember.into());
                        }

                        name = var_name.to_owned();
                        assigned_index = Some(index.to_owned());
                    }
                }

                append = assignment.append;

                match &assignment.value {
                    ast::AssignmentValue::Scalar(s) => {
                        if let Some(index) = &assigned_index {
                            initial_value = Some(ShellValueLiteral::Array(ArrayLiteral(vec![(
                                Some(index.to_owned()),
                                s.value.clone(),
                            )])));
                            name_is_array = true;
                        } else {
                            initial_value = Some(ShellValueLiteral::Scalar(s.value.clone()));
                            name_is_array = false;
                        }
                    }
                    ast::AssignmentValue::Array(a) => {
                        initial_value = Some(ShellValueLiteral::Array(ArrayLiteral(
                            a.iter()
                                .map(|(i, v)| {
                                    (i.as_ref().map(|w| w.value.clone()), v.value.clone())
                                })
                                .collect(),
                        )));
                        name_is_array = true;
                    }
                }
            }
        }

        Ok((name, assigned_index, initial_value, name_is_array, append))
    }

    /// Whether an attribute option was given, `-x` or `+x`.
    fn attribute_given(&self) -> bool {
        [
            self.make_indexed_array.to_bool(),
            self.make_associative_array.to_bool(),
            self.capitalize_value_on_assignment.to_bool(),
            self.make_integer.to_bool(),
            self.lowercase_value_on_assignment.to_bool(),
            self.make_nameref.to_bool(),
            self.make_readonly.to_bool(),
            self.make_traced.to_bool(),
            self.uppercase_value_on_assignment.to_bool(),
            self.make_exported.to_bool(),
        ]
        .iter()
        .any(Option::is_some)
    }

    fn display_matching_env_declarations(
        &self,
        context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        verb: DeclareVerb,
    ) -> Result<(), brush_core::Error> {
        //
        // Dump all declarations. Use attribute flags to filter which variables are dumped.
        //

        // We start by excluding all variables that are not enumerable.
        #[expect(clippy::type_complexity)]
        let mut filters: Vec<Box<dyn Fn((&String, &ShellVariable)) -> bool>> =
            vec![Box::new(|(_, v)| v.is_enumerable())];

        // Add filters depending on verb.
        if matches!(verb, DeclareVerb::Readonly) {
            filters.push(Box::new(|(_, v)| v.is_readonly()));
        }

        // Add filters depending on attribute flags.
        if let Some(value) = self.make_indexed_array.to_bool() {
            filters.push(Box::new(move |(_, v)| {
                matches!(v.value(), ShellValue::IndexedArray(_)) == value
            }));
        }
        if let Some(value) = self.make_associative_array.to_bool() {
            filters.push(Box::new(move |(_, v)| {
                matches!(v.value(), ShellValue::AssociativeArray(_)) == value
            }));
        }
        if let Some(value) = self.make_integer.to_bool() {
            filters.push(Box::new(move |(_, v)| v.is_treated_as_integer() == value));
        }
        if let Some(value) = self.capitalize_value_on_assignment.to_bool() {
            filters.push(Box::new(move |(_, v)| {
                matches!(
                    v.get_update_transform(),
                    ShellVariableUpdateTransform::Capitalize
                ) == value
            }));
        }
        if let Some(value) = self.lowercase_value_on_assignment.to_bool() {
            filters.push(Box::new(move |(_, v)| {
                matches!(
                    v.get_update_transform(),
                    ShellVariableUpdateTransform::Lowercase
                ) == value
            }));
        }
        if let Some(value) = self.make_nameref.to_bool() {
            filters.push(Box::new(move |(_, v)| v.is_treated_as_nameref() == value));
        }
        if let Some(value) = self.make_readonly.to_bool() {
            filters.push(Box::new(move |(_, v)| v.is_readonly() == value));
        }
        if let Some(value) = self.make_traced.to_bool() {
            filters.push(Box::new(move |(_, v)| v.is_trace_enabled() == value));
        }
        if let Some(value) = self.uppercase_value_on_assignment.to_bool() {
            filters.push(Box::new(move |(_, v)| {
                matches!(
                    v.get_update_transform(),
                    ShellVariableUpdateTransform::Uppercase
                ) == value
            }));
        }
        if let Some(value) = self.make_exported.to_bool() {
            filters.push(Box::new(move |(_, v)| v.is_exported() == value));
        }

        let iter_policy = if matches!(verb, DeclareVerb::Local) {
            EnvironmentLookup::OnlyInCurrentLocal
        } else {
            EnvironmentLookup::Anywhere
        };

        // Iterate through an ordered list of all matching declarations tracked in the
        // environment.
        for (name, variable) in context
            .shell
            .env()
            .iter_using_policy(iter_policy)
            .filter(|pair| filters.iter().all(|f| f(*pair)))
            .sorted_by_key(|v| v.0)
        {
            // With attributes to match, bash lists the variables as `declare -p` does, and
            // `local` as `local -p` does; `declare` alone lists `name=value`.
            if self.print || self.attribute_given() || matches!(verb, DeclareVerb::Local) {
                let mut cs = variable.attribute_flags(context.shell);
                if cs.is_empty() {
                    cs.push('-');
                }

                let separator_str = if matches!(variable.value(), ShellValue::Unset(_)) {
                    ""
                } else {
                    "="
                };

                writeln!(
                    context.stdout(),
                    "declare -{cs} {name}{separator_str}{}",
                    variable
                        .value()
                        .format(variables::FormatStyle::DeclarePrint, context.shell)?
                )?;
            } else {
                writeln!(
                    context.stdout(),
                    "{name}={}",
                    variable
                        .value()
                        .format(variables::FormatStyle::Basic, context.shell)?
                )?;
            }
        }

        Ok(())
    }

    fn display_matching_functions(
        &self,
        context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        verb: DeclareVerb,
    ) -> Result<(), brush_core::Error> {
        let readonly_only =
            matches!(verb, DeclareVerb::Readonly) || self.make_readonly.to_bool() == Some(true);
        let exported_only = self.make_exported.to_bool() == Some(true);
        for (name, registration) in context
            .shell
            .funcs()
            .iter()
            .filter(|(_, f)| !readonly_only || f.is_readonly())
            .filter(|(_, f)| !exported_only || f.is_exported())
            .sorted_by_key(|v| v.0)
        {
            // Bash follows a function with the attributes it has, as `declare -f` would set them.
            let flags = registration.attribute_flags();
            if self.function_names_only {
                writeln!(context.stdout(), "declare -f{flags} {name}")?;
            } else {
                writeln!(context.stdout(), "{}", registration.definition())?;
                if !flags.is_empty() {
                    writeln!(context.stdout(), "declare -f{flags} {name}")?;
                }
            }
        }

        Ok(())
    }

    #[expect(clippy::unnecessary_wraps)]
    const fn apply_attributes_before_update(
        &self,
        var: &mut ShellVariable,
    ) -> Result<(), brush_core::Error> {
        if let Some(value) = self.make_integer.to_bool() {
            if value {
                var.treat_as_integer();
            } else {
                var.unset_treat_as_integer();
            }
        }
        if let Some(value) = self.capitalize_value_on_assignment.to_bool() {
            if value {
                var.set_update_transform(ShellVariableUpdateTransform::Capitalize);
            } else if matches!(
                var.get_update_transform(),
                ShellVariableUpdateTransform::Capitalize
            ) {
                var.set_update_transform(ShellVariableUpdateTransform::None);
            }
        }
        if let Some(value) = self.lowercase_value_on_assignment.to_bool() {
            if value {
                var.set_update_transform(ShellVariableUpdateTransform::Lowercase);
            } else if matches!(
                var.get_update_transform(),
                ShellVariableUpdateTransform::Lowercase
            ) {
                var.set_update_transform(ShellVariableUpdateTransform::None);
            }
        }
        if let Some(value) = self.make_nameref.to_bool() {
            if value {
                var.treat_as_nameref();
            } else {
                var.unset_treat_as_nameref();
            }
        }
        if let Some(value) = self.make_traced.to_bool() {
            if value {
                var.enable_trace();
            } else {
                var.disable_trace();
            }
        }
        if let Some(value) = self.uppercase_value_on_assignment.to_bool() {
            if value {
                var.set_update_transform(ShellVariableUpdateTransform::Uppercase);
            } else if matches!(
                var.get_update_transform(),
                ShellVariableUpdateTransform::Uppercase
            ) {
                var.set_update_transform(ShellVariableUpdateTransform::None);
            }
        }
        // Bash turns on no case conversion when asked for more than one (`declare -lu`).
        let capitalize = matches!(self.capitalize_value_on_assignment.to_bool(), Some(true));
        let lowercase = matches!(self.lowercase_value_on_assignment.to_bool(), Some(true));
        let uppercase = matches!(self.uppercase_value_on_assignment.to_bool(), Some(true));
        if (capitalize && (lowercase || uppercase)) || (lowercase && uppercase) {
            var.set_update_transform(ShellVariableUpdateTransform::None);
        }
        if let Some(value) = self.make_exported.to_bool() {
            if value {
                var.export();
            } else {
                var.unexport();
            }
        }

        Ok(())
    }

    fn apply_attributes_after_update(
        &self,
        var: &mut ShellVariable,
        verb: DeclareVerb,
    ) -> Result<(), brush_core::Error> {
        if matches!(verb, DeclareVerb::Readonly) {
            var.set_readonly();
        } else if let Some(value) = self.make_readonly.to_bool() {
            if value {
                var.set_readonly();
            } else {
                var.unset_readonly()?;
            }
        }

        Ok(())
    }
}

/// Whether the error is an attempt to change a readonly variable.
const fn is_readonly_error(error: &brush_core::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::ReadonlyVariable | ErrorKind::ReadonlyVariableNamed(_)
    )
}

/// A declaration builtin's argument that expanded to `name=value`, `name+=value` or
/// `name[index]=value`, which bash treats as an assignment.
pub(crate) struct AssignmentText {
    pub(crate) name: String,
    pub(crate) index: Option<String>,
    pub(crate) append: bool,
    pub(crate) value: String,
}

impl AssignmentText {
    /// The assignment `text` spells, or `None` when it is not one: no `=`, or no valid name
    /// before it.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (target, value) = text.split_once('=')?;
        let (target, append) = match target.strip_suffix('+') {
            Some(target) => (target, true),
            None => (target, false),
        };
        let (name, index) = match target.strip_suffix(']').and_then(|t| t.split_once('[')) {
            Some((name, index)) => (name, Some(index.to_owned())),
            None => (target, None),
        };
        env::valid_variable_name(name).then(|| Self {
            name: name.to_owned(),
            index,
            append,
            value: value.to_owned(),
        })
    }
}
/// Assigns a declaration's value. An element (`a[i]=v`) is assigned as `a[i]=v` would be, so
/// `a[i]+=v` appends to the element (adds, for an integer), as in bash; any other value is the
/// variable's whole value, appended to for `+=` or an element's declaration that assigns none.
fn assign_declared(
    var: &mut ShellVariable,
    value: ShellValueLiteral,
    element: bool,
    append: bool,
) -> Result<(), brush_core::Error> {
    match value {
        ShellValueLiteral::Array(ArrayLiteral(elements)) if element && elements.len() == 1 => {
            let Some((key, value)) = elements.into_iter().next() else {
                return Ok(());
            };
            var.assign_at_index(key.unwrap_or_default(), value, append)
        }
        value => var.assign(value, append || element),
    }
}
