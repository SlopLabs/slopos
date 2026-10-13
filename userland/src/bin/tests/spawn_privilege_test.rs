//! `spawn_path` privilege containment, exercised through the real syscall entry.
//!
//! The utest runner hands this binary every capability as uid 0, so a refusal
//! that is the caller's doing is shown in a forked child that dropped what it
//! names. A spawned child holds the universal set, what its spawner delegates
//! of its own and the kernel's grant for its path, and an exec narrows the
//! mask to what the new image is granted.

use slopos_userland as _;

use slopos_abi::caps::{
    ALL as CAP_MASK_ALL, BOOT_ENTRY as CAP_BOOT_ENTRY, FATE as CAP_FATE,
    NET_ADMIN as CAP_NET_ADMIN, RAW_BLOCK as CAP_RAW_BLOCK, SCHED_HIGH as CAP_SCHED_HIGH,
    SET_ID as CAP_SET_ID, SYS_INSPECT as CAP_SYS_INSPECT, UNIVERSAL as CAP_UNIVERSAL,
};
use slopos_abi::net::NET_IFINDEX_GLOBAL;
use slopos_abi::spawn::{
    SPAWN_SET_ALL, SPAWN_SET_GID, SPAWN_SET_GROUPS, SPAWN_SET_UID, SpawnAttrs, SpawnFdAction,
};
use slopos_abi::syscall::{PROF_OP_STATUS, SYSCALL_SPAWN_PATH};
use slopos_abi::task::{
    SPAWN_RESERVED, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NEW_PGRP, TASK_FLAG_NO_PREEMPT,
    TASK_FLAG_USER_MODE, TaskPriority,
};
use slopos_boot_core::variables;
use slopos_slibc::process::ids::{getresgid, getresuid};
use slopos_slibc::process::shim;
use slopos_userland::syscall::core::{caps_drop, prof_ctl};
use slopos_userland::syscall::efi::efivar_get;
use slopos_userland::syscall::error::SyscallError;
use slopos_userland::syscall::fs::{self, pipe};
use slopos_userland::syscall::net::net_iface_ctl;
use slopos_userland::syscall::process::{self, Endowment};
use slopos_userland::syscall::raw::syscall5;

use std::ffi::CString;
use std::fs::File;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::{Arc, Barrier};

const EPERM: i32 = -1;
const EINVAL: i32 = -22;
const ENOENT: i32 = -2;
const EFAULT: i32 = -14;

const PRIORITY_HIGH: u8 = 0;
const PRIORITY_KERNEL_IO: u8 = 1;
const PRIORITY_NORMAL: u8 = 2;
const PRIORITY_IDLE: u8 = 4;

/// No interface operation has this number, so a caller allowed to reach the
/// handler is answered `EINVAL` and nothing changes.
const UNKNOWN_IFOP: u32 = u32::MAX;

/// A path that fails at VFS open, so a request that clears validation is proven
/// by an exec-stage errno with no child ever created.
const MISSING: &[u8] = b"/bin/__no_such_binary__";

/// Issue one `spawn_path` with a raw priority byte and flag word — built by
/// hand because `process::spawn_path_with_actions`'s typed `TaskPriority`
/// cannot express the tiers under test.
fn spawn_raw(path: &[u8], priority: u8, flags: u16, actions: &[SpawnFdAction]) -> i32 {
    spawn_attrs(
        path,
        &SpawnAttrs {
            actions_ptr: actions.as_ptr() as u64,
            actions_len: actions.len() as u64,
            ..SpawnAttrs::new(priority, flags)
        },
    )
}

/// A spawn of `MISSING` asking for `caps` and, given one, `uid` as every id.
fn spawn_asking(caps: u64, uid: Option<u32>) -> i32 {
    spawn_attrs(
        MISSING,
        &SpawnAttrs {
            caps,
            uid: uid.unwrap_or_default(),
            gid: uid.unwrap_or_default(),
            set: uid.map_or(0, |_| SPAWN_SET_UID | SPAWN_SET_GID),
            ..SpawnAttrs::new(PRIORITY_NORMAL, TASK_FLAG_USER_MODE)
        },
    )
}

/// A spawn of `MISSING` naming `groups` at `ptr`, `count` long.
fn spawn_naming_groups(ptr: u64, count: u32) -> i32 {
    spawn_attrs(
        MISSING,
        &SpawnAttrs {
            groups_ptr: ptr,
            ngroups: count,
            set: SPAWN_SET_GROUPS,
            ..SpawnAttrs::new(PRIORITY_NORMAL, TASK_FLAG_USER_MODE)
        },
    )
}

