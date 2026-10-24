//! `/bin/installer-ui` — the installer as a window, for the person at a live
//! system's desktop.
//!
//! It holds no capability and never touches a disk: its pages are
//! `slopos_installer_core::wizard::Wizard`'s, rendered, and what the wizard
//! asks for goes out through a [`Link`] — privd, which starts
//! `installer --serve` on two pipes once the person allows it, and the
//! engine's frames on them.

mod link;
mod view;

use slopos_abi::signal::SIGPIPE;
use slopos_appkit::{
    Action, App, AppSender, Key, LineEditInput, LineInput, Modifiers, NamedKey, Node, SliderGrab,
    SliderInput,
};
use slopos_installer_core::protocol::Reply;
use slopos_installer_core::wizard::{Input, Out, Page, Wizard};
use slopos_installer_core::{parse_size, size_text};

pub use link::{Link, Privd, relay};

use crate::syscall::{core as sys_core, process};

pub const WIDTH: u32 = 900;
pub const HEIGHT: u32 = 540;

/// The argument init opens the window with on a live boot, whose first page
/// asks whether to try SlopOS or install it; without it the window asks
/// privd for the engine at once.
pub const WELCOME: &str = "--welcome";

/// What privd, and then the engine, said.
#[derive(Clone, Debug)]
pub enum EngineSays {
    /// privd is asking the person.
    Asking,
    Started,
    Refused(String),
    Reply(Reply),
    Ended,
}

#[derive(Clone, Debug)]
pub enum Msg {
    Engine(EngineSays),
    RestartRefused(String),
    /// A row of the list on the page, by its place.
    Disk(usize),
    Tile(usize),
    Region(usize),
    Root(usize),
    Rescan,
    Size(SliderInput),
    SizeText(LineEditInput),
    Remote(LineEditInput),
    First,
    Confirm,
    Next,
    Back,
    Close,
    Restart,
    ShowLog,
    PageScroll(i32),
    LogScroll(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Size,
    Remote,
}

pub struct InstallerApp {
    wizard: Wizard,
    link: Box<dyn Link>,
    welcome: bool,
    sender: Option<AppSender<Msg>>,
    page: Page,
    asking: bool,
    restarting: bool,
    restart_refused: Option<String>,
    grab: Option<SliderGrab>,
    size_typed: Option<LineInput>,
    remote: LineInput,
    field: Option<Field>,
    show_log: bool,
    page_scroll: i32,
    log_scroll: i32,
}

impl InstallerApp {
    pub fn new(link: Box<dyn Link>, welcome: bool) -> InstallerApp {
        InstallerApp {
            wizard: Wizard::new(),
            link,
            welcome,
            sender: None,
            page: Page::Welcome,
            asking: false,
            restarting: false,
            restart_refused: None,
            grab: None,
            size_typed: None,
            remote: LineInput::new(),
            field: None,
            show_log: false,
            page_scroll: 0,
            log_scroll: i32::MAX,
        }
    }

    pub fn wizard(&self) -> &Wizard {
        &self.wizard
    }

    fn on(&mut self, input: Input) -> Action {
        if self.restarting && matches!(input, Input::Next | Input::Back) {
            return Action::None;
        }
        let outs = self.wizard.on(input);
        for out in outs {
            match out {
                Out::Start => {
                    self.asking = false;
                    if let Some(sender) = &self.sender {
                        self.link.start(sender);
                    }
                }
                Out::Send(request) => self.link.send(&request),
                Out::Restart if !self.restarting => {
                    self.restarting = true;
                    self.restart_refused = None;
                    if let Some(sender) = &self.sender {
                        self.link.restart(sender);
                    }
                }
                Out::Restart => {}
                Out::Close => return Action::Exit,
            }
        }
        if self.wizard.page() != self.page {
            self.page = self.wizard.page();
            self.page_scroll = 0;
            self.log_scroll = i32::MAX;
            self.field = None;
            self.size_typed = None;
            self.restart_refused = None;
        }
        if self.wizard.page() != Page::How || self.wizard.root_span().is_none() {
            self.grab = None;
        }
        Action::Rebuild
    }

    fn engine(&mut self, said: EngineSays) -> Action {
        match said {
            EngineSays::Asking => {
                self.asking = true;
                Action::Rebuild
            }
            EngineSays::Started => {
                self.asking = false;
                self.on(Input::Started)
            }
            EngineSays::Refused(why) => {
                self.asking = false;
                self.on(Input::Refused(why))
            }
            EngineSays::Reply(reply) => self.on(Input::Reply(reply)),
            EngineSays::Ended => {
                self.asking = false;
                self.on(Input::Ended)
            }
        }
    }

    fn slide(&mut self, input: SliderInput) -> Action {
        let size = match input {
            SliderInput::Begin(size, grab) => {
                self.grab = Some(grab);
                size
            }
            SliderInput::Change(size) => size,
            SliderInput::End => {
                self.grab = None;
                return Action::Rebuild;
            }
        };
        self.size_typed = None;
        self.resize(size)
    }

    /// Ask for a root of `size`, or all the region gives at its end or past
    /// it.
    fn resize(&mut self, size: u64) -> Action {
        let whole = self.wizard.root_span().is_some_and(|span| size >= span.max);
        let asked = (!whole).then_some(size);
        if self.wizard.answers().map(|a| a.root_size) == Some(asked) {
            return Action::Rebuild;
        }
        self.on(Input::RootSize(asked))
    }

