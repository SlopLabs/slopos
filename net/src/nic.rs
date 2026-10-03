//! Bringing NICs into service, and the two kernel I/O threads every published
//! NIC shares: netpoll drains RX into ingress, net-timer runs the protocol
//! timers and samples carrier.

use slopos_ostd::sync::kernel_io_task::{KernelIoToken, KthreadWait, yield_now};
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KArc, klog_info, klog_warn, lock_class};

use crate::iface;
use crate::napi::NapiContext;
use crate::napi_waker::NapiWaker;
use crate::netdev::{DEVICE_REGISTRY, DeviceHandle, MAX_DEVICES, NetDevice};
use crate::pool::PACKET_POOL;
use crate::types::DevIndex;

const NAPI_BUDGET: u32 = 64;
const LOOPBACK_POLL_BUDGET: usize = 32;
const NET_TIMER_PERIOD_MS: u32 = 50;

/// Armed by every NIC's IRQ handler through [`crate::napi::wake_napi`].
pub(crate) static NAPI_WAKER: NapiWaker = NapiWaker::new(
    "netpoll",
    lock_class!("NETPOLL_WAKER.waiters", LOCK_LEVEL_RESOURCE),
);
static TIMER_WAKER: NapiWaker = NapiWaker::new(
    "net-timer",
    lock_class!("NET_TIMER_WAKER.waiters", LOCK_LEVEL_RESOURCE),
);
static NAPI_CONTEXT: NapiContext = NapiContext::new(NAPI_BUDGET);

type NicTable = [Option<KArc<DeviceHandle>>; MAX_DEVICES];

/// Published NICs. Only snapshotted or edited under the lock; no device is
/// called while it is held.
static NICS: SpinLock<NicTable> = SpinLock::new(
    [const { None }; MAX_DEVICES],
    lock_class!("NIC_TABLE", LOCK_LEVEL_REGISTRY),
);

/// Start the stack's netpoll and net-timer kthreads. Boot calls it once, after
/// loopback and before PCI probe.
pub fn init() {
    PACKET_POOL.init();
    if let Err(err) = slopos_ostd::spawn_kernel_io!(NAPI_WAKER.stop(), netpoll_entry) {
        klog_warn!("net: no netpoll thread, nothing will receive ({:?})", err);
    }
    if let Err(err) = slopos_ostd::spawn_kernel_io!(TIMER_WAKER.stop(), net_timer_entry) {
        klog_warn!(
            "net: no net-timer thread, no protocol timer will fire ({:?})",
            err
        );
    }
}

/// Bring a NIC into service: device registry slot, interface, a place in the
/// netpoll loop and the net-timer's carrier sampling, and a DHCP client.
/// Call with no driver lock held. `None` when the registry or the NIC table is
/// full, or an allocation failed.
pub fn publish(dev: KArc<dyn NetDevice + Send + Sync>) -> Option<DevIndex> {
    let kind = dev.kind();
    let mac = dev.mac();
    let mtu = dev.mtu();
    let carrier = dev.carrier();
    let carrier_detect = dev.carrier_detect();

    let Some(handle) = DEVICE_REGISTRY.register(dev) else {
        klog_info!("nic: device registry full, {} not published", mac);
        return None;
    };
    let index = handle.index();
    let Ok(handle) = KArc::try_new(handle) else {
        DEVICE_REGISTRY.unregister(index);
        return None;
    };

    match iface::attach(index, kind, mac, mtu, carrier, carrier_detect) {
        Ok(ifindex) => klog_info!("nic: dev {} ({}) is interface {}", index, mac, ifindex),
        Err(err) => klog_info!("nic: dev {} has no interface: {:?}", index, err),
    }

    if !insert(handle) {
        klog_info!("nic: NIC table full, dev {} not published", index);
        iface::detach(index);
        DEVICE_REGISTRY.unregister(index);
        return None;
    }

    if !crate::dhcp::start(index) {
        klog_info!("nic: dev {} could not start a DHCP client", index);
    }
    Some(index)
}