fn spawn_attrs(path: &[u8], attrs: &SpawnAttrs) -> i32 {
    let argv: [*const u8; 0] = [];
    unsafe {
        syscall5(
            SYSCALL_SPAWN_PATH,
            path.as_ptr() as u64,
            path.len() as u64,
            argv.as_ptr() as u64,
            0,
            attrs as *const SpawnAttrs as u64,
        ) as i32
    }
}

fn expect(label: &str, got: i32, want: i32) -> bool {
    if got == want {
        return true;
    }
    eprintln!("spawn_privilege_test: {label} returned {got}, want {want}");
    false
}

/// Fork, run `body` in the child and answer whether it exited 0. The child is
/// a separate process, so what it gives up this one keeps.
fn in_forked_child(label: &str, body: fn() -> i32) -> bool {
    let pid = shim::fork();
    if pid == 0 {
        shim::_exit(body());
    }
    if pid < 0 {
        eprintln!("spawn_privilege_test: {label}: fork returned {pid}");
        return false;
    }
    match process::wait_exit_code(pid as u32) {
        0 => true,
        code => {
            eprintln!("spawn_privilege_test: {label}: the child exited {code}");
            false
        }
    }
}

/// `NO_PREEMPT` is refused with `EPERM`: the caller is asking for something
/// real that only the kernel sets, not making a malformed request.
fn no_preempt_is_eperm() -> bool {
    expect(
        "NO_PREEMPT",
        spawn_raw(MISSING, PRIORITY_NORMAL, TASK_FLAG_NO_PREEMPT, &[]),
        EPERM,
    )
}

/// Reserved bits fail closed, so a widened flag word can give one a meaning no
/// deployed caller already assumed. Among them are `0x0040`, the retired
/// `TASK_FLAG_FPU_INITIALIZED`, and every privilege bit that moved to the
/// capability mask: retired, not freed.
///
/// A request carrying both a reserved bit and `NO_PREEMPT` is answered as
/// malformed, so probing reserved bits cannot learn from an `EPERM` that a bit
/// means something.
fn malformed_flags_are_einval() -> bool {
    let mut ok = expect(
        "KERNEL_MODE",
        spawn_raw(MISSING, PRIORITY_NORMAL, TASK_FLAG_KERNEL_MODE, &[]),
        EINVAL,
    );
    for shift in 0..u16::BITS {
        let bit = 1u16 << shift;
        if SPAWN_RESERVED & bit == 0 {
            continue;
        }
        ok &= expect(
            &format!("reserved bit {bit:#06x}"),
            spawn_raw(MISSING, PRIORITY_NORMAL, bit, &[]),
            EINVAL,
        );
        ok &= expect(
            &format!("reserved bit {bit:#06x} alongside NO_PREEMPT"),
            spawn_raw(MISSING, PRIORITY_NORMAL, bit | TASK_FLAG_NO_PREEMPT, &[]),
            EINVAL,
        );
    }
    ok
}

/// `Normal` and `Low` are anyone's and `High` takes `SchedHigh`; `KernelIo`
/// is for kthreads and `Idle` the per-CPU idle loop's, which no spawn names.
fn tiers_follow_sched_high() -> bool {
    expect(
        "High holding SchedHigh",
        spawn_raw(MISSING, PRIORITY_HIGH, TASK_FLAG_USER_MODE, &[]),
        ENOENT,
    ) && expect(
        "KernelIo",
        spawn_raw(MISSING, PRIORITY_KERNEL_IO, TASK_FLAG_USER_MODE, &[]),
        EINVAL,
    ) && expect(
        "Idle",
        spawn_raw(MISSING, PRIORITY_IDLE, TASK_FLAG_USER_MODE, &[]),
        EINVAL,
    ) && in_forked_child("High without SchedHigh", high_without_sched_high)
}

fn high_without_sched_high() -> i32 {
    caps_drop(CAP_SCHED_HIGH);
    if expect(
        "High without SchedHigh",
        spawn_raw(MISSING, PRIORITY_HIGH, TASK_FLAG_USER_MODE, &[]),
        EPERM,
    ) {
        0
    } else {
        1
    }
}

