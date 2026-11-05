//! The shard split against a registry of known shape: every selected test
//! lands in exactly one shard, a kernel module never straddles two, `order =
//! first` tests open every shard, `order = last` ones close theirs, and a
//! fixture is never a test.

use slopos_ostd::KVec;

use super::{select_tests, slice_of};
use crate::config::TestConfig;
use crate::registry::TestKind::{Kernel, Userland};
use crate::registry::TestOrder::{First, Last, Sorted};
use crate::registry::{TestDesc, TestKind, TestOrder, FLAG_FIXTURE};
use crate::TestResult;

fn never_run() -> TestResult {
    TestResult::Fail
}

const fn desc(
    module: &'static str,
    name: &'static str,
    kind: TestKind,
    order: TestOrder,
    flags: u32,
) -> TestDesc {
    TestDesc {
        name,
        module,
        file: file!(),
        line: 0,
        run: never_run,
        kind,
        order,
        rank: 0,
        flags,
        bin: None,
        argv: &[],
    }
}

static K_FIRST: TestDesc = desc("k::boot", "first", Kernel, First, 0);
static K_FIXTURE: TestDesc = desc("k::fs", "ready", Kernel, First, FLAG_FIXTURE);
static K_A1: TestDesc = desc("k::a", "one", Kernel, Sorted, 0);
static K_A2: TestDesc = desc("k::a", "two", Kernel, Sorted, 0);
static K_A3: TestDesc = desc("k::a", "three", Kernel, Sorted, 0);
static K_B1: TestDesc = desc("k::b", "one", Kernel, Sorted, 0);
static K_C1: TestDesc = desc("k::c", "one", Kernel, Sorted, 0);
static K_C2: TestDesc = desc("k::c", "two", Kernel, Sorted, 0);
static K_D1: TestDesc = desc("k::d", "one", Kernel, Sorted, 0);
static K_E1: TestDesc = desc("k::e", "one", Kernel, Sorted, 0);
static K_LAST: TestDesc = desc("k::b", "audit", Kernel, Last, 0);
static U_FIRST: TestDesc = desc("u", "first", Userland, First, 0);
static U_1: TestDesc = desc("u", "a", Userland, Sorted, 0);
static U_2: TestDesc = desc("u", "b", Userland, Sorted, 0);
static U_3: TestDesc = desc("u", "c", Userland, Sorted, 0);
static U_4: TestDesc = desc("u", "d", Userland, Sorted, 0);
static U_5: TestDesc = desc("u", "e", Userland, Sorted, 0);

/// In `registry_sorted` order.
static REGISTRY: [&TestDesc; 17] = [
    &K_FIRST, &K_FIXTURE, &U_FIRST, &K_A1, &K_A3, &K_A2, &K_B1, &K_C1, &K_C2, &K_D1, &K_E1, &U_1,
    &U_2, &U_3, &U_4, &U_5, &K_LAST,
];

const MAX_SHARDS: u32 = 7;

fn position(list: &[&'static TestDesc], d: &TestDesc) -> Option<usize> {
    list.iter().position(|&x| core::ptr::eq(x, d))
}

fn check_kind(kind: TestKind) -> TestResult {
    let cfg = TestConfig {
        enabled: true,
        ..TestConfig::default()
    };
    let Ok(whole) = select_tests(&REGISTRY, &cfg, kind, None) else {
        return crate::fail!("selection allocation failed");
    };
    if whole.iter().any(|d| d.is_fixture() || d.kind != kind) {
        return crate::fail!("a fixture or another phase's test was selected");
    }
    for count in 1..=MAX_SHARDS {
        let mut shards: KVec<KVec<&'static TestDesc>> = KVec::new();
        for index in 1..=count {
            let Ok(part) = select_tests(&REGISTRY, &cfg, kind, Some((index, count))) else {
                return crate::fail!("selection allocation failed");
            };
            if shards.push(part).is_err() {
                return crate::fail!("allocation failed");
            }
        }
        for (k, part) in shards.iter().enumerate() {
            if check_shard_order(&whole, part, k + 1, count) == TestResult::Fail {
                return TestResult::Fail;
            }
        }
        if check_partition(&whole, &shards, kind, count) == TestResult::Fail {
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Shard `index` opens with every first test, runs the rest in the
/// unsharded order, and closes with its last ones.
#[inline(never)]
fn check_shard_order(
    whole: &[&'static TestDesc],
    part: &[&'static TestDesc],
    index: usize,
    count: u32,
) -> TestResult {
    let firsts = whole.iter().filter(|d| d.order == First).count();
    if part.len() < firsts || part[..firsts].iter().any(|d| d.order != First) {
        return crate::fail!(
            "shard {}/{} does not open with every first test",
            index,
            count
        );
    }
    let mut prev = None;
    for &d in part.iter() {
        let Some(at) = position(whole, d) else {
            return crate::fail!("shard {}/{} runs an unselected test", index, count);
        };
        if prev.is_some_and(|p| p >= at) {
            return crate::fail!("shard {}/{} reorders {}", index, count, d.name);
        }
        prev = Some(at);
    }
    if let Some(i) = part.iter().position(|d| d.order == Last) {
        if part[i..].iter().any(|d| d.order != Last) {
            return crate::fail!("shard {}/{} runs a test after a last one", index, count);
        }
    }
    TestResult::Pass
}

/// Every test outside `order = first` runs in exactly one shard, and a kernel
/// module's tests all run in the same one.
#[inline(never)]
fn check_partition(
    whole: &[&'static TestDesc],
    shards: &[KVec<&'static TestDesc>],
    kind: TestKind,
    count: u32,
) -> TestResult {
    let home = |d: &TestDesc| shards.iter().position(|s| position(s, d).is_some());
    for &d in whole.iter().filter(|d| d.order != First) {
        let homes = shards.iter().filter(|s| position(s, d).is_some()).count();
        if homes != 1 {
            return crate::fail!("{} runs in {} of {} shards, want 1", d.name, homes, count);
        }
        let split = kind == Kernel
            && whole
                .iter()
                .any(|&o| o.module == d.module && o.order != First && home(o) != home(d));
        if split {
            return crate::fail!("module {} straddles shards at {}", d.module, count);
        }
    }
    TestResult::Pass
}

pub fn test_kernel_shards_keep_modules_whole() -> TestResult {
    check_kind(Kernel)
}

pub fn test_userland_shards_partition_tests() -> TestResult {
    check_kind(Userland)
}

/// Single items split into `count` contiguous runs whose sizes differ by at
/// most one.
pub fn test_shard_slices_are_balanced() -> TestResult {
    for total in 1..=40usize {
        for count in 1..=MAX_SHARDS {
            let mut sizes = [0usize; MAX_SHARDS as usize];
            let mut last = 0u32;
            for pos in 0..total {
                let s = slice_of(pos, 1, total, count);
                if s >= count || s < last {
                    return crate::fail!(
                        "total={} count={} pos={} -> slice {}",
                        total,
                        count,
                        pos,
                        s
                    );
                }
                last = s;
                sizes[s as usize] += 1;
            }
            let used = &sizes[..count as usize];
            let max = used.iter().copied().max().unwrap_or(0);
            let min = used.iter().copied().min().unwrap_or(0);
            if max - min > 1 {
                return crate::fail!("total={} count={} sizes {:?}", total, count, used);
            }
        }
    }
    TestResult::Pass
}

crate::stest!(name = test_kernel_shards_keep_modules_whole);
crate::stest!(name = test_userland_shards_partition_tests);
crate::stest!(name = test_shard_slices_are_balanced);
