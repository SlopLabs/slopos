// Authority proof.
//
// A Verus-annotated mirror of SlopOS's capability model: the per-task mask in
// `slopos_ostd::task::ops::{task_caps, task_set_caps, task_restrict_caps}`,
// the exec narrowing in `slopos_core::exec::narrow_authority_for_exec`, the
// raise site in `slopos_core::exec::spawn_program_with_attrs`, the delegation
// check in `Endowment::check` beside it, `caps_drop`
// (`slopos_sched::task::process_drop_caps`), and `fork`'s explicit copy in
// `TaskInner::clone_from_raw`.
//
// It machine-checks five obligations:
//
//   (S1) MONOTONE. Every step except `Exec` and `Spawn` preserves or shrinks a
//        task's authority. Authority is never widened by acting, only by
//        loading a new program image — which is the single raise site the whole
//        model rests on.
//   (S2) EXEC INTERSECTS. `caps' == kept(image) & caps`, where the tree's
//        `kept` is `CAP_UNIVERSAL | grant(image) | delegated(image)` (the
//        delegated set only for an image named directly); the model's `grant`
//        operand is that whole set. Both halves are load-bearing: dropping the
//        `& caps` lets an unprivileged task exec a privileged image and gain its
//        grant; dropping the `kept(image)` lets a privileged program's authority
//        survive into an arbitrary binary. Those are the two CVE shapes this
//        model exists to prevent, two years apart in the same vendor's kernel,
//        on the same entitlement.
//   (S3) SPAWN BOUND. `child.caps == base | (Launch in parent.caps ? grant :
//        0) | (delegated ⊆ parent.caps ? delegated : 0)`, `base` being what the
//        kernel caller hands over (from the spawn syscall, S5's endowment).
//        A spawner without `Launch` confers nothing
//        beyond `base` and what it holds itself; one with `Launch` confers the
//        whole grant; none confers a delegated capability it lacks. Without the
//        `Launch` bound any task obtains a privileged child by spawning the
//        privileged path; without the subset check, by spawning a delegated
//        one. A task that dropped capabilities but kept `Launch` still raises a
//        granted image: dropping `Launch` is what stops it.
//   (S4) DROP TOTALITY. `task_restrict_caps` is total: it has no failure case
//        a caller could ignore, and neither has `caps_drop`, which takes it on
//        every task of the calling process. A historical local root came from
//        an attacker making a privilege *drop* fail inside a program that
//        ignored the result.
//   (S5) DELEGATION BOUND. A spawn asking for `ask` is refused unless
//        `ask ⊆ parent.caps`, and then hands over `(parent.caps & universal) |
//        ask`, so S3's `base` is within the spawner's own mask. Without the
//        check any task names a capability in `SpawnAttrs` and its child holds
//        it.
//
// Every obligation carries a BROKEN WITNESS that must fail `auth_inv` or the
// obligation's own postcondition, because a proof of a property nothing can
// violate is a proof about the model rather than about the tree.
//
// NOT COVERED, and deliberately excluded rather than assumed:
//
//   - The `Relaxed`/`Acquire`/`Release` ordering on `Task::caps`, and whether a
//     concurrent reader on another CPU observes the narrowed mask promptly.
//     Verus has no weak-memory model. Audited: the store is `Release`, the load
//     is `Acquire`, a narrowing is one `fetch_and`, and there is no
//     cross-process revocation, which is what licenses the cheap read. Exec
//     and `caps_drop` narrow every thread of the caller's process from the
//     caller's context, so a sibling's syscall already past its check
//     finishes under the mask it checked, as a Linux thread does across a
//     sibling's `setuid`.
//   - `caps_drop`'s reach. The tree records the drop on the process before
//     walking its tasks and `task_commit` narrows every thread the process
//     registers by that record, so a thread whose clone races the drop loses
//     the bits too. A fork or spawn racing the drop is ordered before it: the
//     child is a new process and keeps what its creator held when it was
//     read. The model has one task per state and so states neither.
//   - The kernel's own spawns: `launch_init` hands over `CAP_MASK_ALL` and the
//     utest runner what a test declares, whatever their caller holds. The
//     model's spawner is always a task.
//   - Ids. Which uid and gid a spawn may give its child is POSIX's rule over
//     `Cred`, held by the kernel's `cred` tests rather than this model, which
//     has no ids.
//   - That the dispatcher actually consults the mask before calling a handler.
//     That is a `rustc` totality assert over the syscall table plus
//     `scripts/check_authority_reachability.sh`, not a Verus obligation.
//
// The model is deliberately MORE PERMISSIVE than the tree in one respect:
// `Spawn` and `Exec` here take an arbitrary base, grant and delegated set,
// whereas the tree's `grant_for` and `delegated_for` read a fixed table keyed
// on the image path and the spawn syscall's base is S5's endowment.
// Proving the bounds for every possible input is strictly stronger than
// proving them for the rows the table holds.

