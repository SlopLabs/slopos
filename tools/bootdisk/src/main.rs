//! What `scripts/build_bootdisk.sh` asks of `boot-core`, so the disk the host
//! builds and the one the installer lays out read one statement of the
//! layout and one renderer of the Limine configuration.
//!
//! Usage:
//!   `bootdisk layout` — the layout as shell assignments
//!   `bootdisk limine-conf --boot <partuuid> [--root <partuuid>]
//!     [--resolution <w>x<h>] [--timeout <s>] [--serial]
//!     --slot <slot>[:<cmdline>]... -- <cmdline>`
//!   `bootdisk medium <image> <disk guid> [<payload>]` — hold a built install
//!     medium's table to what the kernel finds its payload by

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::process::ExitCode;

use slopos_boot_core::Guid;
use slopos_boot_core::gpt::{self, Geometry, Header, Partition};
use slopos_boot_core::layout::{
    ALIGN_BYTES, BASE_FILE, BOOT_BYTES, BOOT_LABEL, BOOT_NAME, BOOT_TYPE, CRASH_BYTES, CRASH_NAME,
    CRASH_TYPE, ESP_LABEL, ESP_NAME, ESP_TYPE, FALLBACK_CONFIG, FALLBACK_LOADER, KERNEL_FILE,
    LOADER, LOADER_CONFIG, LOADER_DIR, LOADER_LICENSE, LOADER_NOTICES, MEDIUM_BASE, MEDIUM_DIR,
    MEDIUM_DISK_GUID, MEDIUM_KERNEL, MEDIUM_LOADER, MEDIUM_LOADER_LICENSE, MEDIUM_LOADER_NOTICES,
    MEDIUM_MODULE, MEDIUM_NOTICE, NEW_ESP_BYTES, PAYLOAD_DIR, PAYLOAD_TYPE, ROOT_NAME, ROOT_TYPE,
    SLOTS, SLOTS_DIR,
};
use slopos_boot_core::limine::{Config, MenuEntry};

const MIB: u64 = 1 << 20;

fn layout() {
    let guids = [
        ("ESP_TYPE", ESP_TYPE),
        ("BOOT_TYPE", BOOT_TYPE),
        ("ROOT_TYPE", ROOT_TYPE),
        ("CRASH_TYPE", CRASH_TYPE),
        ("PAYLOAD_TYPE", PAYLOAD_TYPE),
    ];
    for (name, guid) in guids {
        println!("{name}={guid}");
    }
    let sizes = [
        ("ALIGN_MIB", ALIGN_BYTES),
        ("NEW_ESP_MIB", NEW_ESP_BYTES),
        ("BOOT_MIB", BOOT_BYTES),
        ("CRASH_MIB", CRASH_BYTES),
    ];
    for (name, bytes) in sizes {
        println!("{name}={}", bytes / MIB);
    }
    let slots = SLOTS.join(" ");
    let texts = [
        ("ESP_NAME", ESP_NAME),
        ("BOOT_NAME", BOOT_NAME),
        ("ROOT_NAME", ROOT_NAME),
        ("CRASH_NAME", CRASH_NAME),
        ("ESP_LABEL", ESP_LABEL),
        ("BOOT_LABEL", BOOT_LABEL),
        ("LOADER_DIR", LOADER_DIR),
        ("LOADER", LOADER),
        ("LOADER_CONFIG", LOADER_CONFIG),
        ("LOADER_LICENSE", LOADER_LICENSE),
        ("LOADER_NOTICES", LOADER_NOTICES),
        ("FALLBACK_LOADER", FALLBACK_LOADER),
        ("FALLBACK_CONFIG", FALLBACK_CONFIG),
        ("SLOTS", &slots),
        ("SLOTS_DIR", SLOTS_DIR),
        ("KERNEL_FILE", KERNEL_FILE),
        ("BASE_FILE", BASE_FILE),
        ("MEDIUM_DIR", MEDIUM_DIR),
        ("MEDIUM_MODULE", MEDIUM_MODULE),
        ("MEDIUM_KERNEL", MEDIUM_KERNEL),
        ("MEDIUM_BASE", MEDIUM_BASE),
        ("MEDIUM_LOADER", MEDIUM_LOADER),
        ("MEDIUM_LOADER_LICENSE", MEDIUM_LOADER_LICENSE),
        ("MEDIUM_LOADER_NOTICES", MEDIUM_LOADER_NOTICES),
        ("MEDIUM_NOTICE", MEDIUM_NOTICE),
        ("MEDIUM_DISK_GUID", MEDIUM_DISK_GUID),
        ("PAYLOAD_DIR", PAYLOAD_DIR),
    ];
    for (name, text) in texts {
        println!("{name}='{text}'");
    }
}

