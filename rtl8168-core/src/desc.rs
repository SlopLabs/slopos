//! The 16-byte descriptor both rings use, and what a received descriptor's
//! status word says about its frame.

pub const DESCRIPTOR_SIZE: usize = 16;
/// Rings start on this boundary; the driver asserts its ring pages meet it.
pub const RING_ALIGN: usize = 256;

pub const OWN: u32 = 1 << 31;
pub const END_OF_RING: u32 = 1 << 30;
pub const FIRST_FRAGMENT: u32 = 1 << 29;
pub const LAST_FRAGMENT: u32 = 1 << 28;
pub const LENGTH_MASK: u32 = 0x3fff;

pub const RX_WATCHDOG: u32 = 1 << 22;
pub const RX_ERROR_SUMMARY: u32 = 1 << 21;
pub const RX_RUNT: u32 = 1 << 20;
pub const RX_CRC: u32 = 1 << 19;

/// The frame check sequence the chip leaves at the end of every received
/// frame.
pub const FCS_LEN: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Descriptor {
    pub opts1: u32,
    pub opts2: u32,
    pub addr: u64,
}

impl Descriptor {
    pub fn to_bytes(self) -> [u8; DESCRIPTOR_SIZE] {
        let mut b = [0; DESCRIPTOR_SIZE];
        b[0..4].copy_from_slice(&self.opts1.to_le_bytes());
        b[4..8].copy_from_slice(&self.opts2.to_le_bytes());
        b[8..16].copy_from_slice(&self.addr.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8; DESCRIPTOR_SIZE]) -> Self {
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Self {
            opts1: word(0),
            opts2: word(4),
            addr: u64::from(word(8)) | u64::from(word(12)) << 32,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxError {
    /// Longer than the chip's receive watchdog allows.
    Watchdog,
    Runt,
    Crc,
    /// The error summary bit with none of the bits above.
    Other,
    /// The frame spans more than one descriptor: it was longer than a
    /// receive buffer.
    Fragment,
    /// A whole-frame descriptor whose length cannot be a frame in a buffer
    /// of this ring.
    Length,
}

/// What a descriptor the chip has handed back holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxStatus {
    /// A whole frame of this many bytes, FCS excluded.
    Frame(usize),
    Dropped(RxError),
}

/// Decode a received descriptor's status word for a ring whose buffers hold
/// `buffer_len` bytes.
pub fn rx_status(opts1: u32, buffer_len: usize) -> RxStatus {
    if opts1 & RX_ERROR_SUMMARY != 0 {
        let error = if opts1 & RX_WATCHDOG != 0 {
            RxError::Watchdog
        } else if opts1 & RX_RUNT != 0 {
            RxError::Runt
        } else if opts1 & RX_CRC != 0 {
            RxError::Crc
        } else {
            RxError::Other
        };
        return RxStatus::Dropped(error);
    }
    if opts1 & (FIRST_FRAGMENT | LAST_FRAGMENT) != FIRST_FRAGMENT | LAST_FRAGMENT {
        return RxStatus::Dropped(RxError::Fragment);
    }
    let len = (opts1 & LENGTH_MASK) as usize;
    if len <= FCS_LEN || len > buffer_len {
        return RxStatus::Dropped(RxError::Length);
    }
    RxStatus::Frame(len - FCS_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHOLE: u32 = FIRST_FRAGMENT | LAST_FRAGMENT;

    #[test]
    fn descriptor_bytes_are_little_endian_in_field_order() {
        let d = Descriptor {
            opts1: 0x8000_05ea,
            opts2: 0x0102_0304,
            addr: 0x0000_0001_2345_6700,
        };
        let b = d.to_bytes();
        assert_eq!(
            b,
            [
                0xea, 0x05, 0x00, 0x80, 0x04, 0x03, 0x02, 0x01, 0x00, 0x67, 0x45, 0x23, 0x01, 0x00,
                0x00, 0x00
            ]
        );
        assert_eq!(Descriptor::from_bytes(&b), d);
    }

    #[test]
    fn the_fcs_is_stripped_from_a_whole_frame() {
        assert_eq!(rx_status(WHOLE | 64, 2048), RxStatus::Frame(60));
        assert_eq!(rx_status(WHOLE | 1518, 2048), RxStatus::Frame(1514));
    }

    #[test]
    fn length_is_the_low_fourteen_bits_only() {
        assert_eq!(
            rx_status(WHOLE | 1 << 14 | 1 << 15 | 100, 2048),
            RxStatus::Frame(96)
        );
    }

    #[test]
    fn lengths_at_the_edges_of_a_buffer() {
        assert_eq!(rx_status(WHOLE | 2048, 2048), RxStatus::Frame(2044));
        assert_eq!(
            rx_status(WHOLE | 2049, 2048),
            RxStatus::Dropped(RxError::Length)
        );
        assert_eq!(rx_status(WHOLE | 5, 2048), RxStatus::Frame(1));
        assert_eq!(
            rx_status(WHOLE | 4, 2048),
            RxStatus::Dropped(RxError::Length)
        );
        assert_eq!(rx_status(WHOLE, 2048), RxStatus::Dropped(RxError::Length));
    }

    #[test]
    fn errors_take_precedence_over_a_good_length() {
        let bad = |bits| rx_status(WHOLE | RX_ERROR_SUMMARY | bits | 64, 2048);
        assert_eq!(
            bad(RX_WATCHDOG | RX_CRC),
            RxStatus::Dropped(RxError::Watchdog)
        );
        assert_eq!(bad(RX_RUNT), RxStatus::Dropped(RxError::Runt));
        assert_eq!(bad(RX_CRC), RxStatus::Dropped(RxError::Crc));
        assert_eq!(bad(0), RxStatus::Dropped(RxError::Other));
    }

    #[test]
    fn detail_bits_without_the_summary_bit_are_not_errors() {
        assert_eq!(rx_status(WHOLE | RX_CRC | 64, 2048), RxStatus::Frame(60));
    }

    #[test]
    fn any_part_of_a_split_frame_is_a_fragment() {
        for marks in [FIRST_FRAGMENT, LAST_FRAGMENT, 0] {
            assert_eq!(
                rx_status(marks | 64, 2048),
                RxStatus::Dropped(RxError::Fragment)
            );
        }
    }
}
