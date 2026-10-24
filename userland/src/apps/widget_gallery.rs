use std::time::Duration;

use slopos_abi::draw::Color32;
use slopos_appkit::{
    Action, App, AppSender, BarSegment, ButtonStyle, CrossAxisAlignment, EdgeInsets, Length, Node,
    RadioOption, ScrollDirection, ScrollbarVisibility, SegmentFill, SliderGrab, SliderInput,
    TextAlignment,
};

#[derive(Clone, Debug)]
pub enum GalleryMsg {
    ButtonClicked,
    ToggleCheck,
    TextChanged(String),
    TabChanged(usize),
    ListSelect(usize),
    Choose(usize),
    Size(SliderInput),
    StartJob,
    JobProgress(u32),
    JobDone,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Job {
    Idle,
    /// Started, and not yet able to say how far it is.
    Preparing,
    Running(u32),
}

const PROGRESS_TAB: usize = 4;

/// The demo disk, in GiB: an EFI partition, Windows and a data partition.
const DISK: u64 = 512;
const ROOT_MIN: u64 = 7;

pub struct WidgetGalleryApp {
    counter: u32,
    checked: bool,
    text: String,
    active_tab: usize,
    selected_item: Option<usize>,
    choice: Option<usize>,
    root: u64,
    grab: Option<SliderGrab>,
    sender: Option<AppSender<GalleryMsg>>,
    job: Job,
    jobs_done: u32,
}

impl WidgetGalleryApp {
    pub fn new() -> Self {
        Self {
            counter: 0,
            checked: false,
            text: String::new(),
            active_tab: 0,
            selected_item: None,
            choice: Some(0),
            root: 200,
            grab: None,
            sender: None,
            job: Job::Idle,
            jobs_done: 0,
        }
    }

    fn label(text: impl Into<String>) -> Node<GalleryMsg> {
        Node::Label {
            text: text.into(),
            alignment: TextAlignment::Start,
            wrap: false,
            max_lines: None,
        }
    }

    fn choices_tab(&self) -> Node<GalleryMsg> {
        let option = |label: &str, detail: &str, enabled| RadioOption {
            label: String::from(label),
            detail: String::from(detail),
            enabled,
        };
        let segment = |label: &str, size, fill| BarSegment {
            label: String::from(label),
            size,
            fill,
        };
        let solid = |r, g, b| SegmentFill::Solid(Color32::rgb(r, g, b));
        let free = DISK - 3 - 1 - self.root;
        Node::Padding {
            padding: EdgeInsets::all(16),
            child: Box::new(Node::VStack {
                spacing: 8,
                align: CrossAxisAlignment::Stretch,
                children: vec![
                    Self::label("How should SlopOS be installed?"),
                    Node::RadioGroup {
                        options: vec![
                            option("Erase disk and install", "", true),
                            option(
                                "Install beside what is there",
                                "no free region holds 7.0 GiB",
                                false,
                            ),
                            option("Use a partition as the root", "", true),
                            option("Reinstall SlopOS", "keeps what nvme0n1p3 holds", true),
                        ],
                        selected: self.choice,
                        on_select: Some(GalleryMsg::Choose),
                    },
                    Node::Divider,
                    Self::label(format!("Root size: {} GiB", self.root)),
                    Node::Slider {
                        value: self.root,
                        min: ROOT_MIN,
                        max: DISK - 4,
                        step: 1,
                        enabled: true,
                        grab: self.grab,
                        on_input: Some(GalleryMsg::Size),
                    },
                    Self::label("Now"),
                    Node::PartitionBar {
                        segments: vec![
                            segment("EFI", 1, solid(0x98, 0xc3, 0x79)),
                            segment("Windows 380 GiB", 380, solid(0x61, 0xaf, 0xef)),
                            segment("data 131 GiB", 131, solid(0xc6, 0x78, 0xdd)),
                        ],
                        highlight: None,
                    },
                    Self::label("After"),
                    Node::PartitionBar {
                        segments: vec![
                            segment("EFI", 1, solid(0x98, 0xc3, 0x79)),
                            segment("boot", 2, solid(0xe5, 0xc0, 0x7b)),
                            segment(
                                &format!("SlopOS root {} GiB", self.root),
                                self.root,
                                solid(0x74, 0xad, 0xe8),
                            ),
                            segment("crash", 1, solid(0xd0, 0x72, 0x77)),
                            segment(&format!("free {free} GiB"), free, SegmentFill::Hatched),
                        ],
                        highlight: Some(2),
                    },
                ],
            }),
        }
    }

