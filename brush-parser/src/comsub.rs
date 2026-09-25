//! Prints a command substitution's command back as text the way bash does.
//!
//! Bash parses the command in `$( )` (and `<( )`, `>( )`) when it reads the word, and keeps the
//! command printed back from the parse (`print_comsub` in bash's `print_cmd.c`) as the word's
//! text: that text is what the substitution runs, what `declare -f` shows, and what its line
//! numbers count. This is a port of that printer: the newlines that separate commands stay
//! newlines, a `;` becomes `; `, compound commands take bash's layout, and blank lines, comments
//! and line continuations are gone.

use crate::ast;
use crate::word::{self, WordPiece, WordPieceWithSource};
use crate::{Parser, ParserOptions};

/// How many columns a nested command is indented by (bash's `indentation_amount`).
const INDENTATION_AMOUNT: usize = 4;

/// The command `program` (the parsed text of a `$( )`) printed as bash prints it back.
pub fn print_comsub(program: &ast::Program, options: &ParserOptions) -> String {
    let items: Vec<&ast::CompoundListItem> = program
        .complete_commands
        .iter()
        .flat_map(|list| &list.0)
        .collect();
    print_items(&items, options)
}

/// The command `list` (a process substitution's) printed as bash prints it back.
pub fn print_comsub_list(list: &ast::CompoundList, options: &ParserOptions) -> String {
    let items: Vec<&ast::CompoundListItem> = list.0.iter().collect();
    print_items(&items, options)
}

fn print_items(items: &[&ast::CompoundListItem], options: &ParserOptions) -> String {
    let mut printer = Printer::new(options);
    if let Some(command) = list_command(items) {
        printer.command(&command);
    }
    printer.out
}

/// The text of a `$( )` whose command is `text` as bash keeps it: printed back when it parses,
/// with a blank before a `(` so the result does not read as `$((`; `None` when it does not parse.
pub fn reprint_comsub_text(text: &str, options: &ParserOptions) -> Option<String> {
    cached_reprint(text, options)
}

#[cached::macros::cached(
    max_size = 64,
    key = "(String, ParserOptions)",
    convert = r#"{ (text.to_owned(), options.to_owned()) }"#
)]
fn cached_reprint(text: &str, options: &ParserOptions) -> Option<String> {
    let program = Parser::new(text.as_bytes(), options).parse_program().ok()?;
    let printed = print_comsub(&program, options);
    Some(if printed.starts_with('(') {
        format!(" {printed}")
    } else {
        printed
    })
}

/// `word` with each command and process substitution in it printed back as bash keeps it (see
/// [`reprint_comsub_text`]); `None` when it holds none, or they are as bash would print them.
pub fn reprint_word(word: &str, options: &ParserOptions) -> Option<String> {
    if !word.contains("$(") && !word.contains("<(") && !word.contains(">(") {
        return None;
    }
    let pieces = word::parse(word, options).ok()?;
    let mut replacements = vec![];
    collect_replacements(&pieces, options, &mut replacements);
    if replacements.is_empty() {
        return None;
    }
    let mut result = String::with_capacity(word.len());
    let mut copied = 0;
    for (start, end, text) in replacements {
        let (Some(before), Some(_)) = (word.get(copied..start), word.get(start..end)) else {
            return None;
        };
        result.push_str(before);
        result.push_str(&text);
        copied = end;
    }
    result.push_str(word.get(copied..)?);
    (result != word).then_some(result)
}

/// The byte ranges of `pieces`' substitutions and the text bash keeps for each, in order.
fn collect_replacements(
    pieces: &[WordPieceWithSource],
    options: &ParserOptions,
    replacements: &mut Vec<(usize, usize, String)>,
) {
    for piece in pieces {
        match &piece.piece {
            WordPiece::CommandSubstitution(text) => {
                if let Some(printed) = reprint_comsub_text(text, options) {
                    replacements.push((
                        piece.start_index,
                        piece.end_index,
                        format!("$({printed})"),
                    ));
                }
            }
            WordPiece::ProcessSubstitution(kind, text) => {
                if let Some(printed) = reprint_comsub_text(text, options) {
                    replacements.push((
                        piece.start_index,
                        piece.end_index,
                        format!("{kind}({printed})"),
                    ));
                }
            }
            WordPiece::DoubleQuotedSequence(inner)
            | WordPiece::GettextDoubleQuotedSequence(inner) => {
                collect_replacements(inner, options, replacements);
            }
            _ => (),
        }
    }
}

/// A word's text as bash keeps it (see [`reprint_word`]).
fn word_text(word: &str, options: &ParserOptions) -> String {
    reprint_word(word, options).unwrap_or_else(|| word.to_owned())
}

/// A connector between two commands (bash's `command_connect`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Connector {
    Async,
    Semicolon,
    Newline,
    Pipe,
    And,
    Or,
}

