//! `usb-net`: CDC Ethernet adapters, ECM and NCM, as NICs. Netpoll takes what
//! the bulk-IN transfers bring and posts them again, `tx` frames each packet
//! into a free buffer and returns, the drain only wakes netpoll, and the tree
//! recovers a halted pipe. Carrier is what the control interface's last
//! notification said.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_net::iface::{self, IfName};
use slopos_net::netdev::{NetDevice, NetDeviceFeatures, NetDeviceStats};
use slopos_net::packetbuf::PacketBuf;
use slopos_net::pool::PacketPool;
use slopos_net::types::{DevIndex, MacAddr, NetError};
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KArc, KVec, klog_info, lock_class, write_field};
use slopos_usb_core::bus::Path;
use slopos_usb_core::cdc::{self, Model, Notification, capability, filter, ntb};
use slopos_usb_core::device::descriptor::kind;
use slopos_usb_core::device::request::Setup;
use slopos_usb_core::device::string;
use slopos_usb_core::xhci::memory::PAGE_SIZE;
use slopos_usb_core::xhci::transfer::{Transfer, TransferError, TransferResult};

use super::bus::{BoundUsbDevice, UsbFunction, UsbMatch};
use super::xhci::device::{Control, Queue, ReportSink, TransferSink};
use crate::driver_core::bus::{ProbeError, ProbeOutcome, Removal};

const MTU: u16 = 1500;
const RX_BUFFERS: usize = 4;
/// A frame's TD and its zero-length packet fill a ring's eight transfer
/// slots between them.
const TX_BUFFERS: usize = 4;

#[derive(Clone, Copy)]
enum Framing {
    Ecm,
    Ncm(ntb::Out),
}

/// What the drain and the notifications leave for netpoll and the stack.
struct Signals {
    pending: AtomicBool,
    carrier: AtomicBool,
}

impl TransferSink for Signals {
    fn completed(&self) {
        self.pending.store(true, Ordering::Release);
        slopos_net::napi::wake_napi();
    }
}

impl ReportSink for Signals {
    fn report(&self, report: &[u8]) {
        if let Some(Notification::Connection(up)) = cdc::notification(report) {
            self.carrier.store(up, Ordering::Release);
        }
    }
}

/// Transfers posted, oldest first, which is the order they complete in.
struct Posted<const N: usize> {
    order: [Option<(usize, Transfer)>; N],
    head: usize,
    count: usize,
}

impl<const N: usize> Posted<N> {
    const EMPTY: Self = Self {
        order: [None; N],
        head: 0,
        count: 0,
    };

    fn oldest(&self) -> Option<(usize, Transfer)> {
        if self.count == 0 {
            return None;
        }
        self.order[self.head]
    }

    fn pop(&mut self) {
        self.order[self.head] = None;
        self.head = (self.head + 1) % N;
        self.count -= 1;
    }

    fn push(&mut self, buffer: usize, transfer: Transfer) {
        self.order[(self.head + self.count) % N] = Some((buffer, transfer));
        self.count += 1;
    }

    fn holds(&self, buffer: usize) -> bool {
        self.order.iter().flatten().any(|&(b, _)| b == buffer)
    }
}

struct Rx {
    queue: Option<KArc<Queue>>,
    posted: Posted<RX_BUFFERS>,
    /// One buffer's bytes, read out of the device's pages.
    scratch: KVec<u8>,
}

/// A buffer's frame TD and the zero-length packet ending it.
#[derive(Clone, Copy, Default)]
struct Sent {
    frame: Option<Transfer>,
    end: Option<Transfer>,
}

impl Sent {
    fn busy(&self) -> bool {
        self.frame.is_some() || self.end.is_some()
    }
}

struct Tx {
    queue: Option<KArc<Queue>>,
    sent: [Sent; TX_BUFFERS],
    sequence: u16,
    ntb: KVec<u8>,
}

#[derive(Default)]
struct Counters {
    rx_packets: AtomicU64,
    tx_packets: AtomicU64,
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
    rx_errors: AtomicU64,
    tx_errors: AtomicU64,
    rx_dropped: AtomicU64,
    tx_dropped: AtomicU64,
}

fn bump(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Ordering::Relaxed);
}

