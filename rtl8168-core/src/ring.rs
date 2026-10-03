//! Receive and transmit ring bookkeeping over descriptor memory the device
//! reads and writes. One buffer per descriptor, fixed for the ring's life.

use crate::desc::{
    Descriptor, END_OF_RING, FIRST_FRAGMENT, LAST_FRAGMENT, LENGTH_MASK, OWN, RxError, RxStatus,
    rx_status,
};

/// A ring's descriptors, in memory the device reaches by DMA.
pub trait DescriptorMemory {
    fn slots(&self) -> usize;
    /// Read a descriptor, `opts1` first. When that `opts1` shows the driver
    /// owns the descriptor, the other fields and the buffer contents read
    /// after it are the ones the device wrote before handing it back.
    fn read(&self, index: usize) -> Descriptor;
    /// Write a descriptor, `opts1` last: the device never sees the new
    /// `opts1` beside an older `opts2`, `addr` or buffer content.
    fn write(&mut self, index: usize, desc: Descriptor);
}

/// The shortest frame Ethernet carries, FCS excluded; the driver pads
/// anything shorter.
pub const MIN_FRAME_LEN: usize = 60;

fn end_of_ring(index: usize, slots: usize) -> u32 {
    if index + 1 == slots { END_OF_RING } else { 0 }
}

fn valid_buffer_len(len: usize) -> bool {
    len > 0 && len <= LENGTH_MASK as usize
}

pub enum Received<R> {
    Frame(R),
    Dropped(RxError),
}

pub struct RxRing<M> {
    mem: M,
    next: usize,
    buffer_len: usize,
}

impl<M: DescriptorMemory> RxRing<M> {
    /// Give every descriptor to the device with slot `i`'s buffer at
    /// `buffer(i)`. `None` for an empty ring or a buffer length a
    /// descriptor cannot state.
    pub fn new(mem: M, buffer_len: usize, buffer: impl Fn(usize) -> u64) -> Option<Self> {
        if mem.slots() == 0 || !valid_buffer_len(buffer_len) {
            return None;
        }
        let mut ring = Self {
            mem,
            next: 0,
            buffer_len,
        };
        for i in 0..ring.mem.slots() {
            ring.arm(i, buffer(i));
        }
        Some(ring)
    }

    pub fn memory(&self) -> &M {
        &self.mem
    }

    /// The buffer length every descriptor names.
    pub fn buffer_len(&self) -> u16 {
        self.buffer_len as u16
    }

    fn arm(&mut self, index: usize, addr: u64) {
        let opts1 = OWN | end_of_ring(index, self.mem.slots()) | self.buffer_len as u32;
        self.mem.write(
            index,
            Descriptor {
                opts1,
                opts2: 0,
                addr,
            },
        );
    }

    /// The device has handed back the next descriptor.
    pub fn pending(&self) -> bool {
        self.mem.read(self.next).opts1 & OWN == 0
    }

    /// Take the next descriptor the device has handed back. A whole frame
    /// goes to `deliver` as `(slot, length)` while the driver still owns
    /// the slot's buffer; either way the descriptor goes back to the
    /// device before this returns.
    pub fn receive<R>(&mut self, deliver: impl FnOnce(usize, usize) -> R) -> Option<Received<R>> {
        let index = self.next;
        let desc = self.mem.read(index);
        if desc.opts1 & OWN != 0 {
            return None;
        }
        let received = match rx_status(desc.opts1, self.buffer_len) {
            RxStatus::Frame(len) => Received::Frame(deliver(index, len)),
            RxStatus::Dropped(error) => Received::Dropped(error),
        };
        self.arm(index, desc.addr);
        self.next = (index + 1) % self.mem.slots();
        Some(received)
    }

