//! Execution: walking a parsed [`List`] and running what it describes.
//!
//! **[`Flow`]** is how `break`, `continue`, `return` and `exit` leave a
//! nested construct. A builtin cannot return one — its signature is a status —
//! so it *requests* one and the executor honours it where it can.
//!
//! **Where a command runs** is per command: a builtin, a function and a
//! compound command run in this shell so `cd`, an assignment and a loop
//! counter survive; an external program, a `( )` subshell and every pipeline
//! stage run in a fork.
//!
//! **Redirections** follow. In-shell, they are applied around the command and
//! undone after; in a fork, applied in the child, so an unopenable path is the
//! child's status and the shell's descriptors are never at risk.

use core::ffi::c_char;
use core::ptr;

use crate::program_registry;
use crate::syscall::{UserFsStat, core as sys_core, fs, process};
use slopos_abi::fs::{O_APPEND, O_CREAT, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};
use slopos_abi::signal::WUNTRACED;
use slopos_shell_core::ast::{
    AndOr, AndOrOp, Command, CommandKind, List, Pipeline, RedirKind, RedirTarget, Redirect, Word,
};
use slopos_shell_core::lexer::{self, LexError};
use slopos_shell_core::syntax::{self, ParseError};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use super::buffers;
use super::buffers::ParsedTokens;
use super::builtins;
use super::display::{shell_error, shell_error_named, shell_write};
use super::parser::normalize_path;
use super::{env, expand, funcs, jobs};

/// POSIX reserves 2 for the shell's own usage errors, distinct from any status
/// a command could return.
pub const STATUS_SYNTAX_ERROR: i32 = 2;
/// The command was found but could not be executed.
pub const STATUS_CANNOT_EXECUTE: i32 = 126;
pub const STATUS_NOT_FOUND: i32 = 127;

/// Signals a forked job resets to SIG_DFL: the shell catches SIGINT and ignores
/// SIGTTOU/SIGTTIN/SIGTSTP, none of which a launched job should inherit.
const JOB_CONTROL_DEFAULT_SIGNALS: slopos_abi::signal::SigSet = {
    use slopos_abi::signal::{SIGINT, SIGTSTP, SIGTTIN, SIGTTOU, sig_bit};
    sig_bit(SIGINT) | sig_bit(SIGTTOU) | sig_bit(SIGTTIN) | sig_bit(SIGTSTP)
};

/// How a construct ended, beside its status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Normal,
    /// `break n` — leave this many enclosing loops.
    Break(u32),
    /// `continue n` — restart the nth enclosing loop.
    Continue(u32),
    /// `return` — leave the running function or sourced file.
    Return,
    /// `exit` — leave the shell.
    Exit,
}

#[derive(Clone, Copy, Debug)]
pub struct Outcome {
    pub status: i32,
    pub flow: Flow,
}

impl Outcome {
    fn normal(status: i32) -> Self {
        Self {
            status,
            flow: Flow::Normal,
        }
    }
}

/// Control flow a builtin asked for. A builtin returns a status, so this is
/// how `break`, `continue` and `return` reach the executor.
static PENDING_FLOW: Mutex<Option<Flow>> = Mutex::new(None);

pub fn request_flow(flow: Flow) {
    *PENDING_FLOW.lock().unwrap() = Some(flow);
}

fn take_flow() -> Flow {
    PENDING_FLOW.lock().unwrap().take().unwrap_or(Flow::Normal)
}

/// Enclosing loops, so `break` outside one is a diagnostic rather than an
/// escape from the script.
static LOOP_DEPTH: AtomicU32 = AtomicU32::new(0);
/// Enclosing function calls and sourced files, which is what `return` needs.
static RETURN_DEPTH: AtomicU32 = AtomicU32::new(0);
/// Enclosing condition contexts. `set -e` deliberately does not fire inside
/// one: `if grep -q x f; then` is a test, not a failure.
static COND_DEPTH: AtomicU32 = AtomicU32::new(0);

pub fn in_loop() -> bool {
    LOOP_DEPTH.load(Ordering::Relaxed) > 0
}

pub fn in_function() -> bool {
    RETURN_DEPTH.load(Ordering::Relaxed) > 0
}

