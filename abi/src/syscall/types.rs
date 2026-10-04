#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserPollFd {
    pub fd: i32,
    pub events: u16,
    pub revents: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserTimeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

/// System information returned by SYSCALL_SYS_INFO
///
/// The `_pad` members are named on purpose: `copy_to_user` copies
/// `size_of::<Self>()` bytes, and implicit padding is uninitialized under the
/// Rust abstract machine, so a hole here is a repeatable disclosure of the
/// calling task's kernel stack.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserSysInfo {
    pub total_pages: u32,
    pub free_pages: u32,
    pub allocated_pages: u32,
    pub total_tasks: u32,
    pub active_tasks: u32,
    pub _pad0: u32,
    pub task_context_switches: u64,
    pub scheduler_context_switches: u64,
    pub scheduler_yields: u64,
    pub ready_tasks: u32,
    pub schedule_calls: u32,
    pub wl_balance: i64,
    pub boot_flags: u32,
    /// Pages the commit ledger may still promise; `u32::MAX` when it has no
    /// ceiling. What a build driver sizes its parallelism against.
    pub commit_headroom_pages: u32,
    pub commit_limit_pages: u32,
    pub committed_pages: u32,
    /// Processes the OOM killer has taken since boot.
    pub oom_kills: u32,
    /// The pid of the last of them; `INVALID_TASK_ID` before the first.
    pub oom_last_victim: u32,
}

const _: () = assert!(
    core::mem::size_of::<UserSysInfo>() == 88,
    "UserSysInfo must carry no implicit padding"
);

/// Linux x86-64 `struct sysinfo`, what [`SYSCALL_SYSINFO`](super::SYSCALL_SYSINFO)
/// writes. `loads` carry [`SI_LOAD_SHIFT`] fractional bits and every memory
/// figure counts `mem_unit` bytes.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct Sysinfo {
    pub uptime: i64,
    pub loads: [u64; 3],
    pub totalram: u64,
    pub freeram: u64,
    pub sharedram: u64,
    pub bufferram: u64,
    pub totalswap: u64,
    pub freeswap: u64,
    pub procs: u16,
    pub pad: u16,
    pub _pad0: u32,
    pub totalhigh: u64,
    pub freehigh: u64,
    pub mem_unit: u32,
    pub _pad1: u32,
}

const _: () = assert!(
    core::mem::size_of::<Sysinfo>() == 112,
    "Sysinfo must match the Linux x86-64 struct sysinfo"
);

pub const SI_LOAD_SHIFT: u32 = 16;

pub const BOOT_FLAG_ROULETTE_SKIP: u32 = 1 << 0;
pub const BOOT_FLAG_TESTS_ENABLED: u32 = 1 << 1;
/// `/` is backed by a block device: a write there survives the reboot. Clear
/// for a RAM root, whose successful `fsync` still loses the data at power-off.
pub const BOOT_FLAG_ROOT_PERSISTENT: u32 = 1 << 5;

/// POSIX `struct timespec`. Signed, as Linux and every libc have it:
/// `UTIME_OMIT` is a specific `tv_nsec` value and a pre-1970 `st_mtim` is
/// legal.
#[repr(C)]
#[derive(Default, Copy, Clone, PartialEq, Eq)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

const _: () = assert!(
    core::mem::size_of::<Timespec>() == 16,
    "Timespec must match the Linux x86-64 struct timespec"
);

/// `uname(2)` output — Linux x86-64 `struct new_utsname` exactly: six
/// NUL-terminated 65-byte fields, 390 bytes.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct UserUtsname {
    pub sysname: [u8; 65],
    pub nodename: [u8; 65],
    pub release: [u8; 65],
    pub version: [u8; 65],
    pub machine: [u8; 65],
    pub domainname: [u8; 65],
}

impl UserUtsname {
    pub const fn new() -> Self {
        Self {
            sysname: [0; 65],
            nodename: [0; 65],
            release: [0; 65],
            version: [0; 65],
            machine: [0; 65],
            domainname: [0; 65],
        }
    }
}

impl Default for UserUtsname {
    fn default() -> Self {
        Self::new()
    }
}

const _: () = assert!(
    core::mem::size_of::<UserUtsname>() == 390,
    "UserUtsname must match the Linux x86-64 struct new_utsname"
);