/// One bound adapter.
#[derive(slopos_ostd::SlotFields)]
pub struct UsbNet {
    controller: u8,
    path: Path,
    mac: MacAddr,
    framing: Framing,
    carrier_detect: bool,
    signals: KArc<Signals>,
    up: AtomicBool,
    rx: SpinLock<Rx>,
    tx: SpinLock<Tx>,
    published: SpinLock<Option<(DevIndex, IfName)>>,
    counters: Counters,
}

impl UsbNet {
    /// Posts every free buffer, unless the link is down or the ring takes
    /// nothing now, which its recovery ends.
    fn post_rx(&self) {
        let mut rx = self.rx.lock();
        if !self.up.load(Ordering::Acquire) {
            return;
        }
        let Some(queue) = rx.queue.clone() else {
            return;
        };
        let capacity = queue.buffer_bytes() as u32;
        for buffer in 0..RX_BUFFERS {
            if rx.posted.holds(buffer) {
                continue;
            }
            match queue.push(buffer, capacity) {
                Ok(transfer) => rx.posted.push(buffer, transfer),
                Err(_) => return,
            }
        }
    }

    /// Takes each completed bulk-IN transfer, oldest first, until `budget`
    /// packets are out.
    fn receive(&self, budget: usize, pool: &'static PacketPool, packets: &mut KVec<PacketBuf>) {
        let mut rx = self.rx.lock();
        let Some(queue) = rx.queue.clone() else {
            return;
        };
        while packets.len() < budget {
            let Some((buffer, transfer)) = rx.posted.oldest() else {
                break;
            };
            let Some(result) = queue.result(transfer) else {
                break;
            };
            rx.posted.pop();
            let Rx { scratch, .. } = &mut *rx;
            self.received(&queue, buffer, result, scratch, pool, packets);
        }
    }

    fn received(
        &self,
        queue: &Queue,
        buffer: usize,
        result: TransferResult,
        scratch: &mut [u8],
        pool: &'static PacketPool,
        packets: &mut KVec<PacketBuf>,
    ) {
        let length = match result {
            Ok(0) | Err(TransferError::Cancelled) => return,
            Ok(length) => (length as usize).min(scratch.len()),
            Err(error) if error.is_final() => return,
            Err(_) => return bump(&self.counters.rx_errors, 1),
        };
        let bytes = &mut scratch[..length];
        queue.read(buffer, 0, bytes);
        match self.framing {
            Framing::Ecm => self.deliver(bytes, pool, packets),
            Framing::Ncm(_) => {
                let mut found = false;
                for datagram in ntb::datagrams(bytes) {
                    found = true;
                    self.deliver(datagram, pool, packets);
                }
                if !found {
                    bump(&self.counters.rx_errors, 1);
                }
            }
        }
    }

    fn deliver(&self, frame: &[u8], pool: &'static PacketPool, packets: &mut KVec<PacketBuf>) {
        if frame.len() < cdc::ETHERNET_HEADER {
            return bump(&self.counters.rx_errors, 1);
        }
        match PacketBuf::from_raw_copy_in(pool, frame).map(|p| packets.push(p)) {
            Some(Ok(())) => {
                bump(&self.counters.rx_packets, 1);
                bump(&self.counters.rx_bytes, frame.len() as u64);
            }
            _ => bump(&self.counters.rx_dropped, 1),
        }
    }

    fn reclaim(&self, tx: &mut Tx, queue: &Queue) {
        for sent in tx.sent.iter_mut() {
            for transfer in [&mut sent.frame, &mut sent.end] {
                let Some(result) = transfer.and_then(|t| queue.result(t)) else {
                    continue;
                };
                *transfer = None;
                if result.is_err() {
                    bump(&self.counters.tx_errors, 1);
                }
            }
        }
    }

    fn reclaim_tx(&self) {
        let mut tx = self.tx.lock();
        if let Some(queue) = tx.queue.clone() {
            self.reclaim(&mut tx, &queue);
        }
    }

    /// Writes the frame into buffer `buffer` as the function frames it, and
    /// says how many bytes to send.
    fn frame_into(&self, tx: &mut Tx, queue: &Queue, buffer: usize, frame: &[u8]) -> Option<usize> {
        match self.framing {
            Framing::Ecm => {
                queue.write(buffer, 0, frame);
                Some(frame.len())
            }
            Framing::Ncm(out) => {
                let length = ntb::write(frame, tx.sequence, &out, &mut tx.ntb)?;
                tx.sequence = tx.sequence.wrapping_add(1);
                queue.write(buffer, 0, &tx.ntb[..length]);
                Some(length)
            }
        }
    }

