//! Authority: the per-process, immutable, flat capability set.
//!
//! Every operation entry point names exactly one [`Capability`] at its
//! definition site. That value reaches the syscall table *through* the
//! handler, so the dispatcher's decision and the handler's witness read one
//! artifact and cannot diverge, and a `const` histogram over the table proves
//! the classification covers every slot.
//!
//! The claim is **no unchecked authority** — not "no ambient authority".
//! SlopOS is a Linux-ABI kernel whose syscalls take integers, so authorization
//! is a credential consulted against arguments, which is ambient by the
//! standard definition. What is new is that forgetting the check is a compile
//! error rather than a review miss.
//!
//! # Where authority does *not* live
//!
//! Where an operation names an object, authority rides on the resolved object
//! — a descriptor, a seat handle, a task reference — and not on the caller's
//! capability set. The capability axis is the residue: a slot needs a
//! capability only if its footprint is neither the caller nor an object the
//! caller already names, *and* no relation between caller and target answers
//! it. That derivation rule is what refuses a catch-all by construction: a
//! proposed `Admin` fails admission because its operations *do* name objects.

use core::marker::PhantomData;

use slopos_abi::Errno;

/// What an operation requires of its caller.
///
/// The three `None*` variants are counted separately rather than folded into
/// one, because a bare "none" keyword's equilibrium is every slot marked none
/// with a green gate. Distinguishing *why* an operation needs no capability
/// makes the distribution visible and the classification reviewable.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Capability {
    /// A dispatch-table slot with no handler. Not a classification — the
    /// absence of one.
    Unimplemented = 0,

    /// Acts only on the caller itself, or tells it what every process may
    /// know. `getpid`, `exit`, `brk`, `process_list`, `pidfd_open`.
    NoneSelf,
    /// Acts only on an object the caller already names by descriptor. The
    /// descriptor *is* the authority; re-checking a capability on top would be
    /// ceremony. `read`, `write`, `close`, every socket call.
    NoneFd,
    /// Answered by a relation between caller and target that the handler
    /// checks itself — parent-of, same-session, same-process. `waitpid`,
    /// `setpgid`, `set_cpu_affinity`.
    NoneRelation,

    /// Halt and reboot. Deletion condition: becomes a `/dev/power` descriptor
    /// delegated to init.
    Power,
    /// Signalling across a session boundary, or a fan-out that crosses one.
    ProcSignal,
    /// Observing the whole machine: the diagnostic console's informational
    /// commands and the sampling profiler. Never fused with a class that
    /// changes what it observes.
    SysInspect,
    /// Acquiring the screen seat. Deletion condition: collapses into the seat
    /// handle itself.
    DisplaySeat,
    /// Acquiring the input seat. Same deletion condition.
    InputSeat,
    /// Console reconfiguration: the font and the keyboard layout, both single
    /// global tables, and `vhangup`, which ends a whole session. Deletion
    /// condition: becomes an ioctl on the console descriptor.
    ConsoleConfig,
    /// Writing the global kernel console with no descriptor. Deletion
    /// condition: dies when it routes through the caller's controlling TTY.
    ConsoleIo,
    /// The global clipboard. Deletion condition: dies when the clipboard is
    /// memfd-plus-fd-passing only.
    ClipboardGlobal,
    /// The Wheel of Fate. Deletion condition: dies when the wheel is a
    /// descriptor init hands `/bin/roulette`.
    Fate,
    /// The test harness entry points. Empty in shipped images.
    TestHarness,

    /// Attaching and detaching filesystems. The mount table is one global
    /// namespace, so a mount changes every other process's view of every path
    /// but those pinned: the base's and the install medium's. Deletion
    /// condition: dies with per-namespace mounts on a descriptor for the
    /// directory covered.
    Mount,

    /// Setting the wall clock. One global anchor that every filesystem
    /// timestamp hangs off, so moving it backwards makes a build system's
    /// mtime comparisons lie. Not `Power`: it reaches no power primitive.
    /// Deletion condition: dies when the clock is an ioctl on a `/dev/rtc`
    /// descriptor delegated to init.
    Clock,

    /// Setting or clearing a file's immutable flag. The descriptor naming the
    /// file is not enough: the seal holds the base whole, which privd's `Base`
    /// identity rests on. The base's own inodes are sealed for good; this
    /// moves the seal on a writable volume, where it guards the directories a
    /// root keeps for the base and a system an image carries. Deletion
    /// condition: dies when no root holds either, the kernel making the mount
    /// points at boot and no image carrying a system of its own.
    Seal,
    /// Reaching the firmware's boot manager: reading and writing `Boot####`,
    /// `BootOrder` and `BootNext`, and reading `BootCurrent`. `Power` reaches
    /// only the Boot Loader Interface's variables and SlopOS's own; this is
    /// what lets an installer register its loader, and nothing in the global
    /// namespace beyond those four, so no holder enrols a Secure Boot key.
    /// Deletion condition: dies when firmware variables are objects named by
    /// descriptor and the installer is handed the boot manager's.
    BootEntry,
    /// Changing the network's configuration: interface state, addresses,
    /// routes, the resolver and the DHCP client. Reading it needs nothing.
    NetAdmin,
    /// Block devices beneath every filesystem: reading and writing the block
    /// nodes, `BLKRRPART`, the crash records, and no per-process `DiskBlocks`
    /// ceiling. A raw read bypasses every permission above it and recovers
    /// what a filesystem freed.
    RawBlock,
    /// Taking a uid or gid the process does not already hold, and setting
    /// the supplementary groups. Moving among its own real, effective and
    /// saved ids needs nothing.
    SetId,
    /// Bypassing permission bits: reading and writing any file, searching any
    /// directory, executing anything with an execute bit, and owning every
    /// file for `chmod`, `chown`, `utimensat` and the sticky bit. uid 0 owns
    /// the system's files and gets none of this from its uid.
    DacOverride,
    /// Running ahead of everything else: spawning at the `High` tier and
    /// setting a nice value below zero. One `loop {}` there starves the
    /// machine.
    SchedHigh,
    /// Making an archive the base every process runs from: `base_activate`.
    /// What the base holds is what privd's program identity names, so this
    /// is as much as writing the base. Deletion condition: dies when a base
    /// is activated only from a signed image the kernel checks itself.
    BaseUpdate,
    /// Activating and deactivating an extension: `ext_activate` and
    /// `ext_deactivate`. The kernel takes the caller's word for an image's
    /// digest and attests its programs by it, so this is as much as writing
    /// what privd's program identity names under `/opt`. Its holder stores
    /// whole images, so no per-process `DiskBlocks` ceiling binds it.
    /// Deletion condition: as `BaseUpdate`'s.
    ExtensionUpdate,
    /// Handing the screen and input seats from the session in front to
    /// another: `seat_switch`. The seats otherwise refuse an equal rank, so
    /// this is init's choice of whose desktop the person at the machine sees
    /// and types into. Deletion condition: dies when a session's seats are
    /// descriptors init revokes and grants.
    SeatSwitch,
}

