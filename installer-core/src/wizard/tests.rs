use super::*;
use alloc::vec;
use alloc::vec::Vec;

use slopos_boot_core::install::{Mode as PlanMode, PlanError};

use crate::tests::{Case, GIB, foreign, installed};
use crate::{Contents, Disk, DiskState, Layout, Listing, Medium, Reinstall, Tag};

const NODE: &str = "/dev/nvme0n1";

fn no_room() -> Issue {
    Issue::Plan {
        error: PlanError::NoRoom {
            needed: 8 * GIB,
            available: GIB,
        },
        node: NODE.into(),
        root: None,
    }
}

fn opened(free: Fit, reinstall: Option<Reinstall>) -> Opened {
    Opened {
        erase: Ok(()),
        free: free.clone(),
        largest: Some(1),
        regions: vec![free.clone(), free, Err(no_room())],
        reuse: Ok(()),
        roots: vec![(1, Err(no_room())), (2, Ok(())), (4, Ok(()))],
        reinstall,
    }
}

fn system(opened: Option<Opened>) -> System {
    let (header, array) = foreign();
    System {
        medium: Medium {
            toolchain: true,
            source: true,
            payload: Some(GIB),
            root_min: 7 * GIB,
            unmounted_payload: None,
        },
        disks: vec![
            Disk {
                node: "/dev/sda".into(),
                state: DiskState::Refused("the install medium".into()),
            },
            Disk {
                node: NODE.into(),
                state: DiskState::Listed(Listing {
                    capacity: 16 * GIB,
                    block: 512,
                    contents: Contents::Gpt,
                    layout: Layout::of(NODE, &header, &array),
                    opened,
                }),
            },
            Disk {
                node: "/dev/vda".into(),
                state: DiskState::Listed(Listing {
                    capacity: 16 * GIB,
                    block: 512,
                    contents: Contents::Blank,
                    layout: Layout::empty(16 * GIB),
                    opened: None,
                }),
            },
        ],
        others: Ok(vec!["Other OS".into()]),
    }
}

fn sent(out: Vec<Out>) -> Vec<Request> {
    out.into_iter()
        .map(|out| match out {
            Out::Send(request) => request,
            other => panic!("asked for {other:?}, not a frame"),
        })
        .collect()
}

fn reply(w: &mut Wizard, reply: Reply) -> Vec<Request> {
    sent(w.on(Input::Reply(reply)))
}

fn free_proposal() -> Proposal {
    Case::on(foreign(), PlanMode::Free { region: None }).proposal()
}

/// A wizard on the Disk page, the engine started and the machine probed.
fn at_disk() -> Wizard {
    let mut w = Wizard::new();
    assert_eq!(w.page(), Page::Welcome);
    assert!(w.can_advance());
    assert_eq!(w.on(Input::Next), [Out::Start]);
    assert!(w.waiting() && !w.can_advance());
    assert!(w.on(Input::Next).is_empty(), "one start at a time");
    assert_eq!(
        sent(w.on(Input::Started)),
        [Request::Hello { version: VERSION }, Request::Probe]
    );
    assert!(reply(&mut w, Reply::Hello { version: VERSION }).is_empty());
    assert!(reply(&mut w, Reply::System(system(None))).is_empty());
    assert_eq!(w.page(), Page::Disk);
    w
}

/// A wizard on the How page for [`NODE`], answered with `proposal` for the
/// choices it proposed first.
fn at_how(open: Opened, proposal: Proposal) -> (Wizard, Choices) {
    let mut w = at_disk();
    assert_eq!(w.disk(), Some(NODE), "the first disk offered");
    assert_eq!(
        sent(w.on(Input::Next)),
        [Request::Open { disk: NODE.into() }]
    );
    let proposed = reply(&mut w, Reply::System(system(Some(open))));
    let [Request::Propose(choices)] = proposed.as_slice() else {
        panic!("opening proposes, not {proposed:?}");
    };
    assert_eq!(w.page(), Page::How);
    assert!(!w.can_advance(), "the proposal is not back yet");
    assert!(
        reply(
            &mut w,
            Reply::Proposal {
                id: 7,
                proposal: proposal.clone()
            }
        )
        .is_empty()
    );
    assert_eq!(w.proposal(), Some(&proposal));
    (w, choices.clone())
}

