//! Module defining the core shell structure and behavior.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::{
    ExecutionControlFlow, ExecutionResult, builtins, env::ShellEnvironment, error, extensions,
    functions, interfaces, jobs, keywords, openfiles, options::RuntimeOptions, pathcache,
    wellknownvars,
};

/// Type for storing a key bindings helper.
pub type KeyBindingsHelper = Arc<Mutex<dyn interfaces::KeyBindings>>;

/// Type alias for shell file descriptors.
pub type ShellFd = i32;

// NOTE: The submodule files below (e.g., `shell/traps.rs`, `shell/callstack.rs`) contain
// `impl Shell<SE>` blocks that provide methods coordinating with types defined in the
// corresponding top-level modules (e.g., `traps.rs`, `callstack.rs`). This is an intentional
// layered architecture: top-level modules define domain types and data structures, while
// shell/ submodules implement Shell methods that operate on those types.

mod builder;
mod builtin_registry;
mod callstack;
mod completion;
mod env;
mod execution;
mod expansion;
mod fs;
mod funcs;
mod history;
mod initscripts;
mod io;
mod job_control;
mod parsing;
mod prompts;
mod readline;
mod state;
mod traps;

pub use builder::{CreateOptions, ShellBuilder, ShellBuilderState};
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) use callstack::FrameGuard;
pub use initscripts::{ProfileLoadBehavior, RcLoadBehavior};
pub use state::ShellState;

/// Represents an instance of a shell.
///
/// # Type Parameters
///
/// * `SE` - The shell extensions implementation to use. These extensions are statically injected
///   into the shell at compile time to provide custom behavior. When unspecified, defaults to
///   `DefaultShellExtensions`, which provide standard behavior.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Shell<SE: extensions::ShellExtensions = extensions::DefaultShellExtensions> {
    /// Executor callbacks are runtime resources and are supplied anew on restoration.
    #[cfg_attr(feature = "serde", serde(skip))]
    execution_services: crate::execution::ExecutionServices,
    /// Injected error behavior.
    #[cfg_attr(feature = "serde", serde(skip, default = "default_error_formatter"))]
    error_formatter: SE::ErrorFormatter,

    /// Trap handler configuration for the shell.
    traps: crate::traps::TrapHandlerConfig,

    /// Manages files opened and accessible via redirection operators.
    open_files: openfiles::OpenFiles,

    /// The current working directory.
    working_dir: PathBuf,

    /// The shell environment, containing shell variables.
    env: ShellEnvironment,

    /// Shell function definitions.
    funcs: functions::FunctionEnv,

    /// Runtime shell options.
    options: RuntimeOptions,

    /// State of managed jobs.
    /// TODO(serde): Need to warn somehow that jobs cannot be serialized.
    #[cfg_attr(feature = "serde", serde(skip))]
    jobs: jobs::JobManager,

    /// Shell aliases.
    aliases: HashMap<String, String>,

    /// The status of the last completed command.
    last_exit_status: u8,

    /// Tracks changes to `last_exit_status`. Assignment-only commands observe it
    /// to tell whether an expansion set a status (see `interp.rs`), so code that
    /// rolls back `$?` must roll this back too.
    last_exit_status_change_count: usize,

    /// The status of each of the commands in the last pipeline.
    last_pipeline_statuses: Vec<u8>,

    /// Clone depth from the original ancestor shell.
    depth: usize,

    /// Shell name
    name: Option<String>,

    /// Positional shell arguments (not including shell name).
    args: Vec<String>,

    /// Shell version
    version: Option<String>,

    /// Detailed display string for the shell
    product_display_str: Option<String>,

    /// Function/script call stack.
    call_stack: crate::callstack::CallStack,

    /// Directory stack used by pushd et al.
    directory_stack: Vec<PathBuf>,

    /// Completion configuration.
    completion_config: crate::completion::Config,

    /// Shell built-in commands.
    #[cfg_attr(feature = "serde", serde(skip))]
    builtins: HashMap<String, builtins::Registration<SE>>,

    /// Shell program location cache.
    program_location_cache: pathcache::PathCache,

    /// Last "SECONDS" captured time.
    last_stopwatch_time: std::time::SystemTime,

    /// Last "SECONDS" offset requested.
    last_stopwatch_offset: u32,

    /// How many loops enclose the command running now. `break` and `continue` outside any loop
    /// are diagnosed rather than obeyed. A function body and a `( ... )` subshell start at 0; a
    /// command substitution and `eval` see their caller's loops, as in bash.
    pub(crate) loop_depth: usize,

    /// The top-level command running now, as (program, index): an alias defined while it runs
    /// is not expanded until a later one, as bash reads a whole command before running any of it.
    #[cfg_attr(feature = "serde", serde(skip))]
    command_unit: Option<(u64, usize)>,

    /// The top-level command each alias was defined in (see `command_unit`).
    #[cfg_attr(feature = "serde", serde(skip))]
    alias_units: HashMap<String, (u64, usize)>,

    /// How many programs this shell has begun running; numbers `command_unit`s.
    #[cfg_attr(feature = "serde", serde(skip))]
    programs_started: u64,

    /// `set -o` options saved by `local -`, restored when the saving function returns.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) local_option_saves: Vec<Vec<(&'static str, bool)>>,

    /// Parser implementation to use.
    #[cfg_attr(feature = "serde", serde(skip))]
    parser_impl: crate::parser::ParserImpl,

    /// Key bindings for the shell, optionally implemented by an interactive shell.
    #[cfg_attr(feature = "serde", serde(skip))]
    key_bindings: Option<KeyBindingsHelper>,

    /// History of commands executed in the shell.
    history: Option<crate::history::History>,

    /// Synthetic process numbers shared with every clone of this shell.
    #[cfg_attr(feature = "serde", serde(skip))]
    processes: crate::process_table::ProcessTable,

    /// The numbered logical process this shell clone runs as; `None` is the main shell.
    #[cfg_attr(feature = "serde", serde(skip))]
    own_pid: Option<crate::process_table::Pid>,

    /// `$$` of a shell started as a new process (`bash -c`); `None` keeps the session's.
    #[cfg_attr(feature = "serde", serde(skip))]
    shell_pid: Option<crate::process_table::Pid>,

    /// `$!`: the number of the last background job's last process, kept by subshells.
    #[cfg_attr(feature = "serde", serde(skip))]
    last_background_pid: Option<crate::sys::process::ProcessId>,

    /// Registered processes for the stages of this background job's pipeline, in stage order.
    #[cfg(target_arch = "wasm32")]
    #[cfg_attr(feature = "serde", serde(skip))]
    pending_stage_processes: std::collections::VecDeque<crate::execution::process::NumberedProcess>,
}