/// A spawner hands its child capabilities it holds, and asking for one it
/// lacks is `EPERM`: shown on a child that reports its mask, then in a forked
/// child that dropped `RawBlock`.
fn delegation_is_bounded_by_the_spawner() -> bool {
    let delegated = Endowment {
        caps: CAP_SYS_INSPECT,
        ..Endowment::NONE
    };
    let mask = (CAP_UNIVERSAL | CAP_SYS_INSPECT).to_string();
    if run_endowed(SELF, &[HOLDS, &mask], delegated) != Some(0) {
        eprintln!("spawn_privilege_test: a child delegated SysInspect does not hold {mask}");
        return false;
    }
    let undefined_set = SpawnAttrs {
        set: SPAWN_SET_ALL + 1,
        ..SpawnAttrs::new(PRIORITY_NORMAL, TASK_FLAG_USER_MODE)
    };
    let groups = [0u32; 33];
    expect(
        "an undefined capability bit",
        spawn_asking(1 << 63, None),
        EINVAL,
    ) && expect(
        "an undefined id bit",
        spawn_attrs(MISSING, &undefined_set),
        EINVAL,
    ) && expect(
        "33 groups",
        spawn_naming_groups(groups.as_ptr() as u64, 33),
        EINVAL,
    ) && expect("groups at no address", spawn_naming_groups(8, 1), EFAULT)
        && expect(
            "32 groups",
            spawn_naming_groups(groups.as_ptr() as u64, 32),
            ENOENT,
        )
        && in_forked_child("delegating beyond the spawner", delegate_beyond_the_spawner)
}

fn delegate_beyond_the_spawner() -> i32 {
    caps_drop(CAP_RAW_BLOCK);
    let held = expect(
        "delegating a held capability",
        spawn_asking(CAP_NET_ADMIN, None),
        ENOENT,
    );
    let dropped = expect(
        "delegating a dropped capability",
        spawn_asking(CAP_RAW_BLOCK, None),
        EPERM,
    );
    if held && dropped { 0 } else { 1 }
}

/// A child runs as another uid only for a spawner holding `SetId`; without
/// it, its own uid is the one it may hand on.
fn ids_need_set_id() -> bool {
    let as_user = Endowment {
        ids: Some((USER, USER)),
        ..Endowment::NONE
    };
    if run_endowed(SELF, &[RUNS_AS, &USER.to_string()], as_user) != Some(0) {
        eprintln!("spawn_privilege_test: a SetId holder's child does not run as {USER}");
        return false;
    }
    in_forked_child("ids without SetId", ids_without_set_id)
}

fn ids_without_set_id() -> i32 {
    caps_drop(CAP_SET_ID);
    let own = expect("its own uid", spawn_asking(0, Some(0)), ENOENT);
    let other = expect("another uid", spawn_asking(0, Some(USER)), EPERM);
    if own && other { 0 } else { 1 }
}

/// A child holding a gated capability, or running as ids its spawner's real
/// ones are not, starts `AT_SECURE`; one given neither does not.
fn endowed_children_start_secure() -> bool {
    let cases = [
        (
            "1",
            Endowment {
                caps: CAP_SYS_INSPECT,
                ..Endowment::NONE
            },
        ),
        (
            "1",
            Endowment {
                ids: Some((USER, USER)),
                ..Endowment::NONE
            },
        ),
        ("0", Endowment::NONE),
    ];
    cases.into_iter().all(|(secure, endowment)| {
        let ran = run_endowed(SECURE_PROBE, &["self", SECURE_PROBE, secure], endowment);
        if ran != Some(0) {
            eprintln!("spawn_privilege_test: {SECURE_PROBE} wanted AT_SECURE={secure}: {ran:?}");
        }
        ran == Some(0)
    })
}

/// Spawn `path` with `args` and `endowment`, sharing this process's stdio,
/// and answer its exit code.
fn run_endowed(path: &str, args: &[&str], endowment: Endowment<'_>) -> Option<i32> {
    let owned: Vec<CString> = std::iter::once(path)
        .chain(args.iter().copied())
        .map(|arg| CString::new(arg).expect("no argument holds a NUL"))
        .collect();
    let argv: Vec<*const u8> = owned.iter().map(|arg| arg.as_ptr().cast()).collect();
    let stdio = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    let tid = process::spawn_endowed(
        path.as_bytes(),
        &argv,
        &[],
        &[],
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &stdio,
        0,
        endowment,
    );
    (tid > 0).then(|| process::wait_exit_code(tid as u32))
}

/// The control: legal attrs clear validation and reach exec. Without it every
/// case above would also pass against a handler that refused everything.
fn user_settable_flags_reach_exec() -> bool {
    expect(
        "USER_MODE|NEW_PGRP on a missing binary",
        spawn_raw(
            MISSING,
            PRIORITY_NORMAL,
            TASK_FLAG_USER_MODE | TASK_FLAG_NEW_PGRP,
            &[],
        ),
        ENOENT,
    )
}

