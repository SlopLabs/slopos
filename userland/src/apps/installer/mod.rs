//! `/bin/installer` — install SlopOS onto a disk from the medium the live
//! system booted, beside whatever else the disk holds.
//!
//! The medium is what the loader brought, served at [`layout::MEDIUM_DIR`]:
//! the kernel and base every slot gets and Limine for the ESP; and on a stick
//! built with the payload, the partition the kernel mounts at
//! [`layout::PAYLOAD_DIR`], the toolchain for `/usr/local` and a clone of the
//! source for `/src`. An install writes the partition table — a fresh one, or
//! new entries beside the disk's own — and has the kernel re-read it, formats
//! the new partitions, fills the root, writes both slots and the loader with
//! its configuration, makes slot a the default, checks the root with
//! `e2fsck -fn`, and last registers the firmware entry. Nothing the plan does
//! not name is written, and nothing on a shared ESP outside `\EFI\SlopOS\`.
//!
//! Every answer is a flag as well as a question, so a check drives the
//! program a person runs.

mod boot;
mod disk;
mod root;

use std::fs;
use std::io::{Write, stdin, stdout};
use std::path::PathBuf;

use slopos_boot_core::Guid;
use slopos_boot_core::gpt::{Header, Partition};
use slopos_boot_core::install::{self, Mode as PlanMode, Place, PlanError, ROLES, Role};
use slopos_boot_core::layout::{self, BOOT_LABEL, ESP_LABEL, ROOT_TYPE};

use crate::boot_disk::{
    disk_guid_of, medium_disk_guid, not_installable, open_fat, partition_node, table_of,
    whole_disks,
};
use crate::syscall::core as sys_core;
use disk::{Disk, human};

/// What a root holds without the payload: the system's own writes.
const ROOT_MIN_BYTES: u64 = 1 << 30;
/// The room a root keeps beside the payload for building the system, which a
/// clean dev and tests build holds 3.8 GiB of.
const ROOT_WORK_BYTES: u64 = 6 << 30;
/// Room an ESP needs beside the loader for its directories and configuration.
const ESP_SLACK_BYTES: u64 = 1 << 20;
const DEFAULT_REMOTE: &str = "https://github.com/SlopLabs/slopos";
/// A slot that panics resets to the default, which is how a tried system
/// falls back.
const DEFAULT_CMDLINE: &str = "panic=reboot";

/// The install medium the live system serves.
pub struct Medium {
    /// The GPT disk GUID its image was built with: the disk never offered.
    disk_guid: Guid,
    payload: bool,
}

/// The directory `dir`, not a link to one, and the filesystem it is on.
fn dir_and_fs(dir: &str) -> Option<(fs::Metadata, slopos_abi::fs::UserStatfs)> {
    let meta = fs::symlink_metadata(dir)
        .ok()
        .filter(|meta| meta.is_dir())?;
    let path = std::ffi::CString::new(dir).ok()?;
    let stats = crate::syscall::fs::statfs_path(path.as_ptr()).ok()?;
    Some((meta, stats))
}

/// Whether the kernel mounted the medium's payload: the root of a read-only
/// ext4 volume at [`layout::PAYLOAD_DIR`], which no program fakes with a
/// directory.
fn payload_mounted() -> bool {
    dir_and_fs(layout::PAYLOAD_DIR).is_some_and(|(meta, stats)| {
        std::os::unix::fs::MetadataExt::ino(&meta) == root::EXT4_ROOT_INODE
            && stats.f_type == slopos_abi::fs::EXT2_SUPER_MAGIC
            && stats.f_flags & slopos_abi::fs::ST_RDONLY != 0
    })
}

/// The disk carrying the medium's GPT disk GUID, when its table names a
/// payload partition.
fn unmounted_payload(disk_guid: Guid) -> Option<String> {
    whole_disks().ok()?.into_iter().find(|node| {
        disk_guid_of(node) == Some(disk_guid)
            && table_of(node).is_some_and(|table| {
                table
                    .partitions()
                    .any(|p| p.entry.type_guid == layout::PAYLOAD_TYPE)
            })
    })
}