    fn send(&self, frame: &[u8]) -> Result<(), NetError> {
        let mut tx = self.tx.lock();
        if !self.up.load(Ordering::Acquire) || !self.carrier() {
            return Err(NetError::NoBufferSpace);
        }
        let queue = tx.queue.clone().ok_or(NetError::NoBufferSpace)?;
        self.reclaim(&mut tx, &queue);
        let buffer = tx
            .sent
            .iter()
            .position(|s| !s.busy())
            .ok_or(NetError::NoBufferSpace)?;
        if frame.len() > cdc::MAX_FRAME {
            bump(&self.counters.tx_errors, 1);
            return Err(NetError::NoBufferSpace);
        }
        let length = self
            .frame_into(&mut tx, &queue, buffer, frame)
            .ok_or(NetError::NoBufferSpace)?;
        let limit = match self.framing {
            Framing::Ecm => usize::MAX,
            Framing::Ncm(out) => out.max as usize,
        };
        let sent = &mut tx.sent[buffer];
        sent.frame = Some(
            queue
                .push(buffer, length as u32)
                .map_err(|_| NetError::NoBufferSpace)?,
        );
        if cdc::needs_zlp(length, queue.max_packet(), limit) {
            sent.end = queue.push(buffer, 0).ok();
        }
        Ok(())
    }

    /// Netpoll takes whatever completed while the link was down.
    fn start(&self) {
        self.up.store(true, Ordering::Release);
        self.post_rx();
        self.signals.completed();
    }

    /// Lets the device go: nothing of it is touched again, and its buffers
    /// stay the device's until it is freed.
    fn release(&self) {
        self.up.store(false, Ordering::Release);
        let rx = self.rx.lock().queue.take();
        let tx = self.tx.lock().queue.take();
        drop(rx);
        drop(tx);
    }

    fn retire(&self) {
        let published = self.published.lock().take();
        if let Some((dev, name)) = published {
            slopos_net::nic::retire(dev);
            klog_info!("USB: {}-{} {} removed", self.controller, self.path, name);
        }
        self.release();
    }
}

impl NetDevice for UsbNet {
    fn tx(&self, pkt: PacketBuf) -> Result<(), NetError> {
        let frame = pkt.payload();
        match self.send(frame) {
            Ok(()) => {
                bump(&self.counters.tx_packets, 1);
                bump(&self.counters.tx_bytes, frame.len() as u64);
                Ok(())
            }
            Err(error) => {
                bump(&self.counters.tx_dropped, 1);
                Err(error)
            }
        }
    }

    fn poll_tx(&self) {
        self.reclaim_tx();
    }

    fn poll_rx(&self, budget: usize, pool: &'static PacketPool) -> KVec<PacketBuf> {
        self.signals.pending.store(false, Ordering::Release);
        let mut packets = KVec::new();
        if !self.up.load(Ordering::Acquire) {
            return packets;
        }
        self.reclaim_tx();
        self.receive(budget, pool, &mut packets);
        self.post_rx();
        packets
    }

    fn set_up(&self) {
        self.start();
    }

    fn set_down(&self) {
        let _tx = self.tx.lock();
        let _rx = self.rx.lock();
        self.up.store(false, Ordering::Release);
    }

    fn mtu(&self) -> u16 {
        MTU
    }

    fn mac(&self) -> MacAddr {
        self.mac
    }

    fn stats(&self) -> NetDeviceStats {
        let c = &self.counters;
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        NetDeviceStats {
            rx_packets: load(&c.rx_packets),
            tx_packets: load(&c.tx_packets),
            rx_bytes: load(&c.rx_bytes),
            tx_bytes: load(&c.tx_bytes),
            rx_errors: load(&c.rx_errors),
            tx_errors: load(&c.tx_errors),
            rx_dropped: load(&c.rx_dropped),
            tx_dropped: load(&c.tx_dropped),
        }
    }

    fn features(&self) -> NetDeviceFeatures {
        NetDeviceFeatures::empty()
    }

    fn carrier(&self) -> bool {
        self.signals.carrier.load(Ordering::Acquire)
    }

    fn carrier_detect(&self) -> bool {
        self.carrier_detect
    }

