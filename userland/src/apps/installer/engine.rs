//! What the installer finds, proposes and does, apart from how anyone is
//! asked. [`System::probe`] reads the medium and every disk,
//! [`System::propose`] lays a set of [`Choices`] out on what it read, and
//! [`System::install`] reads the chosen disk again and writes nothing unless
//! it still proposes what was reviewed. An install reports each step as an
//! [`Event`], and keeps its steps and log lines as the new root's
//! `/var/log/installer.log`. What it reads and proposes reaches a driver in
//! another process as `slopos_installer_core`'s views.

use std::time::Instant;

use slopos_boot_core::gpt::{Header, Partition};
use slopos_boot_core::install::{self, Mode as PlanMode, Place, Plan, ROLES, Role, RootSize};
use slopos_boot_core::layout::{
    self, BOOT_LABEL, BOOT_TYPE, CRASH_TYPE, ESP_LABEL, ESP_TYPE, ROOT_TYPE,
};
use slopos_boot_core::{Guid, bli};
use slopos_installer_core::{
    self as view, Choices, DEFAULT_CMDLINE, DEFAULT_REMOTE, Event, FIRST_PERSON, Failure,
    FirstPerson, Issue, Mode, Opened, Person, Planned, Proposal, STEPS, Setup, Summary,
    Table as TableView, human,
};

use super::boot::{self, Fallback};
use super::disk::{self, Disk};
use super::medium::Medium;
use super::root;
use crate::boot_disk::{
    ListedOption, boot_order, listed_options, not_installable, open_fat, partition_node,
    whole_disks,
};

const CHECKING_THE_ROOT: u32 = 8;
/// Room an ESP needs beside the loader for its directories and configuration.
const ESP_SLACK_BYTES: u64 = 1 << 20;

pub trait Events {
    fn event(&mut self, event: Event);

    fn log(&mut self, line: String) {
        self.event(Event::Log { line });
    }

    fn progress(&mut self, done: u64, total: u64) {
        self.event(Event::Progress { done, total });
    }
}

/// What a partition of the ESP's or SlopOS's types holds.
enum Holds {
    /// The ESP's FAT32 volume: its free bytes and its removable-media path.
    Esp {
        free: u64,
        fallback: Result<Fallback, String>,
    },
    /// An ESP with no volume, as an install that made it, or a SlopOS format
    /// cut short, leaves it.
    EmptyEsp,
    /// An ESP holding what this installer cannot use, and why.
    UnusableEsp(String),
    Boot {
        fat: bool,
    },
    Root {
        ext4: bool,
    },
}

struct Part {
    entry: Partition,
    node: Result<String, String>,
    holds: Option<Holds>,
}

impl Part {
    fn of(disk: &str, entry: Partition, loader: &[u8]) -> Part {
        let node = partition_node(disk, &entry);
        let holds = node
            .as_deref()
            .ok()
            .and_then(|node| match entry.entry.type_guid {
                ESP_TYPE => Some(match open_fat(node) {
                    Ok(mut volume) => Holds::Esp {
                        free: u64::from(volume.free_clusters()) * volume.cluster_bytes() as u64,
                        fallback: Fallback::read(&mut volume, node, loader),
                    },
                    Err(_) if disk::holds_no_volume(node) => Holds::EmptyEsp,
                    Err(why) => Holds::UnusableEsp(why),
                }),
                BOOT_TYPE => Some(Holds::Boot {
                    fat: open_fat(node).is_ok(),
                }),
                ROOT_TYPE => Some(Holds::Root {
                    ext4: disk::holds_ext4(node),
                }),
                _ => None,
            });
        Part { entry, node, holds }
    }
}

/// The disk chosen, its partitions read.
pub struct OpenDisk {
    pub disk: Disk,
    parts: Vec<Part>,
}

impl OpenDisk {
    fn of(disk: Disk, loader: &[u8]) -> OpenDisk {
        let parts = disk
            .partitions()
            .into_iter()
            .map(|entry| Part::of(&disk.node, entry, loader))
            .collect();
        OpenDisk { disk, parts }
    }

