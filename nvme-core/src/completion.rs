//! Completion queue entries (Base Specification 2.0, §4.2.3) and their
//! status field.

pub const CQE_BYTES: usize = 16;

/// One completion queue entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    /// Dword 0, command specific: the allocated queue counts for Number of
    /// Queues, say.
    pub result: u32,
    pub sq_head: u16,
    pub sq_id: u16,
    pub cid: u16,
    pub phase: bool,
    pub status: Status,
}

impl Completion {
    pub fn from_dwords(dw: [u32; 4]) -> Self {
        Self {
            result: dw[0],
            sq_head: dw[2] as u16,
            sq_id: (dw[2] >> 16) as u16,
            cid: dw[3] as u16,
            phase: dw[3] & (1 << 16) != 0,
            status: Status((dw[3] >> 17) as u16),
        }
    }
}

/// The 15-bit status field: code, code type, retry delay, more, do not retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(pub u16);

/// What a completed command means for the request that sent it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Done,
    /// The controller does not implement the command or a field of it.
    Unsupported,
    /// The LBA range leaves the namespace.
    OutOfRange,
    /// A failure the controller says may succeed if sent again.
    Retry,
    Failed,
}

const SCT_GENERIC: u8 = 0;
const SC_INVALID_OPCODE: u8 = 0x01;
const SC_INVALID_FIELD: u8 = 0x02;
const SC_LBA_OUT_OF_RANGE: u8 = 0x80;

impl Status {
    pub const SUCCESS: Status = Status(0);

    pub fn code(self) -> u8 {
        self.0 as u8
    }

    pub fn code_type(self) -> u8 {
        ((self.0 >> 8) & 0b111) as u8
    }

    pub fn do_not_retry(self) -> bool {
        self.0 & (1 << 14) != 0
    }

    pub fn is_success(self) -> bool {
        self.code_type() == SCT_GENERIC && self.code() == 0
    }

    pub fn disposition(self) -> Disposition {
        if self.is_success() {
            return Disposition::Done;
        }
        if self.code_type() == SCT_GENERIC {
            match self.code() {
                SC_INVALID_OPCODE | SC_INVALID_FIELD => return Disposition::Unsupported,
                SC_LBA_OUT_OF_RANGE => return Disposition::OutOfRange,
                _ => {}
            }
        }
        if self.do_not_retry() {
            Disposition::Failed
        } else {
            Disposition::Retry
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_an_entry() {
        let c = Completion::from_dwords([7, 0, 0x0001_0005, 0x0001_1234]);
        assert_eq!(
            (c.result, c.sq_head, c.sq_id, c.cid, c.phase),
            (7, 5, 1, 0x1234, true)
        );
        assert!(c.status.is_success());
        assert_eq!(c.status.disposition(), Disposition::Done);
    }

    #[test]
    fn classifies_failures() {
        const MEDIA_ERROR: u16 = 0x0281;
        const DO_NOT_RETRY: u16 = 1 << 14;
        let entry = |status: u16| Completion::from_dwords([0, 0, 0, u32::from(status) << 17]);
        assert_eq!(entry(0x0001).status.disposition(), Disposition::Unsupported);
        assert_eq!(entry(0x0080).status.disposition(), Disposition::OutOfRange);
        assert_eq!(entry(MEDIA_ERROR).status.disposition(), Disposition::Retry);
        assert_eq!(
            entry(MEDIA_ERROR | DO_NOT_RETRY).status.disposition(),
            Disposition::Failed
        );
        assert!(entry(MEDIA_ERROR | DO_NOT_RETRY).status.do_not_retry());
        assert_eq!(entry(MEDIA_ERROR).status.code_type(), 2);
    }
}