#[test]
fn probing_offers_the_first_disk_that_is_listed() {
    let mut w = at_disk();
    assert_eq!(w.disk(), Some(NODE));
    assert!(w.can_advance());
    assert!(w.can_go_back());
    w.on(Input::Disk("/dev/sda".into()));
    assert_eq!(w.disk(), Some(NODE), "a refused disk is not chosen");
    w.on(Input::Disk("/dev/vda".into()));
    assert_eq!(w.disk(), Some("/dev/vda"));
    assert_eq!(sent(w.on(Input::Rescan)), [Request::Probe]);
    assert!(!w.can_advance(), "waiting for the probe");
    reply(&mut w, Reply::System(system(None)));
    assert_eq!(w.disk(), Some("/dev/vda"), "a rescan keeps the disk chosen");
}

#[test]
fn a_refusal_or_another_protocol_leaves_the_person_at_welcome() {
    let mut w = Wizard::new();
    assert_eq!(w.on(Input::Next), [Out::Start]);
    w.on(Input::Refused("privd refused org.slopos.install".into()));
    assert_eq!(w.page(), Page::Welcome);
    assert_eq!(w.note(), Some("privd refused org.slopos.install"));
    assert_eq!(w.on(Input::Next), [Out::Start]);
    assert_eq!(w.note(), None);
    sent(w.on(Input::Started));
    reply(
        &mut w,
        Reply::Hello {
            version: VERSION + 1,
        },
    );
    assert_eq!(w.page(), Page::Welcome);
    assert!(w.note().is_some_and(|n| n.contains("version 2")));
    assert!(w.can_advance(), "nothing is owed once the versions differ");
    w.on(Input::Ended);
    assert_eq!(w.on(Input::Next), [Out::Start]);
}

#[test]
fn a_disk_the_engine_will_not_open_keeps_the_person_on_the_disk_page() {
    let mut w = at_disk();
    sent(w.on(Input::Next));
    let busy = Issue::Disk("something on /dev/nvme0n1 is mounted or in use".into());
    reply(&mut w, Reply::Issue(busy.clone()));
    assert_eq!(w.page(), Page::Disk);
    assert_eq!(w.issue(), Some(&busy));
    assert!(w.can_advance(), "it may be tried again");
}

#[test]
fn opening_chooses_free_space_where_it_fits_and_every_change_proposes() {
    let (mut w, first) = at_how(opened(Ok(()), None), free_proposal());
    assert_eq!(first.mode, Mode::Free { region: None });
    assert_eq!(first.remote, None);
    assert!(w.can_advance());
    assert_eq!(w.regions(), [0, 1], "two regions fit");
    let kinds: Vec<Kind> = w.tiles().iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, [Kind::Erase, Kind::Free, Kind::Reuse]);

    let sized = sent(w.on(Input::RootSize(Some(4 * GIB))));
    let [Request::Propose(sized)] = sized.as_slice() else {
        panic!("{sized:?}");
    };
    assert_eq!(sized.root_size, Some(4 * GIB));
    assert!(!w.can_advance(), "the proposal on screen is stale");
    assert!(
        w.on(Input::Region(1)).is_empty(),
        "one proposal in flight at a time"
    );
    assert!(
        w.on(Input::Region(2)).is_empty(),
        "a region that does not fit"
    );
    let again = reply(
        &mut w,
        Reply::Proposal {
            id: 8,
            proposal: free_proposal(),
        },
    );
    let [Request::Propose(again)] = again.as_slice() else {
        panic!("a stale answer proposes the current choices: {again:?}");
    };
    assert_eq!(again.mode, Mode::Free { region: Some(1) });
    assert_eq!(again.root_size, Some(4 * GIB));
    assert_eq!(w.proposal(), None);
    reply(
        &mut w,
        Reply::Proposal {
            id: 9,
            proposal: free_proposal(),
        },
    );
    assert!(w.can_advance());

    let remote = sent(w.on(Input::Remote("https://git.example/x".into())));
    assert!(
        matches!(&remote[..], [Request::Propose(c)] if c.remote.as_deref() == Some("https://git.example/x"))
    );
    reply(&mut w, Reply::Issue(Issue::Remote("x".into())));
    assert_eq!(w.issue(), Some(&Issue::Remote("x".into())));
    assert!(!w.can_advance(), "an issue blocks the review");
    let cleared = sent(w.on(Input::Remote(String::new())));
    assert!(matches!(&cleared[..], [Request::Propose(c)] if c.remote.is_none()));
}

