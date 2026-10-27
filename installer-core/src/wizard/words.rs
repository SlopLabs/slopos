//! The words the window shows, so the copy rules hold them: page headings,
//! the rows a page chooses from, its notes and its summary. Device
//! names, sizes in bytes and paths are captions; what a person acts on is in
//! the voice the desktop speaks.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use slopos_boot_core::install::Role;

use super::{Kind, Page, Wizard};
use crate::{
    Contents, DiskState, Listing, Mode, Part, Piece, Span, System, capitalised, disk_size, short,
};

/// The window's fixed words.
pub mod say {
    pub const BACK: &str = "Back";
    pub const NEXT: &str = "Next";
    pub const CLOSE: &str = "Close";
    pub const TRY: &str = "Try SlopOS";
    pub const KEEP_TRYING: &str = "Keep trying SlopOS";
    pub const RESTART: &str = "Restart";
    pub const SCAN: &str = "Scan again";
    pub const SHOW_DETAILS: &str = "Show details";
    pub const HIDE_DETAILS: &str = "Hide details";
    pub const ASKING: &str = "Allow the install in the dialog to continue.";
    pub const STARTING: &str = "Starting the installer…";
    pub const READING: &str = "Reading the disks…";
    pub const FREE_SPACE: &str = "Free space";
    pub const PARTITION: &str = "Partition";
    pub const NOW: &str = "This disk now";
    pub const AFTER: &str = "After the install";
    pub const SIZE: &str = "Size";
    pub const OPTIONS: &str = "Options";
    pub const REMOTE: &str = "Source code address";
    pub const REMOTE_CAPTION: &str = "Where /src/slopos fetches from and pushes to.";
    pub const FIRST: &str = "Put SlopOS first in the startup order";
    pub const SUMMARY: &str = "Summary";
    pub const PARTITIONS: &str = "What happens to each partition";
    pub const STEPS: &str = "Steps";
    pub const NOT_RESTARTED: &str = "The computer didn't restart. Choose Restart to try again.";
    pub const NOT_STARTED: &str = "The installer couldn't start. Try again.";
    pub const SENTENCES: [&str; 3] = [ASKING, NOT_RESTARTED, NOT_STARTED];
    pub const LABELS: [&str; 22] = [
        BACK,
        NEXT,
        CLOSE,
        TRY,
        KEEP_TRYING,
        RESTART,
        SCAN,
        SHOW_DETAILS,
        HIDE_DETAILS,
        STARTING,
        READING,
        FREE_SPACE,
        PARTITION,
        NOW,
        AFTER,
        SIZE,
        OPTIONS,
        REMOTE,
        FIRST,
        SUMMARY,
        PARTITIONS,
        STEPS,
    ];
}

/// A page's title and the sentence under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    pub title: String,
    pub subtitle: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    /// What stops the page going on.
    Danger,
}

/// Something the person should know before going on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub severity: Severity,
    pub title: String,
    /// The details: a size, a node or what a program said; may be empty.
    pub body: String,
}

impl Note {
    fn new(severity: Severity, title: impl Into<String>, body: impl Into<String>) -> Note {
        Note {
            severity,
            title: title.into(),
            body: body.into(),
        }
    }

    fn issue(issue: &crate::Issue) -> Note {
        Note::new(
            Severity::Danger,
            issue.words(),
            issue.detail().unwrap_or_default(),
        )
    }
}

/// A choice a page lists: a disk, a way to install, free space or a
/// partition, with the reason when it cannot be chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row<K> {
    pub key: K,
    pub title: String,
    /// What it is, in a caption.
    pub detail: String,
    pub reason: Option<String>,
}

impl<K> Row<K> {
    pub fn enabled(&self) -> bool {
        self.reason.is_none()
    }
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Erase => "Erase disk and install",
            Kind::Free => "Install beside what's there",
            Kind::Reuse => "Install on a partition",
            Kind::Reinstall => "Reinstall SlopOS",
        }
    }
}