/// The other half of the control: a legal spawn of a real binary still starts a
/// child and still reaps. `TASK_FLAG_FOREGROUND` is deliberately absent — it
/// would move the harness console's foreground group to the child.
fn ordinary_spawn_still_works() -> bool {
    let stdio = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    // The kernel confers no privilege on `nc` by identity, and with no
    // arguments it prints usage and exits.
    let tid = spawn_raw(b"/bin/nc", PRIORITY_NORMAL, TASK_FLAG_USER_MODE, &stdio);
    if tid <= 0 {
        eprintln!("spawn_privilege_test: ordinary spawn of /bin/nc returned {tid}");
        return false;
    }
    let _ = process::waitpid(tid as u32);
    true
}

/// `Power` reaches SlopOS's own UEFI variables and not the firmware's boot
/// manager, whatever the machine's firmware: a forked child that dropped
/// `BootEntry` is refused `BootOrder` and still reaches its own namespace,
/// while this process, holding both, is not refused.
fn power_alone_reaches_no_boot_entry() -> bool {
    let mut buf = [0u8; 64];
    if let Err(SyscallError::EPERM) = efivar_get("BootOrder", &variables::GLOBAL.0, &mut buf) {
        eprintln!("spawn_privilege_test: BootOrder refused to a BootEntry holder");
        return false;
    }
    in_forked_child("Power without BootEntry", power_without_boot_entry)
}

fn power_without_boot_entry() -> i32 {
    let held = caps_drop(CAP_BOOT_ENTRY);
    if held != CAP_MASK_ALL & !CAP_BOOT_ENTRY {
        eprintln!("spawn_privilege_test: dropping BootEntry left {held:#x}");
        return 1;
    }
    let mut buf = [0u8; 64];
    match efivar_get("BootOrder", &variables::GLOBAL.0, &mut buf) {
        Err(SyscallError::EPERM) => {}
        other => {
            eprintln!("spawn_privilege_test: BootOrder without BootEntry: {other:?}");
            return 2;
        }
    }
    match efivar_get("SpawnPrivProbe", &variables::SLOPOS.0, &mut buf) {
        Err(SyscallError::EPERM) => {
            eprintln!("spawn_privilege_test: Power alone was refused SlopOS's own namespace");
            3
        }
        _ => 0,
    }
}

/// The binaries a grant or privd's policy names cannot be rewritten: program
/// identity is a path, so each is only worth anything while every inode the
/// base serves stays sealed.
fn granted_binaries_are_sealed() -> bool {
    use std::fs::OpenOptions;

    let mut ok = true;
    for path in [
        "/bin/compositor",
        "/bin/roulette",
        "/bin/ip",
        "/sbin/init",
        "/sbin/privd",
        "/usr/share/slopos/privd/actions",
    ] {
        if OpenOptions::new().write(true).open(path).is_ok() {
            eprintln!("spawn_privilege_test: {path} opened for write");
            ok = false;
        }
        if OpenOptions::new().read(true).open(path).is_err() {
            eprintln!("spawn_privilege_test: {path} stopped being readable");
            ok = false;
        }
        if std::fs::remove_file(path).is_ok() {
            eprintln!("spawn_privilege_test: {path} was unlinked");
            ok = false;
        }
    }
    // A path the image never shipped is unaffected.
    if std::fs::write("/tmp/spawn_priv_probe", b"x").is_err() {
        eprintln!("spawn_privilege_test: the seal reached an ordinary path");
        ok = false;
    }
    let _ = std::fs::remove_file("/tmp/spawn_priv_probe");
    ok
}

/// A spawner without `Launch` still starts the launchers' paths, ungranted:
/// [`spawns_without_launch`].
fn launch_bounds_the_raise_site() -> bool {
    match Command::new(SELF).arg(WITHOUT_LAUNCH).status() {
        Ok(status) if status.success() => true,
        other => {
            eprintln!("spawn_privilege_test: the child without Launch reported {other:?}");
            false
        }
    }
}

const SELF: &str = "/bin/spawn_privilege_test";
const WITHOUT_LAUNCH: &str = "--without-launch";
const EXEC_PROBE: &str = "--exec-probe";
const HOLDS: &str = "--holds";
const RUNS_AS: &str = "--runs-as";
/// Exits 0 when its `AT_SECURE` is what its third argument says.
const SECURE_PROBE: &str = "/bin/dl_secure_probe";
const USER: u32 = 1000;
/// Holds `RawBlock` only when its spawner hands it over.
const DUMPE2FS: &str = "/sbin/dumpe2fs";
const ROOT_DISK: &str = "/dev/nvme0n1";
/// Whole-disk nodes, of which the first present is read raw.
const BLOCK_NODES: [&str; 2] = [ROOT_DISK, "/dev/vda"];

fn dumps_root_superblock() -> std::io::Result<std::process::Output> {
    Command::new(DUMPE2FS).args(["-h", ROOT_DISK]).output()
}

