//! Sizing a host memory buffer (Base Specification 2.0, §8.9).
//!
//! A controller without DRAM asks for host memory to cache its mapping
//! tables: a preferred size, a minimum it can use at all, and limits on the
//! descriptors that list the chunks. The host may grant anything from the
//! minimum up, or decline.

use crate::identify::HostMemoryRequest;

/// The size of one [`descriptor`].
pub const DESCRIPTOR_BYTES: usize = 16;

/// Descriptors one list page holds, the most this driver hands over.
pub const MAX_DESCRIPTORS: u32 = (crate::regs::PAGE_SIZE / DESCRIPTOR_BYTES) as u32;

/// One descriptor-list entry: the chunk's address, then its length in
/// memory pages, the rest reserved.
pub fn descriptor(address: u64, pages: u32) -> [u8; DESCRIPTOR_BYTES] {
    let mut entry = [0u8; DESCRIPTOR_BYTES];
    entry[..8].copy_from_slice(&address.to_le_bytes());
    entry[8..12].copy_from_slice(&pages.to_le_bytes());
    entry
}

/// Physically contiguous memory for a host memory buffer.
pub trait HostMemory {
    /// Allocate one more chunk of `bytes`, a power of two of at least a page,
    /// page-aligned; `false` once none is left.
    fn alloc_chunk(&mut self, bytes: u64) -> bool;
    /// Give back every chunk allocated so far.
    fn release_all(&mut self);
}

/// What was granted: `chunks` chunks of `chunk_bytes` each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    pub chunk_bytes: u64,
    pub chunks: u32,
}

impl Grant {
    pub fn bytes(&self) -> u64 {
        self.chunk_bytes * u64::from(self.chunks)
    }
}

/// Allocate a buffer for `request` in chunks no larger than `largest_chunk`,
/// preferring few large chunks: at each chunk size, as many chunks as reach
/// the preferred size or the descriptor limit; a total under the minimum is
/// given back and the next smaller size tried. `None` declines: the
/// controller asked for nothing, or no chunk size reaches its minimum.
pub fn grant(
    request: &HostMemoryRequest,
    largest_chunk: u64,
    memory: &mut impl HostMemory,
) -> Option<Grant> {
    let page = crate::regs::PAGE_SIZE as u64;
    if request.preferred == 0 || request.preferred < request.minimum {
        return None;
    }
    let max_chunks = match request.max_chunks {
        0 => MAX_DESCRIPTORS,
        n => n.min(MAX_DESCRIPTORS),
    };
    let floor = request.min_chunk.max(page);
    let mut chunk = prev_power_of_two(largest_chunk.min(request.preferred.next_power_of_two()));

    while chunk >= floor {
        let want = request.preferred.div_ceil(chunk).min(u64::from(max_chunks)) as u32;
        let mut chunks = 0;
        while chunks < want && memory.alloc_chunk(chunk) {
            chunks += 1;
        }
        let granted = Grant {
            chunk_bytes: chunk,
            chunks,
        };
        if chunks > 0 && granted.bytes() >= request.minimum {
            return Some(granted);
        }
        memory.release_all();
        chunk /= 2;
    }
    None
}

fn prev_power_of_two(n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        1 << (63 - n.leading_zeros())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// Hands out chunks until its budget, counted per chunk size, runs out.
    struct Budget {
        per_size: fn(u64) -> u32,
        taken: u32,
        released: u32,
    }

    impl HostMemory for Budget {
        fn alloc_chunk(&mut self, bytes: u64) -> bool {
            if self.taken >= (self.per_size)(bytes) {
                return false;
            }
            self.taken += 1;
            true
        }

        fn release_all(&mut self) {
            self.taken = 0;
            self.released += 1;
        }
    }

    fn plenty() -> Budget {
        Budget {
            per_size: |_| u32::MAX,
            taken: 0,
            released: 0,
        }
    }

    fn request(preferred: u64, minimum: u64, min_chunk: u64, max_chunks: u32) -> HostMemoryRequest {
        HostMemoryRequest {
            preferred,
            minimum,
            min_chunk,
            max_chunks,
        }
    }

    #[test]
    fn encodes_a_descriptor() {
        let entry = descriptor(0x1234_5000, 1024);
        assert_eq!(entry[..8], 0x1234_5000u64.to_le_bytes());
        assert_eq!(entry[8..12], 1024u32.to_le_bytes());
        assert_eq!(entry[12..], [0; 4]);
    }

    #[test]
    fn grants_the_preferred_size_in_the_largest_chunks() {
        let got = grant(&request(64 * MIB, 32 * MIB, 0, 0), 4 * MIB, &mut plenty());
        assert_eq!(
            got,
            Some(Grant {
                chunk_bytes: 4 * MIB,
                chunks: 16
            })
        );
    }

    #[test]
    fn a_descriptor_limit_caps_the_grant() {
        let got = grant(&request(64 * MIB, 16 * MIB, 0, 8), 4 * MIB, &mut plenty()).unwrap();
        assert_eq!(
            (got.chunk_bytes, got.chunks, got.bytes()),
            (4 * MIB, 8, 32 * MIB)
        );
    }

    #[test]
    fn falls_back_to_smaller_chunks_when_large_ones_run_out() {
        let mut memory = Budget {
            per_size: |bytes| if bytes >= 2 * MIB { 1 } else { u32::MAX },
            ..plenty()
        };
        let got = grant(&request(16 * MIB, 8 * MIB, 0, 0), 4 * MIB, &mut memory).unwrap();
        assert_eq!((got.chunk_bytes, got.chunks), (MIB, 16));
        assert_eq!(
            memory.released, 2,
            "the 4 MiB and 2 MiB attempts were given back"
        );
    }

    #[test]
    fn keeps_a_partial_grant_above_the_minimum() {
        let mut memory = Budget {
            per_size: |_| 5,
            ..plenty()
        };
        let got = grant(&request(64 * MIB, 16 * MIB, 0, 0), 4 * MIB, &mut memory).unwrap();
        assert_eq!(got.bytes(), 20 * MIB);
        assert_eq!(memory.released, 0);
    }

    #[test]
    fn respects_the_minimum_descriptor_size() {
        let mut memory = Budget {
            per_size: |bytes| if bytes >= MIB { 0 } else { u32::MAX },
            ..plenty()
        };
        assert_eq!(
            grant(&request(8 * MIB, 4 * MIB, MIB, 0), 4 * MIB, &mut memory),
            None
        );
    }

    #[test]
    fn declines_what_was_not_asked_for_or_cannot_be_met() {
        assert_eq!(grant(&request(0, 0, 0, 0), 4 * MIB, &mut plenty()), None);
        assert_eq!(
            grant(&request(MIB, 2 * MIB, 0, 0), 4 * MIB, &mut plenty()),
            None
        );
        let mut starved = Budget {
            per_size: |_| 0,
            ..plenty()
        };
        assert_eq!(grant(&request(MIB, MIB, 0, 0), 4 * MIB, &mut starved), None);
    }

    #[test]
    fn rounds_a_small_preference_up_to_one_chunk() {
        let got = grant(&request(24 * 1024, 0, 0, 0), 4 * MIB, &mut plenty()).unwrap();
        assert_eq!((got.chunk_bytes, got.chunks), (32 * 1024, 1));
    }
}
