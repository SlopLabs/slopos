//! Tab completion's candidates: the command tables, every directory on
//! `PATH`, the filesystem, and the rules `complete` declares. A command's
//! rules are read on the first completion of one of its arguments, from a
//! file named after it in a `SHELL_COMPLETION_PATH` directory, so a program
//! installed under `/usr/local` can bring its own.

use std::sync::{Mutex, MutexGuard};

use slopos_abi::fs::UserFsStat;
use slopos_shell_core::ast::Word;
use slopos_shell_core::complete::spec::{self, Action, Query, Request, Rule, Specs};
use slopos_shell_core::complete::{self, Candidate, Listing, Position, Suffix};
use slopos_shell_core::lexer::{self, Tok};

use crate::program_registry;
use crate::syscall::fs;

use super::builtins::BUILTINS;
use super::{env, expand, funcs, script};

const DEFAULT_COMPLETION_PATH: &[u8] =
    b"/usr/local/share/shell/completions:/usr/share/shell/completions";

/// Compiled in: only the shell knows its builtins' arguments.
const BUILTIN_RULES: &[&[&[u8]]] = &[
    &[b"-c", b"cd", b"-f", b"-A", b"directory"],
    &[b"-c", b"help", b"-f", b"-A", b"builtin"],
    &[
        b"-c", b"command", b"-c", b"exec", b"-c", b"time", b"-f", b"-A", b"command",
    ],
    &[b"-c", b"type", b"-P", b"**", b"-f", b"-A", b"command"],
    &[
        b"-c",
        b"command",
        b"-s",
        b"v",
        b"-d",
        b"Print what the name resolves to",
    ],
    &[
        b"-c",
        b"command",
        b"-s",
        b"V",
        b"-d",
        b"Describe what the name resolves to",
    ],
    &[
        b"-c",
        b"command",
        b"-s",
        b"p",
        b"-d",
        b"Search the default PATH",
    ],
    &[b"-c", b"complete", b"-f"],
    &[
        b"-c",
        b"complete",
        b"-s",
        b"c",
        b"-l",
        b"command",
        b"-x",
        b"-A",
        b"command",
        b"-d",
        b"The command the rule is for",
    ],
    &[
        b"-c",
        b"complete",
        b"-s",
        b"P",
        b"-l",
        b"after",
        b"-x",
        b"-d",
        b"Positional words that must come first",
    ],
    &[
        b"-c",
        b"complete",
        b"-s",
        b"s",
        b"-l",
        b"short-option",
        b"-x",
        b"-d",
        b"A one-letter option",
    ],
    &[
        b"-c",
        b"complete",
        b"-s",
        b"l",
        b"-l",
        b"long-option",
        b"-x",
        b"-d",
        b"A long option",
    ],
    &[
        b"-c",
        b"complete",
        b"-s",
        b"r",
        b"-l",
        b"require-parameter",
        b"-d",
        b"The option takes a value",
    ],
    &[
        b"-c",
        b"complete",
        b"-s",
        b"f",
        b"-l",
        b"no-files",
        b"-d",
        b"Offer no file names",
    ],
    &[
        b"-c",
        b"complete",
        b"-s",
        b"x",
        b"-l",
        b"exclusive",
        b"-d",
        b"As -r -f",
    ],
    &[
        b"-c",
        b"complete",
        b"-s",
        b"a",
        b"-l",
        b"arguments",
        b"-x",
        b"-d",
        b"Candidate words, expanded on Tab",
    ],
    &[
        b"-c",
        b"complete",
        b"-s",
        b"A",
        b"-l",
        b"action",
        b"-x",
        b"-a",
        b"file directory command builtin",
        b"-d",
        b"A kind of word",
    ],
    &[
        b"-c",
        b"complete",
        b"-s",
        b"d",
        b"-l",
        b"description",
        b"-x",
        b"-d",
        b"Shown beside the candidates",
    ],
    &[
        b"-c",
        b"complete",
        b"-s",
        b"e",
        b"-l",
        b"erase",
        b"-d",
        b"Forget the command's rules",
    ],
];

struct Registry {
    specs: Specs,
    /// Read once even when they declared nothing.
    sourced: Vec<Vec<u8>>,
    seeded: bool,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    specs: Specs::new(),
    sourced: Vec::new(),
    seeded: false,
});