    fn progress_tab(&self) -> Node<GalleryMsg> {
        let (value, label) = match self.job {
            Job::Idle => (Some(0), String::from("idle")),
            Job::Preparing => (None, String::from("preparing")),
            Job::Running(n) => (Some(n), format!("{n}%")),
        };
        Node::Padding {
            padding: EdgeInsets::all(16),
            child: Box::new(Node::VStack {
                spacing: 8,
                align: CrossAxisAlignment::Stretch,
                children: vec![
                    Self::label("A job on a thread of its own, sending its progress"),
                    Node::ProgressBar {
                        value,
                        label,
                        color: Some(Color32::rgb(0x74, 0xad, 0xe8)),
                    },
                    Node::HStack {
                        spacing: 8,
                        align: CrossAxisAlignment::Center,
                        children: vec![
                            Node::Button {
                                label: String::from("Run a job"),
                                on_press: Some(GalleryMsg::StartJob),
                                style: ButtonStyle::Primary,
                                enabled: self.job == Job::Idle,
                            },
                            Self::label(format!("{} done", self.jobs_done)),
                        ],
                    },
                    Self::label("Indeterminate, always"),
                    Node::ProgressBar {
                        value: None,
                        label: String::new(),
                        color: None,
                    },
                ],
            }),
        }
    }

    fn start_job(&mut self) -> Action {
        let Some(sender) = self.sender.clone() else {
            return Action::None;
        };
        self.job = Job::Preparing;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            for n in 0..=100 {
                sender.send(GalleryMsg::JobProgress(n));
                std::thread::sleep(Duration::from_millis(30));
            }
            sender.send(GalleryMsg::JobDone);
        });
        Action::Rebuild
    }

    fn basics_tab(&self) -> Node<GalleryMsg> {
        Node::Padding {
            padding: EdgeInsets::all(16),
            child: Box::new(Node::VStack {
                spacing: 8,
                align: CrossAxisAlignment::Start,
                children: vec![
                    Node::Label {
                        text: String::from("Widget Gallery"),
                        alignment: TextAlignment::Start,
                        wrap: false,
                        max_lines: None,
                    },
                    Node::Divider,
                    Node::HStack {
                        spacing: 8,
                        align: CrossAxisAlignment::Center,
                        children: vec![
                            Node::Button {
                                label: format!("Click me ({})", self.counter),
                                on_press: Some(GalleryMsg::ButtonClicked),
                                style: ButtonStyle::Primary,
                                enabled: true,
                            },
                            Node::Button {
                                label: String::from("Disabled"),
                                on_press: Some(GalleryMsg::ButtonClicked),
                                style: ButtonStyle::Secondary,
                                enabled: false,
                            },
                            Node::Button {
                                label: String::from("Delete"),
                                on_press: Some(GalleryMsg::ButtonClicked),
                                style: ButtonStyle::Destructive,
                                enabled: true,
                            },
                        ],
                    },
                    Node::Checkbox {
                        checked: self.checked,
                        label: String::from("Enable feature"),
                        on_toggle: Some(GalleryMsg::ToggleCheck),
                        enabled: true,
                    },
                    Node::Spacer {
                        size: Length::Px(16),
                    },
                    Node::Label {
                        text: format!("Button clicked {} times", self.counter),
                        alignment: TextAlignment::Start,
                        wrap: false,
                        max_lines: None,
                    },
                ],
            }),
        }
    }

    fn input_tab(&self) -> Node<GalleryMsg> {
        Node::Padding {
            padding: EdgeInsets::all(16),
            child: Box::new(Node::VStack {
                spacing: 8,
                align: CrossAxisAlignment::Start,
                children: vec![
                    Node::Label {
                        text: String::from("Text Input"),
                        alignment: TextAlignment::Start,
                        wrap: false,
                        max_lines: None,
                    },
                    Node::TextField {
                        text: self.text.clone(),
                        placeholder: String::from("Type here..."),
                        on_change: Some(GalleryMsg::TextChanged),
                        max_length: None,
                        read_only: false,
                    },
                    Node::Label {
                        text: format!("Current value: {}", self.text),
                        alignment: TextAlignment::Start,
                        wrap: true,
                        max_lines: None,
                    },
                ],
            }),
        }
    }

    fn lists_tab(&self) -> Node<GalleryMsg> {
        let items: Vec<Node<GalleryMsg>> = (0..50)
            .map(|i| Node::Label {
                text: format!("Item {}", i),
                alignment: TextAlignment::Start,
                wrap: false,
                max_lines: None,
            })
            .collect();

        Node::Padding {
            padding: EdgeInsets::all(16),
            child: Box::new(Node::VStack {
                spacing: 8,
                align: CrossAxisAlignment::Start,
                children: vec![
                    Node::Label {
                        text: String::from("Scrollable List"),
                        alignment: TextAlignment::Start,
                        wrap: false,
                        max_lines: None,
                    },
                    Node::ScrollView {
                        direction: ScrollDirection::Vertical,
                        show_scrollbar: ScrollbarVisibility::WhenNeeded,
                        scroll_y: 0,
                        on_scroll: None,
                        follow_end: false,
                        child: Box::new(Node::VStack {
                            spacing: 4,
                            align: CrossAxisAlignment::Start,
                            children: items,
                        }),
                    },
                ],
            }),
        }
    }
}

