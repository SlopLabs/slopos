use super::page_table_defs::PageTableLevel;
use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PagingError {
    NoMemory,
    NotAligned { address: u64, required: u64 },
    NotMapped { address: u64, level: PageTableLevel },
    AlreadyMapped { address: u64 },
    MappedToHugePage { level: PageTableLevel },
    InvalidPageTable,
    InvalidPhysicalAddress { address: u64 },
}

impl fmt::Display for PagingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMemory => write!(f, "out of memory for page table allocation"),
            Self::NotAligned { address, required } => {
                write!(f, "address {:#x} not aligned to {:#x}", address, required)
            }
            Self::NotMapped { address, level } => {
                write!(
                    f,
                    "address {:#x} not mapped (stopped at level {})",
                    address, level
                )
            }
            Self::AlreadyMapped { address } => {
                write!(f, "address {:#x} already mapped", address)
            }
            Self::MappedToHugePage { level } => {
                write!(f, "cannot traverse huge page at level {}", level)
            }
            Self::InvalidPageTable => write!(f, "invalid page table pointer"),
            Self::InvalidPhysicalAddress { address } => {
                write!(f, "invalid physical address {:#x}", address)
            }
        }
    }
}

pub type PagingResult<T = ()> = Result<T, PagingError>;
