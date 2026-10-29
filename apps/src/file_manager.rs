//! Files: the folders of the computer, a place at a time.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use slopos_appkit::theme::words::Bytes;
use slopos_appkit::theme::{space, stroke};
use slopos_appkit::{
    Action, App, Button, CrossAxisAlignment, EdgeInsets, EmptyState, Icon, IconButton, Key, List,
    ListRow, Modifiers, NamedKey, Node, Refusal, SIDEBAR_WIDTH, Sidebar, StatusBar, TableColumn,
    TableColumnWidth, Text, TextAlignment, TextRole, Tone, Toolbar, WindowSpec, column,
    document_title, row,
};

use crate::refused;

pub const NAME: &str = "Files";

/// Wide enough for every size under a pebibyte.
fn size_width() -> TableColumnWidth {
    static WIDTH: OnceLock<TableColumnWidth> = OnceLock::new();
    *WIDTH.get_or_init(|| {
        // 1023.9 of each unit, the most digits it shows before the next.
        let longest = (1..=4).map(|unit| 1024u64.pow(unit) * 10_239 / 10);
        let sizes: Vec<String> = [1023]
            .into_iter()
            .chain(longest)
            .map(|n| Bytes(n).to_string())
            .collect();
        TableColumnWidth::fitting("Size", sizes.iter().map(String::as_str))
    })
}

fn columns() -> Vec<TableColumn> {
    vec![
        TableColumn {
            label: "Name".into(),
            width: TableColumnWidth::Flex(1),
            sort_indicator: None,
        },
        TableColumn {
            label: "Size".into(),
            width: size_width(),
            sort_indicator: None,
        },
    ]
}

fn least_width() -> i32 {
    let icon = TextRole::Body.style().icon + space::SM;
    SIDEBAR_WIDTH + stroke::EDGE + icon + TableColumn::least_width(&columns())
}

#[derive(Clone, Debug)]
pub enum FileMsg {
    Back,
    Forward,
    Up,
    Reload,
    Root,
    Select(usize),
    Open(usize),
    Place(usize),
}

struct Place {
    icon: Icon,
    name: &'static str,
    /// Where it is; the person's home when `None`.
    path: Option<&'static str>,
}

impl Place {
    fn path(&self) -> PathBuf {
        self.path.map_or_else(home, PathBuf::from)
    }
}

fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

const PLACES: &[Place] = &[
    Place {
        icon: Icon::Home,
        name: "Home",
        path: None,
    },
    Place {
        icon: Icon::HardDrive,
        name: "Computer",
        path: Some("/"),
    },
    Place {
        icon: Icon::Clock,
        name: "Temporary files",
        path: Some("/tmp"),
    },
    Place {
        icon: Icon::Usb,
        name: "Devices",
        path: Some("/dev"),
    },
];

struct Entry {
    name: String,
    folder: bool,
    size: u64,
}

pub struct FileManagerApp {
    path: PathBuf,
    entries: Result<Vec<Entry>, ErrorKind>,
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    selected: Option<usize>,
}

impl FileManagerApp {
    fn new() -> Self {
        let mut app = Self {
            path: home(),
            entries: Ok(Vec::new()),
            back: Vec::new(),
            forward: Vec::new(),
            selected: None,
        };
        app.reload();
        app
    }

    fn reload(&mut self) {
        self.selected = None;
        self.entries = fs::read_dir(&self.path).map_err(|e| e.kind()).map(|dir| {
            let mut entries: Vec<Entry> = dir
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let name = entry.file_name().into_string().ok()?;
                    let meta = entry.metadata().ok();
                    Some(Entry {
                        name,
                        folder: meta.as_ref().is_some_and(|m| m.is_dir()),
                        size: meta.map_or(0, |m| m.len()),
                    })
                })
                .collect();
            entries.sort_by(|a, b| {
                b.folder
                    .cmp(&a.folder)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            entries
        });
    }

    fn go(&mut self, path: PathBuf) {
        if path == self.path {
            return;
        }
        self.back.push(core::mem::replace(&mut self.path, path));
        self.forward.clear();
        self.reload();
    }