/// A command as bash's parser builds it: lists and pipelines are connections of two commands.
enum Command<'a> {
    Connection {
        first: Box<Self>,
        connector: Connector,
        second: Option<Box<Self>>,
        flags: Flags,
    },
    Simple(&'a ast::SimpleCommand, Flags),
    Compound(
        &'a ast::CompoundCommand,
        Option<&'a ast::RedirectList>,
        Flags,
    ),
    Function(&'a ast::FunctionDefinition, Flags),
    /// A pipeline without commands (`time` alone).
    Empty(Flags),
    /// An `elif` chain, which bash nests as an `if` in the `else` part.
    If {
        condition: &'a ast::CompoundList,
        then: &'a ast::CompoundList,
        elses: &'a [ast::ElseClause],
    },
}

/// Bash's `CMD_TIME_PIPELINE`, `CMD_TIME_POSIX` and `CMD_INVERT_RETURN`.
#[derive(Clone, Copy, Default)]
struct Flags {
    time: Option<bool>,
    invert: bool,
}

impl Command<'_> {
    const fn flags_mut(&mut self) -> Option<&mut Flags> {
        match self {
            Self::Connection { flags, .. }
            | Self::Simple(_, flags)
            | Self::Compound(_, _, flags)
            | Self::Function(_, flags)
            | Self::Empty(flags) => Some(flags),
            Self::If { .. } => None,
        }
    }
}

/// A list as bash builds it: `a; b; c` connects left to right, and a `&` after a `;` list applies
/// to its last command only (bash's `connect_async_list`).
fn list_command<'a>(items: &[&'a ast::CompoundListItem]) -> Option<Command<'a>> {
    let (first, rest) = items.split_first()?;
    let mut command = and_or_command(&first.0);
    let mut separator = &first.1;
    for item in rest {
        let next = and_or_command(&item.0);
        command = match separator {
            ast::SeparatorOperator::Async => connect_async(command, Some(next)),
            ast::SeparatorOperator::Sequence => connect(command, Connector::Semicolon, Some(next)),
            ast::SeparatorOperator::Newline => connect(command, Connector::Newline, Some(next)),
        };
        separator = &item.1;
    }
    if separator.is_async() {
        command = connect_async(command, None);
    }
    Some(command)
}

fn connect<'a>(
    first: Command<'a>,
    connector: Connector,
    second: Option<Command<'a>>,
) -> Command<'a> {
    Command::Connection {
        first: Box::new(first),
        connector,
        second: second.map(Box::new),
        flags: Flags::default(),
    }
}

/// Bash's `connect_async_list`: `a; b & c` runs `b` in the background, not `a; b`.
fn connect_async<'a>(command: Command<'a>, next: Option<Command<'a>>) -> Command<'a> {
    match command {
        Command::Connection {
            first,
            connector: Connector::Semicolon,
            second: Some(second),
            flags,
        } => Command::Connection {
            first,
            connector: Connector::Semicolon,
            second: Some(Box::new(connect_async(*second, next))),
            flags,
        },
        command => connect(command, Connector::Async, next),
    }
}

fn and_or_command(and_or: &ast::AndOrList) -> Command<'_> {
    let mut command = pipeline_command(&and_or.first);
    for next in &and_or.additional {
        command = match next {
            ast::AndOr::And(pipeline) => {
                connect(command, Connector::And, Some(pipeline_command(pipeline)))
            }
            ast::AndOr::Or(pipeline) => {
                connect(command, Connector::Or, Some(pipeline_command(pipeline)))
            }
        };
    }
    command
}

/// A pipeline as bash builds it, right to left (`a | (b | c)`), with its `time` and `!` on the
/// whole of it.
fn pipeline_command(pipeline: &ast::Pipeline) -> Command<'_> {
    let mut commands = pipeline.seq.iter().rev().map(single_command);
    let mut command = commands
        .next()
        .unwrap_or_else(|| Command::Empty(Flags::default()));
    for previous in commands {
        command = connect(previous, Connector::Pipe, Some(command));
    }
    if let Some(flags) = command.flags_mut() {
        flags.time = pipeline
            .timed
            .as_ref()
            .map(|timed| matches!(timed, ast::PipelineTimed::TimedWithPosixOutput(_)));
        flags.invert = pipeline.bang;
    }
    command
}

fn single_command(command: &ast::Command) -> Command<'_> {
    match command {
        ast::Command::Simple(simple) => Command::Simple(simple, Flags::default()),
        ast::Command::Compound(compound, redirects) => {
            Command::Compound(compound, redirects.as_ref(), Flags::default())
        }
        ast::Command::Function(function) => Command::Function(function, Flags::default()),
    }
}

/// A here-document whose body is still to be printed.
struct HereDoc<'a> {
    doc: &'a ast::IoHereDocument,
}

/// Bash's `print_cmd.c` state while it prints one command.
struct Printer<'a> {
    options: &'a ParserOptions,
    out: String,
    indentation: usize,
    skip_this_indent: usize,
    was_heredoc: bool,
    printing_connection: usize,
    inside_function_def: usize,
    deferred_heredocs: Vec<HereDoc<'a>>,
}

impl<'a> Printer<'a> {
    const fn new(options: &'a ParserOptions) -> Self {
        Self {
            options,
            out: String::new(),
            indentation: 0,
            skip_this_indent: 0,
            was_heredoc: false,
            printing_connection: 0,
            inside_function_def: 0,
            deferred_heredocs: vec![],
        }
    }

    fn print(&mut self, text: &str) {
        self.out.push_str(text);
    }

    fn indent(&mut self, amount: usize) {
        self.out.extend(std::iter::repeat_n(' ', amount));
    }

    /// Bash's `newline`: a newline, the indentation, then `text`.
    fn newline(&mut self, text: &str) {
        self.print("\n");
        self.indent(self.indentation);
        self.print(text);
    }

