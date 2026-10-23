//! The installer as a person at a terminal runs it: each choice a question
//! or a flag, the proposal summarised and confirmed by typing, and the
//! install's events printed as lines.

use std::io::{Write, stdin, stdout};

use super::disk::{Disk, human};
use super::engine::{
    Choices, DEFAULT_REMOTE, Event, Events, FIRST_PERSON, Issue, Mode, Person, Proposal, State,
    System,
};

/// How much a copy moves between the lines that say so.
const PROGRESS_EVERY: u64 = 256 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Erase,
    Free,
    Reuse,
}

#[derive(Default)]
struct Options {
    disk: Option<String>,
    mode: Option<Kind>,
    region: Option<usize>,
    root: Option<String>,
    keep_root: Option<bool>,
    remote: Option<String>,
    first: Option<bool>,
    person: Option<String>,
    no_password: bool,
    cmdline: Option<String>,
    yes: bool,
}

const USAGE: &str = "usage: installer [--disk /dev/<disk>] [--mode erase|free|reuse] [--region <n>]
                 [--root /dev/<partition>] [--keep-root | --format-root] [--remote <url>]
                 [--first | --not-first] [--person <name>] [--no-password]
                 [--cmdline <words>] [--yes]
  --disk         the disk to install onto
  --mode         erase: a new partition table over the whole disk
                 free: SlopOS's partitions in free space, beside the others
                 reuse: the root over --root, the rest where the disk has them
  --region       the free region new partitions go into (default: the largest)
  --root         the partition reuse puts the root on
  --keep-root    keep the SlopOS root reuse finds there, updating /usr/local
  --format-root  format it instead
  --remote       the URL /src/slopos fetches from and pushes to
  --first        put SlopOS first in the firmware's boot order
  --not-first    leave it where it stands, or after the systems there
  --person       the name of the person the new root is for (default: user)
  --no-password  give them no password, where one is otherwise typed
  --cmdline      what the installed kernel boots with
  --yes          write the disk without asking";

fn parse(args: &[String]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let mut value = || {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--disk" => opts.disk = Some(value()?),
            "--mode" => {
                opts.mode = Some(match value()?.as_str() {
                    "erase" => Kind::Erase,
                    "free" => Kind::Free,
                    "reuse" => Kind::Reuse,
                    other => return Err(format!("--mode {other}: erase, free or reuse")),
                })
            }
            "--region" => {
                let text = value()?;
                let n: usize = text
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or_else(|| format!("--region {text}: a region's number, from 1"))?;
                opts.region = Some(n - 1);
            }
            "--root" => opts.root = Some(value()?),
            "--keep-root" => opts.keep_root = Some(true),
            "--format-root" => opts.keep_root = Some(false),
            "--remote" => opts.remote = Some(value()?),
            "--first" => opts.first = Some(true),
            "--not-first" => opts.first = Some(false),
            "--person" => opts.person = Some(value()?),
            "--no-password" => opts.no_password = true,
            "--cmdline" => opts.cmdline = Some(value()?),
            "--yes" => opts.yes = true,
            "--help" | "-h" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown flag {other}\n{USAGE}")),
        }
    }
    Ok(opts)
}

/// An answer from standard input, or the default on an empty line; at end of
/// input there is nobody to ask, and the flag is named instead.
fn ask(question: &str, default: Option<&str>, flag: &str) -> Result<String, String> {
    match default {
        Some(d) => print!("{question} [{d}]: "),
        None => print!("{question}: "),
    }
    let _ = stdout().flush();
    let mut line = String::new();
    match stdin().read_line(&mut line) {
        Ok(0) => Err(format!(
            "no answer to \"{question}\"; pass {flag} to answer it"
        )),
        Ok(_) => match (line.trim(), default) {
            ("", Some(d)) => Ok(d.to_owned()),
            ("", None) => ask(question, default, flag),
            (answer, _) => Ok(answer.to_owned()),
        },
        Err(e) => Err(format!("reading the answer: {e}")),
    }
}

fn ask_yes(question: &str, default: bool, flag: &str) -> Result<bool, String> {
    loop {
        match ask(question, Some(if default { "yes" } else { "no" }), flag)?.as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("yes or no"),
        }
    }
}

