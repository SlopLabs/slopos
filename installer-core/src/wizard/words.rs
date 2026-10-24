use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use slopos_boot_core::install::{PlanError, Role};

use super::{Kind, Wizard};
use crate::{
    Contents, DiskState, Issue, Listing, Mode, Part, Piece, STEPS, Span, human, partition_node,
    short,
};

/// A choice a page lists: a disk, a tile, a free region or a partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row<K> {
    pub key: K,
    pub label: String,
    /// What it is, or why it cannot be chosen.
    pub detail: String,
    pub enabled: bool,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Erase => "Erase disk and install",
            Kind::Free => "Install beside what is there",
            Kind::Reuse => "Use a partition as the root",
            Kind::Reinstall => "Reinstall SlopOS",
        }
    }
}

/// An issue as the window says it: the CLI's words, less the flags the CLI
/// names for a way out.
pub fn words(issue: &Issue) -> String {
    match issue {
        Issue::OnlyErase { node, holds } => {
            format!(
                "{} holds {holds}; only erasing it installs onto it",
                short(node)
            )
        }
        Issue::NothingToKeep(node) => format!("{} holds no SlopOS root to keep", short(node)),
        Issue::RootSize => "a reused root keeps its partition's size".into(),
        Issue::Remote(_) => "a URL holds no control character".into(),
        Issue::Plan {
            error: PlanError::Installed(_) | PlanError::RootElsewhere(_),
            node,
            ..
        } => format!("SlopOS is on {} already: reinstall it instead", short(node)),
        other => other.to_string(),
    }
}

fn contents_line(listing: &Listing) -> String {
    match &listing.contents {
        Contents::Gpt => {
            let kinds: Vec<&str> = listing
                .layout
                .segments
                .iter()
                .filter_map(|segment| match &segment.piece {
                    Piece::Partition { kind, .. } => Some(kind.as_str()),
                    Piece::Free { .. } => None,
                })
                .collect();
            let mut said = if kinds.is_empty() {
                String::from("GPT: no partitions")
            } else {
                format!("GPT: {}", kinds.join(", "))
            };
            let free: u64 = listing.layout.regions().map(|segment| segment.bytes).sum();
            if free > 0 {
                said.push_str(&format!(", {} free", human(free)));
            }
            said
        }
        Contents::Blank => "blank".into(),
        other => other.holding(),
    }
}

fn partition_line(listing: &Listing, number: u32) -> String {
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
                let what = if name.is_empty() {
                    kind.clone()
                } else {
                    format!("{kind}, \"{name}\"")
                };
                Some(format!("{}: {what}, {}", short(node), human(segment.bytes)))
            }
            _ => None,
        })
        .unwrap_or_else(|| format!("partition {number}"))
}