fn guid(text: &str) -> Result<Guid, String> {
    Guid::parse(text).ok_or_else(|| format!("{text}: not a GUID"))
}

fn limine_conf(args: &[String]) -> Result<(), String> {
    let mut boot = None;
    let mut root = None;
    let mut resolution = None;
    let mut timeout = 0;
    let mut serial = false;
    let mut slots = Vec::new();
    let mut rest = args.iter();
    let cmdline = loop {
        let Some(flag) = rest.next() else {
            return Err("no -- before the command line".into());
        };
        let mut value = || rest.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--boot" => boot = Some(guid(value()?)?),
            "--root" => root = Some(guid(value()?)?),
            "--resolution" => {
                let text = value()?;
                let (w, h) = text
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                    .ok_or_else(|| format!("{text}: not <w>x<h>"))?;
                resolution = Some((w, h));
            }
            "--timeout" => {
                timeout = value()?
                    .parse()
                    .map_err(|_| "--timeout takes seconds".to_owned())?
            }
            "--serial" => serial = true,
            "--slot" => {
                let text = value()?;
                slots.push(text.split_once(':').unwrap_or((text, "")));
            }
            "--" => break rest.as_slice().join(" "),
            other => return Err(format!("unknown flag {other}")),
        }
    };
    let menu: Vec<MenuEntry<'_>> = slots
        .iter()
        .map(|&(slot, cmdline)| MenuEntry::Slot { slot, cmdline })
        .collect();
    let mut text = String::new();
    Config {
        timeout,
        serial,
        boot_partition: boot.ok_or("--boot is required")?,
        root_partition: root,
        cmdline: &cmdline,
        resolution,
        entries: &menu,
    }
    .render(&mut text)
    .map_err(|e| format!("the configuration does not render: {e:?}"))?;
    print!("{text}");
    Ok(())
}

/// The table copy at `lba` of a 512-byte-sector image, every entry of it
/// whole: the header, then the array it names.
fn table_at(image: &File, geometry: Geometry, lba: u64) -> Result<(Guid, Vec<Partition>), String> {
    let read = |at: Option<u64>, len: usize| {
        let at = at.ok_or("an offset past 64 bits")?;
        let mut bytes = vec![0u8; len];
        image
            .read_exact_at(&mut bytes, at)
            .map(|()| bytes)
            .map_err(|e| format!("reading at {at}: {e}"))
    };
    let block = read(geometry.byte_of(lba), geometry.block() as usize)?;
    let header = Header::parse(&block, lba, geometry)
        .map_err(|why| format!("the table at LBA {lba} is {why:?}"))?;
    let array = read(geometry.byte_of(header.entry_lba()), header.array_bytes())?;
    if !header.array_matches(&array) {
        return Err(format!("the array of the table at LBA {lba} fails its CRC"));
    }
    let partitions = header
        .partitions(&array)
        .map(|p| p.map_err(|skipped| format!("entry {} is {:?}", skipped.number, skipped.why)))
        .collect::<Result<_, _>>()?;
    Ok((header.disk_guid(), partitions))
}