#[test]
fn a_tile_whose_choice_does_not_fit_cannot_be_chosen() {
    let (mut w, first) = at_how(
        opened(Err(no_room()), None),
        Case::erasing(Some(foreign()), Contents::Gpt).proposal(),
    );
    assert_eq!(
        first.mode,
        Mode::Erase,
        "erase when free space does not fit"
    );
    assert!(w.on(Input::Kind(Kind::Free)).is_empty());
    assert_eq!(w.answers().map(|a| a.kind), Some(Kind::Erase));
    assert!(w.regions().is_empty());
    assert!(
        w.tiles()
            .iter()
            .any(|(kind, fit)| *kind == Kind::Free && **fit == Err(no_room()))
    );
    assert!(w.on(Input::RootSize(Some(GIB))).len() == 1);
    let reuse = sent(w.on(Input::Kind(Kind::Reuse)));
    assert!(reuse.is_empty(), "a proposal is in flight");
    reply(
        &mut w,
        Reply::Proposal {
            id: 1,
            proposal: free_proposal(),
        },
    );
    let choices = w.choices().unwrap();
    assert_eq!(
        choices.mode,
        Mode::Reuse {
            root: 2,
            region: None,
            keep: false
        },
        "the first partition that fits"
    );
    assert_eq!(choices.root_size, None, "a reused root has its own size");
    assert!(w.on(Input::Root(1)).is_empty(), "a root that does not fit");
}

#[test]
fn back_keeps_every_answer_and_the_review_asks_again() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    sent(w.on(Input::First(true)));
    reply(
        &mut w,
        Reply::Proposal {
            id: 2,
            proposal: free_proposal(),
        },
    );
    assert!(w.on(Input::Next).is_empty());
    assert_eq!(w.page(), Page::Review);
    assert_eq!(
        w.confirmation(),
        "I understand that new partitions will be created in free space on nvme0n1, and nothing else on it changes"
    );
    assert_eq!(w.primary_label(), "Install beside Other OS");
    assert!(!w.can_advance(), "not until the box is ticked");
    w.on(Input::Confirm(true));
    assert!(w.can_advance());
    w.on(Input::Back);
    assert_eq!(w.page(), Page::How);
    w.on(Input::Back);
    assert_eq!(w.page(), Page::Disk);
    let open = sent(w.on(Input::Next));
    assert_eq!(open, [Request::Open { disk: NODE.into() }]);
    let proposed = reply(&mut w, Reply::System(system(Some(opened(Ok(()), None)))));
    assert!(
        matches!(&proposed[..], [Request::Propose(c)] if c.first && c.mode == Mode::Free { region: None })
    );
    reply(
        &mut w,
        Reply::Proposal {
            id: 3,
            proposal: free_proposal(),
        },
    );
    w.on(Input::Next);
    assert_eq!(w.page(), Page::Review);
    assert!(!w.confirmed(), "the box is ticked again for each review");
}