use vstd::prelude::*;

verus! {

/// A capability mask. `u64` in the tree; `nat`-free `u64` here so the bitwise
/// operations are the ones the kernel actually performs.
pub type Caps = u64;

/// Bit 1 in `slopos_ostd::authority::Capability::Launch`. Named rather than
/// inlined so S3 reads as the rule it is.
pub open spec fn launch_bit() -> Caps {
    2
}

/// Does this mask name `Launch`?
pub open spec fn has_launch(c: Caps) -> bool {
    c & launch_bit() != 0
}

/// One task's authority state.
pub struct Auth {
    /// `Task::caps` — the effective mask.
    pub caps: Caps,
    /// Ghost: the mask this task was created with. Nothing may exceed it
    /// except an `Exec`, which is the point of tracking it.
    pub born_with: Caps,
    /// Ghost: how many times authority was raised outside a load. Must stay 0.
    pub raises: nat,
}

/// The inductive invariant.
pub open spec fn auth_inv(s: Auth) -> bool {
    // (S1) Authority is only ever raised by loading an image. Any other step
    //      that widened it would have incremented this.
    &&& s.raises == 0
}

/// The operations that can change a task's authority.
pub enum Step {
    /// A syscall, a signal, a scheduling decision — anything that is not a
    /// program load. Must not touch the mask.
    Act,
    /// Voluntary reduction, `task_restrict_caps(task, mask)`: keep only `mask`.
    /// `caps_drop(m)` is this step with `!m` on every task of the calling
    /// process, and `task_commit` takes it on each thread the process
    /// registers afterwards.
    Drop(Caps),
    /// `execve`: the mask becomes `kept & caps`, `kept` being
    /// `CAP_UNIVERSAL | grant | delegated` for the image.
    Exec(Caps),
    /// `fork`: the child is the same principal and copies the mask.
    Fork,
}

/// The state machine.
pub open spec fn step(s: Auth, e: Step) -> Auth {
    match e {
        Step::Act => s,
        // Total: an intersection, defined for every input, with no error case.
        Step::Drop(mask) => Auth { caps: s.caps & mask, ..s },
        // The intersection. Both operands, in this order.
        Step::Exec(grant) => Auth { caps: grant & s.caps, ..s },
        Step::Fork => s,
    }
}

/// `x & y` never has a bit that `x` lacks. The whole model reduces to this.
pub proof fn and_is_subset(x: Caps, y: Caps)
    ensures
        x & y == x & y,
        forall|b: Caps| #[trigger] (x & y & b) != 0 ==> (x & b) != 0,
{
    assert forall|b: Caps| #[trigger] (x & y & b) != 0 implies (x & b) != 0 by {
        assert((x & y & b) != 0 ==> (x & b) != 0) by (bit_vector);
    }
}

// ---------------------------------------------------------------------------
// S1 — monotone
// ---------------------------------------------------------------------------