impl Medium {
    /// The medium, whole: what every install takes, and what a payload's
    /// trees take with them, checked before anything is written.
    fn find() -> Result<Medium, String> {
        let dir = layout::MEDIUM_DIR;
        let served =
            dir_and_fs(dir).is_some_and(|(_, stats)| stats.f_type == slopos_abi::fs::BASEFS_MAGIC);
        if !served {
            return Err(format!(
                "no install medium at {dir}; boot the SlopOS ISO to install"
            ));
        }
        let medium = Medium {
            disk_guid: medium_disk_guid()?.ok_or_else(|| {
                format!(
                    "no install medium: {dir}/{} is missing; boot the SlopOS ISO to install",
                    layout::MEDIUM_DISK_GUID
                )
            })?,
            payload: payload_mounted(),
        };
        for file in [
            layout::MEDIUM_KERNEL,
            layout::MEDIUM_BASE,
            layout::MEDIUM_LOADER,
            layout::MEDIUM_LOADER_LICENSE,
            layout::MEDIUM_LOADER_NOTICES,
        ] {
            if !medium.has(file) {
                return Err(format!(
                    "no install medium: {} is missing; boot the SlopOS ISO to install",
                    medium.path(file).display()
                ));
            }
        }
        let toolchain_manifest = format!(
            "{}/{}",
            slopos_tree_core::MANIFEST_DIR,
            slopos_tree_core::manifest_name(root::TOOLCHAIN)
        );
        for (tree, needs) in [
            (root::TOOLCHAIN, toolchain_manifest.as_str()),
            (root::SOURCE, "/src/slopos/.git/config"),
        ] {
            if medium.payload_has(tree) && !medium.payload_has(needs) {
                return Err(format!(
                    "the payload carries {tree} without {needs}; it was not built whole"
                ));
            }
        }
        if !medium.payload
            && let Some(node) = unmounted_payload(medium.disk_guid)
        {
            println!(
                "installer: {node} carries the medium's payload, which the kernel did not mount; its log says why"
            );
        }
        Ok(medium)
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        PathBuf::from(layout::MEDIUM_DIR).join(rel.trim_start_matches('/'))
    }

    pub fn has(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }

    pub fn read(&self, rel: &str) -> Result<Vec<u8>, String> {
        fs::read(self.path(rel)).map_err(|e| format!("{}: {e}", self.path(rel).display()))
    }

    pub fn payload_path(&self, rel: &str) -> PathBuf {
        PathBuf::from(layout::PAYLOAD_DIR).join(rel.trim_start_matches('/'))
    }

    pub fn payload_has(&self, rel: &str) -> bool {
        self.payload && self.payload_path(rel).exists()
    }

    /// The root's least size: the payload and room to build beside it, or the
    /// system's own needs without one.
    fn root_min(&self) -> Result<u64, String> {
        if !self.payload_has(root::TOOLCHAIN) && !self.payload_has(root::SOURCE) {
            return Ok(ROOT_MIN_BYTES);
        }
        let path = std::ffi::CString::new(layout::PAYLOAD_DIR).map_err(|_| "a NUL in a path")?;
        let stats = crate::syscall::fs::statfs_path(path.as_ptr())
            .map_err(|e| format!("{}: {e:?}", layout::PAYLOAD_DIR))?;
        Ok(stats.f_blocks * stats.f_bsize + ROOT_WORK_BYTES)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Erase,
    Free,
    Reuse,
}

#[derive(Default)]
struct Options {
    disk: Option<String>,
    mode: Option<Mode>,
    region: Option<usize>,
    root: Option<String>,
    keep_root: Option<bool>,
    remote: Option<String>,
    first: Option<bool>,
    cmdline: Option<String>,
    yes: bool,
}

const USAGE: &str = "usage: installer [--disk /dev/<disk>] [--mode erase|free|reuse] [--region <n>]
                 [--root /dev/<partition>] [--keep-root | --format-root] [--remote <url>]
                 [--first | --not-first] [--cmdline <words>] [--yes]
  --disk         the disk to install onto
  --mode         erase: a new partition table over the whole disk
                 free: SlopOS's partitions in free space, beside the others
                 reuse: the root over --root, the rest where the disk has them
  --region       the free region new partitions go into (default: the largest)
  --root         the partition reuse puts the root on
  --keep-root    keep the SlopOS root reuse finds there, updating /usr/local
  --format-root  format it instead
  --remote       the URL /src/slopos fetches from and pushes to
  --first        put SlopOS first in the firmware's boot order
  --not-first    leave it where it stands, or after the systems there
  --cmdline      what the installed kernel boots with
  --yes          write the disk without asking";

fn parse(args: &[String]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let mut value = || {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--disk" => opts.disk = Some(value()?),
            "--mode" => {
                opts.mode = Some(match value()?.as_str() {
                    "erase" => Mode::Erase,
                    "free" => Mode::Free,
                    "reuse" => Mode::Reuse,
                    other => return Err(format!("--mode {other}: erase, free or reuse")),
                })
            }
            "--region" => {
                let text = value()?;
                let n: usize = text
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or_else(|| format!("--region {text}: a region's number, from 1"))?;
                opts.region = Some(n - 1);
            }
            "--root" => opts.root = Some(value()?),
            "--keep-root" => opts.keep_root = Some(true),
            "--format-root" => opts.keep_root = Some(false),
            "--remote" => opts.remote = Some(value()?),
            "--first" => opts.first = Some(true),
            "--not-first" => opts.first = Some(false),
            "--cmdline" => opts.cmdline = Some(value()?),
            "--yes" => opts.yes = true,
            "--help" | "-h" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown flag {other}\n{USAGE}")),
        }
    }
    Ok(opts)
}

