//! The interface number that ties a base to the kernels that can run it.
//!
//! A base is built against one number; a kernel serves a range of them.
//! `xtask base-interface` holds the call surface — the private syscalls and
//! every structure they take, and the Linux-numbered calls the kernel answers
//! — to the number, so the surface cannot change while the number stays.

/// The interface of this tree's call surface. Any change to a SlopOS-only
/// call's number, arguments or structures, or to which Linux calls the kernel
/// answers, takes the next number.
pub const BASE_INTERFACE: u32 = 13;

/// The oldest interface a kernel built from this tree still serves. Before v1
/// that is [`BASE_INTERFACE`]: a kernel serves the base of its own tree alone,
/// so a private call may change shape or go with the next number. From v1 it
/// is a number of its own, raised only when a change reshapes something an
/// older base calls.
pub const BASE_INTERFACE_MIN: u32 = BASE_INTERFACE;

const _: () = assert!(0 < BASE_INTERFACE_MIN && BASE_INTERFACE_MIN <= BASE_INTERFACE);

/// The type of [`BaseInterfaceNote`] among notes owned by `SlopOS`.
pub const NT_SLOPOS_BASE_INTERFACE: u32 = 1;

/// The ELF note a kernel carries in `.note.slopos.base-interface`: the range
/// of base interfaces it serves, readable from the file without booting it.
#[repr(C)]
pub struct BaseInterfaceNote {
    pub namesz: u32,
    pub descsz: u32,
    pub kind: u32,
    pub name: [u8; 8],
    pub min: u32,
    pub max: u32,
}

impl BaseInterfaceNote {
    pub const SERVED: Self = Self {
        namesz: b"SlopOS\0".len() as u32,
        descsz: core::mem::size_of::<[u32; 2]>() as u32,
        kind: NT_SLOPOS_BASE_INTERFACE,
        name: *b"SlopOS\0\0",
        min: BASE_INTERFACE_MIN,
        max: BASE_INTERFACE,
    };
}

const _: () = assert!(core::mem::size_of::<BaseInterfaceNote>() == 28);