/// Every step preserves the invariant.
pub proof fn step_preserves(s: Auth, e: Step)
    requires
        auth_inv(s),
    ensures
        auth_inv(step(s, e)),
{
}

/// `Act` and `Fork` do not touch the mask at all, and `Drop` can only shrink
/// it. Stated per-capability, because "shrinks" for a bitmask means no bit
/// appears that was not already there.
pub proof fn non_load_steps_never_widen(s: Auth, e: Step, b: Caps)
    requires
        auth_inv(s),
        !matches!(e, Step::Exec(_)),
        (step(s, e).caps & b) != 0,
    ensures
        (s.caps & b) != 0,
{
    match e {
        Step::Act => {},
        Step::Fork => {},
        Step::Drop(mask) => {
            // Bound to locals: `by (bit_vector)` reasons over plain integers,
            // not struct field projections.
            let c = s.caps;
            assert(((c & mask) & b) != 0 ==> (c & b) != 0) by (bit_vector);
        },
        Step::Exec(_) => {},
    }
}

// ---------------------------------------------------------------------------
// S2 — exec intersects
// ---------------------------------------------------------------------------

/// The exec result is bounded by BOTH operands. This is the obligation stated
/// exactly: a capability survives exec only if the image keeps it (the
/// universal set, its grant or its delegated set) *and* the task already held it.
pub proof fn exec_is_bounded_by_both(s: Auth, grant: Caps, b: Caps)
    requires
        (step(s, Step::Exec(grant)).caps & b) != 0,
    ensures
        (s.caps & b) != 0,
        (grant & b) != 0,
{
    let c = s.caps;
    assert(((grant & c) & b) != 0 ==> (c & b) != 0) by (bit_vector);
    assert(((grant & c) & b) != 0 ==> (grant & b) != 0) by (bit_vector);
}

/// An unprivileged task cannot gain a capability by exec'ing a privileged
/// image. The escalation direction, stated on its own because it is the half a
/// naive "just take the grant" implementation gets wrong.
pub proof fn exec_cannot_raise(s: Auth, grant: Caps, b: Caps)
    requires
        (s.caps & b) == 0,
    ensures
        (step(s, Step::Exec(grant)).caps & b) == 0,
{
    let c = s.caps;
    assert((c & b) == 0 ==> ((grant & c) & b) == 0) by (bit_vector);
}

/// An entitlement does not survive exec of an image that does not earn it.
/// The inheritance direction — the CVE where a privileged program execs an
/// arbitrary binary and the binary keeps the entitlement.
pub proof fn exec_drops_ungranted(s: Auth, grant: Caps, b: Caps)
    requires
        (grant & b) == 0,
    ensures
        (step(s, Step::Exec(grant)).caps & b) == 0,
{
    let c = s.caps;
    assert((grant & b) == 0 ==> ((grant & c) & b) == 0) by (bit_vector);
}

// ---------------------------------------------------------------------------
// S3 — the spawn bound
// ---------------------------------------------------------------------------

/// `CAP_UNIVERSAL`: `ConsoleIo | ClipboardGlobal | Fate`, bits 7 to 9, what the
/// spawn syscall hands the child of a spawner holding all three.
pub open spec fn universal() -> Caps {
    0x380
}

/// Bit 16 in `slopos_ostd::authority::Capability::RawBlock`, e2fsprogs'
/// delegated set.
pub open spec fn raw_block_bit() -> Caps {
    0x1_0000
}

/// The delegated set when the spawner holds all of it, else nothing.
pub open spec fn delegation(parent: Auth, delegated: Caps) -> Caps {
    if delegated & !parent.caps == 0 {
        delegated
    } else {
        0
    }
}