#[test]
fn another_disk_starts_from_its_own_defaults_but_keeps_the_remote() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    sent(w.on(Input::Remote("https://git.example/x".into())));
    reply(
        &mut w,
        Reply::Proposal {
            id: 2,
            proposal: free_proposal(),
        },
    );
    sent(w.on(Input::RootSize(Some(4 * GIB))));
    reply(
        &mut w,
        Reply::Proposal {
            id: 3,
            proposal: free_proposal(),
        },
    );
    w.on(Input::Back);
    w.on(Input::Disk("/dev/vda".into()));
    sent(w.on(Input::Next));
    let mut machine = system(None);
    let DiskState::Listed(listing) = &mut machine.disks[2].state else {
        unreachable!()
    };
    listing.opened = Some(opened(Ok(()), None));
    let proposed = reply(&mut w, Reply::System(machine));
    let [Request::Propose(choices)] = proposed.as_slice() else {
        panic!("{proposed:?}");
    };
    assert_eq!(choices.disk, "/dev/vda");
    assert_eq!(choices.root_size, None);
    assert_eq!(choices.remote.as_deref(), Some("https://git.example/x"));
}

fn review(w: &mut Wizard) -> (Choices, u64) {
    assert!(w.on(Input::Next).is_empty());
    w.on(Input::Confirm(true));
    let install = sent(w.on(Input::Next));
    let [Request::Install { choices, proposal }] = install.as_slice() else {
        panic!("{install:?}");
    };
    assert_eq!(w.page(), Page::Install);
    (choices.clone(), *proposal)
}

#[test]
fn an_install_shows_each_step_and_refuses_to_close() {
    let (mut w, first) = at_how(opened(Ok(()), None), free_proposal());
    let (choices, id) = review(&mut w);
    assert_eq!((choices, id), (first, 7));
    assert!(!w.can_close() && !w.can_advance() && !w.can_go_back());
    w.on(Input::Back);
    assert_eq!(w.page(), Page::Install);
    reply(&mut w, Reply::Event(Event::step(1)));
    reply(&mut w, Reply::Event(Event::step(4)));
    reply(&mut w, Reply::Event(Event::Progress { done: 5, total: 10 }));
    reply(&mut w, Reply::Event(Event::Log { line: "x".into() }));
    let states: Vec<StepState> = w.steps().iter().map(|(_, s)| *s).collect();
    assert_eq!(
        states[..5],
        [
            StepState::Done,
            StepState::Done,
            StepState::Done,
            StepState::Running,
            StepState::Pending
        ]
    );
    assert_eq!(w.progress().bytes, Some((5, 10)));
    assert_eq!(w.progress().log, ["x"]);
    reply(&mut w, Reply::Event(Event::step(5)));
    assert_eq!(w.progress().bytes, None, "a step's copy is its own");
    let summary = Summary {
        disk: NODE.into(),
        entry: 3,
    };
    reply(&mut w, Reply::Done(summary.clone()));
    assert_eq!(w.page(), Page::Done);
    assert_eq!(w.summary(), Some(&summary));
    assert!(w.can_close());
    assert_eq!(w.primary_label(), "Restart now");
    assert!(w.steps().iter().all(|(_, s)| *s == StepState::Done));
    assert_eq!(w.on(Input::Next), [Out::Restart]);
    assert_eq!(w.on(Input::Close), [Out::Close]);
}

#[test]
fn the_region_shown_is_the_one_the_engine_takes() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    assert_eq!(w.region(), Some(1), "the largest, as the engine says");
    sent(w.on(Input::Region(0)));
    assert_eq!(w.region(), Some(0));
    reply(
        &mut w,
        Reply::Proposal {
            id: 8,
            proposal: free_proposal(),
        },
    );
    sent(w.on(Input::Kind(Kind::Erase)));
    assert_eq!(w.region(), None);
}

#[test]
fn an_install_answered_out_of_turn_ends_on_the_error_page() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    review(&mut w);
    reply(&mut w, Reply::Event(Event::step(2)));
    reply(&mut w, Reply::Issue(Issue::RootSize));
    assert_eq!(w.page(), Page::Error);
    assert_eq!(w.failure().map(|f| f.step), Some(2));
    assert!(w.can_close());
}

#[test]
fn free_space_that_formats_an_empty_esp_says_so() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    let mut proposal = free_proposal();
    proposal.actions[0].tag = Tag::Format;
    sent(w.on(Input::First(true)));
    reply(&mut w, Reply::Proposal { id: 8, proposal });
    w.on(Input::Next);
    assert_eq!(
        w.confirmation(),
        "I understand that new partitions will be created in free space on nvme0n1, and what the review says is formatted"
    );
}