/// Runs in a child without Launch. A granted path it spawns runs ungranted, as
/// its own `execve` would, rather than being refused: `sh -c` and
/// `#!/bin/sh` scripts run.
fn spawns_without_launch() -> i32 {
    // SAFETY: nothing else runs in this process yet.
    unsafe { std::env::set_var("PATH", "/bin") };
    let sh = Command::new("sh").args(["-c", "exit 7"]).status();
    if !matches!(&sh, Ok(status) if status.code() == Some(7)) {
        eprintln!("spawn_privilege_test: `sh -c` without Launch ran as {sh:?}");
        return 2;
    }

    let script = "/tmp/spawn_priv_script";
    let staged = std::fs::write(script, "#!/bin/sh\nexit 9\n").is_ok()
        && std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).is_ok();
    let run = staged.then(|| Command::new(script).status());
    let _ = std::fs::remove_file(script);
    if !matches!(&run, Some(Ok(status)) if status.code() == Some(9)) {
        eprintln!("spawn_privilege_test: a #!/bin/sh script without Launch ran as {run:?}");
        return 3;
    }

    0
}

/// e2fsprogs reads a disk only when its spawner hands it `RawBlock`, as the
/// installer does: a spawn that names nothing leaves it the universal set,
/// whatever its spawner holds.
fn e2fsprogs_takes_raw_block_from_its_spawner() -> bool {
    let dump = dumps_root_superblock();
    if !matches!(&dump, Ok(out) if !out.status.success()) {
        eprintln!(
            "spawn_privilege_test: e2fsprogs read a disk it was not handed RawBlock for: {dump:?}"
        );
        return false;
    }
    let delegated = dumps_with(CAP_RAW_BLOCK);
    if delegated != Some(0) {
        eprintln!(
            "spawn_privilege_test: {DUMPE2FS} -h {ROOT_DISK} handed RawBlock exited {delegated:?}"
        );
        return false;
    }
    true
}

/// `dumpe2fs -h` of the root disk spawned holding `caps`, its output dropped:
/// a program holding `RawBlock` started with no stdout would open the disk
/// where its output goes.
fn dumps_with(caps: u64) -> Option<i32> {
    let null = fs::open_cstr(c"/dev/null", slopos_abi::fs::O_RDWR).ok()?;
    let stdio = [0, 1, 2].map(|fd| process::clone_fd(null.raw(), fd));
    let argv = [
        CString::new(DUMPE2FS).ok()?,
        CString::new("-h").ok()?,
        CString::new(ROOT_DISK).ok()?,
    ];
    let argv: Vec<*const u8> = argv.iter().map(|a| a.as_ptr().cast()).collect();
    let tid = process::spawn_endowed(
        DUMPE2FS.as_bytes(),
        &argv,
        &[],
        b"/",
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &stdio,
        0,
        Endowment {
            caps,
            ..Endowment::NONE
        },
    );
    (tid > 0).then(|| process::wait_exit_code(tid as u32))
}

fn block_node() -> Option<&'static str> {
    BLOCK_NODES
        .into_iter()
        .find(|node| std::path::Path::new(node).exists())
}

fn raw_read(node: &str) -> std::io::Result<()> {
    let mut sector = [0u8; 512];
    File::open(node)?.read_exact(&mut sector)
}

/// Exec narrows the mask to the new image's grant: a forked child of this
/// process, which holds every capability, execs this binary, which no grant
/// row names, and keeps the universal set alone. Its network change, raw disk
/// read and signal to this process are refused; the same three succeed here
/// first, so the refusals are the exec's doing.
fn exec_narrows_to_the_images_grant() -> bool {
    let held = caps_drop(0);
    if held != CAP_MASK_ALL {
        eprintln!("spawn_privilege_test: the utest holds {held:#x}, want {CAP_MASK_ALL:#x}");
        return false;
    }
    let Some(node) = block_node() else {
        eprintln!("spawn_privilege_test: none of {BLOCK_NODES:?} exists");
        return false;
    };
    if let Err(e) = raw_read(node) {
        eprintln!("spawn_privilege_test: a RawBlock holder could not read {node}: {e}");
        return false;
    }
    if let Err(SyscallError::EPERM) = net_iface_ctl(NET_IFINDEX_GLOBAL, UNKNOWN_IFOP, 0) {
        eprintln!("spawn_privilege_test: a NetAdmin holder was refused net_iface_ctl");
        return false;
    }

    // The probe holds the read end until this process has signalled it, so
    // the signal reaches a live task rather than a zombie.
    let Ok((gate_read, gate_write)) = pipe() else {
        eprintln!("spawn_privilege_test: pipe failed");
        return false;
    };
    let owned: Vec<CString> = [
        SELF.to_owned(),
        EXEC_PROBE.to_owned(),
        gate_read.raw().to_string(),
        node.to_owned(),
        process::getpid().to_string(),
    ]
    .into_iter()
    .map(|arg| CString::new(arg).expect("no argument holds a NUL"))
    .collect();
    let mut argv: Vec<*const u8> = owned.iter().map(|arg| arg.as_ptr().cast()).collect();
    argv.push(core::ptr::null());
    let envp: [*const u8; 1] = [core::ptr::null()];

    let pid = shim::fork();
    if pid == 0 {
        drop(gate_write);
        process::execve(argv[0], argv.as_ptr(), envp.as_ptr());
        shim::_exit(100);
    }
    drop(gate_read);
    if pid < 0 {
        eprintln!("spawn_privilege_test: fork returned {pid}");
        return false;
    }
    let signalled = process::kill_pid(pid, 0);
    drop(gate_write);
    let code = process::wait_exit_code(pid as u32);
    let ok = expect("kill(child, 0) from the utest", signalled, 0);
    if code != 0 {
        eprintln!("spawn_privilege_test: the exec'd probe exited {code}");
        return false;
    }
    ok
}