/// Per-task entry returned by SYSCALL_PROCESS_LIST.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserTaskEntry {
    pub task_id: u32,
    pub parent_task_id: u32,
    pub process_id: u32,
    pub state: u8,
    pub block_reason: u8,
    pub priority: u8,
    pub last_cpu: u8,
    pub cpu_affinity: u32,
    pub total_runtime_us: u64,
    pub creation_time_ms: u64,
    pub yield_count: u32,
    pub _pad: u32,
    pub name: [u8; 32],
}

/// CPU identification returned by SYSCALL_CPU_INFO.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UserCpuInfo {
    pub vendor: [u8; 16],
    pub brand_string: [u8; 48],
    pub cpu_count: u32,
    pub family: u8,
    pub model: u8,
    pub stepping: u8,
    pub _pad: u8,
    pub features: u64,
}

impl Default for UserCpuInfo {
    fn default() -> Self {
        Self {
            vendor: [0u8; 16],
            brand_string: [0u8; 48],
            cpu_count: 0,
            family: 0,
            model: 0,
            stepping: 0,
            _pad: 0,
            features: 0,
        }
    }
}

/// Per-CPU scheduler stats returned by SYSCALL_PERCPU_STATS.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserPerCpuStats {
    pub cpu_id: u32,
    pub _pad: u32,
    pub total_switches: u64,
    pub total_ticks: u64,
    pub idle_ticks: u64,
    pub ready_count: u32,
    pub _pad2: u32,
}

/// `UserCpuPerfInfo::flags`: `CPUID.06H:ECX[0]`, the APERF/MPERF pair.
pub const CPU_PERF_F_APERF_MPERF: u32 = 1 << 0;
/// `CPUID.06H:EAX[7]`: hardware-controlled performance states.
pub const CPU_PERF_F_HWP: u32 = 1 << 1;
/// `CPUID.06H:EAX[10]`: HWP takes an energy-performance preference.
pub const CPU_PERF_F_HWP_EPP: u32 = 1 << 2;
/// HWP is enabled now (`IA32_PM_ENABLE` bit 0).
pub const CPU_PERF_F_HWP_ACTIVE: u32 = 1 << 3;
/// The firmware had enabled HWP before the kernel looked.
pub const CPU_PERF_F_HWP_FIRMWARE: u32 = 1 << 4;
/// `CPUID.07H:EDX[15]`: the package mixes core types.
pub const CPU_PERF_F_HYBRID: u32 = 1 << 5;
/// `CPUID.06H:EAX[1]`: Turbo Boost.
pub const CPU_PERF_F_TURBO: u32 = 1 << 6;
/// The firmware disabled turbo (`IA32_MISC_ENABLE` bit 38).
pub const CPU_PERF_F_TURBO_DISABLED: u32 = 1 << 7;
/// `CPUID.01H:ECX[7]`: legacy `IA32_PERF_CTL` control.
pub const CPU_PERF_F_EIST: u32 = 1 << 8;
/// Running under a hypervisor, whose frequency registers are its own.
pub const CPU_PERF_F_HYPERVISOR: u32 = 1 << 9;
/// `CPUID.06H:EAX[0]`: a digital thermal sensor per core (`IA32_THERM_STATUS`).
pub const CPU_PERF_F_DTS: u32 = 1 << 10;
/// `CPUID.06H:EAX[6]`: package thermal management (`IA32_PACKAGE_THERM_STATUS`).
pub const CPU_PERF_F_PTM: u32 = 1 << 11;

/// `UserCpuPerfInfo::policy`: the firmware's frequency settings, untouched.
pub const CPU_PERF_POLICY_FIRMWARE: u32 = 0;
/// HWP enabled, autonomous between the configured limits.
pub const CPU_PERF_POLICY_HWP: u32 = 1;

/// `UserCpuPerfInfo::placement`: every idle CPU is as good as another.
pub const CPU_PERF_PLACEMENT_FLAT: u32 = 0;
/// Idle CPUs ranked: an idle P-core, then an E-core, then a busy core's
/// sibling thread.
pub const CPU_PERF_PLACEMENT_RANKED: u32 = 1;

/// `cpu_perf_ctl` operations.
/// Enable HWP on every CPU; `value` is ignored. HWP stays on until reset.
pub const CPU_PERF_OP_HWP: u64 = 1;
/// `value` is the energy-performance preference, 0-255.
pub const CPU_PERF_OP_EPP: u64 = 2;
/// `value` is `min | max << 8` in HWP levels; `0` is the CPU's own bound.
pub const CPU_PERF_OP_LIMITS: u64 = 3;
/// `value` is a `CPU_PERF_PLACEMENT_*`.
pub const CPU_PERF_OP_PLACEMENT: u64 = 4;

