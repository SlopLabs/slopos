//! Every component in each of its states, a page per family: what an app is
//! built from, and what the layout audit holds to its rules.

use slopos_theme_core::words::Bytes;
use slopos_theme_core::{Icon, TextRole, Tone, space};

use crate::components::{
    Button, ButtonRow, Dialog, Disclosure, EmptyState, IconButton, List, ListRow, Notice, Page,
    PropertyList, SearchField, Section, Sidebar, StatusBar, Tag, Text, Toolbar, WindowSpec, column,
    row,
};
use crate::constraints::{CrossAxisAlignment, Length, Orientation, TextAlignment};
use crate::node::{
    Action, App, BarSegment, MenuItem, MenuItemKind, Node, SegmentFill, SortIndicator, TableColumn,
    TableColumnWidth,
};
use crate::style::StyleSheet;
use crate::widgets::code_view::{CodeInput, CodeLine};
use crate::widgets::drag_handle::DragInput;
use crate::widgets::editor_tabs::{EditorTab, TabInput};
use crate::widgets::line_edit::{LineEditInput, LineInput};
use crate::widgets::menu_bar::MenuBarInput;
use crate::widgets::slider::{SliderGrab, SliderInput};
use crate::widgets::tree_view::{TreeInput, TreeRow};

#[derive(Clone, Debug)]
pub enum Msg {
    /// A row of the sidebar as the filter leaves it.
    Show(usize),
    Step(bool),
    Filter(LineEditInput),
    Press,
    /// Which checkbox.
    Toggle(usize),
    Choose(usize),
    Slide(SliderInput),
    Name(LineEditInput),
    Note(LineEditInput),
    Search(LineEditInput),
    Size(LineEditInput),
    Select(usize),
    Place(usize),
    Person(usize),
    Found(usize),
    File(usize),
    Sort(usize),
    Tree(TreeInput),
    /// Which disclosure.
    Expand(usize),
    Tab(usize),
    MenuBar(MenuBarInput),
    Document(TabInput),
    Code(CodeInput),
    Drag(DragInput),
    OpenDialog,
    CloseDialog,
}

/// The pages, by the name `--page` takes.
pub const PAGES: [&str; 13] = [
    "text",
    "buttons",
    "fields",
    "choices",
    "lists",
    "tables",
    "structure",
    "navigation",
    "code",
    "feedback",
    "empty",
    "icons",
    "dialog",
];

const STRUCTURE: usize = 6;
const FEEDBACK: usize = 9;
const DIALOG: usize = 12;

const TITLES: [&str; 13] = [
    "Text",
    "Buttons",
    "Fields",
    "Choices",
    "Lists",
    "Tables and trees",
    "Structure",
    "Tabs and menus",
    "Code",
    "Feedback",
    "Empty states",
    "Icons",
    "Dialogs",
];

const ABOUT: [&str; 13] = [
    "Text takes a role for its size and a tone for its ink, and wraps or ends in an ellipsis.",
    "A row of buttons ends a page: going back on the left, finishing on the right.",
    "Fields share one frame, which thickens and takes the focus colour while typed into.",
    "Choose one of several, turn one thing on, or pick an amount.",
    "Rows that may be chosen, one tab stop for the whole list.",
    "Rows in columns, and a tree of folders, each one tab stop the arrows move along.",
    "A page is made of sections, property lists and details that open, and ends in a row of buttons.",
    "Tabs that switch what a view shows, a menu bar and its menu, and the documents an app has open.",
    "Lines of code beside a pane whose edge can be dragged.",
    "Notices, tags and progress tell the person what happened and what is happening.",
    "What a view says when it has nothing to show.",
    "Every glyph of the icon font that has a name here.",
    "A question over the page, answered by the row of buttons at its foot.",
];

const FILES: [(&str, u64, &str); 5] = [
    ("installer.log", 12_288, "Text"),
    (
        "Screenshot from the installer's review page.png",
        1_468_006,
        "Image",
    ),
    ("ext4-persist.img", 512 << 20, "Disk image"),
    ("notes.txt", 2_048, "Text"),
    ("slop.iso", 740 << 20, "Disc image"),
];

const MENUS: [(&str, &[(&str, Option<&str>)]); 3] = [
    (
        "File",
        &[
            ("New", Some("Ctrl+N")),
            ("Open…", Some("Ctrl+O")),
            ("Save", Some("Ctrl+S")),
        ],
    ),
    (
        "Edit",
        &[
            ("Undo", Some("Ctrl+Z")),
            ("Redo", None),
            ("Find…", Some("Ctrl+F")),
        ],
    ),
    (
        "View",
        &[("Show the sidebar", None), ("Zoom in", Some("Ctrl++"))],
    ),
];

