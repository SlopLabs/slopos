//! ELF64 parsing and validation for user binaries.
//!
//! Input is untrusted: every field is validated before use and any failure
//! aborts the load.
//!
//! Structure layouts and constant values follow the ELF gABI / System V ABI
//! (x86-64 supplement).

use core::fmt;

use crate::memory_layout_defs::{KERNEL_SPACE_START_VA, USER_SPACE_END_VA, USER_SPACE_START_VA};
use crate::paging_defs::PAGE_SIZE_4KB;

pub const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

pub const ELFCLASS64: u8 = 2;

pub const ELFDATA2LSB: u8 = 1;

pub const EV_CURRENT: u8 = 1;

pub const ELFOSABI_NONE: u8 = 0;

pub const ET_EXEC: u16 = 2;

/// Shared object; also how a position-independent executable is encoded.
pub const ET_DYN: u16 = 3;

pub const EM_X86_64: u16 = 0x3E;

pub const PT_LOAD: u32 = 1;

pub const PT_DYNAMIC: u32 = 2;

pub const PT_INTERP: u32 = 3;

pub const PT_NOTE: u32 = 4;

pub const PT_PHDR: u32 = 6;

pub const PT_TLS: u32 = 7;

pub const PT_GNU_STACK: u32 = 0x6474_e551;

pub const PT_GNU_RELRO: u32 = 0x6474_e552;

pub const PF_X: u32 = 0x1;

pub const PF_W: u32 = 0x2;

pub const PF_R: u32 = 0x4;

/// DoS bound on the number of program headers parsed.
pub const MAX_PROGRAM_HEADERS: usize = 128;

/// Bounds the validator's out-slice; not a policy about how many a program may
/// have.
pub const MAX_LOAD_SEGMENTS: usize = 64;

/// DoS bound on the total mapped size of an image.
///
/// The mapped *extent*. What bounds the frames the loader allocates is the
/// file's own size plus [`MAX_TOTAL_ZERO_FILL_SIZE`].
pub const MAX_TOTAL_MAPPED_SIZE: u64 = 2 * 1024 * 1024 * 1024;

/// DoS bound on how much of an image is `p_memsz` past `p_filesz` — its
/// `.bss`, which the loader allocates and zeroes rather than reading.
///
/// Load-bearing because the loader is eager: without it a one-page ELF
/// declaring a 2 GiB `PT_LOAD` would have an unprivileged `exec` memset half a
/// million frames under the per-process lock. Set to what the whole-image
/// ceiling used to permit, so that amplification is unchanged.
pub const MAX_TOTAL_ZERO_FILL_SIZE: u64 = 256 * 1024 * 1024;

/// Bytes of the file the validator needs in hand: the ELF header plus the
/// largest program-header table it will parse.
pub const ELF_HEADER_WINDOW: usize = 64 + MAX_PROGRAM_HEADERS * 56;

pub const MIN_ELF_SIZE: usize = 64;

/// Ceiling on an interpreter path, matching `USER_PATH_MAX`. A `PT_INTERP`
/// past it is refused rather than truncated into the name of another file.
pub const MAX_INTERP_PATH_LEN: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    BufferTooSmall,
    InvalidMagic,
    Not64Bit,
    NotLittleEndian,
    InvalidVersion,
    NotExecutable,
    WrongArchitecture,
    InvalidPhdrOffset,
    InvalidPhdrSize,
    TooManyProgramHeaders,
    PhdrTableOverflow,
    InvalidSegmentOffset,
    FileSizeExceedsMemSize,
    SegmentSizeOverflow,
    InvalidAlignment,
    KernelAddressViolation,
    AddressOutOfBounds,
    SegmentOverlap,
    TotalSizeExceeded,
    EntryPointInvalid,
    TooManyLoadSegments,
    NoLoadSegments,
    NullPointer,
    InterpreterInvalid,
    AliasedFileExtent,
    UnsupportedLoadBase,
    /// The image's pages could not be promised against the commit ceiling.
    OutOfMemory,
}

