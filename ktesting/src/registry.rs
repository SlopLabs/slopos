//! Per-test registry walked by the harness.
//!
//! Every `stest!`, `utest!` and `fixture!` emits a static `TestDesc` into the
//! `.test_registry` linker section; the harness reads the section, sorts it by
//! `(order, module, rank, name)`, then runs each entry. Nothing depends on how
//! a name sorts: a test that must run ahead of others says so with `order` or
//! `rank`, and shared setup is a fixture.

use core::cmp::Ordering;

use slopos_ostd::{AllocError, KVec};

use crate::result::TestResult;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestKind {
    Kernel = 0,
    Userland = 1,
}

/// Where a test runs within its phase.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TestOrder {
    /// Ahead of every other test, in every shard; a failure bails the phase,
    /// so broken plumbing stops the run before anything spends time on it.
    First = 0,
    Sorted = 1,
    /// After every `First` and `Sorted` test of its shard.
    Last = 2,
}

/// Compile-time descriptor for one test entry. `bin`/`argv` are set by `utest!`
/// and ignored by kernel tests.
///
/// `order` and `rank` sit in what was padding after `kind`, so the entry stays
/// 104 bytes, the stride the ELF gates walk the section with.
#[repr(C)]
pub struct TestDesc {
    pub name: &'static str,
    pub module: &'static str,
    pub file: &'static str,
    pub line: u32,
    pub run: fn() -> TestResult,
    pub kind: TestKind,
    pub order: TestOrder,
    /// Within one module and order, every test of a lower rank runs first: the
    /// declared form of "this test reads what those left behind".
    pub rank: u8,
    pub flags: u32,
    pub bin: Option<&'static str>,
    pub argv: &'static [&'static str],
}

const _: () = assert!(core::mem::size_of::<TestDesc>() == 104);

/// `flags` bit: a panic from this test is reported as Pass with an
/// `EXPECTED_PANIC` suffix.
pub const FLAG_EXPECTED_PANIC: u32 = 0x1;

/// `flags` bit: the test's klog reaches the console as it is written. The
/// capture rings keep a test's first kilobytes, which for one that runs for
/// hours is none of what explains its failure.
pub const FLAG_UNCAPTURED: u32 = 0x2;

/// `flags` bit: the test ends the machine, so it runs only when a
/// `tests.run` pattern is its full name, never under a wildcard.
pub const FLAG_EXPLICIT: u32 = 0x4;

/// `flags` bit: the entry is a `fixture!`, not a test. It runs before the
/// first test of its phase whenever that phase runs any, whatever the filter
/// or shard, and is neither counted nor reported as a test.
pub const FLAG_FIXTURE: u32 = 0x8;

impl TestDesc {
    pub fn is_fixture(&self) -> bool {
        self.flags & FLAG_FIXTURE != 0
    }
}

impl slopos_ostd::ffi::registry::RegistryEntry for TestDesc {
    const REGISTRIES: &'static [slopos_ostd::ffi::registry::RegistryId] =
        &[slopos_ostd::ffi::registry::RegistryId::Tests];
}

pub fn registry_iter() -> impl Iterator<Item = &'static TestDesc> {
    slopos_ostd::ffi::registry::registry_slice::<TestDesc>(
        slopos_ostd::ffi::registry::RegistryId::Tests,
    )
    .iter()
}

fn cmp_desc(a: &&TestDesc, b: &&TestDesc) -> Ordering {
    a.order
        .cmp(&b.order)
        .then_with(|| a.module.as_bytes().cmp(b.module.as_bytes()))
        .then_with(|| a.rank.cmp(&b.rank))
        .then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
}

/// One bottom-up merge pass: merge each adjacent pair of `width`-sized sorted
/// runs from `src` into `dst`. Stable — ties resolve to the left run.
fn merge_pass(src: &[&'static TestDesc], dst: &mut [&'static TestDesc], width: usize) {
    let len = src.len();
    let mut start = 0usize;
    while start < len {
        let mid = core::cmp::min(start + width, len);
        let end = core::cmp::min(start + 2 * width, len);
        let (mut i, mut j) = (start, mid);
        let mut k = start;
        while k < end {
            let take_left = if i >= mid {
                false
            } else if j >= end {
                true
            } else {
                cmp_desc(&src[j], &src[i]) != Ordering::Less
            };
            if take_left {
                dst[k] = src[i];
                i += 1;
            } else {
                dst[k] = src[j];
                j += 1;
            }
            k += 1;
        }
        start = end;
    }
}

/// Sort every registry entry by `(order, module, rank, name)`. Bottom-up merge
/// sort: the registry holds thousands of entries and an O(n^2) sort would cost
/// more than the run it orders.
pub fn registry_sorted() -> Result<KVec<&'static TestDesc>, AllocError> {
    let mut src: KVec<&'static TestDesc> = KVec::new();
    for desc in registry_iter() {
        src.push(desc)?;
    }
    let len = src.len();
    if len < 2 {
        return Ok(src);
    }

    let mut dst: KVec<&'static TestDesc> = KVec::with_capacity(len)?;
    for &desc in src.iter() {
        dst.push(desc)?;
    }

    let mut width = 1usize;
    let mut sorted_in_src = true;
    while width < len {
        if sorted_in_src {
            merge_pass(&src, &mut dst, width);
        } else {
            merge_pass(&dst, &mut src, width);
        }
        sorted_in_src = !sorted_in_src;
        width *= 2;
    }

    Ok(if sorted_in_src { src } else { dst })
}
