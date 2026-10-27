//! Files: the folders of the computer, a place at a time.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use slopos_appkit::theme::space;
use slopos_appkit::{
    Action, App, Button, CrossAxisAlignment, EdgeInsets, EmptyState, Icon, IconButton, Key, List,
    ListRow, Modifiers, NamedKey, Node, Sidebar, Surface, TableColumn, TableColumnWidth, Text,
    TextAlignment, TextRole, Tone, Toolbar, WindowSpec, document_title,
};

/// The app's name, as its dock entry and its window give it.
pub const NAME: &str = "Files";

/// A table row tall enough for an icon and the least a control may be.
const ROW_HEIGHT: i32 = 7 * space::XS;
/// The size column, as wide as `1023.9 KiB`.
const SIZE_WIDTH: i32 = 12 * space::SM;

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
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(slopos_abi::fs::SESSION_HOME))
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

    /// The folder's name and where it is.
    fn location(&self) -> Node<FileMsg> {
        Node::Padding {
            padding: EdgeInsets::symmetric(space::LG, space::SM),
            child: Box::new(Node::VStack {
                children: vec![
                    Text::new(self.folder_name()).role(TextRole::Heading).into(),
                    Text::new(self.path.display().to_string())
                        .role(TextRole::Caption)
                        .tone(Tone::Secondary)
                        .into(),
                ],
                spacing: 0,
                align: CrossAxisAlignment::Stretch,
            }),
        }
    }

    fn row(entry: &Entry) -> Vec<Node<FileMsg>> {
        let icon = if entry.folder {
            Icon::Folder
        } else {
            Icon::File
        };
        let name = Node::HStack {
            children: vec![
                Node::Icon {
                    icon,
                    role: TextRole::Body,
                    tone: Tone::Secondary,
                },
                Text::new(entry.name.clone()).into(),
            ],
            spacing: space::SM,
            align: CrossAxisAlignment::Center,
        };
        let size = match entry.folder {
            true => String::new(),
            false => file_size(entry.size),
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
            columns: vec![
                TableColumn {
                    label: "Name".into(),
                    width: TableColumnWidth::Flex(1),
                    sort_indicator: None,
                },
                TableColumn {
                    label: "Size".into(),
                    width: TableColumnWidth::Fixed(SIZE_WIDTH),
                    sort_indicator: None,
                },
            ],
            rows: entries.iter().map(Self::row).collect(),
            row_height: ROW_HEIGHT,
            selected: self.selected,
            on_select: Some(FileMsg::Select),
            on_activate: Some(FileMsg::Open),
            on_header_click: None,
            on_context_menu: None,
            shows: shows.finish(),
        }
    }

    /// Why the folder could not be listed, and the way out.
    fn unreadable(&self, kind: ErrorKind) -> Node<FileMsg> {
        let name = self.folder_name();
        let (title, body) = match kind {
            ErrorKind::PermissionDenied => (
                "You can't open this folder",
                format!("{name} belongs to someone else. Ask them to share it with you."),
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
        let action = match (kind, self.back.is_empty()) {
            (ErrorKind::PermissionDenied | ErrorKind::NotFound, false) => {
                Button::new("Go back").on_press(FileMsg::Back)
            }
            (ErrorKind::PermissionDenied | ErrorKind::NotFound, true) => {
                Button::new("Open Computer").on_press(FileMsg::Root)
            }
            _ => Button::new("Try again").on_press(FileMsg::Reload),
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
                false => format!("“{}” selected, {}", entry.name, file_size(entry.size)),
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

    fn status_bar(&self) -> Node<FileMsg> {
        Node::Background {
            surface: Surface::Chrome,
            child: Box::new(Node::Padding {
                padding: EdgeInsets::symmetric(space::SM, space::XS),
                child: Box::new(
                    Text::new(self.status())
                        .role(TextRole::Caption)
                        .tone(Tone::Secondary)
                        .into(),
                ),
            }),
        }
    }
}

/// A file's size in binary units, as the file manager lists it.
fn file_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return match bytes {
            1 => "1 byte".into(),
            n => format!("{n} bytes"),
        };
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1023.95 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

impl App for FileManagerApp {
    type Message = FileMsg;

    fn view(&self) -> Node<FileMsg> {
        let main = Node::VStack {
            children: vec![
                self.toolbar(),
                self.location(),
                Node::Expand {
                    weight: 1,
                    child: Box::new(self.content()),
                },
                Node::Divider,
                self.status_bar(),
            ],
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
        };
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
            (Key::Named(NamedKey::Enter), _) => self.open_selected(),
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
        WindowSpec::new(720, 480)
    }
}

pub fn file_manager_main() -> ! {
    slopos_appkit::run_app(FileManagerApp::new())
}
