//! The Identify Controller and Identify Namespace data structures (Base
//! Specification 2.0, §5.17.2; NVM Command Set Specification 1.0, §4.1.5).

pub const IDENTIFY_BYTES: usize = 4096;

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from(le32(b, at)) | (u64::from(le32(b, at + 4)) << 32)
}

fn field<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&b[at..at + N]);
    out
}

/// An ASCII identification field without its trailing space padding.
pub fn trim_ascii(field: &[u8]) -> &[u8] {
    let end = field
        .iter()
        .rposition(|&c| c != b' ' && c != 0)
        .map_or(0, |i| i + 1);
    &field[..end]
}

/// What the driver reads from Identify Controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControllerInfo {
    pub vendor: u16,
    pub serial: [u8; 20],
    pub model: [u8; 40],
    pub firmware: [u8; 8],
    /// Largest transfer as a power of two of the minimum page size; 0 is
    /// unlimited.
    pub mdts: u8,
    /// How long a normal shutdown may take, in microseconds; 0 is unreported.
    pub rtd3e_us: u32,
    pub hmb: HostMemoryRequest,
    pub sqes: u8,
    pub cqes: u8,
    pub namespaces: u32,
    pub volatile_write_cache: bool,
}

/// The host memory buffer a controller asks for, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostMemoryRequest {
    pub preferred: u64,
    pub minimum: u64,
    /// Smallest descriptor the controller takes; 0 is any.
    pub min_chunk: u64,
    /// Most descriptors the controller takes; 0 is unlimited.
    pub max_chunks: u32,
}

const HMB_UNIT: u64 = 4096;

impl ControllerInfo {
    pub fn parse(id: &[u8]) -> Option<Self> {
        if id.len() < IDENTIFY_BYTES {
            return None;
        }
        Some(Self {
            vendor: le16(id, 0x00),
            serial: field(id, 0x04),
            model: field(id, 0x18),
            firmware: field(id, 0x40),
            mdts: id[0x4D],
            rtd3e_us: le32(id, 0x58),
            hmb: HostMemoryRequest {
                preferred: u64::from(le32(id, 0x110)) * HMB_UNIT,
                minimum: u64::from(le32(id, 0x114)) * HMB_UNIT,
                min_chunk: u64::from(le32(id, 0x14C)) * HMB_UNIT,
                max_chunks: u32::from(le16(id, 0x150)),
            },
            sqes: id[0x200],
            cqes: id[0x201],
            namespaces: le32(id, 0x204),
            volatile_write_cache: id[0x20D] & 1 != 0,
        })
    }

    /// The largest transfer one command may carry, in bytes; `None` is
    /// unlimited.
    pub fn max_transfer(&self, min_page_size: usize) -> Option<usize> {
        if self.mdts == 0 {
            return None;
        }
        1usize
            .checked_shl(u32::from(self.mdts))
            .and_then(|pages| pages.checked_mul(min_page_size))
    }

    /// Whether the controller takes entries of the sizes this driver writes:
    /// SQES and CQES name the required size in their low nibble and the
    /// largest in their high one, both as powers of two.
    pub fn takes_entry_sizes(&self, sqe: usize, cqe: usize) -> bool {
        let fits = |sizes: u8, bytes: usize| {
            let shift = bytes.trailing_zeros() as u8;
            (sizes & 0xF..=sizes >> 4).contains(&shift)
        };
        fits(self.sqes, sqe) && fits(self.cqes, cqe)
    }
}

/// What the driver reads from Identify Namespace for the format in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamespaceInfo {
    pub blocks: u64,
    pub block_shift: u8,
    pub metadata_bytes: u16,
    /// Metadata interleaved with each block's data rather than in a buffer of
    /// its own.
    pub metadata_extended: bool,
    pub nguid: [u8; 16],
    pub eui64: [u8; 8],
}

/// Why a namespace is not served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamespaceRefusal {
    /// NSZE is zero: the namespace ID is allocated but not attached.
    Inactive,
    /// The format carries metadata, which needs a buffer per command.
    Metadata,
    /// A block smaller than 512 bytes or larger than a page.
    BlockSize,
    /// The formatted LBA format index is beyond the formats reported.
    Format,
}

impl NamespaceInfo {
    pub fn parse(id: &[u8]) -> Result<Self, NamespaceRefusal> {
        if id.len() < IDENTIFY_BYTES {
            return Err(NamespaceRefusal::Format);
        }
        let blocks = le64(id, 0x00);
        if blocks == 0 {
            return Err(NamespaceRefusal::Inactive);
        }
        let formats = usize::from(id[0x19]) + 1;
        let flbas = id[0x1A];
        let mut index = usize::from(flbas & 0xF);
        if formats > 16 {
            index |= usize::from((flbas >> 5) & 0b11) << 4;
        }
        if index >= formats.min(64) {
            return Err(NamespaceRefusal::Format);
        }
        let format = le32(id, 0x80 + 4 * index);
        Ok(Self {
            blocks,
            block_shift: (format >> 16) as u8,
            metadata_bytes: format as u16,
            metadata_extended: flbas & (1 << 4) != 0,
            nguid: field(id, 0x68),
            eui64: field(id, 0x78),
        })
    }

    pub fn block_size(&self) -> u32 {
        1 << self.block_shift
    }

    /// Refuse what the driver cannot serve: metadata of any kind, and blocks
    /// outside `512..=page_size`.
    pub fn check(&self, page_size: usize) -> Result<(), NamespaceRefusal> {
        if self.metadata_bytes != 0 {
            return Err(NamespaceRefusal::Metadata);
        }
        if !(9..=page_size.trailing_zeros() as u8).contains(&self.block_shift) {
            return Err(NamespaceRefusal::BlockSize);
        }
        Ok(())
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.blocks << self.block_shift
    }
}

