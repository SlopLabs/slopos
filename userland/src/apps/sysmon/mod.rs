use std::io::Write;

use crate::apps::endtask::{self, Endable};
use crate::syscall::process as sys_proc;
use slopos_abi::errno::Errno;
use slopos_abi::signal::SIGKILL;
use slopos_appkit::{
    Action, App, AppSender, ContextMenuAt, Key, LineEditInput, Modifiers, NamedKey, Node,
    SIDEBAR_WIDE_WIDTH, TableColumn, WindowSpec,
};

mod format;
pub mod monitor;
mod performance;
mod processes;
pub mod sample;
pub mod selection;
mod state;

pub(crate) use format::task_name_string;
pub use processes::status_line;
use selection::TaskKey;
use state::{ContextMenu, KillTarget};
pub use state::{KillOutcome, Resource, SysmonApp, Tab};

const MENU_KILL: usize = 0;

pub(crate) const REFRESH_INTERVAL_MS: u64 = 1000;
pub const WIDTH: i32 = 980;
pub const HEIGHT: i32 = 680;
const PAGE_LEAST_WIDTH: i32 = 400;
const LEAST_HEIGHT: i32 = 480;

#[derive(Clone, Debug)]
pub enum SysmonMsg {
    Tab(usize),
    Resource(usize),
    Filter(LineEditInput),
    Sort(usize),
    Select(usize),
    OpenContextMenu(ContextMenuAt),
    MenuEnd,
    CloseMenu,
    EndSelected,
    Kill,
    Cancel,
    /// What came of asking privd to end a task.
    Asked {
        key: TaskKey,
        asked: Asked,
    },
}

#[derive(Clone, Debug)]
pub enum Asked {
    Ran(i32),
    NotRun {
        why: Option<String>,
        by_person: bool,
    },
}

impl App for SysmonApp {
    type Message = SysmonMsg;

    fn view(&self) -> Node<SysmonMsg> {
        let content = Tab::ALL
            .iter()
            .map(|&tab| match (tab == self.tab, tab) {
                (false, _) => Node::Empty,
                (true, Tab::Performance) => self.view_performance(),
                (true, Tab::Processes) => self.view_processes(),
            })
            .collect();
        let tabs = Node::TabBar {
            tabs: Tab::ALL.iter().map(|t| String::from(t.name())).collect(),
            active: Tab::ALL.iter().position(|&t| t == self.tab).unwrap_or(0),
            on_change: Some(SysmonMsg::Tab),
            content,
        };
        let mut layers = vec![tabs];
        layers.extend(self.view_kill_dialog());
        Node::ZStack { children: layers }
    }

