//! The Settings app as data: its panels, what each shows and holds, and what
//! it asks the system to do, which the window running it carries out and
//! answers as messages. Nothing here reads a file or asks privd, so every
//! page is laid out and audited on the host.

pub mod about;
pub mod desktop;
pub mod field;
pub mod keyboard;
pub mod network;
pub mod users;

#[cfg(test)]
mod tests;

use slopos_appkit::{
    Action, Icon, LineEditInput, List, ListRow, Node, Page, SearchField, Sidebar, column,
};
use slopos_theme_core::space;

use field::Field;

pub const APP_ID: &str = "org.slopos.settings";
pub const TITLE: &str = "Settings";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Network,
    Users,
    Keyboard,
    Desktop,
    About,
}

impl Panel {
    pub const ALL: [Panel; 5] = [
        Panel::Network,
        Panel::Users,
        Panel::Keyboard,
        Panel::Desktop,
        Panel::About,
    ];

    /// What `settings --page` names it by.
    pub const fn id(self) -> &'static str {
        match self {
            Panel::Network => "network",
            Panel::Users => "users",
            Panel::Keyboard => "keyboard",
            Panel::Desktop => "desktop",
            Panel::About => "about",
        }
    }

    pub fn named(id: &str) -> Option<Panel> {
        Panel::ALL.into_iter().find(|p| p.id() == id)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Panel::Network => "Network",
            Panel::Users => "Users",
            Panel::Keyboard => "Keyboard",
            Panel::Desktop => "Desktop",
            Panel::About => "About",
        }
    }

    pub const fn icon(self) -> Icon {
        match self {
            Panel::Network => Icon::Network,
            Panel::Users => Icon::Users,
            Panel::Keyboard => Icon::Keyboard,
            Panel::Desktop => Icon::Wallpaper,
            Panel::About => Icon::Info,
        }
    }

    /// The words a search finds it by, beside its name.
    const fn keywords(self) -> &'static [&'static str] {
        match self {
            Panel::Network => &[
                "ethernet", "internet", "adapter", "address", "dhcp", "dns", "gateway", "ip",
                "cable", "wired",
            ],
            Panel::Users => &[
                "people",
                "accounts",
                "password",
                "administrator",
                "login",
                "automatic",
                "user",
            ],
            Panel::Keyboard => &["layout", "keymap", "language", "typing", "keys"],
            Panel::Desktop => &[
                "background",
                "wallpaper",
                "picture",
                "clock",
                "time",
                "appearance",
                "gradient",
            ],
            Panel::About => &[
                "version",
                "system",
                "processor",
                "memory",
                "storage",
                "hardware",
                "computer",
            ],
        }
    }

    fn found_by(self, typed: &str) -> bool {
        let typed = typed.trim().to_lowercase();
        typed.is_empty()
            || self.name().to_lowercase().starts_with(&typed)
            || self.keywords().iter().any(|word| word.starts_with(&typed))
    }
}

/// What the person running Settings is, which decides what a panel says
/// before they try anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Person {
    pub uid: u32,
    pub name: String,
    pub admin: bool,
}

#[derive(Clone, Debug)]
pub enum Msg {
    Panel(usize),
    Search(LineEditInput),
    Network(network::Msg),
    Users(users::Msg),
    Keyboard(keyboard::Msg),
    Desktop(desktop::Msg),
    About(about::Msg),
    Answered(Purpose, Asked),
}

/// What the window must do for the panels, each answered by a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Have privd run `action` with `args`, `input` on its standard input,
    /// and answer [`Asked`] for `purpose`.
    Ask(Ask),
    Network(network::Effect),
    Users(users::Effect),
    Keyboard(keyboard::Effect),
    Desktop(desktop::Effect),
    About(about::Effect),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    pub purpose: Purpose,
    pub action: &'static str,
    pub args: Vec<String>,
    pub input: Input,
}

/// What a mechanism reads on its standard input, passwords among it: never
/// shown by `Debug`, and wiped when dropped.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Input(Vec<u8>);

impl Input {
    /// Room for every line a request sends, so nothing is copied as it grows.
    pub fn new() -> Input {
        Input(Vec::with_capacity(4096))
    }

    pub fn line(&mut self, line: &str) {
        self.0.extend_from_slice(line.as_bytes());
        self.0.push(b'\n');
    }
}

impl From<String> for Input {
    fn from(text: String) -> Input {
        Input(text.into_bytes())
    }
}

impl core::ops::Deref for Input {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl core::fmt::Debug for Input {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "<{} bytes>", self.0.len())
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&self.0);
    }
}

/// Which panel's request an answer is to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Network,
    Users(users::Request),
    Keyboard,
}

/// How a request privd was asked for ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asked {
    /// The mechanism ran and exited with `status`, having said `said` on
    /// standard error and `notes` on standard output.
    Ran {
        status: i32,
        said: String,
        notes: String,
    },
    /// It never ran: privd's sentence, or why privd was not reached, and
    /// whether the person said no.
    Refused { why: String, by_person: bool },
}

impl Asked {
    /// What to tell the person of a request that did not do what it was
    /// asked, `None` once it did.
    pub fn trouble(&self) -> Option<String> {
        match self {
            Asked::Ran { status: 0, .. } => None,
            Asked::Ran { said, .. } => Some(said_plainly(said)),
            Asked::Refused {
                by_person: true, ..
            } => Some(String::from("You didn't allow it.")),
            Asked::Refused { why, .. } => Some(why.clone()),
        }
    }
}