const DOCUMENTS: [(&str, bool); 3] = [
    ("main.rs", false),
    ("lib.rs", true),
    ("a-module-with-a-name-longer-than-its-tab.rs", false),
];

const CODE: [&str; 6] = [
    "fn main() {",
    "\tlet name = \"SlopOS\";",
    "\tprintln!(\"Hello from {name}\");",
    "}",
    "",
    "// Found: name",
];

/// The catalogue as an app: its pages down a sidebar.
pub struct Catalogue {
    page: usize,
    presses: u32,
    checked: [bool; 2],
    choice: Option<usize>,
    amount: u64,
    grab: Option<SliderGrab>,
    /// Each line and whether it has focus.
    name: (LineInput, bool),
    note: (LineInput, bool),
    search: (LineInput, bool),
    size: (LineInput, bool),
    filter: (LineInput, bool),
    row: Option<usize>,
    place: Option<usize>,
    person: Option<usize>,
    found: Option<usize>,
    file: Option<usize>,
    descending: bool,
    folder_open: bool,
    tree_row: Option<usize>,
    tree_first: usize,
    tree_dragging: bool,
    expanded: [bool; 2],
    tab: usize,
    menu: Option<usize>,
    document: usize,
    caret: (usize, usize),
    pane: i32,
    dragging: bool,
    /// Where the pointer and the pane's edge were at a drag's first move.
    drag_from: Option<(i32, i32)>,
    dialog: bool,
}

impl Catalogue {
    pub fn new(page: usize) -> Self {
        Self {
            page: page.min(PAGES.len() - 1),
            presses: 0,
            checked: [true, false],
            choice: Some(0),
            amount: 40,
            grab: None,
            name: (LineInput::with_text(String::from("Ada Lovelace")), false),
            note: (LineInput::new(), false),
            search: (LineInput::with_text(String::from("disk")), false),
            size: (LineInput::with_text(String::from("12 TB")), false),
            filter: (LineInput::new(), false),
            row: Some(0),
            place: Some(1),
            person: Some(0),
            found: Some(0),
            file: Some(1),
            descending: false,
            folder_open: true,
            tree_row: Some(1),
            tree_first: 0,
            tree_dragging: false,
            expanded: [true, false],
            tab: 0,
            menu: Some(0),
            document: 0,
            caret: (2, 1),
            pane: PANE_WIDTH,
            dragging: false,
            drag_from: None,
            dialog: true,
        }
    }

    pub(crate) fn shown_page(&self) -> usize {
        self.page
    }

    /// Page `index` as a node, inside a [`Page`].
    pub fn page(&self, index: usize) -> Node<Msg> {
        let content = match index {
            0 => self.text(),
            1 => self.buttons(),
            2 => self.fields(),
            3 => self.choices(),
            4 => self.lists(),
            5 => self.tables(),
            6 => self.structure(),
            7 => self.navigation(),
            8 => self.code(),
            9 => self.feedback(),
            10 => self.empty(),
            11 => self.icons(),
            _ => self.dialogs(),
        };
        let mut page = Page::new(TITLES[index])
            .subtitle(ABOUT[index])
            .content(content);
        if index == STRUCTURE {
            page = page.footer(
                ButtonRow::new()
                    .back(Button::new("Back").on_press(Msg::Press))
                    .confirm(Button::new("Continue").on_press(Msg::Press)),
            );
        }
        page.into()
    }

    fn sections(sections: Vec<Node<Msg>>) -> Node<Msg> {
        column(space::XL, sections)
    }

    /// `child` at `height`, which a view that takes all it is given needs.
    fn tall(height: i32, child: Node<Msg>) -> Node<Msg> {
        Node::SizedBox {
            width: None,
            height: Some(Length::Px(height)),
            child: Box::new(child),
        }
    }