fn choose_disk(system: &System) -> Result<String, String> {
    let mut offered = Vec::new();
    println!("Disks:");
    for found in &system.disks {
        let node = &found.node;
        if let State::Refused(why) = found.state {
            println!("{node}: {why}, not offered");
            continue;
        }
        match found.disk() {
            Ok(disk) => print!("{}", disk.describe()),
            Err(why) => println!("{node}: {why}"),
        }
        offered.push(node.as_str());
    }
    let default = match offered.as_slice() {
        [] => return Err("no disk to install onto".to_owned()),
        [only] => Some(*only),
        _ => None,
    };
    ask("Disk to install onto", default, "--disk")
}

/// `nvme0n1` as well as `/dev/nvme0n1`.
fn disk_node(named: String) -> String {
    if named.starts_with('/') {
        named
    } else {
        format!("/dev/{named}")
    }
}

fn choose_kind(disk: &Disk) -> Result<Kind, String> {
    if disk.table.is_none() {
        println!(
            "{} holds {}, so only erase can install onto it.",
            disk.node,
            disk.holding()
        );
        return Ok(Kind::Erase);
    }
    loop {
        let answer = ask(
            "erase the whole disk, install into free space beside the rest, or reuse a partition as the root (erase/free/reuse)",
            Some("free"),
            "--mode",
        )?;
        match answer.as_str() {
            "erase" => return Ok(Kind::Erase),
            "free" => return Ok(Kind::Free),
            "reuse" => return Ok(Kind::Reuse),
            _ => println!("erase, free or reuse"),
        }
    }
}

fn choose_region(disk: &Disk, given: Option<usize>, ask_it: bool) -> Result<Option<usize>, String> {
    let regions = disk.regions();
    if given.is_some() || !ask_it || regions.len() < 2 {
        return Ok(given);
    }
    let largest = (0..regions.len())
        .max_by_key(|&i| regions[i].blocks())
        .unwrap_or(0);
    let answer = ask(
        "Free region for the new partitions",
        Some(&(largest + 1).to_string()),
        "--region",
    )?;
    answer
        .parse::<usize>()
        .ok()
        .filter(|&n| (1..=regions.len()).contains(&n))
        .map(|n| Some(n - 1))
        .ok_or_else(|| format!("{answer}: no free region of that number"))
}

/// The new root's first person, an administrator the desktop logs in, with
/// their password's hash when one is typed.
fn choose_person(named: Option<String>, no_password: bool) -> Result<Person, String> {
    let name = match named {
        Some(name) => name,
        None => ask(
            "Name of the person this machine is for",
            Some(FIRST_PERSON),
            "--person",
        )?,
    };
    if !slopos_accounts_core::valid_name(&name) {
        return Err(format!(
            "{name}: {}",
            slopos_accounts_core::record::BAD_NAME
        ));
    }
    if no_password {
        return Ok(Person {
            name,
            password: None,
        });
    }
    let _quiet = crate::secret::quiet()?;
    let typed = crate::secret::read(&format!("Password for {name}, or nothing for none: "))?
        .ok_or("no password typed; pass --no-password for none")?;
    if typed.is_empty() {
        return Ok(Person {
            name,
            password: None,
        });
    }
    let again = crate::secret::read("Again: ")?.ok_or("no password typed again")?;
    if again != typed {
        return Err("the two passwords differ; nothing was written".into());
    }
    Ok(Person {
        name,
        password: Some(typed.hash()?),
    })
}