impl From<ElfError> for slopos_abi::Errno {
    fn from(err: ElfError) -> Self {
        match err {
            ElfError::OutOfMemory => Self::ENOMEM,
            _ => Self::ENOEXEC,
        }
    }
}

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BufferTooSmall => write!(f, "buffer too small for ELF header"),
            Self::InvalidMagic => write!(f, "invalid ELF magic bytes"),
            Self::Not64Bit => write!(f, "not a 64-bit ELF"),
            Self::NotLittleEndian => write!(f, "not little-endian"),
            Self::InvalidVersion => write!(f, "invalid ELF version"),
            Self::NotExecutable => write!(f, "not an executable or shared object"),
            Self::WrongArchitecture => write!(f, "not x86-64 architecture"),
            Self::InvalidPhdrOffset => write!(f, "invalid program header offset"),
            Self::InvalidPhdrSize => write!(f, "invalid program header size"),
            Self::TooManyProgramHeaders => write!(f, "too many program headers"),
            Self::PhdrTableOverflow => write!(f, "program header table overflow"),
            Self::InvalidSegmentOffset => write!(f, "segment offset overflow"),
            Self::FileSizeExceedsMemSize => write!(f, "segment file size > memory size"),
            Self::SegmentSizeOverflow => write!(f, "segment size overflow"),
            Self::InvalidAlignment => write!(f, "invalid segment alignment"),
            Self::KernelAddressViolation => write!(f, "segment maps to kernel space"),
            Self::AddressOutOfBounds => write!(f, "segment outside user address space"),
            Self::SegmentOverlap => write!(f, "overlapping segments"),
            Self::TotalSizeExceeded => write!(f, "total mapped size exceeded"),
            Self::EntryPointInvalid => write!(f, "entry point outside loaded segments"),
            Self::TooManyLoadSegments => write!(f, "too many PT_LOAD segments"),
            Self::NoLoadSegments => write!(f, "no PT_LOAD segments found"),
            Self::NullPointer => write!(f, "null pointer"),
            Self::InterpreterInvalid => write!(f, "PT_INTERP does not name a path"),
            Self::AliasedFileExtent => {
                write!(f, "segments name more file bytes than the file holds")
            }
            Self::UnsupportedLoadBase => {
                write!(f, "image is not linked at the process code base")
            }
            Self::OutOfMemory => write!(f, "no commit for the image's pages"),
        }
    }
}

pub type ElfResult<T> = Result<T, ElfError>;

/// Validated 64-bit ELF header; built only after every field has been checked.
#[derive(Debug, Clone, Copy)]
pub struct Elf64Header {
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

impl Elf64Header {
    pub const PHDR_SIZE: u16 = 56;

    /// Parse and validate an ELF header from raw bytes.
    ///
    /// # Safety
    ///
    /// The caller must ensure `data` points to valid memory of at least
    /// `MIN_ELF_SIZE` bytes.
    pub fn parse(data: &[u8]) -> ElfResult<Self> {
        if data.len() < MIN_ELF_SIZE {
            return Err(ElfError::BufferTooSmall);
        }

        if data[0..4] != ELF_MAGIC {
            return Err(ElfError::InvalidMagic);
        }

        if data[4] != ELFCLASS64 {
            return Err(ElfError::Not64Bit);
        }

        if data[5] != ELFDATA2LSB {
            return Err(ElfError::NotLittleEndian);
        }

        if data[6] != EV_CURRENT {
            return Err(ElfError::InvalidVersion);
        }

        let e_type = u16::from_le_bytes([data[16], data[17]]);
        let e_machine = u16::from_le_bytes([data[18], data[19]]);
        let e_version = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
        let e_entry = u64::from_le_bytes([
            data[24], data[25], data[26], data[27], data[28], data[29], data[30], data[31],
        ]);
        let e_phoff = u64::from_le_bytes([
            data[32], data[33], data[34], data[35], data[36], data[37], data[38], data[39],
        ]);
        let e_shoff = u64::from_le_bytes([
            data[40], data[41], data[42], data[43], data[44], data[45], data[46], data[47],
        ]);
        let e_flags = u32::from_le_bytes([data[48], data[49], data[50], data[51]]);
        let e_ehsize = u16::from_le_bytes([data[52], data[53]]);
        let e_phentsize = u16::from_le_bytes([data[54], data[55]]);
        let e_phnum = u16::from_le_bytes([data[56], data[57]]);
        let e_shentsize = u16::from_le_bytes([data[58], data[59]]);
        let e_shnum = u16::from_le_bytes([data[60], data[61]]);
        let e_shstrndx = u16::from_le_bytes([data[62], data[63]]);

        if e_type != ET_EXEC && e_type != ET_DYN {
            return Err(ElfError::NotExecutable);
        }

        if e_machine != EM_X86_64 {
            return Err(ElfError::WrongArchitecture);
        }

        if e_phoff == 0 {
            return Err(ElfError::InvalidPhdrOffset);
        }

        if e_phentsize < Self::PHDR_SIZE {
            return Err(ElfError::InvalidPhdrSize);
        }

        if e_phnum == 0 {
            return Err(ElfError::NoLoadSegments);
        }
        if e_phnum as usize > MAX_PROGRAM_HEADERS {
            return Err(ElfError::TooManyProgramHeaders);
        }

        Ok(Self {
            e_type,
            e_machine,
            e_version,
            e_entry,
            e_phoff,
            e_shoff,
            e_flags,
            e_ehsize,
            e_phentsize,
            e_phnum,
            e_shentsize,
            e_shnum,
            e_shstrndx,
        })
    }

