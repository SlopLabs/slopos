use std::sync::OnceLock;

use slopos_abi::PAGE_SIZE;
use slopos_net_core::{Ipv4, Mac};
use slopos_slibc::mem::malloc::heap_stats;

use crate::syscall::process as sys_proc;
use slopos_abi::errno::Errno;
use slopos_abi::signal::SIGKILL;
use slopos_abi::task::TaskStatus;
use slopos_appkit::theme::space;
use slopos_appkit::theme::words::Bytes;
use slopos_appkit::{
    Action, App, Button, ButtonRow, ContextMenuAt, Dialog, Key, MenuItem, MenuItemKind, Modifiers,
    NamedKey, Node, Page, PropertyList, Section, SortIndicator, StatusBar, TableColumn,
    TableColumnWidth, Text, Tone, WindowSpec, column,
};

mod format;
pub mod selection;
mod state;

pub(crate) use format::*;
pub(crate) use state::{ContextMenu, KillOutcome, SortColumn, SysmonApp, Tab};

const MENU_KILL: usize = 0;

pub(crate) const MAX_CPUS: usize = 64;
pub(crate) const REFRESH_INTERVAL_MS: u64 = 1000;

#[derive(Clone, Debug)]
pub enum SysmonMsg {
    TabChanged(usize),
    SortColumn(usize),
    SelectRow(usize),
    OpenContextMenu(ContextMenuAt),
    MenuAction(usize),
    CloseMenu,
    Kill,
    Cancel,
}

impl App for SysmonApp {
    type Message = SysmonMsg;

    fn view(&self) -> Node<SysmonMsg> {
        let tabs = Node::TabBar {
            tabs: vec![
                String::from("Overview"),
                String::from("Processes"),
                String::from("Hardware"),
            ],
            active: match self.active_tab {
                Tab::Overview => 0,
                Tab::Processes => 1,
                Tab::Hardware => 2,
            },
            on_change: Some(SysmonMsg::TabChanged),
            content: vec![
                self.view_overview(),
                self.view_processes(),
                self.view_hardware(),
            ],
        };
        let mut layers = vec![tabs];
        layers.extend(self.view_kill_dialog());
        Node::ZStack { children: layers }
    }

    fn update(&mut self, msg: SysmonMsg) -> Action {
        match msg {
            SysmonMsg::TabChanged(tab) => {
                self.active_tab = match tab {
                    0 => Tab::Overview,
                    1 => Tab::Processes,
                    _ => Tab::Hardware,
                };
                self.dismiss_transients();
                Action::Rebuild
            }
            SysmonMsg::SortColumn(index) => {
                if let Some(column) = process_columns().get(index) {
                    self.cycle_sort_for_column(column.sort);
                }
                Action::Rebuild
            }
            SysmonMsg::SelectRow(row) => {
                self.select_row(row);
                Action::Rebuild
            }
            SysmonMsg::OpenContextMenu(at) => {
                self.select_row(at.row);
                self.last_kill = None;
                self.context_menu = self.target_for_row(at.row).map(|target| ContextMenu {
                    target,
                    x: at.x,
                    y: at.y,
                });
                Action::Rebuild
            }
            SysmonMsg::MenuAction(item) => {
                let menu = self.context_menu.take();
                if item == MENU_KILL {
                    self.confirm_kill = menu
                        .map(|m| m.target)
                        .filter(|target| self.is_killable(target.pid()));
                }
                Action::Rebuild
            }
            SysmonMsg::CloseMenu => {
                self.context_menu = None;
                Action::Rebuild
            }
            SysmonMsg::Kill => {
                // A refresh since the dialog opened may have recycled the id.
                self.last_kill = match self.pending_kill_target() {
                    Some(target) => {
                        let name = target.name.clone();
                        let rc = sys_proc::kill(target.pid(), SIGKILL);
                        Some(if rc < 0 {
                            KillOutcome::Failed { name, errno: rc }
                        } else {
                            KillOutcome::Sent { name }
                        })
                    }
                    None => self.confirm_kill.as_ref().map(|t| KillOutcome::Vanished {
                        name: t.name.clone(),
                    }),
                };
                self.confirm_kill = None;
                self.refresh_data();
                Action::Rebuild
            }
            SysmonMsg::Cancel => {
                self.confirm_kill = None;
                Action::Rebuild
            }
        }
    }

    fn on_key(&mut self, key: Key, _modifiers: Modifiers) -> Action {
        if key != Key::Named(NamedKey::Delete) || self.active_tab != Tab::Processes {
            return Action::None;
        }
        self.context_menu = None;
        self.last_kill = None;
        self.confirm_kill = self
            .target_for_selection()
            .filter(|target| self.is_killable(target.pid()));
        Action::Rebuild
    }

