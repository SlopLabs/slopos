use alloc::format;
use alloc::string::String;
use core::fmt;

use slopos_boot_core::install::PlanError;

use crate::wizard::Kind;
use crate::{NoTable, capitalised, disk_size, role_words, short};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Partition,
    Reread,
    Format,
    CopySystem,
    CopyStartup,
    Menu,
    Default,
    Check,
    Register,
}

impl Step {
    pub const ALL: [Step; 9] = [
        Step::Partition,
        Step::Reread,
        Step::Format,
        Step::CopySystem,
        Step::CopyStartup,
        Step::Menu,
        Step::Default,
        Step::Check,
        Step::Register,
    ];

    /// Its place in [`Step::ALL`], from 1.
    pub fn number(self) -> u32 {
        self as u32 + 1
    }

    pub fn numbered(number: u32) -> Option<Step> {
        let index = usize::try_from(number).ok()?.checked_sub(1)?;
        Step::ALL.get(index).copied()
    }

    /// What it does, as it completes "The installer stopped while …".
    pub fn doing(self) -> &'static str {
        match self {
            Step::Partition => "partitioning the disk",
            Step::Reread => "reading the new partitions",
            Step::Format => "formatting the new partitions",
            Step::CopySystem => "copying the system",
            Step::CopyStartup => "copying the startup files",
            Step::Menu => "setting up the startup menu",
            Step::Default => "choosing what starts by default",
            Step::Check => "checking the installed system",
            Step::Register => "adding SlopOS to the startup order",
        }
    }
}

/// As the CLI and the install's log count it: `[7/9] Choosing what starts
/// by default`.
impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}/{}] {}",
            self.number(),
            Step::ALL.len(),
            capitalised(self.doing())
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Step(Step),
    /// A copy has put `done` of its `total` bytes on the root.
    Progress {
        done: u64,
        total: u64,
    },
    /// What a step did, or a line a program it ran printed.
    Log {
        line: String,
    },
}

/// Why choices cannot be installed. [`Issue::words`] is what the window
/// says; `Display` is what the CLI says, the flag that helps among it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Issue {
    Medium(String),
    Probe(String),
    Disk(String),
    OnlyErase {
        node: String,
        contents: NoTable,
    },
    Plan {
        error: PlanError,
        node: String,
        /// The SlopOS root the disk holds.
        root: Option<String>,
    },
    NothingToKeep(String),
    /// A disk with no partition the root could go on.
    NoRoot(String),
    Esp {
        node: String,
        trouble: EspTrouble,
    },
    Person(String),
    /// The firmware's entries, which the new startup menu offers, could not
    /// be read, or the menu not rendered.
    Menu(String),
    /// A size asked for a reused root, which keeps its partition's.
    RootSize,
    /// A remote holding what a line of git's configuration cannot.
    Remote(String),
    /// Asked of the engine before it read the disks.
    Unprobed,
    /// A proposal asked for a disk the engine has not opened.
    Unopened(String),
    /// An install of what the engine did not propose last.
    Unproposed,
    /// The disk, read again to install, no longer proposes what was reviewed.
    Changed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EspTrouble {
    Full {
        free: u64,
        needed: u64,
    },
    WrongType,
    /// What reading it said.
    Unreadable(String),
}

impl Issue {
    fn fact(&self) -> String {
        match self {
            Issue::Medium(_) => {
                "SlopOS can only be installed from its own USB stick or DVD.".into()
            }
            Issue::Probe(_) => "The disks on this computer couldn't be read.".into(),
            Issue::Disk(_) => "This disk can't be used.".into(),
            Issue::OnlyErase { contents, .. } => contents.only_erase().into(),
            Issue::NothingToKeep(_) => "There's no SlopOS on this partition to keep.".into(),
            Issue::NoRoot(_) => "This disk has no partition SlopOS can go on.".into(),
            Issue::Esp { .. } => "The EFI system partition on this disk can't be used.".into(),
            Issue::Person(_) => "The first account couldn't be set up.".into(),
            Issue::Menu(_) => "The startup menu couldn't be set up.".into(),
            Issue::RootSize => "A partition that's used again keeps its size.".into(),
            Issue::Remote(_) => {
                "The address of the source code can't contain control characters.".into()
            }
            Issue::Unprobed => "The installer hasn't read the disks yet.".into(),
            Issue::Unopened(_) => "The installer hasn't read this disk yet.".into(),
            Issue::Unproposed => "The installer was asked for an install it didn't propose.".into(),
            Issue::Changed(_) => "This disk changed after it was reviewed.".into(),
            Issue::Plan { error, .. } => match *error {
                PlanError::NoRegion => "That free space isn't on this disk.".into(),
                PlanError::NoRoom { needed, .. } | PlanError::RootTooSmall { needed, .. } => {
                    format!("Too small. SlopOS needs at least {}.", disk_size(needed))
                }
                PlanError::NoPartition(n) => format!("This disk has no partition {n}."),
                PlanError::NotForRoot(_) => {
                    "This partition is in use for starting up or for crash records, so SlopOS can't go on it.".into()
                }
                PlanError::Installed(_) | PlanError::RootElsewhere(_) => {
                    "SlopOS is already on this disk.".into()
                }
                PlanError::Ambiguous(role) => format!(
                    "This disk has two {} partitions, so SlopOS can't tell which to use.",
                    role_words(role)
                ),
                PlanError::RootTooLarge { limit, .. } => {
                    format!("Too large. SlopOS can use at most {}.", disk_size(limit))
                }
                PlanError::NoSlot => {
                    "This disk's partition table has no room for another partition.".into()
                }
            },
        }
    }

