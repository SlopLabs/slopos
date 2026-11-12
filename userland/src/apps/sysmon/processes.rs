use std::sync::LazyLock;

use slopos_appkit::theme::words::Bytes;
use slopos_appkit::theme::{inset, space, stroke};
use slopos_appkit::{
    Button, ButtonRow, CrossAxisAlignment, Dialog, EmptyState, Icon, Length, MenuItem,
    MenuItemKind, Node, Page, PropertyList, SearchField, SortIndicator, StatusBar, Surface,
    TableColumn, TableColumnWidth, Text, TextAlignment, TextRole, Tone, Toolbar, column,
    nice_ceiling, row,
};

use super::format::{count, cpu_time, number, percent, percent_tenths, task_state};
use super::monitor::{Process, WHOLE};
use super::performance::{CPU, MEMORY, caption, expand, scaled, span};
use super::state::{ContextMenu, Ending, KillOutcome, Runner, SortColumn, SysmonApp};
use super::{MENU_KILL, SysmonMsg};

const SEARCH_WIDTH: i32 = 240;
const INSPECTOR_WIDTH: i32 = 300;
/// The least a process's graphs span, so one at rest draws flat.
const BUSY_FLOOR: u64 = WHOLE / 10;
const RESIDENT_FLOOR: u64 = 1024 * 1024;

pub(crate) struct Column {
    heading: &'static str,
    pub(crate) sort: SortColumn,
    width: TableColumnWidth,
    align: TextAlignment,
}

impl Column {
    fn of_figures<'a>(
        heading: &'static str,
        sort: SortColumn,
        values: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        Self {
            heading,
            sort,
            width: TableColumnWidth::fitting(heading, values),
            align: TextAlignment::End,
        }
    }
}

const LONGEST_PID: u32 = 9_999_999;
const LONG_USER: &str = "administrator";
const WIDE_MEMORY: &str = "1023.9 MiB";

pub(crate) static COLUMNS: LazyLock<[Column; 5]> = LazyLock::new(|| {
    [
        Column {
            heading: "Name",
            sort: SortColumn::Name,
            width: TableColumnWidth::Flex(1),
            align: TextAlignment::Start,
        },
        Column {
            heading: "User",
            sort: SortColumn::User,
            width: TableColumnWidth::fitting("User", [LONG_USER]),
            align: TextAlignment::Start,
        },
        Column::of_figures("CPU", SortColumn::Cpu, [percent_tenths(1000).as_str()]),
        Column::of_figures("Memory", SortColumn::Memory, [WIDE_MEMORY]),
        Column::of_figures("PID", SortColumn::Pid, [LONGEST_PID.to_string().as_str()]),
    ]
});

fn runner_icon(runner: Runner) -> Icon {
    match runner {
        Runner::Person => Icon::AppWindow,
        Runner::System => Icon::Settings,
        Runner::Kernel => Icon::Cpu,
    }
}

fn named(process: &Process) -> Node<SysmonMsg> {
    row(
        space::SM,
        vec![
            Node::Icon {
                icon: runner_icon(Runner::of(process)),
                role: TextRole::Body,
                tone: Tone::Secondary,
            },
            expand(Text::new(process.name.clone()).into()),
        ],
    )
}

