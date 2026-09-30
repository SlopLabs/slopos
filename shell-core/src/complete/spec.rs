//! The `complete` builtin's rules. Flags follow fish's `complete`; `-A` is
//! bash's. Fish scopes a rule by a shell condition run on every Tab; `-P`
//! scopes it by the positional words before it, which describes a subcommand
//! tree without running anything:
//!
//! ```text
//! complete -c git -f -a '$(git --list-cmds=main,others,alias,nohelpers)'
//! complete -c git -P push -f -a '$(git remote)'
//! complete -c git -P 'push *' -f -a '$(git for-each-ref --format="%(refname:short)" refs/heads)'
//! complete -c git -P commit -s m -l message -x -d 'Commit message'
//! ```

use alloc::vec;
use alloc::vec::Vec;

use super::Candidate;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    File,
    Directory,
    Command,
    Builtin,
}

const ACTIONS: [(&[u8], Action); 4] = [
    (b"file", Action::File),
    (b"directory", Action::Directory),
    (b"command", Action::Command),
    (b"builtin", Action::Builtin),
];

impl Action {
    pub fn from_name(name: &[u8]) -> Option<Self> {
        ACTIONS.iter().find(|(n, _)| *n == name).map(|&(_, a)| a)
    }

    pub fn name(self) -> &'static [u8] {
        ACTIONS
            .iter()
            .find(|&&(_, a)| a == self)
            .map_or(b"", |&(n, _)| n)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub command: Vec<u8>,
    /// The positional words that must precede; any one alternative will do.
    /// `*` stands for any one word, and a final `**` for any number. The
    /// empty path is the command itself.
    pub after: Vec<Vec<Vec<u8>>>,
    pub short: Option<u8>,
    pub long: Option<Vec<u8>>,
    pub takes_value: bool,
    /// The argument, or the option's value, is not a file.
    pub no_files: bool,
    /// Shell text, expanded on each Tab so `$(...)` lists what is there now.
    pub words: Vec<u8>,
    pub actions: Vec<Action>,
    pub description: Vec<u8>,
}

impl Rule {
    fn is_option(&self) -> bool {
        self.short.is_some() || self.long.is_some()
    }

    /// Options apply anywhere below their path.
    fn applies_below(&self, positionals: &[&[u8]]) -> bool {
        self.after
            .iter()
            .any(|p| path_matches(p, positionals, false))
    }

    /// Arguments apply to the word right after their path.
    fn applies_at(&self, positionals: &[&[u8]]) -> bool {
        self.after
            .iter()
            .any(|p| path_matches(p, positionals, true))
    }

    /// Spelled whole, so the next word is its value.
    fn spelled_by(&self, word: &[u8]) -> bool {
        match word {
            [b'-', b'-', name @ ..] => self.long.as_deref() == Some(name),
            [b'-', short] => self.short == Some(*short),
            _ => false,
        }
    }
}

