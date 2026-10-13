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
    /// Raising authority at a program-identity grant. The one raise site.
    Launch,
    /// Signalling across a session boundary, or a fan-out that crosses one.
    ProcSignal,
    /// Observing the whole machine: the diagnostic console's informational
    /// commands, the sampling profiler, and every task in `process_list`.
    /// Never fused with a class that changes what it observes.
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
    /// — including the paths the program-identity grant table is keyed on.
    /// Deletion condition: dies with per-namespace mounts on a descriptor for
    /// the directory covered.
    Mount,

    /// Setting the wall clock. One global anchor that every filesystem
    /// timestamp hangs off, so moving it backwards makes a build system's
    /// mtime comparisons lie. Not `Power`: it reaches no power primitive.
    /// Deletion condition: dies when the clock is an ioctl on a `/dev/rtc`
    /// descriptor delegated to init.
    Clock,

    /// Setting or clearing a file's immutable flag. The descriptor naming the
    /// file is not enough: the flag is what holds a program-identity grant,
    /// keyed on a path, to the bytes it was granted for. Deletion condition:
    /// dies when grants are keyed on content rather than path.
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
            Self::Launch => caps::LAUNCH,
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
        }
    }

    /// Whether invoking this operation requires a capability at all.
    #[inline]
    pub const fn is_gated(self) -> bool {
        self.bit() != 0
    }

    /// Diagnostic name. Used by the dispatcher's `warn` mode, so a boot with
    /// `authority=warn` names the capability a program lacked.
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unimplemented => "unimplemented",
            Self::NoneSelf => "none/self",
            Self::NoneFd => "none/fd",
            Self::NoneRelation => "none/relation",
            Self::Power => "Power",
            Self::Launch => "Launch",
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
        Self::Launch,
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
    Launch,
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

/// Policy for what a failed authority check does.
///
/// Both modes exist because both OpenBSD and FreeBSD ended up needing them.
/// The failure mode is split by kind: invoking an operation your authority
/// does not name is a program bug and is loud; acting on an object you were
/// not given is a quiet `EPERM`.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum AuthorityMode {
    /// No checks. The measurement mode: the per-check cost is measured
    /// without a separate build.
    Off = 0,
    /// Report each distinct denial once and permit it, so one boot enumerates
    /// every capability the real desktop needs.
    Warn = 1,
    /// Refuse with `EPERM`.
    #[default]
    Enforce = 2,
}

impl AuthorityMode {
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[inline]
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            0 => Self::Off,
            1 => Self::Warn,
            _ => Self::Enforce,
        }
    }
}

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

static MODE: AtomicU8 = AtomicU8::new(AuthorityMode::Enforce as u8);

/// One bit per gated capability, so `warn` reports each distinct denial once
/// rather than once per call — a frame-rate denial would otherwise bury the
/// enumeration it exists to produce.
static WARNED: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn mode() -> AuthorityMode {
    AuthorityMode::from_u8(MODE.load(Ordering::Relaxed))
}

/// Set from the `authority=` cmdline knob, before userland runs.
#[inline]
pub fn set_mode(mode: AuthorityMode) {
    MODE.store(mode.as_u8(), Ordering::Relaxed);
    WARNED.store(0, Ordering::Relaxed);
}

/// Whether `cap`'s denial has already been reported. Marks it reported.
///
/// Returns `true` the first time only, so the caller logs once per capability.
pub fn warn_once(cap: Capability) -> bool {
    let bit = cap.bit();
    bit != 0 && WARNED.fetch_or(bit, Ordering::Relaxed) & bit == 0
}

/// The decision a dispatcher makes for one call.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AuthorityDecision {
    /// Proceed.
    Allow,
    /// Proceed, but this is the first denial of `cap` and it should be logged.
    WarnAndAllow,
    /// Refuse with `EPERM`.
    Deny,
}

/// Decide whether a caller holding `mask` may invoke an operation classified
/// `cap`, under the live mode.
///
/// Total and allocation-free: this is on the syscall entry path.
#[inline]
pub fn decide(mask: u64, cap: Capability) -> AuthorityDecision {
    if mask_permits(mask, cap) {
        return AuthorityDecision::Allow;
    }
    match mode() {
        AuthorityMode::Off => AuthorityDecision::Allow,
        AuthorityMode::Warn => {
            if warn_once(cap) {
                AuthorityDecision::WarnAndAllow
            } else {
                AuthorityDecision::Allow
            }
        }
        AuthorityMode::Enforce => AuthorityDecision::Deny,
    }
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
        assert_eq!(core::mem::size_of::<Cap<'static, Launch>>(), 0);
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
        assert!(check_mask::<Power, _>(&holder, Capability::Launch.bit()).is_err());
    }

    #[test]
    fn enforce_denies_and_off_allows() {
        set_mode(AuthorityMode::Enforce);
        assert_eq!(decide(CAP_NONE, Capability::Power), AuthorityDecision::Deny);
        set_mode(AuthorityMode::Off);
        assert_eq!(
            decide(CAP_NONE, Capability::Power),
            AuthorityDecision::Allow
        );
        set_mode(AuthorityMode::Enforce);
    }

    /// `warn` must report each distinct capability once, not once per call: a
    /// frame-rate denial would otherwise bury the enumeration it exists to
    /// produce.
    #[test]
    fn warn_reports_each_capability_once() {
        set_mode(AuthorityMode::Warn);
        assert_eq!(
            decide(CAP_NONE, Capability::Power),
            AuthorityDecision::WarnAndAllow
        );
        assert_eq!(
            decide(CAP_NONE, Capability::Power),
            AuthorityDecision::Allow
        );
        assert_eq!(
            decide(CAP_NONE, Capability::Fate),
            AuthorityDecision::WarnAndAllow,
            "a different capability still reports"
        );
        for expected in [AuthorityDecision::WarnAndAllow, AuthorityDecision::Allow] {
            assert_eq!(decide(CAP_NONE, Capability::RawBlock), expected);
        }
        set_mode(AuthorityMode::Enforce);
    }

    #[test]
    fn a_permitted_call_never_warns() {
        set_mode(AuthorityMode::Warn);
        let mask = Capability::Power.bit();
        assert_eq!(decide(mask, Capability::Power), AuthorityDecision::Allow);
        set_mode(AuthorityMode::Enforce);
    }

    #[test]
    fn mode_round_trips_through_its_encoding() {
        for mode in [
            AuthorityMode::Off,
            AuthorityMode::Warn,
            AuthorityMode::Enforce,
        ] {
            assert_eq!(AuthorityMode::from_u8(mode.as_u8()), mode);
        }
        assert_eq!(AuthorityMode::from_u8(200), AuthorityMode::Enforce);
    }
}
