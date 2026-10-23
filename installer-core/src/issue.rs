use alloc::string::String;
use core::fmt;

use slopos_boot_core::install::PlanError;

use crate::human;

/// The steps of an install, in order: [`Event::Step`] names the one under
/// way by its place here, from 1.
pub const STEPS: [&str; 9] = [
    "writing the partition table",
    "re-reading it",
    "formatting the new partitions",
    "filling the root",
    "writing both kernel slots and the base",
    "writing the loader and its configuration",
    "making slot a the default",
    "checking the root",
    "registering the firmware entry",
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

/// Why choices cannot be installed, worded as the CLI says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Issue {
    Medium(String),
    /// What could not be read of the machine.
    Probe(String),
    Disk(String),
    /// A disk with no table the installer reads, which only an erase takes.
    OnlyErase {
        node: String,
        holds: String,
    },
    Plan {
        error: PlanError,
        node: String,
        /// The node of the SlopOS root the disk holds, if any.
        root: Option<String>,
    },
    NothingToKeep(String),
    Esp(String),
    Person(String),
    /// The firmware's entries, which the new boot menu offers, could not be
    /// read, or the menu not rendered.
    Menu(String),
    /// A size asked for a reused root, which keeps its partition's.
    RootSize,
    /// A remote holding what a line of git's configuration cannot.
    Remote(String),
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
            Issue::OnlyErase { node, holds } => {
                write!(
                    f,
                    "{node} holds {holds}; only --mode erase installs onto it"
                )
            }
            Issue::NothingToKeep(node) => {
                write!(f, "--keep-root: {node} holds no SlopOS root to keep")
            }
            Issue::RootSize => f.write_str(
                "--root-size sizes a new root; a reused root keeps its partition's size",
            ),
            Issue::Remote(url) => write!(f, "--remote {url:?}: a URL holds no control character"),
            Issue::Plan { error, node, root } => match *error {
                PlanError::NoRegion => write!(f, "{node} has no free region of that number"),
                PlanError::NoRoom { needed, available } => write!(
                    f,
                    "the free region holds {}; SlopOS needs {}",
                    human(available),
                    human(needed)
                ),
                PlanError::NoPartition(n) => write!(f, "{node} has no partition {n}"),
                PlanError::NotForRoot(n) => write!(
                    f,
                    "partition {n} is the ESP or SlopOS's boot or crash partition, not a root"
                ),
                PlanError::Installed(n) => {
                    write!(
                        f,
                        "SlopOS is on {node} already (partition {n}); install over it with --mode reuse"
                    )?;
                    match root {
                        Some(root) => write!(f, " --root {root}"),
                        None => Ok(()),
                    }
                }
                PlanError::Ambiguous(role) => {
                    write!(f, "{node} has two {} partitions", role.name())
                }
                PlanError::RootTooSmall { needed, available } => write!(
                    f,
                    "the root partition holds {}; the root needs {}",
                    human(available),
                    human(needed)
                ),
                PlanError::RootElsewhere(n) => match root {
                    Some(root) => write!(
                        f,
                        "SlopOS's root is partition {n} of {node}; reinstall over it with --root {root}"
                    ),
                    None => write!(
                        f,
                        "SlopOS's root is partition {n} of {node}; reinstall over it with --root {n}"
                    ),
                },
                PlanError::RootTooLarge { limit, available } => write!(
                    f,
                    "the root partition holds {}; a root holds at most {}",
                    human(available),
                    human(limit)
                ),
                PlanError::NoSlot => {
                    write!(f, "{node}'s table has no unused entry for a new partition")
                }
            },
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
            "SlopOS is installed on {}, booted by the firmware entry Boot{:04X}",
            self.disk, self.entry
        )
    }
}