    pub fn phdr_table_size(&self) -> usize {
        self.e_phnum as usize * self.e_phentsize as usize
    }

    pub fn validate_phdr_table(&self, file_size: usize) -> ElfResult<()> {
        let phdr_end = self
            .e_phoff
            .checked_add(self.phdr_table_size() as u64)
            .ok_or(ElfError::PhdrTableOverflow)?;

        if phdr_end > file_size as u64 {
            return Err(ElfError::PhdrTableOverflow);
        }

        Ok(())
    }

    pub fn is_pie(&self) -> bool {
        self.e_type == ET_DYN
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Elf64Phdr {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

impl Elf64Phdr {
    pub fn parse(data: &[u8]) -> ElfResult<Self> {
        if data.len() < 56 {
            return Err(ElfError::BufferTooSmall);
        }

        let p_type = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let p_flags = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let p_offset = u64::from_le_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]);
        let p_vaddr = u64::from_le_bytes([
            data[16], data[17], data[18], data[19], data[20], data[21], data[22], data[23],
        ]);
        let p_paddr = u64::from_le_bytes([
            data[24], data[25], data[26], data[27], data[28], data[29], data[30], data[31],
        ]);
        let p_filesz = u64::from_le_bytes([
            data[32], data[33], data[34], data[35], data[36], data[37], data[38], data[39],
        ]);
        let p_memsz = u64::from_le_bytes([
            data[40], data[41], data[42], data[43], data[44], data[45], data[46], data[47],
        ]);
        let p_align = u64::from_le_bytes([
            data[48], data[49], data[50], data[51], data[52], data[53], data[54], data[55],
        ]);

        Ok(Self {
            p_type,
            p_flags,
            p_offset,
            p_vaddr,
            p_paddr,
            p_filesz,
            p_memsz,
            p_align,
        })
    }

    pub fn is_load(&self) -> bool {
        self.p_type == PT_LOAD
    }

    pub fn is_readable(&self) -> bool {
        (self.p_flags & PF_R) != 0
    }

    pub fn is_writable(&self) -> bool {
        (self.p_flags & PF_W) != 0
    }

    pub fn is_executable(&self) -> bool {
        (self.p_flags & PF_X) != 0
    }

    pub fn end_address(&self) -> ElfResult<u64> {
        self.p_vaddr
            .checked_add(self.p_memsz)
            .ok_or(ElfError::SegmentSizeOverflow)
    }

    pub fn file_end(&self) -> ElfResult<u64> {
        self.p_offset
            .checked_add(self.p_filesz)
            .ok_or(ElfError::InvalidSegmentOffset)
    }

    pub fn aligned_start(&self) -> u64 {
        self.p_vaddr & !(PAGE_SIZE_4KB - 1)
    }

    pub fn aligned_end(&self) -> ElfResult<u64> {
        let end = self.end_address()?;
        Ok((end + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1))
    }
}

/// A fully validated PT_LOAD segment ready for mapping.
#[derive(Debug, Clone, Copy, slopos_ostd::Zeroable)]
#[repr(C)]
pub struct ValidatedSegment {
    /// Page-aligned virtual address start
    pub vaddr_start: u64,
    /// Page-aligned virtual address end
    pub vaddr_end: u64,
    pub file_offset: u64,
    pub file_size: u64,
    pub original_vaddr: u64,
    pub mem_size: u64,
    pub flags: u32,
}

impl ValidatedSegment {
    pub const ZERO: Self = Self {
        vaddr_start: 0,
        vaddr_end: 0,
        file_offset: 0,
        file_size: 0,
        original_vaddr: 0,
        mem_size: 0,
        flags: 0,
    };

