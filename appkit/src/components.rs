//! What an app builds its pages from. Each owns its spacing, its text roles
//! and its states, so a page made of them reads like every other.
//!
//! A component converts into a [`Node`] with `.into()`; one that only
//! arranges others becomes the stacks and padding it stands for.

use slopos_theme_core::{Icon, Surface, TextRole, Tone, inset, space};

use crate::constraints::{
    CrossAxisAlignment, EdgeInsets, Length, ScrollDirection, ScrollbarVisibility, Size,
    TextAlignment,
};
use crate::node::{ButtonStyle, Node};
use crate::widgets::line_edit::LineEditInput;

/// How a text that is wider than its box goes on.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Wrap {
    /// One line per `\n`, each cut short with an ellipsis.
    #[default]
    None,
    /// At spaces; a word longer than a line breaks where it must.
    Words,
    /// Anywhere: a command line or a path, never cut.
    Chars,
}

/// Text in a role and a tone.
#[derive(Clone, Debug)]
pub struct Text {
    pub text: String,
    pub role: TextRole,
    pub tone: Tone,
    pub align: TextAlignment,
    pub wrap: Wrap,
    pub max_lines: Option<u32>,
}

impl Text {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            role: TextRole::Body,
            tone: Tone::Normal,
            align: TextAlignment::Start,
            wrap: Wrap::None,
            max_lines: None,
        }
    }

    pub fn role(mut self, role: TextRole) -> Self {
        self.role = role;
        self
    }

    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    pub fn align(mut self, align: TextAlignment) -> Self {
        self.align = align;
        self
    }

    /// Wraps at spaces, as prose does.
    pub fn wrap(mut self) -> Self {
        self.wrap = Wrap::Words;
        self
    }

    /// Wraps anywhere, so the whole of a command line or a path shows.
    pub fn wrap_anywhere(mut self) -> Self {
        self.wrap = Wrap::Chars;
        self
    }

    /// At most `n` lines, the last cut short with an ellipsis.
    pub fn max_lines(mut self, n: u32) -> Self {
        self.max_lines = Some(n);
        self
    }
}

impl<M> From<Text> for Node<M> {
    fn from(text: Text) -> Self {
        Node::Text(text)
    }
}

/// A push button. Its style follows from where a [`ButtonRow`] puts it; a
/// button outside one is secondary unless told otherwise.
#[derive(Clone, Debug)]
pub struct Button<M> {
    pub label: String,
    pub on_press: Option<M>,
    pub enabled: bool,
    pub style: ButtonStyle,
}

impl<M> Button<M> {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            on_press: None,
            enabled: true,
            style: ButtonStyle::Secondary,
        }
    }

    pub fn on_press(mut self, msg: M) -> Self {
        self.on_press = Some(msg);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Filled with the accent: what the page is for.
    pub fn primary(mut self) -> Self {
        self.style = ButtonStyle::Primary;
        self
    }

    /// Filled with the danger tone: it destroys something.
    pub fn destructive(mut self) -> Self {
        self.style = ButtonStyle::Destructive;
        self
    }
}

impl<M> From<Button<M>> for Node<M> {
    fn from(b: Button<M>) -> Self {
        Node::Button {
            label: b.label,
            on_press: b.on_press,
            style: b.style,
            enabled: b.enabled,
        }
    }
}

/// The buttons that end a page or a dialog: going back or aside on the left,
/// finishing on the right with the default last. In a footer, Enter presses
/// the default unless it destroys, and Escape the way back.
#[derive(Clone, Debug)]
pub struct ButtonRow<M> {
    pub leading: Vec<Button<M>>,
    pub trailing: Vec<Button<M>>,
    /// Index into `trailing` of the button Enter presses.
    pub default: Option<usize>,
    /// The button Escape presses: in `leading` when `true`, else `trailing`.
    pub escape: Option<(bool, usize)>,
    /// Ends a page or a dialog, so Enter and Escape reach it.
    pub(crate) ends: bool,
}

impl<M> Default for ButtonRow<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> ButtonRow<M> {
    pub fn new() -> Self {
        Self {
            leading: Vec::new(),
            trailing: Vec::new(),
            default: None,
            escape: None,
            ends: false,
        }
    }

    /// On the left, pressed by Escape.
    pub fn back(mut self, button: Button<M>) -> Self {
        self.escape = Some((true, self.leading.len()));
        self.leading.push(button.secondary_style());
        self
    }

