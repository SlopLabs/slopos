use slopos_abi::task::{TaskPriority, TaskStatus};
use slopos_appkit::theme::words::Bytes;

use crate::syscall::UserTaskEntry;

pub(crate) fn task_name_bytes(task: &UserTaskEntry) -> &[u8] {
    let end = task
        .name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(task.name.len());
    &task.name[..end]
}

pub(crate) fn task_name_string(task: &UserTaskEntry) -> String {
    String::from_utf8_lossy(task_name_bytes(task)).into_owned()
}

pub(crate) fn c_text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// The most active first; no wildcard arm, so a new [`TaskStatus`] must be
/// placed here.
pub(crate) fn task_state(state: TaskStatus) -> (usize, &'static str) {
    match state {
        TaskStatus::Running => (0, "Running"),
        TaskStatus::Ready => (1, "Ready"),
        TaskStatus::Blocked => (2, "Waiting"),
        TaskStatus::Stopped => (3, "Stopped"),
        TaskStatus::Zombie => (4, "Ending"),
        TaskStatus::Terminated => (5, "Ended"),
        TaskStatus::Invalid => (6, "Unknown"),
    }
}

/// Only the kernel's idle loop carries `Idle`; no spawn may ask for it.
pub(crate) fn is_idle_task(task: &UserTaskEntry) -> bool {
    TaskPriority::from_u8(task.priority) == TaskPriority::Idle
}

/// Text padded out with NULs or spaces at either end, as CPUID gives it.
pub(crate) fn padded_text(bytes: &[u8]) -> String {
    let padding = |b: &u8| *b == 0 || *b == b' ';
    let start = bytes
        .iter()
        .position(|b| !padding(b))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !padding(b))
        .map_or(start, |last| last + 1);
    String::from_utf8_lossy(&bytes[start..end]).into_owned()
}

pub(crate) fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let clock = format!("{}:{:02}:{:02}", rem / 3600, (rem % 3600) / 60, rem % 60);
    match days {
        0 => clock,
        1 => format!("1 day, {clock}"),
        n => format!("{n} days, {clock}"),
    }
}

pub(crate) fn cpu_time(us: u64) -> String {
    let total_cs = us / 10_000;
    let cs = total_cs % 100;
    let total_sec = total_cs / 100;
    let h = total_sec / 3600;
    let m = (total_sec % 3600) / 60;
    let s = total_sec % 60;
    match h {
        0 => format!("{m:02}:{s:02}.{cs:02}"),
        _ => format!("{h}:{m:02}:{s:02}.{cs:02}"),
    }
}

pub(crate) fn percent(permille: u64) -> String {
    format!("{}%", (permille + 5) / 10)
}

pub(crate) fn percent_tenths(permille: u32) -> String {
    format!("{}.{}%", permille / 10, permille % 10)
}

pub(crate) fn rate(bytes_per_second: u64) -> String {
    format!("{}/s", Bytes(bytes_per_second))
}

pub(crate) fn ghz(mhz: u64) -> String {
    let centi = (mhz + 5) / 10;
    format!("{}.{:02} GHz", centi / 100, centi % 100)
}

pub(crate) fn number(n: u64) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len.saturating_sub(1) / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub(crate) fn count(n: u64, one: &str, many: &str) -> String {
    match n {
        1 => format!("1 {one}"),
        n => format!("{} {many}", number(n)),
    }
}

pub(crate) fn cpu_features(features: u64) -> String {
    let ecx = features as u32;
    let edx = (features >> 32) as u32;
    let named: [(bool, &str); 11] = [
        (edx & (1 << 25) != 0, "SSE"),
        (edx & (1 << 26) != 0, "SSE2"),
        (ecx & (1 << 0) != 0, "SSE3"),
        (ecx & (1 << 9) != 0, "SSSE3"),
        (ecx & (1 << 19) != 0, "SSE4.1"),
        (ecx & (1 << 20) != 0, "SSE4.2"),
        (ecx & (1 << 28) != 0, "AVX"),
        (ecx & (1 << 26) != 0, "XSAVE"),
        (ecx & (1 << 21) != 0, "x2APIC"),
        (edx & (1 << 9) != 0, "APIC"),
        (edx & (1 << 4) != 0, "TSC"),
    ];
    let parts: Vec<&str> = named
        .iter()
        .filter(|(held, _)| *held)
        .map(|(_, name)| *name)
        .collect();
    match parts.is_empty() {
        true => format!("0x{features:016x}"),
        false => parts.join(" "),
    }
}
