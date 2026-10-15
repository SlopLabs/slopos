// Authority proof.
//
// A Verus-annotated mirror of SlopOS's capability model: the per-task mask in
// `slopos_ostd::task::ops::{task_caps, task_set_caps, task_restrict_caps}`,
// exec's drop in `slopos_core::exec::drop_authority_for_exec`, the spawn
// bound in `Endowment::check` beside it, `caps_drop`
// (`slopos_sched::task::process_drop_caps`, and the record `task_commit`
// narrows each thread a process registers by), and `fork`'s explicit copy in
// `TaskInner::clone_from_raw`.
//
// It machine-checks four obligations:
//
//   (S1) MONOTONE. No step widens a task's authority: acting and forking
//        leave its mask as it is, dropping and exec'ing narrow it. A new
//        task's authority comes from its creator, by fork's copy or by S3,
//        and never from the program it runs.
//   (S2) EXEC DROPS. `caps' == caps & universal`: every gated capability goes,
//        and a universal one stays only where the task held it. Both halves
//        are load-bearing: an exec that keeps the mask lets a privileged
//        program's authority survive into an arbitrary binary, and one that
//        hands the new image the universal set gives back what the task
//        dropped. The one exec that keeps the mask is init re-executing the
//        base's `/sbin/init` as its sole thread (`exec_keeps_authority`): it
//        loads no program but init's own, so S1 still holds of it. The model
//        takes that guard as given; `only_init_reexecuting_itself_keeps_its_
//        authority` grades it.
//   (S3) SPAWN BOUND. A spawn asking for `ask` is refused unless
//        `ask ⊆ parent.caps`, and then hands over `(parent.caps & universal) |
//        ask`, so a child holds nothing its spawner lacks and nothing gated it
//        was not handed, whatever program it runs. Without the check any task
//        names a capability in `SpawnAttrs` and its child holds it; without
//        the `& parent.caps`, a dropped universal capability comes back on
//        every child; and a spawn that added a grant for the program it loads
//        would raise a child above any spawner.
//   (S4) DROP TOTALITY. `caps_drop(m)` has no failure case a caller could
//        ignore, and takes `m` from every thread of the calling process: the
//        threads registered when it runs, and through the process's record,
//        a thread whose clone read its creator's mask before the drop and
//        registers after. A historical local root came from an attacker
//        making a privilege *drop* fail inside a program that ignored the
//        result.
//
// Every obligation carries a BROKEN WITNESS that must fail its own
// postcondition, because a proof of a property nothing can violate is a proof
// about the model rather than about the tree.
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
//   - That the tree's walk reaches every registered thread. The model's
//     process is the sequence of its threads' masks; the tree's is every task
//     whose process handle matches, which `process_drop_caps` walks with an
//     allocation-free enumeration. A fork or spawn racing the drop is ordered
//     before it: the child is a new process and keeps what its creator held
//     when it was read.
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
// Deleted with the grant table: the witness that a spawn bound written as
// `parent.caps & grant` starved a grant (the shell held no display authority,
// so `/bin/roulette` could never have drawn). It defended conferral by a
// launcher that did not hold what it conferred; authority now flows only from
// holders, and that intersection is S3 itself.

use vstd::prelude::*;