    /// On the left, aside from finishing; one that destroys stays filled with
    /// the danger tone, as *Discard* does.
    pub fn aside(mut self, mut button: Button<M>) -> Self {
        if button.style == ButtonStyle::Primary {
            button.style = ButtonStyle::Secondary;
        }
        self.leading.push(button);
        self
    }

    /// On the right, before the default, pressed by Escape.
    pub fn cancel(mut self, button: Button<M>) -> Self {
        self.escape = Some((false, self.trailing.len()));
        self.trailing.push(button.secondary_style());
        self
    }

    /// Last on the right and filled, pressed by Enter.
    pub fn confirm(mut self, button: Button<M>) -> Self {
        self.default = Some(self.trailing.len());
        self.trailing.push(button.primary());
        self
    }

    /// Last on the right, filled with the danger tone even while disabled;
    /// Enter never presses it.
    pub fn destructive(mut self, button: Button<M>) -> Self {
        self.trailing.push(button.destructive());
        self
    }
}

impl<M> Button<M> {
    fn secondary_style(mut self) -> Self {
        self.style = ButtonStyle::Secondary;
        self
    }
}

impl<M> From<ButtonRow<M>> for Node<M> {
    fn from(row: ButtonRow<M>) -> Self {
        Node::ButtonRow(row)
    }
}

/// A screen of an app: a title, what it is about, its content, scrolled when
/// it does not fit, and the buttons that end it below a separator.
pub struct Page<M> {
    pub title: String,
    pub subtitle: String,
    pub content: Node<M>,
    pub footer: Option<ButtonRow<M>>,
}

impl<M> Page<M> {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            subtitle: String::new(),
            content: Node::Empty,
            footer: None,
        }
    }

    pub fn subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = subtitle.into();
        self
    }

    pub fn content(mut self, content: impl Into<Node<M>>) -> Self {
        self.content = content.into();
        self
    }

    pub fn footer(mut self, footer: ButtonRow<M>) -> Self {
        self.footer = Some(footer);
        self
    }
}

fn header<M>(title: String, subtitle: String) -> Node<M> {
    let mut lines = vec![Text::new(title).role(TextRole::Title).wrap().into()];
    if !subtitle.is_empty() {
        lines.push(Text::new(subtitle).tone(Tone::Secondary).wrap().into());
    }
    column(space::XS, lines)
}

fn column<M>(spacing: i32, children: Vec<Node<M>>) -> Node<M> {
    Node::VStack {
        children,
        spacing,
        align: CrossAxisAlignment::Stretch,
    }
}

fn padded<M>(padding: EdgeInsets, child: Node<M>) -> Node<M> {
    Node::Padding {
        padding,
        child: Box::new(child),
    }
}

fn expand<M>(child: Node<M>) -> Node<M> {
    Node::Expand {
        weight: 1,
        child: Box::new(child),
    }
}

impl<M> From<Page<M>> for Node<M> {
    fn from(page: Page<M>) -> Self {
        let top = EdgeInsets::new(inset::PAGE.top, inset::PAGE.right, 0, inset::PAGE.left);
        let body = Node::ScrollView {
            child: Box::new(padded(inset::PAGE, page.content)),
            direction: ScrollDirection::Vertical,
            show_scrollbar: ScrollbarVisibility::WhenNeeded,
            scroll_y: 0,
            on_scroll: None,
            follow_end: false,
        };
        let mut parts = vec![padded(top, header(page.title, page.subtitle)), expand(body)];
        if let Some(mut footer) = page.footer {
            footer.ends = true;
            parts.push(Node::Divider);
            parts.push(padded(inset::SECTION, footer.into()));
        }
        Node::Background {
            surface: Surface::Base,
            child: Box::new(column(0, parts)),
        }
    }
}

/// A heading and what it heads.
pub struct Section<M> {
    pub heading: String,
    pub content: Node<M>,
}

impl<M> Section<M> {
    pub fn new(heading: impl Into<String>, content: impl Into<Node<M>>) -> Self {
        Self {
            heading: heading.into(),
            content: content.into(),
        }
    }
}

impl<M> From<Section<M>> for Node<M> {
    fn from(section: Section<M>) -> Self {
        column(
            space::SM,
            vec![
                Text::new(section.heading)
                    .role(TextRole::Heading)
                    .wrap()
                    .into(),
                section.content,
            ],
        )
    }
}

