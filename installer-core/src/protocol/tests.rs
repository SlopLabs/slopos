use super::*;
use alloc::vec;
use alloc::vec::Vec;

use slopos_boot_core::install::{Mode as PlanMode, PlanError};

use crate::tests::{Case, GIB, foreign};
use crate::{Opened, Reinstall};

fn payload(frame: &[u8]) -> (u8, &[u8]) {
    (frame[0], &frame[slopos_remote_core::HEADER_LEN..])
}

fn choices() -> Choices {
    Choices {
        disk: "/dev/nvme0n1".into(),
        mode: Mode::Reuse {
            root: 3,
            region: Some(1),
            keep: true,
        },
        root_size: Some(9 * GIB),
        remote: Some("https://git.example/slopos.git".into()),
        first: true,
        person: Some(Person {
            name: "ada".into(),
            password: Some("$argon2id$v=19$m=65536,t=3,p=4$c2FsdA$aGFzaA".into()),
        }),
        cmdline: Some("tests=on panic=reboot".into()),
    }
}

fn issues() -> Vec<Issue> {
    let plan = |error| Issue::Plan {
        error,
        node: "/dev/vda".into(),
        root: None,
    };
    vec![
        Issue::Medium("no install medium at /media/install".into()),
        Issue::Probe("/dev: gone".into()),
        Issue::Disk("".into()),
        Issue::OnlyErase {
            node: "/dev/vda".into(),
            holds: "nothing".into(),
        },
        Issue::NothingToKeep("/dev/vda3".into()),
        Issue::Esp("full".into()),
        Issue::Person("bad".into()),
        Issue::Menu("no".into()),
        Issue::RootSize,
        Issue::Remote("a\nb".into()),
        Issue::Plan {
            error: PlanError::Installed(3),
            node: "/dev/vda".into(),
            root: Some("/dev/vda3".into()),
        },
        plan(PlanError::NoRegion),
        plan(PlanError::NoRoom {
            needed: 7,
            available: 6,
        }),
        plan(PlanError::NoPartition(9)),
        plan(PlanError::NotForRoot(1)),
        plan(PlanError::Ambiguous(Role::Boot)),
        plan(PlanError::RootTooSmall {
            needed: 2,
            available: 1,
        }),
        plan(PlanError::RootTooLarge {
            limit: 2,
            available: 3,
        }),
        plan(PlanError::RootElsewhere(2)),
        plan(PlanError::NoSlot),
    ]
}

fn system() -> System {
    let (header, array) = foreign();
    let layout = Layout::of("/dev/nvme0n1", &header, &array);
    System {
        medium: Medium {
            toolchain: true,
            source: true,
            payload: Some(3 * GIB),
            root_min: 9 * GIB,
            unmounted_payload: Some("/dev/sdb".into()),
        },
        disks: vec![
            Disk {
                node: "/dev/sda".into(),
                state: DiskState::Refused("the install medium".into()),
            },
            Disk {
                node: "/dev/sdc".into(),
                state: DiskState::Unreadable("/dev/sdc: I/O error".into()),
            },
            Disk {
                node: "/dev/vda".into(),
                state: DiskState::Listed(Listing {
                    capacity: 4 * GIB,
                    block: 4096,
                    contents: Contents::Mbr(vec![
                        MbrPartition {
                            index: 0,
                            kind: 0x83,
                            bytes: GIB,
                        },
                        MbrPartition {
                            index: 3,
                            kind: 0x07,
                            bytes: 2 * GIB,
                        },
                    ]),
                    layout: Layout::empty(4 * GIB),
                    opened: None,
                }),
            },
            Disk {
                node: "/dev/nvme0n1".into(),
                state: DiskState::Listed(Listing {
                    capacity: 16 * GIB,
                    block: 512,
                    contents: Contents::Gpt,
                    layout,
                    opened: Some(Opened {
                        erase: Ok(()),
                        free: Ok(()),
                        largest: Some(1),
                        regions: vec![Ok(()), Err(issues()[12].clone())],
                        reuse: Err(issues()[13].clone()),
                        roots: vec![(2, Err(issues()[16].clone())), (5, Ok(()))],
                        reinstall: Some(Reinstall {
                            root: 5,
                            keep: true,
                            fit: Ok(()),
                        }),
                    }),
                }),
            },
        ],
        others: Ok(vec!["Other OS".into(), "Windows Boot Manager".into()]),
    }
}

fn proposals() -> Vec<Proposal> {
    let mut sized = Case::on(foreign(), PlanMode::Free { region: Some(0) });
    sized.size.target = Some(4 * GIB);
    let mut kept = sized.proposal();
    kept.setup.person = None;
    kept.setup.remote = None;
    kept.setup.left_out = vec!["Other OS".into()];
    kept.setup.removed = vec!["Windows Boot Manager".into()];
    vec![
        sized.proposal(),
        kept,
        Case::erasing(Some(foreign()), Contents::Gpt).proposal(),
        Case::erasing(None, Contents::Blank).proposal(),
    ]
}

fn replies() -> Vec<Reply> {
    let mut all = vec![
        Reply::Hello { version: VERSION },
        Reply::System(system()),
        Reply::System(System {
            others: Err("reading BootOrder: EIO".into()),
            disks: Vec::new(),
            ..system()
        }),
        Reply::Event(Event::step(4)),
        Reply::Event(Event::Progress {
            done: 0,
            total: u64::MAX,
        }),
        Reply::Event(Event::Log {
            line: "e2fsck: Pass 5: Checking group summary information".into(),
        }),
        Reply::Done(Summary {
            disk: "/dev/vda".into(),
            entry: 0xffff,
        }),
        Reply::Failed(Failure {
            step: 0,
            message: "/dev/vda changed since it was reviewed; nothing was written".into(),
        }),
    ];
    all.extend(issues().into_iter().map(Reply::Issue));
    all.extend(
        proposals()
            .into_iter()
            .enumerate()
            .map(|(id, proposal)| Reply::Proposal {
                id: id as u64,
                proposal,
            }),
    );
    all
}