/// An answer from standard input, or the default on an empty line; at end of
/// input there is nobody to ask, and the flag is named instead.
fn ask(question: &str, default: Option<&str>, flag: &str) -> Result<String, String> {
    match default {
        Some(d) => print!("{question} [{d}]: "),
        None => print!("{question}: "),
    }
    let _ = stdout().flush();
    let mut line = String::new();
    match stdin().read_line(&mut line) {
        Ok(0) => Err(format!(
            "no answer to \"{question}\"; pass {flag} to answer it"
        )),
        Ok(_) => match (line.trim(), default) {
            ("", Some(d)) => Ok(d.to_owned()),
            ("", None) => ask(question, default, flag),
            (answer, _) => Ok(answer.to_owned()),
        },
        Err(e) => Err(format!("reading the answer: {e}")),
    }
}

fn ask_yes(question: &str, default: bool, flag: &str) -> Result<bool, String> {
    loop {
        match ask(question, Some(if default { "yes" } else { "no" }), flag)?.as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("yes or no"),
        }
    }
}

fn choose_disk(medium: &Medium) -> Result<String, String> {
    let mut offered = Vec::new();
    println!("Disks:");
    for node in whole_disks()? {
        if let Some(why) = not_installable(&node, Some(medium.disk_guid)) {
            println!("{node}: {why}, not offered");
            continue;
        }
        match Disk::open(&node) {
            Ok(disk) => print!("{}", disk.describe()),
            Err(why) => println!("{node}: {why}"),
        }
        offered.push(node);
    }
    let default = match offered.as_slice() {
        [] => return Err("no disk to install onto".to_owned()),
        [only] => Some(only.as_str()),
        _ => None,
    };
    ask("Disk to install onto", default, "--disk")
}

/// `nvme0n1` as well as `/dev/nvme0n1`.
fn disk_node(named: String) -> String {
    if named.starts_with('/') {
        named
    } else {
        format!("/dev/{named}")
    }
}

fn choose_mode(disk: &Disk) -> Result<Mode, String> {
    if disk.table.is_none() {
        println!(
            "{} holds {}, so only erase can install onto it.",
            disk.node,
            disk.holds().unwrap_or_else(|| "nothing".to_owned())
        );
        return Ok(Mode::Erase);
    }
    loop {
        let answer = ask(
            "erase the whole disk, install into free space beside the rest, or reuse a partition as the root (erase/free/reuse)",
            Some("free"),
            "--mode",
        )?;
        match answer.as_str() {
            "erase" => return Ok(Mode::Erase),
            "free" => return Ok(Mode::Free),
            "reuse" => return Ok(Mode::Reuse),
            _ => println!("erase, free or reuse"),
        }
    }
}