impl Wizard {
    fn listing(&self) -> Option<&Listing> {
        self.system
            .as_ref()?
            .disk(&self.answers.as_ref()?.disk)?
            .listing()
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
                let (label, detail) = match &disk.state {
                    DiskState::Listed(listing) => (
                        format!("{name} — {}", human(listing.capacity)),
                        contents_line(listing),
                    ),
                    DiskState::Refused(why) | DiskState::Unreadable(why) => {
                        (name.into(), why.clone())
                    }
                };
                Row {
                    key: disk.node.clone(),
                    label,
                    detail,
                    enabled: disk.listing().is_some(),
                }
            })
            .collect()
    }

    /// How's tiles: what each does, or why it cannot be chosen.
    pub fn tile_rows(&self) -> Vec<Row<Kind>> {
        let disk = self.answers.as_ref().map_or("", |a| a.disk.as_str());
        let reinstall = self.opened().and_then(|o| o.reinstall.as_ref());
        self.tiles()
            .into_iter()
            .map(|(kind, fit)| {
                let detail = match (fit, kind) {
                    (Err(issue), _) => words(issue),
                    (Ok(()), Kind::Erase) => format!("everything on {} is lost", short(disk)),
                    (Ok(()), Kind::Free) => {
                        "new partitions in free space; every other partition stays as it is".into()
                    }
                    (Ok(()), Kind::Reuse) => "the partition chosen is formatted as the root".into(),
                    (Ok(()), Kind::Reinstall) => {
                        let node = reinstall
                            .map_or(String::new(), |r| partition_node(short(disk), r.root));
                        if reinstall.is_some_and(|r| r.keep) {
                            format!("keeps the people and files {node} holds")
                        } else {
                            format!("formats {node}")
                        }
                    }
                };
                Row {
                    key: kind,
                    label: kind.title().into(),
                    detail,
                    enabled: fit.is_ok(),
                }
            })
            .collect()
    }

    /// Every free region of a free-space install, once two or more take it.
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
            .map(|(region, segment)| {
                let fit = opened.regions.get(region);
                Row {
                    key: region,
                    label: format!(
                        "Free region {}: {} at {}",
                        region + 1,
                        human(segment.bytes),
                        human(segment.start)
                    ),
                    detail: match fit {
                        Some(Err(issue)) => words(issue),
                        _ => String::new(),
                    },
                    enabled: fit.is_some_and(|fit| fit.is_ok()),
                }
            })
            .collect()
    }

    /// The partitions a reuse may take as the root, each named as the disk
    /// lists it.
    pub fn root_rows(&self) -> Vec<Row<u32>> {
        let Some(listing) = self.listing() else {
            return Vec::new();
        };
        self.roots()
            .iter()
            .map(|(number, fit)| Row {
                key: *number,
                label: partition_line(listing, *number),
                detail: fit.as_ref().err().map(words).unwrap_or_default(),
                enabled: fit.is_ok(),
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
    /// span, or all the region gives.
    pub fn root_size(&self) -> Option<u64> {
        let span = self.root_span()?;
        Some(match self.answers.as_ref()?.root_size {
            Some(asked) => asked.max(span.min).min(span.max),
            None => span.max,
        })
    }

    /// What the medium lacks, said once the machine is probed.
    pub fn medium_notes(&self) -> Vec<String> {
        let Some(medium) = self.system.as_ref().map(|s| &s.medium) else {
            return Vec::new();
        };
        let mut notes = Vec::new();
        if !medium.toolchain {
            notes.push("The medium carries no toolchain, so the installed system has none".into());
        }
        if let Some(node) = &medium.unmounted_payload {
            notes.push(format!(
                "{} carries the medium's payload, which the kernel did not mount; its log says why",
                short(node)
            ));
        }
        notes
    }

    /// The Review page's summary, a line per subject.
    pub fn summary_lines(&self) -> Vec<(&'static str, String)> {
        let (Some(a), Some(p)) = (self.answers.as_ref(), self.proposal()) else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        let capacity = self.listing().map_or(0, |l| l.capacity);
        lines.push(("Disk", format!("{} — {}", short(&a.disk), human(capacity))));
        lines.push(("How", a.kind.title().into()));
        if let Some((_, part)) = p.role(Role::Root) {
            let node = match part {
                Part::New { node, .. } | Part::Existing { node, .. } => short(node),
            };
            lines.push(("Root", format!("{node}, {}", human(p.root.chosen))));
        }
        lines.push((
            "Toolchain",
            if p.setup.toolchain {
                "the medium's, at /usr/local".into()
            } else {
                "none: the medium carries no toolchain".into()
            },
        ));
        if let Some(url) = &p.setup.remote {
            lines.push(("Source", format!("/src/slopos, fetching from {url}")));
        }
        lines.push((
            "People",
            match &p.setup.person {
                Some(person) => format!("{}, the first administrator", person.name),
                None => "the root's own".into(),
            },
        ));
        lines.push((
            "Boot order",
            if p.setup.first {
                "SlopOS first".into()
            } else {
                "SlopOS where it stands, or after the systems there".into()
            },
        ));
        let mut menu = String::from("SlopOS");
        for other in &p.setup.menu {
            menu.push_str(", ");
            menu.push_str(other);
        }
        lines.push(("Boot menu", menu));
        lines
    }

    /// The Install page's heading, and the Done and Error pages' lines.
    pub fn outcome(&self) -> Vec<String> {
        let disk = self
            .summary
            .as_ref()
            .map(|s| s.disk.as_str())
            .or(self.answers.as_ref().map(|a| a.disk.as_str()))
            .map_or("", short);
        match (&self.summary, &self.failure) {
            (Some(summary), _) if self.page == super::Page::Done => {
                let mut lines = alloc::vec![
                    format!("SlopOS is installed on {disk}"),
                    format!(
                        "The firmware boots it as Boot{:04X}. Take the medium out before the machine restarts.",
                        summary.entry
                    ),
                ];
                if self.system.as_ref().is_some_and(|s| !s.medium.toolchain) {
                    lines.push(
                        "The medium carries no toolchain, so the installed system has none".into(),
                    );
                }
                lines
            }
            (_, Some(failure)) if self.page == super::Page::Error => {
                let said = match STEPS.get((failure.step as usize).wrapping_sub(1)) {
                    Some(title) => format!(
                        "It stopped at step {} of {}, {title}: {}",
                        failure.step,
                        STEPS.len(),
                        failure.message
                    ),
                    None => match failure.message.strip_suffix("; nothing was written") {
                        Some(why) => format!("It stopped before writing anything: {why}"),
                        None => format!("It stopped before its first step: {}", failure.message),
                    },
                };
                alloc::vec![format!("The install on {disk} did not finish"), said]
            }
            _ => alloc::vec![format!("Installing SlopOS on {disk}")],
        }
    }
}
