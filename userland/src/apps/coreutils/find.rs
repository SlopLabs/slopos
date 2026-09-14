//! `find`: walk a tree and evaluate an expression against every entry.
//!
//! The expression is parsed into a tree once and evaluated per entry with
//! short-circuiting, which is what makes `-o`, `!` and `( )` mean what POSIX
//! says rather than what a flag-at-a-time scanner would make of them.

use std::fs;
use std::process::Command;
use std::time::SystemTime;

use super::fsutil::{self, Entry, Kind, Visit, Walk};
use super::input::as_str;
use super::pattern::fnmatch;
use super::{Ctx, Tool};

const USAGE: &str = "find [-LP] [path...] [expression]";

/// Half the kernel's `EXEC_MAX_ARG_PAGES` budget (32 pages, 128 KiB), so a
/// `-exec ... +` batch still leaves room for the child's environment.
const EXEC_ARG_BUDGET: usize = 64 * 1024;

pub static TOOLS: &[Tool] = &[Tool {
    name: "find",
    desc: "Walk a directory tree and act on matching entries",
    usage: USAGE,
    run: find,
}];

#[derive(Clone, Copy)]
enum Cmp {
    Exact,
    Greater,
    Less,
}

enum Node {
    True,
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Not(Box<Node>),
    Name(Vec<u8>),
    IName(Vec<u8>),
    PathGlob(Vec<u8>),
    Type(u8),
    Size(Cmp, u64, u8),
    Newer(SystemTime),
    Empty,
    Prune,
    Print,
    Print0,
    Delete,
    Exec(usize),
}

struct Exec {
    argv: Vec<String>,
    batch: bool,
    pending: Vec<String>,
    base: usize,
    bytes: usize,
}

struct State {
    execs: Vec<Exec>,
    status: i32,
    prune: bool,
}

fn find(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut i = 1;
    let mut follow = false;
    while let Some(tok) = argv.get(i) {
        match *tok {
            b"-L" => follow = true,
            b"-P" => follow = false,
            _ => break,
        }
        i += 1;
    }

    let start = i;
    while let Some(tok) = argv.get(i) {
        if starts_expression(tok) {
            break;
        }
        i += 1;
    }
    let paths = &argv[start..i];

    let mut parser = Parser::new(&argv[i..]);
    let root = match parser.program() {
        Ok(node) => node,
        Err(message) => {
            ctx.warn(message.as_bytes());
            return ctx.usage(USAGE);
        }
    };
    let (maxdepth, mindepth, depth_first) = (parser.maxdepth, parser.mindepth, parser.depth_first);
    let mut state = State {
        execs: parser.execs,
        status: 0,
        prune: false,
    };

    let default_root: [&[u8]; 1] = [b"."];
    let roots = if paths.is_empty() {
        &default_root[..]
    } else {
        paths
    };

    for operand in roots {
        let Some(path) = as_str(ctx, operand) else {
            state.status = 1;
            continue;
        };
        walk_root(
            path,
            follow,
            depth_first,
            maxdepth,
            mindepth,
            &root,
            &mut state,
            ctx,
        );
        if ctx.out.broken() {
            break;
        }
    }

    for index in 0..state.execs.len() {
        if state.execs[index].batch {
            exec_flush(index, &mut state, ctx);
        }
    }
    state.status
}

#[allow(clippy::too_many_arguments)]
fn walk_root(
    path: &str,
    follow: bool,
    depth_first: bool,
    maxdepth: usize,
    mindepth: usize,
    root: &Node,
    state: &mut State,
    ctx: &mut Ctx,
) {
    let mut walk = Walk::new(path);
    if follow {
        walk = walk.follow();
    }
    if depth_first {
        walk = walk.with_post();
    }
    // `Walk` has no skip hook, so a pruned subtree is filtered by path prefix
    // rather than never entered.
    let mut pruned: Vec<String> = Vec::new();

    for item in walk {
        if ctx.out.broken() {
            return;
        }
        let visit = match item {
            Ok(visit) => visit,
            Err(error) => {
                ctx.warn_io(error.path.as_bytes(), &error.error);
                state.status = 1;
                continue;
            }
        };
        let post = matches!(visit, Visit::Post(_));
        let entry = match visit {
            Visit::Pre(entry) | Visit::Post(entry) => entry,
        };
        if depth_first && (entry.kind == Kind::Dir) != post {
            continue;
        }
        if entry.depth > maxdepth || entry.depth < mindepth {
            continue;
        }
        if pruned
            .iter()
            .any(|dir| under(dir.as_str(), entry.path.as_str()))
        {
            continue;
        }
        state.prune = false;
        eval(root, &entry, state, ctx);
        if state.prune {
            pruned.push(entry.path);
        }
    }
}

