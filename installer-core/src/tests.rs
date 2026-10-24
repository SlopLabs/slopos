use super::*;
use alloc::vec;
use alloc::vec::Vec;
use std::string::ToString;

use slopos_boot_core::install::{self, Mode as PlanMode, Role};
use slopos_boot_core::layout::{ESP_NAME, ROOT_NAME};

use crate::fixtures::{Case, GIB, LINUX, MIB, foreign, installed, table};

fn tags(p: &Proposal) -> Vec<(Tag, Option<Role>)> {
    p.actions
        .iter()
        .map(|a| match &a.target {
            Target::Disk | Target::Other(_) => (a.tag, None),
            Target::Role(role, _) => (a.tag, Some(*role)),
        })
        .collect()
}

#[test]
fn amounts_are_shown_in_binary_units() {
    assert_eq!(human(512), "512 B");
    assert_eq!(human(1536), "1.5 KiB");
    assert_eq!(human(7 * GIB), "7.0 GiB");
    assert_eq!(human(3 << 40), "3.0 TiB");
}

#[test]
fn sizes_are_read_in_binary_units() {
    assert_eq!(parse_size("4096"), Some(4096));
    assert_eq!(parse_size("4096B"), Some(4096));
    assert_eq!(parse_size("20G"), Some(20 * GIB));
    assert_eq!(parse_size("20GiB"), Some(20 * GIB));
    assert_eq!(parse_size("512M"), Some(512 * MIB));
    assert_eq!(parse_size("2T"), Some(2 << 40));
    for bad in [
        "",
        "G",
        "iB",
        "1.5G",
        "-1G",
        "20g",
        "20 G",
        "20GB",
        "20iB",
        "5KiBiB",
        "5BiB",
        "16777216T",
    ] {
        assert_eq!(parse_size(bad), None, "{bad:?}");
    }
}

#[test]
fn a_size_is_written_as_it_is_read() {
    assert_eq!(size_text(200 * GIB), "200G");
    assert_eq!(size_text(1536 * MIB), "1536M");
    assert_eq!(size_text(2 << 40), "2T");
    assert_eq!(size_text(4097), "4097B");
    assert_eq!(size_text(0), "0B");
    for bytes in [
        0,
        1,
        4097,
        3 * MIB,
        7 * GIB,
        7 * GIB + MIB,
        5 << 40,
        u64::MAX,
    ] {
        assert_eq!(parse_size(&size_text(bytes)), Some(bytes), "{bytes}");
    }
}

#[test]
fn types_are_named_as_fdisk_names_them() {
    assert_eq!(type_name(&ESP_TYPE), "EFI system");
    assert_eq!(type_name(&ROOT_TYPE), "SlopOS root");
    assert_eq!(type_name(&LINUX), "Linux filesystem");
    let unknown = Guid([7; 16]);
    assert_eq!(type_name(&unknown), unknown.to_string());
}

#[test]
fn a_root_needs_the_payload_and_room_to_build_beside_it() {
    assert_eq!(root_min(None), GIB);
    assert_eq!(root_min(Some(3 * GIB)), 9 * GIB);
}

#[test]
fn partitions_are_named_as_the_kernel_names_them() {
    assert_eq!(partition_node("/dev/nvme0n1", 3), "/dev/nvme0n1p3");
    assert_eq!(partition_node("/dev/vda", 2), "/dev/vda2");
    assert_eq!(short("/dev/sda"), "sda");
}

#[test]
fn a_layout_lists_partitions_and_free_regions_in_order() {
    let (header, array) = foreign();
    let layout = Layout::of("/dev/vda", &header, &array);
    assert_eq!(layout.capacity, 16 * GIB);
    let pieces: Vec<_> = layout
        .segments
        .iter()
        .map(|s| (s.start, &s.piece))
        .collect();
    assert_eq!(pieces.len(), 3);
    assert!(matches!(
        pieces[0],
        (start, Piece::Partition { number: 1, role: Some(Role::Esp), node, .. })
            if start == MIB && node == "/dev/vda1"
    ));
    assert!(matches!(
        pieces[1],
        (_, Piece::Partition { number: 2, role: None, kind, name, .. })
            if kind == "Linux filesystem" && name == "theirs"
    ));
    assert!(matches!(pieces[2], (start, Piece::Free { region: 0 }) if start == 613 * MIB));
    assert_eq!(layout.regions().count(), 1);
}

