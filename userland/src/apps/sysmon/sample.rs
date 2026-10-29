use std::collections::HashMap;

use slopos_abi::net::{
    NET_ADDR_SCOPE_GLOBAL, NET_IFINDEX_NONE, NET_IFKIND_LOOPBACK, NET_Q_ADDRS, NET_Q_IFACES,
    UserAddr, UserIface,
};
use slopos_abi::task::MAX_TASKS;

use crate::syscall::{
    UserBlockStats, UserCpuInfo, UserCpuPerf, UserCpuPerfInfo, UserDisplayStats, UserPerCpuStats,
    UserProcessMemory, UserSysInfo, UserTaskEntry, core as sys_core,
};

use super::format::is_idle_task;

#[derive(Clone, Copy, Default)]
pub struct Iface {
    pub iface: UserIface,
    pub addr: Option<([u8; 4], u8)>,
}

#[derive(Clone, Default)]
pub struct Disk {
    pub stats: UserBlockStats,
    pub model: Option<String>,
}

#[derive(Clone, Default)]
pub struct Sample {
    pub at_ms: u64,
    pub sys: UserSysInfo,
    pub cpu_info: UserCpuInfo,
    pub perf_info: UserCpuPerfInfo,
    pub perf: Vec<UserCpuPerf>,
    pub cpus: Vec<UserPerCpuStats>,
    pub tasks: Vec<UserTaskEntry>,
    pub memory: Vec<UserProcessMemory>,
    /// `None` when unread.
    pub ifaces: Option<Vec<Iface>>,
    pub disks: Vec<Disk>,
    pub display: Option<UserDisplayStats>,
}

pub type Models = HashMap<String, Option<String>>;

impl Sample {
    pub fn take(models: &mut Models) -> Sample {
        let at_ms = sys_core::get_time_ms();
        let mut sys = UserSysInfo::default();
        let _ = sys_core::sys_info(&mut sys);
        let mut cpu_info = UserCpuInfo::default();
        let _ = sys_core::cpu_info(&mut cpu_info);
        let slots = (cpu_info.cpu_count as usize).max(1);

        let mut perf_info = UserCpuPerfInfo::default();
        let mut perf = vec![UserCpuPerf::default(); slots];
        let written = sys_core::cpu_perf(Some(&mut perf_info), &mut perf);
        perf.truncate(written.max(0) as usize);
        perf.retain(|cpu| cpu.online != 0);

        let mut cpus = vec![UserPerCpuStats::default(); slots];
        let written = sys_core::percpu_stats(&mut cpus);
        cpus.truncate(written.max(0) as usize);

        let room = (sys.total_tasks as usize + 64).clamp(256, MAX_TASKS);
        let mut tasks = vec![UserTaskEntry::default(); room];
        let written = sys_core::process_list(&mut tasks);
        tasks.truncate(written.max(0) as usize);
        tasks.retain(|task| !is_idle_task(task));

        let mut memory = vec![UserProcessMemory::default(); room];
        let written = sys_core::process_memory(&mut memory);
        memory.truncate(written.max(0) as usize);

        let mut disks = vec![UserBlockStats::default(); DISKS];
        let written = sys_core::block_stats(&mut disks);
        disks.truncate(written.max(0) as usize);
        // The next disk to take a departed one's name is read afresh.
        models.retain(|node, _| {
            disks
                .iter()
                .any(|d| super::format::c_text(&d.name) == *node)
        });
        let disks = disks
            .into_iter()
            .map(|stats| {
                let node = super::format::c_text(&stats.name);
                let model = models
                    .entry(node.clone())
                    .or_insert_with(|| model_of(&node))
                    .clone();
                Disk { stats, model }
            })
            .collect();

        let mut display = UserDisplayStats::default();
        let display = (sys_core::display_stats(&mut display) == 0).then_some(display);

        Sample {
            at_ms,
            sys,
            cpu_info,
            perf_info,
            perf,
            cpus,
            tasks,
            memory,
            ifaces: ifaces(),
            disks,
            display,
        }
    }
}

const DISKS: usize = 32;

fn model_of(node: &str) -> Option<String> {
    crate::disk_names::by_id(node).1
}

fn ifaces() -> Option<Vec<Iface>> {
    let listed = crate::net_query::fetch::<UserIface>(NET_Q_IFACES, NET_IFINDEX_NONE).ok()?;
    Some(
        listed
            .records
            .into_iter()
            .filter(|iface| iface.kind != NET_IFKIND_LOOPBACK)
            .map(|iface| {
                let addr = crate::net_query::fetch::<UserAddr>(NET_Q_ADDRS, iface.ifindex)
                    .ok()
                    .and_then(|addrs| {
                        addrs
                            .records
                            .into_iter()
                            .find(|a| a.scope == NET_ADDR_SCOPE_GLOBAL as u8)
                    })
                    .map(|a| (a.addr, a.prefix_len));
                Iface { iface, addr }
            })
            .collect(),
    )
}
