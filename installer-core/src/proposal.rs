use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use slopos_boot_core::gpt::{Header, Partition};
use slopos_boot_core::install::{Place, Plan, ROLES, Role};
use slopos_boot_core::layout::{self, ALIGN_BYTES, ROOT_TYPE};

use crate::{Contents, Layout, human, partition_node, short, type_name};

/// An install as it would be done: the disk before and after, what happens
/// to each partition, and what the install sets up beside them. The same
/// choices on the same opened disk give the same proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub disk: String,
    /// What the disk held, which an erase loses.
    pub contents: Contents,
    pub before: Layout,
    pub after: Layout,
    pub erase: bool,
    /// The whole disk's erase first, then SlopOS's partitions in the order a
    /// new run lays them out, then the other partitions, kept.
    pub actions: Vec<Action>,
    pub root: Span,
    pub setup: Setup,
}

/// The sizes a root may take; a reused root's are its partition's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub min: u64,
    pub max: u64,
    pub chosen: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    Erase,
    New,
    Format,
    /// Another system's ESP, which SlopOS writes only `\EFI\SlopOS\` of.
    Share,
    Keep,
    /// Another system's partition, formatted as SlopOS's root.
    Reuse,
}

impl Tag {
    /// What the partition comes to, as the review's list tags it.
    pub fn word(self) -> &'static str {
        match self {
            Tag::Erase => "Erased",
            Tag::New => "New",
            Tag::Format => "Formatted",
            Tag::Share => "Shared",
            Tag::Keep => "Kept",
            Tag::Reuse => "Reused",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub tag: Tag,
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Disk,
    Role(Role, Part),
    Other(Part),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    New {
        number: u32,
        /// The node it will have.
        node: String,
        start: u64,
        bytes: u64,
    },
    Existing {
        number: u32,
        node: String,
        /// Its type as it was read.
        kind: String,
        bytes: u64,
    },
}

/// What an install sets up beside the partitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setup {
    /// Whether `/usr/local` gets the medium's toolchain.
    pub toolchain: bool,
    /// Where `/src/slopos` fetches from, when the medium carries a clone.
    pub remote: Option<String>,
    /// A new root's first person; `None` keeps the root's own people.
    pub person: Option<FirstPerson>,
    pub cmdline: String,
    pub first: bool,
    /// The other systems the boot menu offers.
    pub menu: Vec<String>,
    /// Those of them whose loader is on the disk, which an install beside
    /// them keeps.
    pub beside: Vec<String>,
    /// The other systems left out of it, each colliding with an entry there.
    pub left_out: Vec<String>,
    /// The other systems whose loader is on a partition the install erases
    /// or takes as the root.
    pub removed: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirstPerson {
    pub name: String,
    pub password: bool,
}

#[derive(Clone, Copy)]
pub struct Table<'a> {
    pub header: &'a Header,
    pub array: &'a [u8],
}

impl Table<'_> {
    fn partitions(&self) -> impl Iterator<Item = Partition> + '_ {
        self.header.partitions(self.array).filter_map(Result::ok)
    }
}

/// A plan, the tables it goes between and what it does to the partitions it
/// keeps, which a proposal is built from.
pub struct Planned<'a> {
    pub disk: &'a str,
    pub contents: Contents,
    pub capacity: u64,
    /// The disk's table as it was read: the one a plan that keeps it was made
    /// on.
    pub before: Option<Table<'a>>,
    pub erase: bool,
    pub plan: &'a Plan,
    /// The table as it will be written.
    pub after: Table<'a>,
    pub format_esp: bool,
    pub format_boot: bool,
    pub keep_root: bool,
    pub own_esp: bool,
    pub root_min: u64,
}

impl Proposal {
    pub fn of(planned: Planned<'_>, setup: Setup) -> Proposal {
        let disk = planned.disk;
        let kept = planned.before.filter(|_| !planned.erase);
        let existing = |number: u32| {
            let found = kept
                .as_ref()
                .and_then(|table| table.partitions().find(|p| p.entry.number == number));
            Part::Existing {
                number,
                node: partition_node(disk, number),
                kind: found.map_or_else(String::new, |p| type_name(&p.entry.type_guid)),
                bytes: found.map_or(0, |p| p.len),
            }
        };
        let block = planned.after.header.geometry().block();
        let mut actions = Vec::new();
        if planned.erase {
            actions.push(Action {
                tag: Tag::Erase,
                target: Target::Disk,
            });
        }
        for role in ROLES {
            let (tag, part) = match planned.plan.place(role) {
                Place::New {
                    number,
                    first_lba,
                    last_lba,
                } => (
                    Tag::New,
                    Part::New {
                        number,
                        node: partition_node(disk, number),
                        start: first_lba * block,
                        bytes: (last_lba - first_lba + 1) * block,
                    },
                ),
                Place::Existing(number) => {
                    let was_root = kept.as_ref().is_some_and(|table| {
                        table
                            .partitions()
                            .any(|p| p.entry.number == number && p.entry.type_guid == ROOT_TYPE)
                    });
                    let tag = match role {
                        Role::Esp if planned.format_esp => Tag::Format,
                        Role::Esp if planned.own_esp => Tag::Keep,
                        Role::Esp => Tag::Share,
                        Role::Boot if planned.format_boot => Tag::Format,
                        Role::Root if !planned.keep_root && !was_root => Tag::Reuse,
                        Role::Root | Role::Crash if !planned.keep_root => Tag::Format,
                        _ => Tag::Keep,
                    };
                    (tag, existing(number))
                }
            };
            actions.push(Action {
                tag,
                target: Target::Role(role, part),
            });
        }
        if let Some(table) = kept {
            let named: Vec<u32> = ROLES
                .iter()
                .map(|&role| planned.plan.place(role).number())
                .collect();
            for p in table.partitions() {
                if !named.contains(&p.entry.number) {
                    actions.push(Action {
                        tag: Tag::Keep,
                        target: Target::Other(existing(p.entry.number)),
                    });
                }
            }
        }
        let root = match actions.iter().find_map(|action| match &action.target {
            Target::Role(Role::Root, part) => Some(part),
            _ => None,
        }) {
            Some(Part::New { bytes, .. }) => Span {
                min: planned.root_min.div_ceil(ALIGN_BYTES) * ALIGN_BYTES,
                max: planned.plan.root_max,
                chosen: *bytes,
            },
            Some(Part::Existing { bytes, .. }) => Span {
                min: *bytes,
                max: *bytes,
                chosen: *bytes,
            },
            None => Span {
                min: 0,
                max: 0,
                chosen: 0,
            },
        };
        Proposal {
            disk: disk.into(),
            contents: planned.contents,
            before: planned.before.map_or_else(
                || Layout::empty(planned.capacity),
                |table| Layout::of(disk, table.header, table.array),
            ),
            after: Layout::of(disk, planned.after.header, planned.after.array),
            erase: planned.erase,
            actions,
            root,
            setup,
        }
    }

