//! Builtin command management for shell instances.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::{builtins, extensions};

/// The directories a Linux system keeps its programs in, where the builtins that stand for
/// programs are found (see [`crate::Shell::set_program_names`]); the first is where they are
/// reported to be.
const PROGRAM_DIRS: [&str; 2] = ["/bin", "/usr/bin"];

impl<SE: extensions::ShellExtensions> crate::Shell<SE> {
    /// Declares which builtins stand for programs a Linux system has as files (`cat`, `sed`,
    /// `echo`, ...), as opposed to shell-language builtins (`cd`, `export`): naming one by its
    /// path in [`PROGRAM_DIRS`] (`/bin/cat`, `/usr/bin/cat`) runs the builtin, `[ -x /bin/cat ]`
    /// holds, and `type -P cat` reports `/bin/cat`.
    pub fn set_program_names(&mut self, names: impl IntoIterator<Item = String>) {
        self.program_names = std::sync::Arc::new(names.into_iter().collect());
    }

    /// The builtin that stands for the program at `path` (as written; relative to the working
    /// directory), if there is one and it is enabled.
    pub fn program_builtin(&self, path: &str) -> Option<&str> {
        use normalize_path::NormalizePath as _;
        let path = self.absolute_path(Path::new(path)).normalize();
        let name = path.file_name()?.to_str()?;
        let dir = path.parent()?;
        if !PROGRAM_DIRS
            .iter()
            .any(|program_dir| dir == Path::new(program_dir))
        {
            return None;
        }
        let name = self.program_names.get(name)?;
        self.builtins
            .get(name)
            .is_some_and(|registration| !registration.disabled)
            .then_some(name.as_str())
    }

    /// The path of the program the builtin `name` stands for (`/bin/cat` for `cat`), if it
    /// stands for one.
    pub fn program_path(&self, name: &str) -> Option<PathBuf> {
        let enabled = self
            .builtins
            .get(name)
            .is_some_and(|registration| !registration.disabled);
        (enabled && self.program_names.contains(name))
            .then(|| Path::new(PROGRAM_DIRS[0]).join(name))
    }

    /// Register a builtin to the shell's environment, replacing any existing
    /// registration with the same name.
    ///
    /// # Arguments
    ///
    /// * `name` - The in-shell name of the builtin.
    /// * `registration` - The registration handle for the builtin.
    pub fn register_builtin<S: Into<String>>(
        &mut self,
        name: S,
        registration: builtins::Registration<SE>,
    ) {
        self.builtins.insert(name.into(), registration);
    }

    /// Register a builtin only if no builtin with that name is already registered.
    ///
    /// # Arguments
    ///
    /// * `name` - The in-shell name of the builtin.
    /// * `registration` - The registration handle for the builtin.
    pub fn register_builtin_if_unset<S: Into<String>>(
        &mut self,
        name: S,
        registration: builtins::Registration<SE>,
    ) {
        self.builtins.entry(name.into()).or_insert(registration);
    }

    /// Tries to retrieve a mutable reference to an existing builtin registration.
    /// Returns `None` if no such registration exists.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the builtin to lookup.
    pub fn builtin_mut(&mut self, name: &str) -> Option<&mut builtins::Registration<SE>> {
        self.builtins.get_mut(name)
    }

    /// Returns the registered builtins for the shell.
    pub const fn builtins(&self) -> &HashMap<String, builtins::Registration<SE>> {
        &self.builtins
    }
}

#[cfg(test)]
#[allow(clippy::panic_in_result_fn, reason = "assertions in a fallible test")]
mod tests {
    use std::path::PathBuf;

    use crate::builtins::{self, ContentOptions, ContentType, SimpleCommand};
    use crate::shell::{ProfileLoadBehavior, RcLoadBehavior};
    use crate::{ExecutionResult, Shell, error};

    struct Nop;

    impl SimpleCommand for Nop {
        fn get_content(
            _name: &str,
            _content_type: ContentType,
            _options: &ContentOptions,
        ) -> Result<String, error::Error> {
            Ok(String::new())
        }

        fn execute<SE: crate::extensions::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
            _context: crate::commands::ExecutionContext<'_, SE>,
            _args: I,
        ) -> Result<ExecutionResult, error::Error> {
            Ok(ExecutionResult::success())
        }
    }

    /// A program's path names the builtin declared to stand for it, from either program
    /// directory and however the path is spelled; nothing else does.
    #[tokio::test]
    async fn program_paths_name_the_builtins_that_stand_for_programs() -> Result<(), error::Error> {
        let mut shell = Shell::builder()
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .build()
            .await?;
        shell.register_builtin("cat", builtins::simple_builtin::<Nop, _>());
        shell.register_builtin("cd", builtins::simple_builtin::<Nop, _>());
        assert_eq!(shell.program_builtin("/bin/cat"), None);

        shell.set_program_names(["cat".to_owned()]);
        assert_eq!(shell.program_builtin("/bin/cat"), Some("cat"));
        assert_eq!(shell.program_builtin("/usr/bin/cat"), Some("cat"));
        assert_eq!(
            shell.program_builtin("/usr/bin/../../bin/./cat"),
            Some("cat")
        );
        assert_eq!(shell.program_builtin("/tmp/cat"), None);
        assert_eq!(shell.program_builtin("/bin/cd"), None);
        assert_eq!(shell.program_builtin("/bin/"), None);
        assert_eq!(shell.program_path("cat"), Some(PathBuf::from("/bin/cat")));
        assert_eq!(shell.program_path("cd"), None);

        shell.set_working_dir("/")?;
        assert_eq!(shell.program_builtin("bin/cat"), Some("cat"));
        assert_eq!(shell.program_builtin("usr/bin/cat"), Some("cat"));

        if let Some(registration) = shell.builtin_mut("cat") {
            registration.disabled = true;
        }
        assert_eq!(shell.program_builtin("/bin/cat"), None);
        assert_eq!(shell.program_path("cat"), None);
        Ok(())
    }
}
