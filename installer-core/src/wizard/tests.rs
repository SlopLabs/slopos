use super::*;
use alloc::string::ToString;
use alloc::vec::Vec;

use slopos_boot_core::install::{Mode as PlanMode, PlanError};

use crate::fixtures::{
    Case, GIB, MODEL, NODE, foreign, installed, machine as system, no_room, opened,
};
use crate::{Contents, DiskState, Reinstall, Tag};

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
    assert_eq!(w.note(), Some(MISMATCHED));
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
    assert!(
        w.confirmation()
            .ends_with("nothing else on the disk changes")
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
    assert_eq!(w.primary_label(), say::RESTART);
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
    assert!(w.confirmation().contains(Tag::Format.word()));
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
    assert_eq!(w.primary_label(), "Try again");
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
        (true, Kind::Reinstall.title(), "kept"),
        (false, "Erase and reinstall SlopOS", Tag::Format.word()),
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
        assert!(
            w.confirmation().contains(confirmation),
            "{}",
            w.confirmation()
        );
    }
}

#[test]
fn an_answer_that_changes_nothing_proposes_nothing() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    assert!(w.on(Input::Kind(Kind::Free)).is_empty());
    assert!(w.on(Input::First(false)).is_empty());
    assert!(
        w.on(Input::Root(3)).is_empty(),
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
    assert_eq!(w.primary_label(), Kind::Reinstall.title());
    assert!(w.confirmation().contains("kept"));
}

#[test]
fn erasing_names_the_disk() {
    let (mut w, _) = at_how(
        opened(Err(no_room()), None),
        Case::erasing(Some(foreign()), Contents::Gpt).proposal(),
    );
    w.on(Input::Next);
    assert_eq!(w.primary_label(), Kind::Erase.title());
    assert!(w.confirmation().contains("everything on this disk"));
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

#[test]
fn the_disks_offered_come_first_each_named_by_its_model() {
    let w = at_disk();
    let rows = w.disks();
    let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, [NODE, "/dev/vda", "/dev/sda"]);
    assert_eq!(rows[0].title, alloc::format!("{MODEL} · 17.2 GB"));
    assert!(rows[0].detail.starts_with("nvme0n1 · 3 partitions, "));
    assert_eq!(rows[1].title, "17.2 GB disk");
    assert_eq!(
        (rows[1].detail.as_str(), rows[1].enabled()),
        ("vda · Empty", true)
    );
    assert_eq!(
        rows[2].reason.as_deref(),
        Some(crate::Refusal::Medium.words())
    );
}

#[test]
fn a_tile_says_what_it_does_or_why_not_without_a_flag() {
    let (w, _) = at_how(
        opened(Err(no_room()), None),
        Case::erasing(Some(foreign()), Contents::Gpt).proposal(),
    );
    let rows = w.tile_rows();
    let open: Vec<(Kind, bool)> = rows.iter().map(|r| (r.key, r.enabled())).collect();
    assert_eq!(
        open,
        [
            (Kind::Erase, true),
            (Kind::Free, false),
            (Kind::Reuse, true)
        ]
    );
    assert_eq!(rows[1].reason, Some(no_room().words()));
    for issue in [
        Issue::OnlyErase {
            node: NODE.into(),
            contents: Contents::Blank,
        },
        Issue::NothingToKeep("/dev/nvme0n1p3".into()),
        Issue::RootSize,
        Issue::Remote("x\n".into()),
        Issue::Plan {
            error: PlanError::Installed(3),
            node: NODE.into(),
            root: Some("/dev/nvme0n1p3".into()),
        },
        Issue::Plan {
            error: PlanError::RootElsewhere(3),
            node: NODE.into(),
            root: None,
        },
    ] {
        assert!(!issue.words().contains("--"), "{}", issue.words());
        assert!(issue.to_string().contains("--"), "{issue}");
    }
}