    pub fn page_count(&self) -> u64 {
        (self.vaddr_end - self.vaddr_start) / PAGE_SIZE_4KB
    }
}

/// Performs every security check on an ELF file before returning structures
/// that are safe to use for loading.
///
/// `data` is only the header window; segment file extents are checked against
/// `file_len`, so a toolchain-sized image is never staged to be validated.
pub struct ElfValidator<'a> {
    data: &'a [u8],
    file_len: u64,
    header: Elf64Header,
    load_base: u64,
    budget: SegmentBudget,
}

/// What an image may still spend of the per-`exec` ceilings.
///
/// Carried rather than restarted, because an `exec` loads two images: an
/// executable and its interpreter would otherwise have a budget each.
#[derive(Debug, Clone, Copy)]
pub struct SegmentBudget {
    pub mapped: u64,
    pub zero_fill: u64,
}

impl SegmentBudget {
    pub const FULL: Self = Self {
        mapped: MAX_TOTAL_MAPPED_SIZE,
        zero_fill: MAX_TOTAL_ZERO_FILL_SIZE,
    };

    /// What is left after `segments` were accepted.
    pub fn less(self, segments: &[ValidatedSegment]) -> Self {
        let mut left = self;
        for seg in segments.iter() {
            left.mapped = left.mapped.saturating_sub(seg.vaddr_end - seg.vaddr_start);
            left.zero_fill = left
                .zero_fill
                .saturating_sub(seg.mem_size.saturating_sub(seg.file_size));
        }
        left
    }
}

impl<'a> ElfValidator<'a> {
    /// Validates the ELF header immediately. `data` must cover the header and
    /// the program-header table; `file_len` is the whole file's size.
    pub fn new(data: &'a [u8], file_len: u64) -> ElfResult<Self> {
        let header = Elf64Header::parse(data)?;
        header.validate_phdr_table(data.len())?;

        Ok(Self {
            data,
            file_len,
            header,
            load_base: 0,
            budget: SegmentBudget::FULL,
        })
    }

    pub fn with_load_base(mut self, base: u64) -> Self {
        self.load_base = base;
        self
    }

    pub fn with_budget(mut self, budget: SegmentBudget) -> Self {
        self.budget = budget;
        self
    }

    pub fn header(&self) -> &Elf64Header {
        &self.header
    }

    /// Writes the validated PT_LOAD segments into `out` and returns the count.
    ///
    /// `out` must hold at least `MAX_LOAD_SEGMENTS`; the caller owns the
    /// storage so the segment array never lands on the caller's stack.
    pub fn validate_load_segments_into(&self, out: &mut [ValidatedSegment]) -> ElfResult<usize> {
        if out.len() < MAX_LOAD_SEGMENTS {
            return Err(ElfError::TooManyLoadSegments);
        }
        for slot in out.iter_mut().take(MAX_LOAD_SEGMENTS) {
            *slot = ValidatedSegment::ZERO;
        }
        let mut count = 0usize;
        let mut total_size: u64 = 0;
        let mut total_zero_fill: u64 = 0;
        let mut total_file: u64 = 0;

        for i in 0..self.header.e_phnum as usize {
            let phdr = self.get_program_header(i)?;

            if !phdr.is_load() {
                continue;
            }

            if count >= MAX_LOAD_SEGMENTS {
                return Err(ElfError::TooManyLoadSegments);
            }

            let validated = self.validate_segment(&phdr)?;

            // Ascending and disjoint, on the segments' own addresses rather
            // than their page-aligned extents -- two segments legitimately
            // share a page. The gABI requires the order, and a consumer that
            // walks the array in table order (the loader's VMA pass does)
            // would otherwise leave a mapped page uncovered.
            if count > 0 {
                let prev = &out[count - 1];
                let prev_end = prev.original_vaddr.saturating_add(prev.mem_size);
                if validated.original_vaddr < prev_end {
                    return Err(ElfError::SegmentOverlap);
                }
            }

            let segment_size = validated.vaddr_end - validated.vaddr_start;
            total_size = total_size
                .checked_add(segment_size)
                .ok_or(ElfError::TotalSizeExceeded)?;

            if total_size > self.budget.mapped {
                return Err(ElfError::TotalSizeExceeded);
            }

            total_zero_fill = total_zero_fill
                .checked_add(validated.mem_size.saturating_sub(validated.file_size))
                .ok_or(ElfError::TotalSizeExceeded)?;

            if total_zero_fill > self.budget.zero_fill {
                return Err(ElfError::TotalSizeExceeded);
            }

            // No two segments may name the same file bytes. What this bounds
            // is the *read* volume: the loader streams `p_filesz` per
            // segment, so 64 segments each naming the whole of a 512 MiB file
            // would issue 32 GiB of reads, taking the mount lock every chunk,
            // from an unprivileged caller.
            total_file = total_file
                .checked_add(validated.file_size)
                .ok_or(ElfError::TotalSizeExceeded)?;
            if total_file > self.file_len {
                return Err(ElfError::AliasedFileExtent);
            }

            out[count] = validated;
            count += 1;
        }

        if count == 0 {
            return Err(ElfError::NoLoadSegments);
        }

        Ok(count)
    }

