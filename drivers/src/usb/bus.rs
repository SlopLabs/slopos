//! `UsbBus`, the third [`Bus`]: the bind thread offers each function of an
//! enumerated device to the `usb_driver!` registry, and unbinds it when the
//! device leaves. Hubs are the tree's and never offered.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, Mutex, SpinLock};
use slopos_ostd::{AllocError, KArc, KVec, KVecDeque, klog_info, lock_class};
use slopos_usb_core::bus::{Candidate, Node, Path, STORE_CONFIGURATION};
use slopos_usb_core::device::Speed;
use slopos_usb_core::device::descriptor::{Configuration, Function, MAX_FUNCTIONS};

use super::xhci::device::{Bind, BindState, Control, Device, Pipe, Posted, ReportSink, Reports};
use crate::driver_core::bound::BoundError;
use crate::driver_core::bus::{
    BoundDevice, Bus, ClaimTable, LinearIndex, ProbeError, ProbeOutcome, Probed, Removal, probe_one,
};

/// Functions bound at once across every controller.
pub const MAX_CLAIMS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbFunction {
    pub controller: u8,
    pub slot: u8,
    pub serial: u64,
    pub path: Path,
    pub speed: Speed,
    pub vendor: u16,
    pub product: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub first_interface: u8,
    pub interfaces: u8,
    pub configuration: u8,
}

impl UsbFunction {
    fn covers(&self, interface: u8) -> bool {
        interface >= self.first_interface
            && u16::from(interface) < u16::from(self.first_interface) + u16::from(self.interfaces)
    }
}

/// A declarative match rule; a `None` field matches anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbMatch {
    Class {
        class: u8,
        subclass: Option<u8>,
        protocol: Option<u8>,
    },
    Device {
        vendor: u16,
        product: u16,
    },
}

impl UsbMatch {
    fn matches(&self, vendor: u16, product: u16, function: &Function) -> bool {
        match *self {
            UsbMatch::Class {
                class,
                subclass,
                protocol,
            } => {
                function.class == class
                    && subclass.is_none_or(|s| s == function.subclass)
                    && protocol.is_none_or(|p| p == function.protocol)
            }
            UsbMatch::Device {
                vendor: v,
                product: p,
            } => vendor == v && product == p,
        }
    }
}

pub type BoundUsbDevice<'d> = BoundDevice<'d, UsbBus>;

/// A driver registered by [`usb_driver!`](crate::usb_driver).
#[repr(C)]
pub struct UsbDriverEntry {
    pub name: &'static str,
    pub match_table: &'static [UsbMatch],
    pub priority: u8,
    pub probe: fn(&mut BoundUsbDevice<'_>) -> Result<ProbeOutcome, ProbeError>,
}

impl UsbDriverEntry {
    fn wants(&self, vendor: u16, product: u16, function: &Function) -> bool {
        self.match_table
            .iter()
            .any(|m| m.matches(vendor, product, function))
    }
}

pub struct UsbBus;

fn function_of(dev: &UsbFunction) -> Function {
    Function {
        first_interface: dev.first_interface,
        interfaces: dev.interfaces,
        class: dev.class,
        subclass: dev.subclass,
        protocol: dev.protocol,
    }
}

impl Bus for UsbBus {
    type Device = UsbFunction;
    type DriverEntry = UsbDriverEntry;

    const NAME: &'static str = "USB";

    fn entry_name(entry: &UsbDriverEntry) -> &'static str {
        entry.name
    }

    fn priority(entry: &UsbDriverEntry) -> u8 {
        entry.priority
    }

    fn matches(entry: &UsbDriverEntry, dev: &UsbFunction) -> bool {
        entry.wants(dev.vendor, dev.product, &function_of(dev))
    }

    fn probe(
        entry: &UsbDriverEntry,
        bound: &mut BoundUsbDevice<'_>,
    ) -> Result<ProbeOutcome, ProbeError> {
        (entry.probe)(bound)
    }
}

impl slopos_ostd::ffi::registry::RegistryEntry for UsbDriverEntry {
    const REGISTRIES: &'static [slopos_ostd::ffi::registry::RegistryId] =
        &[slopos_ostd::ffi::registry::RegistryId::UsbDrivers];
}

