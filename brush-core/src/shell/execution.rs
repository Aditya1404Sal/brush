//! Execution support for shell.

use std::{io::Read, path::Path};

use crate::{
    ExecutionControlFlow, ExecutionParameters, ExecutionResult, ProcessGroupPolicy, SourceInfo,
    arithmetic::Evaluatable as _, callstack, error, interp::Execute as _, openfiles,
    trace_categories,
};

impl<SE: crate::extensions::ShellExtensions> crate::Shell<SE> {
    /// Returns the default execution parameters for this shell.
    pub fn default_exec_params(&self) -> ExecutionParameters {
        let mut params = ExecutionParameters::default();

        params.process_group_policy = if self.options.enable_job_control {
            ProcessGroupPolicy::NewProcessGroup
        } else {
            ProcessGroupPolicy::SameProcessGroup
        };

        params
    }

    pub(super) async fn source_if_exists(
        &mut self,
        path: impl AsRef<Path>,
        params: &ExecutionParameters,
    ) -> Result<bool, error::Error> {
        let path = path.as_ref();
        if path.exists() {
            self.source_script(path, std::iter::empty::<String>(), params)
                .await?;
            Ok(true)
        } else {
            tracing::debug!("skipping non-existent file: {}", path.display());
            Ok(false)
        }
    }

    /// Source the given file as a shell script, returning the execution result.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the file to source.
    /// * `args` - The arguments to pass to the script as positional parameters.
    /// * `params` - Execution parameters.
    pub async fn source_script<S: Into<String>, P: AsRef<Path>, I: Iterator<Item = S>>(
        &mut self,
        path: P,
        args: I,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        self.parse_and_execute_script_file(
            path.as_ref(),
            args,
            params,
            callstack::ScriptCallType::Source,
        )
        .await
    }

    /// Parse and execute the given file as a shell script, returning the execution result.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the file to source.
    /// * `args` - The arguments to pass to the script as positional parameters.
    /// * `params` - Execution parameters.
    /// * `call_type` - The type of script call being made.
    async fn parse_and_execute_script_file<
        S: Into<String>,
        P: AsRef<Path>,
        I: Iterator<Item = S>,
    >(
        &mut self,
        path: P,
        args: I,
        params: &ExecutionParameters,
        call_type: callstack::ScriptCallType,
    ) -> Result<ExecutionResult, error::Error> {
        let path = path.as_ref();
        tracing::debug!("sourcing: {}", path.display());

        let mut options = std::fs::File::options();
        options.read(true);

        let opened_file: openfiles::OpenFile = self
            .open_file(&options, path, params)
            .map_err(|e| error::ErrorKind::FailedSourcingFile(path.to_owned(), e))?;

        if opened_file.is_dir() {
            return Err(error::ErrorKind::FailedSourcingFile(
                path.to_owned(),
                std::io::Error::from(std::io::ErrorKind::IsADirectory),
            )
            .into());
        }

        let source_info = crate::SourceInfo::from(path.to_owned());

        let mut result = self
            .source_file(opened_file, &source_info, args, params, call_type)
            .await?;

        // Handle control flow at script execution boundary. If execution completed
        // with a `return`, we need to clear it since it's already been "used". All
        // other control flow types are preserved.
        if matches!(
            result.next_control_flow,
            ExecutionControlFlow::ReturnFromFunctionOrScript
        ) {
            result.next_control_flow = ExecutionControlFlow::Normal;
        }

        Ok(result)
    }

    /// Source the given file as a shell script, returning the execution result.
    ///
    /// # Arguments
    ///
    /// * `file` - The file to source.
    /// * `source_info` - Information about the source of the script.
    /// * `args` - The arguments to pass to the script as positional parameters.
    /// * `params` - Execution parameters.
    /// * `call_type` - The type of script call being made.
    async fn source_file<F: Read, S: Into<String>, I: Iterator<Item = S>>(
        &mut self,
        file: F,
        source_info: &crate::SourceInfo,
        args: I,
        params: &ExecutionParameters,
        call_type: callstack::ScriptCallType,
    ) -> Result<ExecutionResult, error::Error> {
        // The text is kept to word a syntax error as bash does.
        let mut text = String::new();
        std::io::BufReader::new(file).read_to_string(&mut text)?;

        tracing::debug!(target: trace_categories::PARSE, "Parsing sourced file: {}", source_info.source);
        let parse_result = self.parse_string(text.as_str());

        let script_positional_args = args.map(Into::into);

        self.call_stack
            .push_script(call_type, source_info, script_positional_args);
        self.pending_input = Some(text.as_str().into());

        #[cfg(any(target_arch = "wasm32", test))]
        let result = {
            let mut frame = super::FrameGuard::new(
                self,
                |shell| {
                    shell.call_stack.pop();
                    Ok(())
                },
                None,
            );
            frame
                .shell()
                .run_parsed_result(parse_result, Some(&text), source_info, params)
                .await
        };
        #[cfg(not(any(target_arch = "wasm32", test)))]
        let result = {
            let result = self
                .run_parsed_result(parse_result, Some(&text), source_info, params)
                .await;
            self.call_stack.pop();
            result
        };

        self.pending_input = None;

        // The RETURN trap runs as a sourced script returns, as in bash.
        if matches!(call_type, callstack::ScriptCallType::Source) {
            if let Ok(result) = &result {
                self.last_exit_status = result.exit_code.into();
            }
            self.run_return_trap(params).await?;
        }
        result
    }

