//! KTAP-grammar emitter.
//!
//! Every line carries the literal `KTAP\t` prefix so a host parser can ignore
//! interleaved klog; diagnostic YAML blocks add two spaces after the tab.

use slopos_ostd::klog_info;

use crate::capture::write_through;
use crate::registry::TestDesc;
use crate::result::TestResult;

/// Holds `time_ms=N`'s position so a trailing directive keeps the space the host parser needs.
const NO_TIME_BASE: &str = "NO_TIME_BASE";

pub fn emit_header(plan: u32) {
    klog_info!("KTAP\tTAP version 14");
    klog_info!("KTAP\t1..{}", plan);
}

pub fn emit_ok(idx: u32, desc: &TestDesc, time_ms: Option<u32>, suffix: Option<&str>) {
    match (time_ms, suffix) {
        (Some(ms), Some(s)) => klog_info!(
            "KTAP\tok {} - {}::{} # time_ms={} {}",
            idx,
            desc.module,
            desc.name,
            ms,
            s,
        ),
        (Some(ms), None) => klog_info!(
            "KTAP\tok {} - {}::{} # time_ms={}",
            idx,
            desc.module,
            desc.name,
            ms,
        ),
        (None, Some(s)) => klog_info!(
            "KTAP\tok {} - {}::{} # {} {}",
            idx,
            desc.module,
            desc.name,
            NO_TIME_BASE,
            s,
        ),
        (None, None) => klog_info!(
            "KTAP\tok {} - {}::{} # {}",
            idx,
            desc.module,
            desc.name,
            NO_TIME_BASE,
        ),
    }
}

pub fn emit_skip(idx: u32, desc: &TestDesc, reason: &str) {
    klog_info!(
        "KTAP\tok {} - {}::{} # SKIP {}",
        idx,
        desc.module,
        desc.name,
        reason
    );
}

pub fn emit_not_ok(idx: u32, desc: &TestDesc, time_ms: Option<u32>, outcome: TestResult) {
    match time_ms {
        Some(ms) => klog_info!(
            "KTAP\tnot ok {} - {}::{} # time_ms={}",
            idx,
            desc.module,
            desc.name,
            ms,
        ),
        None => klog_info!(
            "KTAP\tnot ok {} - {}::{} # {}",
            idx,
            desc.module,
            desc.name,
            NO_TIME_BASE,
        ),
    }
    klog_info!("KTAP\t  ---");
    klog_info!("KTAP\t  outcome: {:?}", outcome);
    klog_info!("KTAP\t  file: {}:{}", desc.file, desc.line);
}

pub fn emit_footer(elapsed_ms: u32, pass: u32, fail: u32, skip: u32, over_time: u32) {
    klog_info!(
        "KTAP\t# elapsed_ms={} pass={} fail={} skip={} over_time={}",
        elapsed_ms,
        pass,
        fail,
        skip,
        over_time
    );
}

pub fn emit_bail(reason: &str) {
    klog_info!("KTAP\tBail out! {}", reason);
}

// Subtest lines reach the wire *before* their parent's `ok`/`not ok` line; the
// two-space indent is what keys the host parser into nested mode.

/// Pass subtest line. `sub_idx` is the 1-based position within the parent;
/// a note rides after `#`, which the host parser reads as a pass unless it is
/// `SKIP`.
pub fn emit_subtest_ok(sub_idx: u32, name: &str, note: &str) {
    if crate::kernel_phase_summary::quiet() {
        return;
    }
    if note.is_empty() {
        write_through(format_args!("KTAP\t  ok {} - {}", sub_idx, name));
    } else {
        write_through(format_args!("KTAP\t  ok {} - {} # {}", sub_idx, name, note));
    }
}

pub fn emit_subtest_not_ok(sub_idx: u32, name: &str, msg: &str) {
    if msg.is_empty() {
        write_through(format_args!("KTAP\t  not ok {} - {}", sub_idx, name));
    } else {
        write_through(format_args!(
            "KTAP\t  not ok {} - {} # {}",
            sub_idx, name, msg
        ));
    }
}

/// Skip subtest line. KTAP encodes skips as `ok` with a `# SKIP` suffix.
pub fn emit_subtest_skip(sub_idx: u32, name: &str) {
    if crate::kernel_phase_summary::quiet() {
        return;
    }
    write_through(format_args!("KTAP\t  ok {} - {} # SKIP", sub_idx, name));
}