fn requests() -> Vec<Request> {
    let mut erase = choices();
    erase.mode = Mode::Erase;
    erase.person = None;
    let mut free = Choices::new("/dev/vda", Mode::Free { region: None });
    free.remote = Some(String::new());
    vec![
        Request::Hello { version: VERSION },
        Request::Probe,
        Request::Open {
            disk: "/dev/vda".into(),
        },
        Request::Propose(choices()),
        Request::Propose(erase),
        Request::Propose(free.clone()),
        Request::Propose(Choices::new("/dev/vda", Mode::Free { region: Some(0) })),
        Request::Install {
            choices: free,
            proposal: u64::MAX,
        },
    ]
}

#[test]
fn every_request_and_reply_round_trips() {
    for request in requests() {
        let frame = request.frame();
        let (kind, body) = payload(&frame);
        assert_eq!(Request::decode(kind, body), Ok(request));
    }
    for reply in replies() {
        let frame = reply.frame();
        let (kind, body) = payload(&frame);
        assert_eq!(Reply::decode(kind, body), Ok(reply));
    }
}

#[test]
fn frames_arrive_whole_through_the_deframers() {
    let wire: Vec<u8> = replies().iter().flat_map(Reply::frame).collect();
    let mut d = super::replies();
    let mut got = Vec::new();
    for piece in wire.chunks(7) {
        d.push(piece);
        while let Some((kind, body)) = d.next_frame().unwrap() {
            got.push(Reply::decode(kind, &body).unwrap());
        }
    }
    assert_eq!(got, replies());
    let mut d = super::requests();
    d.push(&[kind::PROPOSE, 0, 1, 0, 1]);
    assert!(d.next_frame().is_err(), "a request past its bound");
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Every truncation and scattered pair of byte changes of `bytes`.
fn mutations(bytes: &[u8], seed: u64, rounds: usize, mut check: impl FnMut(&[u8])) {
    let mut state = seed;
    for len in 0..=bytes.len() {
        check(&bytes[..len]);
    }
    for at in 0..bytes.len() {
        for _ in 0..rounds {
            let r = next(&mut state);
            let mut mutated = bytes.to_vec();
            mutated[at] = r as u8;
            let other = (r >> 16) as usize % bytes.len();
            mutated[other] = (r >> 8) as u8;
            check(&mutated);
        }
    }
}

/// A mutated frame decodes to a message or to `Malformed`, never a panic,
/// and a message it decodes to encodes to one that decodes the same.
#[test]
fn mutated_frames_decode_or_are_malformed() {
    for request in requests() {
        let frame = request.frame();
        let (kind, body) = payload(&frame);
        mutations(body, 0x9e37_79b9_7f4a_7c15, 4, |bytes| {
            if let Ok(decoded) = Request::decode(kind, bytes) {
                let again = decoded.frame();
                let (kind, body) = payload(&again);
                assert_eq!(Request::decode(kind, body), Ok(decoded));
            }
        });
    }
    for reply in replies() {
        let frame = reply.frame();
        let (kind, body) = payload(&frame);
        let rounds = if body.len() > 4096 { 1 } else { 4 };
        mutations(body, 0x2545_f491_4f6c_dd1d, rounds, |bytes| {
            if let Ok(decoded) = Reply::decode(kind, bytes) {
                let again = decoded.frame();
                let (kind, body) = payload(&again);
                assert_eq!(Reply::decode(kind, body), Ok(decoded));
            }
        });
    }
}

#[test]
fn a_structure_has_one_encoding() {
    let decode = |fields: Fields| Request::decode(kind::OPEN, &fields.encode());
    assert!(decode(Fields::new().with("disk", "/dev/vda")).is_ok());
    assert_eq!(
        decode(Fields::new().with("disk", "a").with("disk", "b")),
        Err(Malformed)
    );
    assert_eq!(
        decode(Fields::new().with("disk", "a").with("other", "b")),
        Err(Malformed)
    );
    assert_eq!(decode(Fields::new().with("disk", [0xff])), Err(Malformed));
    let hello = |version: &str| {
        Request::decode(
            kind::HELLO,
            &Fields::new().with("version", version).encode(),
        )
    };
    assert_eq!(hello("1"), Ok(Request::Hello { version: 1 }));
    for bad in ["01", "+1", "-1", "", " 1", "4294967296"] {
        assert_eq!(hello(bad), Err(Malformed), "{bad:?}");
    }
    let propose = |choices: Fields| {
        Request::decode(
            kind::PROPOSE,
            &Fields::new().with("choices", choices.encode()).encode(),
        )
    };
    let erase = || Fields::new().with("disk", "/dev/vda").with("mode", "erase");
    assert!(propose(erase()).is_ok());
    assert_eq!(propose(erase().with("root", "3")), Err(Malformed));
    assert_eq!(propose(erase().with("first", "yes")), Err(Malformed));
    let reuse = Fields::new().with("disk", "/dev/vda").with("mode", "reuse");
    assert_eq!(
        propose(reuse.clone()),
        Err(Malformed),
        "a reuse names its root"
    );
    assert!(propose(reuse.with("root", "3")).is_ok());
    assert_eq!(Request::decode(0x7f, &[]), Err(Malformed));
    assert_eq!(Reply::decode(kind::PROBE, &[]), Err(Malformed));
}