    fn text(&self) -> Node<Msg> {
        let roles = [
            (TextRole::Hero, "14:05"),
            (TextRole::Display, "Display, for a page that is one number"),
            (TextRole::Title, "Title, for a page or a dialog"),
            (TextRole::Heading, "Heading, for a section"),
            (TextRole::Body, "Body, for what a person reads"),
            (TextRole::Label, "Label, for a control's name"),
            (TextRole::Caption, "Caption, for a device name or a size"),
            (TextRole::Mono, "Mono, for code: /bin/installer --serve"),
        ];
        let tones = [
            (Tone::Normal, "Normal"),
            (Tone::Secondary, "Secondary"),
            (Tone::Disabled, "Disabled"),
            (Tone::Accent, "Accent"),
            (Tone::Danger, "Danger"),
            (Tone::Warning, "Warning"),
            (Tone::Success, "Success"),
            (Tone::Info, "Info"),
        ];
        Self::sections(vec![
            Section::new(
                "Roles",
                column(
                    space::SM,
                    roles
                        .iter()
                        .map(|&(role, words)| Text::new(words).role(role).into())
                        .collect(),
                ),
            )
            .into(),
            Section::new(
                "Tones",
                column(
                    space::SM,
                    tones
                        .iter()
                        .map(|&(tone, word)| Text::new(word).tone(tone).into())
                        .collect(),
                ),
            )
            .into(),
            Section::new(
                "Long text",
                column(
                    space::SM,
                    vec![
                        Text::new(
                            "Prose wraps at spaces, so a sentence that is longer than its \
                             box goes on to the next line rather than off its edge.",
                        )
                        .wrap()
                        .into(),
                        Text::new(
                            "/usr/local/lib/rustlib/x86_64-unknown-slopos/lib/libstd-a1b2c3d4e5f6a7b8.so --serve --root /media/payload",
                        )
                        .role(TextRole::Mono)
                        .wrap_anywhere()
                        .into(),
                        Text::new(
                            "One line that does not fit its box ends in an ellipsis, and the whole of it shows in a tooltip.",
                        )
                        .into(),
                        Text::new(
                            "At most two lines: a paragraph cut short keeps its first two lines and \
                             ends the second in an ellipsis, however much more there was to say.",
                        )
                        .wrap()
                        .max_lines(2)
                        .into(),
                        Text::new("Centred").align(TextAlignment::Center).into(),
                    ],
                ),
            )
            .into(),
        ])
    }

    fn buttons(&self) -> Node<Msg> {
        let styles = |enabled: bool| {
            row(
                space::SM,
                vec![
                    Button::new("Install")
                        .primary()
                        .on_press(Msg::Press)
                        .enabled(enabled)
                        .into(),
                    Button::new("Cancel")
                        .on_press(Msg::Press)
                        .enabled(enabled)
                        .into(),
                    Button::new("Erase")
                        .destructive()
                        .on_press(Msg::Press)
                        .enabled(enabled)
                        .into(),
                ],
            )
        };
        let tools = row(
            space::SM,
            vec![
                IconButton::new(Icon::FolderPlus, "New folder")
                    .on_press(Msg::Press)
                    .into(),
                IconButton::new(Icon::LayoutGrid, "Show as grid")
                    .on_press(Msg::Press)
                    .active(true)
                    .into(),
                IconButton::new(Icon::Trash, "Move to trash")
                    .on_press(Msg::Press)
                    .enabled(false)
                    .into(),
            ],
        );
        Self::sections(vec![
            Section::new(
                "Styles",
                column(
                    space::SM,
                    vec![
                        styles(true),
                        styles(false),
                        Text::new(format!("Pressed {} times", self.presses))
                            .tone(Tone::Secondary)
                            .into(),
                    ],
                ),
            )
            .into(),
            Section::new(
                "A row that ends a page",
                column(
                    space::SM,
                    vec![
                        ButtonRow::new()
                            .back(Button::new("Back").on_press(Msg::Press))
                            .confirm(Button::new("Continue").on_press(Msg::Press))
                            .into(),
                        ButtonRow::new()
                            .cancel(Button::new("Cancel").on_press(Msg::Press))
                            .destructive(Button::new("Erase disk").on_press(Msg::Press))
                            .into(),
                        ButtonRow::new()
                            .aside(Button::new("Show log").on_press(Msg::Press))
                            .cancel(Button::new("Cancel").on_press(Msg::Press))
                            .confirm(Button::new("Install").on_press(Msg::Press).enabled(false))
                            .into(),
                    ],
                ),
            )
            .into(),
            Section::new("Icon buttons", tools).into(),
        ])
    }

    fn fields(&self) -> Node<Msg> {
        Self::sections(vec![
            Section::new(
                "A line of text",
                column(
                    space::SM,
                    vec![
                        Self::line(&self.name, "Name", Msg::Name),
                        Self::line(&self.note, "Placeholder", Msg::Note),
                    ],
                ),
            )
            .into(),
            Section::new(
                "Search",
                column(
                    space::SM,
                    vec![
                        SearchField::new(self.search.0.text.clone(), self.search.0.caret)
                            .focused(self.search.1)
                            .placeholder("Disk name")
                            .count("3 found")
                            .on_input(Msg::Search)
                            .into(),
                        SearchField::new("zzz", 3)
                            .count("No results")
                            .invalid(true)
                            .tab_stop(false)
                            .into(),
                    ],
                ),
            )
            .into(),
            Section::new(
                "Not usable",
                Node::LineEdit {
                    text: self.size.0.text.clone(),
                    placeholder: String::new(),
                    caret: self.size.0.caret,
                    focused: self.size.1,
                    icon: None,
                    suffix: String::from("Too large"),
                    invalid: true,
                    tab_stop: true,
                    on_input: Some(Msg::Size),
                },
            )
            .into(),
        ])
    }