fn choose_region(disk: &Disk, given: Option<usize>, ask_it: bool) -> Result<Option<usize>, String> {
    let regions = disk.regions();
    if given.is_some() || !ask_it || regions.len() < 2 {
        return Ok(given);
    }
    let largest = (0..regions.len())
        .max_by_key(|&i| regions[i].blocks())
        .unwrap_or(0);
    let answer = ask(
        "Free region for the new partitions",
        Some(&(largest + 1).to_string()),
        "--region",
    )?;
    answer
        .parse::<usize>()
        .ok()
        .filter(|&n| (1..=regions.len()).contains(&n))
        .map(|n| Some(n - 1))
        .ok_or_else(|| format!("{answer}: no free region of that number"))
}

fn explain(e: PlanError, disk: &Disk) -> String {
    let node = &disk.node;
    match e {
        PlanError::NoRegion => format!("{node} has no free region of that number"),
        PlanError::NoRoom { needed, available } => format!(
            "the free region holds {}; SlopOS needs {}",
            human(available),
            human(needed)
        ),
        PlanError::NoPartition(n) => format!("{node} has no partition {n}"),
        PlanError::NotForRoot(n) => {
            format!("partition {n} is the ESP or SlopOS's boot or crash partition, not a root")
        }
        PlanError::Installed(n) => {
            let root = disk
                .partitions()
                .into_iter()
                .find(|p| p.entry.type_guid == ROOT_TYPE)
                .and_then(|p| partition_node(node, &p).ok());
            format!(
                "SlopOS is on {node} already (partition {n}); install over it with --mode reuse{}",
                root.map_or_else(String::new, |root| format!(" --root {root}"))
            )
        }
        PlanError::Ambiguous(role) => format!("{node} has two {} partitions", role.name()),
        PlanError::RootTooSmall { needed, available } => format!(
            "the root partition holds {}; the root needs {}",
            human(available),
            human(needed)
        ),
        PlanError::RootElsewhere(n) => format!(
            "SlopOS's root is partition {n} of {node}; reinstall over it with --root {}",
            disk.partition_named(&n.to_string())
                .and_then(|p| partition_node(node, &p))
                .unwrap_or_else(|_| n.to_string())
        ),
        PlanError::RootTooLarge { limit, available } => format!(
            "the root partition holds {}; a root holds at most {}",
            human(available),
            human(limit)
        ),
        PlanError::NoSlot => format!("{node}'s table has no unused entry for a new partition"),
    }
}

fn describe_place(disk: &Disk, role: Role, place: Place, formatted: bool, own: bool) -> String {
    let block = disk.geometry.block();
    match place {
        Place::New {
            first_lba,
            last_lba,
            ..
        } => format!(
            "new, {} at {}",
            human((last_lba - first_lba + 1) * block),
            human(first_lba * block)
        ),
        Place::Existing(n) => {
            let part = disk.partitions().into_iter().find(|p| p.entry.number == n);
            let what = part.map_or_else(String::new, |p| {
                format!(
                    " ({}, {})",
                    disk::type_name(&p.entry.type_guid),
                    human(p.len)
                )
            });
            match role {
                Role::Esp if formatted => {
                    format!("partition {n}{what}, FORMATTED: it holds no volume yet")
                }
                Role::Esp if own => format!("partition {n}{what}, made by an earlier install"),
                Role::Esp => {
                    format!("partition {n}{what}, shared: SlopOS writes \\EFI\\SlopOS\\ only")
                }
                _ if formatted => format!("partition {n}{what}, FORMATTED"),
                _ => format!("partition {n}{what}, kept"),
            }
        }
    }
}

/// What the installer was asked to do, settled before anything is written.
struct Settled {
    disk: Disk,
    mode: Mode,
    format: Formats,
    /// The table as it will be written, the plan applied.
    header: Header,
    array: Vec<u8>,
    plan: install::Plan,
    keep_root: bool,
    remote: Option<String>,
    first: bool,
    cmdline: String,
    /// Whether the ESP is SlopOS's own, whose removable-media path it writes.
    own_esp: bool,
    limine_conf: String,
}