impl<SE: extensions::ShellExtensions> Clone for Shell<SE> {
    fn clone(&self) -> Self {
        Self {
            execution_services: self.execution_services,
            error_formatter: self.error_formatter.clone(),
            traps: self.traps.clone(),
            open_files: self.open_files.clone(),
            working_dir: self.working_dir.clone(),
            env: self.env.clone(),
            funcs: self.funcs.clone(),
            options: self.options.clone(),
            jobs: self.jobs.listing_copy(),
            aliases: self.aliases.clone(),
            last_exit_status: self.last_exit_status,
            last_exit_status_change_count: self.last_exit_status_change_count,
            last_pipeline_statuses: self.last_pipeline_statuses.clone(),
            name: self.name.clone(),
            args: self.args.clone(),
            version: self.version.clone(),
            product_display_str: self.product_display_str.clone(),
            call_stack: {
                // Subshells must not inherit the parent's "currently handling signal X"
                // state; otherwise a trap handler that spawns a subshell would see itself
                // as already inside that handler and skip re-entrant delivery.
                let mut cs = self.call_stack.clone();
                cs.clear_active_trap_signals();
                cs
            },
            directory_stack: self.directory_stack.clone(),
            completion_config: self.completion_config.clone(),
            builtins: self.builtins.clone(),
            program_location_cache: self.program_location_cache.clone(),
            last_stopwatch_time: self.last_stopwatch_time,
            last_stopwatch_offset: self.last_stopwatch_offset,
            loop_depth: self.loop_depth,
            command_unit: self.command_unit,
            alias_units: self.alias_units.clone(),
            programs_started: self.programs_started,
            local_option_saves: self.local_option_saves.clone(),
            parser_impl: self.parser_impl,
            key_bindings: self.key_bindings.clone(),
            history: self.history.clone(),
            depth: self.depth + 1,
            processes: self.processes.clone(),
            own_pid: self.own_pid,
            shell_pid: self.shell_pid,
            last_background_pid: self.last_background_pid,
            #[cfg(target_arch = "wasm32")]
            pending_stage_processes: std::collections::VecDeque::new(),
        }
    }
}