    fn line(
        (line, focused): &(LineInput, bool),
        placeholder: &str,
        on_input: fn(LineEditInput) -> Msg,
    ) -> Node<Msg> {
        Node::LineEdit {
            text: line.text.clone(),
            placeholder: String::from(placeholder),
            caret: line.caret,
            focused: *focused,
            icon: None,
            suffix: String::new(),
            invalid: false,
            tab_stop: true,
            on_input: Some(on_input),
        }
    }

    fn choices(&self) -> Node<Msg> {
        let options = vec![
            ListRow::new("Erase disk and install"),
            ListRow::new("Install beside what is there").disabled("This disk has no free space."),
            ListRow::new("Reinstall SlopOS").detail("Keeps your files."),
        ];
        let checkbox = |checked: bool, label: &str, on_toggle: Option<Msg>| Node::Checkbox {
            checked,
            label: String::from(label),
            enabled: on_toggle.is_some(),
            on_toggle,
        };
        Self::sections(vec![
            Section::new(
                "One of several",
                List::new(options)
                    .selected(self.choice)
                    .on_select(Msg::Choose),
            )
            .into(),
            Section::new(
                "On or off",
                column(
                    space::SM,
                    vec![
                        checkbox(self.checked[0], "Start automatically", Some(Msg::Toggle(0))),
                        checkbox(self.checked[1], "Show hidden files", Some(Msg::Toggle(1))),
                        checkbox(true, "Set by your administrator", None),
                    ],
                ),
            )
            .into(),
            Section::new(
                "An amount",
                column(
                    space::SM,
                    vec![
                        Text::new(format!("{} GB", self.amount)).into(),
                        Node::Slider {
                            value: self.amount,
                            min: 8,
                            max: 120,
                            step: 1,
                            enabled: true,
                            grab: self.grab,
                            on_input: Some(Msg::Slide),
                        },
                        Node::Slider {
                            value: 64,
                            min: 8,
                            max: 120,
                            step: 1,
                            enabled: false,
                            grab: None,
                            on_input: None,
                        },
                    ],
                ),
            )
            .into(),
        ])
    }

    fn lists(&self) -> Node<Msg> {
        let disks = vec![
            ListRow::new("KINGSTON SNV3S1000G")
                .icon(Icon::HardDrive)
                .detail("nvme0n1 · Windows and a data partition")
                .trailing("1 TB"),
            ListRow::new("SanDisk Ultra")
                .icon(Icon::Usb)
                .detail("sda")
                .trailing("32 GB"),
            ListRow::new("QEMU NVMe Ctrl")
                .icon(Icon::HardDrive)
                .trailing("8 MB")
                .disabled("Too small. SlopOS needs at least 2.5 GB."),
        ];
        let places = vec![
            (
                String::from("Places"),
                vec![
                    ListRow::new("Home").icon(Icon::Home),
                    ListRow::new("Documents").icon(Icon::Folder),
                ],
            ),
            (
                String::from("Devices"),
                vec![ListRow::new("Root").icon(Icon::HardDrive)],
            ),
        ];
        let people = ["ada", "grace", "Édouard"]
            .map(|name| ListRow::new(name).avatar())
            .to_vec();
        let found = [
            ("view.rs", "apps/src/editor"),
            ("disks.rs", "userland/src/apps/installer/engine/probe"),
        ]
        .map(|(name, folder)| ListRow::new(name).detail_tail(folder))
        .to_vec();
        Self::sections(vec![
            Section::new(
                "Rows with details",
                List::new(disks).selected(self.row).on_select(Msg::Select),
            )
            .into(),
            Section::new(
                "In sections",
                List::sectioned(places)
                    .selected(self.place)
                    .on_select(Msg::Place),
            )
            .into(),
            Section::new(
                "Cut at the start",
                List::new(found).selected(self.found).on_select(Msg::Found),
            )
            .into(),
            Section::new(
                "People",
                column(
                    space::SM,
                    vec![
                        Node::Avatar {
                            name: String::from("Ada"),
                        },
                        List::new(people)
                            .selected(self.person)
                            .on_select(Msg::Person)
                            .into(),
                    ],
                ),
            )
            .into(),
        ])
    }