fn registry() -> MutexGuard<'static, Registry> {
    let mut registry = REGISTRY.lock().unwrap();
    if !registry.seeded {
        registry.seeded = true;
        for args in BUILTIN_RULES {
            match spec::parse(args) {
                Ok(Request::Add(rules)) => registry.specs.add(rules),
                _ => unreachable!("a builtin completion rule that does not parse"),
            }
        }
    }
    registry
}

pub fn add_rules(rules: Vec<Rule>) {
    registry().specs.add(rules);
}

pub fn erase_rules(commands: &[Vec<u8>]) {
    let mut registry = registry();
    for command in commands {
        registry.specs.erase(command);
    }
}

/// Empty `commands`: every rule.
pub fn render_rules(commands: &[Vec<u8>]) -> Vec<u8> {
    let registry = registry();
    let mut out = Vec::new();
    let wanted = |rule: &&Rule| commands.is_empty() || commands.contains(&rule.command);
    for rule in registry.specs.rules().iter().filter(wanted) {
        out.extend_from_slice(&spec::render(rule));
        out.push(b'\n');
    }
    out
}

pub struct Outcome {
    pub insert: Vec<u8>,
    /// Set when more than one candidate is left.
    pub listing: Option<Listing>,
}

/// `line` is the text before the cursor.
pub fn complete(line: &[u8], width: usize, max_lines: usize) -> Outcome {
    let ctx = complete::analyze(line);
    // A rule's `$(...)` runs commands; completing is not running one.
    let status = super::last_exit_code();
    let mut found = Vec::new();
    match ctx.position {
        Position::Nothing => {}
        Position::Command if ctx.prefix.contains(&b'/') => {
            files(&ctx.prefix, b"", Kind::Runnable, &mut found);
        }
        Position::Command => commands(&ctx.prefix, b"", &mut found),
        Position::Path => files(&ctx.prefix, b"", Kind::Any, &mut found),
        Position::Argument => arguments(&ctx.words, &ctx.prefix, &mut found),
    }
    super::set_last_exit_code(status);
    expand::take_substituted();

    let done = complete::resolve(&ctx.prefix, ctx.quote, found);
    let listing =
        (done.candidates.len() > 1).then(|| complete::listing(&done.candidates, width, max_lines));
    Outcome {
        insert: done.insert,
        listing,
    }
}

fn arguments(words: &[Vec<u8>], prefix: &[u8], out: &mut Vec<Candidate>) {
    let command = words[0].rsplit(|&b| b == b'/').next().unwrap_or_default();
    let query = query(command, &words[1..], prefix);
    let lead = query.lead.as_slice();
    let value = &prefix[lead.len()..];

    out.extend(query.options);
    for words in &query.words {
        for field in expand_words(&words.text) {
            if field.starts_with(value) {
                let mut word = lead.to_vec();
                word.extend_from_slice(&field);
                out.push(Candidate::new(word).described(&words.description));
            }
        }
    }
    for action in &query.actions {
        match action {
            Action::File => files(value, lead, Kind::Any, out),
            Action::Directory => files(value, lead, Kind::Directories, out),
            Action::Command => commands(value, lead, out),
            Action::Builtin => builtins(value, lead, out),
        }
    }
    if query.files {
        files(value, lead, Kind::Any, out);
    }
}

fn query(command: &[u8], args: &[Vec<u8>], current: &[u8]) -> Query {
    let loaded = {
        let registry = registry();
        registry.specs.covers(command) || registry.sourced.iter().any(|c| c == command)
    };
    // Sourced unlocked: the file's own `complete` lines take the lock.
    if !loaded && let Some(path) = rules_file(command) {
        registry().sourced.push(command.to_vec());
        script::source_file(&path);
    }
    registry().specs.query(command, args, current)
}