/// The machine-wide half of `SYSCALL_CPU_PERF`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserCpuPerfInfo {
    /// `CPU_PERF_F_*`.
    pub flags: u32,
    /// `CPU_PERF_POLICY_*` in effect.
    pub policy: u32,
    /// The energy-performance preference HWP is given.
    pub epp: u32,
    /// The HWP limits asked for, `min | max << 8`.
    pub limits: u32,
    /// `CPU_PERF_PLACEMENT_*` in effect.
    pub placement: u32,
    /// Bumped by every settings change; a CPU has applied the settings once
    /// its `UserCpuPerf::applied` equals it.
    pub generation: u32,
    /// The TSC's rate, which `IA32_MPERF` also counts at; `0` until it has
    /// been measured.
    pub tsc_khz: u64,
    /// `CPUID.16H`: base, maximum and bus frequency in MHz, `0` unreported.
    pub base_mhz: u32,
    pub max_mhz: u32,
    pub bus_mhz: u32,
    pub _pad0: u32,
    /// As the firmware left them, read on the boot CPU before the kernel
    /// wrote any: `IA32_PM_ENABLE` (`0` without HWP), `IA32_MISC_ENABLE`, and
    /// `MSR_PLATFORM_INFO` (`0` under a hypervisor).
    pub boot_pm_enable: u64,
    pub boot_misc_enable: u64,
    pub platform_info: u64,
    /// `MSR_TEMPERATURE_TARGET` (bits 23:16 the throttling point), `0`
    /// without a thermal sensor or under a hypervisor.
    pub temperature_target: u64,
    /// `IA32_PACKAGE_THERM_STATUS` as CPU 0 last sampled it, `0` without one.
    pub package_therm_status: u64,
}

/// One CPU's half of `SYSCALL_CPU_PERF`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserCpuPerf {
    pub cpu: u32,
    pub apic_id: u32,
    /// `CPUID.1AH:EAX[31:24]`: `0x40` P-core, `0x20` E-core, `0` unreported.
    pub core_type: u8,
    /// The thread's index within its core.
    pub smt_thread: u8,
    /// `1` once the CPU has recorded its state.
    pub online: u8,
    pub _pad0: u8,
    /// The x2APIC ID with the thread bits shifted out: siblings share it.
    pub core_id: u32,
    /// The settings generation this CPU last applied.
    pub applied: u32,
    pub _pad1: u32,
    /// `IA32_APERF`, `IA32_MPERF` and the TSC, read together on this CPU at
    /// its last timer tick or idle entry. `0` without the APERF/MPERF pair.
    pub aperf: u64,
    pub mperf: u64,
    pub tsc: u64,
    /// `IA32_HWP_CAPABILITIES` and the `IA32_HWP_REQUEST` this CPU last
    /// wrote; `0` without HWP.
    pub hwp_caps: u64,
    pub hwp_request: u64,
    /// As the firmware left them on this CPU: `IA32_HWP_REQUEST` (`0` unless
    /// it had enabled HWP) and `IA32_PERF_CTL` (`0` without EIST).
    pub boot_hwp_request: u64,
    pub boot_perf_ctl: u64,
    /// `IA32_THERM_STATUS`, sampled with the counters; `0` without a thermal
    /// sensor.
    pub therm_status: u64,
}

/// `prof_ctl` operations.
/// Zero every sample table and start sampling.
pub const PROF_OP_START: u64 = 1;
/// Stop sampling; the tables keep what they hold.
pub const PROF_OP_STOP: u64 = 2;
/// Print the tables to the kernel log as `PROF[<label>]:` lines.
pub const PROF_OP_REPORT: u64 = 3;
/// `1` while sampling, else `0`.
pub const PROF_OP_STATUS: u64 = 4;
/// The longest `PROF_OP_REPORT` label.
pub const PROF_LABEL_MAX: usize = 32;

/// `msync(2)` flags, Linux values.
///
/// `MS_INVALIDATE` is accepted by the ABI and refused by the kernel: one page
/// set per inode, so there is no second copy to invalidate against.
pub const MS_ASYNC: u64 = 1;
pub const MS_INVALIDATE: u64 = 2;
pub const MS_SYNC: u64 = 4;