fn settle(opts: Options, medium: &Medium) -> Result<Settled, String> {
    let interactive = opts.disk.is_none();
    let node = disk_node(match opts.disk {
        Some(node) => node,
        None => choose_disk(medium)?,
    });
    if let Some(why) = not_installable(&node, Some(medium.disk_guid)) {
        return Err(format!("{node} is {why}; nothing is installed onto it"));
    }
    let disk = Disk::open(&node)?;
    disk::reread(&disk)?;
    if !interactive {
        print!("{}", disk.describe());
    }
    let mode = match opts.mode {
        Some(mode) => mode,
        None if opts.yes => {
            return Err(format!(
                "--yes needs --mode: with nobody asked, nothing chooses what happens to {node}"
            ));
        }
        None => choose_mode(&disk)?,
    };
    let fresh = mode == Mode::Erase;
    let (header, array) = if fresh {
        let header = Header::new(disk.geometry, disk::random_guid()?)
            .ok_or_else(|| format!("{node} is too small for a partition table"))?;
        let array = vec![0u8; header.array_bytes()];
        (header, array)
    } else {
        let table = disk.table.as_ref().ok_or_else(|| {
            format!(
                "{node} holds {}; only --mode erase installs onto it",
                disk.holds().unwrap_or_else(|| "nothing".to_owned())
            )
        })?;
        (table.header.grown(), table.array.clone())
    };
    let plan_mode = match mode {
        Mode::Erase => PlanMode::Free { region: None },
        Mode::Free => PlanMode::Free {
            region: choose_region(&disk, opts.region, opts.mode.is_none())?,
        },
        Mode::Reuse => {
            let named = match opts.root {
                Some(named) => named,
                None => ask("Partition for the root", None, "--root")?,
            };
            PlanMode::Reuse {
                root: disk.partition_named(&named)?.entry.number,
                region: choose_region(&disk, opts.region, opts.mode.is_none())?,
            }
        }
    };
    let root_min = medium.root_min()?;
    let plan =
        install::plan(&header, &array, plan_mode, root_min).map_err(|e| explain(e, &disk))?;

    let reused = match plan.root {
        Place::Existing(n) => disk.partitions().into_iter().find(|p| p.entry.number == n),
        Place::New { .. } => None,
    };
    let keep_root = match reused {
        Some(p) => {
            let node = partition_node(&disk.node, &p)?;
            let slopos = p.entry.type_guid == ROOT_TYPE && disk::holds_ext4(&node);
            match opts.keep_root {
                Some(true) if !slopos => {
                    return Err(format!("--keep-root: {node} holds no SlopOS root to keep"));
                }
                Some(keep) => keep,
                None if slopos => ask_yes(
                    &format!("{node} holds a SlopOS root: keep what it holds, updating /usr/local"),
                    true,
                    "--keep-root or --format-root",
                )?,
                None => false,
            }
        }
        None => false,
    };
    let format = check_kept(&disk, &plan, medium)?;
    let remote = if medium.payload_has(root::SOURCE) {
        Some(match opts.remote {
            Some(url) => url,
            None => ask(
                "Where /src/slopos fetches from and pushes to",
                Some(DEFAULT_REMOTE),
                "--remote",
            )?,
        })
    } else {
        None
    };
    let first = match opts.first {
        Some(first) => first,
        None => ask_yes(
            "Put SlopOS first in the firmware's boot order",
            false,
            "--first or --not-first",
        )?,
    };
    let cmdline = opts.cmdline.unwrap_or_else(|| DEFAULT_CMDLINE.to_owned());
    let mut array = array;
    let mut uniques = [slopos_boot_core::Guid::ZERO; 4];
    for unique in &mut uniques {
        *unique = disk::random_guid()?;
    }
    let header = plan.apply(header, &mut array, uniques);
    let unique = |role: Role| {
        header
            .partitions(&array)
            .filter_map(Result::ok)
            .find(|p| p.entry.number == plan.place(role).number())
            .map(|p| p.entry.unique)
            .ok_or_else(|| format!("the planned table has no {}", role.name()))
    };
    let esp_unique = unique(Role::Esp)?;
    let boot_unique = unique(Role::Boot)?;
    let own_esp =
        plan.esp.is_new() || format.esp || esp_was_ours(&disk, &plan, &boot_unique, medium)?;
    let limine_conf = boot::limine_conf(
        boot_unique,
        unique(Role::Root)?,
        &cmdline,
        own_esp.then_some(&esp_unique),
    )?;
    let settled = Settled {
        disk,
        mode,
        header,
        array,
        plan,
        keep_root,
        format,
        remote,
        first,
        cmdline,
        own_esp,
        limine_conf,
    };
    summarise(&settled, medium);
    if !opts.yes {
        confirm(&settled)?;
    }
    Ok(settled)
}

