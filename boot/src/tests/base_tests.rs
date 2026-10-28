//! The choice between a boot entry's two bases, over archives built here, and
//! what the boot itself chose.

use slopos_abi::addr::PhysAddr;
use slopos_abi::fs::S_IFREG;
use slopos_boot_core::base::{INTERFACE_PATH, Reading, Which};
use slopos_boot_core::newc::Record;
use slopos_mm::memory_reservations::{
    MmRegionKind, MmReservationType, mm_region_count, mm_region_get,
};
use slopos_ostd::KVec;
use slopos_testing::{TestResult, assert_test};

use crate::base::{Choice, SERVED, choice, choose, unused_frames};

const UNSERVED: u32 = slopos_abi::BASE_INTERFACE + 1;

fn push(out: &mut KVec<u8>, record: &Record<'_>) -> Option<()> {
    for piece in record.pieces() {
        out.extend_from_slice(piece).ok()?;
    }
    Some(())
}

fn interface_line(mut n: u32, out: &mut [u8; 11]) -> &[u8] {
    let mut at = out.len() - 1;
    out[at] = b'\n';
    loop {
        at -= 1;
        out[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &out[at..];
        }
    }
}

fn base_naming(interface: Option<u32>) -> Option<KVec<u8>> {
    let mut out = KVec::new();
    let mut text = [0u8; 11];
    push(
        &mut out,
        &Record::new(b"/sbin/init", S_IFREG | 0o755, b"\x7fELF")?,
    )?;
    if let Some(n) = interface {
        let text = interface_line(n, &mut text);
        push(
            &mut out,
            &Record::new(INTERFACE_PATH.as_bytes(), S_IFREG | 0o644, text)?,
        )?;
    }
    push(&mut out, &Record::trailer())?;
    Some(out)
}

fn served() -> u32 {
    *SERVED.end()
}

pub fn test_base_current_boots_when_served() -> TestResult {
    let (Some(now), Some(before)) = (base_naming(Some(served())), base_naming(Some(served())))
    else {
        return TestResult::Fail;
    };
    match choose(Some(now.as_slice()), Some(before.as_slice())) {
        Choice::Boots {
            which: Which::Current,
            archive,
            interface,
            unused: Some((Which::Previous, Reading::Interface(i))),
        } => {
            assert_test!(archive.as_ptr() == now.as_slice().as_ptr());
            assert_test!(interface == served() && i == served());
            TestResult::Pass
        }
        other => {
            slopos_ostd::klog_info!("BASE_TEST: chose {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_base_previous_boots_when_current_is_not_served() -> TestResult {
    let (Some(now), Some(before)) = (base_naming(Some(UNSERVED)), base_naming(Some(served())))
    else {
        return TestResult::Fail;
    };
    let (Some(unnamed), Some(cut)) = (base_naming(None), base_naming(Some(served()))) else {
        return TestResult::Fail;
    };
    let cut = &cut.as_slice()[..cut.len() - 8];
    for current in [now.as_slice(), unnamed.as_slice(), cut] {
        match choose(Some(current), Some(before.as_slice())) {
            Choice::Boots {
                which: Which::Previous,
                archive,
                interface,
                unused: Some((Which::Current, reading)),
            } => {
                assert_test!(archive.as_ptr() == before.as_slice().as_ptr());
                assert_test!(interface == served());
                assert_test!(!matches!(reading, Reading::Interface(n) if SERVED.contains(&n)));
            }
            other => {
                slopos_ostd::klog_info!("BASE_TEST: chose {:?}", other);
                return TestResult::Fail;
            }
        }
    }
    TestResult::Pass
}

pub fn test_base_refused_when_neither_is_served() -> TestResult {
    let (Some(now), Some(before)) = (base_naming(Some(UNSERVED)), base_naming(None)) else {
        return TestResult::Fail;
    };
    let refused = |c: Choice<'_>| matches!(c, Choice::Refused { .. });
    assert_test!(refused(choose(
        Some(now.as_slice()),
        Some(before.as_slice())
    )));
    assert_test!(refused(choose(Some(now.as_slice()), None)));
    assert_test!(refused(choose(None, Some(before.as_slice()))));
    assert_test!(refused(choose(Some(b"not an archive"), None)));
    match choose(Some(now.as_slice()), None) {
        Choice::Refused {
            current: Reading::Interface(UNSERVED),
            previous: Reading::Absent,
        } => {}
        _ => return TestResult::Fail,
    }
    assert_test!(matches!(choose(None, None), Choice::NoBase));
    TestResult::Pass
}

pub fn test_base_alone_boots_with_nothing_left_unused() -> TestResult {
    let Some(only) = base_naming(Some(served())) else {
        return TestResult::Fail;
    };
    assert_test!(matches!(
        choose(Some(only.as_slice()), None),
        Choice::Boots {
            which: Which::Current,
            unused: None,
            ..
        }
    ));
    TestResult::Pass
}

/// The boot's own choice: a served base, and the pages of one it left unused,
/// on a two-base entry, usable memory the frame allocator tracks, or its own
/// metadata where it carved that out of them.
pub fn test_base_unused_pages_are_usable_memory() -> TestResult {
    if let Choice::Boots { interface, .. } = choice() {
        assert_test!(SERVED.contains(&interface));
    }
    let Some(frames) = unused_frames() else {
        return TestResult::Pass;
    };
    assert_test!(frames.start % 4096 == 0 && frames.end % 4096 == 0);
    let mut regions = KVec::new();
    for region in (0..mm_region_count()).filter_map(mm_region_get) {
        let held = region.kind == MmRegionKind::Usable
            || region.type_ == MmReservationType::AllocatorMetadata;
        if held && regions.push(region).is_err() {
            return TestResult::Fail;
        }
    }
    let uncovered = (frames.start..frames.end).step_by(4096).find(|&page| {
        !regions
            .iter()
            .any(|r| r.phys_base <= page && page < r.phys_base + r.length)
    });
    assert_test!(
        uncovered.is_none(),
        "a page of the unused base is no usable memory"
    );
    for page in [frames.start, frames.end - 4096] {
        assert_test!(slopos_mm::page_alloc::page_frame_is_tracked(PhysAddr::new(
            page
        )));
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_base_current_boots_when_served,
    suite = boot_base
);
slopos_testing::stest!(
    name = test_base_previous_boots_when_current_is_not_served,
    suite = boot_base
);
slopos_testing::stest!(
    name = test_base_refused_when_neither_is_served,
    suite = boot_base
);
slopos_testing::stest!(
    name = test_base_alone_boots_with_nothing_left_unused,
    suite = boot_base
);
slopos_testing::stest!(
    name = test_base_unused_pages_are_usable_memory,
    suite = boot_base
);