/// What a spawn gives the child, per the tree's rule: `base` from the kernel
/// caller, the grant under `Launch`, the delegated set as far as the spawner
/// holds it.
pub open spec fn spawn_child(parent: Auth, base: Caps, grant: Caps, delegated: Caps) -> Caps {
    base | (if has_launch(parent.caps) {
        grant
    } else {
        0
    }) | delegation(parent, delegated)
}

/// A spawner without `Launch` confers nothing beyond `base` and what it holds
/// itself, whatever the image's grant says.
pub proof fn spawn_without_launch_confers_only_what_it_holds(
    parent: Auth,
    base: Caps,
    grant: Caps,
    delegated: Caps,
    b: Caps,
)
    requires
        !has_launch(parent.caps),
        (spawn_child(parent, base, grant, delegated) & b) != 0,
    ensures
        (base & b) != 0 || (parent.caps & b) != 0,
{
    let c = parent.caps;
    let d = delegation(parent, delegated);
    assert(spawn_child(parent, base, grant, delegated) == base | 0u64 | d);
    if delegated & !c == 0 {
        assert((delegated & !c) == 0 && ((base | 0u64 | delegated) & b) != 0 ==> (base & b) != 0
            || (c & b) != 0) by (bit_vector);
    } else {
        assert(((base | 0u64 | 0u64) & b) != 0 ==> (base & b) != 0) by (bit_vector);
    }
}

/// A spawner WITH `Launch` hands over the image's whole grant — not the
/// parent's own authority, and not an intersection with it.
///
/// The intersection is the tempting mistake and it is arithmetically
/// self-defeating: the shell holds no display authority, so
/// `parent.caps & grant` would mean `/bin/roulette` could never draw.
pub proof fn spawn_with_launch_confers_the_grant(
    parent: Auth,
    base: Caps,
    grant: Caps,
    delegated: Caps,
)
    requires
        has_launch(parent.caps),
    ensures
        spawn_child(parent, base, grant, delegated) & grant == grant,
{
    let d = delegation(parent, delegated);
    assert(spawn_child(parent, base, grant, delegated) == base | grant | d);
    assert((base | grant | d) & grant == grant) by (bit_vector);
}

/// No spawner confers a delegated capability it lacks: a bit outside `base`
/// and the grant reaches the child only if the spawner holds it.
pub proof fn spawn_never_delegates_unheld(
    parent: Auth,
    base: Caps,
    grant: Caps,
    delegated: Caps,
    b: Caps,
)
    requires
        (parent.caps & b) == 0,
        (base & b) == 0,
        (grant & b) == 0,
    ensures
        (spawn_child(parent, base, grant, delegated) & b) == 0,
{
    let c = parent.caps;
    let g: Caps = if has_launch(c) {
        grant
    } else {
        0
    };
    assert((g & b) == 0) by {
        if !has_launch(c) {
            assert(0u64 & b == 0) by (bit_vector);
        }
    }
    if delegated & !c == 0 {
        assert(spawn_child(parent, base, grant, delegated) == base | g | delegated);
        assert((delegated & !c) == 0 && (c & b) == 0 && (base & b) == 0 && (g & b) == 0 ==> ((base
            | g | delegated) & b) == 0) by (bit_vector);
    } else {
        assert(spawn_child(parent, base, grant, delegated) == base | g | 0u64);
        assert((base & b) == 0 && (g & b) == 0 ==> ((base | g | 0u64) & b) == 0) by (bit_vector);
    }
}

/// A spawner holding the whole delegated set hands it over, `Launch` or not:
/// the installer gives `mke2fs` its `RawBlock`.
pub proof fn spawn_delegates_what_the_spawner_holds(
    parent: Auth,
    base: Caps,
    grant: Caps,
    delegated: Caps,
)
    requires
        delegated & !parent.caps == 0,
    ensures
        spawn_child(parent, base, grant, delegated) & delegated == delegated,
{
    let g: Caps = if has_launch(parent.caps) {
        grant
    } else {
        0
    };
    assert(spawn_child(parent, base, grant, delegated) == base | g | delegated);
    assert((base | g | delegated) & delegated == delegated) by (bit_vector);
}