    fn edit(&mut self, field: Field, input: LineEditInput) -> Action {
        match input {
            LineEditInput::Blur if self.field == Some(field) => {
                self.field = None;
                if field == Field::Size {
                    self.size_typed = None;
                }
                return Action::Rebuild;
            }
            LineEditInput::Blur => return Action::Rebuild,
            LineEditInput::Focus => {
                self.field = Some(field);
                return Action::Rebuild;
            }
            _ => self.field = Some(field),
        }
        let shown = self.size_shown();
        let line = match field {
            Field::Remote => &mut self.remote,
            Field::Size => self
                .size_typed
                .get_or_insert_with(|| LineInput::with_text(shown)),
        };
        let before = line.text.clone();
        match input {
            LineEditInput::Text { character } => line.insert(character),
            LineEditInput::Caret { col } => line.set_caret(col),
            LineEditInput::Key { key, modifiers } if !line.handle_key(key, modifiers) => {
                let unread = field == Field::Size && typed_size(&line.text).is_none();
                if unread && key == Key::Named(NamedKey::Enter) {
                    return Action::Rebuild;
                }
                return self.on_key(key, modifiers);
            }
            _ => {}
        }
        if line.text == before {
            return Action::Rebuild;
        }
        let text = line.text.clone();
        match field {
            Field::Remote => self.on(Input::Remote(text)),
            Field::Size => match typed_size(&text) {
                Some(bytes) => self.resize(bytes),
                None => Action::Rebuild,
            },
        }
    }

    fn size_shown(&self) -> String {
        self.wizard.root_size().map(size_text).unwrap_or_default()
    }

    fn indeterminate(&self) -> bool {
        match self.page {
            Page::Welcome => self.wizard.waiting(),
            Page::Install => self.wizard.progress().bytes.is_none(),
            _ => false,
        }
    }
}

/// A size as the box takes one: [`parse_size`]'s in either case, spaces
/// ignored, and a bare number in GiB, which is what a root is sized in.
fn typed_size(text: &str) -> Option<u64> {
    let text: String = text.split_whitespace().collect();
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        return parse_size(&format!("{text}G"));
    }
    parse_size(&text.to_ascii_uppercase().replace("IB", "iB"))
}

impl App for InstallerApp {
    type Message = Msg;

    fn view(&self) -> Node<Msg> {
        view::window(self)
    }

    fn on_start(&mut self, sender: AppSender<Msg>) -> Action {
        self.sender = Some(sender);
        match self.welcome {
            true => Action::None,
            false => self.on(Input::Next),
        }
    }

    fn update(&mut self, msg: Msg) -> Action {
        match msg {
            Msg::Engine(said) => self.engine(said),
            Msg::RestartRefused(why) => {
                self.restarting = false;
                self.restart_refused = Some(why);
                Action::Rebuild
            }
            Msg::Disk(row) => match self.wizard.disks().get(row) {
                Some(disk) => self.on(Input::Disk(disk.key.clone())),
                None => Action::None,
            },
            Msg::Tile(row) => match self.wizard.tile_rows().get(row) {
                Some(tile) => self.on(Input::Kind(tile.key)),
                None => Action::None,
            },
            Msg::Region(row) => match self.wizard.region_rows().get(row) {
                Some(region) => self.on(Input::Region(region.key)),
                None => Action::None,
            },
            Msg::Root(row) => match self.wizard.root_rows().get(row) {
                Some(root) => self.on(Input::Root(root.key)),
                None => Action::None,
            },
            Msg::Rescan => self.on(Input::Rescan),
            Msg::Size(input) => self.slide(input),
            Msg::SizeText(input) => self.edit(Field::Size, input),
            Msg::Remote(input) => self.edit(Field::Remote, input),
            Msg::First => {
                let first = self.wizard.answers().is_some_and(|a| a.first);
                self.on(Input::First(!first))
            }
            Msg::Confirm => self.on(Input::Confirm(!self.wizard.confirmed())),
            Msg::Next => self.on(Input::Next),
            Msg::Back => self.on(Input::Back),
            Msg::Close => self.on(Input::Close),
            Msg::Restart => self.on(Input::Restart),
            Msg::ShowLog => {
                self.show_log = !self.show_log;
                self.log_scroll = i32::MAX;
                Action::Rebuild
            }
            Msg::PageScroll(y) => {
                self.page_scroll = y;
                Action::None
            }
            Msg::LogScroll(y) => {
                self.log_scroll = y;
                Action::None
            }
        }
    }

    /// Enter is not the primary button on Review, whose button erases, nor on
    /// Done, whose button restarts the machine before the medium may be out.
    fn on_key(&mut self, key: Key, _modifiers: Modifiers) -> Action {
        match key {
            Key::Named(NamedKey::Escape) if self.page != Page::Install => self.on(Input::Back),
            Key::Named(NamedKey::Enter) if !matches!(self.page, Page::Review | Page::Done) => {
                self.on(Input::Next)
            }
            _ => Action::None,
        }
    }

    fn page_key(&self) -> u64 {
        self.page as u64
    }

    fn tick_interval_ms(&self) -> Option<u64> {
        self.indeterminate().then_some(50)
    }

    fn tick(&mut self) -> Action {
        Action::Repaint
    }

    fn on_close_request(&mut self) -> (bool, Action) {
        match self.on(Input::Close) {
            Action::Exit => (true, Action::None),
            action => (false, action),
        }
    }

    fn title(&self) -> &str {
        "Install SlopOS"
    }

    fn app_id(&self) -> &str {
        "org.slopos.installer"
    }
}

pub fn installer_ui_main() {
    if !crate::privd::on_the_medium() {
        eprintln!("installer-ui: SlopOS installs from the medium it booted; boot the SlopOS ISO");
        sys_core::exit_with_code(1);
    }
    process::ignore_signal(SIGPIPE);
    let welcome = std::env::args().nth(1).as_deref() == Some(WELCOME);
    let app = InstallerApp::new(Box::new(Privd::default()), welcome);
    slopos_appkit::run_app(app, WIDTH, HEIGHT);
}
