//! The copy rules over every word the window and the CLI take from here, the
//! installer's trade kept to the details.

use alloc::string::String;
use alloc::vec;

use slopos_boot_core::disk_id::Bus;
use slopos_boot_core::install::{PlanError, ROLES};
use slopos_theme_core::words::{label, plain, sentence};

use super::*;
use crate::fixtures::{GIB, gapped, no_tables};
use crate::wizard::say::Shape;
use crate::{Contents, EspTrouble, Layout, Listing, Medium, Refusal, Step, System};

const TRADE: [&str; 13] = [
    "toolchain",
    "payload",
    "slot",
    "base",
    "esp",
    "gpt",
    "mbr",
    "root",
    "firmware",
    "kernel",
    "loader",
    "node",
    "nvme0n1",
];

#[derive(Default)]
struct Said {
    labels: Vec<String>,
    sentences: Vec<String>,
    captions: Vec<String>,
}

impl Said {
    fn label(&mut self, text: impl Into<String>) {
        self.labels.push(text.into());
    }

    fn sentence(&mut self, text: impl Into<String>) {
        self.sentences.push(text.into());
    }

    fn caption(&mut self, text: impl Into<String>) {
        self.captions.push(text.into());
    }

    fn rows<K>(&mut self, rows: &[Row<K>], titled: bool) {
        for row in rows {
            match titled {
                true => self.label(row.title.clone()),
                false => self.caption(row.title.clone()),
            }
            self.caption(row.detail.clone());
            if let Some(reason) = &row.reason {
                self.sentence(reason.clone());
            }
        }
    }

    /// Everything the window shows of `w`'s page, live or from the dock.
    fn page(&mut self, w: &Wizard) {
        for live in [true, false] {
            let heading = w.heading(live);
            self.label(heading.title);
            self.sentence(heading.subtitle);
            if let Some(disk) = heading.disk {
                self.caption(disk);
            }
        }
        if w.page() != Page::Install {
            self.label(w.primary_label());
        }
        self.rows(&w.disks(), false);
        self.rows(&w.tile_rows(), true);
        self.rows(&w.region_rows(), false);
        self.rows(&w.root_rows(), false);
        for note in w.notes() {
            self.sentence(note.title);
            if !note.body.is_empty() {
                self.caption(note.body);
            }
        }
        for (name, value) in w.summary_lines().into_iter().chain(w.done_lines()) {
            self.label(name);
            self.caption(value);
        }
        if let Some(range) = w.size_range() {
            self.sentence(range);
        }
        if w.page() == Page::Review {
            self.label(w.confirmation());
        }
        for (step, _) in w.steps() {
            self.label(step);
        }
        if let Some(listing) = w.listing() {
            self.layout(&listing.layout);
        }
        if let Some(proposal) = w.proposal() {
            self.proposal(proposal);
        }
        if let Some(Failure::Stopped { why, .. }) = w.failure() {
            self.caption(why.clone());
        }
    }

    fn layout(&mut self, layout: &Layout) {
        for segment in &layout.segments {
            self.caption(segment.label());
        }
    }

    fn proposal(&mut self, p: &Proposal) {
        self.layout(&p.after);
        for (tag, line) in p.action_lines() {
            self.label(tag.word());
            self.caption(line);
        }
        for line in p.left_out() {
            self.sentence(line);
        }
        for line in p.review() {
            self.caption(line.replace(&p.setup.cmdline, ""));
        }
    }

    fn hold(&self) {
        for text in self
            .labels
            .iter()
            .chain(&self.sentences)
            .chain(&self.captions)
        {
            for unit in ["KiB", "MiB", "GiB", "TiB", "PiB"] {
                assert!(!text.contains(unit), "{text:?} is not in decimal units");
            }
        }
        for text in &self.labels {
            assert_eq!(label(text, &TRADE), Ok(()), "label {text:?}");
        }
        for text in &self.sentences {
            assert_eq!(sentence(text, &TRADE), Ok(()), "sentence {text:?}");
        }
        for text in &self.captions {
            assert_eq!(plain(text, &[]), Ok(()), "caption {text:?}");
        }
    }
}