#[test]
fn a_reinstall_tile_says_whether_it_keeps_the_root() {
    let mut said = Vec::new();
    for keep in [true, false] {
        let mut case = Case::on(
            installed(),
            PlanMode::Reuse {
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
        let (w, _) = at_how(opened(Err(no_room()), reinstall), case.proposal());
        let rows = w.tile_rows();
        let tile = rows.iter().find(|r| r.key == Kind::Reinstall).unwrap();
        said.push(tile.detail.clone());
    }
    assert!(
        said[0].contains("kept") && said[1].contains("erased"),
        "{said:?}"
    );
}

#[test]
fn every_free_space_is_listed_once_two_take_the_install() {
    let mut w = at_disk();
    sent(w.on(Input::Next));
    reply(&mut w, Reply::System(system(Some(opened(Ok(()), None)))));
    reply(
        &mut w,
        Reply::Proposal {
            id: 1,
            proposal: free_proposal(),
        },
    );
    let rows = w.region_rows();
    let said: Vec<(usize, &str, bool)> = rows
        .iter()
        .map(|r| (r.key, r.title.as_str(), r.enabled()))
        .collect();
    assert_eq!(
        said,
        [
            (0, "2 GB of free space", true),
            (1, "4.3 GB of free space", true),
            (2, "402 MB of free space", false),
        ]
    );
    assert_eq!(rows[0].detail, "Starts at 106 MB");
    assert_eq!(rows[2].reason, Some(no_room().words()));
    sent(w.on(Input::Kind(Kind::Erase)));
    assert!(w.region_rows().is_empty(), "only a free-space install");
}

#[test]
fn a_partition_is_listed_by_its_name_and_size() {
    let mut w = at_disk();
    sent(w.on(Input::Next));
    reply(&mut w, Reply::System(system(Some(opened(Ok(()), None)))));
    reply(
        &mut w,
        Reply::Proposal {
            id: 1,
            proposal: free_proposal(),
        },
    );
    let rows = w.root_rows();
    let said: Vec<(u32, &str, &str, bool)> = rows
        .iter()
        .map(|r| (r.key, r.title.as_str(), r.detail.as_str(), r.enabled()))
        .collect();
    assert_eq!(
        said,
        [
            (
                1,
                "EFI system partition · 105 MB",
                "nvme0n1p1 · EFI system",
                false
            ),
            (2, "theirs · 2.1 GB", "nvme0n1p2 · Linux filesystem", true),
            (3, "theirs · 8.2 GB", "nvme0n1p3 · Linux filesystem", true),
        ]
    );
}

#[test]
fn the_slider_holds_the_size_asked_within_the_engine_s_span() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    let span = free_proposal().root;
    assert_eq!(w.root_span(), Some(span));
    assert_eq!(w.root_size(), Some(span.max), "all the region gives");
    sent(w.on(Input::RootSize(Some(span.min - 1))));
    assert_eq!(w.root_span(), Some(span), "the last answer, meanwhile");
    assert_eq!(w.root_size(), Some(span.min));
    reply(
        &mut w,
        Reply::Proposal {
            id: 8,
            proposal: free_proposal(),
        },
    );
    sent(w.on(Input::Kind(Kind::Reuse)));
    assert_eq!(
        w.root_span(),
        None,
        "a reused root has its partition's size"
    );
    assert_eq!(w.root_size(), None);
}

#[test]
fn the_review_summarises_the_proposal() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    sent(w.on(Input::First(true)));
    assert!(
        w.summary_lines().is_empty(),
        "no proposal for the choices yet"
    );
    reply(
        &mut w,
        Reply::Proposal {
            id: 8,
            proposal: free_proposal(),
        },
    );
    w.on(Input::Next);
    let lines = w.summary_lines();
    let subjects: Vec<&str> = lines.iter().map(|(s, _)| *s).collect();
    assert_eq!(
        subjects,
        [
            "Disk",
            "Method",
            "Partition",
            "Developer tools",
            "Source code",
            "First account",
            "Startup order",
            "Startup menu"
        ]
    );
    assert!(lines[0].1.starts_with(MODEL) && lines[0].1.ends_with("(nvme0n1)"));
    assert_eq!(lines[1].1, Kind::Free.title());
    assert!(lines[2].1.ends_with("(nvme0n1p4)"), "{}", lines[2].1);
    assert!(lines[5].1.starts_with(crate::FIRST_PERSON));
    assert_eq!(lines[7].1, "SlopOS, Other OS, Elsewhere");
}

