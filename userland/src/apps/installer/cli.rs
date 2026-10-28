//! The installer at a terminal: each choice a question or a flag, the
//! proposal confirmed by typing, and the install's events printed as lines.

use std::io::{Write, stdin, stdout};

use slopos_installer_core::{
    Choices, DEFAULT_REMOTE, Event, FIRST_PERSON, Issue, Mode, NOTHING_WRITTEN, Person, disk_size,
    parse_size, short,
};

use super::disk::Disk;
use super::engine::{Events, Settled, State, System};

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
    root_size: Option<u64>,
    remote: Option<String>,
    first: Option<bool>,
    person: Option<String>,
    no_password: bool,
    cmdline: Option<String>,
    yes: bool,
}

const USAGE: &str = "usage: installer [--disk /dev/<disk>] [--mode erase|free|reuse] [--region <n>]
                 [--root-size <size>] [--root /dev/<partition>]
                 [--keep-root | --format-root] [--remote <url>]
                 [--first | --not-first] [--person <name>] [--no-password]
                 [--cmdline <words>] [--yes]
  --disk         the disk to install onto
  --mode         erase: a new partition table over the whole disk
                 free: SlopOS's partitions in free space, beside the others
                 reuse: SlopOS on the partition --root names, the rest where
                 the disk has them
  --region       the free space new partitions go into, by its number in the
                 disk's listing (default: the largest)
  --root-size    how large SlopOS's partition is, in decimal units as 20GB,
                 512MB or 1.5TB, a bare number being GB (default: all the
                 free space gives); the crash record partition follows it
                 and the rest stays free
  --root         the partition reuse installs SlopOS on
  --keep-root    keep the SlopOS that partition holds, its accounts and their
                 files, updating /usr/local
  --format-root  erase it instead
  --remote       the address /src/slopos fetches from and pushes to
  --first        put SlopOS first in the startup order
  --not-first    leave it where it stands, or after the systems there
  --person       the name of the first account (default: user)
  --no-password  give it no password, where one is otherwise typed
  --cmdline      what the installed kernel starts with
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
                    .ok_or_else(|| format!("--region {text}: a free space's number, from 1"))?;
                opts.region = Some(n - 1);
            }
            "--root-size" => {
                let text = value()?;
                opts.root_size = Some(parse_size(&text).ok_or_else(|| {
                    format!("--root-size {text}: a size such as 20GB, 512MB or 1.5TB")
                })?);
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
            "No answer to “{question}”. Pass {flag} to answer it."
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
            _ => println!("Answer yes or no."),
        }
    }
}