impl App for WidgetGalleryApp {
    type Message = GalleryMsg;

    fn view(&self) -> Node<GalleryMsg> {
        Node::TabBar {
            tabs: vec![
                String::from("Basics"),
                String::from("Input"),
                String::from("Lists"),
                String::from("Choices"),
                String::from("Progress"),
            ],
            active: self.active_tab,
            on_change: Some(GalleryMsg::TabChanged),
            content: vec![
                self.basics_tab(),
                self.input_tab(),
                self.lists_tab(),
                self.choices_tab(),
                self.progress_tab(),
            ],
        }
    }

    fn on_start(&mut self, sender: AppSender<GalleryMsg>) -> Action {
        self.sender = Some(sender);
        Action::None
    }

    /// An indeterminate bar moves only as often as it is painted.
    fn tick_interval_ms(&self) -> Option<u64> {
        (self.active_tab == PROGRESS_TAB).then_some(50)
    }

    fn tick(&mut self) -> Action {
        Action::Repaint
    }

    fn update(&mut self, msg: GalleryMsg) -> Action {
        match msg {
            GalleryMsg::ButtonClicked => {
                self.counter += 1;
                Action::Rebuild
            }
            GalleryMsg::ToggleCheck => {
                self.checked = !self.checked;
                Action::Rebuild
            }
            GalleryMsg::TextChanged(_text) => Action::None,
            GalleryMsg::TabChanged(tab) => {
                self.active_tab = tab;
                Action::Rebuild
            }
            GalleryMsg::ListSelect(idx) => {
                self.selected_item = Some(idx);
                Action::Rebuild
            }
            GalleryMsg::Choose(choice) => {
                self.choice = Some(choice);
                Action::Rebuild
            }
            GalleryMsg::Size(input) => {
                match input {
                    SliderInput::Begin(root, grab) => {
                        self.grab = Some(grab);
                        self.root = root;
                    }
                    SliderInput::Change(root) => self.root = root,
                    SliderInput::End => self.grab = None,
                }
                Action::Rebuild
            }
            GalleryMsg::StartJob if self.job == Job::Idle => self.start_job(),
            GalleryMsg::StartJob => Action::None,
            GalleryMsg::JobProgress(n) => {
                self.job = Job::Running(n);
                Action::Rebuild
            }
            GalleryMsg::JobDone => {
                self.job = Job::Idle;
                self.jobs_done += 1;
                Action::Rebuild
            }
        }
    }
}

pub fn main() {
    let app = WidgetGalleryApp::new();
    slopos_appkit::run_app(app, 640, 480);
}
