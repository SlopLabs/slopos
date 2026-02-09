//! Re-export of SwitchContext from task_struct.
//!
//! The canonical `SwitchContext` is defined in `super::task_struct`.
//! Assembly code in switch_asm.rs uses `offset_of!()` directly for compile-time safety.

pub use super::task_struct::SwitchContext;
