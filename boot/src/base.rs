//! Which of the boot entry's two bases this kernel boots: the boot
//! partition's current one when the kernel serves its interface, else the
//! previous. Chosen in the loader handoff, before memory is laid out, so the
//! pages of a base left unused join usable memory and nothing reads them
//! after.

use core::ops::{Range, RangeInclusive};

use slopos_abi::{BASE_INTERFACE, BASE_INTERFACE_MIN};
use slopos_boot_core::base::{self, Reading, Which};
use slopos_ostd::klog_info;
use slopos_ostd::sync::OnceLock;

pub const SERVED: RangeInclusive<u32> = BASE_INTERFACE_MIN..=BASE_INTERFACE;

#[derive(Clone, Copy, Debug)]
pub enum Choice<'a> {
    /// The entry loads no base.
    NoBase,
    Boots {
        which: Which,
        archive: &'a [u8],
        interface: u32,
        /// The other base, loaded and not booted, and what it read as.
        unused: Option<(Which, Reading)>,
    },
    Refused {
        current: Reading,
        previous: Reading,
    },
}

pub fn choose<'a>(current: Option<&'a [u8]>, previous: Option<&'a [u8]>) -> Choice<'a> {
    if current.is_none() && previous.is_none() {
        return Choice::NoBase;
    }
    let (now, before) = (Reading::of(current), Reading::of(previous));
    let refused = Choice::Refused {
        current: now,
        previous: before,
    };
    let (which, archive, reading, unused) = match base::choose(now, before, SERVED) {
        Some(which @ Which::Current) => (
            which,
            current,
            now,
            previous.map(|_| (Which::Previous, before)),
        ),
        Some(which @ Which::Previous) => (
            which,
            previous,
            before,
            current.map(|_| (Which::Current, now)),
        ),
        None => return refused,
    };
    let (Some(archive), Reading::Interface(interface)) = (archive, reading) else {
        return refused;
    };
    Choice::Boots {
        which,
        archive,
        interface,
        unused,
    }
}

/// One of the entry's bases as the loader placed it.
#[derive(Clone, Copy)]
pub struct Module {
    pub archive: &'static [u8],
    /// Its physical address, when known.
    pub at: Option<u64>,
}

impl Module {
    /// The whole 4 KiB pages it covers: nothing else can lie in one.
    fn frames(&self) -> Option<Range<u64>> {
        const PAGE: u64 = 4096;
        let start = self.at?.checked_next_multiple_of(PAGE)?;
        let end = self.at?.checked_add(self.archive.len() as u64)? / PAGE * PAGE;
        (start < end).then_some(start..end)
    }
}

struct Chosen {
    choice: Choice<'static>,
    unused_frames: Option<Range<u64>>,
}

static CHOSEN: OnceLock<Chosen> = OnceLock::new();

/// Choose once, from the modules the loader placed, and say what was chosen.
pub fn record(current: Option<Module>, previous: Option<Module>) {
    let choice = choose(current.map(|m| m.archive), previous.map(|m| m.archive));
    let mut unused_frames = None;
    match choice {
        Choice::NoBase => klog_info!("BOOT: the entry loads no base"),
        Choice::Boots {
            which,
            archive,
            interface,
            unused,
        } => {
            klog_info!(
                "BOOT: base {} ({} bytes), interface {}",
                which.name(),
                archive.len(),
                interface
            );
            if let Some((other, reading)) = unused {
                let module = match other {
                    Which::Current => current,
                    Which::Previous => previous,
                };
                unused_frames = module.and_then(|m| m.frames());
                let fate = match (unused_frames.is_some(), module.and_then(|m| m.at)) {
                    (true, _) => "its memory goes to the allocator",
                    (false, Some(_)) => "it covers no whole page to give the allocator",
                    (false, None) => "its address is unknown, so its memory stays out",
                };
                klog_info!(
                    "BOOT: base {} ({}) not booted; {}",
                    other.name(),
                    reading,
                    fate
                );
            }
        }
        Choice::Refused { current, previous } => klog_info!(
            "BOOT: no base this kernel serves (interfaces {}-{}): current {}, previous {}",
            SERVED.start(),
            SERVED.end(),
            current,
            previous
        ),
    }
    CHOSEN.call_once(|| Chosen {
        choice,
        unused_frames,
    });
}

pub fn choice() -> Choice<'static> {
    CHOSEN.get().map_or(Choice::NoBase, |chosen| chosen.choice)
}

/// The base the kernel boots, when it boots one.
pub fn booted() -> Option<&'static [u8]> {
    match choice() {
        Choice::Boots { archive, .. } => Some(archive),
        _ => None,
    }
}

/// The pages of the base left unused, which memory init hands out.
pub fn unused_frames() -> Option<Range<u64>> {
    CHOSEN.get()?.unused_frames.clone()
}