/// The exec'd half of [`exec_narrows_to_the_images_grant`]. Answers once the
/// parent closes the gate.
fn exec_probe(args: &[String]) -> i32 {
    let [gate, node, parent] = args else {
        eprintln!("spawn_privilege_test: {EXEC_PROBE} takes GATE_FD NODE PARENT_PID");
        return 10;
    };
    let (Ok(gate), Ok(parent)) = (gate.parse::<i32>(), parent.parse::<i32>()) else {
        eprintln!("spawn_privilege_test: {EXEC_PROBE}: unparsable {args:?}");
        return 11;
    };
    let code = exec_probe_checks(node, parent);
    // SAFETY: the parent passed this descriptor across the exec, and nothing
    // else in this process owns it.
    let mut gate = unsafe { File::from_raw_fd(gate) };
    let _ = gate.read_to_end(&mut Vec::new());
    code
}

fn exec_probe_checks(node: &str, parent: i32) -> i32 {
    let held = caps_drop(0);
    if held != CAP_UNIVERSAL {
        eprintln!("spawn_privilege_test: after exec holds {held:#x}, want {CAP_UNIVERSAL:#x}");
        return 1;
    }
    match net_iface_ctl(NET_IFINDEX_GLOBAL, UNKNOWN_IFOP, 0) {
        Err(SyscallError::EPERM) => {}
        other => {
            eprintln!("spawn_privilege_test: net_iface_ctl after exec: {other:?}, want EPERM");
            return 2;
        }
    }
    match raw_read(node) {
        Err(e) if matches!(e.raw_os_error(), Some(1 | 13)) => {}
        other => {
            eprintln!("spawn_privilege_test: raw read of {node} after exec: {other:?}");
            return 3;
        }
    }
    match process::kill_pid(parent, 0) {
        EPERM => 0,
        other => {
            eprintln!("spawn_privilege_test: kill(parent, 0) after exec returned {other}");
            4
        }
    }
}

/// `prof_ctl(PROF_OP_STATUS)`: the profiler's state to a `SysInspect` holder,
/// `-EPERM` to anyone else.
fn prof_status() -> i64 {
    prof_ctl(PROF_OP_STATUS, b"")
}

/// `caps_drop` narrows the whole process, not the calling thread: a thread
/// parked across the drop loses the capability, a thread started after it
/// never has it, and dropping it again changes nothing. The drop happens in a
/// forked child, and this process still holds everything afterwards.
fn caps_drop_is_process_wide() -> bool {
    if !in_forked_child("caps_drop across threads", drop_across_threads) {
        return false;
    }
    match caps_drop(0) {
        CAP_MASK_ALL => true,
        held => {
            eprintln!("spawn_privilege_test: after its child's drop this process holds {held:#x}");
            false
        }
    }
}