verus! {

/// A capability mask, a `u64` as in the tree, so the bitwise operations are
/// the kernel's.
pub type Caps = u64;

/// `CAP_UNIVERSAL`: `ConsoleIo | ClipboardGlobal | Fate`, bits 7 to 9.
pub open spec fn universal() -> Caps {
    0x380
}

/// Bit 0 in `slopos_ostd::authority::Capability::Power`, a gated capability
/// for the witnesses.
pub open spec fn power_bit() -> Caps {
    1
}

/// Bit 9 in `slopos_ostd::authority::Capability::Fate`, a universal one.
pub open spec fn fate_bit() -> Caps {
    0x200
}

/// One task's authority state: `Task::caps`, the effective mask.
pub struct Auth {
    pub caps: Caps,
}

/// The operations that can change a task's authority.
pub enum Step {
    /// A syscall, a signal, a scheduling decision — anything that is not a
    /// drop or a program load. Must not touch the mask.
    Act,
    /// Voluntary reduction, `task_restrict_caps(task, mask)`: keep only `mask`.
    /// S4 states `caps_drop`, which takes it on every thread of a process.
    Drop(Caps),
    /// `execve`: the mask becomes `caps & universal`.
    Exec,
    /// Init re-executing `/sbin/init`: the mask stays as it is.
    Reexec,
    /// `fork`: the child is the same principal and copies the mask.
    Fork,
}

/// The state machine.
pub open spec fn step(s: Auth, e: Step) -> Auth {
    match e {
        Step::Act => s,
        // Total: an intersection, defined for every input, with no error case.
        Step::Drop(mask) => Auth { caps: s.caps & mask },
        Step::Exec => Auth { caps: s.caps & universal() },
        Step::Reexec => s,
        Step::Fork => s,
    }
}

// ---------------------------------------------------------------------------
// S1 — monotone
// ---------------------------------------------------------------------------

/// No step widens the mask. Stated per capability, because "narrows" for a
/// bitmask means no bit appears that was not already there.
pub proof fn no_step_widens(s: Auth, e: Step, b: Caps)
    requires
        (step(s, e).caps & b) != 0,
    ensures
        (s.caps & b) != 0,
{
    let c = s.caps;
    match e {
        Step::Act => {},
        Step::Reexec => {},
        Step::Fork => {},
        Step::Drop(mask) => {
            // Bound to locals: `by (bit_vector)` reasons over plain integers,
            // not struct field projections.
            assert(((c & mask) & b) != 0 ==> (c & b) != 0) by (bit_vector);
        },
        Step::Exec => {
            assert(((c & 0x380u64) & b) != 0 ==> (c & b) != 0) by (bit_vector);
        },
    }
}

// ---------------------------------------------------------------------------
// S2 — exec drops
// ---------------------------------------------------------------------------

/// No gated capability survives an exec, whatever the task held.
pub proof fn exec_keeps_nothing_gated(s: Auth, b: Caps)
    requires
        (b & universal()) == 0,
    ensures
        (step(s, Step::Exec).caps & b) == 0,
{
    let c = s.caps;
    assert((b & 0x380u64) == 0 ==> ((c & 0x380u64) & b) == 0) by (bit_vector);
}

/// A universal capability the task held survives its exec.
pub proof fn exec_keeps_the_universal_set_it_held(s: Auth, b: Caps)
    requires
        (b & !universal()) == 0,
        (s.caps & b) == b,
    ensures
        (step(s, Step::Exec).caps & b) == b,
{
    let c = s.caps;
    assert((b & !0x380u64) == 0 && (c & b) == b ==> ((c & 0x380u64) & b) == b) by (bit_vector);
}

/// Nothing a task lacks comes back at its exec: a dropped universal
/// capability stays dropped.
pub proof fn exec_cannot_raise(s: Auth, b: Caps)
    requires
        (s.caps & b) == 0,
    ensures
        (step(s, Step::Exec).caps & b) == 0,
{
    let c = s.caps;
    assert((c & b) == 0 ==> ((c & 0x380u64) & b) == 0) by (bit_vector);
}

// ---------------------------------------------------------------------------
// S3 — the spawn bound
// ---------------------------------------------------------------------------

/// Whether a spawn may ask for `ask`: the `EPERM` check.
pub open spec fn endow_permitted(parent: Auth, ask: Caps) -> bool {
    ask & !parent.caps == 0
}

/// What a permitted spawn hands the child: the spawner's universal
/// capabilities and what it asked to delegate. No operand names the program.
pub open spec fn endow(parent: Auth, ask: Caps) -> Caps {
    (parent.caps & universal()) | ask
}

/// No permitted spawn endows a child with a capability its spawner lacks.
pub proof fn spawn_is_within_the_spawner(parent: Auth, ask: Caps, b: Caps)
    requires
        endow_permitted(parent, ask),
        (endow(parent, ask) & b) != 0,
    ensures
        (parent.caps & b) != 0,
{
    let c = parent.caps;
    assert((ask & !c) == 0 && (((c & 0x380u64) | ask) & b) != 0 ==> (c & b) != 0) by (bit_vector);
}

/// A child holds nothing gated that its spawner did not hand it.
pub proof fn spawn_hands_only_what_it_asks(parent: Auth, ask: Caps, b: Caps)
    requires
        (b & universal()) == 0,
        (ask & b) == 0,
    ensures
        (endow(parent, ask) & b) == 0,
{
    let c = parent.caps;
    assert((b & 0x380u64) == 0 && (ask & b) == 0 ==> (((c & 0x380u64) | ask) & b) == 0)
        by (bit_vector);
}

/// A permitted spawn hands over everything it asked for: the installer gives
/// `mke2fs` its `RawBlock`.
pub proof fn spawn_carries_the_ask(parent: Auth, ask: Caps)
    requires
        endow_permitted(parent, ask),
    ensures
        endow(parent, ask) & ask == ask,
{
    let c = parent.caps;
    assert(((c & 0x380u64) | ask) & ask == ask) by (bit_vector);
}

// ---------------------------------------------------------------------------
// S4 — drop totality, process-wide
// ---------------------------------------------------------------------------

/// A process: what `caps_drop` has recorded on it, and each registered
/// thread's mask.
pub struct Process {
    pub dropped: Caps,
    pub threads: Seq<Caps>,
}

/// `caps_drop(m)`: the record first, then every registered thread.
pub open spec fn caps_drop(p: Process, m: Caps) -> Process {
    Process { dropped: p.dropped | m, threads: p.threads.map_values(|c: Caps| c & !m) }
}

/// `task_commit` of a thread whose clone read `creator` from its creator,
/// at any time before it registers: narrowed by the record as it registers.
pub open spec fn register(p: Process, creator: Caps) -> Process {
    Process { threads: p.threads.push(creator & !p.dropped), ..p }
}

/// `caps_drop` is defined for every process and mask: there is no input for
/// which it has no result, hence no error a caller could ignore.
pub proof fn caps_drop_is_total(p: Process, m: Caps)
    ensures
        caps_drop(p, m).threads.len() == p.threads.len(),
        caps_drop(p, m).dropped == p.dropped | m,
{
}

/// Every thread registered when `caps_drop` runs loses what it names, and
/// keeps every other bit it held.
pub proof fn caps_drop_reaches_every_thread(p: Process, m: Caps, i: int, b: Caps)
    requires
        0 <= i < p.threads.len(),
    ensures
        caps_drop(p, m).threads[i] & m == 0,
        (b & m) == 0 ==> (caps_drop(p, m).threads[i] & b) == (p.threads[i] & b),
{
    let c = p.threads[i];
    assert(caps_drop(p, m).threads[i] == c & !m);
    assert((c & !m) & m == 0) by (bit_vector);
    assert((b & m) == 0 ==> ((c & !m) & b) == (c & b)) by (bit_vector);
}

/// A thread registered after the drop lacks what it names, whatever its
/// creator held when the clone read the mask.
pub proof fn a_thread_registered_after_lacks_it(p: Process, m: Caps, creator: Caps)
    ensures
        ({
            let q = register(caps_drop(p, m), creator);
            q.threads[q.threads.len() - 1] & m == 0
        }),
{
    let q = register(caps_drop(p, m), creator);
    let d = p.dropped;
    assert(q.threads[q.threads.len() - 1] == creator & !(d | m));
    assert((creator & !(d | m)) & m == 0) by (bit_vector);
}

/// Dropping again changes nothing, so a caller that retries a drop cannot end
/// up holding more than one that succeeded.
pub proof fn caps_drop_is_idempotent(p: Process, m: Caps, i: int)
    requires
        0 <= i < p.threads.len(),
    ensures
        caps_drop(caps_drop(p, m), m).threads[i] == caps_drop(p, m).threads[i],
{
    let c = p.threads[i];
    assert(caps_drop(p, m).threads[i] == c & !m);
    assert(caps_drop(caps_drop(p, m), m).threads[i] == (c & !m) & !m);
    assert((c & !m) & !m == c & !m) by (bit_vector);
}

// ---------------------------------------------------------------------------
// Broken witnesses
//
// Each is a variant the tree deliberately does NOT implement. Every one must
// break something, or the corresponding obligation is vacuous.
// ---------------------------------------------------------------------------

/// BROKEN: a step that widens authority — the shape any "just add the bit"
/// convenience, or a grant at exec, would take.
pub open spec fn broken_widening_step(s: Auth, extra: Caps) -> Auth {
    Auth { caps: s.caps | extra }
}

/// Witness that S1 is load-bearing: a widening step gives a task a bit it
/// lacked, against `no_step_widens`.
pub proof fn broken_widening_step_raises()
    ensures
        exists|s: Auth, extra: Caps, b: Caps|
            #![trigger broken_widening_step(s, extra).caps & b]
            (broken_widening_step(s, extra).caps & b) != 0 && (s.caps & b) == 0,
{
    let ordinary = Auth { caps: 0 };
    assert((0u64 | 1u64) & 1u64 != 0) by (bit_vector);
    assert(0u64 & 1u64 == 0) by (bit_vector);
    assert(exists|s: Auth, extra: Caps, b: Caps|
        #![trigger broken_widening_step(s, extra).caps & b]
        (broken_widening_step(s, extra).caps & b) != 0 && (s.caps & b) == 0) by {
        assert(broken_widening_step(ordinary, power_bit()).caps == 0u64 | 1u64);
    }
}

/// BROKEN: an exec that keeps the mask — a privileged program's authority
/// survives into whatever it execs.
pub open spec fn broken_exec_keeps_caps(s: Auth) -> Caps {
    s.caps
}

/// Witness that the `& universal` half of S2 is load-bearing, against
/// `exec_keeps_nothing_gated`.
pub proof fn broken_exec_keeps_caps_inherits()
    ensures
        exists|s: Auth, b: Caps|
            #![trigger broken_exec_keeps_caps(s) & b]
            (b & universal()) == 0 && (broken_exec_keeps_caps(s) & b) != 0,
{
    let privileged = Auth { caps: power_bit() };
    assert(1u64 & 0x380u64 == 0) by (bit_vector);
    assert(1u64 & 1u64 != 0) by (bit_vector);
    assert(exists|s: Auth, b: Caps|
        #![trigger broken_exec_keeps_caps(s) & b]
        (b & universal()) == 0 && (broken_exec_keeps_caps(s) & b) != 0) by {
        assert(broken_exec_keeps_caps(privileged) == power_bit());
    }
}

/// BROKEN: an exec that clears the mask.
pub open spec fn broken_exec_clears(_s: Auth) -> Caps {
    0
}

/// Witness that S2 keeps what it should: a task holding `Fate` loses it to an
/// exec that clears the mask, against `exec_keeps_the_universal_set_it_held`.
pub proof fn broken_exec_clears_loses_fate()
    ensures
        exists|s: Auth, b: Caps|
            #![trigger broken_exec_clears(s) & b]
            (b & !universal()) == 0 && (s.caps & b) == b && (broken_exec_clears(s) & b) != b,
{
    let ordinary = Auth { caps: universal() };
    assert(0x200u64 & !0x380u64 == 0) by (bit_vector);
    assert(0x380u64 & 0x200u64 == 0x200u64) by (bit_vector);
    assert(0u64 & 0x200u64 != 0x200u64) by (bit_vector);
    assert(exists|s: Auth, b: Caps|
        #![trigger broken_exec_clears(s) & b]
        (b & !universal()) == 0 && (s.caps & b) == b && (broken_exec_clears(s) & b) != b) by {
        assert(broken_exec_clears(ordinary) == 0u64);
        assert((ordinary.caps & fate_bit()) == fate_bit());
    }
}

/// BROKEN: an exec that hands the new image the universal set.
pub open spec fn broken_exec_hands_universal(_s: Auth) -> Caps {
    universal()
}

/// Witness that the `caps &` half of S2 is load-bearing: a task that dropped
/// `Fate` gets it back by exec'ing, against `exec_cannot_raise`.
pub proof fn broken_exec_hands_universal_restores()
    ensures
        exists|s: Auth, b: Caps|
            #![trigger broken_exec_hands_universal(s) & b]
            (s.caps & b) == 0 && (broken_exec_hands_universal(s) & b) != 0,
{
    let dropped_fate = Auth { caps: 0x180 };
    assert(0x180u64 & 0x200u64 == 0) by (bit_vector);
    assert(0x380u64 & 0x200u64 != 0) by (bit_vector);
    assert(exists|s: Auth, b: Caps|
        #![trigger broken_exec_hands_universal(s) & b]
        (s.caps & b) == 0 && (broken_exec_hands_universal(s) & b) != 0) by {
        assert(broken_exec_hands_universal(dropped_fate) == universal());
        assert((dropped_fate.caps & fate_bit()) == 0);
    }
}

/// Witness that S3's check is load-bearing: an endowment taken unchecked
/// gives an ordinary task's child `Power`, against
/// `spawn_is_within_the_spawner`.
pub proof fn broken_endowment_unchecked_raises()
    ensures
        exists|p: Auth, ask: Caps, b: Caps|
            #![trigger endow(p, ask) & b]
            (p.caps & b) == 0 && (endow(p, ask) & b) != 0,
{
    let ordinary = Auth { caps: universal() };
    assert(endow(ordinary, power_bit()) == (0x380u64 & 0x380u64) | 1u64);
    assert(((0x380u64 & 0x380u64) | 1u64) & 1u64 != 0) by (bit_vector);
    assert(0x380u64 & 1u64 == 0) by (bit_vector);
    assert(exists|p: Auth, ask: Caps, b: Caps|
        #![trigger endow(p, ask) & b]
        (p.caps & b) == 0 && (endow(p, ask) & b) != 0);
}

/// BROKEN: a spawn that hands every child the universal set.
pub open spec fn broken_spawn_hands_universal(_parent: Auth, ask: Caps) -> Caps {
    universal() | ask
}

/// Witness that S3's `parent.caps &` is load-bearing: a spawner that dropped
/// `Fate` hands it back to its child, against `spawn_is_within_the_spawner`.
pub proof fn broken_spawn_hands_universal_restores()
    ensures
        exists|p: Auth, ask: Caps, b: Caps|
            #![trigger broken_spawn_hands_universal(p, ask) & b]
            endow_permitted(p, ask) && (p.caps & b) == 0 && (broken_spawn_hands_universal(p, ask)
                & b) != 0,
{
    let dropped_fate = Auth { caps: 0x180 };
    assert(0u64 & !0x180u64 == 0) by (bit_vector);
    assert(endow_permitted(dropped_fate, 0));
    assert(0x180u64 & 0x200u64 == 0) by (bit_vector);
    assert((0x380u64 | 0u64) & 0x200u64 != 0) by (bit_vector);
    assert(exists|p: Auth, ask: Caps, b: Caps|
        #![trigger broken_spawn_hands_universal(p, ask) & b]
        endow_permitted(p, ask) && (p.caps & b) == 0 && (broken_spawn_hands_universal(p, ask) & b)
            != 0) by {
        assert(broken_spawn_hands_universal(dropped_fate, 0) == 0x380u64 | 0u64);
    }
}

/// BROKEN: a spawn that hands the child its spawner's whole mask —
/// inheritance where the tree delegates.
pub open spec fn broken_spawn_inherits(parent: Auth, ask: Caps) -> Caps {
    parent.caps | ask
}

/// Witness that S3 hands only what is asked: a spawner holding `Power` that
/// delegates nothing still passes it on, which `spawn_is_within_the_spawner`
/// does not catch and `spawn_hands_only_what_it_asks` does.
pub proof fn broken_spawn_inherits_passes_power_on()
    ensures
        exists|p: Auth, ask: Caps, b: Caps|
            #![trigger broken_spawn_inherits(p, ask) & b]
            (b & universal()) == 0 && (ask & b) == 0 && (broken_spawn_inherits(p, ask) & b) != 0,
{
    let holder = Auth { caps: power_bit() };
    assert(1u64 & 0x380u64 == 0) by (bit_vector);
    assert(0u64 & 1u64 == 0) by (bit_vector);
    assert((1u64 | 0u64) & 1u64 != 0) by (bit_vector);
    assert(exists|p: Auth, ask: Caps, b: Caps|
        #![trigger broken_spawn_inherits(p, ask) & b]
        (b & universal()) == 0 && (ask & b) == 0 && (broken_spawn_inherits(p, ask) & b) != 0) by {
        assert(broken_spawn_inherits(holder, 0) == 1u64 | 0u64);
    }
}

/// BROKEN: the grant table this tree deleted — a spawn that adds what the
/// program it loads is granted.
pub open spec fn broken_spawn_raises_a_grant(parent: Auth, ask: Caps, grant: Caps) -> Caps {
    endow(parent, ask) | grant
}

/// Witness that S3 holds only because no operand names the program: a spawn
/// granting `/bin/halt` `Power` hands it to the child of a spawner holding the
/// universal set alone, against `spawn_is_within_the_spawner`.
pub proof fn broken_spawn_raises_a_grant_escalates()
    ensures
        exists|p: Auth, ask: Caps, grant: Caps, b: Caps|
            #![trigger broken_spawn_raises_a_grant(p, ask, grant) & b]
            endow_permitted(p, ask) && (p.caps & b) == 0 && (broken_spawn_raises_a_grant(
                p,
                ask,
                grant,
            ) & b) != 0,
{
    let ordinary = Auth { caps: universal() };
    assert(0u64 & !0x380u64 == 0) by (bit_vector);
    assert(endow_permitted(ordinary, 0));
    assert(0x380u64 & 1u64 == 0) by (bit_vector);
    assert((((0x380u64 & 0x380u64) | 0u64) | 1u64) & 1u64 != 0) by (bit_vector);
    assert(exists|p: Auth, ask: Caps, grant: Caps, b: Caps|
        #![trigger broken_spawn_raises_a_grant(p, ask, grant) & b]
        endow_permitted(p, ask) && (p.caps & b) == 0 && (broken_spawn_raises_a_grant(p, ask, grant)
            & b) != 0) by {
        assert(broken_spawn_raises_a_grant(ordinary, 0, power_bit()) == ((0x380u64 & 0x380u64)
            | 0u64) | 1u64);
    }
}

/// BROKEN: a drop that narrows only the calling thread.
pub open spec fn broken_drop_caller_only(p: Process, m: Caps) -> Process {
    Process { dropped: p.dropped | m, threads: p.threads.update(0, p.threads[0] & !m) }
}

/// Witness that S4's reach is load-bearing: a sibling of the dropping thread
/// keeps `Power`, against `caps_drop_reaches_every_thread`.
pub proof fn broken_drop_caller_only_spares_a_sibling()
    ensures
        exists|p: Process, m: Caps, i: int|
            #![trigger broken_drop_caller_only(p, m).threads[i]]
            0 <= i < p.threads.len() && broken_drop_caller_only(p, m).threads[i] & m != 0,
{
    let p = Process { dropped: 0, threads: seq![power_bit(), power_bit()] };
    let q = broken_drop_caller_only(p, power_bit());
    assert(q.threads[1] == power_bit());
    assert(1u64 & 1u64 != 0) by (bit_vector);
    assert(exists|p: Process, m: Caps, i: int|
        #![trigger broken_drop_caller_only(p, m).threads[i]]
        0 <= i < p.threads.len() && broken_drop_caller_only(p, m).threads[i] & m != 0);
}

/// BROKEN: a drop that narrows the threads but records nothing on the process.
pub open spec fn broken_drop_unrecorded(p: Process, m: Caps) -> Process {
    Process { threads: p.threads.map_values(|c: Caps| c & !m), ..p }
}

/// Witness that S4's record is load-bearing: a thread whose clone read the
/// creator's mask before the drop registers holding `Power`, against
/// `a_thread_registered_after_lacks_it`.
pub proof fn broken_drop_unrecorded_misses_a_racing_clone()
    ensures
        exists|p: Process, m: Caps, creator: Caps|
            #![trigger register(broken_drop_unrecorded(p, m), creator)]
            ({
                let q = register(broken_drop_unrecorded(p, m), creator);
                q.threads[q.threads.len() - 1] & m != 0
            }),
{
    let p = Process { dropped: 0, threads: seq![power_bit()] };
    let q = register(broken_drop_unrecorded(p, power_bit()), power_bit());
    assert(q.threads[q.threads.len() - 1] == 1u64 & !0u64);
    assert((1u64 & !0u64) & 1u64 != 0) by (bit_vector);
    assert(exists|p: Process, m: Caps, creator: Caps|
        #![trigger register(broken_drop_unrecorded(p, m), creator)]
        ({
            let q = register(broken_drop_unrecorded(p, m), creator);
            q.threads[q.threads.len() - 1] & m != 0
        }));
}

/// BROKEN: a fallible drop. Modelled as a reduction that sometimes leaves the
/// process untouched — what a caller that ignores an error return observes.
pub open spec fn broken_drop_may_fail(p: Process, m: Caps, fails: bool) -> Process {
    if fails {
        p
    } else {
        caps_drop(p, m)
    }
}

/// Witness that S4's totality matters: a fallible drop leaves authority the
/// caller believes it dropped, which is the historical local-root shape.
pub proof fn broken_fallible_drop_retains_authority()
    ensures
        exists|p: Process, m: Caps, i: int|
            #![trigger broken_drop_may_fail(p, m, true).threads[i]]
            0 <= i < p.threads.len() && broken_drop_may_fail(p, m, true).threads[i] & m != 0,
{
    let p = Process { dropped: 0, threads: seq![power_bit()] };
    assert(broken_drop_may_fail(p, power_bit(), true).threads[0] == power_bit());
    assert(1u64 & 1u64 != 0) by (bit_vector);
    assert(exists|p: Process, m: Caps, i: int|
        #![trigger broken_drop_may_fail(p, m, true).threads[i]]
        0 <= i < p.threads.len() && broken_drop_may_fail(p, m, true).threads[i] & m != 0);
}

} // verus!

fn main() {}