    fn tables(&self) -> Node<Msg> {
        let sort = match self.descending {
            true => SortIndicator::Descending,
            false => SortIndicator::Ascending,
        };
        let heading = |label: &str, width, sorted: bool| TableColumn {
            label: String::from(label),
            width,
            sort_indicator: sorted.then_some(sort),
        };
        let cell = |text: &str| -> Node<Msg> { Text::new(text).into() };
        let mut files = FILES.to_vec();
        if self.descending {
            files.reverse();
        }
        let table = Node::Table {
            columns: vec![
                heading("Name", TableColumnWidth::Flex(2), true),
                heading("Size", TableColumnWidth::Fixed(SIZE_COLUMN), false),
                heading("Kind", TableColumnWidth::Flex(1), false),
            ],
            rows: files
                .iter()
                .map(|&(name, size, kind)| {
                    vec![cell(name), cell(&Bytes(size).to_string()), cell(kind)]
                })
                .collect(),
            selected: self.file,
            on_select: Some(Msg::File),
            on_activate: None,
            on_header_click: Some(Msg::Sort),
            on_context_menu: None,
            shows: 0,
        };
        let rows = self.tree_rows();
        let total_rows = rows.len();
        let tree = Node::TreeView {
            rows: rows
                .into_iter()
                .skip(self.tree_first)
                .take(TREE_ROWS as usize)
                .collect(),
            total_rows,
            first_row: self.tree_first,
            selected: self.tree_row,
            focused: false,
            scroll_dragging: self.tree_dragging,
            on_input: Some(Msg::Tree),
        };
        let row = StyleSheet::current().row_height;
        Self::sections(vec![
            Section::new("A table", Self::tall(row * (TABLE_ROWS + 1), table)).into(),
            Section::new("A tree", Self::tall(row * TREE_ROWS, tree)).into(),
        ])
    }

    fn tree_rows(&self) -> Vec<TreeRow> {
        let entry = |label: &str, depth, is_dir| TreeRow {
            label: String::from(label),
            depth,
            is_dir,
            expanded: false,
            active: false,
            modified: false,
        };
        let mut rows = vec![TreeRow {
            expanded: self.folder_open,
            ..entry("src", 0, true)
        }];
        if self.folder_open {
            rows.push(TreeRow {
                active: true,
                ..entry("main.rs", 1, false)
            });
            rows.push(TreeRow {
                modified: true,
                ..entry("lib.rs", 1, false)
            });
            rows.push(entry(
                "a-module-with-a-name-longer-than-the-tree.rs",
                1,
                false,
            ));
        }
        rows.push(entry("Cargo.toml", 0, false));
        rows
    }

    fn structure(&self) -> Node<Msg> {
        let properties = PropertyList::new()
            .row("Disk", "KINGSTON SNV3S1000G, 1 TB")
            .row(
                "Startup order",
                "Starts after the systems already on this computer",
            )
            .row_with("Root partition", Tag::new("New", Tone::Success))
            .row("People", "user, the first administrator");
        let log = Text::new(
            "mke2fs 1.47.1\nCreating filesystem with 7864320 4k blocks\nWriting superblocks: done",
        )
        .role(TextRole::Mono)
        .wrap_anywhere();
        Self::sections(vec![
            Section::new("A property list", properties).into(),
            Section::new(
                "Details",
                column(
                    space::SM,
                    vec![
                        Disclosure::new("Show details", log)
                            .expanded(self.expanded[0])
                            .on_toggle(Msg::Expand(0))
                            .into(),
                        Disclosure::new("Advanced", Text::new("Nothing more to set."))
                            .expanded(self.expanded[1])
                            .on_toggle(Msg::Expand(1))
                            .into(),
                    ],
                ),
            )
            .into(),
        ])
    }