/// Names and what each names, the names in a column as wide as the widest,
/// each row on one baseline.
pub struct PropertyList<M> {
    pub rows: Vec<(String, Node<M>)>,
}

impl<M> PropertyList<M> {
    pub fn new() -> Self {
        Self { rows: Vec::new() }
    }

    /// A row whose value is text, wrapped.
    pub fn row(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.rows
            .push((label.into(), Text::new(value.into()).wrap().into()));
        self
    }

    pub fn row_with(mut self, label: impl Into<String>, value: impl Into<Node<M>>) -> Self {
        self.rows.push((label.into(), value.into()));
        self
    }
}

impl<M> Default for PropertyList<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> From<PropertyList<M>> for Node<M> {
    fn from(list: PropertyList<M>) -> Self {
        Node::PropertyList(list.rows)
    }
}

/// Something the person should know before going on, with the icon of its
/// tone and what they can do about it.
pub struct Notice<M> {
    pub tone: Tone,
    pub title: String,
    pub body: String,
    pub action: Option<Button<M>>,
}

impl<M> Notice<M> {
    pub fn new(tone: Tone, title: impl Into<String>) -> Self {
        Self {
            tone,
            title: title.into(),
            body: String::new(),
            action: None,
        }
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    pub fn action(mut self, action: Button<M>) -> Self {
        self.action = Some(action);
        self
    }
}

impl<M> From<Notice<M>> for Node<M> {
    fn from(notice: Notice<M>) -> Self {
        let mut words = vec![
            Text::new(notice.title)
                .role(TextRole::Heading)
                .wrap()
                .into(),
        ];
        if !notice.body.is_empty() {
            words.push(Text::new(notice.body).wrap().into());
        }
        let mut text = column(space::XS, words);
        if let Some(action) = notice.action {
            let button = Node::HStack {
                children: vec![action.into()],
                spacing: 0,
                align: CrossAxisAlignment::Start,
            };
            text = column(space::SM, vec![text, button]);
        }
        let icon = Node::Icon {
            icon: Icon::for_tone(notice.tone),
            role: TextRole::Heading,
            tone: notice.tone,
        };
        Node::Card {
            surface: Surface::Raised,
            border: true,
            shadow: false,
            child: Box::new(padded(
                inset::CARD,
                Node::HStack {
                    children: vec![icon, expand(text)],
                    spacing: space::MD,
                    align: CrossAxisAlignment::Start,
                },
            )),
        }
    }
}

/// One row of a [`List`]: what it is, a line about it and what is at its
/// end. A row that cannot be chosen says why in place of its line.
#[derive(Clone, Debug, Default)]
pub struct ListRow {
    pub icon: Option<Icon>,
    pub avatar: bool,
    pub title: String,
    pub detail: String,
    pub trailing: String,
    pub disabled_reason: Option<String>,
}

impl ListRow {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Lead with the initial of its title, as a person is shown.
    pub fn avatar(mut self) -> Self {
        self.avatar = true;
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn trailing(mut self, trailing: impl Into<String>) -> Self {
        self.trailing = trailing.into();
        self
    }

    /// Cannot be chosen, for `reason`.
    pub fn disabled(mut self, reason: impl Into<String>) -> Self {
        self.disabled_reason = Some(reason.into());
        self
    }

    pub fn enabled(&self) -> bool {
        self.disabled_reason.is_none()
    }
}

/// Rows in sections, one of which may be chosen: one tab stop, the arrows
/// moving the choice past rows that cannot be chosen.
pub struct List<M> {
    pub sections: Vec<(String, Vec<ListRow>)>,
    /// Counted across every section's rows.
    pub selected: Option<usize>,
    pub on_select: Option<fn(usize) -> M>,
    pub on_activate: Option<fn(usize) -> M>,
}

impl<M> List<M> {
    pub fn new(rows: Vec<ListRow>) -> Self {
        Self {
            sections: vec![(String::new(), rows)],
            selected: None,
            on_select: None,
            on_activate: None,
        }
    }

    pub fn sectioned(sections: Vec<(String, Vec<ListRow>)>) -> Self {
        Self {
            sections,
            selected: None,
            on_select: None,
            on_activate: None,
        }
    }

    pub fn selected(mut self, selected: Option<usize>) -> Self {
        self.selected = selected;
        self
    }

    pub fn on_select(mut self, f: fn(usize) -> M) -> Self {
        self.on_select = Some(f);
        self
    }