#[test]
fn each_action_is_listed_with_its_tag() {
    let lines = free_proposal().action_lines();
    let tags: Vec<&str> = lines.iter().map(|(tag, _)| tag.word()).collect();
    assert_eq!(tags, ["Shared", "New", "New", "New", "Kept"]);
    assert_eq!(lines[0].1, "EFI system · nvme0n1p1 · 105 MB");
    assert_eq!(lines[4].1, "Linux filesystem · nvme0n1p2 · 537 MB");
    let erased = Case::erasing(Some(foreign()), Contents::Gpt).proposal();
    assert_eq!(
        erased.action_lines()[0],
        (Tag::Erase, "Everything on nvme0n1".into())
    );
}

#[test]
fn every_log_line_is_kept_across_steps() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    review(&mut w);
    reply(&mut w, Reply::Event(Event::step(3)));
    reply(&mut w, Reply::Event(Event::Log { line: "a".into() }));
    reply(&mut w, Reply::Event(Event::Log { line: "b".into() }));
    reply(&mut w, Reply::Event(Event::step(4)));
    assert_eq!(w.progress().log, ["a", "b"]);
    assert!(
        w.heading(false)
            .subtitle
            .starts_with(&alloc::format!("On {MODEL}"))
    );
}

#[test]
fn the_heading_says_where_and_how_the_install_ended() {
    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    review(&mut w);
    reply(
        &mut w,
        Reply::Failed(Failure {
            step: 4,
            message: "copying".into(),
        }),
    );
    assert!(
        w.heading(false)
            .subtitle
            .starts_with("The installer stopped while copying the system.")
    );

    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    review(&mut w);
    reply(
        &mut w,
        Reply::Failed(Failure {
            step: 0,
            message: "/dev/nvme0n1 changed since it was reviewed; nothing was written".into(),
        }),
    );
    assert!(
        w.heading(false)
            .subtitle
            .contains("before writing anything")
    );

    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    review(&mut w);
    w.on(Input::Ended);
    assert_eq!(w.failure().map(|f| f.message.as_str()), Some(STOPPED));

    let (mut w, _) = at_how(opened(Ok(()), None), free_proposal());
    review(&mut w);
    reply(
        &mut w,
        Reply::Done(Summary {
            disk: NODE.into(),
            entry: 0x1a,
        }),
    );
    assert_eq!(
        w.done_lines(),
        [
            ("Disk", "nvme0n1".into()),
            ("Startup entry", "Boot001A".into())
        ]
    );
    assert!(
        w.notes().is_empty(),
        "the installer carried the developer tools"
    );
}

#[test]
fn an_installer_without_developer_tools_says_so_once_probed() {
    let probed = |unread: Option<&str>| {
        let mut w = Wizard::new();
        assert!(w.notes().is_empty());
        w.on(Input::Next);
        sent(w.on(Input::Started));
        reply(&mut w, Reply::Hello { version: VERSION });
        let mut machine = system(None);
        machine.medium.toolchain = false;
        machine.medium.unmounted_payload = unread.map(Into::into);
        reply(&mut w, Reply::System(machine));
        w.notes()
    };
    let lacking = probed(None);
    assert_eq!(lacking.len(), 1);
    assert_eq!(lacking[0].severity, Severity::Info);
    let unread = probed(Some("/dev/sdb"));
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].severity, Severity::Warning);
    assert!(unread[0].body.contains("sdb"));
}

mod copy;