impl<SE: extensions::ShellExtensions> AsRef<Self> for Shell<SE> {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl<SE: extensions::ShellExtensions> AsMut<Self> for Shell<SE> {
    fn as_mut(&mut self) -> &mut Self {
        self
    }
}

impl<SE: extensions::ShellExtensions> Shell<SE> {
    /// This session's synthetic process numbers.
    pub const fn processes(&self) -> &crate::process_table::ProcessTable {
        &self.processes
    }

    /// Marks this shell clone as running as numbered process `pid`.
    pub const fn set_own_pid(&mut self, pid: crate::process_table::Pid) {
        self.own_pid = Some(pid);
    }

    /// Makes `pid` this shell's `$$`, as for a new shell process (`bash -c`); its subshells keep
    /// it.
    pub const fn set_shell_pid(&mut self, pid: crate::process_table::Pid) {
        self.shell_pid = Some(pid);
    }

    /// `$!`: the number of the last process started in the background, if any. Waiting for or
    /// disowning the job does not change it.
    pub const fn last_background_pid(&self) -> Option<crate::sys::process::ProcessId> {
        self.last_background_pid
    }

    /// Records the last process started in the background, as `$!`.
    pub const fn set_last_background_pid(&mut self, pid: crate::sys::process::ProcessId) {
        self.last_background_pid = Some(pid);
    }

    /// This shell's `$$`: the session's shell number, or its own if it was started as a new
    /// shell process.
    pub fn shell_pid(&self) -> crate::process_table::Pid {
        self.shell_pid.unwrap_or_else(|| self.processes.shell_pid())
    }

    /// Hands this background job the registered processes of its pipeline's stages.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn set_stage_processes(
        &mut self,
        processes: std::collections::VecDeque<crate::execution::process::NumberedProcess>,
    ) {
        self.pending_stage_processes = processes;
    }

    /// Takes the next stage's registered process, if this is a numbered background pipeline.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn take_stage_process(
        &mut self,
    ) -> Option<crate::execution::process::NumberedProcess> {
        self.pending_stage_processes.pop_front()
    }

    /// Returns this shell's execution services, inherited by cloned subshells.
    pub const fn execution_services(&self) -> crate::execution::ExecutionServices {
        self.execution_services
    }

    /// Reinstalls runtime callbacks after deserializing shell state.
    pub const fn set_execution_services(&mut self, services: crate::execution::ExecutionServices) {
        self.execution_services = services;
    }

    /// Returns a new shell instance created with the given options.
    /// Does *not* load any configuration files (e.g., bashrc).
    ///
    /// # Arguments
    ///
    /// * `options` - The options to use when creating the shell.
    pub(crate) fn new(options: CreateOptions<SE>) -> Result<Self, error::Error> {
        // Compute runtime options before moving fields out of `options`.
        let runtime_options = RuntimeOptions::defaults_from(&options);

        // Instantiate the shell with some defaults.
        let mut shell = Self {
            execution_services: options.execution_services,
            error_formatter: options.error_formatter,
            open_files: openfiles::OpenFiles::new(),
            options: runtime_options,
            name: options.shell_name,
            args: options.shell_args.unwrap_or_default(),
            version: options.shell_version,
            product_display_str: options.shell_product_display_str,
            working_dir: options.working_dir.map_or_else(std::env::current_dir, Ok)?,
            builtins: options.builtins,
            parser_impl: options.parser,
            key_bindings: options.key_bindings,
            ..Self::default()
        };

        // Add in any open files provided.
        shell.open_files.update_from(options.fds.into_iter());

        // If requested, seed parameters from environment.
        if !options.do_not_inherit_env {
            wellknownvars::inherit_env_vars(&mut shell)?;
        }

        // If requested, set well-known variables.
        if !options.skip_well_known_vars {
            wellknownvars::init_well_known_vars(&mut shell)?;
        }

        // Set any provided variables.
        for (var_name, var_value) in options.vars {
            shell.env.set_global(var_name, var_value)?;
        }

        // Set up history, if relevant. The history file itself is loaded later, once startup
        // files have run, since they may change HISTFILE.
        if shell.options.enable_command_history {
            shell.history = Some(crate::history::History::default());
        }

        Ok(shell)
    }
}

