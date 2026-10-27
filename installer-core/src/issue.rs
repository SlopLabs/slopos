use alloc::format;
use alloc::string::String;
use core::fmt;

use slopos_boot_core::install::{PlanError, Role};

use crate::wizard::Kind;
use crate::{Contents, disk_size};

/// The steps of an install, in order, each as it completes "The installer
/// stopped while …": [`Event::Step`] names the one under way by its place
/// here, from 1.
pub const STEPS: [&str; 9] = [
    "partitioning the disk",
    "reading the new partitions",
    "formatting the new partitions",
    "copying the system",
    "copying the startup files",
    "setting up the startup menu",
    "choosing what starts by default",
    "checking the installed system",
    "adding SlopOS to the startup order",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Step {
        index: u32,
        total: u32,
        title: String,
    },
    /// A copy has put `done` of its `total` bytes on the root.
    Progress { done: u64, total: u64 },
    /// What a step did, or a line a program it ran printed.
    Log { line: String },
}

impl Event {
    /// Step `index` of [`STEPS`], from 1.
    pub fn step(index: u32) -> Event {
        Event::Step {
            index,
            total: STEPS.len() as u32,
            title: STEPS[index as usize - 1].into(),
        }
    }
}

/// Why choices cannot be installed. [`Issue::words`] is what the window
/// says; `Display` is what the CLI says, the flag that helps among it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Issue {
    Medium(String),
    /// What could not be read of the computer.
    Probe(String),
    Disk(String),
    /// A disk with no table the installer reads, which only an erase takes.
    OnlyErase {
        node: String,
        contents: Contents,
    },
    Plan {
        error: PlanError,
        node: String,
        /// The node of the SlopOS root the disk holds, if any.
        root: Option<String>,
    },
    NothingToKeep(String),
    /// A disk with no partition the root could go on.
    NoRoot(String),
    Esp(String),
    Person(String),
    /// The firmware's entries, which the new startup menu offers, could not
    /// be read, or the menu not rendered.
    Menu(String),
    /// A size asked for a reused root, which keeps its partition's.
    RootSize,
    /// A remote holding what a line of git's configuration cannot.
    Remote(String),
}

/// What a partition of `role` is to the person who reads about it.
pub fn role_words(role: Role) -> &'static str {
    match role {
        Role::Esp => "EFI system",
        Role::Boot => "SlopOS startup",
        Role::Root => "SlopOS",
        Role::Crash => "crash record",
    }
}

impl Issue {
    /// What is wrong, as a sentence.
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
            Issue::Esp(_) => "The EFI system partition on this disk can't be used.".into(),
            Issue::Person(_) => "The first account couldn't be set up.".into(),
            Issue::Menu(_) => "The startup menu couldn't be set up.".into(),
            Issue::RootSize => "A partition that's used again keeps its size.".into(),
            Issue::Remote(_) => {
                "The address of the source code can't contain control characters.".into()
            }
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

    /// What the window says: what is wrong, then what to do.
    pub fn words(&self) -> String {
        let instead = |kind: Kind| format!("Choose {} instead.", kind.title());
        let next = match self {
            Issue::Medium(_) => "Start this computer from it, then try again.".into(),
            Issue::Probe(_) | Issue::Menu(_) => "Try again.".into(),
            Issue::Disk(_) => "Choose another disk.".into(),
            Issue::OnlyErase { .. } => instead(Kind::Erase),
            Issue::NothingToKeep(_) => "Format it instead.".into(),
            Issue::NoRoot(_) => "Choose another way to install.".into(),
            Issue::Esp(_) => format!("Choose {} instead, or another disk.", Kind::Erase.title()),
            Issue::Person(_) => "Choose another name.".into(),
            Issue::RootSize => "Leave its size as it is.".into(),
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
        };
        format!("{} {next}", self.fact())
    }

    /// What the engine said of why, for the details: a size, a node or the
    /// error a disk or a program gave.
    pub fn detail(&self) -> Option<String> {
        match self {
            Issue::Medium(why)
            | Issue::Probe(why)
            | Issue::Disk(why)
            | Issue::Esp(why)
            | Issue::Person(why)
            | Issue::Menu(why) => Some(why.clone()),
            Issue::OnlyErase { node, contents } => {
                contents.holds().map(|what| format!("{node} holds {what}."))
            }
            Issue::Plan { error, node, .. } => match *error {
                PlanError::NoRoom { available, .. } => Some(format!(
                    "The free space on {node} holds {}.",
                    disk_size(available)
                )),
                PlanError::RootTooSmall { available, .. }
                | PlanError::RootTooLarge { available, .. } => {
                    Some(format!("The partition holds {}.", disk_size(available)))
                }
                PlanError::NotForRoot(n) | PlanError::Installed(n) => {
                    Some(format!("Partition {n} of {node}."))
                }
                _ => None,
            },
            _ => None,
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Issue::Medium(why)
            | Issue::Probe(why)
            | Issue::Disk(why)
            | Issue::Esp(why)
            | Issue::Person(why)
            | Issue::Menu(why) => f.write_str(why),
            Issue::OnlyErase { node, .. } => {
                write!(f, "{node}: {} Pass --mode erase to erase it.", self.fact())
            }
            Issue::NothingToKeep(node) => {
                write!(
                    f,
                    "{node}: {} Pass --format-root to format it.",
                    self.fact()
                )
            }
            Issue::NoRoot(node) => {
                write!(
                    f,
                    "{node}: {} Pass --mode free or --mode erase.",
                    self.fact()
                )
            }
            Issue::RootSize => write!(f, "{} Leave out --root-size.", self.fact()),
            Issue::Remote(url) => write!(
                f,
                "--remote {url:?}: {} Pass an address without them.",
                self.fact()
            ),
            Issue::Plan { error, node, root } => {
                write!(f, "{node}: {}", self.fact())?;
                if let Some(detail) = self.detail() {
                    write!(f, " {detail}")?;
                }
                match (error, root) {
                    (PlanError::Installed(_) | PlanError::RootElsewhere(_), Some(root)) => {
                        write!(f, " Reinstall it with --mode reuse --root {root}.")
                    }
                    (PlanError::RootElsewhere(n), None) => {
                        write!(f, " Reinstall it with --mode reuse --root {n}.")
                    }
                    (PlanError::NoRegion, _) => write!(f, " Pass a --region the disk lists."),
                    (PlanError::NoRoom { .. }, _) => {
                        write!(f, " Pass a larger --region, or another disk.")
                    }
                    (PlanError::NoPartition(_), _) => write!(f, " Pass a --root the disk lists."),
                    (PlanError::NotForRoot(_), _) => write!(f, " Pass another --root."),
                    (PlanError::RootTooSmall { .. }, _) => {
                        write!(f, " Pass a larger partition as --root.")
                    }
                    (PlanError::RootTooLarge { .. }, _) => {
                        write!(f, " Pass a smaller partition as --root.")
                    }
                    (PlanError::Installed(_) | PlanError::Ambiguous(_) | PlanError::NoSlot, _) => {
                        write!(f, " Pass --mode erase to erase it.")
                    }
                }
            }
        }
    }
}

/// Why an install stopped: at step `step`, or with nothing written before
/// the first when it is 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub step: u32,
    pub message: String,
}

impl Failure {
    /// Where it stopped, as the window says it.
    pub fn words(&self) -> String {
        match STEPS.get((self.step as usize).wrapping_sub(1)) {
            Some(step) => format!("The installer stopped while {step}."),
            None => "The installer stopped before writing anything.".into(),
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