fn path_matches(path: &[Vec<u8>], words: &[&[u8]], exact: bool) -> bool {
    for (i, step) in path.iter().enumerate() {
        if step == b"**" {
            return true;
        }
        match words.get(i) {
            Some(word) if step == b"*" || step == word => {}
            _ => return false,
        }
    }
    !exact || path.len() == words.len()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Add(Vec<Rule>),
    Erase(Vec<Vec<u8>>),
    /// Empty: every command.
    Print(Vec<Vec<u8>>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flag {
    Command,
    After,
    Short,
    Long,
    Require,
    NoFiles,
    Exclusive,
    Words,
    Action,
    Description,
    Erase,
}

impl Flag {
    fn takes_value(self) -> bool {
        matches!(
            self,
            Flag::Command
                | Flag::After
                | Flag::Short
                | Flag::Long
                | Flag::Words
                | Flag::Action
                | Flag::Description
        )
    }
}

const FLAGS: [(u8, &[u8], Flag); 11] = [
    (b'c', b"command", Flag::Command),
    (b'P', b"after", Flag::After),
    (b's', b"short-option", Flag::Short),
    (b'l', b"long-option", Flag::Long),
    (b'r', b"require-parameter", Flag::Require),
    (b'f', b"no-files", Flag::NoFiles),
    (b'x', b"exclusive", Flag::Exclusive),
    (b'a', b"arguments", Flag::Words),
    (b'A', b"action", Flag::Action),
    (b'd', b"description", Flag::Description),
    (b'e', b"erase", Flag::Erase),
];

#[derive(Default)]
struct Parsed {
    commands: Vec<Vec<u8>>,
    after: Vec<Vec<Vec<u8>>>,
    short: Option<u8>,
    long: Option<Vec<u8>>,
    require: bool,
    no_files: bool,
    exclusive: bool,
    words: Vec<u8>,
    actions: Vec<Action>,
    description: Vec<u8>,
    erase: bool,
}

impl Parsed {
    fn apply(&mut self, flag: Flag, value: &[u8]) -> Result<(), &'static str> {
        match flag {
            Flag::Command => {
                if value.is_empty() || value.contains(&b'/') {
                    return Err("-c takes a command name, without a directory");
                }
                self.commands.push(value.to_vec());
            }
            Flag::After => {
                let path: Vec<Vec<u8>> = value
                    .split(|b| b.is_ascii_whitespace())
                    .filter(|w| !w.is_empty())
                    .map(<[u8]>::to_vec)
                    .collect();
                if path.iter().rev().skip(1).any(|w| w == b"**") {
                    return Err("-P: ** may only end a path");
                }
                self.after.push(path);
            }
            Flag::Short => match value {
                [b] if *b != b'-' && !b.is_ascii_whitespace() => {
                    if self.short.replace(*b).is_some() {
                        return Err("one -s per rule");
                    }
                }
                _ => return Err("-s takes one character"),
            },
            Flag::Long => {
                if value.is_empty() || value.contains(&b'=') || value.starts_with(b"-") {
                    return Err("-l takes an option name without its dashes");
                }
                if self.long.replace(value.to_vec()).is_some() {
                    return Err("one -l per rule");
                }
            }
            Flag::Require => self.require = true,
            Flag::NoFiles => self.no_files = true,
            Flag::Exclusive => self.exclusive = true,
            Flag::Words => {
                if !self.words.is_empty() {
                    self.words.push(b' ');
                }
                self.words.extend_from_slice(value);
            }
            Flag::Action => {
                let action = Action::from_name(value)
                    .ok_or("-A takes file, directory, command or builtin")?;
                if !self.actions.contains(&action) {
                    self.actions.push(action);
                }
            }
            Flag::Description => self.description = value.to_vec(),
            Flag::Erase => self.erase = true,
        }
        Ok(())
    }

    fn defines_anything(&self) -> bool {
        !self.after.is_empty()
            || self.short.is_some()
            || self.long.is_some()
            || self.require
            || self.no_files
            || self.exclusive
            || !self.words.is_empty()
            || !self.actions.is_empty()
            || !self.description.is_empty()
    }
}

/// `args` excludes the builtin's own name.
pub fn parse(args: &[&[u8]]) -> Result<Request, &'static str> {
    let mut parsed = Parsed::default();
    let mut i = 0usize;
    let next_value = |i: &mut usize| -> Result<&[u8], &'static str> {
        let value = args.get(*i).ok_or("option requires a value")?;
        *i += 1;
        Ok(*value)
    };
    while i < args.len() {
        let arg = args[i];
        i += 1;
        if let Some(body) = arg.strip_prefix(b"--") {
            let (name, inline) = match body.iter().position(|&b| b == b'=') {
                Some(eq) => (&body[..eq], Some(&body[eq + 1..])),
                None => (body, None),
            };
            let &(_, _, flag) = FLAGS
                .iter()
                .find(|(_, long, _)| *long == name)
                .ok_or("unknown option")?;
            let value: &[u8] = match (flag.takes_value(), inline) {
                (true, Some(value)) => value,
                (true, None) => next_value(&mut i)?,
                (false, Some(_)) => return Err("option takes no value"),
                (false, None) => b"",
            };
            parsed.apply(flag, value)?;
        } else if arg.len() > 1 && arg[0] == b'-' {
            let mut k = 1usize;
            while k < arg.len() {
                let &(_, _, flag) = FLAGS
                    .iter()
                    .find(|(short, _, _)| *short == arg[k])
                    .ok_or("unknown option")?;
                k += 1;
                if !flag.takes_value() {
                    parsed.apply(flag, b"")?;
                    continue;
                }
                let value = if k < arg.len() {
                    &arg[k..]
                } else {
                    next_value(&mut i)?
                };
                parsed.apply(flag, value)?;
                break;
            }
        } else {
            return Err("unexpected argument");
        }
    }

    if parsed.erase {
        if parsed.defines_anything() {
            return Err("-e takes only -c");
        }
        if parsed.commands.is_empty() {
            return Err("-e needs -c");
        }
        return Ok(Request::Erase(parsed.commands));
    }
    if !parsed.defines_anything() {
        return Ok(Request::Print(parsed.commands));
    }
    if parsed.commands.is_empty() {
        return Err("a rule needs -c");
    }
    let is_option = parsed.short.is_some() || parsed.long.is_some();
    if parsed.require && !is_option {
        return Err("-r belongs to an option: give -s or -l");
    }
    let takes_value = is_option
        && (parsed.require
            || parsed.exclusive
            || !parsed.words.is_empty()
            || !parsed.actions.is_empty());
    let after = if parsed.after.is_empty() {
        vec![Vec::new()]
    } else {
        parsed.after
    };
    let rules = parsed
        .commands
        .into_iter()
        .map(|command| Rule {
            command,
            after: after.clone(),
            short: parsed.short,
            long: parsed.long.clone(),
            takes_value,
            no_files: parsed.no_files || parsed.exclusive,
            words: parsed.words.clone(),
            actions: parsed.actions.clone(),
            description: parsed.description.clone(),
        })
        .collect();
    Ok(Request::Add(rules))
}