#[test]
fn nothing_closes_the_window_while_it_installs() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    assert_eq!(w.on(Input::Close), [Out::Close], "until Install");
    review(&mut w);
    assert!(w.on(Input::Close).is_empty());
    assert!(w.on(Input::Restart).is_empty());
    reply(
        &mut w,
        Reply::Failed(Failure {
            step: 2,
            message: "x".into(),
        }),
    );
    assert_eq!(w.on(Input::Restart), [Out::Restart]);
    assert_eq!(w.on(Input::Close), [Out::Close]);
}

#[test]
fn the_last_answer_stays_shown_while_a_change_is_proposed() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    sent(w.on(Input::RootSize(Some(4 * GIB))));
    assert_eq!(w.proposal(), None);
    assert_eq!(
        w.last_answer().map(|p| &p.answer),
        Some(&Ok((7, free_proposal())))
    );
    reply(
        &mut w,
        Reply::Proposal {
            id: 8,
            proposal: free_proposal(),
        },
    );
    let all = sent(w.on(Input::RootSize(None)));
    assert!(matches!(&all[..], [Request::Propose(c)] if c.root_size.is_none()));
}

#[test]
fn back_to_review_opens_the_disk_again_and_proposes_the_same_choices() {
    let (mut w, first) = at_how(opened(Ok(()), None), free_proposal());
    review(&mut w);
    reply(&mut w, Reply::Event(Event::step(3)));
    reply(
        &mut w,
        Reply::Failed(Failure {
            step: 3,
            message: "mke2fs exited".into(),
        }),
    );
    assert_eq!(w.page(), Page::Error);
    assert!(w.can_close());
    assert_eq!(w.steps()[2].1, StepState::Failed);
    assert_eq!(w.primary_label(), "Back to review");
    assert_eq!(sent(w.on(Input::Next)), [Request::Probe]);
    let open = reply(&mut w, Reply::System(system(None)));
    assert_eq!(open, [Request::Open { disk: NODE.into() }]);
    let proposed = reply(&mut w, Reply::System(system(Some(opened(Ok(()), None)))));
    assert_eq!(proposed, [Request::Propose(first)]);
    reply(
        &mut w,
        Reply::Proposal {
            id: 11,
            proposal: free_proposal(),
        },
    );
    assert_eq!(w.page(), Page::Review);
    assert!(!w.confirmed());
}

/// A free-space install cut short leaves SlopOS's partitions: the disk is
/// offered as a reinstall, keeping the root once it holds a volume.
#[test]
fn a_free_install_cut_short_is_offered_as_a_reinstall() {
    for (keep, label, confirmation) in [
        (
            true,
            "Reinstall SlopOS",
            "I understand that SlopOS on nvme0n1 will be replaced, keeping what nvme0n1p3 holds",
        ),
        (
            false,
            "Format nvme0n1p3 and reinstall SlopOS",
            "I understand that nvme0n1p3 will be formatted",
        ),
    ] {
        let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
        review(&mut w);
        reply(
            &mut w,
            Reply::Failed(Failure {
                step: 4,
                message: "copying".into(),
            }),
        );
        w.on(Input::Next);
        assert!(w.failure().is_some(), "the failure stays until the review");
        reply(&mut w, Reply::System(system(None)));
        let installed_now = opened(
            Err(Issue::Plan {
                error: PlanError::Installed(3),
                node: NODE.into(),
                root: None,
            }),
            Some(Reinstall {
                root: 3,
                keep,
                fit: Ok(()),
            }),
        );
        let proposed = reply(&mut w, Reply::System(system(Some(installed_now))));
        assert!(matches!(
            &proposed[..],
            [Request::Propose(c)] if c.mode == Mode::Reuse { root: 3, region: None, keep }
        ));
        assert_eq!(w.page(), Page::How, "the person sees what changed");
        let mut reinstall = Case::on(
            installed(),
            PlanMode::Reuse {
                root: 3,
                region: None,
            },
        );
        reinstall.keep_root = keep;
        reply(
            &mut w,
            Reply::Proposal {
                id: 12,
                proposal: reinstall.proposal(),
            },
        );
        assert_eq!(w.page(), Page::How);
        w.on(Input::Next);
        assert_eq!(w.primary_label(), label);
        assert_eq!(w.confirmation(), confirmation);
    }
}

