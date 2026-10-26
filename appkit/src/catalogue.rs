//! Every component in each of its states, a page per family: what an app is
//! built from, and what the layout audit holds to its rules.

use slopos_theme_core::{Icon, TextRole, Tone, space};

use crate::components::{
    Button, ButtonRow, Disclosure, EmptyState, IconButton, List, ListRow, Notice, Page,
    PropertyList, SearchField, Section, Sidebar, Tag, Text, Toolbar, WindowSpec,
};
use crate::constraints::{CrossAxisAlignment, TextAlignment};
use crate::node::{Action, App, BarSegment, Node, RadioOption, SegmentFill};
use crate::widgets::line_edit::{LineEditInput, LineInput};
use crate::widgets::slider::{SliderGrab, SliderInput};

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
    Name(String),
    Note(String),
    Search(LineEditInput),
    Size(LineEditInput),
    Select(usize),
    Place(usize),
    /// Which disclosure.
    Expand(usize),
    OpenDialog,
    CloseDialog,
}

/// The pages, by the name `--page` takes.
pub const PAGES: [&str; 10] = [
    "text",
    "buttons",
    "fields",
    "choices",
    "lists",
    "structure",
    "feedback",
    "empty",
    "icons",
    "dialog",
];

const STRUCTURE: usize = 5;
const DIALOG: usize = 9;

const TITLES: [&str; 10] = [
    "Text",
    "Buttons",
    "Fields",
    "Choices",
    "Lists",
    "Structure",
    "Feedback",
    "Empty states",
    "Icons",
    "Dialogs",
];

const ABOUT: [&str; 10] = [
    "Text takes a role for its size and a tone for its ink, and wraps or ends in an ellipsis.",
    "A row of buttons ends a page: going back on the left, finishing on the right.",
    "Fields share one frame, which thickens and takes the focus colour while typed into.",
    "Choose one of several, turn one thing on, or pick an amount.",
    "Rows that may be chosen, one tab stop for the whole list.",
    "A page is made of sections, property lists and details that open, and ends in a row of buttons.",
    "Notices, tags and progress tell the person what happened and what is happening.",
    "What a view says when it has nothing to show.",
    "Every glyph of the icon font that has a name here.",
    "A question over the page, answered by the row of buttons at its foot.",
];