    fn rx_pending(&self) -> bool {
        self.signals.pending.load(Ordering::Acquire)
    }
}

struct Unplugged(KArc<UsbNet>);

impl Removal for Unplugged {
    fn remove(&self) {
        self.0.retire();
    }
}

fn declined(info: &UsbFunction, why: impl core::fmt::Display) -> Result<ProbeOutcome, ProbeError> {
    klog_info!(
        "USB: {}-{} network function declined: {}",
        info.controller,
        info.path,
        why
    );
    Ok(ProbeOutcome::Declined)
}

/// The address the function's iMACAddress string names.
#[inline(never)]
fn mac_of(control: &Control, index: u8) -> Option<MacAddr> {
    let mut bytes = [0u8; string::MAX_LEN as usize];
    let read = control
        .read(
            Setup::get_descriptor(kind::STRING, 0, 0, string::MAX_LEN),
            &mut bytes,
        )
        .ok()?;
    let language = string::first_language(&bytes[..read]).unwrap_or(string::ENGLISH);
    let read = control
        .read(
            Setup::get_descriptor(kind::STRING, index, language, string::MAX_LEN),
            &mut bytes,
        )
        .ok()?;
    cdc::mac_address(&bytes[..read]).map(MacAddr)
}

/// GET_NTB_PARAMETERS, and the device held to NTB-16 IN blocks of the size
/// the host chose: how to lay out OUT blocks, and that size.
fn ncm(control: &Control, function: &cdc::Function) -> Result<(ntb::Out, u32), &'static str> {
    let interface = function.control;
    let mut answer = [0u8; ntb::Parameters::LEN];
    let read = control
        .read(Setup::get_ntb_parameters(interface), &mut answer)
        .map_err(|_| "no NTB parameters")?;
    let params = ntb::Parameters::parse(&answer[..read]).ok_or("NTB parameters unusable")?;
    let size = params.in_size().ok_or("NTBs too small")?;
    if params.ntb32()
        && control
            .write(Setup::set_ntb_format_16(interface), &[])
            .is_err()
    {
        return Err("NTB-16 refused");
    }
    let input = Setup::set_ntb_input_size(interface, function.capabilities);
    let data = cdc::ntb_input_size(size);
    if size != params.in_max
        && control
            .write(input, &data[..usize::from(input.length)])
            .is_err()
    {
        return Err("NTB input size refused");
    }
    if function.capabilities & capability::CRC_MODE != 0 {
        let _ = control.write(Setup::set_crc_mode_off(interface), &[]);
    }
    Ok((params.out, size))
}

fn probe(bound: &mut BoundUsbDevice<'_>) -> Result<ProbeOutcome, ProbeError> {
    let info = *bound.info();
    let function = match bound.descriptors(|c| cdc::Function::parse(c, info.first_interface)) {
        Some(Ok(function)) => function,
        Some(Err(why)) => return declined(&info, why),
        None => return Ok(ProbeOutcome::Declined),
    };
    let control = bound.control().map_err(|_| ProbeError::DeviceFault)?;
    let Some(mac) = mac_of(&control, function.mac_string) else {
        return declined(&info, "no usable MAC address");
    };
    let (framing, rx_bytes) = match function.model {
        Model::Ecm => (Framing::Ecm, PAGE_SIZE),
        Model::Ncm => match ncm(&control, &function) {
            Ok((out, size)) => (Framing::Ncm(out), size as usize),
            Err(why) => return declined(&info, why),
        },
    };
    if function.alternate != 0 && bound.select(function.data, function.alternate).is_err() {
        return declined(&info, "its data interface's alternate setting was refused");
    }
    if function.model == Model::Ecm || function.capabilities & capability::PACKET_FILTER != 0 {
        let filter = Setup::set_ethernet_packet_filter(
            function.control,
            filter::DIRECTED | filter::BROADCAST,
        );
        let _ = control.write(filter, &[]);
    }
    let net = bind(bound, &info, &function, mac, framing, rx_bytes)?;
    if bound.on_remove(Unplugged(KArc::clone(&net))).is_err() {
        net.release();
        return Err(ProbeError::OutOfMemory);
    }
    net.start();
    let device: KArc<dyn NetDevice + Send + Sync> = KArc::clone(&net) as _;
    let Some(dev) = slopos_net::nic::publish(device) else {
        net.release();
        return declined(&info, "the network stack has no room");
    };
    let name = iface::get_by_dev(dev).map(|i| i.name);
    if let Some(name) = name {
        *net.published.lock() = Some((dev, name));
        klog_info!(
            "USB: {}-{} is {}, {}, {}",
            info.controller,
            info.path,
            name,
            mac,
            if function.model == Model::Ecm {
                "ECM"
            } else {
                "NCM"
            }
        );
    } else {
        slopos_net::nic::retire(dev);
        net.release();
        return declined(&info, "it has no interface");
    }
    slopos_net::napi::wake_napi();
    Ok(ProbeOutcome::Bound)
}