    pub fn on_activate(mut self, f: fn(usize) -> M) -> Self {
        self.on_activate = Some(f);
        self
    }
}

impl<M> From<List<M>> for Node<M> {
    fn from(list: List<M>) -> Self {
        Node::List(list)
    }
}

/// The places an app moves between, down its side.
pub struct Sidebar<M> {
    content: Node<M>,
}

impl<M> Sidebar<M> {
    pub fn new(list: List<M>) -> Self {
        Self {
            content: list.into(),
        }
    }

    /// A sidebar with no place to go, saying why.
    pub fn empty(reason: impl Into<String>) -> Self {
        Self {
            content: Text::new(reason)
                .role(TextRole::Caption)
                .tone(Tone::Secondary)
                .wrap()
                .into(),
        }
    }
}

impl<M> From<Sidebar<M>> for Node<M> {
    fn from(sidebar: Sidebar<M>) -> Self {
        Node::Background {
            surface: Surface::Sunken,
            child: Box::new(Node::SizedBox {
                width: Some(Length::Px(SIDEBAR_WIDTH)),
                height: None,
                child: Box::new(padded(
                    EdgeInsets::all(space::SM),
                    Node::ScrollView {
                        child: Box::new(sidebar.content),
                        direction: ScrollDirection::Vertical,
                        show_scrollbar: ScrollbarVisibility::WhenNeeded,
                        scroll_y: 0,
                        on_scroll: None,
                        follow_end: false,
                    },
                )),
            }),
        }
    }
}

const SIDEBAR_WIDTH: i32 = 200;

/// A control that shows only an icon; its label is its tooltip and what a
/// screen reader would say.
#[derive(Clone, Debug)]
pub struct IconButton<M> {
    pub icon: Icon,
    pub label: String,
    pub on_press: Option<M>,
    pub enabled: bool,
    /// Shown pressed in, as a toggled tool is.
    pub active: bool,
}

impl<M> IconButton<M> {
    pub fn new(icon: Icon, label: impl Into<String>) -> Self {
        Self {
            icon,
            label: label.into(),
            on_press: None,
            enabled: true,
            active: false,
        }
    }

    pub fn on_press(mut self, msg: M) -> Self {
        self.on_press = Some(msg);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }
}

impl<M> From<IconButton<M>> for Node<M> {
    fn from(button: IconButton<M>) -> Self {
        Node::IconButton(button)
    }
}

/// A band of tools across the top of a view: some at its start, some at its
/// end.
pub struct Toolbar<M> {
    pub start: Vec<Node<M>>,
    pub end: Vec<Node<M>>,
}

impl<M> Toolbar<M> {
    pub fn new() -> Self {
        Self {
            start: Vec::new(),
            end: Vec::new(),
        }
    }

    pub fn start(mut self, item: impl Into<Node<M>>) -> Self {
        self.start.push(item.into());
        self
    }