/// The catalogue as an app: its pages down a sidebar.
pub struct Catalogue {
    page: usize,
    presses: u32,
    checked: [bool; 2],
    choice: Option<usize>,
    amount: u64,
    grab: Option<SliderGrab>,
    name: String,
    note: String,
    /// Each line and whether it has focus.
    search: (LineInput, bool),
    size: (LineInput, bool),
    filter: (LineInput, bool),
    row: Option<usize>,
    place: Option<usize>,
    expanded: [bool; 2],
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
            name: String::from("Ada Lovelace"),
            note: String::new(),
            search: (LineInput::with_text(String::from("disk")), false),
            size: (LineInput::with_text(String::from("12 TB")), false),
            filter: (LineInput::new(), false),
            row: Some(0),
            place: Some(1),
            expanded: [true, false],
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
            5 => self.structure(),
            6 => self.feedback(),
            7 => self.empty(),
            8 => self.icons(),
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
        Node::VStack {
            children: sections,
            spacing: space::XL,
            align: CrossAxisAlignment::Stretch,
        }
    }

    fn column(children: Vec<Node<Msg>>) -> Node<Msg> {
        Node::VStack {
            children,
            spacing: space::SM,
            align: CrossAxisAlignment::Stretch,
        }
    }

    fn row(children: Vec<Node<Msg>>) -> Node<Msg> {
        Node::HStack {
            children,
            spacing: space::SM,
            align: CrossAxisAlignment::Center,
        }
    }

    fn text(&self) -> Node<Msg> {
        let roles = [
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
                Self::column(
                    roles
                        .iter()
                        .map(|&(role, words)| Text::new(words).role(role).into())
                        .collect(),
                ),
            )
            .into(),
            Section::new(
                "Tones",
                Self::column(
                    tones
                        .iter()
                        .map(|&(tone, word)| Text::new(word).tone(tone).into())
                        .collect(),
                ),
            )
            .into(),
            Section::new(
                "Long text",
                Self::column(vec![
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
                ]),
            )
            .into(),
        ])
    }

    fn buttons(&self) -> Node<Msg> {
        let styles = |enabled: bool| {
            Self::row(vec![
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
            ])
        };
        let tools = Self::row(vec![
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
        ]);
        Self::sections(vec![
            Section::new(
                "Styles",
                Self::column(vec![
                    styles(true),
                    styles(false),
                    Text::new(format!("Pressed {} times", self.presses))
                        .tone(Tone::Secondary)
                        .into(),
                ]),
            )
            .into(),
            Section::new(
                "A row that ends a page",
                Self::column(vec![
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
                ]),
            )
            .into(),
            Section::new("Icon buttons", tools).into(),
        ])
    }

    fn fields(&self) -> Node<Msg> {
        Self::sections(vec![
            Section::new(
                "Text field",
                Self::column(vec![
                    Node::TextField {
                        text: self.name.clone(),
                        placeholder: String::from("Name"),
                        on_change: Some(Msg::Name),
                        max_length: None,
                        read_only: false,
                    },
                    Node::TextField {
                        text: self.note.clone(),
                        placeholder: String::from("Placeholder"),
                        on_change: Some(Msg::Note),
                        max_length: None,
                        read_only: false,
                    },
                ]),
            )
            .into(),
            Section::new(
                "Search",
                SearchField::new(self.search.0.text.clone(), self.search.0.caret)
                    .focused(self.search.1)
                    .count("3 found")
                    .on_input(Msg::Search),
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

    fn choices(&self) -> Node<Msg> {
        let option = |label: &str, detail: &str, enabled| RadioOption {
            label: String::from(label),
            detail: String::from(detail),
            enabled,
        };
        Self::sections(vec![
            Section::new(
                "One of several",
                Node::RadioGroup {
                    options: vec![
                        option("Erase disk and install", "", true),
                        option(
                            "Install beside what is there",
                            "This disk has no free space.",
                            false,
                        ),
                        option("Reinstall SlopOS", "Keeps your files.", true),
                    ],
                    selected: self.choice,
                    on_select: Some(Msg::Choose),
                },
            )
            .into(),
            Section::new(
                "On or off",
                Self::column(vec![
                    Node::Checkbox {
                        checked: self.checked[0],
                        label: String::from("Start automatically"),
                        on_toggle: Some(Msg::Toggle(0)),
                        enabled: true,
                    },
                    Node::Checkbox {
                        checked: self.checked[1],
                        label: String::from("Show hidden files"),
                        on_toggle: Some(Msg::Toggle(1)),
                        enabled: true,
                    },
                    Node::Checkbox {
                        checked: true,
                        label: String::from("Set by your administrator"),
                        on_toggle: None,
                        enabled: false,
                    },
                ]),
            )
            .into(),
            Section::new(
                "An amount",
                Self::column(vec![
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
                ]),
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
        ])
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
                Self::column(vec![
                    Disclosure::new("Show details", log)
                        .expanded(self.expanded[0])
                        .on_toggle(Msg::Expand(0))
                        .into(),
                    Disclosure::new("Advanced", Text::new("Nothing more to set."))
                        .expanded(self.expanded[1])
                        .on_toggle(Msg::Expand(1))
                        .into(),
                ]),
            )
            .into(),
        ])
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
                Self::column(vec![
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
                ]),
            )
            .into(),
            Section::new(
                "Tags",
                Self::row(vec![
                    Tag::new("Erased", Tone::Danger).into(),
                    Tag::new("New", Tone::Success).into(),
                    Tag::new("Kept", Tone::Secondary).into(),
                ]),
            )
            .into(),
            Section::new(
                "Progress",
                Self::column(vec![
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
                ]),
            )
            .into(),
        ])
    }

    fn empty(&self) -> Node<Msg> {
        Node::SizedBox {
            width: None,
            height: Some(crate::constraints::Length::Px(EMPTY_HEIGHT)),
            child: Box::new(
                EmptyState::new(Icon::FolderOpen, "This folder is empty")
                    .body("Files you add here will show up in this list.")
                    .action(Button::new("New folder").on_press(Msg::Press))
                    .into(),
            ),
        }
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
        Self::column(vec![
            Text::new("A dialog covers the page until it is answered.")
                .wrap()
                .into(),
            Self::row(vec![
                Button::new("Show the dialog")
                    .on_press(Msg::OpenDialog)
                    .into(),
            ]),
        ])
    }

    fn dialog_node(&self) -> Node<Msg> {
        Node::Dialog {
            title: String::from("Erase this disk?"),
            content: Box::new(
                Text::new("Everything on KINGSTON SNV3S1000G will be deleted.")
                    .wrap()
                    .into(),
            ),
            footer: ButtonRow::new()
                .cancel(Button::new("Cancel").on_press(Msg::CloseDialog))
                .destructive(Button::new("Erase").on_press(Msg::CloseDialog)),
            on_dismiss: Some(Msg::CloseDialog),
        }
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
                width: Some(crate::constraints::Length::Px(SEARCH_WIDTH)),
                height: None,
                child: Box::new(
                    SearchField::new(self.filter.0.text.clone(), self.filter.0.caret)
                        .focused(self.filter.1)
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

impl App for Catalogue {
    type Message = Msg;

    fn view(&self) -> Node<Msg> {
        let window = Node::VStack {
            children: vec![
                self.toolbar(),
                Node::Expand {
                    weight: 1,
                    child: Box::new(Node::HStack {
                        children: vec![
                            self.sidebar(),
                            Node::Expand {
                                weight: 1,
                                child: Box::new(self.page(self.page)),
                            },
                        ],
                        spacing: 0,
                        align: CrossAxisAlignment::Stretch,
                    }),
                },
            ],
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
        };
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
            Msg::Name(text) => self.name = text,
            Msg::Note(text) => self.note = text,
            Msg::Search(input) => edit(&mut self.search, input),
            Msg::Size(input) => edit(&mut self.size, input),
            Msg::Select(row) => self.row = Some(row),
            Msg::Place(row) => self.place = Some(row),
            Msg::Expand(i) => self.expanded[i] = !self.expanded[i],
            Msg::OpenDialog => self.dialog = true,
            Msg::CloseDialog => self.dialog = false,
        }
        Action::Rebuild
    }

    /// The indeterminate bar on the feedback page moves only as often as it
    /// is painted.
    fn tick_interval_ms(&self) -> Option<u64> {
        (self.page == 6).then_some(TICK_MS)
    }

    fn tick(&mut self) -> Action {
        Action::Repaint
    }

    fn title(&self) -> &str {
        "Widget Gallery"
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
