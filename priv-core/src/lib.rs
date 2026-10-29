//! What `/sbin/privd` decides and what it says, apart from the system calls
//! that carry it: the action policy, the request protocol a requester speaks
//! over `/run/privd`, and the consent protocol privd speaks to the compositor.
//!
//! A requester sends one frame per request, framed as `slopos_remote_core`
//! frames, its descriptors riding along as `SCM_RIGHTS`:
//!
//! ```text
//! client REQUEST{id, action, arg.., cwd?, terminal?}
//!                                          + stdin, stdout, stderr[, cwd]
//!   privd  ASKING{id, app?}                the person at the machine is asked
//!   privd  PROMPT{id, prompt, about}       or its terminal, for a password
//!   client SECRET{id, secret}
//!   privd  DENIED{id, reason, person?}
//!   privd  STARTED{id, pid}
//!   client SIGNAL{id, signo}..             relayed, or withdraws what has not started
//!   privd  EXITED{id, code | signal}
//! ```
//!
//! A connection carries one request at a time and any number in turn.
//!
//! The consent channel is a socket pair init hands privd and the compositor:
//!
//! ```text
//! either RESET{epoch}                   a new instance, see `consent::Link`
//!   other RESET_SEEN{epoch}
//! agent READY{version}                  it can draw a dialog
//! privd ASK{id, action, describe, program?, uid, pid, cwd, arg..,
//!           kind, person?, admin.., app?, icon?, detail?, verb,
//!           destructive?, extension?}
//!   agent SHOWN{id}                     on screen
//!   agent ANSWER{id, allow, secret?, as?}
//!   privd CANCEL{id}                    withdrawn, expired or hidden
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod consent;
pub mod policy;
pub mod sessions;
pub mod wire;
pub mod words;

pub use policy::{
    Action, Allow, Level, People, Person, Policy, PolicyError, Principal, Question, Reach, Refused,
    Requester, RunsAs, Shape, Verdict,
};

use slopos_abi::caps;

/// Where privd listens; anyone may connect, and the policy answers.
pub const SOCKET: &str = "/run/privd";
/// The policy, in the sealed base, so no running program can edit it.
pub const POLICY: &str = "/usr/share/slopos/privd/actions";
/// privd itself, as `SO_PEERPROGRAM` names the real one.
pub const PROGRAM: &str = "/sbin/privd";
/// Who makes the consent channel, as `SO_PEERPROGRAM` names it at either end.
pub const INIT: &str = "/sbin/init";
/// The descriptor of an inherited connection, named in the environment.
pub const ENV_FD: &str = "PRIVD_FD";
/// The action privd started a mechanism for.
pub const ENV_ACTION: &str = "PRIV_ACTION";
/// The uid of the requester privd started a mechanism for, as its connection
/// recorded it.
pub const ENV_UID: &str = "PRIV_UID";
/// The search path a base mechanism sees; an extension's sees its own `bin`
/// after it.
pub const MECHANISM_PATH: &str = "/bin:/sbin";
pub const MECHANISM_LANG: &str = "C.UTF-8";

/// All an action may name.
pub const DELEGABLE: u64 = caps::POWER
    | caps::PROC_SIGNAL
    | caps::MOUNT
    | caps::RAW_BLOCK
    | caps::NET_ADMIN
    | caps::CONSOLE_CONFIG
    | caps::SYS_INSPECT
    | caps::CLOCK
    | caps::SEAL
    | caps::BOOT_ENTRY
    | caps::DAC_OVERRIDE
    | caps::BASE_UPDATE
    | caps::EXTENSION_UPDATE;

/// The most an action run as its requester may name: mounts, the network,
/// the console and the clock, and every file but a sealed one. Nothing that
/// writes a raw disk, the base, an extension, a seal, the firmware's or the
/// loader's variables or another identity.
pub const REQUESTER_CEILING: u64 = caps::MOUNT
    | caps::NET_ADMIN
    | caps::CONSOLE_CONFIG
    | caps::SYS_INSPECT
    | caps::CLOCK
    | caps::DAC_OVERRIDE;

/// What init hands privd: what an action may name, among it `ProcSignal`,
/// which relays a requester's signals to a mechanism running as another uid,
/// and what no action names: `SetId`, to start a mechanism as an extension's
/// user or as its requester.
pub const PRIVD_CAPS: u64 = DELEGABLE | caps::SET_ID;

