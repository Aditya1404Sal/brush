//! Environment support for shell.

use std::borrow::Cow;

use crate::{ShellVariable, error};

impl<SE: crate::extensions::ShellExtensions> crate::Shell<SE> {
    /// Tries to retrieve a variable from the shell's environment, converting it into its
    /// string form.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the variable to retrieve.
    pub fn env_str(&self, name: &str) -> Option<Cow<'_, str>> {
        self.env.get_str(name, self)
    }

    /// Tries to retrieve a variable from the shell's environment.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the variable to retrieve.
    pub fn env_var(&self, name: &str) -> Option<&ShellVariable> {
        self.env.get(name).map(|(_, var)| var)
    }

    /// Writes the warnings bash writes when `name` is a circular name reference (`local -n v=v`)
    /// and it looks the name up `lookups` times, then, when `binds`, assigns it: "circular name
    /// reference" for each lookup, then "maximum nameref depth (8) exceeded". Nothing when `name`
    /// is no such reference.
    pub fn warn_circular_nameref(
        &self,
        params: &crate::ExecutionParameters,
        name: &str,
        lookups: usize,
        binds: bool,
    ) {
        use std::io::Write as _;
        if let Some(text) = self.circular_nameref_warnings(name, lookups, binds) {
            let _ = params.stderr(self).write_all(text.as_bytes());
        }
    }

    /// Holds the warnings [`Self::warn_circular_nameref`] would write for `name`, while an
    /// arithmetic expression is evaluated (see `nameref_warnings`).
    pub(crate) fn note_circular_nameref(&mut self, name: &str, lookups: usize, binds: bool) {
        if self.nameref_warnings.is_none() {
            return;
        }
        if let Some(text) = self.circular_nameref_warnings(name, lookups, binds)
            && let Some(warnings) = &mut self.nameref_warnings
        {
            warnings.push_str(&text);
        }
    }

    /// The text of the warnings [`Self::warn_circular_nameref`] writes, or `None` when `name` is
    /// no circular name reference.
    fn circular_nameref_warnings(&self, name: &str, lookups: usize, binds: bool) -> Option<String> {
        use std::fmt::Write as _;
        self.env.circular_nameref(name)?;
        let prefix = self.diagnostic_prefix();
        let mut text = String::new();
        for _ in 0..lookups {
            let _ = writeln!(text, "{prefix}warning: {name}: circular name reference");
        }
        if binds {
            let _ = writeln!(
                text,
                "{prefix}warning: {name}: maximum nameref depth (8) exceeded"
            );
        }
        Some(text)
    }

    /// Tries to set a global variable in the shell's environment.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the variable to add.
    /// * `var` - The variable contents to add.
    pub fn set_env_global(&mut self, name: &str, var: ShellVariable) -> Result<(), error::Error> {
        self.env.set_global(name, var)
    }
}