    fn go_back(&mut self) {
        if let Some(path) = self.back.pop() {
            self.forward.push(core::mem::replace(&mut self.path, path));
            self.reload();
        }
    }

    fn go_forward(&mut self) {
        if let Some(path) = self.forward.pop() {
            self.back.push(core::mem::replace(&mut self.path, path));
            self.reload();
        }
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.path.parent().map(Path::to_path_buf) {
            self.go(parent);
        }
    }

    fn open_selected(&mut self) {
        let Ok(entries) = &self.entries else {
            return;
        };
        if let Some(entry) = self.selected.and_then(|i| entries.get(i))
            && entry.folder
        {
            let next = self.path.join(&entry.name);
            self.go(next);
        }
    }

    fn place(&self) -> Option<usize> {
        PLACES.iter().position(|place| place.path() == self.path)
    }

    /// The folder as the person knows it: its place's name, or its own.
    fn folder_name(&self) -> String {
        match self.place() {
            Some(i) => PLACES[i].name.into(),
            None => self
                .path
                .file_name()
                .map_or_else(|| "/".into(), |n| n.to_string_lossy().into_owned()),
        }
    }

    fn sidebar(&self) -> Node<FileMsg> {
        let rows = PLACES
            .iter()
            .map(|place| ListRow::new(place.name).icon(place.icon))
            .collect();
        let places = List::sectioned(vec![("Places".into(), rows)])
            .selected(self.place())
            .on_select(FileMsg::Place);
        Sidebar::new(places).into()
    }

    fn toolbar(&self) -> Node<FileMsg> {
        Toolbar::new()
            .start(
                IconButton::new(Icon::ArrowLeft, "Back")
                    .on_press(FileMsg::Back)
                    .enabled(!self.back.is_empty()),
            )
            .start(
                IconButton::new(Icon::ArrowRight, "Forward")
                    .on_press(FileMsg::Forward)
                    .enabled(!self.forward.is_empty()),
            )
            .start(
                IconButton::new(Icon::ArrowUp, "Enclosing folder")
                    .on_press(FileMsg::Up)
                    .enabled(self.path.parent().is_some()),
            )
            .end(IconButton::new(Icon::RefreshCw, "Reload").on_press(FileMsg::Reload))
            .into()
    }

    fn location(&self) -> Node<FileMsg> {
        Node::Padding {
            padding: EdgeInsets::symmetric(space::LG, space::SM),
            child: Box::new(column(
                0,
                vec![
                    Text::new(self.folder_name()).role(TextRole::Heading).into(),
                    Text::new(self.path.display().to_string())
                        .role(TextRole::Caption)
                        .tone(Tone::Secondary)
                        .into(),
                ],
            )),
        }
    }

    fn row(entry: &Entry) -> Vec<Node<FileMsg>> {
        let icon = if entry.folder {
            Icon::Folder
        } else {
            Icon::File
        };
        let name = row(
            space::SM,
            vec![
                Node::Icon {
                    icon,
                    role: TextRole::Body,
                    tone: Tone::Secondary,
                },
                Node::Expand {
                    weight: 1,
                    child: Box::new(Text::new(entry.name.clone()).into()),
                },
            ],
        );
        let size = match entry.folder {
            true => String::new(),
            false => Bytes(entry.size).to_string(),
        };
        vec![
            name,
            Text::new(size)
                .tone(Tone::Secondary)
                .align(TextAlignment::End)
                .into(),
        ]
    }

    fn content(&self) -> Node<FileMsg> {
        let entries = match &self.entries {
            Err(kind) => return self.unreadable(*kind),
            Ok(entries) if entries.is_empty() => {
                return EmptyState::new(Icon::FolderOpen, "This folder is empty")
                    .body("Files and folders you put in it will show up here.")
                    .into();
            }
            Ok(entries) => entries,
        };
        let mut shows = DefaultHasher::new();
        self.path.hash(&mut shows);
        Node::Table {
            columns: columns(),
            rows: entries.iter().map(Self::row).collect(),
            selected: self.selected,
            on_select: Some(FileMsg::Select),
            on_activate: Some(FileMsg::Open),
            on_header_click: None,
            on_context_menu: None,
            shows: shows.finish(),
        }
    }