    fn navigation(&self) -> Node<Msg> {
        let panel = |words: &str| -> Node<Msg> { Text::new(words).wrap().into() };
        let tabs = Node::TabBar {
            tabs: ["General", "Advanced", "About"].map(String::from).to_vec(),
            active: self.tab,
            on_change: Some(Msg::Tab),
            content: vec![
                panel("What most people change."),
                panel("What few people need."),
                panel("SlopOS, from the source in /src/slopos."),
            ],
        };
        let shown = self.menu.unwrap_or(0);
        let items = MENUS[shown]
            .1
            .iter()
            .map(|&(label, shortcut)| MenuItem {
                label,
                shortcut,
                enabled: label != "Redo",
                kind: MenuItemKind::Action,
            })
            .collect();
        let menus = column(
            space::SM,
            vec![
                Node::MenuBar {
                    titles: MENUS
                        .iter()
                        .map(|(title, _)| String::from(*title))
                        .collect(),
                    open: self.menu,
                    on_input: Some(Msg::MenuBar),
                },
                row(
                    0,
                    vec![Node::Menu {
                        items,
                        on_action: Some(|_| Msg::Press),
                    }],
                ),
            ],
        );
        let documents = Node::EditorTabs {
            tabs: DOCUMENTS
                .iter()
                .map(|&(title, modified)| EditorTab {
                    title: String::from(title),
                    modified,
                })
                .collect(),
            active: self.document,
            on_input: Some(Msg::Document),
        };
        Self::sections(vec![
            Section::new("Tabs", tabs).into(),
            Section::new("A menu bar and its menu", menus).into(),
            Section::new("Open documents", documents).into(),
        ])
    }

    fn code(&self) -> Node<Msg> {
        let hit = CODE[5].find("name").map(|at| (at, at + "name".len()));
        let lines = CODE
            .iter()
            .enumerate()
            .map(|(number, text)| CodeLine {
                number,
                text: String::from(*text),
                spans: Vec::new(),
                highlights: hit.filter(|_| number == 5).into_iter().collect(),
            })
            .collect();
        let code = Node::CodeView {
            lines,
            first_line: 0,
            total_lines: CODE.len(),
            first_col: 0,
            tab_width: 4,
            cursor: Some(self.caret),
            selection: Some(((1, 5), (1, 9))),
            show_line_numbers: true,
            focused: false,
            selecting: false,
            scroll_dragging: false,
            on_input: Some(Msg::Code),
        };
        let pane = Node::SizedBox {
            width: Some(Length::Px(self.pane)),
            height: None,
            child: Box::new(Sidebar::empty("A pane as wide as its edge is dragged.").into()),
        };
        let split = Node::HStack {
            children: vec![
                pane,
                Node::DragHandle {
                    orientation: Orientation::Vertical,
                    active: self.dragging,
                    on_drag: Some(Msg::Drag),
                },
                Node::Expand {
                    weight: 1,
                    child: Box::new(code),
                },
            ],
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
        };
        let height = crate::text::cell_height() * (CODE.len() as i32 + 1);
        Section::new("A pane and the code beside it", Self::tall(height, split)).into()
    }

    fn feedback(&self) -> Node<Msg> {
        let notice = |tone, title: &str, body: &str| -> Node<Msg> {
            Notice::new(tone, title).body(body).into()
        };
        let segment = |label: &str, size, fill| BarSegment {
            label: String::from(label),
            size,
            fill,
        };
        Self::sections(vec![
            Section::new(
                "Notices",
                column(
                    space::SM,
                    vec![
                        notice(
                            Tone::Info,
                            "Developer tools aren't on this installer",
                            "They won't be installed.",
                        ),
                        notice(
                            Tone::Success,
                            "SlopOS is installed",
                            "Remove the USB stick, then restart.",
                        ),
                        notice(
                            Tone::Warning,
                            "The battery is low",
                            "Plug in the computer soon.",
                        ),
                        Notice::new(Tone::Danger, "SlopOS couldn't be installed")
                            .body("The installer stopped while copying the system.")
                            .action(Button::new("Try again").on_press(Msg::Press))
                            .into(),
                    ],
                ),
            )
            .into(),
            Section::new(
                "Tags",
                row(
                    space::SM,
                    vec![
                        Tag::new("Erased", Tone::Danger).into(),
                        Tag::new("New", Tone::Success).into(),
                        Tag::new("Kept", Tone::Secondary).into(),
                    ],
                ),
            )
            .into(),
            Section::new(
                "Status",
                column(
                    space::SM,
                    vec![
                        StatusBar::new("12 files and 3 folders")
                            .end("Ln 4, Col 18")
                            .into(),
                        StatusBar::new("Couldn't save notes.txt. Try again.")
                            .tone(Tone::Danger)
                            .into(),
                    ],
                ),
            )
            .into(),
            Section::new(
                "Progress",
                column(
                    space::SM,
                    vec![
                        Node::ProgressBar {
                            value: Some(62),
                            label: String::from("62%"),
                            tone: Tone::Accent,
                        },
                        Node::ProgressBar {
                            value: None,
                            label: String::new(),
                            tone: Tone::Accent,
                        },
                        Node::PartitionBar {
                            segments: vec![
                                segment("EFI", 1, SegmentFill::Series(1)),
                                segment("SlopOS", 60, SegmentFill::Series(0)),
                                segment("free", 30, SegmentFill::Hatched),
                            ],
                            highlight: Some(1),
                        },
                    ],
                ),
            )
            .into(),
        ])
    }