/// What a listed disk holds, in a caption.
pub fn holding(listing: &Listing) -> String {
    match &listing.contents {
        Contents::Gpt => {
            let count = listing
                .layout
                .segments
                .iter()
                .filter(|s| matches!(s.piece, Piece::Partition { .. }))
                .count();
            let mut said = match count {
                0 => String::from("No partitions"),
                1 => String::from("1 partition"),
                n => format!("{n} partitions"),
            };
            let free: u64 = listing.layout.regions().map(|segment| segment.bytes).sum();
            if free > 0 {
                said.push_str(&format!(", {} free", disk_size(free)));
            }
            said
        }
        Contents::Blank => "Empty".into(),
        other => capitalised(&other.holds().unwrap_or_default()),
    }
}

/// A partition of `listing` as a reuse lists it: its own name or its type,
/// and its size, with its node in the caption.
fn partition_row(listing: &Listing, number: u32) -> (String, String) {
    listing
        .layout
        .segments
        .iter()
        .find_map(|segment| match &segment.piece {
            Piece::Partition {
                number: n,
                node,
                kind,
                name,
                ..
            } if *n == number => {
                let size = disk_size(segment.bytes);
                Some(match name.is_empty() {
                    true => (format!("{kind} · {size}"), short(node).into()),
                    false => (
                        format!("{name} · {size}"),
                        format!("{} · {kind}", short(node)),
                    ),
                })
            }
            _ => None,
        })
        .unwrap_or_else(|| (format!("Partition {number}"), String::new()))
}

impl Wizard {
    pub(super) fn listing(&self) -> Option<&Listing> {
        self.system
            .as_ref()?
            .disk(&self.answers.as_ref()?.disk)?
            .listing()
    }

    /// A disk as the person knows it: its maker's name and its size.
    pub fn disk_title(&self, node: &str) -> String {
        let Some(disk) = self.system.as_ref().and_then(|s| s.disk(node)) else {
            return short(node).into();
        };
        let size = disk.listing().map(|listing| disk_size(listing.capacity));
        match (&disk.model, size) {
            (Some(model), Some(size)) => format!("{model} · {size}"),
            (Some(model), None) => model.clone(),
            (None, Some(size)) => format!("{size} disk"),
            (None, None) => short(node).into(),
        }
    }

    /// A disk as a sentence names it: by its title, or as the disk of its
    /// size when its maker is unknown.
    fn disk_named(&self, node: &str) -> String {
        let title = self.disk_title(node);
        match self.system.as_ref().and_then(|s| s.disk(node)) {
            Some(disk) if disk.model.is_none() && disk.listing().is_some() => {
                format!("the {title}")
            }
            _ => title,
        }
    }

    /// The title and the sentence under it of the page in front; `live` on
    /// a live boot's own window, which offers to try SlopOS first.
    pub fn heading(&self, live: bool) -> Heading {
        let disk = self
            .summary
            .as_ref()
            .map(|s| s.disk.as_str())
            .or(self.answers.as_ref().map(|a| a.disk.as_str()))
            .map(|node| self.disk_named(node))
            .unwrap_or_default();
        let (title, subtitle) = match self.page {
            Page::Welcome if live => (
                "Welcome to SlopOS".into(),
                "You're running SlopOS from the installer. Nothing you do here is saved. Try it as it is, or install it on this computer.".into(),
            ),
            Page::Welcome => (
                "Install SlopOS".into(),
                "SlopOS is copied from the installer onto a disk of this computer. Nothing is written until you've reviewed what will happen.".into(),
            ),
            Page::Disk => (
                "Where do you want to install SlopOS?".into(),
                "Choose a disk. You'll review what happens to it before anything is written."
                    .into(),
            ),
            Page::How => (
                "How do you want to install SlopOS?".into(),
                format!("On {disk}."),
            ),
            Page::Review => (
                "Ready to install".into(),
                "Check what will happen to the disk, then confirm.".into(),
            ),
            Page::Install => (
                "Installing SlopOS".into(),
                format!("On {disk}. This takes a few minutes."),
            ),
            Page::Done => (
                "SlopOS is installed".into(),
                "Remove the USB stick or DVD you installed from, then restart.".into(),
            ),
            Page::Error => (
                "SlopOS couldn't be installed".into(),
                format!(
                    "{} Try again, or restart and start over.",
                    self.failure
                        .as_ref()
                        .map(crate::Failure::words)
                        .unwrap_or_default()
                ),
            ),
        };
        Heading { title, subtitle }
    }

