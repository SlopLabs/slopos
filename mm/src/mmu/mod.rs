//! Address-space surface: typed CR3 primitives, per-CPU ASID pools, lazy unmap
//! flush and PCID errata gating.
//!
//! Every CR3 write in the kernel funnels through [`cr3::write_cr3_value`].

pub mod asid;
pub mod cr3;
pub mod errata;
pub mod luf;
pub mod luf_hook;
pub mod quiesce;

pub use asid::{init_ap, init_bsp};
pub use cr3::{MmContextId, Pcid, alloc_mm_context_id, read_cr3_value};