impl Capability {
    /// Total over the enum, so a new variant must state its bit here.
    ///
    /// The `None*` and `Unimplemented` variants carry no bit: they are the
    /// absence of a requirement, and a mask bit for them would be a bit every
    /// process holds, which is not a capability.
    #[inline]
    pub const fn bit(self) -> u64 {
        use slopos_abi::caps;
        match self {
            Self::Unimplemented | Self::NoneSelf | Self::NoneFd | Self::NoneRelation => 0,
            Self::Power => caps::POWER,
            Self::ProcSignal => caps::PROC_SIGNAL,
            Self::SysInspect => caps::SYS_INSPECT,
            Self::DisplaySeat => caps::DISPLAY_SEAT,
            Self::InputSeat => caps::INPUT_SEAT,
            Self::ConsoleConfig => caps::CONSOLE_CONFIG,
            Self::ConsoleIo => caps::CONSOLE_IO,
            Self::ClipboardGlobal => caps::CLIPBOARD_GLOBAL,
            Self::Fate => caps::FATE,
            Self::TestHarness => caps::TEST_HARNESS,
            Self::Mount => caps::MOUNT,
            Self::Clock => caps::CLOCK,
            Self::Seal => caps::SEAL,
            Self::BootEntry => caps::BOOT_ENTRY,
            Self::NetAdmin => caps::NET_ADMIN,
            Self::RawBlock => caps::RAW_BLOCK,
            Self::SetId => caps::SET_ID,
            Self::DacOverride => caps::DAC_OVERRIDE,
            Self::SchedHigh => caps::SCHED_HIGH,
            Self::BaseUpdate => caps::BASE_UPDATE,
            Self::ExtensionUpdate => caps::EXTENSION_UPDATE,
            Self::SeatSwitch => caps::SEAT_SWITCH,
        }
    }

    /// Whether invoking this operation requires a capability at all.
    #[inline]
    pub const fn is_gated(self) -> bool {
        self.bit() != 0
    }