    fn tick_interval_ms(&self) -> Option<u64> {
        Some(REFRESH_INTERVAL_MS)
    }

    fn tick(&mut self) -> Action {
        self.refresh_data();
        Action::Rebuild
    }

    fn title(&self) -> String {
        "System Monitor".into()
    }

    fn app_id(&self) -> &str {
        "org.slopos.sysmon"
    }

    fn window(&self) -> WindowSpec {
        let least = TableColumn::least_width(&self.process_table_columns());
        let spec = WindowSpec::new(least.max(640), 480);
        spec.min_size(least, spec.min_size.height)
    }
}

impl SysmonApp {
    fn dismiss_transients(&mut self) {
        self.confirm_kill = None;
        self.context_menu = None;
    }

    fn view_overview(&self) -> Node<SysmonMsg> {
        let cpus = (0..self.cpu_count)
            .map(|i| {
                let pct = self.cpu_usage_pct[i];
                let mut label = format!("CPU {}", self.percpu[i].cpu_id);
                if !self.cpu_type[i].is_empty() {
                    label += &format!(" ({})", self.cpu_type[i]);
                }
                label += &format!(" · {pct}%");
                if let Some(mhz) = self.cpu_eff_mhz[i] {
                    label += &format!(" · {mhz} MHz");
                }
                Node::ProgressBar {
                    value: Some(pct),
                    label,
                    tone: load_tone(pct),
                }
            })
            .collect();
        let (used, total, pct) = self.memory();
        let memory = Node::ProgressBar {
            value: Some(pct),
            label: format!("{} of {} used ({pct}%)", Bytes(used), Bytes(total)),
            tone: load_tone(pct),
        };
        let mut counts = std::collections::BTreeMap::new();
        for task in &self.tasks[..self.task_count] {
            *counts
                .entry(task_state(TaskStatus::from_u8(task.state)))
                .or_insert(0usize) += 1;
        }
        let mut tasks = PropertyList::new().row("All", self.task_count.to_string());
        for ((_, state), count) in counts {
            tasks = tasks.row(state, count.to_string());
        }
        let network: Node<SysmonMsg> = match &self.net {
            Ok(Some(net)) => PropertyList::new()
                .row(
                    "Received",
                    format!("{} in {}", Bytes(net.rx_bytes), packets(net.rx_packets)),
                )
                .row(
                    "Sent",
                    format!("{} in {}", Bytes(net.tx_bytes), packets(net.tx_packets)),
                )
                .into(),
            Ok(None) => Text::new(NO_NETWORK).tone(Tone::Secondary).into(),
            Err(()) => Text::new(UNREAD_NETWORK).tone(Tone::Secondary).into(),
        };
        let sections: Vec<Node<SysmonMsg>> = vec![
            Section::new(
                "System",
                PropertyList::new().row("Running for", format_uptime(self.last_refresh_ms)),
            )
            .into(),
            Section::new("Processors", column(space::XS, cpus)).into(),
            Section::new("Memory", memory).into(),
            Section::new("Tasks", tasks).into(),
            Section::new("Network", network).into(),
        ];
        Page::untitled().content(column(space::XL, sections)).into()
    }

    fn memory(&self) -> (u64, u64, u32) {
        let total = (self.sys_info.total_pages as u64).saturating_mul(PAGE_SIZE);
        let used = (self.sys_info.allocated_pages.min(self.sys_info.total_pages) as u64)
            .saturating_mul(PAGE_SIZE);
        let pct = match total {
            0 => 0,
            _ => (used.saturating_mul(100) / total).min(100) as u32,
        };
        (used, total, pct)
    }

    fn process_table_columns(&self) -> Vec<TableColumn> {
        let sort = match self.sort_ascending {
            true => SortIndicator::Ascending,
            false => SortIndicator::Descending,
        };
        process_columns()
            .iter()
            .map(|column| TableColumn {
                label: String::from(column.heading),
                width: column.width,
                sort_indicator: (self.sort_column == column.sort).then_some(sort),
            })
            .collect()
    }