// ---------------------------------------------------------------------------
// S4 — drop totality
// ---------------------------------------------------------------------------

/// `Drop` is defined for every mask and every state: there is no input for
/// which it has no result, hence no error a caller could ignore.
pub proof fn drop_is_total(s: Auth, mask: Caps)
    ensures
        step(s, Step::Drop(mask)) == (Auth { caps: s.caps & mask, ..s }),
{
}

/// Dropping is idempotent, so a caller that retries a reduction cannot end up
/// with more than one that succeeded.
pub proof fn drop_is_idempotent(s: Auth, mask: Caps)
    ensures
        step(step(s, Step::Drop(mask)), Step::Drop(mask)).caps == step(
            s,
            Step::Drop(mask),
        ).caps,
{
    let c = s.caps;
    assert((c & mask) & mask == c & mask) by (bit_vector);
}

/// Restricting to "everything" is a no-op — the monotonicity guard that makes
/// `task_restrict_caps` safe to call with any mask, including a wider one.
pub proof fn drop_to_all_is_identity(s: Auth)
    ensures
        step(s, Step::Drop(0xFFFF_FFFF_FFFF_FFFF)).caps == s.caps,
{
    let c = s.caps;
    assert(c & 0xFFFF_FFFF_FFFF_FFFFu64 == c) by (bit_vector);
}

// ---------------------------------------------------------------------------
// S5 — the delegation bound
// ---------------------------------------------------------------------------

/// Whether a spawn may ask for `ask`: the `EPERM` check.
pub open spec fn endow_permitted(parent: Auth, ask: Caps) -> bool {
    ask & !parent.caps == 0
}

/// What a permitted spawn hands the child before any grant: the spawner's
/// universal capabilities and what it asked to delegate.
pub open spec fn endow(parent: Auth, ask: Caps) -> Caps {
    (parent.caps & universal()) | ask
}

/// No permitted spawn endows a child with a capability its spawner lacks.
pub proof fn endowment_is_within_the_spawner(parent: Auth, ask: Caps, b: Caps)
    requires
        endow_permitted(parent, ask),
        (endow(parent, ask) & b) != 0,
    ensures
        (parent.caps & b) != 0,
{
    let c = parent.caps;
    assert((ask & !c) == 0 && (((c & 0x380u64) | ask) & b) != 0 ==> (c & b) != 0) by (bit_vector);
}

/// A permitted spawn hands over everything it asked for.
pub proof fn endowment_carries_the_ask(parent: Auth, ask: Caps)
    requires
        endow_permitted(parent, ask),
    ensures
        endow(parent, ask) & ask == ask,
{
    let c = parent.caps;
    assert(((c & 0x380u64) | ask) & ask == ask) by (bit_vector);
}

// ---------------------------------------------------------------------------
// Broken witnesses
//
// Each is a variant the tree deliberately does NOT implement. Every one must
// break something, or the corresponding obligation is vacuous.
// ---------------------------------------------------------------------------

/// BROKEN: a step that widens authority without a load — the shape any "just
/// add the bit" convenience would take.
pub open spec fn broken_widening_step(s: Auth, extra: Caps) -> Auth {
    Auth { caps: s.caps | extra, raises: (s.raises + 1) as nat, ..s }
}

/// Witness that S1 is load-bearing: a widening step exists that breaks the
/// invariant, while every real `Step` preserves it.
pub proof fn broken_widening_violates_invariant()
    ensures
        exists|s: Auth, extra: Caps|
            #![trigger broken_widening_step(s, extra)]
            auth_inv(s) && !auth_inv(broken_widening_step(s, extra)),
        forall|s: Auth, e: Step| auth_inv(s) ==> #[trigger] auth_inv(step(s, e)),
{
    let ordinary = Auth { caps: 0, born_with: 0, raises: 0 };
    assert(auth_inv(ordinary));
    let widened = broken_widening_step(ordinary, launch_bit());
    assert(widened.raises == 1);
    assert(!auth_inv(widened));
    assert(exists|s: Auth, extra: Caps|
        #![trigger broken_widening_step(s, extra)]
        auth_inv(s) && !auth_inv(broken_widening_step(s, extra)));
    assert forall|s: Auth, e: Step| auth_inv(s) implies #[trigger] auth_inv(step(s, e)) by {
        step_preserves(s, e);
    }
}