    /// Give every descriptor back to the device and start again from the
    /// first, as a chip reset requires.
    pub fn rearm(&mut self) {
        for i in 0..self.mem.slots() {
            let addr = self.mem.read(i).addr;
            self.arm(i, addr);
        }
        self.next = 0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxError {
    Full,
    /// Empty, or longer than a slot's buffer.
    Length,
}

pub struct TxRing<M> {
    mem: M,
    head: usize,
    tail: usize,
    in_flight: usize,
    buffer_len: usize,
}

impl<M: DescriptorMemory> TxRing<M> {
    /// Set up every descriptor with slot `i`'s buffer at `buffer(i)`, all
    /// owned by the driver. `None` for an empty ring or a buffer length a
    /// descriptor cannot state.
    pub fn new(mem: M, buffer_len: usize, buffer: impl Fn(usize) -> u64) -> Option<Self> {
        if mem.slots() == 0 || !valid_buffer_len(buffer_len) {
            return None;
        }
        let mut ring = Self {
            mem,
            head: 0,
            tail: 0,
            in_flight: 0,
            buffer_len,
        };
        for i in 0..ring.mem.slots() {
            ring.idle(i, buffer(i));
        }
        Some(ring)
    }

    pub fn memory(&self) -> &M {
        &self.mem
    }

    fn idle(&mut self, index: usize, addr: u64) {
        self.mem.write(
            index,
            Descriptor {
                opts1: end_of_ring(index, self.mem.slots()),
                opts2: 0,
                addr,
            },
        );
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    pub fn free(&self) -> usize {
        self.mem.slots() - self.in_flight
    }

    /// Queue a `len`-byte frame: `fill` writes it into the buffer of the
    /// slot it is given, then the descriptor goes to the device. The caller
    /// rings TxPoll afterwards.
    pub fn send(&mut self, len: usize, fill: impl FnOnce(usize)) -> Result<usize, TxError> {
        if len == 0 || len > self.buffer_len {
            return Err(TxError::Length);
        }
        if self.in_flight == self.mem.slots() {
            return Err(TxError::Full);
        }
        let index = self.head;
        let addr = self.mem.read(index).addr;
        fill(index);
        let opts1 = OWN
            | FIRST_FRAGMENT
            | LAST_FRAGMENT
            | end_of_ring(index, self.mem.slots())
            | len as u32;
        self.mem.write(
            index,
            Descriptor {
                opts1,
                opts2: 0,
                addr,
            },
        );
        self.head = (index + 1) % self.mem.slots();
        self.in_flight += 1;
        Ok(index)
    }

    /// Take back every frame the device has finished with, oldest first,
    /// stopping at the first it still owns. Returns how many.
    pub fn reclaim(&mut self) -> usize {
        let mut done = 0;
        while self.in_flight > 0 && self.mem.read(self.tail).opts1 & OWN == 0 {
            self.tail = (self.tail + 1) % self.mem.slots();
            self.in_flight -= 1;
            done += 1;
        }
        done
    }

    /// Drop whatever is queued and take every descriptor back, as a chip
    /// reset requires. Returns how many frames were dropped.
    pub fn clear(&mut self) -> usize {
        let dropped = self.in_flight;
        for i in 0..self.mem.slots() {
            let addr = self.mem.read(i).addr;
            self.idle(i, addr);
        }
        self.head = 0;
        self.tail = 0;
        self.in_flight = 0;
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::{RX_CRC, RX_ERROR_SUMMARY};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::vec;
    use std::vec::Vec;

    #[derive(Clone)]
    struct Shared(Rc<RefCell<Vec<Descriptor>>>);

    impl Shared {
        fn new(slots: usize) -> Self {
            Self(Rc::new(RefCell::new(vec![Descriptor::default(); slots])))
        }

        fn get(&self, i: usize) -> Descriptor {
            self.0.borrow()[i]
        }

        fn device_writes_back(&self, i: usize, opts1: u32) {
            let mut d = self.0.borrow_mut();
            assert_ne!(
                d[i].opts1 & OWN,
                0,
                "device wrote back slot {i} it did not own"
            );
            d[i].opts1 = opts1;
            d[i].opts2 = 0xdead_beef;
        }
    }

    impl DescriptorMemory for Shared {
        fn slots(&self) -> usize {
            self.0.borrow().len()
        }

        fn read(&self, index: usize) -> Descriptor {
            self.0.borrow()[index]
        }

        fn write(&mut self, index: usize, desc: Descriptor) {
            self.0.borrow_mut()[index] = desc;
        }
    }

    const BUF: usize = 2048;
    const WHOLE: u32 = FIRST_FRAGMENT | LAST_FRAGMENT;

    fn buffer(i: usize) -> u64 {
        0x1_0000_0000 + (i as u64) * 0x1000
    }

    fn rx(slots: usize) -> (Shared, RxRing<Shared>) {
        let mem = Shared::new(slots);
        let ring = RxRing::new(mem.clone(), BUF, buffer).unwrap();
        (mem, ring)
    }

    fn take(ring: &mut RxRing<Shared>) -> Option<Result<(usize, usize), RxError>> {
        ring.receive(|slot, len| (slot, len)).map(|r| match r {
            Received::Frame(f) => Ok(f),
            Received::Dropped(e) => Err(e),
        })
    }

    fn armed(i: usize, slots: usize) -> Descriptor {
        let eor = if i + 1 == slots { END_OF_RING } else { 0 };
        Descriptor {
            opts1: OWN | eor | BUF as u32,
            opts2: 0,
            addr: buffer(i),
        }
    }

    #[test]
    fn a_new_receive_ring_gives_every_buffer_to_the_device() {
        let (mem, mut ring) = rx(4);
        for i in 0..4 {
            assert_eq!(mem.get(i), armed(i, 4));
        }
        assert!(!ring.pending());
        assert!(take(&mut ring).is_none());
    }

    #[test]
    fn a_received_frame_is_delivered_and_its_descriptor_rearmed() {
        let (mem, mut ring) = rx(4);
        mem.device_writes_back(0, WHOLE | 64);
        assert!(ring.pending());
        assert_eq!(take(&mut ring), Some(Ok((0, 60))));
        assert_eq!(mem.get(0), armed(0, 4));
        assert!(take(&mut ring).is_none());
    }

    #[test]
    fn the_buffer_stays_with_the_driver_until_delivery_returns() {
        let (mem, mut ring) = rx(2);
        mem.device_writes_back(0, WHOLE | 100);
        let seen = ring.receive(|slot, _| mem.get(slot).opts1 & OWN);
        assert!(matches!(seen, Some(Received::Frame(0))));
        assert_ne!(mem.get(0).opts1 & OWN, 0);
    }

    #[test]
    fn errored_and_split_frames_are_skipped_but_still_rearmed() {
        let (mem, mut ring) = rx(8);
        mem.device_writes_back(0, WHOLE | RX_ERROR_SUMMARY | RX_CRC | 64);
        mem.device_writes_back(1, FIRST_FRAGMENT | BUF as u32);
        mem.device_writes_back(2, LAST_FRAGMENT | 900);
        mem.device_writes_back(3, WHOLE | 1518);
        let mut delivered = Vec::new();
        let mut dropped = Vec::new();
        while let Some(r) = ring.receive(|slot, len| (slot, len)) {
            match r {
                Received::Frame(f) => delivered.push(f),
                Received::Dropped(e) => dropped.push(e),
            }
        }
        assert_eq!(delivered, [(3, 1514)]);
        assert_eq!(
            dropped,
            [RxError::Crc, RxError::Fragment, RxError::Fragment]
        );
        for i in 0..8 {
            assert_eq!(mem.get(i), armed(i, 8));
        }
    }

    #[test]
    fn the_ring_wraps_and_the_last_slot_keeps_end_of_ring() {
        let (mem, mut ring) = rx(3);
        for round in 0..3 {
            for i in 0..3 {
                mem.device_writes_back(i, WHOLE | (100 + i as u32));
                assert_eq!(take(&mut ring), Some(Ok((i, 96 + i))), "round {round}");
            }
        }
        assert_eq!(mem.get(2), armed(2, 3));
    }

    #[test]
    fn rearm_after_a_reset_starts_from_the_first_slot() {
        let (mem, mut ring) = rx(4);
        mem.device_writes_back(0, WHOLE | 64);
        take(&mut ring);
        mem.device_writes_back(1, WHOLE | 64);
        ring.rearm();
        for i in 0..4 {
            assert_eq!(mem.get(i), armed(i, 4));
        }
        mem.device_writes_back(0, WHOLE | 80);
        assert_eq!(take(&mut ring), Some(Ok((0, 76))));
    }

    #[test]
    fn rings_a_descriptor_cannot_describe_are_refused() {
        assert!(RxRing::new(Shared::new(0), BUF, buffer).is_none());
        assert!(RxRing::new(Shared::new(4), 0, buffer).is_none());
        assert!(RxRing::new(Shared::new(4), 0x4000, buffer).is_none());
        assert!(RxRing::new(Shared::new(4), 0x3fff, buffer).is_some());
        assert!(TxRing::new(Shared::new(0), BUF, buffer).is_none());
        assert!(TxRing::new(Shared::new(4), 0x4000, buffer).is_none());
    }

    fn tx(slots: usize) -> (Shared, TxRing<Shared>) {
        let mem = Shared::new(slots);
        let ring = TxRing::new(mem.clone(), BUF, buffer).unwrap();
        (mem, ring)
    }

    fn device_sends(mem: &Shared, i: usize) {
        let opts1 = mem.get(i).opts1 & !OWN;
        mem.device_writes_back(i, opts1);
    }

    #[test]
    fn a_new_transmit_ring_is_all_the_drivers() {
        let (mem, ring) = tx(4);
        for i in 0..4 {
            let eor = if i == 3 { END_OF_RING } else { 0 };
            assert_eq!(
                mem.get(i),
                Descriptor {
                    opts1: eor,
                    opts2: 0,
                    addr: buffer(i)
                }
            );
        }
        assert_eq!(ring.free(), 4);
    }

    #[test]
    fn a_frame_is_filled_before_its_descriptor_goes_to_the_device() {
        let (mem, mut ring) = tx(4);
        let mut owned_while_filling = None;
        let slot = ring.send(60, |slot| {
            owned_while_filling = Some(mem.get(slot).opts1 & OWN)
        });
        assert_eq!(slot, Ok(0));
        assert_eq!(owned_while_filling, Some(0));
        assert_eq!(
            mem.get(0),
            Descriptor {
                opts1: OWN | WHOLE | 60,
                opts2: 0,
                addr: buffer(0)
            }
        );
        assert_eq!((ring.in_flight(), ring.free()), (1, 3));
    }

    #[test]
    fn a_full_ring_refuses_without_touching_a_buffer() {
        let (mem, mut ring) = tx(4);
        for i in 0..4 {
            assert_eq!(ring.send(100, |_| {}), Ok(i));
        }
        assert_eq!(mem.get(3).opts1, OWN | END_OF_RING | WHOLE | 100);
        let mut filled = false;
        assert_eq!(ring.send(100, |_| filled = true), Err(TxError::Full));
        assert!(!filled);
        assert_eq!(ring.free(), 0);
    }

    #[test]
    fn reclaim_is_in_order_and_stops_at_the_first_owned_slot() {
        let (mem, mut ring) = tx(4);
        for _ in 0..3 {
            ring.send(100, |_| {}).unwrap();
        }
        device_sends(&mem, 1);
        assert_eq!(ring.reclaim(), 0);
        device_sends(&mem, 0);
        assert_eq!(ring.reclaim(), 2);
        assert_eq!(ring.in_flight(), 1);
        device_sends(&mem, 2);
        assert_eq!(ring.reclaim(), 1);
        assert_eq!(ring.reclaim(), 0);
        assert_eq!(ring.free(), 4);
    }

    #[test]
    fn reclaim_ignores_idle_slots_the_device_never_had() {
        let (_, mut ring) = tx(4);
        assert_eq!(ring.reclaim(), 0);
        assert_eq!(ring.free(), 4);
    }

    #[test]
    fn sending_wraps_after_reclaim() {
        let (mem, mut ring) = tx(3);
        for round in 0..4 {
            for i in 0..3 {
                assert_eq!(ring.send(64, |_| {}), Ok(i), "round {round}");
            }
            assert_eq!(ring.send(64, |_| {}), Err(TxError::Full));
            assert_eq!(mem.get(2).opts1 & END_OF_RING, END_OF_RING);
            for i in 0..3 {
                device_sends(&mem, i);
            }
            assert_eq!(ring.reclaim(), 3);
        }
    }

    #[test]
    fn lengths_outside_a_buffer_are_refused() {
        let (_, mut ring) = tx(2);
        assert_eq!(ring.send(0, |_| {}), Err(TxError::Length));
        assert_eq!(ring.send(BUF + 1, |_| {}), Err(TxError::Length));
        assert_eq!(ring.send(BUF, |_| {}), Ok(0));
    }

    #[test]
    fn clear_takes_everything_back() {
        let (mem, mut ring) = tx(4);
        ring.send(64, |_| {}).unwrap();
        ring.send(64, |_| {}).unwrap();
        assert_eq!(ring.clear(), 2);
        assert_eq!(mem.get(0).opts1 & OWN, 0);
        assert_eq!(mem.get(1).opts1 & OWN, 0);
        assert_eq!(ring.send(64, |_| {}), Ok(0));
    }
}