    /// The Disk page's disks: those that may be chosen first, each with what
    /// it holds, then the rest with why not.
    pub fn disks(&self) -> Vec<Row<String>> {
        let Some(system) = &self.system else {
            return Vec::new();
        };
        let (offered, rest): (Vec<_>, Vec<_>) = system
            .disks
            .iter()
            .partition(|disk| disk.listing().is_some());
        offered
            .into_iter()
            .chain(rest)
            .map(|disk| {
                let name = short(&disk.node);
                let (detail, reason) = match &disk.state {
                    DiskState::Listed(listing) => (format!("{name} · {}", holding(listing)), None),
                    DiskState::Refused(why) => (name.into(), Some(why.words().into())),
                    DiskState::Unreadable(_) => (
                        name.into(),
                        Some(format!(
                            "This disk couldn't be read. Check that it's connected, then choose {}.",
                            say::SCAN
                        )),
                    ),
                };
                Row {
                    key: disk.node.clone(),
                    title: self.disk_title(&disk.node),
                    detail,
                    reason,
                }
            })
            .collect()
    }

    /// How's choices: what each does, or why it cannot be chosen.
    pub fn tile_rows(&self) -> Vec<Row<Kind>> {
        let keeps = self
            .opened()
            .and_then(|o| o.reinstall.as_ref())
            .is_some_and(|r| r.keep);
        self.tiles()
            .into_iter()
            .map(|(kind, fit)| Row {
                key: kind,
                title: kind.title().into(),
                detail: match kind {
                    Kind::Erase => "Everything on this disk is deleted.".into(),
                    Kind::Free => {
                        "SlopOS goes in free space. Nothing else on the disk changes.".into()
                    }
                    Kind::Reuse => {
                        "The partition you choose is erased, and SlopOS is installed on it.".into()
                    }
                    Kind::Reinstall if keeps => "Its accounts and their files are kept.".into(),
                    Kind::Reinstall => "Its partition is erased first.".into(),
                },
                reason: fit.as_ref().err().map(crate::Issue::words),
            })
            .collect()
    }

    /// Every free space a free-space install may take, once there are two
    /// or more.
    pub fn region_rows(&self) -> Vec<Row<usize>> {
        let (Some(opened), Some(listing)) = (self.opened(), self.listing()) else {
            return Vec::new();
        };
        if self.region().is_none() || self.regions().is_empty() {
            return Vec::new();
        }
        listing
            .layout
            .regions()
            .filter_map(|segment| match segment.piece {
                Piece::Free { region } => Some((region, segment)),
                Piece::Partition { .. } => None,
            })
            .map(|(region, segment)| Row {
                key: region,
                title: format!("{} of free space", disk_size(segment.bytes)),
                detail: format!("Starts at {}", disk_size(segment.start)),
                reason: match opened.regions.get(region) {
                    Some(Ok(())) => None,
                    Some(Err(issue)) => Some(issue.words()),
                    None => Some("This free space can't be used.".into()),
                },
            })
            .collect()
    }

    /// The partitions a reuse may install on.
    pub fn root_rows(&self) -> Vec<Row<u32>> {
        let Some(listing) = self.listing() else {
            return Vec::new();
        };
        self.roots()
            .iter()
            .map(|(number, fit)| {
                let (title, detail) = partition_row(listing, *number);
                Row {
                    key: *number,
                    title,
                    detail,
                    reason: fit.as_ref().err().map(crate::Issue::words),
                }
            })
            .collect()
    }

    /// The sizes the root may take on the How page: the engine's last
    /// answer's, while it answered a new root of this kind on this disk.
    pub fn root_span(&self) -> Option<Span> {
        let a = self.answers.as_ref()?;
        let proposed = self.proposed.as_ref()?;
        let (_, proposal) = proposed.answer.as_ref().ok()?;
        let same = proposed.choices.disk == a.disk
            && matches!(
                (proposed.choices.mode, a.kind),
                (Mode::Erase, Kind::Erase) | (Mode::Free { .. }, Kind::Free)
            );
        same.then_some(proposal.root)
    }

    /// The root's size as the slider shows it: the size asked, within the
    /// span, or all the space gives.
    pub fn root_size(&self) -> Option<u64> {
        let span = self.root_span()?;
        Some(match self.answers.as_ref()?.root_size {
            Some(asked) => asked.max(span.min).min(span.max),
            None => span.max,
        })
    }