    fn empty(&self) -> Node<Msg> {
        Self::tall(
            EMPTY_HEIGHT,
            EmptyState::new(Icon::FolderOpen, "This folder is empty")
                .body("Files you add here will show up in this list.")
                .action(Button::new("New folder").on_press(Msg::Press))
                .into(),
        )
    }

    fn icons(&self) -> Node<Msg> {
        List::new(
            Icon::ALL
                .iter()
                .map(|&icon| ListRow::new(icon.name()).icon(icon))
                .collect(),
        )
        .into()
    }

    fn dialogs(&self) -> Node<Msg> {
        column(
            space::SM,
            vec![
                Text::new("A dialog covers the page until it is answered.")
                    .wrap()
                    .into(),
                row(
                    space::SM,
                    vec![
                        Button::new("Show the dialog")
                            .on_press(Msg::OpenDialog)
                            .into(),
                    ],
                ),
            ],
        )
    }

    fn dialog_node(&self) -> Node<Msg> {
        Dialog::new("Erase this disk?")
            .content(Text::new("Everything on KINGSTON SNV3S1000G will be deleted.").wrap())
            .footer(
                ButtonRow::new()
                    .cancel(Button::new("Cancel").on_press(Msg::CloseDialog))
                    .destructive(Button::new("Erase").on_press(Msg::CloseDialog)),
            )
            .on_dismiss(Msg::CloseDialog)
            .into()
    }

    fn toolbar(&self) -> Node<Msg> {
        let shown = self.shown();
        let count = match self.filter.0.text.is_empty() {
            true => String::new(),
            false => format!("{} found", shown.len()),
        };
        Toolbar::new()
            .start(
                IconButton::new(Icon::ChevronLeft, "Previous page")
                    .enabled(self.page > 0)
                    .on_press(Msg::Step(false)),
            )
            .start(
                IconButton::new(Icon::ChevronRight, "Next page")
                    .enabled(self.page + 1 < PAGES.len())
                    .on_press(Msg::Step(true)),
            )
            .end(Node::SizedBox {
                width: Some(Length::Px(SEARCH_WIDTH)),
                height: None,
                child: Box::new(
                    SearchField::new(self.filter.0.text.clone(), self.filter.0.caret)
                        .focused(self.filter.1)
                        .placeholder("Find a page")
                        .count(count)
                        .on_input(Msg::Filter)
                        .into(),
                ),
            })
            .into()
    }

    /// The pages whose title holds what the filter says.
    fn shown(&self) -> Vec<usize> {
        let filter = self.filter.0.text.to_lowercase();
        (0..PAGES.len())
            .filter(|&i| TITLES[i].to_lowercase().contains(&filter))
            .collect()
    }

    fn sidebar(&self) -> Node<Msg> {
        let shown = self.shown();
        if shown.is_empty() {
            return Sidebar::empty("No page matches.").into();
        }
        let rows = shown.iter().map(|&i| ListRow::new(TITLES[i])).collect();
        let selected = shown.iter().position(|&i| i == self.page);
        Sidebar::new(List::new(rows).selected(selected).on_select(Msg::Show)).into()
    }

    fn show(&mut self, page: usize) {
        self.page = page.min(PAGES.len() - 1);
        self.dialog = self.page == DIALOG;
    }

    fn tree(&mut self, input: TreeInput) {
        match input {
            TreeInput::Toggle { row: 0 } => self.folder_open = !self.folder_open,
            TreeInput::Activate { row } => self.tree_row = Some(row),
            TreeInput::Scroll { delta_rows } => {
                self.tree_first = self.tree_first.saturating_add_signed(delta_rows as isize);
            }
            TreeInput::ScrollTo { first_row } => {
                self.tree_first = first_row;
                self.tree_dragging = true;
            }
            TreeInput::Release => self.tree_dragging = false,
            _ => {}
        }
        let last = self.tree_rows().len().saturating_sub(TREE_ROWS as usize);
        self.tree_first = self.tree_first.min(last);
    }

    fn menu_bar(&mut self, input: MenuBarInput) {
        self.menu = match input {
            MenuBarInput::Open { index, .. } | MenuBarInput::Hover { index, .. } => Some(index),
            MenuBarInput::Close => None,
        };
    }
}

/// Applies `input` to a line and whether it has focus.
fn edit((line, focused): &mut (LineInput, bool), input: LineEditInput) {
    match input {
        LineEditInput::Text { character } => line.insert(character),
        LineEditInput::Key { key, modifiers } => {
            line.handle_key(key, modifiers);
        }
        LineEditInput::Caret { col } => line.set_caret(col),
        LineEditInput::Focus => *focused = true,
        LineEditInput::Blur => *focused = false,
    }
}