    fn said(&self) -> Option<&str> {
        match self {
            Issue::Medium(why)
            | Issue::Probe(why)
            | Issue::Disk(why)
            | Issue::Person(why)
            | Issue::Menu(why)
            | Issue::Esp {
                trouble: EspTrouble::Unreadable(why),
                ..
            } => Some(why),
            _ => None,
        }
    }

    fn measured(&self) -> Option<String> {
        match self {
            Issue::Esp {
                node,
                trouble: EspTrouble::Full { free, needed },
            } => Some(format!(
                "{} has {} free; SlopOS needs {}.",
                short(node),
                disk_size(*free),
                disk_size(*needed)
            )),
            Issue::Esp {
                node,
                trouble: EspTrouble::WrongType,
            } => Some(format!("{} isn't of the EFI system type.", short(node))),
            Issue::Plan { error, node, .. } => match *error {
                PlanError::NoRoom { available, .. } => Some(format!(
                    "The free space on {} holds {}.",
                    short(node),
                    disk_size(available)
                )),
                PlanError::RootTooSmall { available, .. }
                | PlanError::RootTooLarge { available, .. } => {
                    Some(format!("The partition holds {}.", disk_size(available)))
                }
                PlanError::NotForRoot(n) | PlanError::Installed(n) => {
                    Some(format!("Partition {n} of {}.", short(node)))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// What the window says: what is wrong, then what to do there. An issue
    /// only the CLI's choices raise has nothing to do in the window.
    pub fn words(&self) -> String {
        match self.advice() {
            Some(next) => format!("{} {next}", self.fact()),
            None => self.fact(),
        }
    }

    fn advice(&self) -> Option<String> {
        let instead = |kind: Kind| format!("Choose {} instead.", kind.title());
        Some(match self {
            Issue::NothingToKeep(_) | Issue::Person(_) | Issue::RootSize => return None,
            Issue::Medium(_) => "Start this computer from it, then try again.".into(),
            Issue::Probe(_)
            | Issue::Menu(_)
            | Issue::Unprobed
            | Issue::Unopened(_)
            | Issue::Unproposed
            | Issue::Changed(_) => "Try again.".into(),
            Issue::Disk(_) => "Choose another disk.".into(),
            Issue::OnlyErase { .. } => instead(Kind::Erase),
            Issue::NoRoot(_) => "Choose another way to install.".into(),
            Issue::Esp { .. } => {
                format!("Choose {} instead, or another disk.", Kind::Erase.title())
            }
            Issue::Remote(_) => "Remove them from the address.".into(),
            Issue::Plan { error, root, .. } => match error {
                PlanError::NoRegion => "Choose free space on this disk.".into(),
                PlanError::NoRoom { .. } => {
                    "Choose a disk or free space at least that large.".into()
                }
                PlanError::NoSlot => instead(Kind::Erase),
                PlanError::RootTooSmall { .. } => "Choose a larger partition.".into(),
                PlanError::RootTooLarge { .. } => "Choose a smaller partition.".into(),
                PlanError::NoPartition(_) | PlanError::NotForRoot(_) => {
                    "Choose another partition.".into()
                }
                PlanError::Installed(_) if root.is_none() => instead(Kind::Erase),
                PlanError::Installed(_) | PlanError::RootElsewhere(_) => instead(Kind::Reinstall),
                PlanError::Ambiguous(_) => instead(Kind::Erase),
            },
        })
    }

    /// What the details say: what the engine or a program said, or the size,
    /// node or partition the words turn on.
    pub fn detail(&self) -> Option<String> {
        if let Some(said) = self.said() {
            return Some(said.into());
        }
        match self {
            Issue::OnlyErase { node, contents } => contents
                .holds()
                .map(|what| format!("{} holds {what}.", short(node))),
            _ => self.measured(),
        }
    }

    fn cli_next(&self) -> String {
        match self {
            Issue::Medium(_) => "Start this computer from it, then run the installer again.".into(),
            Issue::Probe(_)
            | Issue::Menu(_)
            | Issue::Unprobed
            | Issue::Unopened(_)
            | Issue::Unproposed
            | Issue::Changed(_) => "Run the installer again.".into(),
            Issue::Disk(_) => "Pass another --disk.".into(),
            Issue::OnlyErase { .. } => "Pass --mode erase to erase it.".into(),
            Issue::NothingToKeep(_) => "Pass --format-root to format it.".into(),
            Issue::NoRoot(_) => "Pass --mode free or --mode erase.".into(),
            Issue::Esp { .. } => "Pass --mode erase to erase the disk, or another --disk.".into(),
            Issue::Person(_) => "Pass another --person.".into(),
            Issue::RootSize => "Leave out --root-size.".into(),
            Issue::Remote(_) => "Pass an address without them.".into(),
            Issue::Plan { error, root, .. } => match (error, root) {
                (PlanError::Installed(_) | PlanError::RootElsewhere(_), Some(root)) => {
                    format!("Reinstall it with --mode reuse --root {root}.")
                }
                (PlanError::RootElsewhere(n), None) => {
                    format!("Reinstall it with --mode reuse --root {n}.")
                }
                (PlanError::NoRegion, _) => "Pass a --region the disk lists.".into(),
                (PlanError::NoRoom { .. }, _) => "Pass a larger --region, or another disk.".into(),
                (PlanError::NoPartition(_), _) => "Pass a --root the disk lists.".into(),
                (PlanError::NotForRoot(_), _) => "Pass another --root.".into(),
                (PlanError::RootTooSmall { .. }, _) => "Pass a larger partition as --root.".into(),
                (PlanError::RootTooLarge { .. }, _) => "Pass a smaller partition as --root.".into(),
                (PlanError::Installed(_) | PlanError::Ambiguous(_) | PlanError::NoSlot, _) => {
                    "Pass --mode erase to erase it.".into()
                }
            },
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Issue::OnlyErase { node, .. }
            | Issue::Plan { node, .. }
            | Issue::NothingToKeep(node)
            | Issue::NoRoot(node)
            | Issue::Esp { node, .. }
            | Issue::Unopened(node)
            | Issue::Changed(node) => write!(f, "{node}: ")?,
            Issue::Remote(url) => write!(f, "--remote {url:?}: ")?,
            _ => {}
        }
        let fact = self.fact();
        match self.said() {
            Some(said) => write!(
                f,
                "{} ({}).",
                fact.trim_end_matches('.'),
                said.trim_end_matches('.')
            )?,
            None => f.write_str(&fact)?,
        }
        if let Some(measured) = self.measured() {
            write!(f, " {measured}")?;
        }
        write!(f, " {}", self.cli_next())
    }
}

pub const NOTHING_WRITTEN: &str = "Nothing was written.";

/// How an install ended short. Each driver words a refusal's [`Issue`] its
/// own way; `Display` is the CLI's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Refused before anything was written.
    Refused(Issue),
    /// What the engine or a program it ran said, `step` the last begun.
    Stopped { step: Option<Step>, why: String },
}

impl Failure {
    pub fn step(&self) -> Option<Step> {
        match self {
            Failure::Refused(_) => None,
            Failure::Stopped { step, .. } => *step,
        }
    }

    pub fn words(&self) -> String {
        match self.step() {
            Some(step) => format!("The installer stopped while {}.", step.doing()),
            None => "The installer stopped before writing anything.".into(),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Refused(issue) => write!(f, "{issue} {NOTHING_WRITTEN}"),
            Failure::Stopped { why, .. } => f.write_str(why),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub disk: String,
    /// The firmware entry that boots it: `Boot<entry>`.
    pub entry: u16,
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SlopOS is installed on {}, and starts as Boot{:04X}.",
            self.disk, self.entry
        )
    }
}
