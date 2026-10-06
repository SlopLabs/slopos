//! What `scripts/build_bootdisk.sh` asks of `boot-core`, so the disk the host
//! builds and the one the installer lays out read one statement of the
//! layout and one renderer of the Limine configuration.
//!
//! Usage:
//!   `bootdisk layout` — the layout as shell assignments
//!   `bootdisk limine-conf --boot <partuuid> [--root <partuuid>]
//!     [--resolution <w>x<h>] [--timeout <s>] [--serial]
//!     --slot <slot>[:<cmdline>]... -- <cmdline>`

use std::process::ExitCode;

use slopos_boot_core::Guid;
use slopos_boot_core::layout::{
    ALIGN_BYTES, BASE_FILE, BOOT_BYTES, BOOT_LABEL, BOOT_NAME, BOOT_TYPE, CRASH_BYTES, CRASH_NAME,
    CRASH_TYPE, ESP_LABEL, ESP_NAME, ESP_TYPE, FALLBACK_CONFIG, FALLBACK_LOADER, KERNEL_FILE,
    LOADER, LOADER_CONFIG, LOADER_DIR, LOADER_LICENSE, LOADER_NOTICES, MEDIUM_BASE, MEDIUM_DIR,
    MEDIUM_KERNEL, MEDIUM_LOADER, MEDIUM_LOADER_LICENSE, MEDIUM_LOADER_NOTICES, MEDIUM_MODULE,
    MEDIUM_NOTICE, NEW_ESP_BYTES, ROOT_NAME, ROOT_TYPE, SLOTS, SLOTS_DIR,
};
use slopos_boot_core::limine::{Config, MenuEntry};

const MIB: u64 = 1 << 20;

fn layout() {
    let guids = [
        ("ESP_TYPE", ESP_TYPE),
        ("BOOT_TYPE", BOOT_TYPE),
        ("ROOT_TYPE", ROOT_TYPE),
        ("CRASH_TYPE", CRASH_TYPE),
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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("layout") if args.len() == 1 => {
            layout();
            Ok(())
        }
        Some("limine-conf") => limine_conf(&args[1..]),
        _ => Err("usage: bootdisk layout | bootdisk limine-conf ...".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("bootdisk: {why}");
            ExitCode::FAILURE
        }
    }
}