fn drop_across_threads() -> i32 {
    let gate = Arc::new(Barrier::new(2));
    let parked = {
        let gate = Arc::clone(&gate);
        std::thread::spawn(move || {
            let before = (caps_drop(0), prof_status());
            gate.wait();
            gate.wait();
            (before, (caps_drop(0), prof_status()))
        })
    };
    gate.wait();
    let held = caps_drop(CAP_SYS_INSPECT);
    gate.wait();
    let Ok(((mask_before, status_before), (mask_after, status_after))) = parked.join() else {
        eprintln!("spawn_privilege_test: the parked thread panicked");
        return 1;
    };
    if mask_before & CAP_SYS_INSPECT == 0 || status_before < 0 {
        eprintln!(
            "spawn_privilege_test: before the drop a thread held {mask_before:#x}, prof_ctl {status_before}"
        );
        return 2;
    }
    if held != CAP_MASK_ALL & !CAP_SYS_INSPECT || prof_status() != i64::from(EPERM) {
        eprintln!("spawn_privilege_test: dropping SysInspect left {held:#x}");
        return 3;
    }
    if mask_after != held || status_after != i64::from(EPERM) {
        eprintln!(
            "spawn_privilege_test: the parked thread kept {mask_after:#x}, prof_ctl {status_after}"
        );
        return 4;
    }
    match std::thread::spawn(|| (caps_drop(0), prof_status())).join() {
        Ok((mask, status)) if mask == held && status == i64::from(EPERM) => {}
        other => {
            eprintln!("spawn_privilege_test: a thread started after the drop: {other:?}");
            return 5;
        }
    }
    match caps_drop(CAP_SYS_INSPECT) {
        again if again == held => 0,
        again => {
            eprintln!("spawn_privilege_test: a second drop left {again:#x}, the first {held:#x}");
            6
        }
    }
}

/// A program spawned after a drop holds none of what was dropped, the
/// universal set included, so its spawner still holds everything it holds.
fn a_spawn_after_a_drop_lacks_it() -> bool {
    in_forked_child("spawn after a drop", spawn_after_drop)
}

fn spawned_child_holds(mask: u64) -> bool {
    matches!(
        Command::new(SELF).args([HOLDS, &mask.to_string()]).status(),
        Ok(status) if status.success()
    )
}

fn spawn_after_drop() -> i32 {
    if !spawned_child_holds(CAP_UNIVERSAL) {
        eprintln!(
            "spawn_privilege_test: before the drop a spawned child lacked {CAP_UNIVERSAL:#x}"
        );
        return 1;
    }
    let held = caps_drop(CAP_FATE);
    if !spawned_child_holds(CAP_UNIVERSAL & held) {
        eprintln!(
            "spawn_privilege_test: a child spawned after dropping Fate held more than {held:#x}"
        );
        return 2;
    }
    0
}

/// Exit 0 when this process holds exactly `args[0]`.
fn holds(args: &[String]) -> i32 {
    match args.first().and_then(|mask| mask.parse::<u64>().ok()) {
        Some(want) if caps_drop(0) == want => 0,
        _ => 1,
    }
}

/// Exit 0 when every uid and gid of this process is `args[0]`.
fn runs_as(args: &[String]) -> i32 {
    let mut uids = [0u32; 3];
    let mut gids = [0u32; 3];
    let read = unsafe {
        getresuid(&mut uids[0], &mut uids[1], &mut uids[2]) == 0
            && getresgid(&mut gids[0], &mut gids[1], &mut gids[2]) == 0
    };
    match args.first().and_then(|id| id.parse::<u32>().ok()) {
        Some(want) if read && uids == [want; 3] && gids == [want; 3] => 0,
        _ => {
            eprintln!("spawn_privilege_test: runs as uids {uids:?} gids {gids:?}");
            1
        }
    }
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("no_preempt_is_eperm", no_preempt_is_eperm),
    ("malformed_flags_are_einval", malformed_flags_are_einval),
    ("tiers_follow_sched_high", tiers_follow_sched_high),
    (
        "delegation_is_bounded_by_the_spawner",
        delegation_is_bounded_by_the_spawner,
    ),
    ("ids_need_set_id", ids_need_set_id),
    (
        "endowed_children_start_secure",
        endowed_children_start_secure,
    ),
    (
        "user_settable_flags_reach_exec",
        user_settable_flags_reach_exec,
    ),
    ("ordinary_spawn_still_works", ordinary_spawn_still_works),
    ("granted_binaries_are_sealed", granted_binaries_are_sealed),
    ("grant_directories_are_sealed", grant_directories_are_sealed),
    ("launch_bounds_the_raise_site", launch_bounds_the_raise_site),
    (
        "e2fsprogs_takes_raw_block_from_its_spawner",
        e2fsprogs_takes_raw_block_from_its_spawner,
    ),
    (
        "power_alone_reaches_no_boot_entry",
        power_alone_reaches_no_boot_entry,
    ),
    (
        "exec_narrows_to_the_images_grant",
        exec_narrows_to_the_images_grant,
    ),
    ("caps_drop_is_process_wide", caps_drop_is_process_wide),
    (
        "a_spawn_after_a_drop_lacks_it",
        a_spawn_after_a_drop_lacks_it,
    ),
];