/// A miss is not remembered, so a file installed later is found.
fn rules_file(command: &[u8]) -> Option<Vec<u8>> {
    if command.is_empty() || command == b"." || command == b".." {
        return None;
    }
    let search =
        env::get(b"SHELL_COMPLETION_PATH").unwrap_or_else(|| DEFAULT_COMPLETION_PATH.to_vec());
    search
        .split(|&b| b == b':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| join(dir, command))
        .find(|path| stat(path).is_some_and(|s| s.is_file()))
}

fn expand_words(text: &[u8]) -> Vec<Vec<u8>> {
    let Ok(tokens) = lexer::lex(text) else {
        return Vec::new();
    };
    let mut words = Vec::with_capacity(tokens.len());
    for token in tokens {
        match token {
            Tok::Word(raw) => words.push(Word::raw(raw)),
            Tok::Newline => {}
            _ => return Vec::new(),
        }
    }
    expand::word_list(&words).unwrap_or_default()
}

fn push_named(
    name: &[u8],
    description: &[u8],
    prefix: &[u8],
    lead: &[u8],
    out: &mut Vec<Candidate>,
) {
    if name.starts_with(prefix) {
        let mut word = lead.to_vec();
        word.extend_from_slice(name);
        out.push(Candidate::new(word).described(description));
    }
}

fn builtins(prefix: &[u8], lead: &[u8], out: &mut Vec<Candidate>) {
    for entry in BUILTINS {
        push_named(
            entry.name.as_bytes(),
            entry.desc.as_bytes(),
            prefix,
            lead,
            out,
        );
    }
}

fn commands(prefix: &[u8], lead: &[u8], out: &mut Vec<Candidate>) {
    builtins(prefix, lead, out);
    for name in funcs::names() {
        push_named(&name, b"", prefix, lead, out);
    }
    for spec in program_registry::user_programs() {
        push_named(
            spec.name.as_bytes(),
            spec.desc.as_bytes(),
            prefix,
            lead,
            out,
        );
    }
    let path = env::get(b"PATH").unwrap_or_default();
    for dir in path.split(|&b| b == b':') {
        let dir = absolute(if dir.is_empty() { b"." } else { dir });
        for name in names_in(&dir, prefix) {
            let Some(stat) = stat(&join(&dir, &name)) else {
                continue;
            };
            if stat.is_file() && stat.st_mode & 0o111 != 0 {
                push_named(&name, tool_description(&name), prefix, lead, out);
            }
        }
    }
}

fn tool_description(name: &[u8]) -> &'static [u8] {
    crate::apps::coreutils::tools()
        .find(|tool| tool.name.as_bytes() == name)
        .map_or(b"", |tool| tool.desc.as_bytes())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Any,
    Directories,
    /// Executables, and the directories on the way to one.
    Runnable,
}

fn files(prefix: &[u8], lead: &[u8], kind: Kind, out: &mut Vec<Candidate>) {
    let split = prefix.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    let (typed_dir, base) = prefix.split_at(split);
    let dir = absolute(if typed_dir.is_empty() {
        b"."
    } else {
        typed_dir
    });
    for name in names_in(&dir, base) {
        let Some(stat) = stat(&join(&dir, &name)) else {
            continue;
        };
        let is_dir = stat.is_directory();
        let wanted = match kind {
            Kind::Any => true,
            Kind::Directories => is_dir,
            Kind::Runnable => is_dir || stat.st_mode & 0o111 != 0,
        };
        if !wanted {
            continue;
        }
        let mut word = lead.to_vec();
        word.extend_from_slice(typed_dir);
        word.extend_from_slice(&name);
        out.push(Candidate {
            word,
            shown_from: lead.len() + typed_dir.len(),
            description: Vec::new(),
            suffix: if is_dir { Suffix::Slash } else { Suffix::Space },
        });
    }
}

fn names_in(dir: &[u8], start: &[u8]) -> Vec<Vec<u8>> {
    let Ok(dir) = core::str::from_utf8(dir) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .map(String::into_bytes)
        .filter(|name| name.starts_with(start) && name != b"." && name != b"..")
        .collect()
}

/// Against the shell's working directory, as every relative path it resolves.
fn absolute(path: &[u8]) -> Vec<u8> {
    if path.starts_with(b"/") {
        return path.to_vec();
    }
    let mut cwd = super::cwd_bytes();
    cwd.pop();
    join(&cwd, path)
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(dir.len() + name.len() + 1);
    path.extend_from_slice(dir);
    if path.last() != Some(&b'/') {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

fn stat(path: &[u8]) -> Option<UserFsStat> {
    let mut path_z = Vec::with_capacity(path.len() + 1);
    path_z.extend_from_slice(path);
    path_z.push(0);
    let mut stat = UserFsStat::default();
    fs::stat_path(path_z.as_ptr() as *const core::ffi::c_char, &mut stat).ok()?;
    Some(stat)
}