    fn part(&self, number: u32) -> Result<&Part, Issue> {
        self.parts
            .iter()
            .find(|part| part.entry.entry.number == number)
            .ok_or_else(|| Issue::Disk(format!("{} has no partition {number}", self.disk.node)))
    }

    pub fn node(&self, number: u32) -> Result<String, Issue> {
        self.part(number)?.node.clone().map_err(Issue::Disk)
    }

    /// Whether partition `number` holds a SlopOS root, which an install may
    /// keep.
    pub fn holds_root(&self, number: u32) -> bool {
        self.part(number)
            .is_ok_and(|part| matches!(part.holds, Some(Holds::Root { ext4: true })))
    }

    fn slopos_root(&self) -> Option<String> {
        self.parts
            .iter()
            .find(|part| part.entry.entry.type_guid == ROOT_TYPE)
            .and_then(|part| part.node.clone().ok())
    }
}

pub enum State {
    /// Never offered, and why.
    Refused(&'static str),
    /// Offered, but it could not be read.
    Unreadable(String),
    Listed(Disk),
    /// Chosen: its table read again by the kernel, and its partitions read.
    Opened(OpenDisk),
}

pub struct Found {
    pub node: String,
    pub state: State,
}

impl Found {
    fn probe(node: String, medium: Guid) -> Found {
        let state = match not_installable(&node, Some(medium)) {
            Some(why) => State::Refused(why),
            None => match Disk::open(&node) {
                Ok(disk) => State::Listed(disk),
                Err(why) => State::Unreadable(why),
            },
        };
        Found { node, state }
    }