fn under(dir: &str, path: &str) -> bool {
    path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'/'
}

fn starts_expression(token: &[u8]) -> bool {
    token == b"!" || token == b"(" || token == b")" || (token.len() > 1 && token[0] == b'-')
}

fn eval(node: &Node, entry: &Entry, state: &mut State, ctx: &mut Ctx) -> bool {
    match node {
        Node::True => true,
        Node::And(left, right) => eval(left, entry, state, ctx) && eval(right, entry, state, ctx),
        Node::Or(left, right) => eval(left, entry, state, ctx) || eval(right, entry, state, ctx),
        Node::Not(inner) => !eval(inner, entry, state, ctx),
        Node::Name(pattern) => fnmatch(pattern, fsutil::base_name(&entry.path).as_bytes()),
        Node::IName(pattern) => {
            let name = fsutil::base_name(&entry.path).to_ascii_lowercase();
            fnmatch(pattern, name.as_bytes())
        }
        Node::PathGlob(pattern) => fnmatch(pattern, entry.path.as_bytes()),
        Node::Type(kind) => match kind {
            b'f' => entry.kind == Kind::File,
            b'd' => entry.kind == Kind::Dir,
            b'l' => entry.kind == Kind::Symlink,
            _ => false,
        },
        Node::Size(cmp, want, unit) => {
            let scale = match unit {
                b'c' => 1,
                b'k' => 1024,
                b'M' => 1024 * 1024,
                _ => 512,
            };
            let have = entry.meta.len().div_ceil(scale);
            match cmp {
                Cmp::Exact => have == *want,
                Cmp::Greater => have > *want,
                Cmp::Less => have < *want,
            }
        }
        Node::Newer(reference) => entry
            .meta
            .modified()
            .map(|stamp| stamp > *reference)
            .unwrap_or(false),
        Node::Empty => match entry.kind {
            Kind::Dir => fs::read_dir(&entry.path)
                .map(|mut dir| dir.next().is_none())
                .unwrap_or(false),
            Kind::File => entry.meta.len() == 0,
            _ => false,
        },
        Node::Prune => {
            if entry.kind == Kind::Dir {
                state.prune = true;
            }
            true
        }
        Node::Print => {
            ctx.out.s(&entry.path);
            ctx.out.nl();
            true
        }
        Node::Print0 => {
            ctx.out.s(&entry.path);
            ctx.out.b(0);
            true
        }
        Node::Delete => {
            let removed = if entry.kind == Kind::Dir {
                fs::remove_dir(&entry.path)
            } else {
                fs::remove_file(&entry.path)
            };
            match removed {
                Ok(()) => true,
                Err(error) => {
                    ctx.warn_io(entry.path.as_bytes(), &error);
                    state.status = 1;
                    false
                }
            }
        }
        Node::Exec(index) => {
            if state.execs[*index].batch {
                exec_push(*index, &entry.path, state, ctx);
                true
            } else {
                exec_one(*index, &entry.path, state, ctx)
            }
        }
    }
}

fn exec_one(index: usize, path: &str, state: &mut State, ctx: &mut Ctx) -> bool {
    let argv: Vec<String> = state.execs[index]
        .argv
        .iter()
        .map(|arg| arg.replace("{}", path))
        .collect();
    run_and_record(&argv, state, ctx)
}

fn exec_push(index: usize, path: &str, state: &mut State, ctx: &mut Ctx) {
    let spec = &state.execs[index];
    if !spec.pending.is_empty() && spec.bytes + path.len() + 1 > EXEC_ARG_BUDGET {
        exec_flush(index, state, ctx);
    }
    let spec = &mut state.execs[index];
    spec.bytes += path.len() + 1;
    spec.pending.push(path.to_string());
}