#[test]
fn an_erase_loses_the_whole_disk() {
    let p = Case::erasing(Some(foreign()), Contents::Gpt).proposal();
    assert!(p.erase);
    assert_eq!(
        tags(&p),
        [
            (Tag::Erase, None),
            (Tag::New, Some(Role::Esp)),
            (Tag::New, Some(Role::Boot)),
            (Tag::New, Some(Role::Root)),
            (Tag::New, Some(Role::Crash)),
        ]
    );
    assert_eq!(p.before.segments.len(), 3, "what is lost is shown");
    assert_eq!(p.after.regions().count(), 0);
    assert_eq!(p.root.min, GIB);
    assert_eq!(p.root.chosen, p.root.max);
    assert_eq!(
        p.review()[0],
        "a new partition table: EVERYTHING on /dev/nvme0n1 is lost"
    );
}

#[test]
fn an_erase_of_a_disk_without_a_table_shows_what_it_held() {
    for contents in [
        Contents::Blank,
        Contents::Mbr(vec![MbrPartition {
            index: 0,
            kind: 0x83,
            bytes: GIB,
        }]),
    ] {
        let p = Case::erasing(None, contents.clone()).proposal();
        assert_eq!(p.contents, contents);
        assert!(p.before.segments.is_empty());
        assert_eq!(p.before.capacity, 16 * GIB);
        assert_eq!(p.actions.len(), 5);
    }
}

#[test]
fn free_space_shares_the_esp_and_keeps_every_other_partition() {
    let p = Case::on(foreign(), PlanMode::Free { region: None }).proposal();
    assert!(!p.erase);
    assert_eq!(
        tags(&p),
        [
            (Tag::Share, Some(Role::Esp)),
            (Tag::New, Some(Role::Boot)),
            (Tag::New, Some(Role::Root)),
            (Tag::New, Some(Role::Crash)),
            (Tag::Keep, None),
        ]
    );
    assert!(matches!(
        &p.actions[4].target,
        Target::Other(Part::Existing { number: 2, node, kind, bytes })
            if node == "/dev/nvme0n1p2" && kind == "Linux filesystem" && *bytes == 512 * MIB
    ));
    assert_eq!(
        p.review(),
        [
            "new entries in its partition table; every other partition stays as it is",
            "EFI system partition: partition 1 (EFI system, 100.0 MiB), shared: SlopOS writes \\EFI\\SlopOS\\ only",
            "SlopOS boot: new, 1.0 GiB at 613.0 MiB",
            &std::format!("SlopOS root: new, {} at 1.6 GiB", human(p.root.chosen)),
            &std::format!(
                "SlopOS crash: new, 4.0 MiB at {}",
                human(1637 * MIB + p.root.chosen)
            ),
            "/usr/local: the toolchain the medium carries",
            "/src/slopos: the medium's clone, fetching from https://github.com/SlopLabs/slopos",
            "user: its first administrator, logged in by the desktop, with no password",
            "boots with: panic=reboot",
            "firmware entry \"SlopOS\": where it stands in the boot order, or after the systems there",
        ]
    );
}

#[test]
fn the_systems_a_menu_leaves_out_are_said_with_why() {
    let mut p = Case::on(foreign(), PlanMode::Free { region: None }).proposal();
    p.setup.left_out = vec!["Twin".into()];
    p.setup.removed = vec!["Gone".into()];
    let lines: Vec<String> = p.left_out().collect();
    assert_eq!(
        lines,
        [
            "leaving \"Twin\" out of the menu: an entry there collides with it",
            "leaving \"Gone\" out of the menu: the install removes its loader",
        ]
    );
}

#[test]
fn the_least_root_is_said_as_the_planner_rounds_it() {
    let mut case = Case::on(foreign(), PlanMode::Free { region: None });
    case.size.min = GIB + 1;
    case.size.target = Some(0);
    let p = case.proposal();
    assert_eq!(p.root.min, GIB + MIB);
    assert_eq!(p.root.chosen, p.root.min);
}

#[test]
fn a_root_size_leaves_the_rest_free_and_the_span_holds_both_bounds() {
    let whole = Case::on(foreign(), PlanMode::Free { region: None }).proposal();
    let max = whole.root.max;
    for (target, chosen) in [
        (Some(4 * GIB), 4 * GIB),
        (Some(GIB), GIB),
        (Some(GIB / 2), GIB),
        (Some(max), max),
        (Some(max + GIB), max),
        (None, max),
    ] {
        let mut case = Case::on(foreign(), PlanMode::Free { region: None });
        case.size.target = target;
        let p = case.proposal();
        assert_eq!(
            p.root,
            Span {
                min: GIB,
                max,
                chosen
            },
            "{target:?}"
        );
        let free: u64 = p.after.regions().map(|s| s.bytes).sum();
        assert_eq!(free, max - chosen, "{target:?}");
    }
}

