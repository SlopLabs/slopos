use slopos_userland as _;

use slopos_abi::net::{NET_IFKIND_ETHERNET, NET_OPER_UP, UserIface};
use slopos_abi::syscall::BLOCK_TRANSPORT_NVME;
use slopos_abi::syscall::types::{
    DISPLAY_ADAPTER_VIRTIO_GPU, UserBlockStats, UserCpuInfo, UserDisplayStats, UserPerCpuStats,
    UserProcessMemory, UserSysInfo, UserTaskEntry,
};
use slopos_abi::task::{INVALID_PROCESS_ID, TaskStatus};
use slopos_appkit::{App, Node};
use slopos_userland::apps::sysmon::monitor::{Monitor, Process, WHOLE};
use slopos_userland::apps::sysmon::sample::{Disk, Iface, Sample};
use slopos_userland::apps::sysmon::selection::{TaskKey, index_of, key_at_row, row_of};
use slopos_userland::apps::sysmon::{SysmonApp, SysmonMsg};

const MIB: u64 = 1024 * 1024;

fn process(pid: u32, started_ms: u64) -> Process {
    Process {
        key: TaskKey { pid, started_ms },
        name: format!("app{pid}"),
        uid: 1000,
        kernel: false,
        threads: 1,
        cpu: 0,
        memory: None,
        cpu_time_us: 0,
        state: TaskStatus::Blocked,
    }
}

fn test_selection_follows_process_across_resort() -> bool {
    let processes = [process(10, 1), process(20, 2), process(30, 3)];
    let Some(key) = key_at_row(&processes, &[0, 1, 2], 0) else {
        return false;
    };
    key.pid == 10 && row_of(key, &processes, &[2, 1, 0]) == Some(2)
}

fn test_recycled_pid_is_not_the_same_process() -> bool {
    let before = [process(13, 100)];
    let after = [process(13, 900)];
    let key = before[0].key;
    row_of(key, &after, &[0]).is_none() && index_of(key, &after).is_none()
}

fn test_filtered_out_process_has_no_row() -> bool {
    let processes = [process(10, 1), process(20, 2)];
    let key = processes[1].key;
    index_of(key, &processes) == Some(1)
        && row_of(key, &processes, &[0]).is_none()
        && key_at_row(&processes, &[0], 5).is_none()
}

fn test_every_ending_is_said_as_a_sentence() -> bool {
    use slopos_appkit::theme::words::sentence;
    use slopos_userland::apps::sysmon::{KillOutcome, status_line};
    let name = || String::from("terminal");
    let refusals = [
        KillOutcome::Powerful { name: name() },
        KillOutcome::System { name: name() },
        KillOutcome::Unowned { name: name() },
        KillOutcome::Refused { name: name() },
    ];
    let rest = [
        KillOutcome::Sent { name: name() },
        KillOutcome::Failed { name: name() },
        KillOutcome::Vanished { name: name() },
        KillOutcome::Asking { name: name() },
        KillOutcome::Denied {
            name: name(),
            why: Some(String::from("You didn't allow this.")),
            by_person: true,
        },
        KillOutcome::Denied {
            name: name(),
            why: None,
            by_person: false,
        },
    ];
    let said = |outcome: Option<&KillOutcome>| status_line(outcome).0;
    let mut refused: Vec<String> = refusals.iter().map(|o| said(Some(o))).collect();
    let every = refused
        .iter()
        .cloned()
        .chain(rest.iter().map(|o| said(Some(o))))
        .chain([said(None)]);
    let mut kept = true;
    for line in every {
        if sentence(&line, &[]).is_err() {
            eprintln!("sysmon_test: {line:?}");
            kept = false;
        }
    }
    refused.sort_unstable();
    refused.dedup();
    kept && refused.len() == refusals.len()
}

fn task(task_id: u32, process_id: u32, runtime_us: u64) -> UserTaskEntry {
    let mut name = [0u8; 32];
    let label = format!("task{task_id}");
    name[..label.len()].copy_from_slice(label.as_bytes());
    UserTaskEntry {
        task_id,
        process_id,
        creation_time_ms: 10,
        total_runtime_us: runtime_us,
        uid: 1000,
        state: TaskStatus::Blocked.as_u8(),
        name,
        ..UserTaskEntry::default()
    }
}

fn padded<const N: usize>(text: &str) -> [u8; N] {
    let mut out = [0u8; N];
    out[..text.len()].copy_from_slice(text.as_bytes());
    out
}

