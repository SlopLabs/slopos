use slopos_abi::task::{TaskPriority, TaskStatus};

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

/// Its name and its place in the order the overview's rows and the State
/// column keep. No wildcard arm, so a new [`TaskStatus`] is a build error
/// until it has both.
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

pub(crate) fn priority_label(priority: u8) -> &'static str {
    match TaskPriority::from_u8(priority) {
        TaskPriority::High => "High",
        TaskPriority::KernelIo => "Above normal",
        TaskPriority::Normal => "Normal",
        TaskPriority::Low => "Low",
        TaskPriority::Idle => "Idle",
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

pub(crate) fn format_uptime(ms: u64) -> String {
    let total_sec = ms / 1000;
    let days = total_sec / 86_400;
    let rem = total_sec % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    format!("{}d {:02}:{:02}:{:02}", days, h, m, s)
}

pub(crate) fn format_runtime(us: u64) -> String {
    let total_cs = us / 10_000;
    let cs = total_cs % 100;
    let total_sec = total_cs / 100;
    let h = total_sec / 3600;
    let m = (total_sec % 3600) / 60;
    let s = total_sec % 60;
    if h == 0 {
        format!("{:02}:{:02}.{:02}", m, s, cs)
    } else {
        format!("{}:{:02}:{:02}.{:02}", h, m, s, cs)
    }
}

pub(crate) fn format_pct(pct_x10: u32) -> String {
    format!("{}.{}%", pct_x10 / 10, pct_x10 % 10)
}

pub(crate) fn format_number(n: u64) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + ((len.saturating_sub(1)) / 3));
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub(crate) fn format_cpu_features(features: u64) -> String {
    let ecx = features as u32;
    let edx = (features >> 32) as u32;
    let mut parts: String = String::new();
    let mut push_part = |part: &str| {
        if !parts.is_empty() {
            parts.push(' ');
        }
        parts.push_str(part);
    };
    if edx & (1 << 25) != 0 {
        push_part("SSE");
    }
    if edx & (1 << 26) != 0 {
        push_part("SSE2");
    }
    if ecx & (1 << 0) != 0 {
        push_part("SSE3");
    }
    if ecx & (1 << 9) != 0 {
        push_part("SSSE3");
    }
    if ecx & (1 << 19) != 0 {
        push_part("SSE4.1");
    }
    if ecx & (1 << 20) != 0 {
        push_part("SSE4.2");
    }
    if ecx & (1 << 28) != 0 {
        push_part("AVX");
    }
    if ecx & (1 << 26) != 0 {
        push_part("XSAVE");
    }
    if ecx & (1 << 21) != 0 {
        push_part("x2APIC");
    }
    if edx & (1 << 9) != 0 {
        push_part("APIC");
    }
    if edx & (1 << 4) != 0 {
        push_part("TSC");
    }
    if parts.is_empty() {
        return format!("0x{:016x}", features);
    }
    parts
}
