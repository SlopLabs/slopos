//! The CPUID leaves that describe frequency control and core types.

/// `CPUID.06H`: thermal and power management.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PowerFeatures {
    /// `EAX[0]`: a digital thermal sensor per core (`IA32_THERM_STATUS`).
    pub dts: bool,
    /// `EAX[1]`: Turbo Boost.
    pub turbo: bool,
    /// `EAX[6]`: package thermal management (`IA32_PACKAGE_THERM_STATUS`).
    pub ptm: bool,
    /// `EAX[7]`: `IA32_PM_ENABLE`, `IA32_HWP_CAPABILITIES`, `IA32_HWP_REQUEST`
    /// and `IA32_HWP_STATUS`.
    pub hwp: bool,
    /// `EAX[8]`: `IA32_HWP_INTERRUPT`.
    pub hwp_notify: bool,
    /// `EAX[10]`: the energy-performance preference field of
    /// `IA32_HWP_REQUEST`.
    pub hwp_epp: bool,
    /// `EAX[11]`: `IA32_HWP_REQUEST_PKG`.
    pub hwp_pkg: bool,
    /// `ECX[0]`: `IA32_APERF` and `IA32_MPERF`.
    pub aperf_mperf: bool,
    /// `ECX[3]`: `IA32_ENERGY_PERF_BIAS`.
    pub epb: bool,
}

impl PowerFeatures {
    pub const LEAF: u32 = 0x06;

    pub const fn from_cpuid6(eax: u32, ecx: u32) -> Self {
        Self {
            dts: eax & 1 != 0,
            turbo: eax & (1 << 1) != 0,
            ptm: eax & (1 << 6) != 0,
            hwp: eax & (1 << 7) != 0,
            hwp_notify: eax & (1 << 8) != 0,
            hwp_epp: eax & (1 << 10) != 0,
            hwp_pkg: eax & (1 << 11) != 0,
            aperf_mperf: ecx & 1 != 0,
            epb: ecx & (1 << 3) != 0,
        }
    }
}

/// `CPUID.01H:ECX[7]`: Enhanced SpeedStep, the legacy `IA32_PERF_CTL`
/// interface.
pub const fn has_eist(ecx1: u32) -> bool {
    ecx1 & (1 << 7) != 0
}

/// `CPUID.07H.0:EDX[15]`: the package mixes core types.
pub const fn is_hybrid(edx7: u32) -> bool {
    edx7 & (1 << 15) != 0
}

/// The core type `CPUID.1AH:EAX[31:24]` reports for the CPU executing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreType {
    /// Leaf 0x1A absent, or a part that is not hybrid.
    Unreported,
    /// `0x20`: an efficiency core (Intel Atom).
    Atom,
    /// `0x40`: a performance core (Intel Core).
    Core,
    Other(u8),
}

impl CoreType {
    pub const LEAF: u32 = 0x1A;
    const ATOM: u8 = 0x20;
    const CORE: u8 = 0x40;

    pub const fn from_cpuid1a(eax: u32) -> Self {
        Self::from_raw((eax >> 24) as u8)
    }

    pub const fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Unreported,
            Self::ATOM => Self::Atom,
            Self::CORE => Self::Core,
            other => Self::Other(other),
        }
    }

    pub const fn raw(self) -> u8 {
        match self {
            Self::Unreported => 0,
            Self::Atom => Self::ATOM,
            Self::Core => Self::CORE,
            Self::Other(raw) => raw,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Unreported => "-",
            Self::Atom => "E",
            Self::Core => "P",
            Self::Other(_) => "?",
        }
    }
}

/// Where a logical CPU sits among its core's hardware threads, from
/// `CPUID.0BH` subleaf 0: `EAX[4:0]` is how many low bits of the x2APIC ID
/// (`EDX`) number the threads of one core, and `ECX[15:8]` is `1` when that
/// level is SMT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmtTopology {
    pub x2apic_id: u32,
    /// The thread's index within its core.
    pub thread: u32,
    /// The x2APIC ID with the thread bits shifted out: equal for siblings.
    pub core: u32,
}

impl SmtTopology {
    pub const LEAF: u32 = 0x0B;
    const LEVEL_TYPE_SMT: u32 = 1;

    /// `None` when subleaf 0 does not describe an SMT level, which is how a
    /// CPU without leaf 0xB answers.
    pub const fn from_cpuid_b(eax: u32, ebx: u32, ecx: u32, edx: u32) -> Option<Self> {
        if ebx & 0xFFFF == 0 || (ecx >> 8) & 0xFF != Self::LEVEL_TYPE_SMT {
            return None;
        }
        let shift = eax & 0x1F;
        Some(Self {
            x2apic_id: edx,
            thread: edx & ((1 << shift) - 1),
            core: edx >> shift,
        })
    }

    /// A CPU that reports no SMT level is a core of its own.
    pub const fn single(apic_id: u32) -> Self {
        Self {
            x2apic_id: apic_id,
            thread: 0,
            core: apic_id,
        }
    }
}
