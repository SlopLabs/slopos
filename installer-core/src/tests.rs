use super::*;
use alloc::vec;
use alloc::vec::Vec;
use std::string::ToString;

use slopos_boot_core::install::{self, Mode as PlanMode, Role};

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
fn sizes_are_read_in_the_units_given() {
    assert_eq!(parse_size("4096"), Some(4096));
    assert_eq!(parse_size("20GB"), Some(20_000_000_000));
    assert_eq!(parse_size("512MB"), Some(512_000_000));
    assert_eq!(parse_size("17.2GB"), Some(17_200_000_000));
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
        "20gb",
        "20iB",
        "5KiBiB",
        "5BiB",
        "16777216T",
    ] {
        assert_eq!(parse_size(bad), None, "{bad:?}");
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
    assert!(p.review()[0].contains("deleting everything on /dev/nvme0n1"));
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
    let review = p.review();
    assert_eq!(review.len(), 10, "{review:?}");
    assert!(review[1].contains("shared") && review[1].contains("\\EFI\\SlopOS\\"));
    for (line, at) in review[2..5]
        .iter()
        .zip([613 * MIB, 1637 * MIB, 1637 * MIB + p.root.chosen])
    {
        assert!(line.contains("new, "), "{line}");
        assert!(
            line.ends_with(&std::format!("at {}", disk_size(at))),
            "{line}"
        );
    }
    assert!(review[6].contains(DEFAULT_REMOTE));
    assert!(review[7].starts_with("user:") && review[7].contains("no password"));
    assert!(review[8].ends_with(DEFAULT_CMDLINE));
    assert!(review[9].starts_with("Startup order:"));
}

#[test]
fn the_systems_a_menu_leaves_out_are_said_with_why() {
    let mut p = Case::on(foreign(), PlanMode::Free { region: None }).proposal();
    p.setup.left_out = vec!["Twin".into()];
    p.setup.removed = vec!["Gone".into()];
    let lines: Vec<String> = p.left_out().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("“Twin”") && lines[0].contains("same name"));
    assert!(lines[1].contains("“Gone”") && lines[1].contains("erases"));
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
    assert!(p.review()[3].starts_with("SlopOS partition: partition 1 (Linux filesystem"));
    assert!(p.review()[3].ends_with(", formatted"));
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
    assert!(
        review[1].ends_with("made by an earlier install"),
        "{}",
        review[1]
    );
    assert!(review.iter().any(|l| l.starts_with("Accounts:")));
    assert!(
        review
            .iter()
            .any(|l| l.contains("the installed system's own if it has one"))
    );
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
    for (line, role) in p.review()[1..5].iter().zip(install::ROLES) {
        assert!(line.starts_with(&capitalised(role_words(role))), "{line}");
    }
    assert!(p.review()[3].ends_with(", formatted"));
}

#[test]
fn the_cli_names_the_flag_the_window_leaves_out() {
    let plan = |error| Issue::Plan {
        error,
        node: "/dev/vda".into(),
        root: Some("/dev/vda3".into()),
    };
    let installed = plan(install::PlanError::Installed(3));
    assert!(
        installed
            .to_string()
            .ends_with("--mode reuse --root /dev/vda3.")
    );
    assert!(
        installed
            .words()
            .ends_with("Choose Reinstall SlopOS instead.")
    );
    let elsewhere = Issue::Plan {
        error: install::PlanError::RootElsewhere(3),
        node: "/dev/vda".into(),
        root: None,
    };
    assert!(elsewhere.to_string().ends_with("--root 3."));
    let only = Issue::OnlyErase {
        node: "/dev/vda".into(),
        contents: Contents::Blank,
    };
    assert!(only.to_string().starts_with("/dev/vda: "));
    assert!(only.to_string().contains("--mode erase"));
    assert!(!only.words().contains("--"));
    let room = plan(install::PlanError::NoRoom {
        needed: 2_500_000_000,
        available: 31_000_000,
    });
    assert!(room.words().contains("2.5 GB") && !room.words().contains("31 MB"));
    assert!(room.to_string().contains("31 MB"));
    let summary = Summary {
        disk: "/dev/vda".into(),
        entry: 0x1f,
    };
    assert!(summary.to_string().contains("/dev/vda") && summary.to_string().contains("Boot001F"));
}