    /// Test-only; production code uses [`validate_load_segments_into`]. The
    /// array is heap-allocated so the caller's stack frame stays bounded.
    #[cfg(feature = "test-hooks")]
    pub fn validate_load_segments(
        &self,
    ) -> ElfResult<(
        slopos_ostd::KBox<[ValidatedSegment; MAX_LOAD_SEGMENTS]>,
        usize,
    )> {
        let mut segments: slopos_ostd::KBox<[ValidatedSegment; MAX_LOAD_SEGMENTS]> =
            slopos_ostd::KBox::zeroed().expect("test alloc");
        let count = self.validate_load_segments_into(&mut *segments)?;
        Ok((segments, count))
    }

    /// The entry point must fall within one of the loaded segments.
    pub fn validate_entry_point(&self, segments: &[ValidatedSegment]) -> ElfResult<u64> {
        let entry = self.adjusted_entry_point();

        let valid = segments
            .iter()
            .any(|seg| entry >= seg.vaddr_start && entry < seg.vaddr_end);

        if !valid {
            return Err(ElfError::EntryPointInvalid);
        }

        Ok(entry)
    }

    pub fn adjusted_entry_point(&self) -> u64 {
        if self.header.is_pie() {
            self.load_base.wrapping_add(self.header.e_entry)
        } else {
            self.header.e_entry
        }
    }

    fn get_program_header(&self, index: usize) -> ElfResult<Elf64Phdr> {
        if index >= self.header.e_phnum as usize {
            return Err(ElfError::InvalidPhdrOffset);
        }

        let offset = self.header.e_phoff as usize + index * self.header.e_phentsize as usize;
        let end = offset + 56;

        if end > self.data.len() {
            return Err(ElfError::PhdrTableOverflow);
        }

        Elf64Phdr::parse(&self.data[offset..end])
    }

    fn validate_segment(&self, phdr: &Elf64Phdr) -> ElfResult<ValidatedSegment> {
        let file_end = phdr.file_end()?;
        if file_end > self.file_len {
            return Err(ElfError::InvalidSegmentOffset);
        }

        if phdr.p_filesz > phdr.p_memsz {
            return Err(ElfError::FileSizeExceedsMemSize);
        }

        let vaddr_end = phdr.end_address()?;

        if phdr.p_align != 0 && !phdr.p_align.is_power_of_two() {
            return Err(ElfError::InvalidAlignment);
        }

        let vaddr = if self.header.is_pie() {
            self.load_base.wrapping_add(phdr.p_vaddr)
        } else {
            phdr.p_vaddr
        };

        let mem_end = if self.header.is_pie() {
            self.load_base.wrapping_add(vaddr_end)
        } else {
            vaddr_end
        };

        if vaddr >= KERNEL_SPACE_START_VA || mem_end > KERNEL_SPACE_START_VA {
            return Err(ElfError::KernelAddressViolation);
        }

        if vaddr < USER_SPACE_START_VA || mem_end > USER_SPACE_END_VA {
            return Err(ElfError::AddressOutOfBounds);
        }

        let aligned_start = vaddr & !(PAGE_SIZE_4KB - 1);
        let aligned_end = (mem_end + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1);

        if aligned_end < aligned_start {
            return Err(ElfError::SegmentSizeOverflow);
        }

        Ok(ValidatedSegment {
            vaddr_start: aligned_start,
            vaddr_end: aligned_end,
            file_offset: phdr.p_offset,
            file_size: phdr.p_filesz,
            original_vaddr: vaddr,
            mem_size: phdr.p_memsz,
            flags: phdr.p_flags,
        })
    }