#[test]
fn a_reused_partition_is_formatted_and_its_size_is_the_root_s() {
    let (header, array) = table(16 * GIB, &[(1, LINUX, 1, 10_241)]);
    let p = Case::on(
        (header, array),
        PlanMode::Reuse {
            root: 1,
            region: None,
        },
    )
    .proposal();
    assert_eq!(
        tags(&p),
        [
            (Tag::New, Some(Role::Esp)),
            (Tag::New, Some(Role::Boot)),
            (Tag::Reuse, Some(Role::Root)),
            (Tag::New, Some(Role::Crash)),
        ]
    );
    assert_eq!(
        p.root,
        Span {
            min: 10 * GIB,
            max: 10 * GIB,
            chosen: 10 * GIB
        }
    );
    assert_eq!(
        p.review()[3],
        "SlopOS root: partition 1 (Linux filesystem, 10.0 GiB), FORMATTED"
    );
    assert!(!p.keeps_root());
}

#[test]
fn a_reinstall_keeps_every_partition_and_the_root_s_people() {
    let mut case = Case::on(
        installed(),
        PlanMode::Reuse {
            root: 3,
            region: None,
        },
    );
    case.keep_root = true;
    case.own_esp = true;
    let mut p = case.proposal();
    assert!(tags(&p).iter().all(|(tag, _)| *tag == Tag::Keep));
    assert!(p.keeps_root());
    p.setup.person = None;
    let review = p.review();
    assert_eq!(
        review[1],
        "EFI system partition: partition 1 (EFI system, 260.0 MiB), made by an earlier install"
    );
    assert!(review.iter().any(|l| l == "people: the root's own"));
    assert!(review.iter().any(|l| {
        l.starts_with("/src/slopos: the root's own if it has one, else the medium's clone")
    }));
}

#[test]
fn a_reinstall_that_formats_formats_the_root_and_the_crash_records() {
    let p = Case::on(
        installed(),
        PlanMode::Reuse {
            root: 3,
            region: None,
        },
    )
    .proposal();
    assert_eq!(
        tags(&p),
        [
            (Tag::Share, Some(Role::Esp)),
            (Tag::Keep, Some(Role::Boot)),
            (Tag::Format, Some(Role::Root)),
            (Tag::Format, Some(Role::Crash)),
        ]
    );
    let names = [ESP_NAME, "SlopOS boot", ROOT_NAME, "SlopOS crash"];
    for (line, name) in p.review()[1..5].iter().zip(names) {
        assert!(line.starts_with(name), "{line}");
    }
}

#[test]
fn issues_read_as_the_cli_says_them() {
    let plan = |error| Issue::Plan {
        error,
        node: "/dev/vda".into(),
        root: Some("/dev/vda3".into()),
    };
    assert_eq!(
        plan(install::PlanError::Installed(3)).to_string(),
        "SlopOS is on /dev/vda already (partition 3); install over it with --mode reuse --root /dev/vda3"
    );
    assert_eq!(
        Issue::Plan {
            error: install::PlanError::RootElsewhere(3),
            node: "/dev/vda".into(),
            root: None,
        }
        .to_string(),
        "SlopOS's root is partition 3 of /dev/vda; reinstall over it with --root 3"
    );
    assert_eq!(
        Issue::OnlyErase {
            node: "/dev/vda".into(),
            holds: Contents::Blank.holding(),
        }
        .to_string(),
        "/dev/vda holds nothing; only --mode erase installs onto it"
    );
    assert_eq!(
        Summary {
            disk: "/dev/vda".into(),
            entry: 0x1f
        }
        .to_string(),
        "SlopOS is installed on /dev/vda, booted by the firmware entry Boot001F"
    );
}

#[test]
fn every_step_is_named_from_one() {
    assert_eq!(
        Event::step(1),
        Event::Step {
            index: 1,
            total: 9,
            title: "writing the partition table".into()
        }
    );
    assert!(
        matches!(Event::step(9), Event::Step { title, .. } if title == "registering the firmware entry")
    );
}