/// BROKEN: exec that takes the grant WITHOUT intersecting — an unprivileged
/// task exec'ing a privileged image gains its authority.
pub open spec fn broken_exec_ungated(_s: Auth, grant: Caps) -> Caps {
    grant
}

/// Witness that the `& caps` half of S2 is load-bearing.
pub proof fn broken_exec_ungated_escalates()
    ensures
        exists|s: Auth, grant: Caps, b: Caps|
            #![trigger broken_exec_ungated(s, grant) & b]
            (s.caps & b) == 0 && (broken_exec_ungated(s, grant) & b) != 0,
{
    // An ordinary task exec'ing an image whose identity earns `Launch`.
    let ordinary = Auth { caps: 0, born_with: 0, raises: 0 };
    assert(0u64 & 2u64 == 0) by (bit_vector);
    assert(broken_exec_ungated(ordinary, launch_bit()) == launch_bit());
    assert(2u64 & 2u64 != 0) by (bit_vector);
    assert(exists|s: Auth, grant: Caps, b: Caps|
        #![trigger broken_exec_ungated(s, grant) & b]
        (s.caps & b) == 0 && (broken_exec_ungated(s, grant) & b) != 0);
}

/// BROKEN: exec that keeps the old mask un-intersected — the entitlement
/// survives into an arbitrary binary.
pub open spec fn broken_exec_keeps_caps(s: Auth, _grant: Caps) -> Caps {
    s.caps
}

/// Witness that the `grant &` half of S2 is load-bearing.
pub proof fn broken_exec_keeps_caps_inherits()
    ensures
        exists|s: Auth, grant: Caps, b: Caps|
            #![trigger broken_exec_keeps_caps(s, grant) & b]
            (grant & b) == 0 && (broken_exec_keeps_caps(s, grant) & b) != 0,
{
    // A privileged program exec'ing an image that earns nothing.
    let privileged = Auth { caps: launch_bit(), born_with: launch_bit(), raises: 0 };
    assert(broken_exec_keeps_caps(privileged, 0) == launch_bit());
    assert(0u64 & 2u64 == 0) by (bit_vector);
    assert(2u64 & 2u64 != 0) by (bit_vector);
    assert(exists|s: Auth, grant: Caps, b: Caps|
        #![trigger broken_exec_keeps_caps(s, grant) & b]
        (grant & b) == 0 && (broken_exec_keeps_caps(s, grant) & b) != 0);
}

/// Witness that S5's check is load-bearing: an endowment taken unchecked
/// gives an ordinary task's child `Launch`, against
/// `endowment_is_within_the_spawner`.
pub proof fn broken_endowment_unchecked_raises()
    ensures
        exists|p: Auth, ask: Caps, b: Caps|
            #![trigger endow(p, ask) & b]
            (p.caps & b) == 0 && (endow(p, ask) & b) != 0,
{
    let ordinary = Auth { caps: 0, born_with: 0, raises: 0 };
    assert(endow(ordinary, launch_bit()) == (0u64 & 0x380u64) | 2u64);
    assert(((0u64 & 0x380u64) | 2u64) & 2u64 != 0) by (bit_vector);
    assert(0u64 & 2u64 == 0) by (bit_vector);
    assert(exists|p: Auth, ask: Caps, b: Caps|
        #![trigger endow(p, ask) & b]
        (p.caps & b) == 0 && (endow(p, ask) & b) != 0);
}

