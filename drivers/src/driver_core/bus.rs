//! The bus-agnostic device/driver model: one [`Bus`] trait, one [`probe_bus`]
//! matchmaker, one claim table shape.
//!
//! Each bus keeps its own `#[repr(C)]` link-section entry type and its own
//! enumerator; what is shared is the *binding protocol* — offer each device to
//! its candidate drivers in priority order, bind the first that returns
//! [`ProbeOutcome::Bound`], and hold the resources it acquired in that device's
//! claim slot for the life of the binding.
//!
//! Device-ID formats are inherently bus-specific, so the generic core never
//! interprets one: [`Bus::matches`] is the bus's own predicate and
//! [`DriverIndex`] only *narrows* the candidate set.

use slopos_ostd::dev::Devres;
use slopos_ostd::{AllocError, KBox, KVec, klog_info};

/// What a driver's probe decided about a device it was offered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeOutcome {
    /// The claim is recorded by device index; no other driver is offered this
    /// device.
    Bound,
    /// Matched but deliberately did not bind; lower-priority candidates are
    /// still offered the device.
    Declined,
}

/// Why a probe rejected a candidate device. Shared by every bus: the set is a
/// property of the binding protocol, not of any one bus's ID format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeError {
    /// Initial match passed but post-inspection rules rejected the candidate
    /// (e.g., feature negotiation failed).
    Mismatch,
    OutOfMemory,
    DeviceFault,
    /// A required capability (e.g., MSI-X) is unavailable on the device.
    Unsupported,
    /// Matched and would bind, but a dependency is not ready; the registry
    /// retries it in a later bounded pass.
    Deferred,
}

/// Runs when the device leaves, before the driver's resources are released.
pub trait Removal: Send + Sync {
    fn remove(&self);
}

/// A driver's claim on a device, stored in the per-device claim slot once its
/// probe returns [`ProbeOutcome::Bound`].
pub struct Binding {
    name: &'static str,
    removal: Option<KBox<dyn Removal>>,
}

impl Binding {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            removal: None,
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn remove(&mut self) {
        if let Some(removal) = self.removal.take() {
            removal.remove();
        }
    }
}

/// One bus's device/driver model.
///
/// Enumeration is deliberately **not** on this trait: a bus has no single
/// static device source (PCI reads a locked array, the platform bus builds a
/// heap list per call, a rescan would supply a third). [`probe_bus`] takes the
/// accessor as a parameter instead, which is also what keeps the 2 KiB stack
/// gate honest — a caller passes a single-element accessor and never copies a
/// device array onto a frame.
pub trait Bus: Sized + 'static {
    /// The bus's enumeration record, snapshot into a probe by `Copy`.
    type Device: Copy + 'static;
    /// The bus's `#[repr(C)]` link-section driver descriptor.
    type DriverEntry: 'static;

    const NAME: &'static str;

    fn entry_name(entry: &Self::DriverEntry) -> &'static str;

    /// Bind order, ascending: a lower value binds first, so a specific driver
    /// can beat a generic one for the same device.
    fn priority(entry: &Self::DriverEntry) -> u8;

    /// The bus's own match predicate, declarative table plus any imperative
    /// fallback. [`probe_bus`] treats this as opaque.
    fn matches(entry: &Self::DriverEntry, dev: &Self::Device) -> bool;

    fn probe(
        entry: &Self::DriverEntry,
        bound: &mut BoundDevice<'_, Self>,
    ) -> Result<ProbeOutcome, ProbeError>;
}

/// The capability a probe drives to acquire device resources.
///
/// Every vend hands ownership to the device's [`Devres`] bag, so a probe that
/// fails partway releases what it took in reverse order; on success the bag
/// lives for the binding's lifetime. Bus-agnostic vends live in
/// [`crate::driver_core::bound`]; the PCI-only and platform-only ones live
/// beside their bus.
pub struct BoundDevice<'d, B: Bus + 'static> {
    pub(crate) info: &'d B::Device,
    pub(crate) res: &'d mut Devres,
    pub(crate) removal: Option<KBox<dyn Removal>>,
}

impl<'d, B: Bus + 'static> BoundDevice<'d, B> {
    pub fn new(info: &'d B::Device, res: &'d mut Devres) -> Self {
        Self {
            info,
            res,
            removal: None,
        }
    }

    fn into_binding(self, name: &'static str) -> Binding {
        Binding {
            name,
            removal: self.removal,
        }
    }

    /// `B::Device` is `Copy`: snapshot it (`let info = *bound.info();`) to free
    /// the borrow for subsequent `&mut self` vend calls.
    #[inline]
    pub fn info(&self) -> &B::Device {
        self.info
    }
}