    /// Diagnostic name.
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unimplemented => "unimplemented",
            Self::NoneSelf => "none/self",
            Self::NoneFd => "none/fd",
            Self::NoneRelation => "none/relation",
            Self::Power => "Power",
            Self::ProcSignal => "ProcSignal",
            Self::SysInspect => "SysInspect",
            Self::DisplaySeat => "DisplaySeat",
            Self::InputSeat => "InputSeat",
            Self::ConsoleConfig => "ConsoleConfig",
            Self::ConsoleIo => "ConsoleIo",
            Self::ClipboardGlobal => "ClipboardGlobal",
            Self::Fate => "Fate",
            Self::TestHarness => "TestHarness",
            Self::Mount => "Mount",
            Self::Clock => "Clock",
            Self::Seal => "Seal",
            Self::BootEntry => "BootEntry",
            Self::NetAdmin => "NetAdmin",
            Self::RawBlock => "RawBlock",
            Self::SetId => "SetId",
            Self::DacOverride => "DacOverride",
            Self::SchedHigh => "SchedHigh",
            Self::BaseUpdate => "BaseUpdate",
            Self::ExtensionUpdate => "ExtensionUpdate",
            Self::SeatSwitch => "SeatSwitch",
        }
    }

    /// Every capability, for the histogram and the boot-time dump. Ordered by
    /// discriminant.
    pub const ALL: &'static [Capability] = &[
        Self::Unimplemented,
        Self::NoneSelf,
        Self::NoneFd,
        Self::NoneRelation,
        Self::Power,
        Self::ProcSignal,
        Self::SysInspect,
        Self::DisplaySeat,
        Self::InputSeat,
        Self::ConsoleConfig,
        Self::ConsoleIo,
        Self::ClipboardGlobal,
        Self::Fate,
        Self::TestHarness,
        Self::Mount,
        Self::Clock,
        Self::Seal,
        Self::BootEntry,
        Self::NetAdmin,
        Self::RawBlock,
        Self::SetId,
        Self::DacOverride,
        Self::SchedHigh,
        Self::BaseUpdate,
        Self::ExtensionUpdate,
        Self::SeatSwitch,
    ];
}

/// Every bit any capability names. A mask with a bit outside this is
/// malformed.
pub const CAP_MASK_ALL: u64 = {
    let mut mask = 0u64;
    let mut i = 0;
    while i < Capability::ALL.len() {
        mask |= Capability::ALL[i].bit();
        i += 1;
    }
    mask
};

const _: () = assert!(
    CAP_MASK_ALL == slopos_abi::caps::ALL,
    "every ABI capability bit needs a Capability, and every Capability an ABI bit",
);

/// No authority.
pub const CAP_NONE: u64 = 0;

/// What every program holds: globals with no object form yet, so withholding
/// one would break every program and protect nothing. Classified rather than
/// ungated so each one's deletion is a diff here.
pub const CAP_UNIVERSAL: u64 = slopos_abi::caps::UNIVERSAL;

mod cap_seal {
    pub trait Sealed {}
}

/// A capability, as a type.
///
/// Sealed: exactly the impls in this module exist, so a downstream crate
/// cannot mint a new capability kind and hand itself a witness for it.
pub trait CapKind: cap_seal::Sealed {
    const BIT: u64;
    const CAP: Capability;
}

/// Declare a marker type per gated capability, plus its sealed `CapKind`.
macro_rules! cap_kinds {
    ($($name:ident),+ $(,)?) => {
        $(
            #[derive(Copy, Clone, Debug)]
            pub struct $name;
            impl cap_seal::Sealed for $name {}
            impl CapKind for $name {
                const BIT: u64 = Capability::$name.bit();
                const CAP: Capability = Capability::$name;
            }
            const _: () = assert!(
                <$name as CapKind>::BIT != 0,
                "a capability marker type must name a gated capability",
            );
        )+
    };
}

cap_kinds!(
    Power,
    ProcSignal,
    SysInspect,
    DisplaySeat,
    InputSeat,
    ConsoleConfig,
    ConsoleIo,
    ClipboardGlobal,
    Fate,
    TestHarness,
    Mount,
    Seal,
    BootEntry,
);

/// Proof that a capability check ran, for the request it was minted in.
///
/// A ZST with private fields, `!Send`, no `Copy` and no `Clone`, and
/// deliberately **no** re-mint from a borrowed one — [`crate::sync::BspToken`]
/// has `from_witness`, but for authority that would be a laundering hole.
/// Branded to the borrow the checker was handed, so it cannot be stashed in a
/// static or captured beyond the request that produced it.
///
/// Unforgeable by *visibility*, not by `unsafe`: the constructor is private to
/// this module and [`check`] is the only thing that calls it. That is the
/// stronger form — an `unsafe impl` mint would be rejected by this tree's
/// zero-baseline contract-surface gate.
pub struct Cap<'ctx, R: CapKind> {
    _brand: PhantomData<fn(&'ctx ()) -> &'ctx ()>,
    _kind: PhantomData<fn() -> R>,
    _not_send: PhantomData<*const ()>,
}

