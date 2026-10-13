//! The authority model's load-bearing claims, checked in the kernel.
//!
//! These test the *decision*, not the effect: a test that actually called
//! `halt` would power the machine off mid-run. What matters is that the grant
//! table and the syscall classification say the right things, because the
//! dispatcher's behaviour is a total function of those two and a task's mask.
//! A mechanism's authority comes from privd's delegation, which `userland`'s
//! `privd_test` grades.

use slopos_abi::syscall::{
    SYSCALL_PRIVATE_BASE, SYSCALL_PRIVATE_END, SYSCALL_REBOOT, SYSCALL_ROULETTE_RESULT,
    SYSCALL_TABLE_SIZE,
};
use slopos_ostd::authority::{
    AuthorityDecision, CAP_MASK_ALL, CAP_NONE, CAP_UNIVERSAL, Capability, decide, mask_permits,
};
use slopos_testing::{TestResult, fail, pass};

use crate::exec::grants::{delegated_for, grant_for, granted_paths};
use crate::syscall::handlers::{cap_counts, syscall_lookup};
use crate::syscall::signal::Sender;
use slopos_ostd::process::Cred;

/// What a spawn by a `Launch` holder gives `path`, delegation included.
fn spawned_holding(path: &[u8]) -> u64 {
    CAP_UNIVERSAL | grant_for(path) | delegated_for(path)
}