/// What the installer was asked to do, settled and confirmed before anything
/// is written.
fn settle(opts: Options, system: &mut System) -> Result<(Choices, Proposal), String> {
    let issue = |issue: Issue| issue.to_string();
    let interactive = opts.disk.is_none();
    let node = disk_node(match opts.disk {
        Some(node) => node,
        None => choose_disk(system)?,
    });
    system.open(&node).map_err(issue)?;
    let open = system.opened(&node).map_err(issue)?;
    let disk = &open.disk;
    if !interactive {
        print!("{}", disk.describe());
    }
    let kind = match opts.mode {
        Some(kind) => kind,
        None if opts.yes => {
            return Err(format!(
                "--yes needs --mode: with nobody asked, nothing chooses what happens to {node}"
            ));
        }
        None => choose_kind(disk)?,
    };
    if kind != Kind::Erase && disk.table.is_none() {
        return Err(issue(Issue::OnlyErase {
            node,
            holds: disk.holding(),
        }));
    }
    let asked = opts.mode.is_none();
    let mode = match kind {
        Kind::Erase => Mode::Erase,
        Kind::Free => Mode::Free {
            region: choose_region(disk, opts.region, asked)?,
        },
        Kind::Reuse => {
            let named = match opts.root {
                Some(named) => named,
                None => ask("Partition for the root", None, "--root")?,
            };
            Mode::Reuse {
                root: disk.partition_named(&named)?.entry.number,
                region: choose_region(disk, opts.region, asked)?,
                keep: false,
            }
        }
    };
    let mut choices = Choices {
        disk: node,
        mode,
        remote: None,
        first: false,
        person: None,
        cmdline: opts.cmdline,
    };
    // What the layout refuses is said before anything else is asked.
    system.propose(&choices).map_err(issue)?;

    if let Mode::Reuse { root, keep, .. } = &mut choices.mode {
        let slopos = open.holds_root(*root);
        *keep = match opts.keep_root {
            Some(true) if !slopos => {
                return Err(issue(Issue::NothingToKeep(
                    open.node(*root).map_err(issue)?,
                )));
            }
            Some(keep) => keep,
            None if slopos => ask_yes(
                &format!(
                    "{} holds a SlopOS root: keep what it holds, updating /usr/local",
                    open.node(*root).map_err(issue)?
                ),
                true,
                "--keep-root or --format-root",
            )?,
            None => false,
        };
    }
    let keep_root = matches!(choices.mode, Mode::Reuse { keep: true, .. });
    if system.medium.source {
        choices.remote = Some(match opts.remote {
            Some(url) => url,
            None => ask(
                "Where /src/slopos fetches from and pushes to",
                Some(DEFAULT_REMOTE),
                "--remote",
            )?,
        });
    }
    choices.first = match opts.first {
        Some(first) => first,
        None => ask_yes(
            "Put SlopOS first in the firmware's boot order",
            false,
            "--first or --not-first",
        )?,
    };
    choices.person = match keep_root {
        true if opts.person.is_some() || opts.no_password => {
            return Err(
                "--person and --no-password are a new root's; a kept root keeps its people".into(),
            );
        }
        true => None,
        false => Some(choose_person(opts.person, opts.no_password)?),
    };
    let proposal = system.propose(&choices).map_err(issue)?;
    for line in proposal.left_out() {
        println!("installer:   {line}");
    }
    println!("\nSlopOS will be installed on {}:", proposal.disk());
    for line in &proposal.review {
        println!("  {line}");
    }
    if !opts.yes {
        confirm(&proposal)?;
    }
    Ok((choices, proposal))
}

fn confirm(p: &Proposal) -> Result<(), String> {
    let (want, question) = if p.erase {
        let name = p.disk().rsplit('/').next().unwrap_or(p.disk()).to_owned();
        let question = format!("Type {name} to erase it");
        (name, question)
    } else {
        ("yes".to_owned(), "Type yes to write the disk".to_owned())
    };
    let answer = ask(&question, None, "--yes")?;
    if answer == want {
        Ok(())
    } else {
        Err("nothing was written".to_owned())
    }
}

/// Each event as a line, and a copy's progress every [`PROGRESS_EVERY`].
#[derive(Default)]
struct Lines {
    reported: u64,
}

impl Events for Lines {
    fn event(&mut self, event: Event) {
        match event {
            Event::Step {
                index,
                total,
                title,
            } => println!("installer: [{index}/{total}] {title}"),
            Event::Log { line } => println!("installer:   {line}"),
            Event::Progress { done, total } => {
                if done < self.reported {
                    self.reported = 0;
                }
                if done - self.reported >= PROGRESS_EVERY {
                    self.reported = done;
                    println!("installer:   {} of {} copied", human(done), human(total));
                }
            }
        }
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    let opts = parse(args)?;
    crate::privd::elevate(
        Some("org.slopos.install"),
        &crate::privd::own_args(),
        crate::privd::Input::Terminal,
    );
    crate::secret::claim_terminal();
    let mut system = System::probe().map_err(|issue| issue.to_string())?;
    if let Some(node) = &system.medium.unmounted_payload {
        println!(
            "installer: {node} carries the medium's payload, which the kernel did not mount; its log says why"
        );
    }
    let (choices, proposal) = settle(opts, &mut system)?;
    let summary = system
        .install(&choices, &proposal, &mut Lines::default())
        .map_err(|failure| failure.message)?;
    println!("installer: {summary}; take the medium out and restart");
    Ok(())
}