fn exec_flush(index: usize, state: &mut State, ctx: &mut Ctx) {
    if state.execs[index].pending.is_empty() {
        return;
    }
    let mut argv = state.execs[index].argv.clone();
    argv.append(&mut state.execs[index].pending);
    state.execs[index].bytes = state.execs[index].base;
    run_and_record(&argv, state, ctx);
}

fn run_and_record(argv: &[String], state: &mut State, ctx: &mut Ctx) -> bool {
    // The child inherits fd 1; buffered output has to land before it writes.
    ctx.out.flush();
    let Some(program) = resolve(&argv[0]) else {
        ctx.warn_at(argv[0].as_bytes(), b"not found");
        state.status = 1;
        return false;
    };
    let mut command = Command::new(&program);
    for arg in &argv[1..] {
        command.arg(arg);
    }
    match command.status() {
        Ok(status) if status.code() == Some(0) => true,
        Ok(_) => {
            state.status = 1;
            false
        }
        Err(error) => {
            ctx.warn_io(argv[0].as_bytes(), &error);
            state.status = 1;
            false
        }
    }
}

fn resolve(name: &str) -> Option<String> {
    if name.contains('/') {
        return is_executable(name).then(|| name.to_string());
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/bin:/sbin".to_string());
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| fsutil::join(dir, name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &str) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

struct Parser<'t> {
    toks: &'t [&'t [u8]],
    pos: usize,
    execs: Vec<Exec>,
    maxdepth: usize,
    mindepth: usize,
    has_action: bool,
    depth_first: bool,
}