fn at_most_one(partitions: &[Partition], type_guid: Guid) -> Result<Option<Partition>, String> {
    let mut typed = partitions.iter().filter(|p| p.entry.type_guid == type_guid);
    match (typed.next(), typed.next()) {
        (Some(_), Some(_)) => Err(format!("more than one partition of type {type_guid}")),
        (one, _) => Ok(one.copied()),
    }
}

/// The payload's window in the copy at `lba`, which must name `disk` and
/// one ESP.
fn payload_window(
    image: &File,
    geometry: Geometry,
    lba: u64,
    disk: Guid,
) -> Result<Option<(u64, u64)>, String> {
    let (named, partitions) = table_at(image, geometry, lba)?;
    if named != disk {
        return Err(format!("the table at LBA {lba} names {named}, not {disk}"));
    }
    at_most_one(&partitions, ESP_TYPE)?.ok_or("no EFI system partition")?;
    Ok(at_most_one(&partitions, PAYLOAD_TYPE)?.map(|p| (p.start, p.len)))
}

/// Both copies of `image`'s table, each read whole, naming `disk`, one ESP
/// and the same payload partition, which holds `payload` byte for byte; or
/// none, without `payload`.
fn medium(image: &str, disk: &str, payload: Option<&str>) -> Result<(), String> {
    let disk = guid(disk)?;
    let file = File::open(image).map_err(|e| format!("{image}: {e}"))?;
    let len = file.metadata().map_err(|e| format!("{image}: {e}"))?.len();
    let geometry = Geometry::new(len, 512).ok_or("no geometry")?;
    let in_image = |why: String| format!("{image}: {why}");
    let window = payload_window(&file, geometry, gpt::PRIMARY_LBA, disk).map_err(in_image)?;
    if payload_window(&file, geometry, geometry.backup_lba(), disk).map_err(in_image)? != window {
        return Err(in_image(
            "the two copies name different payloads".to_owned(),
        ));
    }
    match (window, payload) {
        (None, None) => Ok(()),
        (Some((start, _)), None) => Err(in_image(format!(
            "a payload partition at byte {start}, built without one"
        ))),
        (None, Some(_)) => Err(in_image("no payload partition".to_owned())),
        (Some(window), Some(path)) => held_to(&file, window, path).map_err(in_image),
    }
}

/// Fails unless the `(start, len)` window of `image` is the file at `path`,
/// byte for byte.
fn held_to(image: &File, (start, len): (u64, u64), path: &str) -> Result<(), String> {
    let payload = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let size = payload
        .metadata()
        .map_err(|e| format!("{path}: {e}"))?
        .len();
    if size != len {
        return Err(format!(
            "the payload partition holds {len} bytes, {path} {size}"
        ));
    }
    let mut want = vec![0u8; 1 << 20];
    let mut got = vec![0u8; 1 << 20];
    let mut at = 0;
    while at < len {
        let n = (len - at).min(want.len() as u64) as usize;
        payload
            .read_exact_at(&mut want[..n], at)
            .map_err(|e| format!("{path}: {e}"))?;
        image
            .read_exact_at(&mut got[..n], start + at)
            .map_err(|e| e.to_string())?;
        if want[..n] != got[..n] {
            return Err(format!(
                "the payload partition differs from {path} in the MiB at byte {at}"
            ));
        }
        at += n as u64;
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("layout") if args.len() == 1 => {
            layout();
            Ok(())
        }
        Some("limine-conf") => limine_conf(&args[1..]),
        Some("medium") if (3..=4).contains(&args.len()) => {
            medium(&args[1], &args[2], args.get(3).map(String::as_str))
        }
        _ => Err("usage: bootdisk layout | bootdisk limine-conf ... | bootdisk medium ...".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("bootdisk: {why}");
            ExitCode::FAILURE
        }
    }
}