/// Whether the disk's existing ESP was made by an earlier SlopOS install.
fn esp_was_ours(
    disk: &Disk,
    plan: &install::Plan,
    boot: &slopos_boot_core::Guid,
    medium: &Medium,
) -> Result<bool, String> {
    let Place::Existing(n) = plan.esp else {
        return Ok(false);
    };
    let node = partition_node(&disk.node, &disk.partition_named(&n.to_string())?)?;
    boot::esp_is_ours(&node, boot, medium)
}

/// The existing partitions an install formats: those an install cut short
/// left without a volume.
#[derive(Clone, Copy)]
struct Formats {
    esp: bool,
    boot: bool,
}

/// A partition the install keeps must already be what it is used as: the ESP
/// a FAT32 volume with room for the loader. An ESP that starts no volume, and
/// SlopOS's own boot partition when it holds no FAT32 one, are what an install
/// cut short leaves, and are formatted rather than kept.
fn check_kept(disk: &Disk, plan: &install::Plan, medium: &Medium) -> Result<Formats, String> {
    let partitions = disk.partitions();
    let node_of = |place: Place| -> Result<Option<String>, String> {
        let Place::Existing(n) = place else {
            return Ok(None);
        };
        let p = partitions
            .iter()
            .find(|p| p.entry.number == n)
            .ok_or_else(|| format!("{} has no partition {n}", disk.node))?;
        partition_node(&disk.node, p).map(Some)
    };
    let esp = match node_of(plan.esp)? {
        None => false,
        Some(node) => match open_fat(&node) {
            Ok(volume) => {
                let loader = fs::metadata(medium.path(layout::MEDIUM_LOADER))
                    .map_err(|e| format!("{}: {e}", layout::MEDIUM_LOADER))?
                    .len();
                let free = u64::from(volume.free_clusters()) * volume.cluster_bytes() as u64;
                if free < loader + ESP_SLACK_BYTES {
                    return Err(format!(
                        "the ESP {node} has {} free; the loader needs {}",
                        human(free),
                        human(loader + ESP_SLACK_BYTES)
                    ));
                }
                false
            }
            Err(_) if disk::holds_no_volume(&node) => true,
            Err(why) => {
                return Err(format!(
                    "the ESP {node} holds a volume this installer cannot use: {why}"
                ));
            }
        },
    };
    Ok(Formats {
        esp,
        boot: node_of(plan.boot)?.is_some_and(|node| open_fat(&node).is_err()),
    })
}

fn summarise(s: &Settled, medium: &Medium) {
    println!("\nSlopOS will be installed on {}:", s.disk.node);
    match s.mode {
        Mode::Erase => println!(
            "  a new partition table: EVERYTHING on {} is lost",
            s.disk.node
        ),
        _ => println!("  new entries in its partition table; every other partition stays as it is"),
    }
    for role in ROLES {
        println!(
            "  {}: {}",
            role.name(),
            describe_place(
                &s.disk,
                role,
                s.plan.place(role),
                match role {
                    Role::Esp => s.format.esp,
                    Role::Boot => s.format.boot,
                    Role::Root | Role::Crash => !s.keep_root,
                },
                s.own_esp,
            )
        );
    }
    if medium.payload_has(root::TOOLCHAIN) {
        println!("  /usr/local: the toolchain the medium carries");
    }
    match &s.remote {
        Some(url) if s.keep_root => println!(
            "  /src/slopos: the root's own if it has one, else the medium's clone, fetching from {url}"
        ),
        Some(url) => println!("  /src/slopos: the medium's clone, fetching from {url}"),
        None => {}
    }
    println!("  boots with: {}", s.cmdline);
    println!(
        "  firmware entry \"{}\": {}",
        layout::FIRMWARE_ENTRY,
        if s.first {
            "first in the boot order"
        } else {
            "where it stands in the boot order, or after the systems there"
        }
    );
}