#[test]
fn an_answer_that_changes_nothing_proposes_nothing() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    assert!(w.on(Input::Kind(Kind::Free)).is_empty());
    assert!(w.on(Input::First(false)).is_empty());
    assert!(
        w.on(Input::Root(4)).is_empty(),
        "a root matters only to a reuse"
    );
    assert_eq!(sent(w.on(Input::Region(0))).len(), 1);
}

#[test]
fn back_from_the_disk_page_is_the_welcome_page() {
    let mut w = at_disk();
    assert!(w.can_go_back());
    w.on(Input::Back);
    assert_eq!(w.page(), Page::Welcome);
    assert_eq!(sent(w.on(Input::Next)), [Request::Probe], "the engine runs");
}

#[test]
fn an_engine_that_stops_during_back_to_review_says_so() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    review(&mut w);
    reply(
        &mut w,
        Reply::Failed(Failure {
            step: 1,
            message: "x".into(),
        }),
    );
    sent(w.on(Input::Next));
    w.on(Input::Ended);
    assert_eq!(w.page(), Page::Error);
    assert!(w.note().is_some());
    assert_eq!(w.failure().map(|f| f.step), Some(1));
}

#[test]
fn a_reinstall_that_keeps_the_root_says_so() {
    let mut kept = Case::on(
        installed(),
        PlanMode::Reuse {
            root: 3,
            region: None,
        },
    );
    kept.keep_root = true;
    let reinstall = Some(Reinstall {
        root: 3,
        keep: true,
        fit: Ok(()),
    });
    let (mut w, first) = at_how(opened(Err(no_room()), reinstall), kept.proposal());
    assert_eq!(
        first.mode,
        Mode::Reuse {
            root: 3,
            region: None,
            keep: true
        }
    );
    w.on(Input::Next);
    assert_eq!(w.primary_label(), "Reinstall SlopOS");
    assert_eq!(
        w.confirmation(),
        "I understand that SlopOS on nvme0n1 will be replaced, keeping what nvme0n1p3 holds"
    );
}

#[test]
fn erasing_names_the_disk() {
    let (mut w, _) = at_how(
        opened(Err(no_room()), None),
        Case::erasing(Some(foreign()), Contents::Gpt).proposal(),
    );
    w.on(Input::Next);
    assert_eq!(w.primary_label(), "Erase nvme0n1 and install");
    assert_eq!(
        w.confirmation(),
        "I understand that everything on nvme0n1 will be erased"
    );
}

#[test]
fn an_engine_that_stops_ends_the_install_as_a_failure() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    review(&mut w);
    reply(&mut w, Reply::Event(Event::step(6)));
    w.on(Input::Ended);
    assert_eq!(w.page(), Page::Error);
    assert_eq!(w.failure().map(|f| f.step), Some(6));
    assert_eq!(
        w.on(Input::Next),
        [Out::Start],
        "back to review starts a new engine"
    );
    let resumed = sent(w.on(Input::Started));
    assert_eq!(
        resumed,
        [Request::Hello { version: VERSION }, Request::Probe]
    );
    reply(&mut w, Reply::Hello { version: VERSION });
    let open = reply(&mut w, Reply::System(system(None)));
    assert_eq!(open, [Request::Open { disk: NODE.into() }]);
}

#[test]
fn an_engine_that_stops_before_an_install_returns_to_welcome() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    w.on(Input::Ended);
    assert_eq!(w.page(), Page::Welcome);
    assert!(w.note().is_some());
    assert_eq!(w.on(Input::Next), [Out::Start]);
}