/// Every issue the engine can answer the window's choices with.
fn window_issues() -> Vec<Issue> {
    let plan = |error| Issue::Plan {
        error,
        node: NODE.into(),
        root: Some("/dev/nvme0n1p3".into()),
    };
    let mut issues = vec![
        Issue::Medium("/media/install holds no installer".into()),
        Issue::Probe("/dev: gone".into()),
        Issue::Disk("/dev/nvme0n1: I/O error".into()),
        Issue::NoRoot("/dev/nvme0n1".into()),
        Issue::Esp {
            node: "/dev/nvme0n1p1".into(),
            trouble: EspTrouble::Full {
                free: 512 << 10,
                needed: 3 << 20,
            },
        },
        Issue::Esp {
            node: "/dev/nvme0n1p1".into(),
            trouble: EspTrouble::Unreadable("not FAT32".into()),
        },
        Issue::Esp {
            node: "/dev/nvme0n1p1".into(),
            trouble: EspTrouble::WrongType,
        },
        Issue::Menu("no".into()),
        Issue::Remote("a\nb".into()),
        Issue::Unprobed,
        Issue::Unopened(NODE.into()),
        Issue::Unproposed,
        Issue::Changed(NODE.into()),
        plan(PlanError::NoRegion),
        plan(PlanError::NoRoom {
            needed: 8 * GIB,
            available: GIB,
        }),
        plan(PlanError::NoPartition(9)),
        plan(PlanError::NotForRoot(1)),
        plan(PlanError::Installed(3)),
        plan(PlanError::RootTooSmall {
            needed: 8 * GIB,
            available: GIB,
        }),
        plan(PlanError::RootTooLarge {
            limit: 16 << 40,
            available: 17 << 40,
        }),
        plan(PlanError::RootElsewhere(3)),
        plan(PlanError::NoSlot),
    ];
    issues.extend(ROLES.map(|role| plan(PlanError::Ambiguous(role))));
    issues.extend(no_tables().into_iter().map(|contents| Issue::OnlyErase {
        node: NODE.into(),
        contents,
    }));
    issues
}

/// The window's issues and those only the CLI's own choices raise: a name,
/// a size for a reused root or a root to keep.
fn issues() -> Vec<Issue> {
    let mut issues = window_issues();
    issues.extend([
        Issue::NothingToKeep("/dev/nvme0n1p3".into()),
        Issue::Person("bad".into()),
        Issue::RootSize,
    ]);
    issues
}

/// A computer whose installer lacks the developer tools and could not read
/// them, with a disk of every kind the Disk page lists.
fn every_disk() -> System {
    let (header, array) = gapped();
    let listed = |contents: Contents, layout| {
        DiskState::Listed(Listing {
            capacity: 16 * GIB,
            block: 512,
            contents,
            layout,
            opened: None,
        })
    };
    let mut disks = vec![
        crate::Disk {
            node: "/dev/sda".into(),
            bus: Some(Bus::Usb),
            model: None,
            state: DiskState::Refused(Refusal::Medium),
        },
        crate::Disk {
            node: "/dev/sdb".into(),
            bus: Some(Bus::Usb),
            model: Some("Ultra Fit".into()),
            state: DiskState::Refused(Refusal::ReadOnly),
        },
        crate::Disk {
            node: "/dev/sdc".into(),
            bus: Some(Bus::Usb),
            model: None,
            state: DiskState::Unreadable("/dev/sdc: I/O error".into()),
        },
        crate::Disk {
            node: NODE.into(),
            bus: Some(Bus::Nvme),
            model: Some(MODEL.into()),
            state: listed(Contents::Gpt, Layout::of(NODE, &header, &array)),
        },
    ];
    for (n, contents) in no_tables().into_iter().enumerate() {
        disks.push(crate::Disk {
            node: alloc::format!("/dev/vd{}", (b'a' + n as u8) as char),
            bus: Some(Bus::Virtio),
            model: None,
            state: listed(Contents::NoTable(contents), Layout::empty(16 * GIB)),
        });
    }
    System {
        medium: Medium {
            toolchain: false,
            source: false,
            payload: None,
            root_min: GIB,
            unmounted_payload: Some("/dev/sdd".into()),
        },
        disks,
        others: Ok(Vec::new()),
    }
}

fn steps() -> impl Iterator<Item = Option<Step>> {
    core::iter::once(None).chain(Step::ALL.map(Some))
}

#[test]
fn the_fixed_words_keep_the_rules() {
    let mut said = Said::default();
    for &(shape, text) in say::ALL {
        match shape {
            Shape::Label => said.label(text),
            Shape::Sentence => said.sentence(text),
            Shape::Caption => said.caption(text),
        }
    }
    for kind in [Kind::Erase, Kind::Free, Kind::Reuse, Kind::Reinstall] {
        said.label(kind.title());
    }
    for refusal in [Refusal::Medium, Refusal::ReadOnly] {
        said.sentence(refusal.words());
    }
    for contents in no_tables() {
        said.sentence(contents.only_erase());
        said.caption(contents.holds().unwrap_or_default());
    }
    for step in steps() {
        said.sentence(
            Failure::Stopped {
                step,
                why: String::new(),
            }
            .words(),
        );
    }
    for issue in window_issues() {
        let words = issue.words();
        assert!(
            words.matches(". ").count() >= 1,
            "{words:?} ends at the fact"
        );
        said.sentence(words);
        said.caption(issue.detail().unwrap_or_default());
    }
    said.hold();
}