impl<'t> Parser<'t> {
    fn new(toks: &'t [&'t [u8]]) -> Self {
        Self {
            toks,
            pos: 0,
            execs: Vec::new(),
            maxdepth: usize::MAX,
            mindepth: 0,
            has_action: false,
            depth_first: false,
        }
    }

    fn peek(&self) -> Option<&'t [u8]> {
        self.toks.get(self.pos).copied()
    }

    fn take(&mut self) -> Option<&'t [u8]> {
        let token = self.toks.get(self.pos).copied()?;
        self.pos += 1;
        Some(token)
    }

    fn argument(&mut self, flag: &str) -> Result<&'t [u8], String> {
        self.take()
            .ok_or_else(|| format!("missing argument to {flag}"))
    }

    fn program(&mut self) -> Result<Node, String> {
        if self.toks.is_empty() {
            return Ok(Node::Print);
        }
        let node = self.or_expr()?;
        if self.pos < self.toks.len() {
            return Err(format!("unexpected `{}`", show(self.toks[self.pos])));
        }
        if self.has_action {
            Ok(node)
        } else {
            Ok(Node::And(Box::new(node), Box::new(Node::Print)))
        }
    }

    fn or_expr(&mut self) -> Result<Node, String> {
        let mut left = self.and_expr()?;
        while matches!(self.peek(), Some(t) if t == b"-o" || t == b"-or") {
            self.pos += 1;
            let right = self.and_expr()?;
            left = Node::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Node, String> {
        let mut left = self.unary()?;
        loop {
            let Some(token) = self.peek() else { break };
            if token == b")" || token == b"-o" || token == b"-or" {
                break;
            }
            if token == b"-a" || token == b"-and" {
                self.pos += 1;
                if self.peek().is_none() {
                    return Err("missing expression after -a".to_string());
                }
            }
            let right = self.unary()?;
            left = Node::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Node, String> {
        match self.peek() {
            Some(token) if token == b"!" || token == b"-not" => {
                self.pos += 1;
                Ok(Node::Not(Box::new(self.unary()?)))
            }
            Some(_) => self.primary(),
            None => Err("missing expression".to_string()),
        }
    }

    fn primary(&mut self) -> Result<Node, String> {
        let token = self
            .take()
            .ok_or_else(|| "missing expression".to_string())?;
        if token == b"(" {
            let node = self.or_expr()?;
            match self.take() {
                Some(close) if close == b")" => return Ok(node),
                _ => return Err("missing `)`".to_string()),
            }
        }
        match token {
            b"-name" => Ok(Node::Name(self.argument("-name")?.to_vec())),
            b"-iname" => Ok(Node::IName(self.argument("-iname")?.to_ascii_lowercase())),
            b"-path" | b"-wholename" => Ok(Node::PathGlob(self.argument("-path")?.to_vec())),
            b"-type" => {
                let value = self.argument("-type")?;
                match value {
                    [kind @ (b'f' | b'd' | b'l')] => Ok(Node::Type(*kind)),
                    _ => Err(format!("unknown type `{}`", show(value))),
                }
            }
            // GNU treats these as global options: true wherever they appear,
            // and they bound the walk rather than the match.
            b"-maxdepth" => {
                self.maxdepth = self.count("-maxdepth")? as usize;
                Ok(Node::True)
            }
            b"-mindepth" => {
                self.mindepth = self.count("-mindepth")? as usize;
                Ok(Node::True)
            }
            b"-size" => self.size(),
            b"-newer" => {
                let value = self.argument("-newer")?;
                let name = std::str::from_utf8(value)
                    .map_err(|_| format!("invalid path `{}`", show(value)))?;
                let stamp = fs::metadata(name)
                    .and_then(|meta| meta.modified())
                    .map_err(|_| format!("cannot stat `{name}`"))?;
                Ok(Node::Newer(stamp))
            }
            b"-empty" => Ok(Node::Empty),
            b"-prune" => Ok(Node::Prune),
            b"-print" => {
                self.has_action = true;
                Ok(Node::Print)
            }
            b"-print0" => {
                self.has_action = true;
                Ok(Node::Print0)
            }
            b"-delete" => {
                self.has_action = true;
                // `-delete` needs its children gone first, so it turns the
                // walk depth-first, which in turn makes `-prune` inert.
                self.depth_first = true;
                Ok(Node::Delete)
            }
            b"-exec" => self.exec(),
            _ => Err(format!("unknown predicate `{}`", show(token))),
        }
    }

    fn count(&mut self, flag: &str) -> Result<u64, String> {
        let value = self.argument(flag)?;
        super::opts::parse_u64(value)
            .ok_or_else(|| format!("invalid argument `{}` to {flag}", show(value)))
    }

    fn size(&mut self) -> Result<Node, String> {
        let value = self.argument("-size")?;
        let (cmp, rest) = match value.first() {
            Some(b'+') => (Cmp::Greater, &value[1..]),
            Some(b'-') => (Cmp::Less, &value[1..]),
            _ => (Cmp::Exact, value),
        };
        let (digits, unit) = match rest.last() {
            Some(unit @ (b'c' | b'k' | b'M' | b'b')) => (&rest[..rest.len() - 1], *unit),
            _ => (rest, b'b'),
        };
        let want = super::opts::parse_u64(digits)
            .ok_or_else(|| format!("invalid size `{}`", show(value)))?;
        Ok(Node::Size(cmp, want, unit))
    }

    fn exec(&mut self) -> Result<Node, String> {
        let mut argv: Vec<String> = Vec::new();
        let mut batch = false;
        let mut closed = false;
        while let Some(token) = self.take() {
            if token == b";" {
                closed = true;
                break;
            }
            if token == b"+" {
                batch = true;
                closed = true;
                break;
            }
            let text = std::str::from_utf8(token)
                .map_err(|_| format!("invalid argument `{}` to -exec", show(token)))?;
            argv.push(text.to_string());
        }
        if !closed {
            return Err("missing `;` or `+` after -exec".to_string());
        }
        if batch {
            match argv.last() {
                Some(last) if last == "{}" => {
                    argv.pop();
                }
                _ => return Err("-exec ... + needs `{}` before the `+`".to_string()),
            }
        }
        if argv.is_empty() {
            return Err("missing command for -exec".to_string());
        }
        let base = argv.iter().map(|arg| arg.len() + 1).sum();
        self.has_action = true;
        self.execs.push(Exec {
            argv,
            batch,
            pending: Vec::new(),
            base,
            bytes: base,
        });
        Ok(Node::Exec(self.execs.len() - 1))
    }
}

fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
