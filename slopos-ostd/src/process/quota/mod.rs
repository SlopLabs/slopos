//! Resource accounting: every kernel allocation is charged to an account, and
//! the charge is a linear token that lives inside the thing it accounts for.
//!
//! An account is a row in a fixed `.bss` arena named by a generation-stamped
//! [`AccountId`](crate::process::AccountId), one per
//! [`Process`](crate::process::Process) plus one kernel-owned root. Its parent
//! edge is set at creation and re-pointed only when that parent is released,
//! to the grandparent.
//!
//! [`try_charge`] debits the leaf and every ancestor and hands back a linear
//! [`Reservation`]; the charged object's constructor consumes that reservation
//! and stores a [`Charge`]; the same `Drop` that releases the object refunds.
//! The token's fields are private, so no service crate can forge, clone or
//! bypass a `Charge`.
//!
//! `Charge::drop` touches nothing but atomics on `.bss` and holds no counted
//! reference, so it is legal from a hard IRQ, from under a cli-spinlock, from
//! the IRQ-off switch tail, and from a dying task's own unwind.
//!
//! The type system guarantees the *token* is unique, never that the number
//! matches reality; [`ledger_audit`] is the only mechanism that can see a
//! forgotten or unwinder-skipped charge.
//!
//! Ceilings are measured, and deliberately live in two places: the enforced
//! runtime default in [`slopos_abi::quota`], and the gate ceiling in
//! `scripts/gates/quota/<variant>.txt`. Deriving either from the other would
//! make the ratchet measure its own configuration.

mod arena;
mod axis;
mod charged;
mod disk;
mod token;