#[test]
fn every_step_is_named_from_one() {
    assert_eq!(
        Event::step(1),
        Event::Step {
            index: 1,
            total: 9,
            title: STEPS[0].into()
        }
    );
    assert!(matches!(Event::step(9), Event::Step { title, .. } if title == STEPS[8]));
}

#[test]
fn a_disk_s_size_is_said_as_its_label_says_it() {
    assert_eq!(disk_size(999), "999 bytes");
    assert_eq!(disk_size(1_000_204_886_016), "1 TB");
    assert_eq!(disk_size(16 * GIB), "17.2 GB");
    assert_eq!(disk_size(512 * 1_000_000), "512 MB");
    assert_eq!(disk_size(2_500_000_000), "2.5 GB");
    assert_eq!(disk_size(100 * MIB), "105 MB");
    assert_eq!(disk_size(999_600_000_000), "1 TB");
    assert_eq!(disk_size(999_950_000), "1 GB");
    assert_eq!(disk_size(1), "1 byte");
}

#[test]
fn a_typed_size_is_read_in_the_units_the_box_shows() {
    assert_eq!(parse_disk_size("20"), Some(20_000_000_000));
    assert_eq!(parse_disk_size("20 GB"), Some(20_000_000_000));
    assert_eq!(parse_disk_size("17.2 gb"), Some(17_200_000_000));
    assert_eq!(parse_disk_size("512MB"), Some(512_000_000));
    assert_eq!(parse_disk_size("1.5 TB"), Some(1_500_000_000_000));
    assert_eq!(parse_disk_size("9 GiB"), Some(9 * GIB));
    assert_eq!(parse_disk_size(&disk_size(16 * GIB)), Some(17_200_000_000));
    let huge = "340282366920938463463374607431768211455.5 B";
    for bad in [
        "",
        "GB",
        "1.2.3",
        "x",
        "20 parsecs",
        ".5",
        "1.0000000001",
        huge,
    ] {
        assert_eq!(parse_disk_size(bad), None, "{bad:?}");
    }
}

#[test]
fn a_disk_s_model_is_read_back_from_the_names_devfs_gives_it() {
    use slopos_boot_core::disk_id::{DISK_ID_MAX, DiskId};
    let model = |id: Option<DiskId>| {
        let id = id.expect("a name");
        let names: Vec<String> = [false, true]
            .into_iter()
            .filter_map(|alias| {
                let mut out = [0u8; DISK_ID_MAX];
                let len = id.write(alias, None, &mut out)?;
                String::from_utf8(out[..len].to_vec()).ok()
            })
            .collect();
        model_of(names.iter().map(String::as_str))
    };
    let kingston = DiskId::nvme(b"KINGSTON SNV3S1000G   ", b"50026B7382A1B2C3  ", 1);
    assert_eq!(model(kingston), Some("KINGSTON SNV3S1000G".into()));
    let spaced = DiskId::nvme(b"Model X", b"  S4EW123", 2);
    assert_eq!(model(spaced), Some("Model X".into()));
    let stick = DiskId::usb(b"SanDisk ", b"Ultra Fit       ", Some(b"4C530001"), 0);
    assert_eq!(model(stick), Some("SanDisk Ultra Fit".into()));
    let unnamed = DiskId::usb(b"", b"Flash Disk", Some(b"4C530001"), 0);
    assert_eq!(model(unnamed), Some("Flash Disk".into()));
}

#[test]
fn a_disk_s_model_is_read_from_the_names_udev_gives_it() {
    let nvme = [
        "nvme-QEMU_NVMe_Ctrl_slopos-root_1",
        "nvme-QEMU_NVMe_Ctrl_slopos-root",
    ];
    assert_eq!(model_of(nvme), Some("QEMU NVMe Ctrl".into()));
    assert_eq!(
        model_of(nvme.into_iter().rev()),
        Some("QEMU NVMe Ctrl".into())
    );
    assert_eq!(
        model_of(["nvme-KINGSTON_SNV3S1000G_50026B7382A1B2C3_2"]),
        Some("KINGSTON SNV3S1000G".into())
    );
    assert_eq!(
        model_of(["usb-QEMU_QEMU_HARDDISK_slopos-usb-0:0"]),
        Some("QEMU QEMU HARDDISK".into())
    );
    assert_eq!(
        model_of(["nvme-Model_X___S4EW123_2"]),
        Some("Model X".into())
    );
    assert_eq!(model_of(["virtio-slopos-verified"]), None);
    assert_eq!(model_of(["nvme-nothing"]), None);
    assert_eq!(model_of([]), None);
}