    pub fn role(&self, role: Role) -> Option<(Tag, &Part)> {
        self.actions.iter().find_map(|action| match &action.target {
            Target::Role(r, part) if *r == role => Some((action.tag, part)),
            _ => None,
        })
    }

    /// Whether the root's people and files stay.
    pub fn keeps_root(&self) -> bool {
        matches!(self.role(Role::Root), Some((Tag::Keep, _)))
    }

    /// Each action's partition as the review's list names it.
    pub fn action_lines(&self) -> Vec<(Tag, String)> {
        let part = |part: &Part| match part {
            Part::New { node, bytes, .. } => format!("{}, {}", short(node), human(*bytes)),
            Part::Existing {
                node, kind, bytes, ..
            } if kind.is_empty() => format!("{}, {}", short(node), human(*bytes)),
            Part::Existing {
                node, kind, bytes, ..
            } => format!("{}, {kind}, {}", short(node), human(*bytes)),
        };
        self.actions
            .iter()
            .map(|action| {
                let said = match &action.target {
                    Target::Disk => match self.contents.holds() {
                        Some(what) => format!("{}, which holds {what}", short(&self.disk)),
                        None => format!("{}, which is blank", short(&self.disk)),
                    },
                    Target::Role(role, p) => format!("{}: {}", role.name(), part(p)),
                    Target::Other(p) => part(p),
                };
                (action.tag, said)
            })
            .collect()
    }

    /// The other systems the boot menu leaves out, a line each.
    pub fn left_out(&self) -> impl Iterator<Item = String> + '_ {
        let colliding = self.setup.left_out.iter().map(|title| {
            format!("leaving \"{title}\" out of the menu: an entry there collides with it")
        });
        let removed = self.setup.removed.iter().map(|title| {
            format!("leaving \"{title}\" out of the menu: the install removes its loader")
        });
        colliding.chain(removed)
    }

    /// What the install does, a line each, as the CLI and the install's log
    /// say it.
    pub fn review(&self) -> Vec<String> {
        let mut lines = vec![if self.erase {
            format!("a new partition table: EVERYTHING on {} is lost", self.disk)
        } else {
            "new entries in its partition table; every other partition stays as it is".into()
        }];
        for action in &self.actions {
            if let Target::Role(role, part) = &action.target {
                lines.push(format!(
                    "{}: {}",
                    role.name(),
                    describe(*role, part, action.tag)
                ));
            }
        }
        let setup = &self.setup;
        if setup.toolchain {
            lines.push("/usr/local: the toolchain the medium carries".into());
        }
        match &setup.remote {
            Some(url) if self.keeps_root() => lines.push(format!(
                "/src/slopos: the root's own if it has one, else the medium's clone, fetching from {url}"
            )),
            Some(url) => lines.push(format!(
                "/src/slopos: the medium's clone, fetching from {url}"
            )),
            None => {}
        }
        lines.push(match &setup.person {
            Some(person) => format!(
                "{}: its first administrator, logged in by the desktop, {}",
                person.name,
                if person.password {
                    "with the password typed"
                } else {
                    "with no password"
                }
            ),
            None => "people: the root's own".into(),
        });
        lines.push(format!("boots with: {}", setup.cmdline));
        lines.push(format!(
            "firmware entry \"{}\": {}",
            layout::FIRMWARE_ENTRY,
            if setup.first {
                "first in the boot order"
            } else {
                "where it stands in the boot order, or after the systems there"
            }
        ));
        lines
    }
}

fn describe(role: Role, part: &Part, tag: Tag) -> String {
    match part {
        Part::New { start, bytes, .. } => format!("new, {} at {}", human(*bytes), human(*start)),
        Part::Existing {
            number,
            kind,
            bytes,
            ..
        } => {
            let what = if kind.is_empty() {
                format!("partition {number}")
            } else {
                format!("partition {number} ({kind}, {})", human(*bytes))
            };
            match (role, tag) {
                (Role::Esp, Tag::Format) => format!("{what}, FORMATTED: it holds no volume yet"),
                (Role::Esp, Tag::Keep) => format!("{what}, made by an earlier install"),
                (_, Tag::Share) => format!("{what}, shared: SlopOS writes \\EFI\\SlopOS\\ only"),
                (_, Tag::Format | Tag::Reuse) => format!("{what}, FORMATTED"),
                _ => format!("{what}, kept"),
            }
        }
    }
}