    /// The sizes SlopOS's partition may take, under the slider.
    pub fn size_range(&self) -> Option<String> {
        let span = self.root_span()?;
        Some(format!(
            "From {} to {}.",
            disk_size(span.min),
            disk_size(span.max)
        ))
    }

    /// What the page in front tells the person before they go on: what the
    /// installer lacks, what stops the page, and what the window said.
    pub fn notes(&self) -> Vec<Note> {
        let mut notes = Vec::new();
        let medium = self.system.as_ref().map(|s| &s.medium);
        let unread = medium.and_then(|m| m.unmounted_payload.as_ref());
        let lacking = medium.is_some_and(|m| !m.toolchain);
        match self.page {
            Page::Disk => {
                if lacking && unread.is_none() {
                    notes.push(Note::new(
                        Severity::Info,
                        "Developer tools aren't on this installer, so they won't be installed.",
                        "",
                    ));
                }
                if let Some(node) = unread {
                    notes.push(Note::new(
                        Severity::Warning,
                        "The installer's developer tools couldn't be read, so they won't be installed.",
                        format!("They are on {}. The system log says why.", short(node)),
                    ));
                }
                let offered = |s: &System| s.disks.iter().any(|d| d.listing().is_some());
                if self.system.as_ref().is_some_and(|s| !offered(s)) {
                    notes.push(Note::new(
                        Severity::Warning,
                        "There's no disk to install SlopOS on.",
                        format!("Connect a disk, then choose {}.", say::SCAN),
                    ));
                }
            }
            Page::Review => {
                for line in self.proposal().into_iter().flat_map(|p| p.left_out()) {
                    notes.push(Note::new(Severity::Warning, line, ""));
                }
            }
            Page::Done if lacking => {
                notes.push(Note::new(
                    Severity::Info,
                    match unread {
                        Some(_) => {
                            "Developer tools weren't installed, since they couldn't be read from the installer."
                        }
                        None => {
                            "Developer tools weren't installed, since this installer doesn't have them."
                        }
                    },
                    "",
                ));
            }
            _ => {}
        }
        notes.extend(self.issue().map(Note::issue));
        if let Some(note) = self.note() {
            notes.push(Note::new(Severity::Warning, note, ""));
        }
        notes
    }

    /// The Review page's summary, a line per subject.
    pub fn summary_lines(&self) -> Vec<(&'static str, String)> {
        let (Some(a), Some(p)) = (self.answers.as_ref(), self.proposal()) else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        lines.push((
            "Disk",
            format!("{} ({})", self.disk_title(&a.disk), short(&a.disk)),
        ));
        lines.push(("Method", a.kind.title().into()));
        if let Some((_, part)) = p.role(Role::Root) {
            let node = match part {
                Part::New { node, .. } | Part::Existing { node, .. } => short(node),
            };
            lines.push((
                "Partition",
                format!("{} ({node})", disk_size(p.root.chosen)),
            ));
        }
        lines.push((
            "Developer tools",
            match (p.setup.toolchain, self.system.as_ref().map(|s| &s.medium)) {
                (true, _) => "Installed at /usr/local".into(),
                (false, Some(m)) if m.unmounted_payload.is_some() => {
                    "Not installed, since they couldn't be read".into()
                }
                (false, _) => "Not on this installer".into(),
            },
        ));
        if let Some(url) = &p.setup.remote {
            lines.push(("Source code", format!("At /src/slopos, fetched from {url}")));
        }
        lines.push(match &p.setup.person {
            Some(person) => (
                "First account",
                format!("{}, an administrator", person.name),
            ),
            None => ("Accounts", "Kept as they are".into()),
        });
        lines.push((
            "Startup order",
            match p.setup.first {
                true => "Starts first".into(),
                false => {
                    "Keeps its place, or starts after the systems already on this computer".into()
                }
            },
        ));
        let mut menu = String::from("SlopOS");
        for other in &p.setup.menu {
            menu.push_str(", ");
            menu.push_str(other);
        }
        lines.push(("Startup menu", menu));
        lines
    }

    /// What the Done page's details name: where SlopOS went and the startup
    /// entry that starts it.
    pub fn done_lines(&self) -> Vec<(&'static str, String)> {
        let Some(summary) = &self.summary else {
            return Vec::new();
        };
        alloc::vec![
            ("Disk", short(&summary.disk).into()),
            ("Startup entry", format!("Boot{:04X}", summary.entry)),
        ]
    }
}