    fn update(&mut self, msg: SysmonMsg) -> Action {
        match msg {
            SysmonMsg::Tab(index) => {
                self.tab = Tab::ALL.get(index).copied().unwrap_or(Tab::Performance);
                self.confirm_kill = None;
                self.context_menu = None;
                self.filter_focused = false;
            }
            SysmonMsg::Resource(row) => {
                if let Some(resource) = self.resources().into_iter().nth(row) {
                    self.resource = resource;
                }
            }
            SysmonMsg::Filter(input) => self.filter_input(input),
            SysmonMsg::Sort(index) => {
                if let Some(column) = processes::COLUMNS.get(index) {
                    self.sort_by(column.sort);
                }
            }
            SysmonMsg::Select(row) => {
                self.select_row(row);
                self.filter_focused = false;
            }
            SysmonMsg::OpenContextMenu(at) => {
                self.select_row(at.row);
                self.last_kill = None;
                self.context_menu = self.target_for_row(at.row).map(|target| ContextMenu {
                    target,
                    x: at.x,
                    y: at.y,
                });
            }
            SysmonMsg::MenuEnd => {
                if let Some(menu) = self.context_menu.take() {
                    self.choose_to_end(menu.target);
                }
            }
            SysmonMsg::CloseMenu => self.context_menu = None,
            SysmonMsg::EndSelected => {
                self.context_menu = None;
                if let Some(target) = self.target_for_selection() {
                    self.choose_to_end(target);
                }
            }
            SysmonMsg::Kill => {
                // A refresh since the dialog opened may have recycled the id.
                self.last_kill = match self.pending_kill_target().cloned() {
                    Some(target) => self.end(target),
                    None => self.confirm_kill.as_ref().map(|t| KillOutcome::Vanished {
                        name: t.name.clone(),
                    }),
                };
                self.confirm_kill = None;
                self.refresh();
            }
            SysmonMsg::Cancel => self.confirm_kill = None,
            SysmonMsg::Asked { key, asked } => {
                let Some(target) = self.asking.take_if(|target| target.key == key) else {
                    return Action::None;
                };
                let name = target.name;
                self.last_kill = Some(match asked {
                    Asked::Ran(0) => KillOutcome::Sent { name },
                    Asked::Ran(endtask::GONE) => KillOutcome::Vanished { name },
                    Asked::Ran(endtask::POWERFUL) => KillOutcome::Powerful { name },
                    Asked::Ran(endtask::SYSTEM) => KillOutcome::System { name },
                    Asked::Ran(_) => KillOutcome::Failed { name },
                    Asked::NotRun { why, by_person } => KillOutcome::Denied {
                        name,
                        why,
                        by_person,
                    },
                });
                self.refresh();
            }
        }
        Action::Rebuild
    }

    fn on_start(&mut self, sender: AppSender<SysmonMsg>) -> Action {
        self.sender = Some(sender);
        Action::None
    }

    fn page_key(&self) -> u64 {
        Tab::ALL.iter().position(|&t| t == self.tab).unwrap_or(0) as u64
    }

    fn on_key(&mut self, key: Key, modifiers: Modifiers) -> Action {
        if self.tab != Tab::Processes {
            return Action::None;
        }
        match key {
            Key::Char('f') if modifiers.ctrl => {
                self.filter_focused = true;
                Action::Rebuild
            }
            Key::Named(NamedKey::Delete) if !self.filter_focused => {
                self.context_menu = None;
                if let Some(target) = self.target_for_selection() {
                    self.choose_to_end(target);
                }
                Action::Rebuild
            }
            _ => Action::None,
        }
    }

    fn on_resize(&mut self, width: u32, _height: u32) -> Action {
        self.width = width as i32;
        Action::Rebuild
    }

    fn tick_interval_ms(&self) -> Option<u64> {
        Some(REFRESH_INTERVAL_MS)
    }

    fn tick(&mut self) -> Action {
        self.refresh();
        Action::Rebuild
    }

    fn title(&self) -> String {
        "System Monitor".into()
    }

    fn app_id(&self) -> &str {
        "org.slopos.sysmon"
    }

    fn window(&self) -> WindowSpec {
        let least = TableColumn::least_width(&self.table_columns())
            .max(SIDEBAR_WIDE_WIDTH + PAGE_LEAST_WIDTH);
        WindowSpec::new(WIDTH.max(least), HEIGHT).min_size(least, LEAST_HEIGHT)
    }
}

impl SysmonApp {
    fn filter_input(&mut self, input: LineEditInput) {
        match input {
            LineEditInput::Text { character } => self.filter.insert(character),
            LineEditInput::Key {
                key: Key::Named(NamedKey::Escape),
                ..
            } => self.filter.clear(),
            LineEditInput::Key { key, modifiers } => {
                self.filter.handle_key(key, modifiers);
            }
            LineEditInput::Caret { col } => self.filter.set_caret(col),
            LineEditInput::Focus => self.filter_focused = true,
            LineEditInput::Blur => self.filter_focused = false,
        }
        self.reorder();
    }