/// A figure at the end of its cell, quiet when it says nothing is going on.
fn figured(text: String, quiet: bool) -> Node<SysmonMsg> {
    let tone = match quiet {
        true => Tone::Secondary,
        false => Tone::Normal,
    };
    Text::new(text).align(TextAlignment::End).tone(tone).into()
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
                align: column.align,
                sort_indicator: (self.sort_column == column.sort).then_some(sort),
            })
            .collect()
    }

    /// The least width the tab lays out at: the table beside what is said of
    /// the chosen process.
    pub(crate) fn processes_least_width(&self) -> i32 {
        let icon = TextRole::Body.style().icon + space::SM;
        icon + TableColumn::least_width(&self.table_columns()) + stroke::EDGE + INSPECTOR_WIDTH
    }

    pub(crate) fn view_processes(&self) -> Node<SysmonMsg> {
        let shown = self.order.len();
        let filtering = !self.filter.text.trim().is_empty();
        let found = match filtering {
            true => format!("{shown} found"),
            false => String::new(),
        };
        let toolbar = Toolbar::new().start(Node::SizedBox {
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
        });

        let listed = match shown {
            0 if filtering => EmptyState::new(Icon::Search, "No process matches")
                .body("Search for another name, user or ID.")
                .into(),
            _ => self.table(),
        };

        let asking = self.asking.as_ref().map(|target| KillOutcome::Asking {
            name: target.name.clone(),
        });
        let (said, tone) = status_line(asking.as_ref().or(self.last_kill.as_ref()));
        let content = column(
            0,
            vec![
                toolbar.into(),
                expand(Node::HStack {
                    children: vec![expand(listed), Node::Divider, self.inspector()],
                    spacing: 0,
                    align: CrossAxisAlignment::Stretch,
                }),
                StatusBar::new(said).tone(tone).end(self.tally()).into(),
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

    fn table(&self) -> Node<SysmonMsg> {
        let processes = self.processes();
        let rows = self
            .order
            .iter()
            .map(|&i| {
                let p = &processes[i];
                vec![
                    named(p),
                    Text::new(self.user_of(p)).tone(Tone::Secondary).into(),
                    figured(percent_tenths(p.cpu), p.cpu == 0),
                    figured(
                        p.memory.map_or_else(String::new, |b| Bytes(b).to_string()),
                        false,
                    ),
                    figured(p.key.pid.to_string(), true),
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

    /// Every row counted once: the kernel's own tasks are rows too, though
    /// no process.
    fn tally(&self) -> String {
        let processes = self.monitor.process_count();
        let system = self.processes().len() - processes;
        let mut tally = vec![count(processes as u64, "process", "processes")];
        if system > 0 {
            tally.push(count(system as u64, "system task", "system tasks"));
        }
        let threads = self.monitor.thread_count() as u64;
        tally.push(count(threads, "thread", "threads"));
        tally.join(", ")
    }

    fn inspector(&self) -> Node<SysmonMsg> {
        let said = match self.chosen() {
            Some(process) => self.chosen_page(process),
            None => EmptyState::new(Icon::Activity, "No process chosen")
                .body("Choose one to see what it is doing, and to end it.")
                .into(),
        };
        Node::SizedBox {
            width: Some(Length::Px(INSPECTOR_WIDTH)),
            height: None,
            child: Box::new(Node::Background {
                surface: Surface::Base,
                child: Box::new(said),
            }),
        }
    }

    /// The row already says the process's user and figures, so here is what
    /// it does not: how they moved, and where the process came from.
    fn chosen_page(&self, process: &Process) -> Node<SysmonMsg> {
        let busy_top = nice_ceiling(process.busy.peak().max(BUSY_FLOOR), WHOLE);
        let mut graphs = vec![scaled(
            "CPU",
            percent(busy_top),
            self.graph("CPU of the process")
                .line(process.busy.values(), CPU)
                .max(busy_top)
                .small(),
        )];
        if process.memory.is_some() {
            let top = nice_ceiling(process.resident.peak().max(RESIDENT_FLOOR), 1024);
            graphs.push(scaled(
                "Memory",
                Bytes(top).to_string(),
                self.graph("Memory of the process")
                    .line(process.resident.values(), MEMORY)
                    .max(top)
                    .small(),
            ));
        }

        let mut details = PropertyList::new();
        if let Some(parent) = process.parent.and_then(|pid| self.listed(pid)) {
            details = details.row("Started by", parent.name.clone());
        }
        details = details
            .row("Threads", number(u64::from(process.threads)))
            .row("CPU time", cpu_time(process.cpu_time_us));
        if let Some(peak) = process.peak_memory {
            details = details.row("Memory", format!("{} at most", Bytes(peak)));
        }

        let state = task_state(process.state).1.to_lowercase();
        let page = Page::new(process.name.clone())
            .subtitle(format!("Process {}, {state}", process.key.pid))
            .content(column(
                space::XL,
                vec![
                    column(space::XS, vec![column(space::LG, graphs), span()]),
                    details.into(),
                ],
            ));

        let end = Button::new("End task…")
            .enabled(self.ending.is_some_and(Ending::offered))
            .on_press(SysmonMsg::EndSelected);
        let ending = column(
            space::MD,
            vec![
                caption(ending_line(self.ending)).wrap().into(),
                ButtonRow::new().aside(end).into(),
            ],
        );
        column(
            0,
            vec![
                expand(page.into()),
                Node::Divider,
                Node::Padding {
                    padding: inset::SECTION,
                    child: Box::new(ending),
                },
            ],
        )
    }

    fn view_context_menu(&self, menu: &ContextMenu) -> Node<SysmonMsg> {
        Node::Popup {
            x: menu.x,
            y: menu.y,
            on_dismiss: Some(SysmonMsg::CloseMenu),
            child: Box::new(Node::Menu {
                items: vec![MenuItem {
                    label: "End task…",
                    shortcut: Some("Delete"),
                    enabled: self.ending.is_some_and(Ending::offered),
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

/// What ending the chosen process would come to, said before anyone asks;
/// `None` is one that ended since it was chosen.
pub fn ending_line(ending: Option<Ending>) -> &'static str {
    match ending {
        Some(Ending::Here) => "Ending it stops it at once, without saving its work.",
        Some(Ending::Asked) => "Another user runs it, so ending it asks for permission first.",
        Some(Ending::Itself) => "This is System Monitor. Close its window to end it.",
        Some(Ending::Powerful) => "It holds powers over the computer, so it can't be ended.",
        Some(Ending::System) => "The system runs it, so it can't be ended.",
        Some(Ending::Unowned) => "Its user couldn't be found, so it can't be ended.",
        None => "It has already ended.",
    }
}

/// What came of the last ending asked for, and nothing before one is.
pub fn status_line(outcome: Option<&KillOutcome>) -> (String, Tone) {
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
        None => (String::new(), Tone::Secondary),
    }
}