fn oom<E>(_: E) -> ProbeError {
    ProbeError::OutOfMemory
}

fn fault<E>(_: E) -> ProbeError {
    ProbeError::DeviceFault
}

/// The pipes, their buffers and the notifications.
#[inline(never)]
fn bind(
    bound: &mut BoundUsbDevice<'_>,
    info: &UsbFunction,
    function: &cdc::Function,
    mac: MacAddr,
    framing: Framing,
    rx_bytes: usize,
) -> Result<KArc<UsbNet>, ProbeError> {
    let rx = bound
        .queue(
            function.bulk_in.address,
            RX_BUFFERS,
            rx_bytes.div_ceil(PAGE_SIZE),
        )
        .map_err(fault)?;
    let tx = bound
        .queue(function.bulk_out.address, TX_BUFFERS, 1)
        .map_err(fault)?;
    let signals = KArc::try_new(Signals {
        pending: AtomicBool::new(false),
        carrier: AtomicBool::new(function.notify.is_none()),
    })
    .map_err(oom)?;
    rx.attach(KArc::clone(&signals) as _).map_err(oom)?;
    tx.attach(KArc::clone(&signals) as _).map_err(oom)?;
    if let Some(notify) = function.notify {
        let length = u32::from(notify.max_packet_size().max(8));
        bound
            .reports(notify.address, length, KArc::clone(&signals) as _)
            .map_err(fault)?;
    }
    let mut rx_scratch = KVec::new();
    rx_scratch.resize(rx.buffer_bytes(), 0u8).map_err(oom)?;
    let mut tx_ntb = KVec::new();
    tx_ntb.resize(tx.buffer_bytes(), 0u8).map_err(oom)?;
    let (controller, path) = (info.controller, info.path);
    let carrier_detect = function.notify.is_some();
    KArc::try_init(init_struct_with(
        move |slot: SlotPtr<UsbNet>| -> Result<Initialised<UsbNet>, AllocError> {
            write_field!(slot, controller, controller);
            write_field!(slot, path, path);
            write_field!(slot, mac, mac);
            write_field!(slot, framing, framing);
            write_field!(slot, carrier_detect, carrier_detect);
            write_field!(slot, signals, signals);
            write_field!(slot, up, AtomicBool::new(false));
            write_field!(
                slot,
                rx,
                SpinLock::new(
                    Rx {
                        queue: Some(rx),
                        posted: Posted::EMPTY,
                        scratch: rx_scratch,
                    },
                    lock_class!("usb-net.rx", LOCK_LEVEL_RESOURCE)
                )
            );
            write_field!(
                slot,
                tx,
                SpinLock::new(
                    Tx {
                        queue: Some(tx),
                        sent: [Sent::default(); TX_BUFFERS],
                        sequence: 0,
                        ntb: tx_ntb,
                    },
                    lock_class!("usb-net.tx", LOCK_LEVEL_RESOURCE)
                )
            );
            write_field!(
                slot,
                published,
                SpinLock::new(None, lock_class!("usb-net.published", LOCK_LEVEL_RESOURCE))
            );
            write_field!(slot, counters, Counters::default());
            Ok(slot.finish())
        },
    ))
    .map_err(oom)
}

crate::usb_driver! {
    pub static USB_NET = {
        name: "usb-net",
        match_table: &[
            UsbMatch::Class {
                class: cdc::CLASS,
                subclass: Some(cdc::SUBCLASS_ECM),
                protocol: None,
            },
            UsbMatch::Class {
                class: cdc::CLASS,
                subclass: Some(cdc::SUBCLASS_NCM),
                protocol: None,
            },
        ],
        probe: probe,
    };
}