/// Every capability, by the name `slopos_ostd::authority::Capability` gives
/// it.
pub const CAPABILITIES: [(&str, u64); 22] = [
    ("Power", caps::POWER),
    ("ProcSignal", caps::PROC_SIGNAL),
    ("SysInspect", caps::SYS_INSPECT),
    ("DisplaySeat", caps::DISPLAY_SEAT),
    ("InputSeat", caps::INPUT_SEAT),
    ("ConsoleConfig", caps::CONSOLE_CONFIG),
    ("ConsoleIo", caps::CONSOLE_IO),
    ("ClipboardGlobal", caps::CLIPBOARD_GLOBAL),
    ("Fate", caps::FATE),
    ("TestHarness", caps::TEST_HARNESS),
    ("Mount", caps::MOUNT),
    ("Clock", caps::CLOCK),
    ("Seal", caps::SEAL),
    ("BootEntry", caps::BOOT_ENTRY),
    ("NetAdmin", caps::NET_ADMIN),
    ("RawBlock", caps::RAW_BLOCK),
    ("SetId", caps::SET_ID),
    ("DacOverride", caps::DAC_OVERRIDE),
    ("SchedHigh", caps::SCHED_HIGH),
    ("BaseUpdate", caps::BASE_UPDATE),
    ("ExtensionUpdate", caps::EXTENSION_UPDATE),
    ("SeatSwitch", caps::SEAT_SWITCH),
];

const _: () = {
    let mut all = 0;
    let mut i = 0;
    while i < CAPABILITIES.len() {
        assert!(all & CAPABILITIES[i].1 == 0);
        all |= CAPABILITIES[i].1;
        i += 1;
    }
    assert!(all == caps::ALL);
};

pub fn capability(name: &str) -> Option<u64> {
    CAPABILITIES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|&(_, bit)| bit)
}

/// The names of the capabilities in `mask`, in [`CAPABILITIES`]' order.
pub fn capability_names(mask: u64) -> impl Iterator<Item = &'static str> {
    CAPABILITIES
        .iter()
        .filter(move |(_, bit)| mask & bit != 0)
        .map(|&(name, _)| name)
}

/// Whether an ELF image names a program interpreter, `None` when it is not a
/// 64-bit little-endian ELF image whose program headers it holds.
pub fn elf_interpreted(image: &[u8]) -> Option<bool> {
    const PT_INTERP: u32 = 3;
    if image.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let u16_at = |at: usize| Some(u16::from_le_bytes(image.get(at..at + 2)?.try_into().ok()?));
    let phoff = usize::try_from(u64::from_le_bytes(image.get(32..40)?.try_into().ok()?)).ok()?;
    let phentsize = usize::from(u16_at(54)?);
    let phnum = usize::from(u16_at(56)?);
    if phentsize < 4 {
        return None;
    }
    let mut interpreted = false;
    for i in 0..phnum {
        let at = phoff.checked_add(i.checked_mul(phentsize)?)?;
        let header = image.get(at..at.checked_add(phentsize)?)?;
        let kind = u32::from_le_bytes(header[..4].try_into().ok()?);
        interpreted |= kind == PT_INTERP;
    }
    Some(interpreted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn image(types: &[u32]) -> Vec<u8> {
        let mut out = vec![0u8; 64 + 56 * types.len()];
        out[..6].copy_from_slice(b"\x7fELF\x02\x01");
        out[32..40].copy_from_slice(&64u64.to_le_bytes());
        out[54..56].copy_from_slice(&56u16.to_le_bytes());
        out[56..58].copy_from_slice(&(types.len() as u16).to_le_bytes());
        for (i, kind) in types.iter().enumerate() {
            out[64 + 56 * i..68 + 56 * i].copy_from_slice(&kind.to_le_bytes());
        }
        out
    }

    #[test]
    fn an_interpreter_header_is_found_among_the_others() {
        assert_eq!(elf_interpreted(&image(&[1, 1, 4])), Some(false));
        assert_eq!(elf_interpreted(&image(&[6, 3, 1])), Some(true));
        assert_eq!(elf_interpreted(&image(&[])), Some(false));
    }

    #[test]
    fn what_is_not_a_whole_elf_image_is_unknown() {
        let good = image(&[1, 3]);
        for cut in 0..good.len() {
            assert_eq!(elf_interpreted(&good[..cut]), None, "{cut}");
        }
        let mut elf32 = good.clone();
        elf32[4] = 1;
        assert_eq!(elf_interpreted(&elf32), None);
        let mut far = good;
        far[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(elf_interpreted(&far), None);
    }
}