    /// The disk, unless it is never offered or could not be read.
    pub fn disk(&self) -> Result<&Disk, Issue> {
        let node = &self.node;
        match &self.state {
            State::Refused(why) => Err(Issue::Disk(format!(
                "{node} is {why}; nothing is installed onto it"
            ))),
            State::Unreadable(why) => Err(Issue::Disk(why.clone())),
            State::Listed(disk) => Ok(disk),
            State::Opened(open) => Ok(&open.disk),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Guids {
    /// A fresh table's.
    disk: Guid,
    /// The new partitions', by role.
    partitions: [Guid; 4],
}

impl Guids {
    fn random() -> Result<Guids, String> {
        let mut partitions = [Guid::ZERO; 4];
        for guid in &mut partitions {
            *guid = disk::random_guid()?;
        }
        Ok(Guids {
            disk: disk::random_guid()?,
            partitions,
        })
    }
}

/// The existing partitions an install formats: those an install cut short
/// left without a volume.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Formats {
    esp: bool,
    boot: bool,
}

/// An install as it will be done, settled before anything is written.
#[derive(PartialEq)]
pub struct Settled {
    /// The disk as it was read, which an install reads it again to match.
    before: Disk,
    plan: Plan,
    /// The table as it will be written, the plan applied.
    header: Header,
    array: Vec<u8>,
    formats: Formats,
    /// Whether the ESP is SlopOS's own, whose removable-media path it writes.
    own_esp: bool,
    limine_conf: String,
    person: Option<Person>,
    guids: Guids,
    pub view: Proposal,
}

/// What the install medium and the machine hold, as the installer read them.
pub struct System {
    pub medium: Medium,
    pub disks: Vec<Found>,
    loader: Vec<u8>,
    /// The options the firmware's `BootOrder` lists, which the new boot menu
    /// offers.
    boot_options: Result<Vec<ListedOption>, String>,
    guids: Guids,
}

impl System {
    pub fn probe() -> Result<System, Issue> {
        let medium = Medium::find().map_err(Issue::Medium)?;
        let loader = medium.read(layout::MEDIUM_LOADER).map_err(Issue::Medium)?;
        let disks = whole_disks()
            .map_err(Issue::Probe)?
            .into_iter()
            .map(|node| Found::probe(node, medium.disk_guid))
            .collect();
        Ok(System {
            medium,
            disks,
            loader,
            boot_options: boot_order().and_then(|order| listed_options(&order)),
            guids: Guids::random().map_err(Issue::Probe)?,
        })
    }

    fn index(&self, node: &str) -> Result<usize, Issue> {
        self.disks
            .iter()
            .position(|found| found.node == node)
            .ok_or_else(|| {
                let disks: Vec<&str> = self.disks.iter().map(|d| d.node.as_str()).collect();
                Issue::Disk(format!(
                    "{node} is not a disk; SlopOS installs onto a whole disk, one of {}",
                    disks.join(", ")
                ))
            })
    }

    pub fn opened(&self, node: &str) -> Result<&OpenDisk, Issue> {
        let found = &self.disks[self.index(node)?];
        match &found.state {
            State::Opened(open) => Ok(open),
            _ => Err(found
                .disk()
                .err()
                .unwrap_or_else(|| Issue::Disk(format!("{node} has not been opened")))),
        }
    }

    /// The disk at `index` read afresh, its partitions with it, once the
    /// kernel has read its table again, which it refuses while anything on
    /// the disk is in use, as every write to the disk would be.
    fn reopen(&self, index: usize) -> Result<OpenDisk, Issue> {
        let node = &self.disks[index].node;
        self.disks[index].disk()?;
        disk::reread(node).map_err(Issue::Disk)?;
        let found = Found::probe(node.clone(), self.medium.disk_guid);
        Ok(OpenDisk::of(found.disk()?.clone(), &self.loader))
    }

    /// Choose the disk `node`, which a proposal is made for.
    pub fn open(&mut self, node: &str) -> Result<(), Issue> {
        let index = self.index(node)?;
        self.disks[index].state = State::Opened(self.reopen(index)?);
        Ok(())
    }

    pub fn propose(&self, choices: &Choices) -> Result<Settled, Issue> {
        self.propose_on(self.opened(&choices.disk)?, choices, self.guids)
    }

    fn propose_on(
        &self,
        open: &OpenDisk,
        choices: &Choices,
        guids: Guids,
    ) -> Result<Settled, Issue> {
        let disk = &open.disk;
        let node = &disk.node;
        let erase = choices.mode == Mode::Erase;
        let (header, mut array) = if erase {
            let header = Header::new(disk.geometry, guids.disk)
                .ok_or_else(|| Issue::Disk(format!("{node} is too small for a partition table")))?;
            (header, vec![0u8; header.array_bytes()])
        } else {
            let table = disk.table.as_ref().ok_or_else(|| Issue::OnlyErase {
                node: node.clone(),
                holds: disk.contents.holding(),
            })?;
            (table.header.grown(), table.array.clone())
        };
        let (plan_mode, keep_root) = match choices.mode {
            Mode::Erase => (PlanMode::Free { region: None }, false),
            Mode::Free { region } => (PlanMode::Free { region }, false),
            Mode::Reuse { .. } if choices.root_size.is_some() => return Err(Issue::RootSize),
            Mode::Reuse { root, region, keep } => (PlanMode::Reuse { root, region }, keep),
        };
        let size = RootSize {
            min: self.medium.root_min,
            target: choices.root_size,
        };
        let plan =
            install::plan(&header, &array, plan_mode, size).map_err(|error| Issue::Plan {
                error,
                node: node.clone(),
                root: open.slopos_root(),
            })?;
        if keep_root
            && let Place::Existing(n) = plan.root
            && !open.holds_root(n)
        {
            return Err(Issue::NothingToKeep(open.node(n)?));
        }
        let person = if keep_root {
            None
        } else {
            let person = choices.person.clone().unwrap_or_else(|| Person {
                name: FIRST_PERSON.to_owned(),
                password: None,
            });
            slopos_accounts_core::Accounts::first(&person.name, person.password.clone())
                .map_err(|why| Issue::Person(format!("{}: {why}", person.name)))?;
            Some(person)
        };
        let formats = self.formats(open, &plan)?;
        let header = plan.apply(header, &mut array, guids.partitions);
        let unique = |role: Role| {
            header
                .partitions(&array)
                .filter_map(Result::ok)
                .find(|p| p.entry.number == plan.place(role).number())
                .map(|p| p.entry.unique)
                .ok_or_else(|| Issue::Disk(format!("the planned table has no {}", role.name())))
        };
        let esp_unique = unique(Role::Esp)?;
        let boot_unique = unique(Role::Boot)?;
        let own_esp = match plan.esp {
            Place::New { .. } => true,
            Place::Existing(_) if formats.esp => true,
            Place::Existing(n) => match &open.part(n)?.holds {
                Some(Holds::Esp { fallback, .. }) => fallback
                    .as_ref()
                    .map_err(|why| Issue::Esp(why.clone()))?
                    .ours(&boot_unique),
                _ => false,
            },
        };
        let options = self
            .boot_options
            .as_ref()
            .map_err(|why| Issue::Menu(why.clone()))?;
        let cmdline = choices.cmdline.as_deref().unwrap_or(DEFAULT_CMDLINE);
        let after: Vec<Guid> = header
            .partitions(&array)
            .filter_map(Result::ok)
            .map(|p| p.entry.unique)
            .collect();
        let gone: Vec<Guid> = disk
            .partitions()
            .iter()
            .map(|p| p.entry.unique)
            .filter(|unique| !after.contains(unique))
            .collect();
        let menu = boot::menu(
            boot_unique,
            unique(Role::Root)?,
            cmdline,
            own_esp.then_some(&esp_unique),
            options,
            &gone,
        )
        .map_err(Issue::Menu)?;
        let remote = match &choices.remote {
            Some(url) if url.chars().any(char::is_control) => {
                return Err(Issue::Remote(url.clone()));
            }
            remote => self
                .medium
                .source
                .then(|| remote.clone().unwrap_or_else(|| DEFAULT_REMOTE.to_owned())),
        };
        let read = disk.table.as_ref().map(|t| (t.header.grown(), &t.array));
        let beside = menu
            .offered
            .iter()
            .filter(|other| after.contains(&other.partition))
            .map(|other| other.title.clone())
            .collect();
        let view = Proposal::of(
            Planned {
                disk: node,
                contents: disk.contents.clone(),
                capacity: disk.geometry.capacity(),
                before: read
                    .as_ref()
                    .map(|(header, array)| TableView { header, array }),
                erase,
                plan: &plan,
                after: TableView {
                    header: &header,
                    array: &array,
                },
                format_esp: formats.esp,
                format_boot: formats.boot,
                keep_root,
                own_esp,
                root_min: size.min,
            },
            Setup {
                toolchain: self.medium.toolchain,
                remote,
                person: person.as_ref().map(|p| FirstPerson {
                    name: p.name.clone(),
                    password: p.password.is_some(),
                }),
                cmdline: cmdline.to_owned(),
                first: choices.first,
                menu: menu.offered.into_iter().map(|other| other.title).collect(),
                beside,
                left_out: menu.left_out,
                removed: menu.removed,
            },
        );
        Ok(Settled {
            before: disk.clone(),
            plan,
            header,
            array,
            formats,
            own_esp,
            limine_conf: menu.conf,
            person,
            guids,
            view,
        })
    }

    /// What the engine read, as a driver in another process sees it.
    pub fn view(&self) -> view::System {
        let medium = &self.medium;
        let listing = |disk: &Disk, opened| view::Listing {
            capacity: disk.geometry.capacity(),
            block: disk.geometry.block(),
            contents: disk.contents.clone(),
            layout: disk.layout(),
            opened,
        };
        let disks = self
            .disks
            .iter()
            .map(|found| view::Disk {
                node: found.node.clone(),
                state: match &found.state {
                    State::Refused(why) => view::DiskState::Refused((*why).to_owned()),
                    State::Unreadable(why) => view::DiskState::Unreadable(why.clone()),
                    State::Listed(disk) => view::DiskState::Listed(listing(disk, None)),
                    State::Opened(open) => {
                        view::DiskState::Listed(listing(&open.disk, Some(self.offers(open))))
                    }
                },
            })
            .collect();
        view::System {
            medium: view::Medium {
                toolchain: medium.toolchain,
                source: medium.source,
                payload: medium.payload_bytes,
                root_min: medium.root_min,
                unmounted_payload: medium.unmounted_payload.clone(),
            },
            disks,
            others: self
                .boot_options
                .as_ref()
                .map(|options| {
                    boot::other_systems(options, None)
                        .into_iter()
                        .map(|other| other.title)
                        .collect()
                })
                .map_err(Clone::clone),
        }
    }

    /// What each choice on the opened disk proposes, with every other answer
    /// the engine's default.
    fn offers(&self, open: &OpenDisk) -> Opened {
        let node = &open.disk.node;
        let fit = |mode| {
            self.propose_on(open, &Choices::new(node, mode), self.guids)
                .map(drop)
        };
        let roots: Vec<(u32, view::Fit)> = open
            .parts
            .iter()
            .map(|part| part.entry.entry)
            .filter(|entry| ![ESP_TYPE, BOOT_TYPE, CRASH_TYPE].contains(&entry.type_guid))
            .map(|entry| {
                let mode = Mode::Reuse {
                    root: entry.number,
                    region: None,
                    keep: false,
                };
                (entry.number, fit(mode))
            })
            .collect();
        let reuse = match roots.iter().find(|(_, fit)| fit.is_ok()).or(roots.first()) {
            Some((_, fit)) => fit.clone(),
            None if open.disk.table.is_none() => Err(Issue::OnlyErase {
                node: node.clone(),
                holds: open.disk.contents.holding(),
            }),
            None => Err(Issue::Disk(format!(
                "{node} has no partition the root could go on"
            ))),
        };
        let reinstall = open
            .parts
            .iter()
            .find(|part| part.entry.entry.type_guid == ROOT_TYPE)
            .map(|part| {
                let root = part.entry.entry.number;
                let keep = open.holds_root(root);
                view::Reinstall {
                    root,
                    keep,
                    fit: fit(Mode::Reuse {
                        root,
                        region: None,
                        keep,
                    }),
                }
            });
        Opened {
            erase: fit(Mode::Erase),
            free: fit(Mode::Free { region: None }),
            largest: open.disk.largest_region(),
            regions: (0..open.disk.regions().len())
                .map(|region| {
                    fit(Mode::Free {
                        region: Some(region),
                    })
                })
                .collect(),
            reuse,
            roots,
            reinstall,
        }
    }

    /// A partition the install keeps must already be what it is used as: the
    /// ESP a FAT32 volume with room for the loader. An ESP that starts no
    /// volume, and SlopOS's own boot partition when it holds no FAT32 one, are
    /// what an install cut short leaves, and are formatted rather than kept.
    fn formats(&self, open: &OpenDisk, plan: &Plan) -> Result<Formats, Issue> {
        let esp = match plan.esp {
            Place::New { .. } => false,
            Place::Existing(n) => {
                let node = open.node(n)?;
                match &open.part(n)?.holds {
                    Some(Holds::Esp { free, .. }) => {
                        let needed = self.loader.len() as u64 + ESP_SLACK_BYTES;
                        if *free < needed {
                            return Err(Issue::Esp(format!(
                                "the ESP {node} has {} free; the loader needs {}",
                                human(*free),
                                human(needed)
                            )));
                        }
                        false
                    }
                    Some(Holds::EmptyEsp) => true,
                    Some(Holds::UnusableEsp(why)) => {
                        return Err(Issue::Esp(format!(
                            "the ESP {node} holds a volume this installer cannot use: {why}"
                        )));
                    }
                    _ => return Err(Issue::Esp(format!("{node} is no ESP"))),
                }
            }
        };
        let boot = match plan.boot {
            Place::New { .. } => false,
            Place::Existing(n) => {
                open.node(n)?;
                !matches!(open.part(n)?.holds, Some(Holds::Boot { fat: true }))
            }
        };
        Ok(Formats { esp, boot })
    }

    /// Install what `reviewed` proposes for `choices`, once the disk read
    /// again still proposes it.
    pub fn install(
        &self,
        choices: &Choices,
        reviewed: &Settled,
        events: &mut dyn Events,
    ) -> Result<Summary, Failure> {
        let mut run = Run {
            events,
            started: Instant::now(),
            head: reviewed.head(),
            lines: Vec::new(),
            step: 0,
            root: None,
        };
        let result = self
            .run(&mut run, choices, reviewed)
            .map_err(|message| Failure {
                step: run.step,
                message,
            });
        if let Err(failure) = &result {
            run.note(&format!(
                "failed in step {} of {}: {}",
                failure.step,
                STEPS.len(),
                failure.message
            ));
            // Nothing writes the root once it has been checked.
            if failure.step < CHECKING_THE_ROOT {
                let kept = run.keep_remounted();
                run.unless_kept(kept);
            }
        }
        result
    }

    fn run(
        &self,
        run: &mut Run<'_>,
        choices: &Choices,
        reviewed: &Settled,
    ) -> Result<Summary, String> {
        let unwritten = |issue: Issue| format!("{issue}; nothing was written");
        let open = self
            .index(&choices.disk)
            .and_then(|index| self.reopen(index))
            .map_err(unwritten)?;
        let p = self
            .propose_on(&open, choices, reviewed.guids)
            .map_err(unwritten)?;
        if p != *reviewed {
            return Err(format!(
                "{} changed since it was reviewed; nothing was written",
                choices.disk
            ));
        }
        let disk = &open.disk;
        let medium = &self.medium;

        run.step(1);
        for role in ROLES {
            if let Place::New {
                first_lba,
                last_lba,
                ..
            } = p.plan.place(role)
            {
                disk::clear_start(disk, first_lba, last_lba)?;
            }
        }
        disk::write_table(disk, &p.header, &p.array, p.view.erase)?;
        run.step(2);
        disk::reread(&disk.node)?;
        let [
            (esp, esp_node),
            (_, boot_node),
            (_, root_node),
            (_, crash_node),
        ] = placed(&disk.node, &p)?;

        run.step(3);
        if p.plan.esp.is_new() || p.formats.esp {
            disk::format_fat(&esp_node, ESP_LABEL)?;
            run.log(format!("{esp_node}: FAT32, {ESP_LABEL}"));
        }
        if p.plan.boot.is_new() || p.formats.boot {
            disk::format_fat(&boot_node, BOOT_LABEL)?;
            run.log(format!("{boot_node}: FAT32, {BOOT_LABEL}"));
        }
        // Records of the system a formatted root held, or what an install cut
        // short left, would read as this one's.
        let keep_root = p.view.keeps_root();
        if p.plan.crash.is_new() || !keep_root {
            disk::zero(&crash_node)?;
            run.log(format!("{crash_node}: zeroed for crash records"));
        }
        if !keep_root {
            disk::mke2fs(&root_node, run)?;
            run.log(format!("{root_node}: ext4"));
        }

        run.step(4);
        let mounted = root::Mounted::at(&root_node)?;
        let owners = root::lay_out(&mounted, !keep_root)?;
        if keep_root {
            root::forget_slots(&mounted)?;
        }
        run.root = Some(root_node.clone());
        run.keep_on(&mounted);
        root::fill(
            &mounted,
            medium,
            p.view.setup.remote.as_deref(),
            &owners,
            run,
        )?;
        if let Some(person) = &p.person {
            root::make_people(&mounted, &person.name, person.password.clone())?;
        }
        mounted.finish()?;

        run.step(5);
        boot::write_boot(&boot_node, medium, run)?;

        run.step(6);
        boot::write_loader(&esp_node, p.own_esp, &p.limine_conf, medium)?;
        boot::forget_slot_bases(&boot_node, run)?;

        run.step(7);
        let entry = boot::set_default()?;
        run.log(format!("{} is {entry}", bli::ENTRY_DEFAULT));
        // Before the check, which then covers the log, and the firmware entry,
        // which stays the last thing written.
        run.keep_remounted()?;

        run.step(CHECKING_THE_ROOT);
        disk::e2fsck(&root_node, run)?;

        run.step(9);
        let entry = boot::register(esp, boot_node, p.view.setup.first)?;
        Ok(Summary {
            disk: disk.node.clone(),
            entry,
        })
    }
}

impl Settled {
    pub fn disk(&self) -> &str {
        &self.view.disk
    }

    /// How the proposal heads the install's log.
    fn head(&self) -> Vec<String> {
        std::iter::once(format!("SlopOS will be installed on {}:", self.disk()))
            .chain(
                self.view
                    .review()
                    .into_iter()
                    .map(|line| format!("  {line}")),
            )
            .chain(self.view.left_out())
            .collect()
    }
}

/// The plan's partitions, as the table the kernel re-read names them: each
/// entry must be the one written, GUIDs and blocks alike, since the loader's
/// configuration was rendered from those.
fn placed(node: &str, p: &Settled) -> Result<[(Partition, String); 4], String> {
    let reread = Disk::open(node)?.partitions();
    let written: Vec<Partition> = p
        .header
        .partitions(&p.array)
        .filter_map(Result::ok)
        .collect();
    let find = |role: Role| -> Result<(Partition, String), String> {
        let number = p.plan.place(role).number();
        let entry = |list: &[Partition]| list.iter().find(|p| p.entry.number == number).copied();
        match (entry(&written), entry(&reread)) {
            (Some(want), Some(got)) if want.entry == got.entry => {
                Ok((got, partition_node(node, &got)?))
            }
            _ => Err(format!(
                "partition {number} of {node} after the re-read is not the {} the table was written with",
                role.name()
            )),
        }
    };
    Ok([
        find(Role::Esp)?,
        find(Role::Boot)?,
        find(Role::Root)?,
        find(Role::Crash)?,
    ])
}

/// An install under way: each event passed on, and kept as a line of the
/// log the new root holds.
struct Run<'a> {
    events: &'a mut dyn Events,
    started: Instant,
    head: Vec<String>,
    lines: Vec<String>,
    step: u32,
    /// The root's node once it is laid out, which the log is kept on.
    root: Option<String>,
}

impl Run<'_> {
    fn step(&mut self, index: u32) {
        self.event(Event::step(index));
    }