    fn view_processes(&self) -> Node<SysmonMsg> {
        let columns = self.process_table_columns();
        let mut rows: Vec<Vec<Node<SysmonMsg>>> = Vec::new();
        for row_idx in 0..self.task_count {
            let Some(idx) = self.sorted_task_index(row_idx) else {
                continue;
            };
            let task = &self.tasks[idx];
            let (_, state) = task_state(TaskStatus::from_u8(task.state));
            rows.push(vec![
                cell(&format!("{}", task.task_id)),
                cell(&task_name_string(task)),
                cell(state),
                cell(&format_pct(self.task_cpu_pct[idx])),
                cell(priority_label(task.priority)),
                cell(&format!("{}", task.last_cpu)),
                cell(&format_runtime(task.total_runtime_us)),
            ]);
        }

        let table = Node::Table {
            columns,
            rows,
            selected: self.selected_row(),
            on_select: Some(SysmonMsg::SelectRow),
            on_activate: None,
            on_header_click: Some(SysmonMsg::SortColumn),
            on_context_menu: Some(SysmonMsg::OpenContextMenu),
            shows: 0,
        };

        let body = column(
            0,
            vec![
                Node::Expand {
                    weight: 1,
                    child: Box::new(table),
                },
                self.view_status_line(),
            ],
        );

        let mut layers = vec![body];
        layers.extend(
            self.context_menu
                .as_ref()
                .map(|m| self.view_context_menu(m)),
        );
        Node::ZStack { children: layers }
    }

    fn view_context_menu(&self, menu: &ContextMenu) -> Node<SysmonMsg> {
        Node::Popup {
            x: menu.x,
            y: menu.y,
            on_dismiss: Some(SysmonMsg::CloseMenu),
            child: Box::new(Node::Menu {
                items: vec![MenuItem {
                    label: "End task",
                    shortcut: Some("Delete"),
                    enabled: self.is_killable(menu.target.pid()),
                    kind: MenuItemKind::Action,
                }],
                on_action: Some(SysmonMsg::MenuAction),
            }),
        }
    }

    fn view_kill_dialog(&self) -> Option<Node<SysmonMsg>> {
        let target = self.confirm_kill.as_ref()?;
        Some(
            Dialog::new(format!("End “{}”?", target.name))
                .content(
                    Text::new(format!(
                        "Task {} stops at once, without saving its work.",
                        target.pid()
                    ))
                    .wrap(),
                )
                .footer(
                    ButtonRow::new()
                        .cancel(Button::new("Cancel").on_press(SysmonMsg::Cancel))
                        .destructive(Button::new("End task").on_press(SysmonMsg::Kill)),
                )
                .on_dismiss(SysmonMsg::Cancel)
                .into(),
        )
    }

    fn view_status_line(&self) -> Node<SysmonMsg> {
        let (text, tone) = match &self.last_kill {
            Some(KillOutcome::Sent { name }) => (format!("Ended “{name}”."), Tone::Success),
            Some(KillOutcome::Failed { name, errno }) if *errno == Errno::EPERM.raw() => (
                format!(
                    "Couldn't end “{name}”, because you aren't allowed to. Ask an administrator to end it."
                ),
                Tone::Danger,
            ),
            Some(KillOutcome::Failed { name, errno }) if *errno == Errno::ESRCH.raw() => {
                (format!("“{name}” had already ended."), Tone::Warning)
            }
            Some(KillOutcome::Failed { name, .. }) => {
                (format!("Couldn't end “{name}”. Try again."), Tone::Danger)
            }
            Some(KillOutcome::Vanished { name }) => {
                (format!("“{name}” had already ended."), Tone::Warning)
            }
            None => (
                String::from("Right-click a task, or choose it and press Delete, to end it."),
                Tone::Secondary,
            ),
        };
        StatusBar::new(text).tone(tone).into()
    }