    fn unreadable(&self, kind: ErrorKind) -> Node<FileMsg> {
        let name = self.folder_name();
        let (title, body) = match kind {
            kind if Refusal::is(kind) => (
                "You can't open this folder",
                refused::to_open(&name, Refusal::reading(&self.path)),
            ),
            ErrorKind::NotFound => (
                "This folder isn't there",
                match self.back.is_empty() {
                    false => format!(
                        "{name} was moved or deleted. Go back, or choose a place on the left."
                    ),
                    true => format!("{name} was moved or deleted. Choose a place on the left."),
                },
            ),
            _ => (
                "This folder couldn't be read",
                format!("Something went wrong reading {name}. Try again."),
            ),
        };
        let elsewhere = Refusal::is(kind) || kind == ErrorKind::NotFound;
        let action = match (elsewhere, self.back.is_empty()) {
            (true, false) => Button::new("Go back").on_press(FileMsg::Back),
            (true, true) => Button::new("Open Computer").on_press(FileMsg::Root),
            (false, _) => Button::new("Try again").on_press(FileMsg::Reload),
        };
        EmptyState::new(Icon::CircleAlert, title)
            .body(body)
            .action(action)
            .into()
    }

    fn status(&self) -> String {
        let Ok(entries) = &self.entries else {
            return String::new();
        };
        if let Some(entry) = self.selected.and_then(|i| entries.get(i)) {
            return match entry.folder {
                true => format!("“{}” selected", entry.name),
                false => format!("“{}” selected, {}", entry.name, Bytes(entry.size)),
            };
        }
        let folders = entries.iter().filter(|e| e.folder).count();
        let files = entries.len() - folders;
        let count = |n: usize, one: &str, many: &str| match n {
            1 => format!("1 {one}"),
            n => format!("{n} {many}"),
        };
        match (folders, files) {
            (0, _) => count(files, "file", "files"),
            (_, 0) => count(folders, "folder", "folders"),
            _ => format!(
                "{} and {}",
                count(folders, "folder", "folders"),
                count(files, "file", "files")
            ),
        }
    }
}

impl App for FileManagerApp {
    type Message = FileMsg;

    fn view(&self) -> Node<FileMsg> {
        let main = column(
            0,
            vec![
                self.toolbar(),
                self.location(),
                Node::Expand {
                    weight: 1,
                    child: Box::new(self.content()),
                },
                StatusBar::new(self.status()).into(),
            ],
        );
        Node::HStack {
            children: vec![
                self.sidebar(),
                Node::Divider,
                Node::Expand {
                    weight: 1,
                    child: Box::new(main),
                },
            ],
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
        }
    }

    fn update(&mut self, msg: FileMsg) -> Action {
        match msg {
            FileMsg::Back => self.go_back(),
            FileMsg::Forward => self.go_forward(),
            FileMsg::Up => self.go_up(),
            FileMsg::Reload => self.reload(),
            FileMsg::Root => self.go(PathBuf::from("/")),
            FileMsg::Select(i) => self.selected = Some(i),
            FileMsg::Open(i) => {
                self.selected = Some(i);
                self.open_selected();
            }
            FileMsg::Place(i) => {
                if let Some(place) = PLACES.get(i) {
                    self.go(place.path());
                }
            }
        }
        Action::Rebuild
    }

    fn on_key(&mut self, key: Key, modifiers: Modifiers) -> Action {
        match (key, modifiers.alt) {
            (Key::Named(NamedKey::Backspace), _) | (Key::Named(NamedKey::Up), true) => self.go_up(),
            (Key::Named(NamedKey::Left), true) => self.go_back(),
            (Key::Named(NamedKey::Right), true) => self.go_forward(),
            _ => return Action::None,
        }
        Action::Rebuild
    }

    fn title(&self) -> String {
        document_title(&self.folder_name(), NAME)
    }

    fn app_id(&self) -> &str {
        "org.slopos.files"
    }

    fn window(&self) -> WindowSpec {
        let least = least_width();
        let spec = WindowSpec::new(least.max(720), 480);
        spec.min_size(least, spec.min_size.height)
    }
}

pub fn file_manager_main() -> ! {
    slopos_appkit::run_app(FileManagerApp::new())
}