/// The CLI's words less the flags it names, which are its own spelling.
fn unflagged(cli: &str) -> String {
    cli.split(' ')
        .filter(|word| !word.starts_with("--"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn the_cli_says_what_the_window_says_and_names_its_flag() {
    for issue in issues() {
        let cli = issue.to_string();
        assert_eq!(plain(&unflagged(&cli), &[]), Ok(()), "{cli}");
        assert!(cli.ends_with('.'), "{cli:?} ends at a full stop");
        let words = issue.words();
        let fact = words
            .split_inclusive(". ")
            .next()
            .unwrap_or(&words)
            .trim_end()
            .trim_end_matches('.');
        assert!(cli.contains(fact), "{cli:?} does not say {fact:?}");
        let no_flag = matches!(
            issue,
            Issue::Medium(_)
                | Issue::Probe(_)
                | Issue::Menu(_)
                | Issue::Unprobed
                | Issue::Unopened(_)
                | Issue::Unproposed
                | Issue::Changed(_)
        );
        assert_eq!(
            cli.contains("--"),
            !no_flag,
            "{cli:?} names a flag where one changes it"
        );
    }
}

#[test]
fn every_page_of_every_install_keeps_the_rules() {
    let mut said = Said::default();
    let mut w = Wizard::new();
    said.page(&w);
    w.on(Input::Next);
    w.on(Input::Refused(
        "Nothing was installed, because you didn't allow it.".into(),
    ));
    said.page(&w);
    w.on(Input::Next);
    sent(w.on(Input::Started));
    reply(&mut w, Reply::Hello { version: VERSION });
    reply(&mut w, Reply::System(every_disk()));
    said.page(&w);

    let reinstall = |keep| {
        let mut case = Case::on(
            installed(),
            slopos_boot_core::install::Mode::Reuse {
                root: 3,
                region: None,
            },
        );
        case.keep_root = keep;
        let reinstall = Some(Reinstall {
            root: 3,
            keep,
            fit: Ok(()),
        });
        (opened(Ok(()), reinstall), case.proposal())
    };
    let free = || {
        let mut proposal = free_proposal();
        proposal.setup.left_out = vec!["Twin".into()];
        proposal.setup.removed = vec!["Gone".into()];
        proposal.setup.person = None;
        proposal
    };
    let cases = [
        (opened(Ok(()), None), free_proposal()),
        (opened(Err(no_room()), None), free()),
        (
            opened(Ok(()), None),
            Case::erasing(Some(foreign())).proposal(),
        ),
        reinstall(true),
        reinstall(false),
    ];
    for (open, proposal) in cases {
        let (mut w, _) = at_how(open, proposal);
        said.page(&w);
        for kind in [Kind::Free, Kind::Reuse, Kind::Erase, Kind::Reinstall] {
            w.on(Input::Kind(kind));
            said.page(&w);
        }
    }
    for failed in steps() {
        let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
        review(&mut w);
        let step = failed.unwrap_or(Step::Partition);
        reply(&mut w, Reply::Event(Event::Step(step)));
        said.page(&w);
        reply(
            &mut w,
            Reply::Failed(Failure::Stopped {
                step: failed,
                why: "mke2fs exited 1".into(),
            }),
        );
        said.page(&w);
    }
    for issue in window_issues() {
        let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
        review(&mut w);
        reply(&mut w, Reply::Failed(Failure::Refused(issue)));
        said.page(&w);
    }
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    review(&mut w);
    reply(
        &mut w,
        Reply::Done(Summary {
            disk: NODE.into(),
            entry: 3,
        }),
    );
    said.page(&w);
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    review(&mut w);
    w.on(Input::Ended);
    said.page(&w);
    said.hold();
    assert!(said.labels.len() > 100 && said.sentences.len() > 50);
}

#[test]
fn a_device_name_stays_in_the_captions() {
    let mut said = Said::default();
    let (w, _) = at_how(opened(Ok(()), None), free_proposal());
    said.page(&w);
    for text in said.labels.iter().chain(&said.sentences) {
        assert!(!text.contains("/dev/"), "{text}");
    }
    assert!(said.captions.iter().any(|c| c.contains("nvme0n1")));
}