struct Depth(&'static AtomicU32);

impl Depth {
    fn enter(cell: &'static AtomicU32) -> Self {
        cell.fetch_add(1, Ordering::Relaxed);
        Depth(cell)
    }
}

impl Drop for Depth {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

static FOREGROUND_PGID: AtomicU32 = AtomicU32::new(0);
static SHELL_PGID: AtomicU32 = AtomicU32::new(0);

pub fn foreground_pgid() -> u32 {
    FOREGROUND_PGID.load(Ordering::Relaxed)
}

pub fn set_foreground_pgid(pgid: u32) {
    FOREGROUND_PGID.store(pgid, Ordering::Relaxed);
}

pub fn clear_foreground_pgid() {
    FOREGROUND_PGID.store(0, Ordering::Relaxed);
}

/// Claim the terminal and become a session leader — interactive shells only.
///
/// A shell running a script must stay in the process group its parent placed it
/// in: that group is the terminal's foreground group, so a Ctrl+C the user
/// aimed at the pipeline reaches the script *and* the commands it is running.
pub fn initialize_job_control() {
    if !super::is_interactive() {
        return;
    }

    // A successful TIOCGSID means another session already owns this terminal;
    // detaching it would leave them unable to read their own input.
    if fs::tcgetsid(0).is_err() {
        let _ = process::setsid();
        let _ = fs::tiocsctty(0);
    }

    // Also what a forked child inherits as *ignored*, for long enough to
    // claim the terminal before it resets them.
    process::ignore_signal(slopos_abi::signal::SIGTTOU);
    process::ignore_signal(slopos_abi::signal::SIGTTIN);
    process::ignore_signal(slopos_abi::signal::SIGTSTP);

    let _ = process::setpgid(0, 0);
    let shell_pgid = process::getpgid(0);
    if shell_pgid > 0 {
        SHELL_PGID.store(shell_pgid as u32, Ordering::Relaxed);
        let _ = fs::tcsetpgrp(0, shell_pgid as u32);
    }
}

fn shell_pgid() -> u32 {
    SHELL_PGID.load(Ordering::Relaxed)
}

pub fn enter_foreground(pgid: u32) {
    if pgid == 0 || !super::is_interactive() {
        return;
    }
    set_foreground_pgid(pgid);
    let _ = fs::tcsetpgrp(0, pgid);
}

pub fn leave_foreground() {
    if !super::is_interactive() {
        return;
    }
    let pgid = shell_pgid();
    if pgid != 0 {
        let _ = fs::tcsetpgrp(0, pgid);
    }
    clear_foreground_pgid();
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Why a line of text could not be turned into a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseFailure {
    /// The text ends mid-construct. Appending more input may complete it,
    /// which is what a script reader and a PS2 prompt both do.
    Incomplete,
    Syntax(&'static str),
}

pub fn parse_text(text: &[u8]) -> Result<List, ParseFailure> {
    let toks = lexer::lex(text).map_err(|e| match e {
        LexError::Incomplete => ParseFailure::Incomplete,
        LexError::Syntax(msg) => ParseFailure::Syntax(msg),
    })?;
    syntax::parse(&toks).map_err(|e| match e {
        ParseError::Incomplete => ParseFailure::Incomplete,
        ParseError::Syntax(msg) => ParseFailure::Syntax(msg),
    })
}

pub fn report_parse_failure(failure: ParseFailure) {
    match failure {
        ParseFailure::Incomplete => shell_error(b"sh: syntax error: unexpected end of input\n"),
        ParseFailure::Syntax(msg) => {
            shell_error_named(b"syntax error", msg.as_bytes());
            true
        }
    };
}

/// Run one complete command text — a whole line, or a whole compound command
/// gathered over several lines.
pub fn execute_text(text: &[u8]) -> i32 {
    match parse_text(text) {
        Ok(list) => execute_list(&list),
        Err(failure) => {
            report_parse_failure(failure);
            STATUS_SYNTAX_ERROR
        }
    }
}

/// Run an already-parsed command list as a top-level command.
pub fn execute_list(list: &List) -> i32 {
    execute_list_flow(list).status
}

/// As [`execute_list`], but the flow is visible: a script reader stops on
/// `return`, which is what ends a sourced file rather than the line.
pub fn execute_list_flow(list: &List) -> Outcome {
    super::interrupt::clear();
    let outcome = run_list(list);
    if outcome.flow == Flow::Exit && super::exit_requested().is_none() {
        super::request_exit(outcome.status);
    }
    outcome
}

/// Run `f` with `return` legal inside it: a function call, or a sourced file.
pub fn in_return_scope<R>(f: impl FnOnce() -> R) -> R {
    let _depth = Depth::enter(&RETURN_DEPTH);
    f()
}

/// Run a list from inside a builtin — `eval`, `.` — where the flow belongs to
/// the caller: a `break` in `eval`'s text must reach the loop the `eval` is
/// in, and an `exit` must not be swallowed here.
pub fn run_nested_list(list: &List) -> Outcome {
    run_list(list)
}

/// Run a command built out of pre-expanded words. The words are final: nothing
/// here expands, splits or globs them.
pub fn execute_tokens(tokens: &ParsedTokens) -> i32 {
    if tokens.count() == 0 {
        return 0;
    }
    let toks = tokens.to_syntax_tokens();
    match syntax::parse(&toks) {
        Ok(list) => execute_list(&list),
        Err(ParseError::Incomplete) => {
            report_parse_failure(ParseFailure::Incomplete);
            STATUS_SYNTAX_ERROR
        }
        Err(ParseError::Syntax(msg)) => {
            report_parse_failure(ParseFailure::Syntax(msg));
            STATUS_SYNTAX_ERROR
        }
    }
}

/// Run `text` in a subshell and collect its standard output — the mechanism
/// behind `$(...)` and backticks.
///
/// The output is data, never re-tokenized: a `;` or a `>` in it is a byte the
/// command receives, not an operator.
pub fn capture(text: &[u8]) -> Result<Vec<u8>, ()> {
    let list = match parse_text(text) {
        Ok(list) => list,
        Err(failure) => {
            report_parse_failure(failure);
            return Err(());
        }
    };

    let Ok((read_end, write_end)) = fs::pipe() else {
        shell_error(b"sh: cannot create pipe\n");
        return Err(());
    };
    let (read_fd, write_fd) = (read_end.into_raw(), write_end.into_raw());

    let pid = process::fork();
    if pid < 0 {
        let _ = fs::close_fd_raw(read_fd);
        let _ = fs::close_fd_raw(write_fd);
        shell_error(b"sh: cannot fork\n");
        return Err(());
    }
    if pid == 0 {
        let _ = fs::close_fd_raw(read_fd);
        if fs::dup2(write_fd, 1).is_err() {
            sys_core::exit_with_code(1);
        }
        let _ = fs::close_fd_raw(write_fd);
        super::interrupt::mark_forked_child();
        let outcome = run_list(&list);
        sys_core::exit_with_code(outcome.status);
    }

    let _ = fs::close_fd_raw(write_fd);
    let mut out = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match fs::read_slice(read_fd, &mut chunk) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(e) if e == crate::syscall::SyscallError::EINTR => continue,
            Err(_) => break,
        }
    }
    let _ = fs::close_fd_raw(read_fd);
    let status = process::wait_exit_code(pid as u32);
    super::set_last_exit_code(status);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Lists, and-or lists and pipelines
// ---------------------------------------------------------------------------

fn run_list(list: &List) -> Outcome {
    let mut status = super::last_exit_code();
    for item in &list.items {
        if item.background {
            status = run_background(&item.andor);
            super::set_last_exit_code(status);
            continue;
        }
        let (outcome, errexit_applies) = run_and_or(&item.andor);
        status = outcome.status;
        super::set_last_exit_code(status);
        if outcome.flow != Flow::Normal {
            return Outcome {
                status,
                flow: outcome.flow,
            };
        }
        if super::exit_requested().is_some() {
            return Outcome {
                status,
                flow: Flow::Exit,
            };
        }
        if funcs::errexit()
            && errexit_applies
            && status != 0
            && COND_DEPTH.load(Ordering::Relaxed) == 0
        {
            return Outcome {
                status,
                flow: Flow::Exit,
            };
        }
    }
    Outcome::normal(status)
}

/// The outcome, and whether `set -e` may act on its status: POSIX exempts
/// every command of an and-or list but the last, and a `!` pipeline.
fn run_and_or(and_or: &AndOr) -> (Outcome, bool) {
    let mut outcome = run_pipeline_cond(&and_or.first, !and_or.rest.is_empty());
    let mut errexit_applies = and_or.rest.is_empty() && !and_or.first.negate;
    if outcome.flow != Flow::Normal {
        return (outcome, errexit_applies);
    }
    for (index, (op, pipeline)) in and_or.rest.iter().enumerate() {
        let should_run = match op {
            AndOrOp::And => outcome.status == 0,
            AndOrOp::Or => outcome.status != 0,
        };
        if !should_run {
            continue;
        }
        let last = index + 1 == and_or.rest.len();
        outcome = run_pipeline_cond(pipeline, !last);
        errexit_applies = last && !pipeline.negate;
        if outcome.flow != Flow::Normal {
            return (outcome, errexit_applies);
        }
    }
    (outcome, errexit_applies)
}

/// A pipeline whose status another operator is about to judge is a condition,
/// so `set -e` must not fire on it.
fn run_pipeline_cond(pipeline: &Pipeline, is_condition: bool) -> Outcome {
    if is_condition || pipeline.negate {
        let _depth = Depth::enter(&COND_DEPTH);
        run_pipeline(pipeline, false)
    } else {
        run_pipeline(pipeline, false)
    }
}

/// Evaluate a list purely for its status — an `if`/`while` condition.
fn run_condition(list: &List) -> Outcome {
    let _depth = Depth::enter(&COND_DEPTH);
    run_list(list)
}

fn run_background(and_or: &AndOr) -> i32 {
    // A single pipeline has job-control machinery of its own; anything more
    // has to become a subshell, because `a && b &` backgrounds the pair.
    if and_or.rest.is_empty() {
        return run_pipeline(&and_or.first, true).status;
    }
    let list = List {
        items: vec![slopos_shell_core::ast::ListItem {
            andor: and_or.clone(),
            background: false,
        }],
    };
    fork_subshell(&list, &[], true).status
}

fn run_pipeline(pipeline: &Pipeline, background: bool) -> Outcome {
    let single = pipeline.cmds.len() == 1;
    let mut outcome = if single && !background && runs_in_shell(&pipeline.cmds[0]) {
        run_command(&pipeline.cmds[0])
    } else if single {
        run_forked_command(&pipeline.cmds[0], background)
    } else {
        run_staged_pipeline(pipeline, background)
    };
    if pipeline.negate {
        outcome.status = i32::from(outcome.status == 0);
    }
    outcome
}

/// Whether this command can run in the shell's own process. A subshell must
/// not, by definition; a simple command's answer depends on what its name
/// resolves to, which is decided in [`run_simple`] after expansion.
fn runs_in_shell(cmd: &Command) -> bool {
    !matches!(cmd.kind, CommandKind::Subshell(_))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn run_command(cmd: &Command) -> Outcome {
    match &cmd.kind {
        CommandKind::Simple { assigns, words } => run_simple(cmd, assigns, words),
        CommandKind::Subshell(list) => fork_subshell(list, &cmd.redirects, false),
        _ => {
            let Some(applied) = Applied::install(&cmd.redirects) else {
                return Outcome::normal(1);
            };
            let outcome = run_compound(&cmd.kind);
            applied.undo();
            outcome
        }
    }
}

fn run_compound(kind: &CommandKind) -> Outcome {
    match kind {
        CommandKind::Group(list) => run_list(list),
        CommandKind::If { arms, otherwise } => {
            for (cond, body) in arms {
                let decision = run_condition(cond);
                if decision.flow != Flow::Normal {
                    return decision;
                }
                if decision.status == 0 {
                    return run_list(body);
                }
            }
            match otherwise {
                Some(body) => run_list(body),
                None => Outcome::normal(0),
            }
        }
        CommandKind::Loop { until, cond, body } => run_loop(*until, cond, body),
        CommandKind::For { name, words, body } => run_for(name, words.as_deref(), body),
        CommandKind::Case { word, items } => run_case(word, items),
        CommandKind::Function { name, body } => {
            funcs::define(name, body.clone());
            Outcome::normal(0)
        }
        // Handled by `run_command`, which never routes them here.
        CommandKind::Simple { .. } | CommandKind::Subshell(_) => Outcome::normal(1),
    }
}

fn run_loop(until: bool, cond: &List, body: &List) -> Outcome {
    let _depth = Depth::enter(&LOOP_DEPTH);
    let mut status = 0;
    loop {
        if super::interrupt::take_pending() {
            return Outcome {
                status: super::interrupt::EXIT_INTERRUPTED,
                flow: Flow::Normal,
            };
        }
        let decision = run_condition(cond);
        if decision.flow != Flow::Normal {
            return decision;
        }
        let proceed = if until {
            decision.status != 0
        } else {
            decision.status == 0
        };
        if !proceed {
            return Outcome::normal(status);
        }
        let outcome = run_list(body);
        status = outcome.status;
        match loop_flow(outcome.flow) {
            LoopStep::Continue => continue,
            LoopStep::Break => return Outcome::normal(status),
            LoopStep::Propagate(flow) => return Outcome { status, flow },
        }
    }
}

fn run_for(name: &[u8], words: Option<&[Word]>, body: &List) -> Outcome {
    let items = match words {
        Some(words) => match expand::word_list(words) {
            Ok(items) => items,
            Err(e) => return Outcome::normal(e.report()),
        },
        None => super::args::positional_args(),
    };

    let _depth = Depth::enter(&LOOP_DEPTH);
    let mut status = 0;
    for item in items {
        if super::interrupt::take_pending() {
            return Outcome {
                status: super::interrupt::EXIT_INTERRUPTED,
                flow: Flow::Normal,
            };
        }
        env::set(name, &item);
        let outcome = run_list(body);
        status = outcome.status;
        match loop_flow(outcome.flow) {
            LoopStep::Continue => continue,
            LoopStep::Break => break,
            LoopStep::Propagate(flow) => return Outcome { status, flow },
        }
    }
    Outcome::normal(status)
}

enum LoopStep {
    Continue,
    Break,
    Propagate(Flow),
}

/// Consume one level of a `break`/`continue` that names this loop, and pass
/// the rest outward.
fn loop_flow(flow: Flow) -> LoopStep {
    match flow {
        Flow::Normal => LoopStep::Continue,
        Flow::Break(1) => LoopStep::Break,
        Flow::Break(n) => LoopStep::Propagate(Flow::Break(n.saturating_sub(1))),
        Flow::Continue(1) => LoopStep::Continue,
        Flow::Continue(n) => LoopStep::Propagate(Flow::Continue(n.saturating_sub(1))),
        other => LoopStep::Propagate(other),
    }
}

fn run_case(word: &Word, items: &[slopos_shell_core::ast::CaseItem]) -> Outcome {
    let subject = match expand::single(word) {
        Ok(subject) => subject,
        Err(e) => return Outcome::normal(e.report()),
    };
    for item in items {
        for pat in &item.patterns {
            let pattern = match expand::as_pattern(pat) {
                Ok(pattern) => pattern,
                Err(e) => return Outcome::normal(e.report()),
            };
            if slopos_shell_core::pattern::matches(&pattern, &subject) {
                return run_list(&item.body);
            }
        }
    }
    Outcome::normal(0)
}

// ---------------------------------------------------------------------------
// Simple commands
// ---------------------------------------------------------------------------

/// A variable's value before an assignment overrode it, so a one-shot
/// `NAME=VALUE cmd` prefix can be undone once `cmd` has run.
type SavedEnv = Vec<(Vec<u8>, Option<Vec<u8>>, bool)>;

fn expand_assignments(assigns: &[Word]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, expand::ExpandError> {
    let mut out = Vec::new();
    for word in assigns {
        let Some((name, rhs)) = lexer::assignment_split(&word.text) else {
            continue;
        };
        let name = name.to_vec();
        let value = if word.literal {
            rhs.to_vec()
        } else {
            expand::single(&Word::raw(rhs.to_vec()))?
        };
        out.push((name, value));
    }
    Ok(out)
}

/// Apply a command's assignment prefix. `remember` keeps what to put back, for
/// a command that runs in this shell.
fn apply_assignments(pairs: &[(Vec<u8>, Vec<u8>)], remember: bool) -> SavedEnv {
    let mut saved = SavedEnv::new();
    for (name, value) in pairs {
        if remember {
            saved.push((name.clone(), env::get(name), env::is_exported(name)));
        }
        // POSIX: a prefix on a command is exported to that command.
        env::set_exported(name, value);
    }
    saved
}

fn restore_assignments(saved: SavedEnv) {
    for (name, previous, exported) in saved {
        match previous {
            Some(value) => {
                if exported {
                    env::set_exported(&name, &value);
                } else {
                    env::unset(&name);
                    env::set(&name, &value);
                }
            }
            None => {
                env::unset(&name);
            }
        }
    }
}

fn run_simple(cmd: &Command, assigns: &[Word], words: &[Word]) -> Outcome {
    // Cleared so what the expansions below record is theirs alone.
    expand::take_substituted();
    let argv = match expand::word_list(words) {
        Ok(argv) => argv,
        Err(e) => return Outcome::normal(e.report()),
    };
    let pairs = match expand_assignments(assigns) {
        Ok(pairs) => pairs,
        Err(e) => return Outcome::normal(e.report()),
    };
    let substituted = expand::take_substituted();

    // `NAME=VALUE` with no command sets the variable outright. The
    // redirections still happen, which is how `> f` truncates a file.
    if argv.is_empty() {
        let Some(applied) = Applied::install(&cmd.redirects) else {
            return Outcome::normal(1);
        };
        applied.undo();
        for (name, value) in &pairs {
            env::set(name, value);
        }
        // POSIX: with no command name but a substitution, the status is the
        // substitution's — what makes `x=$(cmd) || exit 1` mean anything.
        return Outcome::normal(if substituted {
            super::last_exit_code()
        } else {
            0
        });
    }

    if funcs::xtrace() {
        trace(&argv);
    }

    if let Some(body) = funcs::lookup(&argv[0]) {
        return call_function(cmd, &pairs, &argv, &body);
    }

    if let Some(entry) = builtins::find_builtin(&argv[0]) {
        let saved_env = apply_assignments(&pairs, true);
        let outcome = match Applied::install(&cmd.redirects) {
            Some(applied) => {
                let slices: Vec<&[u8]> = argv.iter().map(|a| a.as_slice()).collect();
                let status = (entry.func)(argv.len() as i32, &slices);
                applied.undo();
                Outcome {
                    status,
                    flow: take_flow(),
                }
            }
            None => Outcome::normal(1),
        };
        restore_assignments(saved_env);
        return outcome;
    }

    if let Some(spec) = registry_spec(&argv[0]) {
        let saved_env = apply_assignments(&pairs, true);
        let outcome = match Applied::install(&cmd.redirects) {
            Some(applied) => {
                let status = spawn_registry_program(spec, &argv, false);
                applied.undo();
                Outcome::normal(status)
            }
            None => Outcome::normal(1),
        };
        restore_assignments(saved_env);
        return outcome;
    }

    spawn_external(cmd, &pairs, &argv, false)
}

fn call_function(
    cmd: &Command,
    pairs: &[(Vec<u8>, Vec<u8>)],
    argv: &[Vec<u8>],
    body: &Command,
) -> Outcome {
    let saved_env = apply_assignments(pairs, true);
    let saved_args = super::args::shadow_args(argv[1..].to_vec());
    let outcome = match Applied::install(&cmd.redirects) {
        Some(applied) => {
            let _depth = Depth::enter(&RETURN_DEPTH);
            // `break` in a function body must not escape into the caller's
            // loop, so its nesting starts fresh.
            let saved_loops = LOOP_DEPTH.swap(0, Ordering::Relaxed);
            let mut outcome = run_command(body);
            LOOP_DEPTH.store(saved_loops, Ordering::Relaxed);
            applied.undo();
            if matches!(
                outcome.flow,
                Flow::Return | Flow::Break(_) | Flow::Continue(_)
            ) {
                outcome.flow = Flow::Normal;
            }
            outcome
        }
        None => Outcome::normal(1),
    };
    super::args::restore_args(saved_args);
    restore_assignments(saved_env);
    outcome
}

fn trace(argv: &[Vec<u8>]) {
    let mut line = b"+".to_vec();
    for arg in argv {
        line.push(b' ');
        line.extend_from_slice(arg);
    }
    line.push(b'\n');
    shell_error(&line);
}

// ---------------------------------------------------------------------------
// Redirections
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct SavedFd {
    fd: i32,
    backup: i32,
}

/// Redirections installed on the shell's own descriptors, and what it takes to
/// undo them.
struct Applied {
    saved: Vec<SavedFd>,
    /// Here-document writers, reaped after the descriptors are put back so a
    /// body larger than the pipe cannot deadlock the reap.
    writers: Vec<u32>,
}

impl Applied {
    /// Apply every redirection, saving what each displaced. `None` means one
    /// could not be applied, and everything already applied has been undone.
    fn install(redirects: &[Redirect]) -> Option<Self> {
        let mut applied = Applied {
            saved: Vec::new(),
            writers: Vec::new(),
        };
        for redirect in redirects {
            // Backed up *before* the open, because the kernel hands out the
            // lowest free descriptor: `3>out` with only 0/1/2 open lands on
            // fd 3, and a later backup would capture the file itself.
            let backup = match fs::dup(redirect.fd) {
                Ok(fd) => fd.into_raw(),
                // A descriptor that is not open has nothing to restore.
                Err(_) => -1,
            };
            applied.saved.push(SavedFd {
                fd: redirect.fd,
                backup,
            });
            let Some(source) = open_target(redirect, &mut applied.writers) else {
                applied.undo();
                return None;
            };
            let ok = match source {
                // The open landed on the descriptor already; the close after
                // a no-op `dup2` would drop the only copy.
                Source::Opened(fd) if fd == redirect.fd => true,
                Source::Opened(fd) => {
                    let ok = fs::dup2(fd, redirect.fd).is_ok();
                    let _ = fs::close_fd_raw(fd);
                    ok
                }
                Source::Dup(src) => fs::dup2(src, redirect.fd).is_ok(),
                Source::Close => {
                    let _ = fs::close_fd_raw(redirect.fd);
                    true
                }
            };
            if !ok {
                shell_error(b"sh: cannot redirect\n");
                applied.undo();
                return None;
            }
        }
        Some(applied)
    }

    fn undo(self) {
        for slot in self.saved.iter().rev() {
            if slot.backup < 0 {
                // Nothing was open here before, so leave nothing open now.
                let _ = fs::close_fd_raw(slot.fd);
                continue;
            }
            let _ = fs::dup2(slot.backup, slot.fd);
            let _ = fs::close_fd_raw(slot.backup);
        }
        for pid in self.writers {
            let _ = process::wait_exit_code(pid);
        }
    }
}

enum Source {
    Opened(i32),
    Dup(i32),
    Close,
}

fn open_target(redirect: &Redirect, writers: &mut Vec<u32>) -> Option<Source> {
    match &redirect.target {
        RedirTarget::Dup(word) => {
            let operand = match expand::single(word) {
                Ok(operand) => operand,
                Err(e) => {
                    e.report();
                    return None;
                }
            };
            if operand == b"-" {
                return Some(Source::Close);
            }
            if operand.is_empty() || !operand.iter().all(|b| b.is_ascii_digit()) {
                shell_error_named(&operand, b"bad file descriptor");
                return None;
            }
            let fd = operand.iter().fold(0i32, |acc, b| {
                acc.saturating_mul(10).saturating_add((b - b'0') as i32)
            });
            Some(Source::Dup(fd))
        }
        RedirTarget::Here(body) => {
            let text = match expand::here_body(body) {
                Ok(text) => text,
                Err(e) => {
                    e.report();
                    return None;
                }
            };
            let (fd, pid) = open_heredoc(&text)?;
            writers.push(pid);
            Some(Source::Opened(fd))
        }
        RedirTarget::Path(word) => {
            let operand = match expand::fields(word) {
                Ok(fields) if fields.len() == 1 => fields.into_iter().next().expect("one field"),
                Ok(_) => {
                    shell_error(b"sh: ambiguous redirect\n");
                    return None;
                }
                Err(e) => {
                    e.report();
                    return None;
                }
            };
            let mut path_buf = buffers::path_scratch();
            if normalize_path(&operand, &mut path_buf) != 0 {
                shell_error_named(&operand, b"path too long");
                return None;
            }
            let flags = match redirect.kind {
                RedirKind::Input => O_RDONLY,
                RedirKind::InputOutput => O_RDWR | O_CREAT,
                // O_TRUNC rather than unlink-and-recreate: unlinking destroys
                // the file even when the open then fails, and breaks hard
                // links and device nodes.
                RedirKind::OutputTruncate => O_WRONLY | O_CREAT | O_TRUNC,
                RedirKind::OutputAppend => O_WRONLY | O_CREAT | O_APPEND,
            };
            match fs::open_path(path_buf.as_ptr() as *const c_char, flags) {
                Ok(fd) => Some(Source::Opened(fd.into_raw())),
                Err(_) => {
                    shell_error_named(&operand, b"cannot open");
                    None
                }
            }
        }
    }
}

/// Stage a here-document body on a pipe. The writer must be a separate
/// process, or a body past the pipe's capacity blocks the shell on its own
/// read end before anything has read it.
fn open_heredoc(body: &[u8]) -> Option<(i32, u32)> {
    let Ok((read_end, write_end)) = fs::pipe() else {
        shell_error(b"sh: cannot create pipe\n");
        return None;
    };
    let (read_fd, write_fd) = (read_end.into_raw(), write_end.into_raw());
    let pid = process::fork();
    if pid < 0 {
        let _ = fs::close_fd_raw(read_fd);
        let _ = fs::close_fd_raw(write_fd);
        shell_error(b"sh: cannot fork\n");
        return None;
    }
    if pid == 0 {
        let _ = fs::close_fd_raw(read_fd);
        let mut written = 0usize;
        while written < body.len() {
            match fs::write_slice(write_fd, &body[written..]) {
                Ok(0) => break,
                Ok(n) => written += n,
                Err(e) if e == crate::syscall::SyscallError::EINTR => continue,
                // The reader closed early — a `head` on a long body.
                Err(_) => break,
            }
        }
        let _ = fs::close_fd_raw(write_fd);
        sys_core::exit_with_code(0);
    }
    let _ = fs::close_fd_raw(write_fd);
    Some((read_fd, pid as u32))
}

/// Apply redirections permanently, in a process that is about to become the
/// command. Never returns on failure: the child is the command, and a command
/// whose redirection failed did not run.
fn install_redirects_in_child(redirects: &[Redirect]) {
    let mut writers = Vec::new();
    for redirect in redirects {
        let Some(source) = open_target(redirect, &mut writers) else {
            sys_core::exit_with_code(1);
        };
        let ok = match source {
            // As in `Applied::install`: the open may have landed on the
            // descriptor already, and the close would drop the only copy.
            Source::Opened(fd) if fd == redirect.fd => true,
            Source::Opened(fd) => {
                let ok = fs::dup2(fd, redirect.fd).is_ok();
                let _ = fs::close_fd_raw(fd);
                ok
            }
            Source::Dup(src) => fs::dup2(src, redirect.fd).is_ok(),
            // Closing an unopened descriptor is not an error: `prog 3>&-`
            // must not fail where the same redirection on a builtin succeeds.
            Source::Close => {
                let _ = fs::close_fd_raw(redirect.fd);
                true
            }
        };
        if !ok {
            shell_error(b"sh: cannot redirect\n");
            sys_core::exit_with_code(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Program resolution
// ---------------------------------------------------------------------------

fn resolve_via_path(name: &[u8], tmp: &mut [u8]) -> bool {
    let Some(path_value) = env::get(b"PATH") else {
        return false;
    };
    if path_value.is_empty() {
        return false;
    }

    for dir in path_value.split(|&b| b == b':') {
        let dir: &[u8] = if dir.is_empty() { b"." } else { dir };
        let mut candidate = Vec::with_capacity(dir.len() + name.len() + 2);
        candidate.extend_from_slice(dir);
        if candidate.last() != Some(&b'/') {
            candidate.push(b'/');
        }
        candidate.extend_from_slice(name);
        if normalize_path(&candidate, tmp) != 0 {
            continue;
        }
        let mut stat = UserFsStat::default();
        // A directory on `PATH` is not a command.
        if fs::stat_path(tmp.as_ptr() as *const c_char, &mut stat).is_ok() && stat.is_file() {
            return true;
        }
    }
    false
}

/// Resolve a command name to a path the loader will accept. A name holding a
/// `/` is a path; anything else is a registry program or a `PATH` lookup.
fn resolve_exec_path(name: &[u8], tmp: &mut [u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.contains(&b'/') {
        if normalize_path(name, tmp) != 0 {
            return false;
        }
        let mut stat = UserFsStat::default();
        if fs::stat_path(tmp.as_ptr() as *const c_char, &mut stat).is_err() {
            return false;
        }
        return stat.is_file();
    }

    // A pipeline stage execs a registry program like any other.
    if let Ok(name_str) = core::str::from_utf8(name)
        && let Some(spec) = program_registry::resolve_program(name_str)
    {
        let path_bytes = spec.path.as_bytes();
        let path_len = path_bytes.len().min(tmp.len() - 1);
        tmp[..path_len].copy_from_slice(&path_bytes[..path_len]);
        tmp[path_len] = 0;
        return true;
    }

    resolve_via_path(name, tmp)
}

/// Whether `name` names something this shell can run, which is what
/// `command -v` and `type` answer.
pub fn resolve_command(name: &[u8]) -> Option<Vec<u8>> {
    if funcs::lookup(name).is_some() {
        return Some(name.to_vec());
    }
    resolve_command_ignoring_functions(name)
}

/// As [`resolve_command`], but blind to the function table — what
/// `command NAME` needs, or `ls() { command ls -F "$@"; }` resolves to itself
/// and recurses until the stack runs out.
pub fn resolve_command_ignoring_functions(name: &[u8]) -> Option<Vec<u8>> {
    if name.is_empty() {
        return None;
    }
    if builtins::find_builtin(name).is_some() {
        return Some(name.to_vec());
    }
    if let Some(spec) = registry_spec(name) {
        return Some(spec.path.as_bytes().to_vec());
    }
    let mut tmp = buffers::path_scratch();
    if !resolve_exec_path(name, &mut tmp) {
        return None;
    }
    let len = tmp.iter().position(|&b| b == 0).unwrap_or(tmp.len());
    Some(tmp[..len].to_vec())
}

fn registry_spec(name: &[u8]) -> Option<&'static program_registry::ProgramSpec> {
    if name.is_empty() {
        return None;
    }
    if name.contains(&b'/') {
        let mut tmp = buffers::path_scratch();
        if normalize_path(name, &mut tmp) != 0 {
            return None;
        }
        let len = tmp.iter().position(|&b| b == 0).unwrap_or(tmp.len());
        let path = core::str::from_utf8(&tmp[..len]).ok()?;
        return program_registry::resolve_program_path(path);
    }
    let name = core::str::from_utf8(name).ok()?;
    program_registry::resolve_program(name)
}

/// Build a NUL-terminated `argv` for the exec/spawn ABI boundary.
///
/// The owned strings come back with the pointer array: the kernel reads through
/// those pointers during the syscall, so the bytes must outlive it.
fn build_c_argv(argv: &[Vec<u8>]) -> (Vec<Vec<u8>>, Vec<*const u8>) {
    let owned: Vec<Vec<u8>> = argv
        .iter()
        .map(|arg| {
            let mut s = Vec::with_capacity(arg.len() + 1);
            s.extend_from_slice(arg);
            s.push(0);
            s
        })
        .collect();
    let mut ptrs: Vec<*const u8> = owned.iter().map(|s| s.as_ptr()).collect();
    ptrs.push(ptr::null());
    (owned, ptrs)
}

/// Build a NUL-terminated `envp` of the exported variables.
fn build_c_envp() -> (Vec<Vec<u8>>, Vec<*const u8>) {
    let mut owned: Vec<Vec<u8>> = Vec::new();
    env::for_each_exported(|key, value| {
        let mut s = Vec::with_capacity(key.len() + value.len() + 2);
        s.extend_from_slice(key);
        s.push(b'=');
        s.extend_from_slice(value);
        s.push(0);
        owned.push(s);
    });
    let mut ptrs: Vec<*const u8> = owned.iter().map(|s| s.as_ptr()).collect();
    ptrs.push(ptr::null());
    (owned, ptrs)
}

// ---------------------------------------------------------------------------
// Waiting and job bookkeeping
// ---------------------------------------------------------------------------

/// `WUNTRACED` is what makes Ctrl-Z observable: without it a suspended child
/// never reports and the shell waits forever. A blocking wait, not a poll:
/// a script's every command would otherwise pay half a poll interval.
fn wait_foreground(pid: u32) -> process::WaitStatus {
    loop {
        let mut status = 0i32;
        let rc = process::waitpid_raw(pid as i32, &mut status, WUNTRACED);
        if rc > 0 {
            return process::wait_status(status);
        }
        if rc != slopos_abi::Errno::EINTR.raw() as i64 {
            // No such child any more: nothing will ever report.
            return process::WaitStatus::Exited(127);
        }
    }
}

/// A stop is not a termination: it becomes a job the user can `fg`.
fn finish_foreground(pid: u32, pgid: u32, command: &[u8]) -> i32 {
    let report = wait_foreground(pid);
    leave_foreground();
    if let process::WaitStatus::Stopped(signum) = report {
        match jobs::add_stopped(pid, pgid, command) {
            Some(job_id) => jobs::report_stopped(job_id),
            None => {
                shell_error(b"sh: job table full\n");
            }
        }
        return 128 + signum as i32;
    }
    report.exit_code().unwrap_or(1)
}

/// The job already has a table entry, so a second stop updates it rather than
/// creating another.
pub fn wait_resumed_job(pid: u32) -> i32 {
    let report = wait_foreground(pid);
    leave_foreground();
    if let process::WaitStatus::Stopped(signum) = report {
        jobs::set_state_by_pid(pid, jobs::JobState::Stopped);
        if let Some(job_id) = jobs::find_job_id_by_pid(pid) {
            jobs::report_stopped(job_id);
        }
        return 128 + signum as i32;
    }
    report.exit_code().unwrap_or(1)
}

fn print_background_job_started(job_id: u16, pid: u32) {
    super::set_last_bg_pid(pid);
    // Job bookkeeping is a message to the user, not program output.
    if !super::is_interactive() {
        return;
    }
    shell_write(b"[");
    jobs::write_u64(job_id as u64);
    shell_write(b"] ");
    jobs::write_u64(pid as u64);
    shell_write(b"\n");
}

fn register_background(pgid: u32, command: &[u8]) {
    match jobs::add(pgid, pgid, command) {
        Some(job_id) => print_background_job_started(job_id, pgid),
        None => {
            shell_error(b"sh: job table full\n");
        }
    }
}

// ---------------------------------------------------------------------------
// Forked execution
// ---------------------------------------------------------------------------

/// Prepare a freshly forked child: its process group, the terminal, and the
/// signal dispositions the shell holds but a job must not inherit.
fn child_setup(pgid: u32, foreground: bool) {
    let job_control = super::is_interactive();
    if job_control {
        let _ = process::setpgid(0, pgid);
    }

    // The child claims the terminal for its own pgrp, racing the parent's
    // `enter_foreground`; both set the same value.  Must precede the sigdefault
    // reset below: SIGTTOU is still inherited-ignored there, so this tcsetpgrp
    // is not denied to a not-yet-foreground child.
    if foreground && job_control {
        let fg = if pgid == 0 {
            process::getpid() as u32
        } else {
            pgid
        };
        let _ = fs::tcsetpgrp(0, fg);
    }

    // The reset must be explicit: execve preserves ignored dispositions, and an
    // in-child builtin never execs at all.
    let _ = process::sigdefault(JOB_CONTROL_DEFAULT_SIGNALS);
    super::interrupt::mark_forked_child();
}

/// Run one command in this process and never return: the tail of every forked
/// stage. An external program `execve`s; everything else runs in-process and
/// exits with its status.
fn become_command(cmd: &Command) -> ! {
    if let CommandKind::Simple { assigns, words } = &cmd.kind {
        let argv = match expand::word_list(words) {
            Ok(argv) => argv,
            Err(e) => sys_core::exit_with_code(e.report()),
        };
        let pairs = match expand_assignments(assigns) {
            Ok(pairs) => pairs,
            Err(e) => sys_core::exit_with_code(e.report()),
        };
        // The stage runs one command and exits, so the prefix needs no undoing.
        apply_assignments(&pairs, false);
        install_redirects_in_child(&cmd.redirects);

        if argv.is_empty() {
            sys_core::exit_with_code(0);
        }
        if funcs::xtrace() {
            trace(&argv);
        }
        if let Some(body) = funcs::lookup(&argv[0]) {
            let _args = super::args::shadow_args(argv[1..].to_vec());
            let outcome = run_command(&body);
            sys_core::exit_with_code(outcome.status);
        }
        if let Some(entry) = builtins::find_builtin(&argv[0]) {
            let slices: Vec<&[u8]> = argv.iter().map(|a| a.as_slice()).collect();
            let status = (entry.func)(argv.len() as i32, &slices);
            sys_core::exit_with_code(status);
        }

        let mut path_buf = buffers::path_scratch();
        if !resolve_exec_path(&argv[0], &mut path_buf) {
            shell_error_named(&argv[0], b"not found");
            sys_core::exit_with_code(STATUS_NOT_FOUND);
        }
        let (argv_owned, argv_ptrs) = build_c_argv(&argv);
        let (envp_owned, envp_ptrs) = build_c_envp();
        let rc = process::execve(path_buf.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
        drop(argv_owned);
        drop(envp_owned);
        if rc < 0 {
            shell_error_named(&argv[0], b"cannot execute");
        }
        sys_core::exit_with_code(STATUS_CANNOT_EXECUTE);
    }

    install_redirects_in_child(&cmd.redirects);
    let outcome = match &cmd.kind {
        CommandKind::Subshell(list) => run_list(list),
        other => run_compound(other),
    };
    sys_core::exit_with_code(outcome.status);
}

/// Run one command in a fork of this shell — a subshell, a pipeline stage that
/// is a compound command, or an external program.
fn run_forked_command(cmd: &Command, background: bool) -> Outcome {
    let pid = process::fork();
    if pid < 0 {
        shell_error(b"sh: cannot fork\n");
        return Outcome::normal(1);
    }
    if pid == 0 {
        child_setup(0, !background);
        become_command(cmd);
    }
    let child = pid as u32;
    if super::is_interactive() {
        let _ = process::setpgid(child, child);
    }
    let text = describe(cmd);
    if background {
        register_background(child, &text);
        return Outcome::normal(0);
    }
    enter_foreground(child);
    Outcome::normal(finish_foreground(child, child, &text))
}

fn fork_subshell(list: &List, redirects: &[Redirect], background: bool) -> Outcome {
    let cmd = Command {
        kind: CommandKind::Subshell(list.clone()),
        redirects: redirects.to_vec(),
    };
    run_forked_command(&cmd, background)
}

/// A simple command that is an external program: one fork, and the child
/// becomes the program.
fn spawn_external(
    cmd: &Command,
    pairs: &[(Vec<u8>, Vec<u8>)],
    argv: &[Vec<u8>],
    background: bool,
) -> Outcome {
    let pid = process::fork();
    if pid < 0 {
        shell_error(b"sh: cannot fork\n");
        return Outcome::normal(1);
    }
    if pid == 0 {
        child_setup(0, !background);
        apply_assignments(pairs, false);
        install_redirects_in_child(&cmd.redirects);
        let mut path_buf = buffers::path_scratch();
        if !resolve_exec_path(&argv[0], &mut path_buf) {
            shell_error_named(&argv[0], b"not found");
            sys_core::exit_with_code(STATUS_NOT_FOUND);
        }
        let (argv_owned, argv_ptrs) = build_c_argv(argv);
        let (envp_owned, envp_ptrs) = build_c_envp();
        let rc = process::execve(path_buf.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
        drop(argv_owned);
        drop(envp_owned);
        if rc < 0 {
            shell_error_named(&argv[0], b"cannot execute");
        }
        sys_core::exit_with_code(STATUS_CANNOT_EXECUTE);
    }
    let child = pid as u32;
    if super::is_interactive() {
        let _ = process::setpgid(child, child);
    }
    if background {
        register_background(child, &argv[0]);
        return Outcome::normal(0);
    }
    enter_foreground(child);
    Outcome::normal(finish_foreground(child, child, &argv[0]))
}

/// A registry program: one the kernel gives special authority or placement,
/// spawned rather than forked so the grant is applied to a fresh task.
fn spawn_registry_program(
    spec: &'static program_registry::ProgramSpec,
    argv: &[Vec<u8>],
    background: bool,
) -> i32 {
    // A foreground job gets its own pgrp and the kernel-side handoff, so it is
    // the terminal's foreground group before its first fd-0 read.  Only a shell
    // that owns the terminal may ask: the kernel resolves the handoff from the
    // inherited controlling tty, so a script shell would hand the *user's*
    // foreground group to the child with no way to restore it.
    let spawn_flags = if background || !super::is_interactive() {
        spec.flags
    } else {
        spec.flags | slopos_abi::task::TASK_FLAG_NEW_PGRP | slopos_abi::task::TASK_FLAG_FOREGROUND
    };

    let (argv_owned, argv_ptrs) = build_c_argv(argv);
    let (envp_owned, envp_ptrs) = build_c_envp();

    // Cloning the shell's own 0/1/2 keeps `isatty(1)` true for the child, so
    // its stdio stays line-buffered and output appears as it is produced — and
    // it is what carries an applied redirection into the child.
    let actions = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    let tid = process::spawn_path_with_env(
        spec.path.as_bytes(),
        &argv_ptrs[..argv.len()],
        &envp_ptrs[..envp_owned.len()],
        spec.priority,
        spawn_flags,
        &actions,
        0,
    );
    drop(argv_owned);
    drop(envp_owned);

    if tid <= 0 {
        shell_error(b"sh: spawn failed\n");
        return 1;
    }
    let pid = tid as u32;
    if background {
        register_background(pid, &argv[0]);
        return 0;
    }
    // Idempotent: the child already has this pgid via TASK_FLAG_NEW_PGRP.
    if super::is_interactive() {
        let _ = process::setpgid(pid, pid);
    }
    enter_foreground(pid);
    finish_foreground(pid, pid, &argv[0])
}

/// A pipeline of two or more stages: one pipe between each pair, one fork per
/// stage, and the pipeline's status is the last stage's.
fn run_staged_pipeline(pipeline: &Pipeline, background: bool) -> Outcome {
    let stages = pipeline.cmds.len();
    let mut pipes: Vec<[i32; 2]> = Vec::with_capacity(stages.saturating_sub(1));
    for _ in 0..stages - 1 {
        match fs::pipe() {
            Ok((r, w)) => pipes.push([r.into_raw(), w.into_raw()]),
            Err(_) => {
                shell_error(b"sh: cannot create pipe\n");
                for pair in &pipes {
                    let _ = fs::close_fd_raw(pair[0]);
                    let _ = fs::close_fd_raw(pair[1]);
                }
                return Outcome::normal(1);
            }
        }
    }

    let mut pids: Vec<u32> = Vec::with_capacity(stages);
    let mut pgid = 0u32;
    for (index, cmd) in pipeline.cmds.iter().enumerate() {
        let stdin_fd = if index > 0 { pipes[index - 1][0] } else { -1 };
        let stdout_fd = if index + 1 < stages {
            pipes[index][1]
        } else {
            -1
        };

        let pid = process::fork();
        if pid < 0 {
            shell_error(b"sh: cannot fork\n");
            for pair in &pipes {
                let _ = fs::close_fd_raw(pair[0]);
                let _ = fs::close_fd_raw(pair[1]);
            }
            // The pipes are closed, so every forked stage exits; reaping here
            // is what keeps a failing `fork` from leaving a zombie behind.
            for pid in &pids {
                let _ = process::wait_exit_code(*pid);
            }
            return Outcome::normal(1);
        }
        if pid == 0 {
            child_setup(pgid, !background);
            if stdin_fd >= 0 && fs::dup2(stdin_fd, 0).is_err() {
                shell_error(b"sh: cannot set up stdin\n");
                sys_core::exit_with_code(1);
            }
            if stdout_fd >= 0 && fs::dup2(stdout_fd, 1).is_err() {
                shell_error(b"sh: cannot set up stdout\n");
                sys_core::exit_with_code(1);
            }
            for pair in &pipes {
                let _ = fs::close_fd_raw(pair[0]);
                let _ = fs::close_fd_raw(pair[1]);
            }
            become_command(cmd);
        }

        let child = pid as u32;
        if pgid == 0 {
            pgid = child;
        }
        if super::is_interactive() {
            let _ = process::setpgid(child, pgid);
        }
        pids.push(child);
    }

    for pair in &pipes {
        let _ = fs::close_fd_raw(pair[0]);
        let _ = fs::close_fd_raw(pair[1]);
    }

    let text = describe_pipeline(pipeline);
    if background {
        register_background(pgid, &text);
        return Outcome::normal(0);
    }

    enter_foreground(pgid);

    // Every stage is reaped, but the pipeline's status is the last stage's.
    // A terminal stop hits the whole group at once, so the first stage to
    // report one suspends the pipeline as a single job.
    let mut status = 0;
    for (index, pid) in pids.iter().enumerate() {
        let report = wait_foreground(*pid);
        if let process::WaitStatus::Stopped(signum) = report {
            leave_foreground();
            match jobs::add_stopped(pgid, pgid, &text) {
                Some(job_id) => jobs::report_stopped(job_id),
                None => {
                    shell_error(b"sh: job table full\n");
                }
            }
            return Outcome::normal(128 + signum as i32);
        }
        if index + 1 == pids.len() {
            status = report.exit_code().unwrap_or(1);
        }
    }
    leave_foreground();
    Outcome::normal(status)
}

/// A short label for the job table, of unexpanded words: expanding for it
/// would run a command substitution a second time.
fn describe(cmd: &Command) -> Vec<u8> {
    const MAX: usize = 128;
    let mut out = Vec::new();
    match &cmd.kind {
        CommandKind::Simple { words, .. } => {
            for word in words {
                if !out.is_empty() {
                    out.push(b' ');
                }
                out.extend_from_slice(&word.text);
                if out.len() >= MAX {
                    break;
                }
            }
        }
        CommandKind::Subshell(_) => out.extend_from_slice(b"( ... )"),
        CommandKind::Group(_) => out.extend_from_slice(b"{ ... }"),
        CommandKind::If { .. } => out.extend_from_slice(b"if ..."),
        CommandKind::Loop { until, .. } => {
            out.extend_from_slice(if *until { b"until ..." } else { b"while ..." })
        }
        CommandKind::For { .. } => out.extend_from_slice(b"for ..."),
        CommandKind::Case { .. } => out.extend_from_slice(b"case ..."),
        CommandKind::Function { name, .. } => {
            out.extend_from_slice(name);
            out.extend_from_slice(b"()");
        }
    }
    out.truncate(MAX);
    out
}

fn describe_pipeline(pipeline: &Pipeline) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, cmd) in pipeline.cmds.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b" | ");
        }
        out.extend_from_slice(&describe(cmd));
    }
    out.truncate(128);
    out
}