impl<SE: extensions::ShellExtensions> Shell<SE> {
    /// Increments the interactive line offset in the shell by the indicated number
    /// of lines.
    ///
    /// # Arguments
    ///
    /// * `delta` - The number of lines to increment the current line offset by.
    pub fn increment_interactive_line_offset(&mut self, delta: usize) {
        self.call_stack.increment_current_line_offset(delta);
    }

    /// Updates the currently executing command in the shell.
    pub fn set_current_cmd(&mut self, cmd: &impl brush_parser::ast::Node) {
        self.call_stack
            .set_current_pos(cmd.location().map(|span| span.start));
    }

    /// Updates the `$_` shell variable (last-argument of the previous simple
    /// command).
    ///
    /// Passes `Some(last_arg)` to record the last argument of the just-executed
    /// command, or `None` to clear `$_` (used for assignment-only statements,
    /// which bash treats as having no "last argument").
    ///
    /// The update is applied in-place so that attributes on `_` (notably
    /// `readonly`) are preserved: attempting to update a readonly `_` is a
    /// silent no-op, matching bash's observable stdout behavior.
    pub(crate) fn update_last_arg_variable(&mut self, last_arg: Option<String>) {
        // Bash refuses to update a readonly `_`, emitting an error to stderr
        // on each attempt. We silently skip the update here — the observable
        // stdout effect ($_ stays unchanged) matches bash; the missing stderr
        // diagnostics are harmless.
        if self
            .env
            .get_using_policy("_", crate::env::EnvironmentLookup::Anywhere)
            .is_some_and(|v| v.is_readonly())
        {
            return;
        }

        // Replace the variable entirely (fresh, non-exported). This matches
        // bash, which never exports `_` — even under `set -a` — and always
        // clears any previously-set attributes (except readonly, handled
        // above).
        let value = last_arg.unwrap_or_default();
        let _ = self
            .env
            .set_global("_", crate::variables::ShellVariable::new(value));
    }

    /// Captures the state the last command left behind; see [`SavedCommandStatus`].
    pub fn save_command_status(&self) -> SavedCommandStatus {
        SavedCommandStatus {
            exit_status: self.last_exit_status,
            exit_status_change_count: self.last_exit_status_change_count,
            pipeline_statuses: self.last_pipeline_statuses.clone(),
            last_arg: self.env_str("_").map(|value| value.into_owned()),
        }
    }

    /// Reapplies a snapshot, exit-status change counter included. Consumes it; `clone` it to
    /// put the same one back more than once, e.g. between successive hook functions.
    ///
    /// # Arguments
    ///
    /// * `saved` - The snapshot to restore.
    pub fn restore_command_status(&mut self, saved: SavedCommandStatus) {
        self.last_pipeline_statuses = saved.pipeline_statuses;
        // Assigned directly rather than through `set_last_exit_status`, which would bump
        // the change counter we're about to put back.
        self.last_exit_status = saved.exit_status;
        self.last_exit_status_change_count = saved.exit_status_change_count;
        match saved.last_arg {
            Some(last_arg) => self.update_last_arg_variable(Some(last_arg)),
            // `_` was unset when the snapshot was taken, so put it back that way. Unsetting
            // a readonly `_` fails; ignore that, as `update_last_arg_variable` does.
            None => _ = self.env.unset("_"),
        }
    }

    /// Applies errexit semantics to a result if enabled and appropriate.
    /// This should be called at "statement boundaries" where errexit should be checked.
    ///
    /// # Arguments
    ///
    /// * `result` - The execution result to potentially modify.
    pub const fn apply_errexit_if_enabled(&self, result: &mut ExecutionResult) {
        if self.options.exit_on_nonzero_command_exit
            && !result.is_success()
            && result.is_normal_flow()
        {
            result.next_control_flow = ExecutionControlFlow::ExitShell;
        }
    }

    /// Returns the keywords that are reserved by the shell.
    pub(crate) fn get_keywords(&self) -> impl IntoIterator<Item = &str> {
        if self.options.sh_mode {
            keywords::SH_MODE_KEYWORDS.iter().copied()
        } else {
            keywords::KEYWORDS.iter().copied()
        }
    }

    /// Checks if the given string is a keyword reserved in this shell.
    ///
    /// # Arguments
    ///
    /// * `s` - The string to check.
    pub fn is_keyword(&self, s: &str) -> bool {
        if self.options.sh_mode {
            keywords::SH_MODE_KEYWORDS.contains(s)
        } else {
            keywords::KEYWORDS.contains(s)
        }
    }