/// `cap` is conferred on exactly the programs `holders` names, and on no other
/// row of the table.
fn conferred_on_exactly(cap: Capability, holders: &[&'static [u8]]) -> Result<(), &'static [u8]> {
    for &path in holders {
        if !mask_permits(spawned_holding(path), cap) {
            return Err(path);
        }
    }
    for path in granted_paths() {
        if mask_permits(spawned_holding(path), cap) != holders.contains(&path) {
            return Err(path);
        }
    }
    Ok(())
}

/// The finding phase 1 closed: `halt` and `reboot` had no authorization
/// check at all, so one instruction from any task powered off the machine.
fn power_is_denied_to_an_ordinary_task() -> TestResult {
    if mask_permits(CAP_UNIVERSAL, Capability::Power) {
        return fail!("an ordinary user task must not hold Power");
    }
    if decide(CAP_UNIVERSAL, Capability::Power) != AuthorityDecision::Deny {
        return fail!("Power must be denied under the default enforce mode");
    }
    pass!()
}

/// What a mechanism may hold, which privd's action delegates and no path
/// earns.
const MECHANISM_CAPS: [Capability; 10] = [
    Capability::Power,
    Capability::Mount,
    Capability::RawBlock,
    Capability::NetAdmin,
    Capability::ConsoleConfig,
    Capability::SysInspect,
    Capability::Clock,
    Capability::Seal,
    Capability::BootEntry,
    Capability::DacOverride,
];

/// No program earns a mechanism's authority by its path: the grant table's
/// remaining rows confer `Launch` and the display seat, and the programs
/// privd runs are in it neither as grants nor as delegations.
fn no_path_earns_a_mechanism_s_authority() -> TestResult {
    for path in granted_paths() {
        let earned = spawned_holding(path);
        if let Some(cap) = MECHANISM_CAPS
            .iter()
            .find(|&&cap| mask_permits(earned, cap))
        {
            return fail!(
                "{:?} earns {} by its path",
                core::str::from_utf8(path),
                cap.name()
            );
        }
    }
    for path in [
        &b"/bin/halt"[..],
        b"/bin/bootctl",
        b"/bin/cpufreq",
        b"/bin/installer",
        b"/bin/ip",
        b"/bin/keymap",
        b"/bin/kconsole",
        b"/bin/prof",
        b"/sbin/mke2fs",
        b"/sbin/privd",
    ] {
        if grant_for(path) != CAP_NONE || delegated_for(path) != CAP_NONE {
            return fail!(
                "{:?} is still in the grant table",
                core::str::from_utf8(path)
            );
        }
    }
    pass!()
}

/// The classification reaches the dispatch table through the handler, so the
/// slot a caller invokes and the capability the dispatcher tests are one
/// artifact. If these drift, every other claim in the model is void.
fn the_table_classifies_power() -> TestResult {
    let Some(entry) = syscall_lookup(SYSCALL_REBOOT) else {
        return fail!("reboot is not registered");
    };
    if entry.cap != Capability::Power {
        return fail!("reboot is classified {}, want Power", entry.cap.name());
    }

    // The reachability case: `roulette_result` reaches the reboot primitive on
    // its loss arm, so a slot-level gate alone would leave it green. It is
    // classified `Fate` *and* two-keyed on a boot flag.
    let Some(entry) = syscall_lookup(SYSCALL_ROULETTE_RESULT) else {
        return fail!("roulette_result is not registered");
    };
    if entry.cap != Capability::Fate {
        return fail!(
            "roulette_result is classified {}, want Fate",
            entry.cap.name()
        );
    }
    pass!()
}

/// Totality, checked at runtime as well as by the const assert: every slot
/// carries a classification, and a registered slot is never `Unimplemented`.
fn every_slot_is_classified() -> TestResult {
    let mut registered = 0usize;
    let all = (0..SYSCALL_TABLE_SIZE as u64).chain(SYSCALL_PRIVATE_BASE..SYSCALL_PRIVATE_END);
    for sysno in all {
        if let Some(entry) = syscall_lookup(sysno) {
            registered += 1;
            if entry.cap == Capability::Unimplemented {
                return fail!("syscall {} is registered but unclassified", sysno);
            }
        }
    }
    if registered == 0 {
        return fail!("no syscall resolved -- the lookup is broken, not the table");
    }

    // The histogram counts registered entry points, so it sums to what the
    // walk just found. A drift here means the const assert and the live table
    // disagree, which should be impossible.
    let total: usize = cap_counts().iter().map(|(_, n)| *n).sum();
    if total != registered {
        return fail!(
            "the recorded distribution sums to {}, want {}",
            total,
            registered
        );
    }
    pass!()
}

/// An ungated operation is permitted by the empty mask. Getting this backwards
/// denies every unprivileged syscall in the system, so it is worth an explicit
/// test rather than resting on the enum's shape.
fn ungated_operations_need_nothing() -> TestResult {
    for cap in [
        Capability::NoneSelf,
        Capability::NoneFd,
        Capability::NoneRelation,
    ] {
        if !mask_permits(CAP_NONE, cap) {
            return fail!("{} must be permitted with no capabilities", cap.name());
        }
        if decide(CAP_NONE, cap) != AuthorityDecision::Allow {
            return fail!("{} must be allowed with no capabilities", cap.name());
        }
    }
    pass!()
}

slopos_testing::stest!(
    name = power_is_denied_to_an_ordinary_task,
    suite = authority
);
slopos_testing::stest!(
    name = no_path_earns_a_mechanism_s_authority,
    suite = authority
);
slopos_testing::stest!(name = the_table_classifies_power, suite = authority);
slopos_testing::stest!(name = every_slot_is_classified, suite = authority);
slopos_testing::stest!(name = ungated_operations_need_nothing, suite = authority);

/// The capabilities every task holds, pinned so their removal is a visible
/// diff rather than a silent widening.
///
/// Each names a global the kernel has not yet given an object form. They are
/// classified rather than left ungated so that the deletion conditions in
/// `slopos_ostd::authority` have something to delete: when `write` routes
/// through the controlling TTY and the clipboard becomes fd-passing only,
/// these entries go, and this test is what fails to say so.
fn the_universal_capabilities_are_the_recorded_set() -> TestResult {
    let universal = [
        Capability::ConsoleIo,
        Capability::ClipboardGlobal,
        Capability::Fate,
    ];
    for &cap in Capability::ALL.iter().filter(|cap| cap.is_gated()) {
        if mask_permits(CAP_UNIVERSAL, cap) != universal.contains(&cap) {
            return fail!(
                "{} changed universality -- if deliberate, this test records it",
                cap.name()
            );
        }
    }
    pass!()
}

/// A nonexistent id is `ESRCH` even to a caller holding every capability:
/// never a success, and never a permission answer that would disclose whether
/// the id names anything. Init's refusal is
/// [`init_is_told_apart_by_its_task_id`], since init does not exist yet here.
fn signalable_refuses_invalid_targets() -> TestResult {
    use crate::syscall::signalable::resolve_signal_target;
    use slopos_abi::Errno;
    use slopos_abi::task::INVALID_TASK_ID;

    let sender = Sender::new(CAP_MASK_ALL, Cred::ROOT, 0);
    for (id, label) in [(INVALID_TASK_ID, "an invalid id"), (0, "id 0")] {
        match resolve_signal_target(&sender, id, 0) {
            Err(Errno::ESRCH) => {}
            Err(other) => return fail!("{} gave {:?}, want ESRCH", label, other),
            Ok(_) => return fail!("{} must not resolve", label),
        }
    }
    pass!()
}

slopos_testing::stest!(
    name = the_universal_capabilities_are_the_recorded_set,
    suite = authority
);
slopos_testing::stest!(name = signalable_refuses_invalid_targets, suite = authority);

/// Init is told apart by its task id, which is never a signal target. Runs in
/// the userland phase, the first in which init exists.
fn init_is_told_apart_by_its_task_id() -> TestResult {
    use crate::syscall::signalable::resolve_signal_target;
    use core::ffi::c_char;
    use slopos_abi::Errno;
    use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE};
    use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
    use slopos_sched::task::{task_create, task_find_by_id, task_terminate};

    let init_id = crate::exec::init_task_id();
    let Some(init) = task_find_by_id(init_id) else {
        return fail!("init is not running in the userland phase");
    };
    let other_id = task_create(
        b"NotInit\0".as_ptr() as *const c_char,
        slopos_sched::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
        core::ptr::null_mut(),
        1,
        TASK_FLAG_USER_MODE,
    );
    let Some(other) = task_find_by_id(other_id) else {
        return fail!("no test task");
    };
    slopos_ostd::task::ops::task_set_caps(&other, CAP_UNIVERSAL);
    let identities = (
        crate::exec::task_is_init(&init),
        crate::exec::task_is_init(&other),
    );
    let everything = Sender::new(CAP_MASK_ALL, Cred::ROOT, 0);
    let init_target = resolve_signal_target(&everything, init_id, 0).map(|_| ());
    let other_target = resolve_signal_target(&everything, other_id, 0).map(|_| ());
    drop((init, other));
    if other_id != INVALID_TASK_ID {
        task_terminate(other_id);
    }

    if identities != (true, false) {
        return fail!("task_is_init answered {:?} for (init, another)", identities);
    }
    if init_target != Err(Errno::EPERM) {
        return fail!("init resolved as a signal target: {:?}", init_target);
    }
    if other_target.is_err() {
        return fail!("a task holding everything could not signal an ordinary one");
    }
    pass!()
}

slopos_testing::stest!(
    name = init_is_told_apart_by_its_task_id,
    flags = 0,
    kind = Userland
);

/// exec narrows authority to what the new image's identity earns, and never
/// raises it.
///
/// An entitlement surviving `exec` is one of the two CVE shapes this model is
/// built against: a privileged program execs an arbitrary binary and the
/// binary inherits the privilege. The other is an unprivileged task execing a
/// privileged image and gaining its grant. Checked on a real task through the
/// function `execve` calls: `/bin/roulette`, which still earns the display
/// seat, and `/bin/halt`, a mechanism that earns nothing and so keeps nothing
/// of what a task held when it exec'd it.
fn exec_intersection_never_widens() -> TestResult {
    use core::ffi::c_char;
    use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE};
    use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
    use slopos_ostd::task::ops::{task_caps, task_set_caps};
    use slopos_sched::task::{task_create, task_find_by_id, task_terminate};

    let (Ok(roulette), Ok(halt)) = (
        slopos_fs::vfs::canon::canonicalise_at(b"/bin/roulette", b"/"),
        slopos_fs::vfs::canon::canonicalise_at(b"/bin/halt", b"/"),
    ) else {
        return fail!("no path for /bin/roulette or /bin/halt");
    };
    let id = task_create(
        b"ExecNarrowTest\0".as_ptr() as *const c_char,
        slopos_sched::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
        core::ptr::null_mut(),
        1,
        TASK_FLAG_USER_MODE,
    );
    let Some(task) = task_find_by_id(id) else {
        return fail!("no test task");
    };
    let seat = Capability::DisplaySeat.bit();
    let mut after = [CAP_NONE; 4];
    for (slot, (before, image)) in after.iter_mut().zip([
        (CAP_UNIVERSAL, &roulette),
        (CAP_UNIVERSAL | seat, &roulette),
        (CAP_MASK_ALL, &roulette),
        (CAP_MASK_ALL, &halt),
    ]) {
        task_set_caps(&task, before);
        crate::exec::narrow_authority_for_exec(&task, id, image, true);
        *slot = task_caps(&task);
    }
    drop(task);
    if id != INVALID_TASK_ID {
        task_terminate(id);
    }

    let [ordinary, holder, everything, mechanism] = after;
    if ordinary != CAP_UNIVERSAL {
        return fail!(
            "exec of a granted image raised an ordinary task to {:#x}",
            ordinary
        );
    }
    if holder != CAP_UNIVERSAL | seat {
        return fail!("exec of /bin/roulette took the display seat from a task holding it");
    }
    if everything != CAP_UNIVERSAL | seat {
        return fail!(
            "exec of /bin/roulette kept {:#x} beyond its grant",
            everything
        );
    }
    if mechanism != CAP_UNIVERSAL {
        return fail!("exec of /bin/halt kept {:#x}", mechanism);
    }
    pass!()
}