    /// Bash's `semicolon`: a `;`, unless a newline or ` &` already ends the text.
    fn semicolon(&mut self) {
        if self.out.ends_with('\n') || self.out.ends_with(" &") {
            return;
        }
        self.print(";");
    }

    fn word(&mut self, word: &ast::Word) {
        let text = word_text(&word.value, self.options);
        self.print(&text);
    }

    /// Bash's `make_command_string_internal`.
    fn command(&mut self, command: &Command<'a>) {
        if self.skip_this_indent > 0 {
            self.skip_this_indent -= 1;
        } else {
            self.indent(self.indentation);
        }

        let flags = match command {
            Command::Connection { flags, .. }
            | Command::Simple(_, flags)
            | Command::Compound(_, _, flags)
            | Command::Function(_, flags)
            | Command::Empty(flags) => *flags,
            Command::If { .. } => Flags::default(),
        };
        if let Some(posix) = flags.time {
            self.print("time ");
            if posix {
                self.print("-p ");
            }
        }
        if flags.invert {
            self.print("! ");
        }

        match command {
            Command::Connection {
                first,
                connector,
                second,
                ..
            } => self.connection(first, *connector, second.as_deref()),
            Command::Simple(simple, _) => self.simple_command(simple),
            Command::Compound(compound, redirects, _) => {
                self.compound_command(compound);
                if let Some(redirects) = redirects {
                    self.print(" ");
                    self.redirection_list(&redirects.0);
                }
            }
            Command::Function(function, _) => self.function_def(function),
            Command::Empty(_) => {}
            Command::If {
                condition,
                then,
                elses,
            } => self.if_command(condition, then, elses),
        }
    }

    fn connection(
        &mut self,
        first: &Command<'a>,
        connector: Connector,
        second: Option<&Command<'a>>,
    ) {
        self.skip_this_indent += 1;
        self.printing_connection += 1;
        self.command(first);

        match connector {
            Connector::Async | Connector::Pipe => {
                let text = if connector == Connector::Async {
                    " &"
                } else {
                    " |"
                };
                self.print_deferred_heredocs(text);
                if connector != Connector::Async || second.is_some() {
                    self.print(" ");
                    self.skip_this_indent += 1;
                }
            }
            Connector::And | Connector::Or => {
                self.print_deferred_heredocs(if connector == Connector::And {
                    " && "
                } else {
                    " || "
                });
                if second.is_some() {
                    self.skip_this_indent += 1;
                }
            }
            Connector::Semicolon | Connector::Newline => {
                let newline = connector == Connector::Newline;
                let was_newline = self.deferred_heredocs.is_empty() && !self.was_heredoc && newline;
                if self.deferred_heredocs.is_empty() {
                    if self.was_heredoc {
                        self.was_heredoc = false;
                    } else {
                        self.print(if newline { "\n" } else { ";" });
                    }
                } else {
                    self.print_deferred_heredocs(if self.inside_function_def > 0 {
                        ""
                    } else {
                        ";"
                    });
                }

                if self.inside_function_def > 0 {
                    self.print("\n");
                } else if newline && !was_newline {
                    self.print("\n");
                } else {
                    if !newline {
                        self.print(" ");
                    }
                    if second.is_some() {
                        self.skip_this_indent += 1;
                    }
                }
            }
        }

        if let Some(second) = second {
            self.command(second);
        }
        if self.printing_connection == 1 {
            self.print_deferred_heredocs("");
        }
        self.printing_connection -= 1;
    }

    /// A command list inside a compound command.
    fn list(&mut self, list: &'a ast::CompoundList) {
        let items: Vec<&ast::CompoundListItem> = list.0.iter().collect();
        if let Some(command) = list_command(&items) {
            self.command(&command);
        } else {
            self.print("");
        }
    }