fn choose_disk(system: &System) -> Result<String, String> {
    let mut offered = Vec::new();
    println!("Disks:");
    for found in &system.disks {
        let node = &found.node;
        let model = found.model.as_deref();
        match &found.state {
            State::Refused(why) => {
                println!("{node}: {} It isn't offered.", why.words());
                continue;
            }
            State::Unreadable(why) => {
                println!("{why}");
                continue;
            }
            State::Listed(disk) => print!("{}", disk.describe(model)),
            State::Opened(open) => print!("{}", open.disk.describe(model)),
        }
        offered.push(node.as_str());
    }
    let default = match offered.as_slice() {
        [] => return Err("There's no disk to install onto.".to_owned()),
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
    if let Err(held) = &disk.table {
        println!(
            "{}: {} SlopOS is installed by erasing it.",
            disk.node,
            held.only_erase()
        );
        return Ok(Kind::Erase);
    }
    loop {
        let answer = ask(
            "Erase the whole disk, install in free space beside the rest, or install on a partition (erase/free/reuse)",
            Some("free"),
            "--mode",
        )?;
        match answer.as_str() {
            "erase" => return Ok(Kind::Erase),
            "free" => return Ok(Kind::Free),
            "reuse" => return Ok(Kind::Reuse),
            _ => println!("Answer erase, free or reuse."),
        }
    }
}

fn choose_region(disk: &Disk, given: Option<usize>, ask_it: bool) -> Result<Option<usize>, String> {
    let regions = disk.regions();
    if given.is_some() || !ask_it || regions.len() < 2 {
        return Ok(given);
    }
    let largest = disk.largest_region().unwrap_or(0);
    let answer = ask(
        "Free space for the new partitions",
        Some(&(largest + 1).to_string()),
        "--region",
    )?;
    answer
        .parse::<usize>()
        .ok()
        .filter(|&n| (1..=regions.len()).contains(&n))
        .map(|n| Some(n - 1))
        .ok_or_else(|| format!("{answer}: the disk lists no free space of that number"))
}

/// The new root's first person, an administrator the desktop logs in, with
/// their password's hash when one is typed.
fn choose_person(named: Option<String>, no_password: bool) -> Result<Person, String> {
    let name = match named {
        Some(name) => name,
        None => ask(
            "Name of the first account on this computer",
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
        .ok_or("No password was typed. Pass --no-password for none.")?;
    if typed.is_empty() {
        return Ok(Person {
            name,
            password: None,
        });
    }
    let again = crate::secret::read("Again: ")?.ok_or("The password wasn't typed again.")?;
    if again != typed {
        return Err(format!("The two passwords differ. {NOTHING_WRITTEN}"));
    }
    Ok(Person {
        name,
        password: Some(typed.hash()?),
    })
}

/// What the installer was asked to do, settled and confirmed before anything
/// is written.
fn settle(opts: Options, system: &mut System) -> Result<(Choices, Settled), String> {
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
        print!("{}", disk.describe(system.model(&node)));
    }
    let kind = match opts.mode {
        Some(kind) => kind,
        None if opts.yes => {
            return Err(format!(
                "--yes needs --mode: with nobody asked, nothing chooses what happens to {node}."
            ));
        }
        None => choose_kind(disk)?,
    };
    if kind == Kind::Reuse && opts.root_size.is_some() {
        return Err(issue(Issue::RootSize));
    }
    if kind != Kind::Erase
        && let Err(held) = &disk.table
    {
        return Err(issue(Issue::OnlyErase {
            node,
            contents: held.clone(),
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
                None => ask("Partition to install SlopOS on", None, "--root")?,
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
        root_size: opts.root_size,
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
                    "{} holds SlopOS: keep its accounts and their files, updating /usr/local",
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
                "Address /src/slopos fetches from and pushes to",
                Some(DEFAULT_REMOTE),
                "--remote",
            )?,
        });
    }
    choices.first = match opts.first {
        Some(first) => first,
        None => ask_yes(
            "Put SlopOS first in the startup order",
            false,
            "--first or --not-first",
        )?,
    };
    choices.person = match keep_root {
        true if opts.person.is_some() || opts.no_password => {
            return Err(
                "--person and --no-password set up a new SlopOS; a kept one keeps its accounts."
                    .into(),
            );
        }
        true => None,
        false => Some(choose_person(opts.person, opts.no_password)?),
    };
    let settled = system.propose(&choices).map_err(issue)?;
    for line in settled.view.left_out() {
        println!("installer:   {line}");
    }
    println!("\nSlopOS will be installed on {}:", settled.disk());
    for line in settled.view.review() {
        println!("  {line}");
    }
    if !opts.yes {
        confirm(&settled)?;
    }
    Ok((choices, settled))
}

fn confirm(p: &Settled) -> Result<(), String> {
    let (want, question) = if p.view.erase {
        let name = short(p.disk()).to_owned();
        let question = format!("Type {name} to erase it");
        (name, question)
    } else {
        ("yes".to_owned(), "Type yes to write the disk".to_owned())
    };
    let answer = ask(&question, None, "--yes")?;
    if answer == want {
        Ok(())
    } else {
        Err(NOTHING_WRITTEN.to_owned())
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
            Event::Step(step) => println!("installer: {step}"),
            Event::Log { line } => println!("installer:   {line}"),
            Event::Progress { done, total } => {
                if done < self.reported {
                    self.reported = 0;
                }
                if done - self.reported >= PROGRESS_EVERY {
                    self.reported = done;
                    println!(
                        "installer:   {} of {} copied",
                        disk_size(done),
                        disk_size(total)
                    );
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
            "installer: {node} holds the installer's developer tools, which weren't read; the system log says why"
        );
    }
    let (choices, proposal) = settle(opts, &mut system)?;
    let summary = system
        .install(&choices, &proposal, &mut Lines::default())
        .map_err(|failure| failure.to_string())?;
    println!("installer: {summary} Remove the USB stick or DVD, then restart.");
    Ok(())
}