    pub(crate) const fn last_exit_status_change_count(&self) -> usize {
        self.last_exit_status_change_count
    }

    /// Empties the call stack, for a copy of this shell that stands for a newly started shell
    /// process (whose line numbers and function names start afresh).
    pub fn reset_call_stack(&mut self) {
        self.call_stack = crate::callstack::CallStack::new();
    }

    /// How many loops enclose the command running now (see the field's documentation).
    pub const fn loop_depth(&self) -> usize {
        self.loop_depth
    }

    /// Defines an alias as the `alias` builtin does: it is not expanded until the next top-level
    /// command, since bash has already read the rest of the current one.
    pub fn define_alias(&mut self, name: String, value: String) {
        match self.command_unit {
            Some(unit) => self.alias_units.insert(name.clone(), unit),
            None => self.alias_units.remove(&name),
        };
        self.aliases.insert(name, value);
    }

    /// Whether the alias `name` may be expanded in the command running now.
    pub(crate) fn alias_in_effect(&self, name: &str) -> bool {
        self.command_unit.is_none() || self.alias_units.get(name) != self.command_unit.as_ref()
    }

    /// Marks the start of a program's top-level commands; returns its number and the unit it
    /// interrupts, to restore with [`Self::end_program`].
    pub(crate) const fn begin_program(&mut self) -> (u64, Option<(u64, usize)>) {
        self.programs_started += 1;
        (self.programs_started, self.command_unit)
    }

    /// Marks the start of top-level command `index` of program `program`.
    pub(crate) const fn begin_command_unit(&mut self, program: u64, index: usize) {
        self.command_unit = Some((program, index));
    }

    /// Restores the unit a program interrupted.
    pub(crate) const fn end_program(&mut self, previous: Option<(u64, usize)>) {
        self.command_unit = previous;
    }

    /// Saves the `set -o` options, as `local -` does, to restore when the function running now
    /// returns.
    pub fn save_options_locally(&mut self) {
        let saved = crate::namedoptions::options(crate::namedoptions::ShellOptionKind::SetO)
            .iter()
            .map(|option| (option.name, option.definition.get(&self.options)))
            .collect();
        self.local_option_saves.push(saved);
    }

    /// Restores the options the first `local -` since `mark` saved, and forgets the saves since.
    pub(crate) fn restore_local_options(&mut self, mark: usize) {
        if let Some(saved) = self.local_option_saves.get(mark).cloned() {
            let options = crate::namedoptions::options(crate::namedoptions::ShellOptionKind::SetO);
            for (name, value) in saved {
                if let Some(definition) = options.get(name) {
                    definition.set(&mut self.options, value);
                }
            }
        }
        self.local_option_saves.truncate(mark);
    }

    /// Whether a trap handler is running.
    pub fn running_trap_handler(&self) -> bool {
        self.call_stack
            .iter()
            .any(|frame| frame.frame_type.is_trap_handler())
    }

    /// Sets the shell's name: `$0`, and the name its diagnostics start with, outside a script.
    pub fn set_shell_name(&mut self, name: impl Into<String>) {
        self.name = Some(name.into());
    }

    /// Sets `POSIXLY_CORRECT=y` while posix mode is on and unsets it when it goes off, as bash
    /// does when `set -o posix` changes.
    pub fn sync_posixly_correct(&mut self) -> Result<(), error::Error> {
        if self.options.posix_mode {
            self.env
                .set_global("POSIXLY_CORRECT", crate::variables::ShellVariable::new("y"))?;
        } else {
            self.env.unset("POSIXLY_CORRECT")?;
        }
        Ok(())
    }

    /// The name diagnostics start with: the shell's name (`$0`), as bash uses.
    pub fn diagnostic_name(&self) -> String {
        self.current_shell_name()
            .map_or_else(|| "bash".to_owned(), |n| n.to_string())
    }

    /// The prefix bash puts on a diagnostic: `NAME: line N: ` in a script or command string,
    /// `NAME: ` in an interactive shell, where NAME is `$0`.
    pub fn diagnostic_prefix(&self) -> String {
        let name = self.diagnostic_name();
        if self.options.interactive {
            return format!("{name}: ");
        }
        let line = self
            .call_stack
            .current_frame()
            .and_then(|frame| frame.current_line())
            .unwrap_or(1);
        format!("{name}: line {line}: ")
    }
}