    pub fn end(mut self, item: impl Into<Node<M>>) -> Self {
        self.end.push(item.into());
        self
    }
}

impl<M> Default for Toolbar<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> From<Toolbar<M>> for Node<M> {
    fn from(bar: Toolbar<M>) -> Self {
        let group = |items: Vec<Node<M>>| Node::HStack {
            children: items,
            spacing: space::XS,
            align: CrossAxisAlignment::Center,
        };
        let row = Node::HStack {
            children: vec![
                group(bar.start),
                expand(Node::Spacer {
                    size: Length::Px(0),
                }),
                group(bar.end),
            ],
            spacing: space::SM,
            align: CrossAxisAlignment::Center,
        };
        Node::Background {
            surface: Surface::Chrome,
            child: Box::new(column(
                0,
                vec![
                    padded(EdgeInsets::symmetric(space::SM, space::XS), row),
                    Node::Divider,
                ],
            )),
        }
    }
}

/// A field that narrows what a view shows as the person types.
pub struct SearchField<M> {
    pub text: String,
    pub caret: usize,
    pub focused: bool,
    /// Right-aligned inside the field: how many matched.
    pub count: String,
    pub on_input: Option<fn(LineEditInput) -> M>,
}

impl<M> SearchField<M> {
    pub fn new(text: impl Into<String>, caret: usize) -> Self {
        Self {
            text: text.into(),
            caret,
            focused: false,
            count: String::new(),
            on_input: None,
        }
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn count(mut self, count: impl Into<String>) -> Self {
        self.count = count.into();
        self
    }

    pub fn on_input(mut self, f: fn(LineEditInput) -> M) -> Self {
        self.on_input = Some(f);
        self
    }
}

impl<M> From<SearchField<M>> for Node<M> {
    fn from(field: SearchField<M>) -> Self {
        Node::LineEdit {
            text: field.text,
            placeholder: String::from("Search"),
            caret: field.caret,
            focused: field.focused,
            icon: Some(Icon::Search),
            suffix: field.count,
            invalid: false,
            tab_stop: true,
            on_input: field.on_input,
        }
    }
}

/// A word that marks what happens to a thing: *Erased*, *New*, *Kept*.
#[derive(Clone, Debug)]
pub struct Tag {
    pub text: String,
    pub tone: Tone,
}

impl Tag {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

impl<M> From<Tag> for Node<M> {
    fn from(tag: Tag) -> Self {
        Node::Tag(tag)
    }
}

/// What a view shows when it has nothing to show: why, and what to do.
pub struct EmptyState<M> {
    pub icon: Icon,
    pub title: String,
    pub body: String,
    pub action: Option<Button<M>>,
}

impl<M> EmptyState<M> {
    pub fn new(icon: Icon, title: impl Into<String>) -> Self {
        Self {
            icon,
            title: title.into(),
            body: String::new(),
            action: None,
        }
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    pub fn action(mut self, action: Button<M>) -> Self {
        self.action = Some(action);
        self
    }
}

impl<M> From<EmptyState<M>> for Node<M> {
    fn from(state: EmptyState<M>) -> Self {
        let centred = |text: Text| text.align(TextAlignment::Center).wrap().into();
        let mut parts = vec![
            Node::Icon {
                icon: state.icon,
                role: TextRole::Display,
                tone: Tone::Secondary,
            },
            centred(Text::new(state.title).role(TextRole::Title)),
        ];
        if !state.body.is_empty() {
            parts.push(centred(Text::new(state.body).tone(Tone::Secondary)));
        }
        if let Some(action) = state.action {
            parts.push(action.primary().into());
        }
        let fill = || {
            expand(Node::Spacer {
                size: Length::Px(0),
            })
        };
        padded(
            inset::PAGE,
            Node::VStack {
                children: vec![
                    fill(),
                    Node::VStack {
                        children: parts,
                        spacing: space::MD,
                        align: CrossAxisAlignment::Center,
                    },
                    fill(),
                ],
                spacing: 0,
                align: CrossAxisAlignment::Stretch,
            },
        )
    }
}

/// More that the person may open: a title with a chevron, its content shown
/// while expanded.
pub struct Disclosure<M> {
    pub title: String,
    pub expanded: bool,
    pub on_toggle: Option<M>,
    pub content: Node<M>,
}

impl<M> Disclosure<M> {
    pub fn new(title: impl Into<String>, content: impl Into<Node<M>>) -> Self {
        Self {
            title: title.into(),
            expanded: false,
            on_toggle: None,
            content: content.into(),
        }
    }

    pub fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = expanded;
        self
    }

    pub fn on_toggle(mut self, msg: M) -> Self {
        self.on_toggle = Some(msg);
        self
    }
}

impl<M> From<Disclosure<M>> for Node<M> {
    fn from(disclosure: Disclosure<M>) -> Self {
        Node::Disclosure(Box::new(disclosure))
    }
}

/// What a window asks of the compositor when it opens.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct WindowSpec {
    pub size: Size,
    pub min_size: Size,
    pub resizable: bool,
}

impl WindowSpec {
    /// A resizable window of `width` × `height`, as small as half of each.
    pub const fn new(width: i32, height: i32) -> Self {
        Self {
            size: Size::new(width, height),
            min_size: Size::new(width / 2, height / 2),
            resizable: true,
        }
    }

    pub const fn min_size(mut self, width: i32, height: i32) -> Self {
        self.min_size = Size::new(width, height);
        self
    }

    /// Always `size`: a dialog-like window whose layout has one size.
    pub const fn fixed(mut self) -> Self {
        self.resizable = false;
        self
    }

    /// The least the window may be, and the most when it may not grow.
    pub const fn limits(&self) -> (Size, Option<Size>) {
        match self.resizable {
            true => (self.min_size, None),
            false => (self.size, Some(self.size)),
        }
    }
}

impl Default for WindowSpec {
    fn default() -> Self {
        Self::new(640, 480)
    }
}