/// A mechanism's last line, which says what went wrong, without the name
/// it prints before it.
fn said_plainly(said: &str) -> String {
    let last = said.lines().rev().find(|line| !line.trim().is_empty());
    let last = last.unwrap_or("It did not work.").trim();
    let last = last.split_once(": ").map_or(last, |(program, rest)| {
        match program.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
            true => rest,
            false => last,
        }
    });
    let mut said = String::new();
    let mut chars = last.chars();
    if let Some(first) = chars.next() {
        said.extend(first.to_uppercase());
        said.push_str(chars.as_str());
    }
    if !said.ends_with(['.', '?', '!']) {
        said.push('.');
    }
    said
}

pub struct Settings {
    pub panel: Panel,
    search: Field,
    pub network: network::Network,
    pub users: users::Users,
    pub keyboard: keyboard::Keyboard,
    pub desktop: desktop::Desktop,
    pub about: about::About,
}

impl Settings {
    pub fn new(panel: Panel, person: Person) -> Settings {
        Settings {
            panel,
            search: Field::default(),
            network: network::Network::new(person.admin),
            users: users::Users::new(person),
            keyboard: keyboard::Keyboard::default(),
            desktop: desktop::Desktop::default(),
            about: about::About::default(),
        }
    }

    /// What the window must do before the first view: read what every panel
    /// shows, the pictures' thumbnails last.
    pub fn start(&self) -> Vec<Effect> {
        vec![
            Effect::Network(network::Effect::Read),
            Effect::Users(users::Effect::Read),
            Effect::Keyboard(keyboard::Effect::Read),
            Effect::About(about::Effect::Read),
            Effect::Desktop(desktop::Effect::Read),
        ]
    }

    fn shown(&self) -> Vec<Panel> {
        Panel::ALL
            .into_iter()
            .filter(|p| p.found_by(self.search.text()))
            .collect()
    }

    pub fn view(&self) -> Node<Msg> {
        let shown = self.shown();
        let rows = shown
            .iter()
            .map(|p| ListRow::new(p.name()).icon(p.icon()))
            .collect();
        let field = SearchField::new(self.search.text(), self.search.input.caret)
            .focused(self.search.focused)
            .placeholder("Search settings")
            .invalid(shown.is_empty())
            .on_input(Msg::Search);
        let sidebar = match shown.is_empty() {
            true => Sidebar::empty("No setting matches what you typed."),
            false => Sidebar::new(
                List::new(rows)
                    .selected(shown.iter().position(|&p| p == self.panel))
                    .on_select(Msg::Panel),
            ),
        }
        .search(field);
        let (page, dialog) = match self.panel {
            Panel::Network => (self.network.view(), self.network.dialog()),
            Panel::Users => (self.users.view(), self.users.dialog()),
            Panel::Keyboard => (self.keyboard.view(), self.keyboard.dialog()),
            Panel::Desktop => (self.desktop.view(), self.desktop.dialog()),
            Panel::About => (self.about.view(), None),
        };
        let page = Node::Expand {
            weight: 1,
            child: Box::new(page),
        };
        let mut layers = vec![Node::HStack {
            children: vec![sidebar.into(), page],
            spacing: 0,
            align: slopos_appkit::CrossAxisAlignment::Stretch,
        }];
        layers.extend(dialog);
        Node::ZStack { children: layers }
    }

    pub fn update(&mut self, msg: Msg) -> (Action, Vec<Effect>) {
        let effects = match msg {
            Msg::Panel(row) => match self.shown().get(row) {
                Some(&panel) => self.show(panel),
                None => Vec::new(),
            },
            Msg::Search(input) => match self.search.edit(input) {
                field::Left::Enter => match self.shown().first() {
                    Some(&first) => self.show(first),
                    None => Vec::new(),
                },
                field::Left::Nothing => Vec::new(),
            },
            Msg::Network(network::Msg::Read(Ok(read))) if self.network.holds(&read) => {
                return (Action::None, Vec::new());
            }
            Msg::Network(msg) => self.network.update(msg),
            Msg::Users(msg) => {
                let effects = self.users.update(msg);
                self.network.set_admin(self.users.me_admin());
                effects
            }
            Msg::Keyboard(msg) => self.keyboard.update(msg),
            Msg::Desktop(msg) => self.desktop.update(msg),
            Msg::About(msg) => self.about.update(msg),
            Msg::Answered(Purpose::Network, asked) => self.network.answered(asked),
            Msg::Answered(Purpose::Users(request), asked) => self.users.answered(request, asked),
            Msg::Answered(Purpose::Keyboard, asked) => self.keyboard.answered(asked),
        };
        (Action::Rebuild, effects)
    }

    /// Show `panel`, reading the people again when what was read of them is
    /// not the whole list.
    fn show(&mut self, panel: Panel) -> Vec<Effect> {
        let again = panel == Panel::Users && self.panel != panel && !self.users.whole();
        self.panel = panel;
        match again {
            true => vec![Effect::Users(users::Effect::Read)],
            false => Vec::new(),
        }
    }

    /// How often the window reads the network again, while it shows it.
    pub fn tick_interval_ms(&self) -> Option<u64> {
        (self.panel == Panel::Network).then_some(network::REFRESH_MS)
    }

    pub fn tick(&mut self) -> Vec<Effect> {
        match self.panel {
            Panel::Network => vec![Effect::Network(network::Effect::Read)],
            _ => Vec::new(),
        }
    }
}

/// A panel's page: its name, a sentence about it and what it holds, the
/// buttons that end it when it has any.
fn page<M>(
    panel: Panel,
    about: &str,
    content: Vec<Node<M>>,
    footer: Option<slopos_appkit::ButtonRow<M>>,
) -> Node<M> {
    let page = Page::new(panel.name())
        .subtitle(about)
        .content(column(space::XL, content));
    match footer {
        Some(footer) => page.footer(footer).into(),
        None => page.into(),
    }
}