/// Per-device ownership slot. The binding is declared first so it drops first:
/// an unbind must quiesce the driver before the resource bag releases what a
/// late interrupt could still touch.
pub enum ClaimSlot {
    Unclaimed,
    Claimed {
        binding: Binding,
        // Held for its `Drop`, which releases the device's acquired resources.
        #[allow(dead_code)]
        devres: Devres,
    },
}

impl ClaimSlot {
    /// The resources stay until the slot drops.
    pub fn remove(&mut self) {
        if let ClaimSlot::Claimed { binding, .. } = self {
            binding.remove();
        }
    }
}

/// Records which driver owns each enumerated device, indexed by device index.
pub struct ClaimTable<const N: usize> {
    slots: [ClaimSlot; N],
}

impl<const N: usize> ClaimTable<N> {
    pub const fn new() -> Self {
        Self {
            slots: [const { ClaimSlot::Unclaimed }; N],
        }
    }

    pub fn is_claimed(&self, dev_idx: usize) -> bool {
        matches!(self.slots.get(dev_idx), Some(ClaimSlot::Claimed { .. }))
    }

    /// Only the USB bus unbinds.
    pub fn release(&mut self, dev_idx: usize) -> ClaimSlot {
        match self.slots.get_mut(dev_idx) {
            Some(slot) => core::mem::replace(slot, ClaimSlot::Unclaimed),
            None => ClaimSlot::Unclaimed,
        }
    }

    pub fn owner(&self, dev_idx: usize) -> Option<&'static str> {
        match self.slots.get(dev_idx) {
            Some(ClaimSlot::Claimed { binding, .. }) => Some(binding.name()),
            _ => None,
        }
    }

    /// Moving the resource bag is allocation-free (just its `KVec` header), so
    /// this is safe under a claim lock.
    pub fn claim(&mut self, dev_idx: usize, binding: Binding, devres: Devres) {
        if dev_idx < self.slots.len() {
            self.slots[dev_idx] = ClaimSlot::Claimed { binding, devres };
        }
    }
}