/// BROKEN: a spawn that ignores the `Launch` bound.
pub open spec fn broken_spawn_unbounded(
    parent: Auth,
    base: Caps,
    grant: Caps,
    delegated: Caps,
) -> Caps {
    base | grant | delegation(parent, delegated)
}

/// Witness that S3's `Launch` bound is load-bearing: without it an
/// unprivileged spawner produces a child holding what neither `base` nor the
/// spawner holds, against `spawn_without_launch_confers_only_what_it_holds`.
pub proof fn broken_spawn_unbounded_raises()
    ensures
        exists|p: Auth, base: Caps, grant: Caps, delegated: Caps, b: Caps|
            #![trigger broken_spawn_unbounded(p, base, grant, delegated) & b]
            !has_launch(p.caps) && (base & b) == 0 && (p.caps & b) == 0 && (
            broken_spawn_unbounded(p, base, grant, delegated) & b) != 0,
{
    // An ordinary task spawning an image whose identity earns `Launch`.
    let ordinary = Auth { caps: 0, born_with: 0, raises: 0 };
    assert(0u64 & 2u64 == 0) by (bit_vector);
    assert(!has_launch(ordinary.caps));
    assert(0u64 & !0u64 == 0) by (bit_vector);
    assert(delegation(ordinary, 0) == 0);
    assert(broken_spawn_unbounded(ordinary, universal(), launch_bit(), 0) == 0x380u64 | 2u64
        | 0u64);
    assert((0x380u64 | 2u64 | 0u64) & 2u64 != 0) by (bit_vector);
    assert(0x380u64 & 2u64 == 0) by (bit_vector);
    assert(exists|p: Auth, base: Caps, grant: Caps, delegated: Caps, b: Caps|
        #![trigger broken_spawn_unbounded(p, base, grant, delegated) & b]
        !has_launch(p.caps) && (base & b) == 0 && (p.caps & b) == 0 && (broken_spawn_unbounded(
            p,
            base,
            grant,
            delegated,
        ) & b) != 0);
}

/// BROKEN: the spawn bound written as an intersection with the parent's own
/// authority — the tempting mistake S3 names.
pub open spec fn broken_spawn_intersects_parent(
    parent: Auth,
    base: Caps,
    grant: Caps,
    delegated: Caps,
) -> Caps {
    base | (parent.caps & grant) | delegation(parent, delegated)
}

/// Witness that the intersection form is not merely different but WRONG: a
/// launcher holding `Launch` and nothing else could confer no grant at all, so
/// no privileged program could ever start, against
/// `spawn_with_launch_confers_the_grant`.
pub proof fn broken_spawn_intersection_starves_the_grant()
    ensures
        exists|p: Auth, base: Caps, grant: Caps, delegated: Caps|
            #![trigger broken_spawn_intersects_parent(p, base, grant, delegated)]
            has_launch(p.caps) && broken_spawn_intersects_parent(p, base, grant, delegated) & grant
                != grant,
{
    // The shell: holds `Launch`, holds no display authority.
    let shell = Auth { caps: launch_bit(), born_with: launch_bit(), raises: 0 };
    // `/bin/roulette`'s grant: a display bit, which the shell does not hold.
    let display: Caps = 16;
    assert(2u64 & 2u64 != 0) by (bit_vector);
    assert(has_launch(shell.caps));
    assert(0u64 & !2u64 == 0) by (bit_vector);
    assert(delegation(shell, 0) == 0);
    assert(broken_spawn_intersects_parent(shell, universal(), display, 0) == 0x380u64 | (2u64
        & 16u64) | 0u64);
    assert((0x380u64 | (2u64 & 16u64) | 0u64) & 16u64 != 16u64) by (bit_vector);
    assert(exists|p: Auth, base: Caps, grant: Caps, delegated: Caps|
        #![trigger broken_spawn_intersects_parent(p, base, grant, delegated)]
        has_launch(p.caps) && broken_spawn_intersects_parent(p, base, grant, delegated) & grant
            != grant);
}

