use std::sync::LazyLock;

use slopos_appkit::theme::words::Bytes;
use slopos_appkit::{
    Button, ButtonRow, Dialog, EmptyState, Icon, Length, MenuItem, MenuItemKind, Node, SearchField,
    SortIndicator, StatusBar, TableColumn, TableColumnWidth, Text, Toolbar, column,
};

use super::format::{count, cpu_time, number, percent_tenths};
use super::state::{ContextMenu, KillOutcome, SortColumn, SysmonApp};
use super::{MENU_KILL, SysmonMsg};

const SEARCH_WIDTH: i32 = 240;

pub(crate) struct Column {
    heading: &'static str,
    pub(crate) sort: SortColumn,
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

const LONGEST_PID: u32 = 9_999_999;
const LONGEST_CPU_TIME_US: u64 = 100 * 3600 * 1_000_000 - 1;
const LONG_USER: &str = "administrator";
const WIDE_MEMORY: &str = "1023.9 MiB";

pub(crate) static COLUMNS: LazyLock<[Column; 7]> = LazyLock::new(|| {
    [
        Column {
            heading: "Name",
            sort: SortColumn::Name,
            width: TableColumnWidth::Flex(1),
        },
        Column::fitting("User", SortColumn::User, [LONG_USER]),
        Column::fitting("CPU", SortColumn::Cpu, [percent_tenths(1000).as_str()]),
        Column::fitting("Memory", SortColumn::Memory, [WIDE_MEMORY]),
        Column::fitting("Threads", SortColumn::Threads, ["9,999"]),
        Column::fitting(
            "CPU time",
            SortColumn::CpuTime,
            [cpu_time(LONGEST_CPU_TIME_US).as_str()],
        ),
        Column::fitting("PID", SortColumn::Pid, [LONGEST_PID.to_string().as_str()]),
    ]
});

fn cell(text: String) -> Node<SysmonMsg> {
    Text::new(text).into()
}

impl SysmonApp {
    pub(crate) fn table_columns(&self) -> Vec<TableColumn> {
        let sort = match self.sort_ascending {
            true => SortIndicator::Ascending,
            false => SortIndicator::Descending,
        };
        COLUMNS
            .iter()
            .map(|column| TableColumn {
                label: String::from(column.heading),
                width: column.width,
                sort_indicator: (self.sort_column == column.sort).then_some(sort),
            })
            .collect()
    }

    pub(crate) fn view_processes(&self) -> Node<SysmonMsg> {
        let processes = self.processes();
        let shown = self.order.len();
        let filtering = !self.filter.text.trim().is_empty();
        let found = match filtering {
            true => format!("{shown} found"),
            false => String::new(),
        };
        let can_end = self
            .target_for_selection()
            .is_some_and(|t| self.is_killable(t.pid()))
            && self.asking.is_none();
        let toolbar = Toolbar::new()
            .start(Node::SizedBox {
                width: Some(Length::Px(SEARCH_WIDTH)),
                height: None,
                child: Box::new(
                    SearchField::new(self.filter.text.clone(), self.filter.caret)
                        .focused(self.filter_focused)
                        .placeholder("Search processes")
                        .count(found)
                        .invalid(filtering && shown == 0)
                        .on_input(SysmonMsg::Filter)
                        .into(),
                ),
            })
            .end(
                Button::new("End task")
                    .enabled(can_end)
                    .on_press(SysmonMsg::EndSelected),
            );

        let body = match shown {
            0 if filtering => EmptyState::new(Icon::Search, "No process matches")
                .body("Search for another name, user or ID.")
                .into(),
            _ => {
                let rows = self
                    .order
                    .iter()
                    .map(|&i| {
                        let p = &processes[i];
                        vec![
                            cell(p.name.clone()),
                            cell(String::from(self.user_of(p))),
                            cell(percent_tenths(p.cpu)),
                            cell(p.memory.map_or_else(String::new, |b| Bytes(b).to_string())),
                            cell(number(u64::from(p.threads))),
                            cell(cpu_time(p.cpu_time_us)),
                            cell(p.key.pid.to_string()),
                        ]
                    })
                    .collect();
                Node::Table {
                    columns: self.table_columns(),
                    rows,
                    selected: self.selected_row(),
                    on_select: Some(SysmonMsg::Select),
                    on_activate: None,
                    on_header_click: Some(SysmonMsg::Sort),
                    on_context_menu: Some(SysmonMsg::OpenContextMenu),
                    shows: 0,
                }
            }
        };

        let threads = self.monitor.thread_count() as u64;
        let tally = format!(
            "{}, {}",
            count(self.monitor.process_count() as u64, "process", "processes"),
            count(threads, "thread", "threads")
        );
        let asking = self.asking.as_ref().map(|target| KillOutcome::Asking {
            name: target.name.clone(),
        });
        let (said, tone) = status_line(asking.as_ref().or(self.last_kill.as_ref()));
        let content = column(
            0,
            vec![
                toolbar.into(),
                Node::Expand {
                    weight: 1,
                    child: Box::new(body),
                },
                StatusBar::new(said).tone(tone).end(tally).into(),
            ],
        );
        let mut layers = vec![content];
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
                    enabled: self.is_killable(menu.target.pid()) && self.asking.is_none(),
                    kind: MenuItemKind::Action,
                }],
                on_action: Some(|item| match item {
                    MENU_KILL => SysmonMsg::MenuEnd,
                    _ => SysmonMsg::CloseMenu,
                }),
            }),
        }
    }

    pub(crate) fn view_kill_dialog(&self) -> Option<Node<SysmonMsg>> {
        let target = self.confirm_kill.as_ref()?;
        Some(
            Dialog::new(format!("End “{}”?", target.name))
                .content(
                    Text::new(format!(
                        "Process {} stops at once, without saving its work.",
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
}

pub fn status_line(outcome: Option<&KillOutcome>) -> (String, slopos_appkit::Tone) {
    use slopos_appkit::Tone;
    match outcome {
        Some(KillOutcome::Sent { name }) => (format!("Ended “{name}”."), Tone::Success),
        Some(KillOutcome::Failed { name }) => {
            (format!("Couldn't end “{name}”. Try again."), Tone::Danger)
        }
        Some(KillOutcome::Vanished { name }) => (
            format!("The task “{name}” had already ended."),
            Tone::Warning,
        ),
        Some(KillOutcome::Powerful { name }) => (
            format!("Couldn't end “{name}”, because it holds powers over the computer."),
            Tone::Danger,
        ),
        Some(KillOutcome::System { name }) => (
            format!("Couldn't end “{name}”, because the system runs it."),
            Tone::Danger,
        ),
        Some(KillOutcome::Unowned { name }) => (
            format!("Couldn't end “{name}”, because its user couldn't be found."),
            Tone::Danger,
        ),
        Some(KillOutcome::Refused { name }) => (
            format!("Couldn't end “{name}”, because you don't have permission to."),
            Tone::Danger,
        ),
        Some(KillOutcome::Asking { name }) => (
            format!("Waiting for permission to end “{name}”."),
            Tone::Secondary,
        ),
        Some(KillOutcome::Denied {
            name,
            why,
            by_person,
        }) => (
            match why {
                Some(why) => format!("Couldn't end “{name}”. {why}"),
                None => format!("Couldn't end “{name}”. Try again."),
            },
            match by_person {
                true => Tone::Secondary,
                false => Tone::Danger,
            },
        ),
        None => (
            String::from("Choose a process, then End task or Delete ends it."),
            Tone::Secondary,
        ),
    }
}