/// Snapshot of the state the last command left behind: `$?`, `PIPESTATUS`, and `$_`.
///
/// Take one with [`Shell::save_command_status`] and put it back with
/// [`Shell::restore_command_status`] around anything the user didn't type -- a shell hook,
/// `PROMPT_COMMAND`, prompt expansion -- so it stays invisible to the next command.
#[derive(Clone, Debug)]
pub struct SavedCommandStatus {
    exit_status: u8,
    exit_status_change_count: usize,
    pipeline_statuses: Vec<u8>,
    last_arg: Option<String>,
}

#[inherent::inherent]
impl<SE: extensions::ShellExtensions> ShellState for Shell<SE> {
    /// Returns the number of the logical process this shell runs as (`$$` for the main shell).
    pub fn own_pid(&self) -> crate::process_table::Pid {
        self.own_pid.unwrap_or_else(|| self.shell_pid())
    }

    /// Returns whether or not this shell is a subshell.
    pub fn is_subshell(&self) -> bool {
        self.depth > 0
    }

    /// Returns the last "SECONDS" captured time.
    pub fn last_stopwatch_time(&self) -> std::time::SystemTime {
        self.last_stopwatch_time
    }

    /// Returns the last "SECONDS" offset requested.
    pub fn last_stopwatch_offset(&self) -> u32 {
        self.last_stopwatch_offset
    }

    /// Returns the shell environment containing variables.
    pub fn env(&self) -> &ShellEnvironment {
        &self.env
    }

    /// Returns a mutable reference to the shell environment.
    pub fn env_mut(&mut self) -> &mut ShellEnvironment {
        &mut self.env
    }

    /// Returns the shell's runtime options.
    pub fn options(&self) -> &RuntimeOptions {
        &self.options
    }

    /// Returns a mutable reference to the shell's runtime options.
    pub fn options_mut(&mut self) -> &mut RuntimeOptions {
        &mut self.options
    }

    /// Returns the shell's aliases.
    pub fn aliases(&self) -> &HashMap<String, String> {
        &self.aliases
    }