/// BROKEN: delegation without the subset check — the image's delegated set
/// handed over whether or not the spawner holds it.
pub open spec fn broken_spawn_delegates_unheld(
    parent: Auth,
    base: Caps,
    grant: Caps,
    delegated: Caps,
) -> Caps {
    base | (if has_launch(parent.caps) {
        grant
    } else {
        0
    }) | delegated
}

/// Witness that S3's subset check is load-bearing: without it the shell,
/// which holds `Launch` and no `RawBlock`, starts `mke2fs` able to write any
/// disk raw, against `spawn_never_delegates_unheld`.
pub proof fn broken_spawn_delegation_raises()
    ensures
        exists|p: Auth, base: Caps, grant: Caps, delegated: Caps, b: Caps|
            #![trigger broken_spawn_delegates_unheld(p, base, grant, delegated) & b]
            (p.caps & b) == 0 && (base & b) == 0 && (grant & b) == 0 && (
            broken_spawn_delegates_unheld(p, base, grant, delegated) & b) != 0,
{
    let shell = Auth { caps: launch_bit(), born_with: launch_bit(), raises: 0 };
    assert(2u64 & 2u64 != 0) by (bit_vector);
    assert(has_launch(shell.caps));
    // `mke2fs`: no grant, `RawBlock` delegated.
    assert(broken_spawn_delegates_unheld(shell, universal(), 0, raw_block_bit()) == 0x380u64
        | 0u64 | 0x1_0000u64);
    assert((0x380u64 | 0u64 | 0x1_0000u64) & 0x1_0000u64 != 0) by (bit_vector);
    assert(2u64 & 0x1_0000u64 == 0) by (bit_vector);
    assert(0x380u64 & 0x1_0000u64 == 0) by (bit_vector);
    assert(0u64 & 0x1_0000u64 == 0) by (bit_vector);
    assert(exists|p: Auth, base: Caps, grant: Caps, delegated: Caps, b: Caps|
        #![trigger broken_spawn_delegates_unheld(p, base, grant, delegated) & b]
        (p.caps & b) == 0 && (base & b) == 0 && (grant & b) == 0 && (broken_spawn_delegates_unheld(
            p,
            base,
            grant,
            delegated,
        ) & b) != 0);
}

/// BROKEN: a fallible drop. Modelled as a reduction that sometimes leaves the
/// mask untouched — what a caller that ignores an error return observes.
pub open spec fn broken_drop_may_fail(s: Auth, mask: Caps, fails: bool) -> Caps {
    if fails {
        s.caps
    } else {
        s.caps & mask
    }
}

/// Witness that S4's totality matters: a fallible drop leaves authority the
/// caller believes it dropped, which is the historical local-root shape.
pub proof fn broken_fallible_drop_retains_authority()
    ensures
        exists|s: Auth, mask: Caps, b: Caps|
            #![trigger broken_drop_may_fail(s, mask, true) & b]
            ((s.caps & mask) & b) == 0 && (broken_drop_may_fail(s, mask, true) & b) != 0,
{
    let privileged = Auth { caps: launch_bit(), born_with: launch_bit(), raises: 0 };
    // The program asks to drop everything.
    assert(2u64 & 0u64 == 0) by (bit_vector);
    assert((2u64 & 0u64) & 2u64 == 0) by (bit_vector);
    assert(broken_drop_may_fail(privileged, 0, true) == launch_bit());
    assert(launch_bit() & launch_bit() != 0) by (bit_vector);
    assert(exists|s: Auth, mask: Caps, b: Caps|
        #![trigger broken_drop_may_fail(s, mask, true) & b]
        ((s.caps & mask) & b) == 0 && (broken_drop_may_fail(s, mask, true) & b) != 0);
}

} // verus!

fn main() {}