pub use arena::{
    AccountCreateError, KindStats, LedgerFault, MAX_ACCOUNT_DEPTH, NO_LIMIT, PagesReconciler,
    TryChargeError, account_count, account_create, account_depth, account_release,
    account_release_by_slot, fit_disk_blocks, for_each_account, held_by, ledger_audit, quota_mode,
    register_pages_reconciler, reset_for_test, root, set_derived_process_limit, set_limit,
    set_quota_mode, stats, try_charge,
};
pub use axis::{Refundable, ResourceAxis};
pub use charged::{
    AliasOf, ChargeAuditEntry, Charged, FileBacking, SharedCharge, charge_audit_entries,
    sealed as charged_sealed,
};
pub use disk::{blocks_held, charge_blocks, refund_blocks};
pub use token::{Charge, ChargeSlot, Reservation};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::AccountId;
    use crate::process::account::alloc_generation_for_test;
    use crate::test_support::global_lock::{GlobalTestStateGuard, lock_global_test_state};
    use slopos_abi::quota::{FdSlot, ObjectRow, ProcCount, QuotaMode, ResourceKind};

    /// A fresh account on `slot`, debiting through `parent`. The generation
    /// comes from the global counter, so no two accounts in a run share one
    /// even on the same slot.
    fn account(slot: u32, parent: AccountId) -> AccountId {
        let id = AccountId::from_parts(slot, alloc_generation_for_test());
        account_create(id, parent).expect("create");
        id
    }

    /// The arena is process-global, so this serialises against every other
    /// global-state test and clears rows a sibling would inherit as ancestors.
    fn fixture() -> impl Drop {
        struct Guard(GlobalTestStateGuard);
        impl Drop for Guard {
            fn drop(&mut self) {
                reset_for_test();
            }
        }
        let guard = lock_global_test_state();
        reset_for_test();
        set_quota_mode(QuotaMode::Enforce);
        Guard(guard)
    }

    fn used(id: AccountId, kind: ResourceKind) -> u32 {
        stats(id, kind).map_or(0, |s| s.used)
    }

    #[test]
    fn a_charge_debits_the_leaf_and_every_ancestor() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);

        let charge = Charge::commit(try_charge::<FdSlot>(child, 3).expect("charge"));
        assert_eq!(used(child, ResourceKind::FdSlot), 3);
        assert_eq!(
            used(parent, ResourceKind::FdSlot),
            3,
            "the ancestor pays too"
        );
        assert_eq!(
            used(root(), ResourceKind::FdSlot),
            3,
            "and so does the root"
        );

        drop(charge);
        assert_eq!(used(child, ResourceKind::FdSlot), 0);
        assert_eq!(used(parent, ResourceKind::FdSlot), 0);
        assert_eq!(used(root(), ResourceKind::FdSlot), 0);
    }

    /// The raw-device right frees a principal of the block ceiling, and losing
    /// it restores the default.
    #[test]
    fn the_block_ceiling_follows_the_raw_device_right() {
        use crate::authority::caps_from_task_flags;
        use slopos_abi::task::{TASK_FLAG_MOUNT, TASK_FLAG_USER_MODE};
        let _f = fixture();
        let installer = account(1, root());
        let limit = |id| stats(id, ResourceKind::DiskBlocks).map(|s| s.limit);
        let default = slopos_abi::quota::default_process_limit(ResourceKind::DiskBlocks);
        assert_eq!(limit(installer), Some(default));
        fit_disk_blocks(
            installer,
            caps_from_task_flags(TASK_FLAG_USER_MODE | TASK_FLAG_MOUNT),
        );
        assert_eq!(limit(installer), Some(NO_LIMIT));
        fit_disk_blocks(installer, caps_from_task_flags(TASK_FLAG_USER_MODE));
        assert_eq!(limit(installer), Some(default));
    }

    /// L4: a batch that succeeds at level *k* and fails at *k+1* leaves every
    /// row exactly as it found it.
    #[test]
    fn a_refusal_partway_up_is_the_identity_on_every_row() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);
        set_limit(parent, ResourceKind::Process, 4);

        let held = Charge::commit(try_charge::<ProcCount>(child, 4).expect("fill the parent"));
        assert_eq!(used(child, ResourceKind::Process), 4);

        let refused = try_charge::<ProcCount>(child, 1).expect_err("the parent is full");
        assert_eq!(refused.refused_by, parent, "the refusing level is named");
        assert_eq!(refused.errno, slopos_abi::Errno::EAGAIN);
        assert_eq!(
            used(child, ResourceKind::Process),
            4,
            "the leaf debit must be unwound, not left behind"
        );
        assert_eq!(used(parent, ResourceKind::Process), 4);

        drop(held);
        assert_eq!(used(child, ResourceKind::Process), 0);
        assert_eq!(used(parent, ResourceKind::Process), 0);
    }

    /// A subtree-bounded kind still names its holder: a child's commit is the
    /// child's alone, and a charge the machine refuses leaves no share behind.
    #[test]
    fn a_subtree_charge_is_held_by_its_own_account_alone() {
        use slopos_abi::quota::CommitPagesAxis;
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);
        set_limit(root(), ResourceKind::CommitPages, 10);

        let parents = Charge::commit(try_charge::<CommitPagesAxis>(parent, 2).expect("parent"));
        let childs = Charge::commit(try_charge::<CommitPagesAxis>(child, 7).expect("child"));
        assert_eq!(used(parent, ResourceKind::CommitPages), 9);
        assert_eq!(held_by(parent, ResourceKind::CommitPages), 2);
        assert_eq!(held_by(child, ResourceKind::CommitPages), 7);

        try_charge::<CommitPagesAxis>(child, 2).expect_err("the machine is full");
        assert_eq!(
            held_by(child, ResourceKind::CommitPages),
            7,
            "a refused charge must leave the holder's share as it was"
        );

        drop(childs);
        assert_eq!(held_by(child, ResourceKind::CommitPages), 0);
        assert_eq!(held_by(parent, ResourceKind::CommitPages), 2);
        drop(parents);
        assert_eq!(held_by(parent, ResourceKind::CommitPages), 0);
    }

    /// A per-process ceiling is an `RLIMIT_*`: what a child holds is its own,
    /// and never spends the parent's.
    #[test]
    fn a_principal_ceiling_bounds_the_process_not_its_descendants() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);
        set_limit(parent, ResourceKind::FdSlot, 4);

        let childs = Charge::commit(try_charge::<FdSlot>(child, 10).expect("the child's own"));
        assert_eq!(
            used(parent, ResourceKind::FdSlot),
            10,
            "still counted above"
        );
        let parents = Charge::commit(try_charge::<FdSlot>(parent, 4).expect("the parent's own"));
        let refused = try_charge::<FdSlot>(parent, 1).expect_err("the parent's own is full");
        assert_eq!(refused.refused_by, parent);
        assert_eq!(used(parent, ResourceKind::FdSlot), 14);

        drop(parents);
        let refilled = Charge::commit(try_charge::<FdSlot>(parent, 4).expect("refunded own"));
        drop(refilled);
        drop(childs);
        assert_eq!(used(parent, ResourceKind::FdSlot), 0);
    }

    /// The root's ceilings are the machine's: they bound every principal's
    /// holdings together, whatever the kind's per-process scope.
    #[test]
    fn a_root_ceiling_bounds_every_kind_across_the_tree() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);
        set_limit(root(), ResourceKind::FdSlot, 4);

        let held = Charge::commit(try_charge::<FdSlot>(child, 3).expect("under the root"));
        let refused = try_charge::<FdSlot>(parent, 2).expect_err("the machine is full");
        assert_eq!(refused.refused_by, root());
        assert_eq!(used(parent, ResourceKind::FdSlot), 3, "the refusal unwound");
        drop(held);
    }

    /// The share a released child hands up is its own, not the ancestor's, so
    /// it leaves the ancestor's per-process headroom where it was.
    #[test]
    fn a_released_childs_share_does_not_spend_the_parents_own() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);
        set_limit(parent, ResourceKind::FdSlot, 2);

        let parents = Charge::commit(try_charge::<FdSlot>(parent, 2).expect("the parent's own"));
        let outlives = Charge::commit(try_charge::<FdSlot>(child, 3).expect("the child's own"));
        account_release(child);
        assert_eq!(used(parent, ResourceKind::FdSlot), 2);
        try_charge::<FdSlot>(parent, 1).expect_err("the parent's own is still full");

        drop(outlives);
        drop(parents);
        let refilled = Charge::commit(try_charge::<FdSlot>(parent, 2).expect("own intact"));
        drop(refilled);
    }

    /// L2 as a step property: no successful charge leaves `used > limit`.
    #[test]
    fn no_granted_charge_exceeds_the_ceiling() {
        let _f = fixture();
        let a = account(1, root());
        set_limit(a, ResourceKind::FdSlot, 3);

        let mut held = crate::KVec::new();
        while let Ok(r) = try_charge::<FdSlot>(a, 1) {
            held.push(Charge::commit(r)).expect("hold");
            assert!(
                used(a, ResourceKind::FdSlot) <= 3,
                "used passed the limit on a granted charge"
            );
        }
        assert_eq!(held.len(), 3);
        assert_eq!(stats(a, ResourceKind::FdSlot).expect("row").denials, 1);
    }

    /// The charges outstanding against a released row are exactly the ones
    /// whose refunds are about to become generation-mismatch no-ops, so its
    /// ancestors would otherwise keep debits nothing can retire.
    #[test]
    fn releasing_an_account_hands_its_outstanding_amount_back_up() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);

        // A charge that outlives its process: an in-flight SCM_RIGHTS
        // reference, a keepalive pin, a task in the graveyard.
        let outlives = Charge::commit(try_charge::<FdSlot>(child, 4).expect("charge"));
        assert_eq!(used(root(), ResourceKind::FdSlot), 4);

        account_release(child);
        assert_eq!(
            used(parent, ResourceKind::FdSlot),
            0,
            "the parent must not keep a debit nothing can retire"
        );
        assert_eq!(used(root(), ResourceKind::FdSlot), 0);

        drop(outlives);
        assert_eq!(used(root(), ResourceKind::FdSlot), 0);
    }

    /// L5: a refund against a released row is a defined no-op, not a write
    /// into whichever principal holds that slot now.
    #[test]
    fn a_stale_refund_does_not_touch_the_slots_new_occupant() {
        let _f = fixture();
        let first = account(1, root());
        let charge = Charge::commit(try_charge::<FdSlot>(first, 5).expect("charge"));
        assert_eq!(used(root(), ResourceKind::FdSlot), 5);

        account_release(first);
        let second = account(1, root());
        assert_ne!(first, second, "same slot, different generation");
        let fresh = Charge::commit(try_charge::<FdSlot>(second, 2).expect("charge"));
        assert_eq!(used(second, ResourceKind::FdSlot), 2);

        drop(charge);
        assert_eq!(
            used(second, ResourceKind::FdSlot),
            2,
            "the stale refund must not credit the new occupant"
        );
        drop(fresh);
    }

    #[test]
    fn warn_grants_over_limit_charges_and_counts_them() {
        let _f = fixture();
        set_quota_mode(QuotaMode::Warn);
        let a = account(1, root());
        set_limit(a, ResourceKind::FdSlot, 1);

        let first = Charge::commit(try_charge::<FdSlot>(a, 1).expect("under the limit"));
        let second = Charge::commit(try_charge::<FdSlot>(a, 1).expect("warn grants"));
        let s = stats(a, ResourceKind::FdSlot).expect("row");
        assert_eq!(s.used, 2, "warn grants, so the peak is measurable");
        assert_eq!(s.peak, 2);
        assert_eq!(s.denials, 1, "and records what enforce would have refused");
        drop((first, second));
    }

    #[test]
    fn off_consults_no_ceiling_but_still_counts() {
        let _f = fixture();
        set_quota_mode(QuotaMode::Off);
        let a = account(1, root());
        set_limit(a, ResourceKind::FdSlot, 1);
        let held = Charge::commit(try_charge::<FdSlot>(a, 9).expect("off never refuses"));
        let s = stats(a, ResourceKind::FdSlot).expect("row");
        assert_eq!(s.used, 9);
        assert_eq!(s.denials, 0);
        drop(held);
    }

    #[test]
    fn peak_survives_the_charge_being_refunded() {
        let _f = fixture();
        let a = account(1, root());
        drop(Charge::commit(try_charge::<FdSlot>(a, 7).expect("charge")));
        let s = stats(a, ResourceKind::FdSlot).expect("row");
        assert_eq!(s.used, 0, "the charge is gone");
        assert_eq!(s.peak, 7, "but the high-water mark is what a ceiling needs");
    }

    #[test]
    fn kinds_do_not_bleed_into_one_another() {
        let _f = fixture();
        let a = account(1, root());
        let fd = Charge::commit(try_charge::<FdSlot>(a, 2).expect("charge"));
        assert_eq!(used(a, ResourceKind::FdSlot), 2);
        assert_eq!(used(a, ResourceKind::ObjectRow), 0);
        let row = Charge::commit(try_charge::<ObjectRow>(a, 5).expect("charge"));
        assert_eq!(used(a, ResourceKind::FdSlot), 2);
        assert_eq!(used(a, ResourceKind::ObjectRow), 5);
        drop((fd, row));
    }

    #[test]
    fn extend_grows_one_token_rather_than_accumulating_them() {
        let _f = fixture();
        let a = account(1, root());
        let charge = Charge::commit(try_charge::<FdSlot>(a, 2).expect("charge"));
        let charge = charge.try_extend(try_charge::<FdSlot>(a, 3).expect("charge"));
        assert_eq!(charge.amount(), 5);
        assert_eq!(used(a, ResourceKind::FdSlot), 5);

        let charge = charge.shrink(4);
        assert_eq!(charge.amount(), 1);
        assert_eq!(used(a, ResourceKind::FdSlot), 1);
        drop(charge);
        assert_eq!(used(a, ResourceKind::FdSlot), 0);
    }

    #[test]
    fn extend_across_accounts_does_not_migrate_the_charge() {
        let _f = fixture();
        let a = account(1, root());
        let b = account(2, root());
        let charge = Charge::commit(try_charge::<FdSlot>(a, 2).expect("charge"));
        let foreign = try_charge::<FdSlot>(b, 3).expect("charge");
        let charge = charge.try_extend(foreign);
        assert_eq!(charge.amount(), 2, "the charge did not move");
        assert_eq!(used(b, ResourceKind::FdSlot), 0, "and b was refunded");
        drop(charge);
    }

    /// `release` is the exit-latch path: it consumes the token, so there is no
    /// later `Drop` to double the refund.
    #[test]
    fn release_refunds_exactly_once() {
        let _f = fixture();
        let a = account(1, root());
        let charge = Charge::commit(try_charge::<FdSlot>(a, 4).expect("charge"));
        charge.release();
        assert_eq!(used(a, ResourceKind::FdSlot), 0);
        assert_eq!(used(root(), ResourceKind::FdSlot), 0);
    }

    /// The hierarchical rollback and the "caller changed its mind" path are
    /// deliberately the same code.
    #[test]
    fn an_uncommitted_reservation_refunds_on_drop() {
        let _f = fixture();
        let a = account(1, root());
        {
            let _r = try_charge::<FdSlot>(a, 6).expect("charge");
            assert_eq!(used(a, ResourceKind::FdSlot), 6);
        }
        assert_eq!(used(a, ResourceKind::FdSlot), 0);
    }

    /// A process spawned below the bound still gets a row, debiting through
    /// its spawner's nearest ancestor with room: the walk stays bounded, and
    /// the machine's ceiling still refuses it, charge and refund alike.
    #[test]
    fn creation_below_the_bound_debits_through_the_nearest_ancestor_with_room() {
        use slopos_abi::quota::CommitPagesAxis;
        let _f = fixture();
        let mut chain = [root(); MAX_ACCOUNT_DEPTH as usize];
        // The root occupies one level, so MAX_ACCOUNT_DEPTH - 1 more fit.
        for slot in 1..MAX_ACCOUNT_DEPTH as usize {
            chain[slot] = account(slot as u32, chain[slot - 1]);
        }
        let bottom = chain[MAX_ACCOUNT_DEPTH as usize - 1];
        let below = account(MAX_ACCOUNT_DEPTH as u32, bottom);
        let further = account(MAX_ACCOUNT_DEPTH as u32 + 1, below);
        assert_eq!(account_depth(below), Some(MAX_ACCOUNT_DEPTH));
        assert_eq!(account_depth(further), Some(MAX_ACCOUNT_DEPTH));

        set_limit(root(), ResourceKind::CommitPages, 10);
        let held = Charge::commit(try_charge::<CommitPagesAxis>(further, 6).expect("charge"));
        assert_eq!(used(root(), ResourceKind::CommitPages), 6, "the root pays");
        assert_eq!(held_by(further, ResourceKind::CommitPages), 6);
        let refused = try_charge::<CommitPagesAxis>(further, 5).expect_err("the machine is full");
        assert_eq!(refused.refused_by, root());

        drop(held);
        assert_eq!(used(root(), ResourceKind::CommitPages), 0);
        assert_eq!(used(chain[1], ResourceKind::CommitPages), 0);
        assert_eq!(held_by(further, ResourceKind::CommitPages), 0);
    }

    #[test]
    fn the_audit_accepts_a_consistent_ledger() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);
        let held = Charge::commit(try_charge::<FdSlot>(child, 3).expect("charge"));
        let other = Charge::commit(try_charge::<ObjectRow>(parent, 2).expect("charge"));

        let mut faults = KVecFaults::default();
        assert_eq!(ledger_audit(|f| faults.push(f)), 0, "{:?}", faults.first);
        drop((held, other));
        assert_eq!(ledger_audit(|f| faults.push(f)), 0);
    }

    /// The modelled failure: a refund that reached a descendant but not its
    /// ancestor, leaving the ancestor below the sum of the rows debiting
    /// through it.
    #[test]
    fn the_audit_catches_an_ancestor_that_lost_a_refund() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);
        let held = Charge::commit(try_charge::<FdSlot>(child, 4).expect("charge"));

        let mut faults = KVecFaults::default();
        assert_eq!(ledger_audit(|f| faults.push(f)), 0, "consistent to start");

        // Refund the parent alone, as a hand-written cancel loop that missed a
        // level would.
        arena::refund_raw_one_level_for_test(parent, ResourceKind::FdSlot, 4);

        let mut faults = KVecFaults::default();
        let found = ledger_audit(|f| faults.push(f));
        assert!(found > 0, "the audit must see an under-counted ancestor");
        assert!(
            matches!(
                faults.first,
                Some(LedgerFault::AncestorUnderCount {
                    kind: ResourceKind::FdSlot,
                    ancestor_used: 0,
                    children_used: 4,
                    ..
                })
            ),
            "{:?}",
            faults.first
        );

        // Undo the planted corruption before the real token drops: its refund
        // walks the same chain and would underflow the emptied row.
        arena::charge_raw_one_level_for_test(parent, ResourceKind::FdSlot, 4);
        drop(held);
    }

    /// Collects audit findings without allocating: the audit runs in contexts
    /// where a `KVec` push would be the wrong thing to do.
    #[derive(Default)]
    struct KVecFaults {
        first: Option<LedgerFault>,
        count: usize,
    }

    impl KVecFaults {
        fn push(&mut self, fault: LedgerFault) {
            if self.first.is_none() {
                self.first = Some(fault);
            }
            self.count += 1;
        }
    }

    /// The exit latch of a `Task` and the reap of a `Process` can each fire
    /// twice, so "refunds exactly once" has to be a property of the slot
    /// rather than of the caller.
    #[test]
    fn a_charge_slot_releases_exactly_once() {
        let _f = fixture();
        let a = account(1, root());
        let slot: ChargeSlot<FdSlot> = ChargeSlot::empty();

        slot.put(try_charge::<FdSlot>(a, 3).expect("charge"));
        assert!(slot.is_occupied());
        assert_eq!(used(a, ResourceKind::FdSlot), 3);

        slot.take();
        assert!(!slot.is_occupied());
        assert_eq!(used(a, ResourceKind::FdSlot), 0);

        slot.take();
        assert_eq!(used(a, ResourceKind::FdSlot), 0);
    }

    /// A recycled id whose predecessor never reached its latch must cost
    /// nothing, so an overwrite refunds the charge it displaces.
    #[test]
    fn a_charge_slot_refunds_what_it_displaces() {
        let _f = fixture();
        let a = account(1, root());
        let slot: ChargeSlot<FdSlot> = ChargeSlot::empty();

        slot.put(try_charge::<FdSlot>(a, 3).expect("charge"));
        slot.put(try_charge::<FdSlot>(a, 5).expect("charge"));
        assert_eq!(
            used(a, ResourceKind::FdSlot),
            5,
            "the displaced charge must be given back, not leaked"
        );
        slot.take();
        assert_eq!(used(a, ResourceKind::FdSlot), 0);
    }

    /// A latched charge is released in pieces as the resource is given back,
    /// so over-shrinking must saturate rather than credit the account for an
    /// amount it never held.
    #[test]
    fn a_charge_slot_shrinks_by_what_it_holds_and_no_more() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);
        let slot: ChargeSlot<FdSlot> = ChargeSlot::empty();

        slot.put(try_charge::<FdSlot>(child, 3).expect("charge"));
        slot.grow(try_charge::<FdSlot>(child, 5).expect("charge"));
        assert_eq!(slot.amount(), 8);
        assert_eq!(used(child, ResourceKind::FdSlot), 8);

        slot.shrink(3);
        assert_eq!(slot.amount(), 5, "the remainder stays charged");
        assert_eq!(used(child, ResourceKind::FdSlot), 5);
        assert_eq!(used(parent, ResourceKind::FdSlot), 5, "and up the chain");

        slot.shrink(u32::MAX);
        assert_eq!(slot.amount(), 0);
        assert_eq!(used(child, ResourceKind::FdSlot), 0);
        assert_eq!(used(root(), ResourceKind::FdSlot), 0);

        slot.take();
        assert_eq!(used(child, ResourceKind::FdSlot), 0, "no second refund");
    }

    #[test]
    fn a_dropped_charge_slot_refunds() {
        let _f = fixture();
        let a = account(1, root());
        {
            let slot: ChargeSlot<FdSlot> = ChargeSlot::empty();
            slot.put(try_charge::<FdSlot>(a, 6).expect("charge"));
            assert_eq!(used(a, ResourceKind::FdSlot), 6);
        }
        assert_eq!(used(a, ResourceKind::FdSlot), 0);
    }

    /// A charge whose object outlives its account is handed to the root before
    /// the release: the machine's ceiling keeps counting it, no account holds
    /// it as its own, and its refund still reaches a live row. The dying
    /// account's other charges are credited back up as ever.
    #[test]
    fn a_bequeathed_charge_outlives_its_account_and_still_refunds() {
        use slopos_abi::quota::CommitPagesAxis;
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);
        set_limit(root(), ResourceKind::CommitPages, 10);

        let object: ChargeSlot<CommitPagesAxis> = ChargeSlot::empty();
        object.put(try_charge::<CommitPagesAxis>(child, 6).expect("the object's"));
        let parents: ChargeSlot<CommitPagesAxis> = ChargeSlot::empty();
        parents.put(try_charge::<CommitPagesAxis>(parent, 1).expect("the parent's"));
        let dies_with = Charge::commit(try_charge::<CommitPagesAxis>(child, 2).expect("own"));

        object.bequeath(child);
        parents.bequeath(child);
        assert_eq!(object.account(), root());
        assert_eq!(parents.account(), parent, "a slot charged elsewhere stays");
        assert_eq!(held_by(child, ResourceKind::CommitPages), 2);
        assert_eq!(held_by(parent, ResourceKind::CommitPages), 1);
        assert_eq!(used(child, ResourceKind::CommitPages), 2);
        assert_eq!(used(parent, ResourceKind::CommitPages), 3);
        assert_eq!(used(root(), ResourceKind::CommitPages), 9);
        assert_eq!(ledger_audit(|_| {}), 0);

        account_release(child);
        drop(dies_with);
        assert_eq!(used(root(), ResourceKind::CommitPages), 7);
        try_charge::<CommitPagesAxis>(parent, 4).expect_err("the object still fills the machine");
        assert_eq!(ledger_audit(|_| {}), 0);

        object.take();
        assert_eq!(held_by(parent, ResourceKind::CommitPages), 1);
        assert_eq!(used(root(), ResourceKind::CommitPages), 1);
        parents.take();
        assert_eq!(used(root(), ResourceKind::CommitPages), 0);
    }

    /// A child's hand-over that lands after its parent's own pass and before
    /// the parent's release, as when both die at once, still leaves the charge
    /// counted until the object goes.
    #[test]
    fn a_hand_over_between_the_parents_pass_and_its_release_stays_counted() {
        use slopos_abi::quota::CommitPagesAxis;
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);

        let object: ChargeSlot<CommitPagesAxis> = ChargeSlot::empty();
        object.put(try_charge::<CommitPagesAxis>(child, 6).expect("the object's"));

        object.bequeath(parent);
        object.bequeath(child);
        account_release(parent);
        account_release(child);
        assert_eq!(used(root(), ResourceKind::CommitPages), 6);
        assert_eq!(ledger_audit(|_| {}), 0);

        object.take();
        assert_eq!(used(root(), ResourceKind::CommitPages), 0);
    }

    /// Refusing would leave a process that had already been released unable to
    /// close its own descriptors.
    #[test]
    fn charging_a_released_account_is_vacuous_rather_than_refused() {
        let _f = fixture();
        let a = account(1, root());
        account_release(a);
        let charge = Charge::commit(try_charge::<FdSlot>(a, 3).expect("vacuous success"));
        assert_eq!(used(root(), ResourceKind::FdSlot), 0, "nothing was debited");
        drop(charge);
    }
}