fn confirm(s: &Settled) -> Result<(), String> {
    let (want, question) = match s.mode {
        Mode::Erase => {
            let name = s
                .disk
                .node
                .rsplit('/')
                .next()
                .unwrap_or(&s.disk.node)
                .to_owned();
            let question = format!("Type {name} to erase it");
            (name, question)
        }
        _ => ("yes".to_owned(), "Type yes to write the disk".to_owned()),
    };
    let answer = ask(&question, None, "--yes")?;
    if answer == want {
        Ok(())
    } else {
        Err("nothing was written".to_owned())
    }
}

/// The plan's partitions, as the table the kernel re-read names them: each
/// entry must be the one written, GUIDs and blocks alike, since the loader's
/// configuration was rendered from those.
fn placed(s: &Settled) -> Result<[(Partition, String); 4], String> {
    let node = &s.disk.node;
    let reread = Disk::open(node)?.partitions();
    let written: Vec<Partition> = s
        .header
        .partitions(&s.array)
        .filter_map(Result::ok)
        .collect();
    let find = |role: Role| -> Result<(Partition, String), String> {
        let number = s.plan.place(role).number();
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

fn step(n: u32, what: &str) {
    println!("installer: [{n}/9] {what}");
}

fn install(s: Settled, medium: &Medium) -> Result<(), String> {
    step(1, "writing the partition table");
    for role in ROLES {
        if let Place::New {
            first_lba,
            last_lba,
            ..
        } = s.plan.place(role)
        {
            disk::clear_start(&s.disk, first_lba, last_lba)?;
        }
    }
    disk::write_table(&s.disk, &s.header, &s.array, s.mode == Mode::Erase)?;
    step(2, "re-reading it");
    disk::reread(&s.disk)?;
    let [
        (esp, esp_node),
        (_, boot_node),
        (_, root_node),
        (_, crash_node),
    ] = placed(&s)?;

    step(3, "formatting the new partitions");
    if s.plan.esp.is_new() || s.format.esp {
        disk::format_fat(&esp_node, ESP_LABEL)?;
        println!("installer:   {esp_node}: FAT32, {ESP_LABEL}");
    }
    if s.plan.boot.is_new() || s.format.boot {
        disk::format_fat(&boot_node, BOOT_LABEL)?;
        println!("installer:   {boot_node}: FAT32, {BOOT_LABEL}");
    }
    // Records of the system a formatted root held, or what an install cut
    // short left, would read as this one's.
    if s.plan.crash.is_new() || !s.keep_root {
        disk::zero(&crash_node)?;
        println!("installer:   {crash_node}: zeroed for crash records");
    }
    if !s.keep_root {
        disk::mke2fs(&root_node)?;
        println!("installer:   {root_node}: ext4");
    }

    step(4, "filling the root");
    let mounted = root::Mounted::at(&root_node)?;
    root::lay_out(&mounted)?;
    if s.keep_root {
        root::forget_slots(&mounted)?;
    }
    root::fill(&mounted, medium, s.remote.as_deref())?;
    mounted.finish()?;

    step(5, "writing both slots");
    boot::write_slots(&boot_node, medium)?;

    step(6, "writing the loader and its configuration");
    boot::write_loader(&esp_node, s.own_esp, &s.limine_conf, medium)?;

    step(7, "making slot a the default");
    let entry = boot::set_default()?;
    println!(
        "installer:   {} is {entry}",
        slopos_boot_core::bli::ENTRY_DEFAULT
    );

    step(8, "checking the root");
    disk::e2fsck(&root_node)?;

    step(9, "registering the firmware entry");
    let number = boot::register(esp, boot_node, s.first)?;
    println!(
        "installer: SlopOS is installed on {}, booted by the firmware entry Boot{number:04X}; take the medium out and restart",
        s.disk.node
    );
    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    let opts = parse(args)?;
    let medium = Medium::find()?;
    let settled = settle(opts, &medium)?;
    install(settled, &medium)
}

pub fn installer_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(message) = run(&args) {
        eprintln!("installer: {message}");
        sys_core::exit_with_code(1);
    }
}
