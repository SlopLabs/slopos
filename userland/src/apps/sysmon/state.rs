use core::cmp::Ordering;
use std::collections::HashMap;

use slopos_appkit::{AppSender, LineInput};

use super::SysmonMsg;
use super::monitor::{Monitor, Process};
use super::sample::{Models, Sample};
use super::selection::{self, TaskKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Performance,
    Processes,
}

impl Tab {
    pub(crate) const ALL: [Tab; 2] = [Tab::Performance, Tab::Processes];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Tab::Performance => "Performance",
            Tab::Processes => "Processes",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resource {
    Cpu,
    Memory,
    Disk(String),
    Adapter(u32),
    Gpu,
}

#[derive(Clone, PartialEq)]
pub(crate) struct KillTarget {
    pub(crate) key: TaskKey,
    pub(crate) name: String,
}

impl KillTarget {
    pub(crate) fn pid(&self) -> u32 {
        self.key.pid
    }
}

pub(crate) struct ContextMenu {
    pub(crate) target: KillTarget,
    pub(crate) x: i32,
    pub(crate) y: i32,
}

pub enum KillOutcome {
    Sent {
        name: String,
    },
    Failed {
        name: String,
    },
    Vanished {
        name: String,
    },
    /// It holds a capability beyond those every program holds.
    Powerful {
        name: String,
    },
    /// The system runs it, for itself or for everyone.
    System {
        name: String,
    },
    /// Who it runs as, or what it holds, could not be read.
    Unowned {
        name: String,
    },
    /// Refused, and privd could not be asked to end it.
    Refused {
        name: String,
    },
    /// privd is asking whoever may allow it to be ended.
    Asking {
        name: String,
    },
    /// privd ran nothing; `why` is its answer, when it gave one.
    Denied {
        name: String,
        why: Option<String>,
        by_person: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SortColumn {
    Name,
    User,
    Cpu,
    Memory,
    Threads,
    CpuTime,
    Pid,
}

impl SortColumn {
    fn descends_first(self) -> bool {
        matches!(
            self,
            SortColumn::Cpu | SortColumn::Memory | SortColumn::Threads | SortColumn::CpuTime
        )
    }
}

pub struct SysmonApp {
    pub(crate) monitor: Monitor,
    models: Models,
    users: HashMap<u32, String>,
    pub tab: Tab,
    pub resource: Resource,
    pub(crate) width: i32,
    pub(crate) filter: LineInput,
    pub(crate) filter_focused: bool,
    pub(crate) order: Vec<usize>,
    /// A process, not a row: the table re-sorts every refresh.
    pub(crate) selected: Option<TaskKey>,
    pub(crate) sort_column: SortColumn,
    pub(crate) sort_ascending: bool,
    pub(crate) confirm_kill: Option<KillTarget>,
    pub(crate) context_menu: Option<ContextMenu>,
    pub(crate) last_kill: Option<KillOutcome>,
    /// The task privd is asked to end, until it answers.
    pub(crate) asking: Option<KillTarget>,
    pub(crate) self_pid: u32,
    /// What a thread asking privd answers through.
    pub(crate) sender: Option<AppSender<SysmonMsg>>,
}

pub(crate) const SYSTEM_USER: &str = "System";

impl SysmonApp {
    /// Two readings a moment apart, so the first frame has figures.
    pub fn new() -> Self {
        let mut app = Self::from_monitor(Monitor::default());
        app.refresh();
        std::thread::sleep(std::time::Duration::from_millis(FIRST_INTERVAL_MS));
        app.refresh();
        app
    }

    pub fn from_monitor(monitor: Monitor) -> Self {
        let mut app = Self {
            monitor,
            models: Models::new(),
            users: HashMap::new(),
            tab: Tab::Performance,
            resource: Resource::Cpu,
            width: super::WIDTH,
            filter: LineInput::new(),
            filter_focused: false,
            order: Vec::new(),
            selected: None,
            sort_column: SortColumn::Cpu,
            sort_ascending: false,
            confirm_kill: None,
            context_menu: None,
            last_kill: None,
            asking: None,
            self_pid: crate::syscall::process::getpid(),
            sender: None,
        };
        app.settle();
        app
    }

    pub(crate) fn refresh(&mut self) {
        self.monitor.record(Sample::take(&mut self.models));
        self.settle();
    }

    fn settle(&mut self) {
        for process in &self.monitor.processes {
            if !process.kernel {
                self.users.entry(process.uid).or_insert_with(|| {
                    crate::accounts::name_of(process.uid).unwrap_or_else(|| process.uid.to_string())
                });
            }
        }
        if !self.resources().contains(&self.resource) {
            self.resource = Resource::Cpu;
        }
        self.reorder();
        if self.selected.is_some_and(|key| self.row_of(key).is_none()) {
            self.selected = None;
        }
        // A target that exited designates nothing, rather than whatever
        // inherits its number.
        if self
            .confirm_kill
            .as_ref()
            .is_some_and(|t| self.live(t).is_none())
        {
            self.confirm_kill = None;
        }
        if self
            .context_menu
            .as_ref()
            .is_some_and(|m| self.live(&m.target).is_none())
        {
            self.context_menu = None;
        }
    }

    pub fn resources(&self) -> Vec<Resource> {
        let mut out = vec![Resource::Cpu, Resource::Memory];
        out.extend(
            self.monitor
                .disks
                .iter()
                .map(|d| Resource::Disk(d.name.clone())),
        );
        out.extend(
            self.monitor
                .adapters
                .iter()
                .map(|a| Resource::Adapter(a.ifindex)),
        );
        if self.monitor.display.is_some() {
            out.push(Resource::Gpu);
        }
        out
    }

    pub(crate) fn open(&mut self, page: &str) {
        let first = |pick: fn(&Resource) -> bool| self.resources().into_iter().find(pick);
        let resource = match page {
            "memory" => Some(Resource::Memory),
            "disk" => first(|r| matches!(r, Resource::Disk(_))),
            "network" => first(|r| matches!(r, Resource::Adapter(_))),
            "gpu" => first(|r| *r == Resource::Gpu),
            _ => None,
        };
        self.tab = match page {
            "processes" => Tab::Processes,
            _ => Tab::Performance,
        };
        self.resource = resource.unwrap_or(Resource::Cpu);
    }

    pub(crate) fn user_of(&self, process: &Process) -> &str {
        match process.kernel {
            true => SYSTEM_USER,
            false => self.users.get(&process.uid).map_or("", String::as_str),
        }
    }

    pub(crate) fn processes(&self) -> &[Process] {
        &self.monitor.processes
    }

    fn live(&self, target: &KillTarget) -> Option<usize> {
        selection::index_of(target.key, self.processes())
    }

    pub(crate) fn selected_row(&self) -> Option<usize> {
        self.selected.and_then(|key| self.row_of(key))
    }

    fn row_of(&self, key: TaskKey) -> Option<usize> {
        selection::row_of(key, self.processes(), &self.order)
    }

    pub(crate) fn select_row(&mut self, row: usize) {
        self.selected = selection::key_at_row(self.processes(), &self.order, row);
    }

    /// Refuses only sysmon itself, which would vanish mid-frame; the kernel,
    /// and for a task of another user privd, decide the rest.
    pub(crate) fn is_killable(&self, pid: u32) -> bool {
        pid != self.self_pid
    }

    pub(crate) fn target_for_row(&self, row: usize) -> Option<KillTarget> {
        let process = self.processes().get(*self.order.get(row)?)?;
        Some(KillTarget {
            key: process.key,
            name: process.name.clone(),
        })
    }

    pub(crate) fn target_for_selection(&self) -> Option<KillTarget> {
        let process = self.processes().get(self.live_key(self.selected?)?)?;
        Some(KillTarget {
            key: process.key,
            name: process.name.clone(),
        })
    }

    fn live_key(&self, key: TaskKey) -> Option<usize> {
        selection::index_of(key, self.processes())
    }

    pub(crate) fn pending_kill_target(&self) -> Option<&KillTarget> {
        let target = self.confirm_kill.as_ref()?;
        self.live(target).map(|_| target)
    }

    fn kept(&self, process: &Process) -> bool {
        let wanted = self.filter.text.trim().to_lowercase();
        wanted.is_empty()
            || process.name.to_lowercase().contains(&wanted)
            || self.user_of(process).to_lowercase().contains(&wanted)
            || process.key.pid.to_string().contains(&wanted)
    }

    pub(crate) fn reorder(&mut self) {
        let processes = self.processes();
        let mut order: Vec<usize> = (0..processes.len())
            .filter(|&i| self.kept(&processes[i]))
            .collect();
        order.sort_by(|&a, &b| {
            let ord = self.compare(&processes[a], &processes[b]);
            let ord = if self.sort_ascending {
                ord
            } else {
                ord.reverse()
            };
            ord.then_with(|| processes[a].key.pid.cmp(&processes[b].key.pid))
        });
        self.order = order;
    }

    fn compare(&self, a: &Process, b: &Process) -> Ordering {
        match self.sort_column {
            SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortColumn::User => self.user_of(a).cmp(self.user_of(b)),
            SortColumn::Cpu => a.cpu.cmp(&b.cpu),
            SortColumn::Memory => a.memory.cmp(&b.memory),
            SortColumn::Threads => a.threads.cmp(&b.threads),
            SortColumn::CpuTime => a.cpu_time_us.cmp(&b.cpu_time_us),
            SortColumn::Pid => a.key.pid.cmp(&b.key.pid),
        }
    }

    pub(crate) fn sort_by(&mut self, column: SortColumn) {
        if self.sort_column == column {
            self.sort_ascending = !self.sort_ascending;
        } else {
            self.sort_column = column;
            self.sort_ascending = !column.descends_first();
        }
        self.reorder();
    }
}

const FIRST_INTERVAL_MS: u64 = 250;