    /// Returns a mutable reference to the shell's aliases.
    pub fn aliases_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.aliases
    }

    /// Returns the shell's job manager.
    pub fn jobs(&self) -> &jobs::JobManager {
        &self.jobs
    }

    /// Returns a mutable reference to the shell's job manager.
    pub fn jobs_mut(&mut self) -> &mut jobs::JobManager {
        &mut self.jobs
    }

    /// Returns the shell's trap handler configuration.
    pub fn traps(&self) -> &crate::traps::TrapHandlerConfig {
        &self.traps
    }

    /// Returns a mutable reference to the shell's trap handler configuration.
    pub fn traps_mut(&mut self) -> &mut crate::traps::TrapHandlerConfig {
        &mut self.traps
    }

    /// Returns the shell's directory stack.
    pub fn directory_stack(&self) -> &[PathBuf] {
        &self.directory_stack
    }

    /// Returns a mutable reference to the shell's directory stack.
    pub fn directory_stack_mut(&mut self) -> &mut Vec<PathBuf> {
        &mut self.directory_stack
    }

    /// Returns the statuses of commands in the last pipeline.
    pub fn last_pipeline_statuses(&self) -> &[u8] {
        &self.last_pipeline_statuses
    }

    /// Returns a mutable reference to the statuses of commands in the last pipeline.
    pub fn last_pipeline_statuses_mut(&mut self) -> &mut Vec<u8> {
        &mut self.last_pipeline_statuses
    }

    /// Returns the shell's program location cache.
    pub fn program_location_cache(&self) -> &pathcache::PathCache {
        &self.program_location_cache
    }

    /// Returns a mutable reference to the shell's program location cache.
    pub fn program_location_cache_mut(&mut self) -> &mut pathcache::PathCache {
        &mut self.program_location_cache
    }

    /// Returns the shell's completion configuration.
    pub fn completion_config(&self) -> &crate::completion::Config {
        &self.completion_config
    }

    /// Returns a mutable reference to the shell's completion configuration.
    pub fn completion_config_mut(&mut self) -> &mut crate::completion::Config {
        &mut self.completion_config
    }

    /// Returns the shell's open files.
    pub fn open_files(&self) -> &openfiles::OpenFiles {
        &self.open_files
    }

    /// Returns a mutable reference to the shell's open files.
    pub fn open_files_mut(&mut self) -> &mut openfiles::OpenFiles {
        &mut self.open_files
    }

    /// Returns the *current* name of the shell ($0).
    /// Influenced by the current call stack.
    pub fn current_shell_name(&self) -> Option<Cow<'_, str>> {
        for frame in self.call_stack.iter() {
            // Executed scripts shadow the shell name.
            if frame.frame_type.is_run_script() {
                return Some(frame.frame_type.name());
            }
        }

        self.name.as_deref().map(|name| name.into())
    }

    /// Returns the current subshell depth; 0 is returned if this shell is not a subshell.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Returns the call stack for the shell.
    pub fn call_stack(&self) -> &crate::callstack::CallStack {
        &self.call_stack
    }

    /// Returns the shell's history, if it exists.
    pub fn history(&self) -> Option<&crate::history::History> {
        self.history.as_ref()
    }

    /// Returns a mutable reference to the shell's history, if it exists.
    pub fn history_mut(&mut self) -> Option<&mut crate::history::History> {
        self.history.as_mut()
    }

    /// Returns the shell's official version string (if available).
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Returns the exit status of the last command executed in this shell.
    pub fn last_exit_status(&self) -> u8 {
        self.last_exit_status
    }

    /// Updates the last exit status, bumping `last_exit_status_change_count`. To *restore* a
    /// status rather than set one, use [`Self::restore_command_status`], which puts the
    /// counter back too.
    pub fn set_last_exit_status(&mut self, status: u8) {
        self.last_exit_status = status;
        self.last_exit_status_change_count += 1;
    }

    /// Returns the key bindings helper for the shell.
    pub fn key_bindings(&self) -> Option<&KeyBindingsHelper> {
        self.key_bindings.as_ref()
    }

    /// Sets the key bindings helper for the shell.
    pub fn set_key_bindings(&mut self, key_bindings: Option<KeyBindingsHelper>) {
        self.key_bindings = key_bindings;
    }

    /// Returns the shell's current working directory.
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// Returns a mutable reference to the shell's current working directory.
    /// This is only accessible within the crate.
    pub(crate) fn working_dir_mut(&mut self) -> &mut PathBuf {
        &mut self.working_dir
    }

    /// Returns the product display name for this shell.
    pub fn product_display_str(&self) -> Option<&str> {
        self.product_display_str.as_deref()
    }
}

#[cfg(feature = "serde")]
fn default_error_formatter<EF: extensions::ErrorFormatter>() -> EF {
    EF::default()
}

#[cfg(test)]
#[allow(clippy::panic_in_result_fn, reason = "assertions in a fallible test")]
mod tests {
    use super::*;

    /// `$_` round-trips through a snapshot, unset included, so an embedder that saves before
    /// running commands of its own gets back exactly what was there -- not an empty string.
    /// (The interactive loop can't reach the unset case: every command resets `$_`.)
    #[tokio::test]
    async fn saved_command_status_round_trips_last_arg() -> Result<(), error::Error> {
        let mut shell = Shell::builder()
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .build()
            .await?;

        shell.update_last_arg_variable(Some(String::from("saved")));
        let with_value = shell.save_command_status();

        shell.env.unset("_")?;
        let while_unset = shell.save_command_status();

        shell.restore_command_status(with_value);
        assert_eq!(shell.env_str("_").as_deref(), Some("saved"));

        shell.restore_command_status(while_unset);
        assert_eq!(shell.env_str("_"), None);

        Ok(())
    }

    #[tokio::test]
    async fn unset_env_str_contract_is_independent_of_ifs_default() -> Result<(), error::Error> {
        let mut shell = Shell::builder()
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .build()
            .await?;

        shell.env.set_global(
            "IFS",
            crate::ShellVariable::new(crate::ShellValue::Unset(
                crate::variables::ShellValueUnsetType::Untyped,
            )),
        )?;
        assert_eq!(shell.env_str("IFS").as_deref(), Some(""));
        assert_eq!(shell.ifs(), " \t\n");

        shell.env.set_global("IFS", crate::ShellVariable::new(""))?;
        assert_eq!(shell.env_str("IFS").as_deref(), Some(""));
        assert_eq!(shell.ifs(), "");

        Ok(())
    }
}
