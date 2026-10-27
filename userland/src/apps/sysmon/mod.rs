use std::format;
use std::string::String;

use slopos_abi::PAGE_SIZE;
use slopos_net_core::{Ipv4, Mac};
use slopos_slibc::mem::malloc::heap_stats;

use crate::syscall::process as sys_proc;
use slopos_abi::errno::Errno;
use slopos_abi::signal::SIGKILL;
use slopos_abi::task::TaskStatus;
use slopos_appkit::theme::{inset, space};
use slopos_appkit::{
    Action, App, Button, ButtonRow, ContextMenuAt, CrossAxisAlignment, EdgeInsets, Key, MenuItem,
    MenuItemKind, Modifiers, NamedKey, Node, PropertyList, ScrollDirection, ScrollbarVisibility,
    Section, SortIndicator, TableColumn, TableColumnWidth, Text, Tone, WindowSpec,
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
            SysmonMsg::SortColumn(col_idx) => {
                let col = match col_idx {
                    0 => SortColumn::Pid,
                    1 => SortColumn::Name,
                    2 => SortColumn::State,
                    3 => SortColumn::CpuPct,
                    4 => SortColumn::Priority,
                    5 => SortColumn::Cpu,
                    _ => SortColumn::Runtime,
                };
                self.cycle_sort_for_column(col);
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
                // Re-checked here rather than trusted from when the dialog
                // opened: a refresh in between may have recycled the id.
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
        WindowSpec::new(640, 480)
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
            label: format!(
                "{} of {} used ({pct}%)",
                format_bytes_mib(used),
                format_bytes_mib(total)
            ),
            tone: load_tone(pct),
        };
        let mut counts = [0usize; 4];
        for task in &self.tasks[..self.task_count] {
            let at = match TaskStatus::from_u8(task.state) {
                TaskStatus::Running => 0,
                TaskStatus::Ready => 1,
                TaskStatus::Blocked => 2,
                TaskStatus::Stopped
                | TaskStatus::Zombie
                | TaskStatus::Terminated
                | TaskStatus::Invalid => 3,
            };
            counts[at] += 1;
        }
        let tasks = PropertyList::new()
            .row("All", self.task_count.to_string())
            .row("Running", counts[0].to_string())
            .row("Ready to run", counts[1].to_string())
            .row("Waiting", counts[2].to_string())
            .row("Stopped or ending", counts[3].to_string());
        let network: Node<SysmonMsg> = match &self.net {
            Ok(Some(net)) => PropertyList::new()
                .row(
                    "Received",
                    format!(
                        "{} in {}",
                        format_bytes_mib(net.rx_bytes),
                        packets(net.rx_packets)
                    ),
                )
                .row(
                    "Sent",
                    format!(
                        "{} in {}",
                        format_bytes_mib(net.tx_bytes),
                        packets(net.tx_packets)
                    ),
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
        page(column(space::XL, sections))
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

    fn view_processes(&self) -> Node<SysmonMsg> {
        let sort_ind = |col: SortColumn| -> Option<SortIndicator> {
            if self.sort_column == col {
                Some(if self.sort_ascending {
                    SortIndicator::Ascending
                } else {
                    SortIndicator::Descending
                })
            } else {
                None
            }
        };

        let columns = vec![
            TableColumn {
                label: String::from("PID"),
                width: TableColumnWidth::Fixed(60),
                sort_indicator: sort_ind(SortColumn::Pid),
            },
            TableColumn {
                label: String::from("Name"),
                width: TableColumnWidth::Flex(2),
                sort_indicator: sort_ind(SortColumn::Name),
            },
            TableColumn {
                label: String::from("State"),
                width: TableColumnWidth::Fixed(70),
                sort_indicator: sort_ind(SortColumn::State),
            },
            TableColumn {
                label: String::from("CPU %"),
                width: TableColumnWidth::Fixed(70),
                sort_indicator: sort_ind(SortColumn::CpuPct),
            },
            TableColumn {
                label: String::from("Priority"),
                width: TableColumnWidth::Fixed(70),
                sort_indicator: sort_ind(SortColumn::Priority),
            },
            TableColumn {
                label: String::from("CPU"),
                width: TableColumnWidth::Fixed(50),
                sort_indicator: sort_ind(SortColumn::Cpu),
            },
            TableColumn {
                label: String::from("Run time"),
                width: TableColumnWidth::Flex(1),
                sort_indicator: sort_ind(SortColumn::Runtime),
            },
        ];

        let mut rows: Vec<Vec<Node<SysmonMsg>>> = Vec::new();
        for row_idx in 0..self.task_count {
            let Some(idx) = self.sorted_task_index(row_idx) else {
                continue;
            };
            let task = &self.tasks[idx];
            let state_str = task_state(task.state);
            rows.push(vec![
                cell(&format!("{}", task.task_id)),
                cell(&truncate_name(&task_name_string(task), 16)),
                cell(state_str),
                cell(&format_pct(self.task_cpu_pct[idx])),
                cell(priority_label(task.priority)),
                cell(&format!("{}", task.last_cpu)),
                cell(&format_runtime(task.total_runtime_us)),
            ]);
        }

        let table = Node::Table {
            columns,
            rows,
            row_height: 20,
            // Resolved from the selected task each frame, so a re-sort carries
            // the highlight with its task rather than leaving it on the row.
            selected: self.selected_row(),
            on_select: Some(SysmonMsg::SelectRow),
            on_activate: None,
            on_header_click: Some(SysmonMsg::SortColumn),
            on_context_menu: Some(SysmonMsg::OpenContextMenu),
            shows: 0,
        };

        let body = Node::VStack {
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
            children: vec![
                Node::Expand {
                    weight: 1,
                    child: Box::new(table),
                },
                self.view_status_line(),
            ],
        };

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
                    shortcut: Some("Del"),
                    enabled: self.is_killable(menu.target.pid()),
                    kind: MenuItemKind::Action,
                }],
                on_action: Some(SysmonMsg::MenuAction),
            }),
        }
    }

    fn view_kill_dialog(&self) -> Option<Node<SysmonMsg>> {
        let target = self.confirm_kill.as_ref()?;
        Some(Node::Dialog {
            title: format!("End “{}”?", target.name),
            icon: None,
            content: Box::new(
                Text::new(format!(
                    "Task {} stops at once, without saving its work.",
                    target.pid()
                ))
                .wrap()
                .into(),
            ),
            footer: ButtonRow::new()
                .cancel(Button::new("Cancel").on_press(SysmonMsg::Cancel))
                .destructive(Button::new("End task").on_press(SysmonMsg::Kill)),
            on_dismiss: Some(SysmonMsg::Cancel),
            anchor: None,
        })
    }

    /// Footer: the outcome of the last kill, else the available gestures.
    fn view_status_line(&self) -> Node<SysmonMsg> {
        let (text, tone) = match &self.last_kill {
            Some(KillOutcome::Sent { name }) => (format!("Ended “{name}”."), Tone::Success),
            Some(KillOutcome::Failed { name, errno }) if *errno == Errno::EPERM.raw() => (
                format!(
                    "Couldn't end “{name}”, because you aren't allowed to. Ask an administrator to end it."
                ),
                Tone::Warning,
            ),
            Some(KillOutcome::Failed { name, errno }) if *errno == Errno::ESRCH.raw() => {
                (format!("“{name}” had already ended."), Tone::Warning)
            }
            Some(KillOutcome::Failed { name, .. }) => {
                (format!("Couldn't end “{name}”. Try again."), Tone::Warning)
            }
            Some(KillOutcome::Vanished { name }) => {
                (format!("“{name}” had already ended."), Tone::Warning)
            }
            None => (
                String::from("Right-click a task, or press the Menu key, to end it."),
                Tone::Secondary,
            ),
        };
        Node::Padding {
            padding: EdgeInsets::symmetric(space::SM, space::XS),
            child: Box::new(Text::new(text).tone(tone).into()),
        }
    }

    fn view_hardware(&self) -> Node<SysmonMsg> {
        let processor = PropertyList::new()
            .row("Model", trim_ascii(&self.cpu_info.brand_string))
            .row("Vendor", trim_ascii(&self.cpu_info.vendor))
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
                format!(
                    "{} ({} pages)",
                    format_bytes_mib(total),
                    self.sys_info.total_pages
                ),
            )
            .row("Free", format_bytes_mib(free))
            .row("In use", format_bytes_mib(used));
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
            .row("Arena", format_bytes_mib(stats.arena_size as u64))
            .row("Cached", format_bytes_mib(stats.cached_size as u64))
            .row(
                "Largest free block",
                format_bytes_mib(stats.largest_free as u64),
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
        page(column(space::XL, sections))
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

/// An interface's operational state, as RFC 2863 names them, in words.
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

fn column(spacing: i32, children: Vec<Node<SysmonMsg>>) -> Node<SysmonMsg> {
    Node::VStack {
        spacing,
        align: CrossAxisAlignment::Stretch,
        children,
    }
}

/// A tab's content, scrolled when it does not fit.
fn page(content: Node<SysmonMsg>) -> Node<SysmonMsg> {
    Node::ScrollView {
        direction: ScrollDirection::Vertical,
        show_scrollbar: ScrollbarVisibility::WhenNeeded,
        scroll_y: 0,
        on_scroll: None,
        follow_end: false,
        child: Box::new(Node::Padding {
            padding: inset::PAGE,
            child: Box::new(content),
        }),
    }
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