const EMPTY_HEIGHT: i32 = 360;
const SEARCH_WIDTH: i32 = 220;
const SIZE_COLUMN: i32 = 96;
const PANE_WIDTH: i32 = 160;

// Fewer rows than there are, so the table and the tree scroll.
const TABLE_ROWS: i32 = 3;
const TREE_ROWS: i32 = 3;

impl App for Catalogue {
    type Message = Msg;

    fn view(&self) -> Node<Msg> {
        let body = Node::HStack {
            children: vec![
                self.sidebar(),
                Node::Expand {
                    weight: 1,
                    child: Box::new(self.page(self.page)),
                },
            ],
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
        };
        let window = column(
            0,
            vec![
                self.toolbar(),
                Node::Expand {
                    weight: 1,
                    child: Box::new(body),
                },
            ],
        );
        if self.page == DIALOG && self.dialog {
            return Node::ZStack {
                children: vec![window, self.dialog_node()],
            };
        }
        window
    }

    fn update(&mut self, msg: Msg) -> Action {
        match msg {
            Msg::Show(row) => {
                if let Some(&page) = self.shown().get(row) {
                    self.show(page);
                }
            }
            Msg::Step(forward) => match forward {
                true => self.show(self.page + 1),
                false => self.show(self.page.saturating_sub(1)),
            },
            Msg::Filter(input) => edit(&mut self.filter, input),
            Msg::Press => self.presses += 1,
            Msg::Toggle(i) => self.checked[i] = !self.checked[i],
            Msg::Choose(i) => self.choice = Some(i),
            Msg::Slide(SliderInput::Begin(v, grab)) => {
                self.amount = v;
                self.grab = Some(grab);
            }
            Msg::Slide(SliderInput::Change(v)) => self.amount = v,
            Msg::Slide(SliderInput::End) => self.grab = None,
            Msg::Name(input) => edit(&mut self.name, input),
            Msg::Note(input) => edit(&mut self.note, input),
            Msg::Search(input) => edit(&mut self.search, input),
            Msg::Size(input) => edit(&mut self.size, input),
            Msg::Select(row) => self.row = Some(row),
            Msg::Place(row) => self.place = Some(row),
            Msg::Person(row) => self.person = Some(row),
            Msg::Found(row) => self.found = Some(row),
            Msg::File(row) => self.file = Some(row),
            Msg::Sort(_) => {
                self.descending = !self.descending;
                self.file = self.file.map(|row| FILES.len() - 1 - row);
            }
            Msg::Tree(input) => self.tree(input),
            Msg::Expand(i) => self.expanded[i] = !self.expanded[i],
            Msg::Tab(tab) => self.tab = tab,
            Msg::MenuBar(input) => self.menu_bar(input),
            Msg::Document(TabInput::Select(tab)) => self.document = tab,
            Msg::Document(TabInput::Close(_)) => return Action::None,
            Msg::Code(CodeInput::Click { line, col, .. }) => self.caret = (line, col),
            Msg::Code(_) => return Action::None,
            Msg::Drag(DragInput::Begin) => self.dragging = true,
            Msg::Drag(DragInput::Move(x)) => {
                let (x0, pane) = *self.drag_from.get_or_insert((x, self.pane));
                self.pane = (pane + x - x0).clamp(PANE_WIDTH / 2, PANE_WIDTH * 2);
            }
            Msg::Drag(DragInput::End) => {
                self.dragging = false;
                self.drag_from = None;
            }
            Msg::OpenDialog => self.dialog = true,
            Msg::CloseDialog => self.dialog = false,
        }
        Action::Rebuild
    }

    /// The indeterminate bar moves only as often as it is painted.
    fn tick_interval_ms(&self) -> Option<u64> {
        (self.page == FEEDBACK).then_some(TICK_MS)
    }

    fn tick(&mut self) -> Action {
        Action::Repaint
    }

    fn title(&self) -> String {
        "Widget Gallery".into()
    }

    fn app_id(&self) -> &str {
        "org.slopos.widget-gallery"
    }

    fn window(&self) -> WindowSpec {
        WindowSpec::new(WIDTH, HEIGHT).min_size(MIN_WIDTH, MIN_HEIGHT)
    }
}

const TICK_MS: u64 = 50;
const WIDTH: i32 = 960;
const HEIGHT: i32 = 600;
const MIN_WIDTH: i32 = 560;
const MIN_HEIGHT: i32 = 400;