/// A sealed binary is only as safe as the directory that names it: rename
/// `/bin` aside, `mkdir /bin`, plant a `shell`, and the grant table keyed on
/// `/bin/shell` confers Launch on whatever was planted. So the directory refuses
/// rename, and refuses a create or unlink beneath it, on whichever root booted.
fn grant_directories_are_sealed() -> bool {
    let mut ok = true;
    for dir in ["/bin", "/sbin", "/lib"] {
        if std::fs::rename(dir, "/spawn_priv_moved").is_ok() {
            eprintln!("spawn_privilege_test: {dir} was renamed aside");
            let _ = std::fs::rename("/spawn_priv_moved", dir);
            ok = false;
        }
        let planted = format!("{dir}/spawn_priv_planted");
        if std::fs::write(&planted, b"x").is_ok() {
            eprintln!("spawn_privilege_test: a file was created inside {dir}");
            let _ = std::fs::remove_file(&planted);
            ok = false;
        }
        if std::fs::remove_dir(dir).is_ok() {
            eprintln!("spawn_privilege_test: {dir} was removed");
            ok = false;
        }
    }
    // The seal is on those three names and nothing else.
    if std::fs::create_dir("/spawn_priv_dir").is_err() {
        eprintln!("spawn_privilege_test: the directory seal reached the root");
        ok = false;
    }
    let _ = std::fs::remove_dir("/spawn_priv_dir");
    ok
}

/// The session's view of the spawn boundary: holding nothing beyond the
/// universal set as uid 1000, it can delegate nothing gated, take no other
/// uid and ask for no higher tier, and every request one step inside those
/// bounds reaches exec.
const SESSION: &str = "--session";

fn session_holds_only_the_universal_set() -> bool {
    let mut uids = [0u32; 3];
    let read = unsafe { getresuid(&mut uids[0], &mut uids[1], &mut uids[2]) == 0 };
    let held = caps_drop(0);
    if !read || uids != [USER; 3] || held != CAP_UNIVERSAL {
        eprintln!("spawn_privilege_test: the session runs as {uids:?} holding {held:#x}");
        return false;
    }
    true
}

fn session_delegates_nothing_gated() -> bool {
    let mut ok = expect(
        "delegating a universal capability",
        spawn_asking(CAP_FATE, None),
        ENOENT,
    );
    for bit in (0..u64::BITS).map(|shift| 1u64 << shift) {
        if bit & CAP_MASK_ALL & !CAP_UNIVERSAL == 0 {
            continue;
        }
        ok &= expect(
            &format!("delegating {bit:#x}"),
            spawn_asking(bit, None),
            EPERM,
        );
    }
    ok
}

/// Zeroed id fields ask for nothing: they are uid 0 only under a `set` bit.
fn session_takes_no_other_uid() -> bool {
    let zeroed = SpawnAttrs {
        uid: 0,
        gid: 0,
        ..SpawnAttrs::new(PRIORITY_NORMAL, TASK_FLAG_USER_MODE)
    };
    expect("its own uid", spawn_asking(0, Some(USER)), ENOENT)
        && expect("uid 0", spawn_asking(0, Some(0)), EPERM)
        && expect("zeroed ids", spawn_attrs(MISSING, &zeroed), ENOENT)
        && expect("no groups", spawn_naming_groups(0, 0), EPERM)
}

fn session_asks_no_higher_tier() -> bool {
    expect(
        "Normal",
        spawn_raw(MISSING, PRIORITY_NORMAL, TASK_FLAG_USER_MODE, &[]),
        ENOENT,
    ) && expect(
        "High",
        spawn_raw(MISSING, PRIORITY_HIGH, TASK_FLAG_USER_MODE, &[]),
        EPERM,
    )
}

const SESSION_CASES: &[(&str, fn() -> bool)] = &[
    (
        "session_holds_only_the_universal_set",
        session_holds_only_the_universal_set,
    ),
    (
        "session_delegates_nothing_gated",
        session_delegates_nothing_gated,
    ),
    ("session_takes_no_other_uid", session_takes_no_other_uid),
    ("session_asks_no_higher_tier", session_asks_no_higher_tier),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some(SESSION) => slopos_slibc::test_harness::run(SESSION_CASES),
        Some(WITHOUT_LAUNCH) => std::process::exit(spawns_without_launch()),
        Some(EXEC_PROBE) => std::process::exit(exec_probe(&args[2..])),
        Some(HOLDS) => std::process::exit(holds(&args[2..])),
        Some(RUNS_AS) => std::process::exit(runs_as(&args[2..])),
        _ => slopos_slibc::test_harness::run(CASES),
    }
}
