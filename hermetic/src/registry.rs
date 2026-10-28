//! Linker-section registry of `HermeticState` impls: each emits a
//! `#[link_section = ".hermetic_state_registry"] static` of type
//! `HermeticVTable`, which the linker concatenates into a contiguous slice.

use slopos_ostd::{AllocError, KVec};

// The vtable lives in OSTD so `hermetic_state!` writes the same type into the
// section that this walker reads.
pub use slopos_ostd::test_support::hermetic::HermeticVTable;

/// Iterate every registered `HermeticVTable` in linker order, which depends on
/// translation-unit link order; `topo_order` is the capture-order source of
/// truth.
pub fn registry_iter() -> impl Iterator<Item = &'static HermeticVTable> {
    slopos_ostd::ffi::registry::registry_slice::<HermeticVTable>(
        slopos_ostd::ffi::registry::RegistryId::HermeticStates,
    )
    .iter()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    /// `DEPENDS_ON` graph contains a cycle.
    CycleDetected,
    /// A `DEPENDS_ON` entry names a state not present in the registry.
    MissingDep,
    /// Snapshot allocation failed during enter.
    Alloc,
}

impl From<AllocError> for RegistryError {
    fn from(_: AllocError) -> Self {
        Self::Alloc
    }
}

/// Topo-sort the registry by `DEPENDS_ON`: predecessors first, then
/// dependents. The scope captures snapshots in this order and restores in
/// reverse. `O(N²)` name lookup, bounded by N ≤ ~20 registered states.
pub fn topo_order() -> Result<KVec<&'static HermeticVTable>, RegistryError> {
    let entries: KVec<&'static HermeticVTable> = registry_iter().collect();
    let n = entries.len();

    for vt in entries.iter() {
        for dep_name in vt.depends_on {
            if !entries.iter().any(|other| other.name == *dep_name) {
                return Err(RegistryError::MissingDep);
            }
        }
    }

    let mut result: KVec<&'static HermeticVTable> = KVec::new();
    let mut emitted: KVec<bool> = (0..n).map(|_| false).collect();

    loop {
        let mut progress = false;
        for (i, vt) in entries.iter().enumerate() {
            if emitted[i] {
                continue;
            }
            let mut all_deps_ready = true;
            for dep_name in vt.depends_on {
                let mut dep_emitted = false;
                for (j, other) in entries.iter().enumerate() {
                    if other.name == *dep_name && emitted[j] {
                        dep_emitted = true;
                        break;
                    }
                }
                if !dep_emitted {
                    all_deps_ready = false;
                    break;
                }
            }
            if all_deps_ready {
                let _ = result.push(*vt);
                emitted[i] = true;
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }

    if result.len() != n {
        return Err(RegistryError::CycleDetected);
    }

    Ok(result)
}
