//! The authority model's load-bearing claims, checked in the kernel.
//!
//! These test the *decision*, not the effect: a test that actually called
//! `halt` would power the machine off mid-run. What matters is that the
//! syscall classification says the right things, because the dispatcher's
//! behaviour is a total function of it and a task's mask. A mechanism's
//! authority comes from privd's delegation, which `userland`'s `privd_test`
//! grades.

use slopos_abi::syscall::{
    SYSCALL_PRIVATE_BASE, SYSCALL_PRIVATE_END, SYSCALL_REBOOT, SYSCALL_ROULETTE_RESULT,
    SYSCALL_TABLE_SIZE,
};
use slopos_ostd::authority::{CAP_MASK_ALL, CAP_NONE, CAP_UNIVERSAL, Capability, mask_permits};
use slopos_testing::{TestResult, fail, pass};

use crate::syscall::handlers::{cap_counts, syscall_lookup};
use crate::syscall::signal::Sender;
use slopos_ostd::process::Cred;

/// `halt` and `reboot` take `Power`, so an ordinary task cannot power the
/// machine off.
fn power_is_denied_to_an_ordinary_task() -> TestResult {
    if mask_permits(CAP_UNIVERSAL, Capability::Power) {
        return fail!("an ordinary user task must not hold Power");
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
    }
    pass!()
}

slopos_testing::stest!(
    name = power_is_denied_to_an_ordinary_task,
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
    use slopos_abi::Errno;
    use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE};
    use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
    use slopos_sched::task::{task_create, task_find_by_id, task_terminate};

    let init_id = crate::exec::init_task_id();
    let Some(init) = task_find_by_id(init_id) else {
        return fail!("init is not running in the userland phase");
    };
    let other_id = task_create(
        b"NotInit",
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

/// An exec keeps a task's capabilities only for init re-executing the base's
/// `/sbin/init` as its sole thread; every other exec drops them.
fn only_init_reexecuting_itself_keeps_its_authority() -> TestResult {
    use core::num::NonZeroU64;
    use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE};
    use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
    use slopos_ostd::process::Program;
    use slopos_sched::task::{task_create, task_find_by_id, task_terminate};

    use crate::exec::{INIT_PATH, exec_keeps_authority};

    let Some(init) = task_find_by_id(crate::exec::init_task_id()) else {
        return fail!("init is not running in the userland phase");
    };
    let other_id = task_create(
        b"NotInit",
        slopos_sched::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
        core::ptr::null_mut(),
        1,
        TASK_FLAG_USER_MODE,
    );
    let Some(other) = task_find_by_id(other_id) else {
        return fail!("no test task");
    };
    let base = Program::Base(NonZeroU64::MIN);
    let answers = [
        exec_keeps_authority(&init, INIT_PATH, base, true),
        exec_keeps_authority(&other, INIT_PATH, base, true),
        exec_keeps_authority(&init, INIT_PATH, Program::Other, true),
        exec_keeps_authority(&init, b"/sbin/privd", base, true),
        exec_keeps_authority(&init, INIT_PATH, base, false),
    ];
    drop((init, other));
    if other_id != INVALID_TASK_ID {
        task_terminate(other_id);
    }
    if answers != [true, false, false, false, false] {
        return fail!(
            "exec kept authority for (init re-exec, another task, a program outside the \
             base, another base program, a second thread): {:?}",
            answers
        );
    }
    pass!()
}

slopos_testing::stest!(
    name = only_init_reexecuting_itself_keeps_its_authority,
    flags = 0,
    kind = Userland
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
            || mask_permits(role_alone, entry.cap)
            || !mask_permits(installer, entry.cap)
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