    /// Bash's `print_simple_command`: the words (assignments and the command's) in order, then
    /// the redirections.
    fn simple_command(&mut self, simple: &'a ast::SimpleCommand) {
        let mut words = vec![];
        let mut redirects = vec![];
        let options = self.options;
        let take = |items: &'a [ast::CommandPrefixOrSuffixItem],
                    words: &mut Vec<String>,
                    redirects: &mut Vec<&'a ast::IoRedirect>| {
            for item in items {
                match item {
                    ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                        redirects.push(redirect);
                    }
                    ast::CommandPrefixOrSuffixItem::Word(word) => {
                        words.push(word_text(&word.value, options));
                    }
                    ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) => {
                        words.push(assignment_text(assignment, word, options));
                    }
                    ast::CommandPrefixOrSuffixItem::ProcessSubstitution(kind, subshell) => {
                        words.push(process_substitution_text(kind, subshell, options));
                    }
                }
            }
        };
        if let Some(prefix) = &simple.prefix {
            take(&prefix.0, &mut words, &mut redirects);
        }
        if let Some(name) = &simple.word_or_name {
            words.push(word_text(&name.value, options));
        }
        if let Some(suffix) = &simple.suffix {
            take(&suffix.0, &mut words, &mut redirects);
        }
        self.print(&words.join(" "));
        if !redirects.is_empty() {
            if !words.is_empty() {
                self.print(" ");
            }
            self.redirection_list_refs(&redirects);
        }
    }

    fn redirection_list(&mut self, redirects: &'a [ast::IoRedirect]) {
        let redirects: Vec<&'a ast::IoRedirect> = redirects.iter().collect();
        self.redirection_list_refs(&redirects);
    }

    /// Bash's `print_redirection_list`: the redirections on one line, then the bodies of its
    /// here-documents, or, inside a connection, after its connector.
    fn redirection_list_refs(&mut self, redirects: &[&'a ast::IoRedirect]) {
        let mut heredocs = vec![];
        self.was_heredoc = false;
        for (index, redirect) in redirects.iter().enumerate() {
            if let ast::IoRedirect::HereDocument(fd, doc) = redirect {
                self.heredoc_header(*fd, doc);
                heredocs.push(HereDoc { doc });
            } else {
                self.redirection(redirect);
            }
            if index + 1 < redirects.len() {
                self.print(" ");
            }
        }
        if !heredocs.is_empty() {
            if self.printing_connection > 0 {
                self.deferred_heredocs = heredocs;
            } else {
                self.heredoc_bodies(&heredocs);
            }
        }
    }

    fn heredoc_header(&mut self, fd: Option<ast::IoFd>, doc: &ast::IoHereDocument) {
        if let Some(fd) = fd.filter(|fd| *fd != 0) {
            self.print(&fd.to_string());
        }
        self.print("<<");
        if doc.remove_tabs {
            self.print("-");
        }
        let delimiter = doc.delimiter();
        if doc.here_end.value.contains(['\'', '"', '\\']) {
            self.print(&format!("'{}'", delimiter.replace('\'', "'\\''")));
        } else {
            self.print(&delimiter);
        }
    }

    fn heredoc_bodies(&mut self, heredocs: &[HereDoc<'_>]) {
        self.print("\n");
        for heredoc in heredocs {
            self.print(&heredoc.doc.doc.value);
            self.print(&heredoc.doc.delimiter());
            self.print("\n");
        }
        self.was_heredoc = true;
    }

    /// Bash's `print_deferred_heredocs`: the connector (a lone `;` is left out), then the bodies
    /// of the here-documents waiting for it.
    fn print_deferred_heredocs(&mut self, connector: &str) {
        let prints_connector = !connector.is_empty() && connector != ";";
        if prints_connector {
            self.print(connector);
        }
        if !self.deferred_heredocs.is_empty() {
            let heredocs = std::mem::take(&mut self.deferred_heredocs);
            self.heredoc_bodies(&heredocs);
            if prints_connector {
                self.print(" ");
            }
            self.was_heredoc = true;
        }
    }

    /// Bash's `print_redirection`.
    fn redirection(&mut self, redirect: &ast::IoRedirect) {
        let text = match redirect {
            ast::IoRedirect::File(fd, kind, target) => {
                self.file_redirection(None, *fd, kind, target)
            }
            ast::IoRedirect::NamedFd(variable, kind, target) => {
                let named = format!("{{{variable}}}");
                self.file_redirection(Some(&named), None, kind, target)
            }
            ast::IoRedirect::OutputAndError(target, append) => format!(
                "&>{} {}",
                if *append { ">" } else { "" },
                word_text(&target.value, self.options)
            ),
            ast::IoRedirect::HereString(fd, word) => {
                let fd = fd.filter(|fd| *fd != 0).map(|fd| fd.to_string());
                format!(
                    "{}<<< {}",
                    fd.unwrap_or_default(),
                    word_text(&word.value, self.options)
                )
            }
            ast::IoRedirect::HereDocument(fd, doc) => {
                self.heredoc_header(*fd, doc);
                self.print("\n");
                self.print(&doc.doc.value);
                self.print(&doc.delimiter());
                return;
            }
        };
        self.print(&text);
    }

    /// A file redirection or duplication. `named` is how its descriptor is written when it has
    /// one (`2`, `{fd}`); a default descriptor is left out where bash leaves it out.
    fn file_redirection(
        &self,
        named: Option<&str>,
        fd: Option<ast::IoFd>,
        kind: &ast::IoFileRedirectKind,
        target: &ast::IoFileRedirectTarget,
    ) -> String {
        use ast::IoFileRedirectKind as Kind;
        let input = matches!(kind, Kind::Read | Kind::ReadAndWrite | Kind::DuplicateInput);
        let default_fd = if input { 0 } else { 1 };
        // The descriptor as bash writes it when it may leave it out.
        let optional = || match (named, fd) {
            (Some(named), None) => named.to_owned(),
            (_, Some(fd)) if fd != default_fd => fd.to_string(),
            _ => String::new(),
        };
        // The descriptor as bash always writes it (duplications and closings).
        let always = || match (named, fd) {
            (Some(named), None) => named.to_owned(),
            (_, Some(fd)) => fd.to_string(),
            _ => default_fd.to_string(),
        };
        let target_text = match target {
            ast::IoFileRedirectTarget::Filename(word)
            | ast::IoFileRedirectTarget::Duplicate(word) => word_text(&word.value, self.options),
            ast::IoFileRedirectTarget::Fd(fd) => fd.to_string(),
            ast::IoFileRedirectTarget::ProcessSubstitution(kind, subshell) => {
                process_substitution_text(kind, subshell, self.options)
            }
        };
        match kind {
            Kind::Read => format!("{}< {target_text}", optional()),
            Kind::Write => format!("{}> {target_text}", optional()),
            Kind::Append => format!("{}>> {target_text}", optional()),
            Kind::ReadAndWrite => format!("{}<> {target_text}", optional()),
            Kind::Clobber => format!("{}>| {target_text}", optional()),
            Kind::DuplicateInput | Kind::DuplicateOutput => {
                let op = if matches!(kind, Kind::DuplicateInput) {
                    "<&"
                } else {
                    ">&"
                };
                let numeric =
                    |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
                if target_text == "-" {
                    // Closing is written with `>&`, whichever way it was written.
                    format!("{}>&-", always())
                } else if matches!(target, ast::IoFileRedirectTarget::Fd(_))
                    || numeric(&target_text)
                    || target_text.strip_suffix('-').is_some_and(numeric)
                {
                    // A duplication or move of a descriptor number names both descriptors.
                    format!("{}{op}{target_text}", always())
                } else {
                    format!("{}{op}{target_text}", optional())
                }
            }
        }
    }

    fn compound_command(&mut self, compound: &'a ast::CompoundCommand) {
        match compound {
            ast::CompoundCommand::ForClause(clause) => {
                self.print(&format!("for {} in ", clause.variable_name));
                self.word_list(clause.values.as_deref());
                self.print(";");
                self.newline("do\n");
                self.body_with_semicolon(&clause.body.list);
                self.newline("done");
            }
            ast::CompoundCommand::SelectClause(clause) => {
                self.print(&format!("select {} in ", clause.variable_name));
                self.word_list(clause.values.as_deref());
                self.print(";");
                self.newline("do\n");
                self.body_with_semicolon(&clause.body.list);
                self.newline("done");
            }
            ast::CompoundCommand::ArithmeticForClause(clause) => {
                let part = |expr: &Option<ast::UnexpandedArithmeticExpr>| {
                    expr.as_ref()
                        .map_or(String::new(), |expr| expr.value.clone())
                };
                self.print(&format!(
                    "for (({}; {}; {}))",
                    part(&clause.initializer),
                    part(&clause.condition),
                    part(&clause.updater)
                ));
                self.newline("do\n");
                self.body_with_semicolon(&clause.body.list);
                self.newline("done");
            }
            ast::CompoundCommand::CaseClause(clause) => {
                self.print("case ");
                self.word(&clause.value);
                self.print(" in ");
                if !clause.cases.is_empty() {
                    self.case_clauses(&clause.cases);
                }
                self.newline("esac");
            }
            ast::CompoundCommand::WhileClause(clause) => self.until_or_while(clause, "while"),
            ast::CompoundCommand::UntilClause(clause) => self.until_or_while(clause, "until"),
            ast::CompoundCommand::IfClause(clause) => self.if_command(
                &clause.condition,
                &clause.then,
                clause.elses.as_deref().unwrap_or_default(),
            ),
            ast::CompoundCommand::Arithmetic(arithmetic) => {
                self.print(&format!("(({}))", arithmetic.expr.value));
            }
            ast::CompoundCommand::ExtendedTest(test) => {
                self.print(&format!("[[ {} ]]", test.expr));
            }
            ast::CompoundCommand::BraceGroup(group) => self.group_command(&group.list),
            ast::CompoundCommand::Subshell(subshell) => {
                self.print("( ");
                self.skip_this_indent += 1;
                self.list(&subshell.list);
                self.print_deferred_heredocs("");
                self.print(" )");
                self.was_heredoc = false;
            }
            ast::CompoundCommand::Coprocess(coproc) => {
                self.print("coproc ");
                if !matches!(*coproc.body, ast::Command::Simple(_)) {
                    let name = coproc
                        .name
                        .as_ref()
                        .map_or_else(|| "COPROC".to_owned(), |name| name.value.clone());
                    self.print(&format!("{name} "));
                }
                self.skip_this_indent += 1;
                let command = single_command(&coproc.body);
                self.command(&command);
            }
        }
    }

    /// A loop's body: indented, ending with a `;` (bash's `print_for_command` and friends).
    fn body_with_semicolon(&mut self, list: &'a ast::CompoundList) {
        self.indentation += INDENTATION_AMOUNT;
        self.list(list);
        self.print_deferred_heredocs("");
        self.semicolon();
        self.indentation -= INDENTATION_AMOUNT;
    }

    fn word_list(&mut self, words: Option<&[ast::Word]>) {
        match words {
            // A loop without `in` loops over `"$@"`, which bash writes out.
            None => self.print("\"$@\""),
            Some(words) => {
                let words: Vec<String> = words
                    .iter()
                    .map(|word| word_text(&word.value, self.options))
                    .collect();
                self.print(&words.join(" "));
            }
        }
    }

    fn case_clauses(&mut self, cases: &'a [ast::CaseItem]) {
        self.indentation += INDENTATION_AMOUNT;
        for (index, case) in cases.iter().enumerate() {
            // The first pattern follows `in` on its line, so the text reads back as a case.
            if index > 0 {
                self.newline("");
            }
            if case
                .patterns
                .first()
                .is_some_and(|pattern| pattern.value == "esac")
            {
                self.print("(");
            }
            let patterns: Vec<String> = case
                .patterns
                .iter()
                .map(|pattern| word_text(&pattern.value, self.options))
                .collect();
            self.print(&patterns.join(" | "));
            self.print(")\n");
            self.indentation += INDENTATION_AMOUNT;
            if let Some(list) = &case.cmd {
                self.list(list);
            }
            self.indentation -= INDENTATION_AMOUNT;
            self.print_deferred_heredocs("");
            self.newline(match case.post_action {
                ast::CaseItemPostAction::ExitCase => ";;",
                ast::CaseItemPostAction::UnconditionallyExecuteNextCaseItem => ";&",
                ast::CaseItemPostAction::ContinueEvaluatingCases => ";;&",
            });
        }
        self.indentation -= INDENTATION_AMOUNT;
    }

    fn until_or_while(&mut self, clause: &'a ast::WhileOrUntilClauseCommand, which: &str) {
        self.print(which);
        self.print(" ");
        self.skip_this_indent += 1;
        self.list(&clause.0);
        self.print_deferred_heredocs("");
        self.semicolon();
        if self.was_heredoc {
            self.indent(self.indentation);
            self.print("do\n");
            self.was_heredoc = false;
        } else {
            self.print(" do\n");
        }
        self.indentation += INDENTATION_AMOUNT;
        self.list(&clause.1.list);
        self.print_deferred_heredocs("");
        self.indentation -= INDENTATION_AMOUNT;
        self.semicolon();
        self.newline("done");
    }

    fn if_command(
        &mut self,
        condition: &'a ast::CompoundList,
        then: &'a ast::CompoundList,
        elses: &'a [ast::ElseClause],
    ) {
        self.print("if ");
        self.skip_this_indent += 1;
        self.list(condition);
        self.print_deferred_heredocs("");
        self.semicolon();
        if self.was_heredoc {
            self.indent(INDENTATION_AMOUNT);
            self.print("then\n");
            self.was_heredoc = false;
        } else {
            self.print(" then\n");
        }
        self.indentation += INDENTATION_AMOUNT;
        self.list(then);
        self.print_deferred_heredocs("");
        self.indentation -= INDENTATION_AMOUNT;

        if let Some((first, rest)) = elses.split_first() {
            self.semicolon();
            self.newline("else\n");
            self.indentation += INDENTATION_AMOUNT;
            match &first.condition {
                // `elif` is an `if` in the `else` part.
                Some(condition) => {
                    let nested = Command::If {
                        condition,
                        then: &first.body,
                        elses: rest,
                    };
                    self.command(&nested);
                }
                None => self.list(&first.body),
            }
            self.print_deferred_heredocs("");
            self.indentation -= INDENTATION_AMOUNT;
        }
        self.semicolon();
        self.newline("fi");
    }

    fn group_command(&mut self, list: &'a ast::CompoundList) {
        self.print("{ ");
        if self.inside_function_def == 0 {
            self.skip_this_indent += 1;
        } else {
            self.print("\n");
            self.indentation += INDENTATION_AMOUNT;
        }
        self.list(list);
        self.print_deferred_heredocs("");
        if self.inside_function_def > 0 {
            self.print("\n");
            self.indentation -= INDENTATION_AMOUNT;
            self.indent(self.indentation);
        } else {
            self.semicolon();
            self.print(" ");
        }
        self.print("}");
        self.was_heredoc = false;
    }

    /// Bash's `print_function_def`, as a function defined in a substitution is printed back.
    fn function_def(&mut self, function: &'a ast::FunctionDefinition) {
        let name = &function.fname.value;
        if self.options.posix_mode && valid_function_name(name) {
            self.print(&format!("{name} () \n"));
        } else {
            self.print(&format!("function {name} () \n"));
        }
        self.indent(self.indentation);
        self.print("{ \n");
        self.inside_function_def += 1;
        self.indentation += INDENTATION_AMOUNT;
        let ast::FunctionBody(body, redirects) = &function.body;
        match body {
            ast::CompoundCommand::BraceGroup(group) => self.list(&group.list),
            body => {
                let command = Command::Compound(body, None, Flags::default());
                self.command(&command);
            }
        }
        self.print_deferred_heredocs("");
        self.indentation -= INDENTATION_AMOUNT;
        self.inside_function_def -= 1;
        if let Some(redirects) = redirects {
            self.newline("} ");
            self.redirection_list(&redirects.0);
        } else {
            self.newline("}");
            self.was_heredoc = false;
        }
    }
}

/// An assignment word as bash keeps it: an array's elements one blank apart.
fn assignment_text(
    assignment: &ast::Assignment,
    word: &ast::Word,
    options: &ParserOptions,
) -> String {
    match &assignment.value {
        ast::AssignmentValue::Array(elements) => {
            let name = match &assignment.name {
                ast::AssignmentName::VariableName(name) => name.clone(),
                ast::AssignmentName::ArrayElementName(name, index) => format!("{name}[{index}]"),
            };
            let op = if assignment.append { "+=" } else { "=" };
            let elements: Vec<String> = elements
                .iter()
                .map(|(key, value)| {
                    let value = word_text(&value.value, options);
                    match key {
                        Some(key) => format!("[{}]={value}", word_text(&key.value, options)),
                        None => value,
                    }
                })
                .collect();
            format!("{name}{op}({})", elements.join(" "))
        }
        ast::AssignmentValue::Scalar(_) => word_text(&word.value, options),
    }
}

/// A process substitution as bash keeps it in the word it is part of.
fn process_substitution_text(
    kind: &ast::ProcessSubstitutionKind,
    subshell: &ast::SubshellCommand,
    options: &ParserOptions,
) -> String {
    let printed = print_comsub_list(&subshell.list, options);
    let blank = if printed.starts_with('(') { " " } else { "" };
    format!("{kind}({blank}{printed})")
}

/// Whether `name` is a function name POSIX allows (bash's `valid_function_name` in POSIX mode):
/// a name, and not a reserved word.
fn valid_function_name(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "!", "[[", "]]", "case", "coproc", "do", "done", "elif", "else", "esac", "fi", "for",
        "function", "if", "in", "select", "then", "time", "until", "while", "{", "}",
    ];
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !RESERVED.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::reprint_comsub_text;

    /// Command substitution bodies and the text bash 5.3 keeps for each (recorded from
    /// `declare -f` of a function holding `$(body)`, or `$( body)` for a body that starts with
    /// `(`, which bash would otherwise read as arithmetic).
    const BASH: &[(&str, &str)] = &[
        ("a\nb", "a\nb"),
        ("\n  true\n  nosuchA\n", "true\nnosuchA"),
        ("true; nosuchB", "true; nosuchB"),
        ("true\n\n\nnosuchC", "true\nnosuchC"),
        (
            "if true; then\n nosuchD\nfi",
            "if true; then\n    nosuchD;\nfi",
        ),
        (
            "true; if true; then nosuchE; fi",
            "true; if true; then\n    nosuchE;\nfi",
        ),
        ("# comment\n  nosuchF\n", "nosuchF"),
        ("true &&\n  nosuchG", "true && nosuchG"),
        (
            "f() {\n  nosuchH\n}\nf",
            "function f () \n{ \n    nosuchH\n}\nf",
        ),
        (
            "for i in 1; do\n  true\n  nosuchI\ndone",
            "for i in 1;\ndo\n    true\nnosuchI;\ndone",
        ),
        ("cat <<E\nhi\nE\nnosuchJ", "cat <<E\nhi\nE\n\nnosuchJ"),
        ("echo \"a\nb\"; nosuchK", "echo \"a\nb\"; nosuchK"),
        (
            "if true; then a; b\nc; fi",
            "if true; then\n    a; b\nc;\nfi",
        ),
        ("a;\nb", "a; b"),
        ("a &\nb", "a & b"),
        ("a & b", "a & b"),
        ("a &", "a &"),
        ("a | b | c", "a | b | c"),
        ("a |\n b", "a | b"),
        ("! a | b", "! a | b"),
        ("time a", "time a"),
        ("time -p a | b", "time -p a | b"),
        ("{ a; b; }", "{ a; b; }"),
        ("{ a\nb\n}", "{ a\nb; }"),
        ("( a; b )", " ( a; b )"),
        ("while a; do b; done", "while a; do\n    b;\ndone"),
        ("until a\ndo\nb\ndone", "until a; do\n    b;\ndone"),
        (
            "case x in a) b;; c|d) e;& f) g;;& esac",
            "case x in a)\n        b\n    ;;\n    c | d)\n        e\n    ;&\n    f)\n        g\n    ;;&\nesac",
        ),
        (
            "case x in\n a)\n  b\n  ;;\nesac",
            "case x in a)\n        b\n    ;;\nesac",
        ),
        (
            "for ((i=0; i<3; i++)); do a; done",
            "for ((i=0; i<3; i++))\ndo\n    a;\ndone",
        ),
        ("for i; do a; done", "for i in \"$@\";\ndo\n    a;\ndone"),
        ("for i in; do a; done", "for i in ;\ndo\n    a;\ndone"),
        (
            "select x in a b; do c; done",
            "select x in a b;\ndo\n    c;\ndone",
        ),
        ("(( i++ ))", " (( i++ ))"),
        ("((  1  ))", " ((  1  ))"),
        ("[[ -n $a && ! -z b ]]", "[[ -n $a && ! -z b ]]"),
        ("a > f 2>&1 < g", "a > f 2>&1 < g"),
        (">f a b", "a b > f"),
        ("x=1 y=2 a", "x=1 y=2 a"),
        (
            "a 2>/dev/null >>f 3<>g 4<&0 5>&- <<< str",
            "a 2> /dev/null >> f 3<> g 4<&0 5>&- <<< str",
        ),
        ("cat <<-E\n\tx\n\tE\n", "cat <<-E\nx\nE\n"),
        ("cat <<'E'\nx\nE", "cat <<'E'\nx\nE\n"),
        ("cat <<E; d\nx\nE", "cat <<E\nx\nE\n d"),
        ("cat <<E | wc\nx\nE", "cat <<E |\nx\nE\n  wc"),
        ("cat <<E && d\nx\nE", "cat <<E && \nx\nE\n d"),
        ("{ a; } > f", "{ a; } > f"),
        ("g() { a; } >f; g", "function g () \n{ \n    a\n} > f; g"),
        (
            "function g { a\nb\n}",
            "function g () \n{ \n    a\n\n    b\n}",
        ),
        ("a $(b\nc) d", "a $(b\nc) d"),
        ("echo `a\nb`", "echo `a\nb`"),
        ("a && b || c", "a && b || c"),
        ("a &&\n\nb", "a && b"),
        ("a; b &", "a; b &"),
        ("a & b &", "a & b &"),
        ("{ a & }", "{ a & }"),
        ("while a\ndo\n  b &\ndone", "while a; do\n    b &\ndone"),
        (
            "if a; then b; elif c; then d; else e; fi",
            "if a; then\n    b;\nelse\n    if c; then\n        d;\n    else\n        e;\n    fi;\nfi",
        ),
        ("echo 'x\ny'", "echo 'x\ny'"),
        ("a # c\nb", "a\nb"),
        ("a\n# c\nb", "a\nb"),
        ("a ; b", "a; b"),
        (
            "case x in (a) b;; esac",
            "case x in a)\n        b\n    ;;\nesac",
        ),
        ("a | b &", "a | b &"),
        ("[[ a =~ ^b ]]", "[[ a =~ ^b ]]"),
        ("g() ( a )", "function g () \n{ \n    ( a )\n}"),
        ("{ a\n} 2>/dev/null", "{ a; } 2> /dev/null"),
        ("if a\nthen\n  b\nfi > f", "if a; then\n    b;\nfi > f"),
        ("{ a; b; } &", "{ a; b; } &"),
        ("a && { b; c; }", "a && { b; c; }"),
        (
            "for i in 1 2; do\n  for j in 3; do\n    a\n  done\ndone",
            "for i in 1 2;\ndo\n    for j in 3;\n    do\n        a;\n    done;\ndone",
        ),
        (
            "if a; then\n  if b; then\n    c\n  fi\nfi",
            "if a; then\n    if b; then\n        c;\n    fi;\nfi",
        ),
        ("echo \"$(a\nb)\"", "echo \"$(a\nb)\""),
        ("a 2>&1 | b", "a 2>&1 | b"),
        ("a |& b", "a 2>&1 | b"),
        ("a\n", "a"),
        ("a;", "a"),
        ("", ""),
        ("\n\n", ""),
        ("cat <<E\nx\nE", "cat <<E\nx\nE\n"),
        ("cat <<E\nx\nE\n", "cat <<E\nx\nE\n"),
        ("a <<E <<F\n1\nE\n2\nF\nb", "a <<E <<F\n1\nE\n2\nF\n\nb"),
        ("case x in a) ;; esac", "case x in a)\n\n    ;;\nesac"),
        ("case x in\nesac", "case x in \nesac"),
        (
            "while cat <<E; do b; done\nx\nE",
            "while cat <<E\nx\nE\ndo\n    b;\ndone",
        ),
        (
            "if cat <<E\nx\nE\nthen b; fi",
            "if cat <<E\nx\nE\n    then\n    b;\nfi",
        ),
        (
            "a | while read l; do b; done",
            "a | while read l; do\n    b;\ndone",
        ),
        ("x=(1 2) y=([a]=b) c", "x=(1 2) y=([a]=b) c"),
        ("declare -A m=([k]=v)", "declare -A m=([k]=v)"),
        ("a >&2", "a 1>&2"),
        ("a 1>&2 2>&1 0<&3", "a 1>&2 2>&1 0<&3"),
        ("a {fd}>f {fd}>&-", "a {fd}> f {fd}>&-"),
        ("exec 3>&-", "exec 3>&-"),
        ("a &>f &>>g", "a &> f &>> g"),
        ("a >|f", "a >| f"),
        ("a <f >g", "a < f > g"),
        ("{ a\nb; }", "{ a\nb; }"),
        ("( a\nb )", " ( a\nb )"),
        (
            "f() {\n  a\n  b\n}",
            "function f () \n{ \n    a\n\n    b\n}",
        ),
        (
            "f() { a; b; }\ng() { c; }",
            "function f () \n{ \n    a;\n    b\n}\nfunction g () \n{ \n    c\n}",
        ),
        ("function h () { a; }", "function h () \n{ \n    a\n}"),
        ("coproc a", "coproc a"),
        ("coproc N { a; }", "coproc N { a; }"),
        ("! { a; }", "! { a; }"),
        ("time { a; }", "time { a; }"),
        ("a || ! b", "a || ! b"),
        ("if ! a; then b; fi", "if ! a; then\n    b;\nfi"),
        ("a && (b)", "a && ( b )"),
        ("x=$((1+2)) a", "x=$((1+2)) a"),
        ("echo $[1+2]", "echo $[1+2]"),
        ("a\\\nb", "ab"),
        ("a 'b\nc' d", "a 'b\nc' d"),
        ("a\t\tb", "a b"),
        (
            "for i in a\\ b 'c d'; do e; done",
            "for i in a\\ b 'c d';\ndo\n    e;\ndone",
        ),
        (
            "case $x in *) a;; esac",
            "case $x in *)\n        a\n    ;;\nesac",
        ),
        ("a >f; b <g", "a > f; b < g"),
        ("arr=(\n 1\n 2\n)", "arr=(1 2)"),
    ];

    #[test]
    fn substitutions_are_printed_back_as_bash_prints_them() {
        let options = crate::ParserOptions::default();
        let mut failures = vec![];
        for (body, expected) in BASH {
            let printed = reprint_comsub_text(body, &options);
            if printed.as_deref() != Some(*expected) {
                failures.push(format!("{body:?}: expected {expected:?}, got {printed:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