fn sample(second: u64, step: u64) -> Sample {
    let at = second * step;
    let mut cpu_info = UserCpuInfo::default();
    cpu_info.cpu_count = 2;
    cpu_info.vendor = padded("GenuineIntel");
    cpu_info.brand_string = padded("Test CPU at 2.00GHz");
    Sample {
        at_ms: 1000 + second * 1000,
        sys: UserSysInfo {
            total_pages: 512 * 1024 / 4,
            free_pages: 256 * 1024 / 4,
            allocated_pages: 256 * 1024 / 4,
            ..UserSysInfo::default()
        },
        cpu_info,
        cpus: vec![
            UserPerCpuStats {
                cpu_id: 0,
                total_ticks: 100 * at,
                idle_ticks: 50 * at,
                ..UserPerCpuStats::default()
            },
            UserPerCpuStats {
                cpu_id: 1,
                total_ticks: 100 * at,
                idle_ticks: 100 * at,
                ..UserPerCpuStats::default()
            },
        ],
        tasks: vec![
            task(10, 2, 250_000 * at),
            task(11, 2, 250_000 * at),
            task(2, INVALID_PROCESS_ID, 0),
        ],
        memory: vec![UserProcessMemory {
            process_id: 2,
            resident_pages: 100,
            ..UserProcessMemory::default()
        }],
        ifaces: Some(vec![Iface {
            iface: UserIface {
                ifindex: 2,
                kind: NET_IFKIND_ETHERNET,
                oper_state: NET_OPER_UP,
                name: padded("eth0"),
                mtu: 1500,
                rx_bytes: MIB * at,
                tx_bytes: MIB / 2 * at,
                ..UserIface::default()
            },
            addr: Some(([10, 0, 2, 15], 24)),
        }]),
        disks: vec![Disk {
            stats: UserBlockStats {
                name: padded("nvme0n1"),
                capacity_bytes: 64 * 1000 * 1000 * 1000,
                logical_block_size: 512,
                transport: BLOCK_TRANSPORT_NVME,
                read_bytes: 2 * MIB * at,
                read_requests: 16 * at,
                busy_ns: 250_000_000 * at,
                ..UserBlockStats::default()
            },
            model: Some(String::from("QEMU NVMe Ctrl")),
        }],
        display: Some(UserDisplayStats {
            adapter: DISPLAY_ADAPTER_VIRTIO_GPU,
            width: 1280,
            height: 800,
            bytes_per_pixel: 4,
            scanout_bytes: 1280 * 800 * 4,
            frames: 60 * at,
            present_ns: 100_000_000 * at,
            ..UserDisplayStats::default()
        }),
        ..Sample::default()
    }
}

fn monitor(seconds: u64) -> Monitor {
    let mut monitor = Monitor::default();
    for second in 0..seconds {
        monitor.record(sample(second, 1));
    }
    monitor
}

fn test_figures_are_what_moved_between_samples() -> bool {
    let first = monitor(1);
    if first.intervals != 0 || !first.cpu.busy.values().is_empty() {
        eprintln!("sysmon_test: one sample drew a figure");
        return false;
    }
    let m = monitor(2);
    let adapter = &m.adapters[0];
    let disk = &m.disks[0];
    let display = m.display.as_ref();
    let got = [
        ("CPU busy", m.cpu.busy.last(), WHOLE / 4),
        ("CPU 0 busy", m.cpu.processors[0].busy.last(), WHOLE / 2),
        ("CPU 1 busy", m.cpu.processors[1].busy.last(), 0),
        ("received", adapter.received.last(), MIB),
        ("sent", adapter.sent.last(), MIB / 2),
        ("disk active", disk.active.last(), WHOLE / 4),
        ("disk read", disk.read.last(), 2 * MIB),
        ("disk written", disk.written.last(), 0),
        ("GPU busy", display.map_or(0, |d| d.busy.last()), WHOLE / 10),
        ("frames", display.map_or(0, |d| d.frames.last()), 60),
        ("memory", m.memory.last(), 256 * MIB),
    ];
    let mut kept = true;
    for (what, got, want) in got {
        if got != want {
            eprintln!("sysmon_test: {what} reads {got}, not {want}");
            kept = false;
        }
    }
    kept
}

/// Kernel task ids overlap process ids, so neither may stand for the other.
fn test_threads_fold_into_their_process() -> bool {
    let m = monitor(2);
    let Some(app) = m.processes.iter().find(|p| p.key.pid == 10) else {
        eprintln!("sysmon_test: process 10 is missing: {:?}", m.processes);
        return false;
    };
    let kernel = m.processes.iter().find(|p| p.key.pid == 2);
    let folded = app.threads == 2 && app.cpu == (WHOLE / 4) as u32 && app.name == "task10";
    let resident = app.memory == Some(100 * 4096);
    let kept_apart = kernel.is_some_and(|k| k.kernel && k.threads == 1 && k.memory.is_none());
    if !(folded && resident && kept_apart) {
        eprintln!("sysmon_test: {app:?}, kernel {kernel:?}");
    }
    folded && resident && kept_apart && m.processes.len() == 2 && m.process_count() == 1
}

fn audited(app: &mut SysmonApp, page: &str, width: i32) -> Vec<String> {
    app.on_resize(width as u32, 720);
    let node: Node<SysmonMsg> = app.view();
    slopos_appkit::audit::audit_node(&node, &[width], 720)
        .into_iter()
        .map(|found| format!("{page} at {width}: {found}"))
        .collect()
}

fn test_every_page_reads_whole() -> bool {
    let mut app = SysmonApp::from_monitor(monitor(70));
    let least = app.window().min_size.width;
    let widths = [least, slopos_userland::apps::sysmon::WIDTH, 1280];
    let mut found = Vec::new();
    let resources = app.resources().len();
    for row in 0..resources {
        app.update(SysmonMsg::Resource(row));
        let page = format!("{:?}", app.resource);
        for width in widths {
            found.extend(audited(&mut app, &page, width));
        }
    }
    app.update(SysmonMsg::Tab(1));
    for width in widths {
        found.extend(audited(&mut app, "processes", width));
    }
    for finding in &found {
        eprintln!("sysmon_test: {finding}");
    }
    found.is_empty() && resources == 5
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "every_ending_is_said_as_a_sentence",
        test_every_ending_is_said_as_a_sentence,
    ),
    (
        "selection_follows_process_across_resort",
        test_selection_follows_process_across_resort,
    ),
    (
        "recycled_pid_is_not_the_same_process",
        test_recycled_pid_is_not_the_same_process,
    ),
    (
        "filtered_out_process_has_no_row",
        test_filtered_out_process_has_no_row,
    ),
    (
        "figures_are_what_moved_between_samples",
        test_figures_are_what_moved_between_samples,
    ),
    (
        "threads_fold_into_their_process",
        test_threads_fold_into_their_process,
    ),
    ("every_page_reads_whole", test_every_page_reads_whole),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