fn insert(handle: KArc<DeviceHandle>) -> bool {
    let mut nics = NICS.lock();
    match nics.iter_mut().find(|slot| slot.is_none()) {
        Some(slot) => {
            *slot = Some(handle);
            true
        }
        None => false,
    }
}

fn snapshot(out: &mut NicTable) {
    let nics = NICS.lock();
    for (slot, nic) in out.iter_mut().zip(nics.iter()) {
        *slot = nic.clone();
    }
}

/// Take `dev` out of service: the inverse of [`publish`]. Production never
/// retires a NIC; tests do, to leave the stack as they found it.
#[cfg(feature = "test-hooks")]
pub fn retire(dev: DevIndex) -> bool {
    let removed = {
        let mut nics = NICS.lock();
        nics.iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|nic| nic.index() == dev))
            .and_then(Option::take)
    };
    if removed.is_none() {
        return false;
    }
    drop(removed);

    crate::dhcp::stop(dev);
    crate::route::remove_device_routes(dev);
    drop(crate::neighbor::NEIGHBOR_CACHE.flush_device(dev));
    iface::detach(dev);
    DEVICE_REGISTRY.unregister(dev);
    true
}

/// Whether `dev` is in the netpoll loop and the carrier sampling.
#[cfg(feature = "test-hooks")]
pub fn is_published(dev: DevIndex) -> bool {
    NICS.lock().iter().flatten().any(|nic| nic.index() == dev)
}

/// The data-plane handle of published NIC `dev`.
#[cfg(feature = "test-hooks")]
pub fn handle(dev: DevIndex) -> Option<KArc<DeviceHandle>> {
    NICS.lock()
        .iter()
        .flatten()
        .find(|nic| nic.index() == dev)
        .cloned()
}

/// Run one netpoll burst on the calling thread, for test fixtures that wait on
/// the live network from a boot step.
#[cfg(feature = "test-hooks")]
pub fn force_napi_poll() {
    NAPI_WAKER.arm_and_wake();
    let _ = run_burst();
    crate::socket::socket_process_timers();
}

struct Burst {
    exhausted: bool,
    rx_pending: bool,
}

fn run_burst() -> Burst {
    let software = crate::napi::poll_software_devices(LOOPBACK_POLL_BUDGET);
    let mut burst = Burst {
        exhausted: software as usize >= LOOPBACK_POLL_BUDGET,
        rx_pending: false,
    };

    let mut nics: NicTable = [const { None }; MAX_DEVICES];
    snapshot(&mut nics);
    let budget = NAPI_CONTEXT.budget();
    for nic in nics.iter().flatten() {
        let packets = nic.poll_rx(budget as usize, &PACKET_POOL);
        let processed = packets.len() as u32;
        for pkt in packets {
            crate::ingress::net_rx(nic, pkt);
        }
        NAPI_CONTEXT.add_processed(processed);
        burst.exhausted |= processed >= budget;
        burst.rx_pending |= nic.rx_pending();
    }
    burst
}

fn netpoll_entry(token: KernelIoToken<'static>) {
    loop {
        if NAPI_WAKER.wait(&token) == KthreadWait::Stop {
            // Packets an IRQ already committed would otherwise never be collected.
            let _ = run_burst();
            break;
        }
        let burst = run_burst();
        crate::socket::socket_process_timers();

        if burst.rx_pending {
            NAPI_WAKER.rearm();
        }
        if burst.exhausted {
            NAPI_WAKER.rearm();
            yield_now(&token);
        }
    }
    NAPI_WAKER.stop().note_exited();
}

fn net_timer_entry(token: KernelIoToken<'static>) {
    loop {
        if TIMER_WAKER.wait_timeout_ms(&token, NET_TIMER_PERIOD_MS) == KthreadWait::Stop {
            break;
        }
        crate::timer::net_timer_process();
        crate::socket::socket_process_timers();
        sample_carriers();
        yield_now(&token);
    }
    TIMER_WAKER.stop().note_exited();
}

fn sample_carriers() {
    let mut nics: NicTable = [const { None }; MAX_DEVICES];
    snapshot(&mut nics);
    for nic in nics.iter().flatten() {
        nic.sample_carrier();
        let _ = iface::set_carrier(nic.index(), nic.carrier());
    }
}