pub fn driver_registry_iter() -> impl Iterator<Item = &'static UsbDriverEntry> {
    slopos_ostd::ffi::registry::registry_slice::<UsbDriverEntry>(
        slopos_ostd::ffi::registry::RegistryId::UsbDrivers,
    )
    .iter()
}

#[macro_export]
#[doc(hidden)]
macro_rules! __usb_driver_opt {
    (, $default:expr) => {
        $default
    };
    ($val:expr, $default:expr) => {
        $val
    };
}

/// `priority`, ascending, defaults to 128.
///
/// ```ignore
/// usb_driver! {
///     pub static USB_HID = {
///         name: "usb-hid",
///         match_table: &[UsbMatch::Class { class: 3, subclass: None, protocol: None }],
///         probe: hid_probe,
///     };
/// }
/// ```
#[macro_export]
macro_rules! usb_driver {
    (
        $(#[$attr:meta])*
        $vis:vis static $name:ident = {
            name: $drv_name:expr,
            match_table: $match_table:expr,
            $(priority: $priority:expr,)?
            probe: $probe:path $(,)?
        };
    ) => {
        slopos_ostd::registry_entry! {
            usb_drivers,
            $(#[$attr])*
            $vis static $name: $crate::usb::bus::UsbDriverEntry = $crate::usb::bus::UsbDriverEntry {
                name: $drv_name,
                match_table: $match_table,
                priority: $crate::__usb_driver_opt!($($priority)?, 128),
                probe: $probe,
            };
        }
    };
}

pub(super) fn wanted(candidate: &Candidate) -> bool {
    driver_registry_iter()
        .any(|e| e.wants(candidate.vendor, candidate.product, &candidate.function))
}

static CLAIMS: SpinLock<ClaimTable<MAX_CLAIMS>> = SpinLock::new(
    ClaimTable::new(),
    lock_class!("usb.CLAIMS", LOCK_LEVEL_RESOURCE),
);
static CLAIM_INDICES: [AtomicU64; MAX_CLAIMS / 64] = [const { AtomicU64::new(0) }; MAX_CLAIMS / 64];

fn take_claim_index() -> Option<u16> {
    for (word, bits) in CLAIM_INDICES.iter().enumerate() {
        let mut current = bits.load(Ordering::Acquire);
        while current != u64::MAX {
            let bit = current.trailing_ones();
            match bits.compare_exchange(
                current,
                current | 1 << bit,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some((word * 64) as u16 + bit as u16),
                Err(seen) => current = seen,
            }
        }
    }
    None
}

fn give_back_claim_index(index: u16) {
    if let Some(bits) = CLAIM_INDICES.get(usize::from(index) / 64) {
        bits.fetch_and(!(1 << (index % 64)), Ordering::AcqRel);
    }
}

/// Bound functions, and unbound ones whose slots are not yet released.
#[cfg(feature = "test-hooks")]
pub fn claims_held() -> u32 {
    CLAIM_INDICES
        .iter()
        .map(|b| b.load(Ordering::Acquire).count_ones())
        .sum()
}

struct Job(KArc<Device>, usize);

static JOBS: Mutex<KVecDeque<Job>> = Mutex::new(
    KVecDeque::new(),
    lock_class!("usb.JOBS", LOCK_LEVEL_RESOURCE),
);

fn queue(job: Job) -> Result<(), AllocError> {
    JOBS.lock().map_err(|_| AllocError)?.push_back(job)?;
    super::wake_binder();
    Ok(())
}

fn next_job() -> Option<Job> {
    JOBS.lock().ok()?.pop_front()
}

/// A boot step asks with no task to sleep, so a contended queue is busy.
pub(super) fn idle() -> bool {
    JOBS.try_lock().is_some_and(|jobs| jobs.is_empty()) && !super::binder_busy()
}

pub(super) fn offer(device: &KArc<Device>, node: &Node) {
    device.set_node(*node);
    let functions = device.stored(|b| {
        Configuration::parse(&b[STORE_CONFIGURATION..])
            .map(|c| c.functions())
            .unwrap_or_default()
    });
    let mut binds = KVec::new();
    for &function in functions.as_slice() {
        if binds
            .push(Bind {
                function,
                state: BindState::Pending,
            })
            .is_err()
        {
            break;
        }
    }
    let offered = binds.len();
    device.set_binds(binds);
    for index in 0..offered {
        if queue(Job(KArc::clone(device), index)).is_err() {
            device.set_bind(index, BindState::Unbound);
        }
    }
    if functions.dropped > 0 {
        klog_info!(
            "USB: {}-{} {} functions past the {} offered left unbound",
            device.controller,
            node.path,
            functions.dropped,
            offered
        );
    }
    if offered == 0 {
        log_bound(device);
    }
}

/// Runs after every bind queued ahead; asking allocates nothing, so it
/// cannot fail.
pub(super) fn unbind(device: &Device) {
    device.ask_unbind();
    super::wake_binder();
}

/// Every bind queued, then every unbind asked for.
pub(super) fn run_jobs() {
    while let Some(Job(device, index)) = next_job() {
        bind(&device, index);
        super::wake();
    }
    for controller in super::xhci::published() {
        for device in controller.devices().iter() {
            if device.take_unbind() {
                release_now(device);
                super::wake();
            }
        }
    }
}

fn snapshot(device: &Device, function: &Function) -> Option<UsbFunction> {
    let node = device.node()?;
    Some(UsbFunction {
        controller: device.controller,
        slot: device.slot,
        serial: device.serial,
        path: node.path,
        speed: node.speed,
        vendor: node.vendor,
        product: node.product,
        class: function.class,
        subclass: function.subclass,
        protocol: function.protocol,
        first_interface: function.first_interface,
        interfaces: function.interfaces,
        configuration: node.configuration,
    })
}

fn bind(device: &KArc<Device>, index: usize) {
    let Some(entry) = device.bind(index) else {
        return;
    };
    let snapshot = snapshot(device, &entry.function);
    let state = match (snapshot, device.is_gone()) {
        (Some(function), false) => probe(&function),
        _ => BindState::Skipped,
    };
    device.set_bind(index, state);
    if device.resolved() {
        log_bound(device);
    }
}

#[inline(never)]
fn probe(function: &UsbFunction) -> BindState {
    let Some(claim) = take_claim_index() else {
        klog_info!(
            "USB: {}-{} no room for another bound function",
            function.controller,
            function.path
        );
        return BindState::Unbound;
    };
    let mut drivers = KVec::new();
    for entry in driver_registry_iter() {
        if drivers.push(entry).is_err() {
            break;
        }
    }
    let index = LinearIndex::<UsbBus>::from_entries(drivers);
    match probe_one::<UsbBus>(&index, function, usize::from(claim)) {
        Ok(Probed::Bound(binding, devres)) => {
            let driver = binding.name();
            CLAIMS.lock().claim(usize::from(claim), binding, devres);
            BindState::Bound { driver, claim }
        }
        Ok(Probed::Unbound) | Err(_) => {
            give_back_claim_index(claim);
            BindState::Unbound
        }
    }
}

/// Runs each removal with no lock held; the claims' resources go when the
/// slot is released.
fn release_now(device: &Device) {
    let mut index = 0;
    while let Some(entry) = device.bind(index) {
        if let BindState::Bound { driver, claim } = entry.state {
            let mut slot = CLAIMS.lock().release(usize::from(claim));
            slot.remove();
            device.retire(slot);
            device.set_bind(index, BindState::Released { driver, claim });
        }
        index += 1;
    }
    device.set_unbound();
}

/// Disable Slot has completed: each binding drops before its resources.
pub(super) fn release_claims(device: &Device) {
    let retired = device.take_retired();
    drop(retired);
    let mut index = 0;
    while let Some(entry) = device.bind(index) {
        if let BindState::Released { claim, .. } = entry.state {
            give_back_claim_index(claim);
            device.set_bind(index, BindState::Skipped);
        }
        index += 1;
    }
}

struct Drivers {
    names: [&'static str; MAX_FUNCTIONS],
    len: usize,
}

impl Drivers {
    fn of(device: &Device) -> Self {
        let mut drivers = Self {
            names: [""; MAX_FUNCTIONS],
            len: 0,
        };
        device.binds(|_, bind| {
            if let BindState::Bound { driver, .. } = bind.state
                && drivers.len < MAX_FUNCTIONS
                && !drivers.names[..drivers.len].contains(&driver)
            {
                drivers.names[drivers.len] = driver;
                drivers.len += 1;
            }
        });
        drivers
    }
}

impl fmt::Display for Drivers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.len == 0 {
            return f.write_str("no driver");
        }
        for (i, name) in self.names[..self.len].iter().enumerate() {
            let separator = if i == 0 { "bound " } else { ", " };
            write!(f, "{}{}", separator, name)?;
        }
        Ok(())
    }
}

fn log_bound(device: &Device) {
    let Some(node) = device.node() else {
        return;
    };
    let mut count = 0;
    device.binds(|_, _| count += 1);
    let drivers = Drivers::of(device);
    klog_info!(
        "USB: {}-{} {:04x}:{:04x} {} function{}, {}",
        device.controller,
        node.path,
        node.vendor,
        node.product,
        count,
        if count == 1 { "" } else { "s" },
        drivers
    );
}

impl<'d> BoundDevice<'d, UsbBus> {
    fn device(&self) -> Result<KArc<Device>, BoundError> {
        super::xhci::device_of(self.info).ok_or(BoundError::Gone)
    }

    /// Once the device leaves, requests answer `Gone`.
    pub fn control(&mut self) -> Result<KArc<Control>, BoundError> {
        let control = Control::new(self.device()?);
        self.keep(control)
    }

    /// Opened once per binding.
    pub fn pipe(&mut self, address: u8) -> Result<KArc<Pipe>, BoundError> {
        self.owned_endpoint(address)?;
        let pipe = Pipe::open(self.device()?, address)?;
        self.keep(pipe)
    }

    /// Keeps a report of `length` bytes posted on the interrupt endpoint at
    /// `address`, handing each to `sink` from wherever the drain runs.
    pub fn reports(
        &mut self,
        address: u8,
        length: u32,
        sink: KArc<dyn ReportSink>,
    ) -> Result<KArc<Reports>, BoundError> {
        self.owned_endpoint(address)?;
        let reports = Reports::open(self.device()?, address, length, sink)?;
        self.keep(reports)
    }

    /// Control requests for a thread that may not wait on one.
    pub fn posted(&mut self) -> Result<KArc<Posted>, BoundError> {
        let posted = Posted::new(self.device()?)?;
        self.keep(posted)
    }

    fn owned_endpoint(&self, address: u8) -> Result<(), BoundError> {
        let info = *self.info;
        let device = self.device()?;
        let owned = device.stored(|b| {
            let config = Configuration::parse(&b[STORE_CONFIGURATION..]).ok()?;
            config
                .interfaces()
                .filter(|i| i.alternate == 0 && info.covers(i.number))
                .flat_map(|i| config.endpoints(i.number, 0))
                .find(|e| e.address == address)
        });
        owned.map(|_| ()).ok_or(BoundError::NoSuchEndpoint)
    }

    /// The binding's resources keep a handle too.
    fn keep<T: Send + Sync + 'static>(&mut self, value: T) -> Result<KArc<T>, BoundError> {
        let handle = KArc::try_new(value).map_err(|_| BoundError::OutOfMemory)?;
        self.attach(KArc::clone(&handle))?;
        Ok(handle)
    }

    pub fn descriptors<R>(&self, read: impl FnOnce(&Configuration<'_>) -> R) -> Option<R> {
        let device = self.device().ok()?;
        device.stored(|b| {
            Configuration::parse(&b[STORE_CONFIGURATION..])
                .ok()
                .map(|c| read(&c))
        })
    }

    /// Runs when the device leaves, before the resources are released.
    pub fn on_remove(&mut self, removal: impl Removal + 'static) -> Result<(), BoundError> {
        let boxed = slopos_ostd::KBox::try_new(removal).map_err(|_| BoundError::OutOfMemory)?;
        self.removal = Some(boxed);
        Ok(())
    }
}
