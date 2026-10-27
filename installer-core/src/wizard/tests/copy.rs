//! The copy rules over every word the window and the CLI take from here,
//! on every fixture: no flag, no three dots, no word the glossary replaces;
//! a capital on what is a label and a full stop on what is a sentence; and
//! the installer's trade — what the details may name — off both.

use alloc::string::String;
use alloc::vec;

use slopos_boot_core::install::{PlanError, ROLES};
use slopos_theme_core::words::{label, plain, sentence};

use super::*;
use crate::fixtures::{GIB, gapped};
use crate::{Layout, Listing, MbrPartition, Medium, Refusal, STEPS, System};

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

    /// Everything the window shows of `w`'s page, `live` or from the dock.
    fn page(&mut self, w: &Wizard) {
        for live in [true, false] {
            let heading = w.heading(live);
            self.label(heading.title);
            self.sentence(heading.subtitle);
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

fn issues() -> Vec<Issue> {
    let plan = |error| Issue::Plan {
        error,
        node: NODE.into(),
        root: Some("/dev/nvme0n1p3".into()),
    };
    let mut issues = vec![
        Issue::Medium("/media/install holds no installer".into()),
        Issue::Probe("/dev: gone".into()),
        Issue::Disk("/dev/nvme0n1: I/O error".into()),
        Issue::NothingToKeep("/dev/nvme0n1p3".into()),
        Issue::NoRoot("/dev/nvme0n1".into()),
        Issue::Esp("full".into()),
        Issue::Person("bad".into()),
        Issue::Menu("no".into()),
        Issue::RootSize,
        Issue::Remote("a\nb".into()),
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
    issues.extend(contents().into_iter().map(|contents| Issue::OnlyErase {
        node: NODE.into(),
        contents,
    }));
    issues
}

fn contents() -> Vec<Contents> {
    vec![
        Contents::Gpt,
        Contents::Blank,
        Contents::Mbr(vec![MbrPartition {
            index: 0,
            kind: 0x83,
            bytes: GIB,
        }]),
        Contents::UnreadableGpt,
        Contents::Ext4,
        Contents::Data,
    ]
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
            model: None,
            state: DiskState::Refused(Refusal::Medium),
        },
        crate::Disk {
            node: "/dev/sdb".into(),
            model: Some("Ultra Fit".into()),
            state: DiskState::Refused(Refusal::ReadOnly),
        },
        crate::Disk {
            node: "/dev/sdc".into(),
            model: None,
            state: DiskState::Unreadable("/dev/sdc: I/O error".into()),
        },
        crate::Disk {
            node: NODE.into(),
            model: Some(MODEL.into()),
            state: listed(Contents::Gpt, Layout::of(NODE, &header, &array)),
        },
    ];
    for (n, contents) in contents().into_iter().skip(1).enumerate() {
        disks.push(crate::Disk {
            node: alloc::format!("/dev/vd{}", (b'a' + n as u8) as char),
            model: None,
            state: listed(contents, Layout::empty(16 * GIB)),
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

#[test]
fn the_fixed_words_keep_the_rules() {
    let mut said = Said::default();
    for text in say::LABELS {
        said.label(text);
    }
    for text in say::SENTENCES {
        said.sentence(text);
    }
    said.sentence(say::REMOTE_CAPTION);
    for kind in [Kind::Erase, Kind::Free, Kind::Reuse, Kind::Reinstall] {
        said.label(kind.title());
    }
    for refusal in [Refusal::Medium, Refusal::ReadOnly] {
        said.sentence(refusal.words());
    }
    for contents in contents() {
        said.sentence(contents.only_erase());
        said.caption(contents.holds().unwrap_or_default());
    }
    for step in 0..=STEPS.len() as u32 {
        said.sentence(
            Failure {
                step,
                message: String::new(),
            }
            .words(),
        );
    }
    for issue in issues() {
        let words = issue.words();
        assert!(
            words.matches(". ").count() >= 1,
            "{words:?} ends at the fact"
        );
        said.sentence(words);
        said.caption(issue.detail().unwrap_or_default());
    }
    said.sentence(MISMATCHED);
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
        let engine_said = matches!(
            issue,
            Issue::Medium(_)
                | Issue::Probe(_)
                | Issue::Disk(_)
                | Issue::Esp(_)
                | Issue::Person(_)
                | Issue::Menu(_)
        );
        if engine_said {
            continue;
        }
        let words = issue.words();
        let fact = words
            .split_inclusive(". ")
            .next()
            .unwrap_or(&words)
            .trim_end();
        assert!(cli.contains(fact), "{cli:?} does not say {fact:?}");
        assert!(cli.contains("--"), "{cli:?} names no flag to pass instead");
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
            Case::erasing(Some(foreign()), Contents::Gpt).proposal(),
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
    for failed in 0..=STEPS.len() as u32 {
        let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
        review(&mut w);
        reply(&mut w, Reply::Event(Event::step(failed.max(1))));
        said.page(&w);
        reply(
            &mut w,
            Reply::Failed(Failure {
                step: failed,
                message: "mke2fs exited 1".into(),
            }),
        );
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