    fn view_hardware(&self) -> Node<SysmonMsg> {
        let processor = PropertyList::new()
            .row("Model", padded_text(&self.cpu_info.brand_string))
            .row("Vendor", padded_text(&self.cpu_info.vendor))
            .row("Cores", self.cpu_info.cpu_count.to_string())
            .row(
                "Family and model",
                format!(
                    "{} / {}, stepping {}",
                    self.cpu_info.family, self.cpu_info.model, self.cpu_info.stepping
                ),
            )
            .row("Features", format_cpu_features(self.cpu_info.features));
        let (used, total, _) = self.memory();
        let free = (self.sys_info.free_pages as u64).saturating_mul(PAGE_SIZE);
        let memory = PropertyList::new()
            .row(
                "Total",
                format!("{} ({} pages)", Bytes(total), self.sys_info.total_pages),
            )
            .row("Free", Bytes(free).to_string())
            .row("In use", Bytes(used).to_string());
        let scheduler = PropertyList::new()
            .row(
                "Context switches",
                format_number(self.sys_info.task_context_switches),
            )
            .row(
                "Scheduler switches",
                format_number(self.sys_info.scheduler_context_switches),
            )
            .row("Yields", format_number(self.sys_info.scheduler_yields))
            .row(
                "Scheduler runs",
                format_number(self.sys_info.schedule_calls as u64),
            );
        let stats = heap_stats();
        let heap = PropertyList::new()
            .row("Arena", Bytes(stats.arena_size as u64).to_string())
            .row("Cached", Bytes(stats.cached_size as u64).to_string())
            .row(
                "Largest free block",
                Bytes(stats.largest_free as u64).to_string(),
            )
            .row(
                "Direct allocations",
                format_number(stats.direct_count as u64),
            );
        let network: Node<SysmonMsg> = match &self.net {
            Ok(Some(net)) => PropertyList::new()
                .row("Status", connection(net.oper_state))
                .row("Address", format!("{}/{}", Ipv4(net.addr), net.prefix_len))
                .row("Hardware address", format!("{}", Mac(net.mac)))
                .into(),
            Ok(None) => Text::new(NO_NETWORK).tone(Tone::Secondary).into(),
            Err(()) => Text::new(UNREAD_NETWORK).tone(Tone::Secondary).into(),
        };
        let startup = PropertyList::new()
            .row("Running for", format_uptime(self.last_refresh_ms))
            .row(
                "Startup flags",
                format!("0x{:08x}", self.sys_info.boot_flags),
            )
            .row(
                "Wheel of Fate balance",
                match self.sys_info.wl_balance {
                    owed if owed < 0 => format!("−{}", format_number(owed.unsigned_abs())),
                    held => format_number(held as u64),
                },
            );
        let sections = vec![
            Section::new("Processor", processor).into(),
            Section::new("Memory", memory).into(),
            Section::new("Scheduler", scheduler).into(),
            Section::new("Memory allocator", heap).into(),
            Section::new("Network", network).into(),
            Section::new("Startup", startup).into(),
        ];
        Page::untitled().content(column(space::XL, sections)).into()
    }
}

/// Said where the computer has no network adapter, which a down one is not.
const NO_NETWORK: &str = "No network adapter";
const UNREAD_NETWORK: &str = "The network couldn't be read";

fn packets(n: u64) -> String {
    match n {
        1 => String::from("1 packet"),
        n => format!("{} packets", format_number(n)),
    }
}

/// RFC 2863's operational states, in words.
fn connection(state: u8) -> &'static str {
    use slopos_abi::net::{
        NET_OPER_DORMANT, NET_OPER_LOWERLAYERDOWN, NET_OPER_TESTING, NET_OPER_UP,
    };
    match state {
        NET_OPER_UP => "Connected",
        NET_OPER_LOWERLAYERDOWN => "No cable",
        NET_OPER_DORMANT => "Waiting to connect",
        NET_OPER_TESTING => "Testing",
        _ => "Not connected",
    }
}

struct Column {
    heading: &'static str,
    sort: SortColumn,
    width: TableColumnWidth,
}

impl Column {
    fn fitting<'a>(
        heading: &'static str,
        sort: SortColumn,
        values: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        Self {
            heading,
            sort,
            width: TableColumnWidth::fitting(heading, values),
        }
    }
}

/// The longest id shown whole.
const LONGEST_PID: u32 = 9_999_999;
/// The longest run time shown whole.
const LONGEST_RUN_US: u64 = 100 * 3600 * 1_000_000 - 1;

/// The process table's columns in order, each but the name as wide as the
/// widest it shows.
fn process_columns() -> &'static [Column; 7] {
    static COLUMNS: OnceLock<[Column; 7]> = OnceLock::new();
    COLUMNS.get_or_init(|| {
        let states = (0..=u8::MAX).map(|state| task_state(TaskStatus::from_u8(state)).1);
        [
            Column::fitting("PID", SortColumn::Pid, [LONGEST_PID.to_string().as_str()]),
            Column {
                heading: "Name",
                sort: SortColumn::Name,
                width: TableColumnWidth::Flex(1),
            },
            Column::fitting("State", SortColumn::State, states),
            Column::fitting("CPU %", SortColumn::CpuPct, [format_pct(1000).as_str()]),
            Column::fitting(
                "Priority",
                SortColumn::Priority,
                (0..=u8::MAX).map(priority_label),
            ),
            Column::fitting(
                "CPU",
                SortColumn::Cpu,
                [(MAX_CPUS - 1).to_string().as_str()],
            ),
            Column::fitting(
                "Run time",
                SortColumn::Runtime,
                [format_runtime(LONGEST_RUN_US).as_str()],
            ),
        ]
    })
}

fn load_tone(pct: u32) -> Tone {
    match pct {
        0..50 => Tone::Success,
        50..80 => Tone::Warning,
        _ => Tone::Danger,
    }
}

fn cell(text: &str) -> Node<SysmonMsg> {
    Text::new(text).into()
}

pub fn sysmon_main() -> ! {
    slopos_appkit::run_app(SysmonApp::new())
}