    /// Executes the given string as a shell program, returning the resulting exit status.
    ///
    /// # Arguments
    ///
    /// * `command` - The command to execute.
    /// * `source_info` - Information about the source of the command text.
    /// * `params` - Execution parameters.
    pub async fn run_string<S: Into<String>>(
        &mut self,
        command: S,
        source_info: &crate::SourceInfo,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let command: String = command.into();
        let parse_result = self.parse_string(command.as_str());
        self.pending_input = Some(command.as_str().into());
        let result = self
            .run_parsed_result(parse_result, Some(&command), source_info, params)
            .await;
        self.pending_input = None;
        result
    }

    /// Executes the given command, provided to a shell executable on the command
    /// line (i.e., via `-c`).
    ///
    /// It is expected that the shell will not be used for any further execution
    /// after this command; this function will perform any necessary shell exit
    /// handling.
    ///
    /// # Arguments
    ///
    /// * `command` - The command to execute.
    pub async fn run_dash_c_command<S: Into<String>>(
        &mut self,
        command: S,
    ) -> Result<ExecutionResult, error::Error> {
        self.start_command_string_mode();

        // Execute the command string.
        let params = self.default_exec_params();
        let source_info = SourceInfo::from("-c");
        let result = self.run_string(command, &source_info, &params).await?;

        self.end_command_string_mode()?;

        // Give the shell a chance to run on-exit tasks, but ignore the result.
        let _ = self.on_exit().await;

        Ok(result)
    }

    /// Executes the given script file, returning the resulting exit status.
    ///
    /// It is expected that the shell will not be used for any further execution
    /// after this command; this function will perform any necessary shell exit
    /// handling.
    ///
    /// # Arguments
    ///
    /// * `script_path` - The path to the script file to execute.
    /// * `args` - The arguments to pass to the script as positional parameters.
    pub async fn run_script<S: Into<String>, P: AsRef<Path>, I: Iterator<Item = S>>(
        &mut self,
        script_path: P,
        args: I,
    ) -> Result<ExecutionResult, error::Error> {
        let params = self.default_exec_params();
        let result = self
            .parse_and_execute_script_file(
                script_path.as_ref(),
                args,
                &params,
                callstack::ScriptCallType::Run,
            )
            .await?;

        // Give the shell a chance to run on-exit tasks, but ignore the result.
        let _ = self.on_exit().await;

        Ok(result)
    }

    /// Runs a parsed program, or reports why it did not parse. `source` is the text that was
    /// parsed, when known, so a syntax error can be worded as bash words it.
    pub(crate) async fn run_parsed_result(
        &mut self,
        parse_result: Result<brush_parser::ast::Program, brush_parser::ParseError>,
        source: Option<&str>,
        source_info: &crate::SourceInfo,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // If parsing succeeded, run the program. If there's a parse error, it's fatal (per spec).
        let result = match parse_result {
            Ok(prog) => self.run_program(prog, params).await,
            Err(parse_err) => Err(self
                .syntax_error(parse_err, source, source_info)
                .into_fatal()),
        };

        // Report any errors.
        match result {
            Ok(result) => Ok(result),
            Err(err) => {
                let _ = self.display_error(&mut params.stderr(self), &err);

                let result = err.into_result(self);
                self.set_last_exit_status(result.exit_code.into());

                Ok(result)
            }
        }
    }

    /// A parse error, worded as bash words it when the parsed text is known. Bash names what it
    /// was parsing: a trap handler as `exit trap`, otherwise the source (`-c`, `eval`, a path).
    fn syntax_error(
        &self,
        error: brush_parser::ParseError,
        source: Option<&str>,
        source_info: &crate::SourceInfo,
    ) -> error::Error {
        let Some(source) = source else {
            return error::ErrorKind::ParseError(error, source_info.clone()).into();
        };
        let origin = match self
            .call_stack
            .current_frame()
            .map(|frame| &frame.frame_type)
        {
            Some(callstack::FrameType::TrapHandler(signal)) => {
                std::format!("{} trap", signal.to_string().to_lowercase())
            }
            // Command strings and the substitutions inside them.
            _ if matches!(source_info.source.as_str(), "main" | "environment") => "-c".to_owned(),
            _ => source_info.source.clone(),
        };
        let lines = brush_parser::bash_diagnostic(&error, source, &self.parser_options());
        error::ErrorKind::SyntaxError { origin, lines }.into()
    }

    /// Executes the given parsed shell program, returning the resulting exit status.
    ///
    /// # Arguments
    ///
    /// * `program` - The program to execute.
    /// * `params` - Execution parameters.
    pub async fn run_program(
        &mut self,
        program: brush_parser::ast::Program,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        #[cfg(target_arch = "wasm32")]
        if !crate::execution::process::is_active() {
            return crate::execution::process::run_process(
                self.traps().pipe_disposition(),
                program.execute(self, params),
            )
            .await;
        }
        program.execute(self, params).await
    }

    /// Evaluate the given arithmetic expression, returning the result.
    pub fn eval_arithmetic(
        &mut self,
        expr: &brush_parser::ast::ArithmeticExpr,
    ) -> Result<i64, error::Error> {
        Ok(expr.eval(self)?)
    }
}