/// `Launch` bounds the one raise site, and is held by exactly the launchers.
///
/// Without it any task obtains a privileged child by spawning the privileged
/// path -- an entitlement leaking to an unrelated program with nothing granted
/// to it.
fn launch_is_held_by_the_launchers_only() -> TestResult {
    if mask_permits(CAP_UNIVERSAL, Capability::Launch) {
        return fail!("an ordinary task must not hold Launch");
    }
    if let Err(path) = conferred_on_exactly(
        Capability::Launch,
        &[b"/bin/shell", b"/bin/terminal", b"/bin/remoted"],
    ) {
        return fail!("Launch is misgranted at {:?}", core::str::from_utf8(path));
    }
    pass!()
}

slopos_testing::stest!(name = exec_intersection_never_widens, suite = authority);
slopos_testing::stest!(
    name = launch_is_held_by_the_launchers_only,
    suite = authority
);

/// `BootEntry` adds to `Power`, under which the UEFI variable calls are
/// classified, so the installer's role without `Power` reaches no variable.
fn boot_entry_alone_reaches_no_variable() -> TestResult {
    let role_alone = CAP_UNIVERSAL | Capability::BootEntry.bit() | Capability::Seal.bit();
    let installer = role_alone | Capability::Power.bit();
    for sysno in [
        slopos_abi::syscall::SYSCALL_EFIVAR_GET,
        slopos_abi::syscall::SYSCALL_EFIVAR_SET,
    ] {
        let Some(entry) = syscall_lookup(sysno) else {
            return fail!("efivar syscall {} is not registered", sysno);
        };
        if entry.cap != Capability::Power
            || decide(role_alone, entry.cap) != AuthorityDecision::Deny
            || decide(installer, entry.cap) != AuthorityDecision::Allow
        {
            return fail!("efivar must be gated on Power, which the role alone does not pass");
        }
    }
    pass!()
}

slopos_testing::stest!(
    name = boot_entry_alone_reaches_no_variable,
    suite = authority
);