/// Parses back to `rule`.
pub fn render(rule: &Rule) -> Vec<u8> {
    let mut out = b"complete -c ".to_vec();
    quote_arg(&rule.command, &mut out);
    let root: [Vec<Vec<u8>>; 1] = [Vec::new()];
    if rule.after != root {
        for path in &rule.after {
            out.extend_from_slice(b" -P ");
            quote_arg(&path.join(&b' '), &mut out);
        }
    }
    if let Some(short) = rule.short {
        out.extend_from_slice(b" -s ");
        quote_arg(&[short], &mut out);
    }
    if let Some(long) = &rule.long {
        out.extend_from_slice(b" -l ");
        quote_arg(long, &mut out);
    }
    if rule.takes_value {
        out.extend_from_slice(b" -r");
    }
    if rule.no_files {
        out.extend_from_slice(b" -f");
    }
    if !rule.words.is_empty() {
        out.extend_from_slice(b" -a ");
        quote_arg(&rule.words, &mut out);
    }
    for action in &rule.actions {
        out.extend_from_slice(b" -A ");
        out.extend_from_slice(action.name());
    }
    if !rule.description.is_empty() {
        out.extend_from_slice(b" -d ");
        quote_arg(&rule.description, &mut out);
    }
    out
}

fn quote_arg(value: &[u8], out: &mut Vec<u8>) {
    let plain = !value.is_empty()
        && value
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./:@%+,=".contains(b));
    if plain {
        out.extend_from_slice(value);
        return;
    }
    out.push(b'\'');
    for &b in value {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
}

#[derive(Debug, PartialEq, Eq)]
pub struct Words {
    pub text: Vec<u8>,
    pub description: Vec<u8>,
}

/// Where the candidates for one word come from.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// Final: needs no expansion.
    pub options: Vec<Candidate>,
    pub words: Vec<Words>,
    pub actions: Vec<Action>,
    pub files: bool,
    /// `--name=` when a value is spelled into its option: it leads every
    /// candidate the shell finds, which match what follows it.
    pub lead: Vec<u8>,
}

impl Query {
    fn files() -> Self {
        Self {
            files: true,
            ..Self::default()
        }
    }

    fn value_of(rule: &Rule, lead: Vec<u8>) -> Self {
        let words = if rule.words.is_empty() {
            Vec::new()
        } else {
            vec![Words {
                text: rule.words.clone(),
                description: Vec::new(),
            }]
        };
        Self {
            options: Vec::new(),
            words,
            actions: rule.actions.clone(),
            files: !rule.no_files,
            lead,
        }
    }
}

#[derive(Default)]
pub struct Specs {
    rules: Vec<Rule>,
}

impl Specs {
    pub const fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn add(&mut self, rules: Vec<Rule>) {
        for rule in rules {
            if !self.rules.contains(&rule) {
                self.rules.push(rule);
            }
        }
    }

    pub fn erase(&mut self, command: &[u8]) {
        self.rules.retain(|r| r.command != command);
    }