    fn note(&mut self, text: &str) {
        let seconds = self.started.elapsed().as_secs_f64();
        self.lines.push(format!("{seconds:8.1}s {text}"));
    }

    fn text(&self) -> String {
        let mut text = self.head.join("\n");
        text.push_str("\n\n");
        for line in &self.lines {
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    fn keep_on(&mut self, root: &root::Mounted) {
        let kept = root.keep_log(&self.text());
        self.unless_kept(kept);
    }

    /// Keep the log on the root, mounted again, once the install has laid it
    /// out. Only a root left mounted is an error.
    fn keep_remounted(&mut self) -> Result<(), String> {
        let Some(node) = self.root.clone() else {
            return Ok(());
        };
        match root::Mounted::at(&node) {
            Ok(root) => {
                self.keep_on(&root);
                root.finish()
            }
            Err(why) => {
                self.unless_kept(Err(why));
                Ok(())
            }
        }
    }

    /// A log that could not be kept is said, and fails no install.
    fn unless_kept(&mut self, kept: Result<(), String>) {
        if let Err(why) = kept {
            self.log(format!("the root keeps no install log: {why}"));
        }
    }
}

impl Events for Run<'_> {
    fn event(&mut self, event: Event) {
        match &event {
            Event::Step {
                index,
                total,
                title,
            } => {
                self.step = *index;
                self.note(&format!("[{index}/{total}] {title}"));
            }
            Event::Log { line } => self.note(&format!("  {line}")),
            Event::Progress { .. } => {}
        }
        self.events.event(event);
    }
}