    /// The strictest `p_align` any `PT_LOAD` asks for, never below a page.
    /// An `ET_DYN` placed at a base that is not a multiple of it has its
    /// alignment-sensitive data misaligned rather than refused.
    pub fn max_load_align(&self) -> ElfResult<u64> {
        let mut align = PAGE_SIZE_4KB;
        for i in 0..self.header.e_phnum as usize {
            let phdr = self.get_program_header(i)?;
            if phdr.is_load() && phdr.p_align > align {
                align = phdr.p_align;
            }
        }
        Ok(align)
    }

    /// `PT_INTERP`'s `(p_offset, p_filesz)`, the file extent holding the
    /// interpreter's NUL-terminated path.
    pub fn interpreter_extent(&self) -> ElfResult<Option<(u64, u64)>> {
        for i in 0..self.header.e_phnum as usize {
            let phdr = self.get_program_header(i)?;
            if phdr.p_type != PT_INTERP {
                continue;
            }
            let end = phdr.file_end()?;
            if phdr.p_filesz == 0
                || phdr.p_filesz > MAX_INTERP_PATH_LEN as u64
                || end > self.file_len
            {
                return Err(ElfError::InterpreterInvalid);
            }
            return Ok(Some((phdr.p_offset, phdr.p_filesz)));
        }
        Ok(None)
    }

    /// Returns PT_TLS's `(p_vaddr, p_filesz, p_memsz, p_align)` when present.
    pub fn find_tls_segment(&self) -> ElfResult<Option<(u64, u64, u64, u64)>> {
        let Some(phdr) = self.find_tls_program_header()? else {
            return Ok(None);
        };
        Ok(Some((
            phdr.p_vaddr,
            phdr.p_filesz,
            phdr.p_memsz,
            phdr.p_align,
        )))
    }

    /// Find PT_TLS and return file offset for the .tdata template.
    pub fn find_tls_offset(&self) -> ElfResult<Option<u64>> {
        Ok(self.find_tls_program_header()?.map(|phdr| phdr.p_offset))
    }

    fn find_tls_program_header(&self) -> ElfResult<Option<Elf64Phdr>> {
        for i in 0..self.header.e_phnum as usize {
            let phdr = self.get_program_header(i)?;
            if phdr.p_type == PT_TLS {
                return Ok(Some(phdr));
            }
        }
        Ok(None)
    }
}

/// `PT_INTERP`'s `(p_offset, p_filesz)` for an image the caller has only the
/// header window of, or `None` when the image is statically linked.
pub fn interpreter_extent(header: &[u8], file_len: u64) -> ElfResult<Option<(u64, u64)>> {
    ElfValidator::new(header, file_len)?.interpreter_extent()
}

/// Metadata collected during ELF loading, used to populate the auxiliary vector
/// on the user stack.
#[derive(Debug, Clone, Copy)]
pub struct ElfExecInfo {
    pub entry: u64,
    /// User-space address where program headers are mapped, or 0 if not mapped.
    pub phdr_addr: u64,
    pub phent_size: u16,
    pub phnum: u16,
    /// Size of the initialized TLS template (.tdata).
    pub tls_filesz: u64,
    /// Total static TLS size (.tdata + .tbss).
    pub tls_memsz: u64,
    pub tls_align: u64,
    pub tls_vaddr: u64,
    /// Load base of the `PT_INTERP` interpreter, or 0 when the image is
    /// statically linked. Handed to the process as `AT_BASE`.
    pub interp_base: u64,
    /// Where the interpreter is entered. Zero when there is none, in which
    /// case the process starts at [`Self::entry`].
    pub interp_entry: u64,
    /// Thread pointer (TCB address) to load into FS base.
    pub tls_tp: u64,
}