/// The namespace IDs in an Active Namespace ID list, ascending until the
/// first zero.
pub fn active_namespaces(list: &[u8]) -> impl Iterator<Item = u32> + '_ {
    list.chunks_exact(4)
        .map(|c| le32(c, 0))
        .take_while(|&nsid| nsid != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> [u8; IDENTIFY_BYTES] {
        [0u8; IDENTIFY_BYTES]
    }

    #[test]
    fn parses_a_controller() {
        let mut id = blank();
        id[0..2].copy_from_slice(&0x2646u16.to_le_bytes());
        id[0x04..0x18].copy_from_slice(b"50026B7382A1B2C3    ");
        id[0x18..0x40].copy_from_slice(b"KINGSTON SNV3S1000G                     ");
        id[0x4D] = 5;
        id[0x58..0x5C].copy_from_slice(&8_000_000u32.to_le_bytes());
        id[0x110..0x114].copy_from_slice(&16384u32.to_le_bytes());
        id[0x114..0x118].copy_from_slice(&8192u32.to_le_bytes());
        id[0x14C..0x150].copy_from_slice(&512u32.to_le_bytes());
        id[0x150..0x152].copy_from_slice(&8u16.to_le_bytes());
        id[0x200] = 0x66;
        id[0x201] = 0x44;
        id[0x204..0x208].copy_from_slice(&1u32.to_le_bytes());
        id[0x20D] = 1;

        let c = ControllerInfo::parse(&id).unwrap();
        assert_eq!(c.vendor, 0x2646);
        assert_eq!(trim_ascii(&c.model), b"KINGSTON SNV3S1000G");
        assert_eq!(trim_ascii(&c.serial), b"50026B7382A1B2C3");
        assert_eq!(c.max_transfer(4096), Some(128 * 1024));
        assert_eq!(c.rtd3e_us, 8_000_000);
        assert_eq!(
            c.hmb,
            HostMemoryRequest {
                preferred: 64 << 20,
                minimum: 32 << 20,
                min_chunk: 2 << 20,
                max_chunks: 8,
            }
        );
        assert!(c.takes_entry_sizes(64, 16));
        assert!(!c.takes_entry_sizes(128, 16));
        assert!(c.volatile_write_cache);
        assert_eq!(c.namespaces, 1);
        assert!(ControllerInfo::parse(&id[..100]).is_none());
    }

    #[test]
    fn unlimited_transfer_is_none() {
        let c = ControllerInfo::parse(&blank()).unwrap();
        assert_eq!(c.max_transfer(4096), None);
    }

    fn namespace(blocks: u64, formats: &[u32], flbas: u8) -> [u8; IDENTIFY_BYTES] {
        let mut id = blank();
        id[0..8].copy_from_slice(&blocks.to_le_bytes());
        id[0x19] = (formats.len() - 1) as u8;
        id[0x1A] = flbas;
        for (i, f) in formats.iter().enumerate() {
            id[0x80 + 4 * i..0x84 + 4 * i].copy_from_slice(&f.to_le_bytes());
        }
        id
    }

    const LBA512: u32 = 9 << 16;
    const LBA4K: u32 = 12 << 16;

    #[test]
    fn parses_the_format_in_use() {
        let ns = NamespaceInfo::parse(&namespace(1 << 20, &[LBA512, LBA4K], 1)).unwrap();
        assert_eq!(ns.block_size(), 4096);
        assert_eq!(ns.capacity_bytes(), 4 << 30);
        assert_eq!(ns.check(4096), Ok(()));

        let ns = NamespaceInfo::parse(&namespace(8, &[LBA512, LBA4K], 0)).unwrap();
        assert_eq!(ns.block_size(), 512);
    }

    #[test]
    fn reads_the_format_index_high_bits_past_sixteen_formats() {
        let mut formats = [LBA512; 20];
        formats[17] = LBA4K;
        let ns = NamespaceInfo::parse(&namespace(8, &formats, 0b0010_0001)).unwrap();
        assert_eq!(ns.block_size(), 4096);
        let few = NamespaceInfo::parse(&namespace(8, &[LBA512, LBA4K], 0b0010_0001));
        assert_eq!(few.unwrap().block_size(), 4096, "high bits ignored");
    }

    #[test]
    fn refuses_what_it_cannot_serve() {
        assert_eq!(
            NamespaceInfo::parse(&namespace(0, &[LBA512], 0)),
            Err(NamespaceRefusal::Inactive)
        );
        assert_eq!(
            NamespaceInfo::parse(&namespace(8, &[LBA512], 3)),
            Err(NamespaceRefusal::Format)
        );
        let meta = NamespaceInfo::parse(&namespace(8, &[LBA512 | 8], 0x10)).unwrap();
        assert!(meta.metadata_extended);
        assert_eq!(meta.check(4096), Err(NamespaceRefusal::Metadata));
        let big = NamespaceInfo::parse(&namespace(8, &[16 << 16], 0)).unwrap();
        assert_eq!(big.check(4096), Err(NamespaceRefusal::BlockSize));
    }

    #[test]
    fn lists_active_namespaces() {
        let mut list = [0u8; 16];
        list[0..4].copy_from_slice(&1u32.to_le_bytes());
        list[4..8].copy_from_slice(&3u32.to_le_bytes());
        list[12..16].copy_from_slice(&9u32.to_le_bytes());
        let mut ids = active_namespaces(&list);
        assert_eq!(
            (ids.next(), ids.next(), ids.next()),
            (Some(1), Some(3), None)
        );
    }
}