impl<const N: usize> Default for ClaimTable<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Abstracts a live `CLAIMED_BY` static (boot) from a heap-backed map (unit
/// tests), so the matchmaker core is exercisable over synthetic devices.
///
/// This is the only place [`probe_bus`] touches a lock, which is what keeps
/// probe itself lock-free.
pub trait ClaimSink {
    fn is_claimed(&self, dev_idx: usize) -> bool;
    fn record(&self, dev_idx: usize, binding: Binding, devres: Devres);
}

/// Narrows the driver registry to the candidates worth offering a device.
///
/// An implementation may over-approximate — [`probe_bus`] confirms every
/// candidate with [`Bus::matches`] — but must emit each driver at most once,
/// sorted by `(priority, link-index)` ascending.
pub trait DriverIndex<B: Bus> {
    fn entry(&self, li: u16) -> &'static B::DriverEntry;
    fn candidates_for(&self, dev: &B::Device, out: &mut KVec<u16>) -> Result<(), AllocError>;
}

/// Append `li` to `out` unless it is already present (a driver with two
/// matching rules must only probe a device once).
pub fn push_unique(out: &mut KVec<u16>, li: u16) -> Result<(), AllocError> {
    if !out.contains(&li) {
        out.push(li)?;
    }
    Ok(())
}

/// Sort candidate link-indices by `(priority, link-index)` ascending.
pub fn sort_candidates<B: Bus>(entries: &[&'static B::DriverEntry], out: &mut KVec<u16>) {
    out.sort_unstable_by(|&a, &b| {
        let pa = B::priority(entries[a as usize]);
        let pb = B::priority(entries[b as usize]);
        pa.cmp(&pb).then(a.cmp(&b))
    });
}

/// Every registered driver is a candidate for every device, in
/// `(priority, link-index)` order. The bus's own [`Bus::matches`] does the
/// filtering — right for a registry small enough that an index would cost more
/// than it saves.
pub struct LinearIndex<B: Bus> {
    entries: KVec<&'static B::DriverEntry>,
}

impl<B: Bus> LinearIndex<B> {
    /// Build over an explicit driver set; the in-QEMU unit tests pass synthetic
    /// drivers here.
    pub fn from_entries(entries: KVec<&'static B::DriverEntry>) -> Self {
        Self { entries }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.len() == 0
    }
}

impl<B: Bus> DriverIndex<B> for LinearIndex<B> {
    fn entry(&self, li: u16) -> &'static B::DriverEntry {
        self.entries[li as usize]
    }

    fn candidates_for(&self, _dev: &B::Device, out: &mut KVec<u16>) -> Result<(), AllocError> {
        out.clear();
        for i in 0..self.entries.len() {
            out.push(i as u16)?;
        }
        sort_candidates::<B>(self.entries.as_slice(), out);
        Ok(())
    }
}

/// Offer each device to its candidate drivers in priority order, recording the
/// first that binds, then run one bounded deferred-retry pass.
///
/// `B::probe` runs with **no lock held** — neither the bus's enumeration lock
/// nor its claim lock — so it may block and allocate. The only lock this
/// function reaches is the one behind `claims`, taken one call at a time and
/// never across a probe.
pub fn probe_bus<B: Bus>(
    idx: &dyn DriverIndex<B>,
    device_count: usize,
    device_at: &dyn Fn(usize) -> Option<B::Device>,
    claims: &dyn ClaimSink,
) -> Result<(), AllocError> {
    let mut cands: KVec<u16> = KVec::new();
    let mut deferred: KVec<(u16, usize)> = KVec::new();

    for dev_idx in 0..device_count {
        if claims.is_claimed(dev_idx) {
            continue;
        }
        let Some(dev) = device_at(dev_idx) else {
            continue;
        };
        idx.candidates_for(&dev, &mut cands)?;
        for k in 0..cands.len() {
            let li = cands[k];
            let entry = idx.entry(li);
            if !B::matches(entry, &dev) {
                continue;
            }
            match offer::<B>(entry, &dev, dev_idx, claims) {
                Offer::Bound => break,
                Offer::Passed => continue,
                Offer::Deferred => {
                    deferred.push((li, dev_idx))?;
                    continue;
                }
            }
        }
    }

    for n in 0..deferred.len() {
        let (li, dev_idx) = deferred[n];
        if claims.is_claimed(dev_idx) {
            continue;
        }
        let Some(dev) = device_at(dev_idx) else {
            continue;
        };
        let entry = idx.entry(li);
        if !B::matches(entry, &dev) {
            continue;
        }
        if let Offer::Deferred = offer::<B>(entry, &dev, dev_idx, claims) {
            klog_info!(
                "{}: {} gave up on device {} after deferral",
                B::NAME,
                B::entry_name(entry),
                dev_idx
            );
        }
    }
    Ok(())
}

enum Offer {
    Bound,
    Passed,
    Deferred,
}

pub enum Probed {
    /// With the resources the probe acquired.
    Bound(Binding, Devres),
    Unbound,
}

/// The first binding, for a bus that records claims itself: a deferral is a
/// decline, as such a bus has no later pass.
pub fn probe_one<B: Bus>(
    idx: &dyn DriverIndex<B>,
    dev: &B::Device,
    dev_idx: usize,
) -> Result<Probed, AllocError> {
    let mut cands: KVec<u16> = KVec::new();
    idx.candidates_for(dev, &mut cands)?;
    for &li in cands.iter() {
        let entry = idx.entry(li);
        if !B::matches(entry, dev) {
            continue;
        }
        if let Ok(probed @ Probed::Bound(..)) = attempt::<B>(entry, dev, dev_idx) {
            return Ok(probed);
        }
    }
    Ok(Probed::Unbound)
}

/// `#[inline(never)]` so the `Devres` bag, the `BoundDevice` and the log
/// scratch stay in this frame rather than accumulating in the caller's.
#[inline(never)]
fn attempt<B: Bus>(
    entry: &'static B::DriverEntry,
    dev: &B::Device,
    dev_idx: usize,
) -> Result<Probed, ProbeError> {
    // On `Bound` the bag moves out with the binding; otherwise it drops here,
    // releasing every acquired resource in reverse order.
    let mut devres = Devres::new();
    let mut bound = BoundDevice::<B>::new(dev, &mut devres);
    let outcome = B::probe(entry, &mut bound);
    match outcome {
        Ok(ProbeOutcome::Bound) => {
            let binding = bound.into_binding(B::entry_name(entry));
            Ok(Probed::Bound(binding, devres))
        }
        Ok(ProbeOutcome::Declined) => Ok(Probed::Unbound),
        Err(err) => {
            if err != ProbeError::Deferred {
                klog_info!(
                    "{}: {} declined device {}: {:?}",
                    B::NAME,
                    B::entry_name(entry),
                    dev_idx,
                    err
                );
            }
            Err(err)
        }
    }
}

fn offer<B: Bus>(
    entry: &'static B::DriverEntry,
    dev: &B::Device,
    dev_idx: usize,
    claims: &dyn ClaimSink,
) -> Offer {
    match attempt::<B>(entry, dev, dev_idx) {
        Ok(Probed::Bound(binding, devres)) => {
            claims.record(dev_idx, binding, devres);
            Offer::Bound
        }
        Ok(Probed::Unbound) => Offer::Passed,
        Err(ProbeError::Deferred) => Offer::Deferred,
        Err(_) => Offer::Passed,
    }
}