    /// Confirm here a task the kernel lets this process end, or one root may
    /// have privd end unasked; for any other, privd's question is the
    /// confirmation.
    fn choose_to_end(&mut self, target: KillTarget) {
        self.last_kill = None;
        self.confirm_kill = None;
        if !self.is_killable(target.pid()) || self.asking.is_some() {
            return;
        }
        match sys_proc::kill(target.pid(), 0) {
            0 => self.confirm_kill = Some(target),
            rc if rc == Errno::EPERM.raw() && sys_proc::getuid() == 0 => {
                match endtask::endable(target.pid()) {
                    Endable::Yes { name, .. } if name == target.name => {
                        self.confirm_kill = Some(target);
                    }
                    endable => self.last_kill = Some(not_ended(target.name, endable)),
                }
            }
            rc => self.last_kill = self.refused(target, rc),
        }
    }

    /// `None` while privd is asked.
    fn end(&mut self, target: KillTarget) -> Option<KillOutcome> {
        match sys_proc::kill(target.pid(), SIGKILL) {
            0 => Some(KillOutcome::Sent { name: target.name }),
            rc => self.refused(target, rc),
        }
    }

    /// What follows the kernel refusing `target` with `rc`: a task of another
    /// person's session that holds no powers is privd's to end once someone
    /// allows it, and `None` while privd is asked.
    fn refused(&mut self, target: KillTarget, rc: i32) -> Option<KillOutcome> {
        let name = target.name.clone();
        if rc == Errno::ESRCH.raw() {
            return Some(match endtask::endable(target.pid()) {
                Endable::System => KillOutcome::System { name },
                _ => KillOutcome::Vanished { name },
            });
        }
        if rc != Errno::EPERM.raw() {
            return Some(KillOutcome::Failed { name });
        }
        match endtask::endable(target.pid()) {
            Endable::Yes { name: now, user } if now == name => self.ask_to_end(target, user),
            endable => Some(not_ended(name, endable)),
        }
    }

    /// On a thread of its own, since privd answers once someone has.
    fn ask_to_end(&mut self, target: KillTarget, user: String) -> Option<KillOutcome> {
        let refused = KillOutcome::Refused {
            name: target.name.clone(),
        };
        let Some(sender) = self.sender.clone() else {
            return Some(refused);
        };
        let (key, pid, name) = (target.key, target.pid().to_string(), target.name.clone());
        let spawned = std::thread::Builder::new()
            .name("sysmon-end".into())
            .spawn(move || {
                let asked =
                    match crate::privd::run_quietly(endtask::ACTION, &[&pid, &name, &user], b"") {
                        Ok(code) => Asked::Ran(code),
                        Err(not) => {
                            // Not `eprintln!`, whose panic would end the window.
                            let _ = writeln!(
                                std::io::stderr(),
                                "sysmon: {}: {}",
                                endtask::ACTION,
                                not.reason
                            );
                            Asked::NotRun {
                                why: not.from_privd.then_some(not.reason),
                                by_person: not.by_person,
                            }
                        }
                    };
                sender.send(SysmonMsg::Asked { key, asked });
            });
        match spawned {
            Ok(_) => {
                self.asking = Some(target);
                None
            }
            Err(_) => Some(refused),
        }
    }
}

/// Why `name`, which privd will not be asked to end, was not ended.
fn not_ended(name: String, endable: Endable) -> KillOutcome {
    match endable {
        Endable::Yes { .. } | Endable::Gone => KillOutcome::Vanished { name },
        Endable::Powerful => KillOutcome::Powerful { name },
        Endable::System => KillOutcome::System { name },
        Endable::Unknown => KillOutcome::Unowned { name },
    }
}

pub const PAGES: [&str; 6] = ["cpu", "memory", "disk", "network", "gpu", "processes"];

pub fn sysmon_main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let page = match args.as_slice() {
        [flag, name] if flag == "--page" => PAGES.iter().find(|p| *p == name),
        [] => Some(&PAGES[0]),
        _ => None,
    };
    let Some(page) = page else {
        eprintln!("usage: sysmon [--page {}]", PAGES.join("|"));
        crate::syscall::core::exit_with_code(2)
    };
    let mut app = SysmonApp::new();
    app.open(page);
    slopos_appkit::run_app(app)
}