const _: () = assert!(core::mem::size_of::<Cap<'static, Power>>() == 0);

impl<R: CapKind> core::fmt::Debug for Cap<'_, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Cap<{}>", R::CAP.name())
    }
}

impl<'ctx, R: CapKind> Cap<'ctx, R> {
    /// The only constructor. Private, and every caller is in this module.
    #[inline]
    const fn mint() -> Self {
        Self {
            _brand: PhantomData,
            _kind: PhantomData,
            _not_send: PhantomData,
        }
    }

    /// Which capability this witnesses. For diagnostics.
    #[inline]
    pub const fn capability(&self) -> Capability {
        R::CAP
    }
}

/// Check `mask` for `R` and mint the witness on success.
///
/// One `Relaxed` load and a mask at the call site — no RCU section, no
/// refcount traffic. The lifetime brand comes from `holder`, which callers
/// pass as the borrow the request already holds.
#[inline]
pub fn check_mask<'a, R: CapKind, T: ?Sized>(
    holder: &'a T,
    mask: u64,
) -> Result<Cap<'a, R>, Errno> {
    let _ = holder;
    if mask & R::BIT != 0 {
        Ok(Cap::mint())
    } else {
        Err(Errno::EPERM)
    }
}

/// Whether `mask` names `cap`. The dispatcher's check: one compare against a
/// byte in a cache line it has just touched.
///
/// An ungated capability is permitted by every mask, including the empty one —
/// that is what "needs no capability" means.
#[inline]
pub const fn mask_permits(mask: u64, cap: Capability) -> bool {
    let bit = cap.bit();
    bit == 0 || (mask & bit) != 0
}

/// Mint a `Power` witness for a kernel-initiated action.
///
/// `pub(crate)` so only [`crate::platform::power`] can reach it, and that
/// module re-exports it under a name the reachability gate greps for. The
/// authority of a kernel-initiated shutdown comes from *being the kernel*,
/// which no runtime check can establish; what keeps the caller set small is
/// the tracked list, not this function.
#[inline]
pub(crate) fn mint_kernel_power() -> Cap<'static, Power> {
    Cap::mint()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_gated_capability_has_a_distinct_bit() {
        let mut seen = 0u64;
        for cap in Capability::ALL {
            let bit = cap.bit();
            if bit == 0 {
                continue;
            }
            assert_eq!(bit & seen, 0, "{} reuses a bit", cap.name());
            assert_eq!(bit.count_ones(), 1, "{} is not a single bit", cap.name());
            seen |= bit;
        }
        assert_eq!(seen, CAP_MASK_ALL);
    }

    #[test]
    fn the_none_classes_carry_no_bit() {
        for cap in [
            Capability::Unimplemented,
            Capability::NoneSelf,
            Capability::NoneFd,
            Capability::NoneRelation,
        ] {
            assert_eq!(cap.bit(), 0, "{} must not be a mask bit", cap.name());
            assert!(!cap.is_gated());
        }
    }

    /// An ungated operation is permitted by the empty mask; that is what
    /// "needs no capability" means, and getting it backwards would deny every
    /// unprivileged syscall.
    #[test]
    fn an_ungated_capability_is_permitted_by_the_empty_mask() {
        for cap in [Capability::NoneSelf, Capability::NoneFd] {
            assert!(mask_permits(CAP_NONE, cap));
        }
        assert!(!mask_permits(CAP_NONE, Capability::Power));
        assert!(mask_permits(Capability::Power.bit(), Capability::Power));
    }

    #[test]
    fn a_witness_is_zero_sized_for_every_kind() {
        assert_eq!(core::mem::size_of::<Cap<'static, Power>>(), 0);
        assert_eq!(core::mem::size_of::<Cap<'static, Mount>>(), 0);
        assert_eq!(core::mem::size_of::<Cap<'static, TestHarness>>(), 0);
        assert_eq!(
            core::mem::size_of::<Result<Cap<'static, Power>, Errno>>(),
            core::mem::size_of::<Errno>(),
            "a ZST witness must not widen the Result the checker returns",
        );
    }

    #[test]
    fn check_mints_only_when_the_mask_names_the_capability() {
        let holder = ();
        assert!(check_mask::<Power, _>(&holder, Capability::Power.bit()).is_ok());
        assert!(check_mask::<Power, _>(&holder, CAP_NONE).is_err());
        // A different capability's bit does not mint this one.
        assert!(check_mask::<Power, _>(&holder, Capability::Mount.bit()).is_err());
    }
}