    pub fn covers(&self, command: &[u8]) -> bool {
        self.rules.iter().any(|r| r.command == command)
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// `args` are the command's words before `current`.
    pub fn query(&self, command: &[u8], args: &[Vec<u8>], current: &[u8]) -> Query {
        let rules: Vec<&Rule> = self.rules.iter().filter(|r| r.command == command).collect();
        if rules.is_empty() {
            return Query::files();
        }

        let mut positionals: Vec<&[u8]> = Vec::new();
        let mut value_of: Option<&Rule> = None;
        let mut options_ended = false;
        for arg in args {
            if value_of.take().is_some() {
                continue;
            }
            if !options_ended && arg == b"--" {
                options_ended = true;
            } else if !options_ended && arg.len() > 1 && arg[0] == b'-' {
                value_of = rules
                    .iter()
                    .copied()
                    .find(|r| r.takes_value && r.spelled_by(arg) && r.applies_below(&positionals));
            } else {
                positionals.push(arg);
            }
        }
        if let Some(rule) = value_of {
            return Query::value_of(rule, Vec::new());
        }

        if !options_ended && current.first() == Some(&b'-') {
            if let Some(eq) = current.iter().position(|&b| b == b'=')
                && let Some(name) = current[..eq].strip_prefix(b"--")
                && let Some(rule) = rules.iter().find(|r| {
                    r.takes_value
                        && r.long.as_deref() == Some(name)
                        && r.applies_below(&positionals)
                })
            {
                return Query::value_of(rule, current[..=eq].to_vec());
            }
            let mut options = Vec::new();
            for rule in rules.iter().filter(|r| r.applies_below(&positionals)) {
                if let Some(long) = &rule.long {
                    let mut word = b"--".to_vec();
                    word.extend_from_slice(long);
                    options.push(Candidate::new(word).described(&rule.description));
                }
                if let Some(short) = rule.short {
                    options.push(Candidate::new(vec![b'-', short]).described(&rule.description));
                }
            }
            options.retain(|c| c.word.starts_with(current));
            if !options.is_empty() {
                return Query {
                    options,
                    ..Query::default()
                };
            }
        }

        let matching: Vec<&Rule> = rules
            .iter()
            .copied()
            .filter(|r| !r.is_option() && r.applies_at(&positionals))
            .collect();
        if matching.is_empty() {
            return Query::files();
        }
        let mut query = Query {
            files: !matching.iter().any(|r| r.no_files),
            ..Query::default()
        };
        for rule in matching {
            if !rule.words.is_empty() {
                query.words.push(Words {
                    text: rule.words.clone(),
                    description: rule.description.clone(),
                });
            }
            for &action in &rule.actions {
                if !query.actions.contains(&action) {
                    query.actions.push(action);
                }
            }
        }
        query
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs(lines: &[&[&str]]) -> Specs {
        let mut specs = Specs::default();
        for line in lines {
            let args: Vec<&[u8]> = line.iter().map(|a| a.as_bytes()).collect();
            match parse(&args) {
                Ok(Request::Add(rules)) => specs.add(rules),
                other => panic!("{line:?}: {other:?}"),
            }
        }
        specs
    }

    fn query(specs: &Specs, line: &str) -> Query {
        let mut words: Vec<Vec<u8>> = line.split(' ').map(|w| w.as_bytes().to_vec()).collect();
        let current = words.pop().unwrap();
        let command = words.remove(0);
        specs.query(&command, &words, &current)
    }

    fn texts(query: &Query) -> Vec<&[u8]> {
        query.words.iter().map(|w| w.text.as_slice()).collect()
    }

    fn args(line: &str) -> Vec<&[u8]> {
        line.split(' ').map(str::as_bytes).collect()
    }

    fn git() -> Specs {
        specs(&[
            &["-c", "git", "-f", "-a", "$(git --list-cmds=main)"],
            &["-c", "git", "-s", "C", "-x", "-A", "directory"],
            &["-c", "git", "-l", "version", "-d", "Print the version"],
            &["-c", "git", "-P", "push", "-f", "-a", "$(git remote)"],
            &["-c", "git", "-P", "push *", "-f", "-a", "$(branches)"],
            &[
                "-c", "git", "-P", "commit", "-s", "m", "-l", "message", "-x",
            ],
            &["-c", "git", "-P", "commit", "-l", "amend"],
        ])
    }

    #[test]
    fn an_unknown_command_completes_files() {
        assert_eq!(query(&git(), "cat x"), Query::files());
    }

    #[test]
    fn a_path_scopes_a_rule_to_one_word() {
        let git = git();
        let root = query(&git, "git pu");
        assert_eq!(texts(&root), [&b"$(git --list-cmds=main)"[..]]);
        assert!(!root.files);
        assert_eq!(texts(&query(&git, "git push or")), [&b"$(git remote)"[..]]);
        assert_eq!(
            texts(&query(&git, "git push origin ma")),
            [&b"$(branches)"[..]]
        );
        assert_eq!(query(&git, "git push origin main x"), Query::files());
        assert_eq!(query(&git, "git add sr"), Query::files());
    }

    #[test]
    fn options_and_their_values_are_skipped_when_counting() {
        let git = git();
        let query = query(&git, "git -C /tmp push --force or");
        assert_eq!(texts(&query), [&b"$(git remote)"[..]]);
    }

    #[test]
    fn an_option_value_completes_from_its_rule() {
        let git = git();
        let value = query(&git, "git -C sr");
        assert_eq!(value.actions, [Action::Directory]);
        assert!(!value.files);
        let message = query(&git, "git commit -m ");
        assert_eq!(message, Query::default());
        let spelled = query(&git, "git commit --message=x");
        assert_eq!(spelled.lead, b"--message=");
    }

    #[test]
    fn a_dash_offers_the_options_under_the_path() {
        let git = git();
        let names = |q: Query| -> Vec<Vec<u8>> { q.options.into_iter().map(|c| c.word).collect() };
        assert_eq!(names(query(&git, "git --v")), [b"--version".to_vec()]);
        let under_commit = names(query(&git, "git commit --"));
        assert!(under_commit.contains(&b"--amend".to_vec()));
        assert!(under_commit.contains(&b"--version".to_vec()));
        assert!(!names(query(&git, "git push --")).contains(&b"--amend".to_vec()));
        assert_eq!(
            texts(&query(&git, "git -q")),
            [&b"$(git --list-cmds=main)"[..]]
        );
    }

    #[test]
    fn double_star_matches_any_depth() {
        let mkdir = specs(&[&["-c", "mkdir", "-P", "**", "-f", "-A", "directory"]]);
        for line in ["mkdir a", "mkdir x y z a"] {
            assert_eq!(query(&mkdir, line).actions, [Action::Directory], "{line}");
        }
    }

    #[test]
    fn alternatives_share_a_rule() {
        let cargo = specs(&[&["-c", "cargo", "-P", "build", "-P", "b", "-l", "release"]]);
        for line in ["cargo build --r", "cargo b --r"] {
            assert_eq!(query(&cargo, line).options.len(), 1, "{line}");
        }
        assert!(query(&cargo, "cargo run --r").options.is_empty());
    }

    #[test]
    fn requests_parse_as_getopt_reads_them() {
        assert_eq!(parse(&[]), Ok(Request::Print(Vec::new())));
        assert_eq!(
            parse(&args("-c git")),
            Ok(Request::Print(vec![b"git".to_vec()]))
        );
        assert_eq!(
            parse(&args("-e -c git")),
            Ok(Request::Erase(vec![b"git".to_vec()]))
        );
        let Ok(Request::Add(rules)) = parse(&args("-cgit -xs C --action=directory")) else {
            panic!("combined flags");
        };
        assert_eq!(rules[0].short, Some(b'C'));
        assert!(rules[0].takes_value && rules[0].no_files);
        assert_eq!(rules[0].actions, [Action::Directory]);
        let Ok(Request::Add(rules)) = parse(&args("-c a -c b -a x -a y")) else {
            panic!("two commands");
        };
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[1].words, b"x y");
    }

    #[test]
    fn malformed_requests_are_refused() {
        for bad in [
            "-a x",
            "-c git -r",
            "-c git -s ab",
            "-c git -l --x",
            "-c git -A nothing",
            "-c /usr/bin/git -f",
            "-c git -e -f",
            "-c git -q",
            "-c git stray",
            "-c",
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad}");
        }
        assert!(parse(&[b"-c", b"git", b"-P", b"** x"]).is_err());
    }

    #[test]
    fn a_rendered_rule_parses_back_to_itself() {
        let git = git();
        for rule in git.rules() {
            let text = render(rule);
            let words = crate::lexer::lex(&text).unwrap();
            let args: Vec<Vec<u8>> = words
                .iter()
                .skip(1)
                .map(|t| match t {
                    crate::lexer::Tok::Word(w) => crate::lexer::remove_quotes(w),
                    other => panic!("{other:?}"),
                })
                .collect();
            let args: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
            assert_eq!(parse(&args), Ok(Request::Add(vec![rule.clone()])));
        }
    }
}
