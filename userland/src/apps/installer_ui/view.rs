use slopos_abi::draw::Color32;
use slopos_appkit::{
    BarSegment, ButtonStyle, CrossAxisAlignment, EdgeInsets, Length, LineEditInput, Node,
    RadioOption, ScrollDirection, ScrollbarVisibility, SegmentFill, TextAlignment,
};
use slopos_boot_core::install::Role;
use slopos_installer_core::wizard::{Kind, Page, Row, StepState, words};
use slopos_installer_core::{Contents, DEFAULT_REMOTE, Layout, Piece, Tag, human, short};

use super::{Field, InstallerApp, Msg, typed_size};

const MIB: u64 = 1 << 20;
const ACCENT: Color32 = Color32::rgb(0x74, 0xad, 0xe8);
const GREEN: Color32 = Color32::rgb(0x98, 0xc3, 0x79);
const YELLOW: Color32 = Color32::rgb(0xe5, 0xc0, 0x7b);
const RED: Color32 = Color32::rgb(0xe0, 0x6c, 0x75);
const MUTED: Color32 = Color32::rgb(0xa9, 0xaf, 0xbc);
const FAINT: Color32 = Color32::rgb(0x6b, 0x71, 0x7d);
const OTHER_SYSTEMS: [Color32; 3] = [
    Color32::rgb(0xc6, 0x78, 0xdd),
    Color32::rgb(0x56, 0xb6, 0xc2),
    Color32::rgb(0xd1, 0x9a, 0x66),
];

fn label(text: impl Into<String>) -> Node<Msg> {
    Node::Label {
        text: text.into(),
        alignment: TextAlignment::Start,
        wrap: true,
        max_lines: None,
    }
}

fn wrapped(text: impl Into<String>, color: Color32) -> Node<Msg> {
    Node::StyledLabel {
        text: text.into(),
        color,
        alignment: TextAlignment::Start,
        wrap: true,
    }
}

fn colored(text: impl Into<String>, color: Color32) -> Node<Msg> {
    Node::StyledLabel {
        text: text.into(),
        color,
        alignment: TextAlignment::Start,
        wrap: false,
    }
}

fn column(spacing: i32, children: Vec<Node<Msg>>) -> Node<Msg> {
    Node::VStack {
        children,
        spacing,
        align: CrossAxisAlignment::Stretch,
    }
}

fn row(children: Vec<Node<Msg>>) -> Node<Msg> {
    Node::HStack {
        children,
        spacing: 8,
        align: CrossAxisAlignment::Center,
    }
}

fn expand(child: Node<Msg>) -> Node<Msg> {
    Node::Expand {
        weight: 1,
        child: Box::new(child),
    }
}

fn wide(width: i32, child: Node<Msg>) -> Node<Msg> {
    Node::SizedBox {
        width: Some(Length::Px(width)),
        height: None,
        child: Box::new(child),
    }
}

/// A subject and what it says of it, the subjects in one column.
fn pair(subject: &str, color: Color32, text: impl Into<String>) -> Node<Msg> {
    row(vec![
        wide(100, colored(subject, color)),
        expand(label(text)),
    ])
}

fn button(text: impl Into<String>, msg: Msg, style: ButtonStyle, enabled: bool) -> Node<Msg> {
    Node::Button {
        label: text.into(),
        on_press: Some(msg),
        style,
        enabled,
    }
}

fn choices<K>(rows: &[Row<K>], selected: Option<usize>, on: fn(usize) -> Msg) -> Node<Msg> {
    Node::RadioGroup {
        options: rows
            .iter()
            .map(|r| RadioOption {
                label: r.label.clone(),
                detail: r.detail.clone(),
                enabled: r.enabled,
            })
            .collect(),
        selected,
        on_select: Some(on),
    }
}

fn place<K: PartialEq>(rows: &[Row<K>], key: Option<K>) -> Option<usize> {
    let key = key?;
    rows.iter().position(|r| r.key == key)
}

/// `layout` in proportion, or the whole disk as `holding` when it lays out
/// nothing.
fn bar(layout: &Layout, highlight_root: bool, holding: &str) -> Node<Msg> {
    if layout.segments.is_empty() {
        return Node::PartitionBar {
            segments: vec![BarSegment {
                label: format!("{holding} {}", human(layout.capacity)),
                size: layout.capacity.max(1),
                fill: SegmentFill::Hatched,
            }],
            highlight: None,
        };
    }
    let mut others = 0;
    let mut highlight = None;
    let segments = layout
        .segments
        .iter()
        .enumerate()
        .map(|(i, segment)| {
            let fill = match &segment.piece {
                Piece::Free { .. } => SegmentFill::Hatched,
                Piece::Partition { role, .. } => SegmentFill::Solid(match role {
                    Some(Role::Esp) => GREEN,
                    Some(Role::Boot) => YELLOW,
                    Some(Role::Root) => {
                        highlight = highlight_root.then_some(i);
                        ACCENT
                    }
                    Some(Role::Crash) => RED,
                    None => {
                        others += 1;
                        OTHER_SYSTEMS[(others - 1) % OTHER_SYSTEMS.len()]
                    }
                }),
            };
            BarSegment {
                label: segment.label(),
                size: segment.bytes,
                fill,
            }
        })
        .collect();
    Node::PartitionBar {
        segments,
        highlight,
    }
}

fn line_edit(app: &InstallerApp, field: Field, placeholder: &str) -> Node<Msg> {
    let shown;
    let (text, caret, invalid, on_input): (&str, usize, bool, fn(LineEditInput) -> Msg) =
        match (field, &app.size_typed) {
            (Field::Remote, _) => (&app.remote.text, app.remote.caret, false, Msg::Remote),
            (Field::Size, Some(typed)) => (
                &typed.text,
                typed.caret,
                typed_size(&typed.text).is_none(),
                Msg::SizeText,
            ),
            (Field::Size, None) => {
                shown = app.size_shown();
                (&shown, shown.chars().count(), false, Msg::SizeText)
            }
        };
    Node::LineEdit {
        text: text.into(),
        placeholder: placeholder.into(),
        caret,
        focused: app.field == Some(field),
        icon: None,
        suffix: String::new(),
        invalid,
        tab_stop: true,
        on_input: Some(on_input),
    }
}

fn strip(page: Page) -> Node<Msg> {
    let at = match page {
        Page::Welcome => None,
        Page::Disk => Some(0),
        Page::How => Some(1),
        Page::Review => Some(2),
        Page::Install | Page::Done | Page::Error => Some(3),
    };
    let mut children = Vec::new();
    for (i, step) in ["Disk", "How", "Review", "Install"].into_iter().enumerate() {
        if i > 0 {
            children.push(colored("·", FAINT));
        }
        let color = match at {
            Some(at) if i == at => ACCENT,
            Some(at) if i < at => MUTED,
            _ => FAINT,
        };
        children.push(colored(step, color));
    }
    row(children)
}

fn header(app: &InstallerApp) -> Node<Msg> {
    Node::Padding {
        padding: EdgeInsets::symmetric(24, 12),
        child: Box::new(row(vec![
            expand(colored("Install SlopOS", MUTED)),
            strip(app.page),
        ])),
    }
}

fn footer(app: &InstallerApp) -> Node<Msg> {
    let w = &app.wizard;
    let left = match app.page {
        Page::Welcome => Some(button(
            "Try SlopOS",
            Msg::Close,
            ButtonStyle::Secondary,
            true,
        )),
        Page::Disk | Page::How | Page::Review => Some(button(
            "Back",
            Msg::Back,
            ButtonStyle::Secondary,
            w.can_go_back(),
        )),
        Page::Done => Some(button(
            "Keep using the live system",
            Msg::Close,
            ButtonStyle::Secondary,
            true,
        )),
        Page::Error => Some(button(
            "Restart",
            Msg::Restart,
            ButtonStyle::Secondary,
            true,
        )),
        Page::Install => None,
    };
    let status = match (&app.restart_refused, app.page) {
        (Some(why), _) => wrapped(format!("The restart was refused: {why}"), RED),
        (None, _) if app.asking => wrapped("Allow the install in the dialog to go on", MUTED),
        (None, Page::Welcome) if w.waiting() => wrapped("Starting the installer…", MUTED),
        (None, Page::Disk | Page::Error) if w.waiting() => wrapped("Reading the disks…", MUTED),
        _ => Node::Empty,
    };
    let style = match app.page {
        Page::Review => ButtonStyle::Destructive,
        _ => ButtonStyle::Primary,
    };
    let mut children: Vec<Node<Msg>> = left.into_iter().collect();
    children.push(expand(status));
    if app.page != Page::Install {
        let enabled = w.can_advance() && !app.restarting;
        children.push(button(w.primary_label(), Msg::Next, style, enabled));
    }
    Node::Padding {
        padding: EdgeInsets::symmetric(24, 12),
        child: Box::new(row(children)),
    }
}

fn note(app: &InstallerApp) -> Option<Node<Msg>> {
    app.wizard.note().map(|note| wrapped(note, YELLOW))
}

fn welcome(app: &InstallerApp) -> Vec<Node<Msg>> {
    let mut page = vec![
        label("Welcome to SlopOS"),
        label(
            "This system runs from the medium it started from, and keeps nothing you do. \
             Try it as it is, or install it on a disk of this machine.",
        ),
    ];
    page.extend(note(app));
    if app.wizard.waiting() {
        page.push(Node::ProgressBar {
            value: None,
            label: String::new(),
            color: Some(ACCENT),
        });
    }
    page
}

fn disk(app: &InstallerApp) -> Vec<Node<Msg>> {
    let w = &app.wizard;
    let rows = w.disks();
    let mut page = vec![label("Which disk should SlopOS be installed on?")];
    page.extend(w.medium_notes().into_iter().map(|n| wrapped(n, YELLOW)));
    if rows.is_empty() {
        page.push(wrapped("No disk was found.", YELLOW));
    } else {
        page.push(choices(
            &rows,
            place(&rows, w.disk().map(String::from)),
            Msg::Disk,
        ));
    }
    page.push(row(vec![button(
        "Rescan",
        Msg::Rescan,
        ButtonStyle::Secondary,
        !w.waiting(),
    )]));
    page.extend(w.issue().map(|issue| wrapped(words(issue), RED)));
    page.extend(note(app));
    page
}

fn how(app: &InstallerApp) -> Vec<Node<Msg>> {
    let w = &app.wizard;
    let Some(answers) = w.answers() else {
        return Vec::new();
    };
    let mut page = vec![label(format!(
        "How should SlopOS be installed on {}?",
        short(&answers.disk)
    ))];
    let tiles = w.tile_rows();
    page.push(choices(
        &tiles,
        place(&tiles, Some(answers.kind)),
        Msg::Tile,
    ));
    let regions = w.region_rows();
    if !regions.is_empty() {
        page.push(colored("Free region", MUTED));
        page.push(choices(&regions, place(&regions, w.region()), Msg::Region));
    }
    if answers.kind == Kind::Reuse {
        let roots = w.root_rows();
        page.push(colored("Partition for the root", MUTED));
        page.push(choices(&roots, place(&roots, answers.root), Msg::Root));
    }
    if let Some(listing) = w.system().and_then(|s| s.disk(&answers.disk)?.listing()) {
        page.push(colored("Now", MUTED));
        let holding = match &listing.contents {
            Contents::Blank => "blank".into(),
            other => other.holding(),
        };
        page.push(bar(&listing.layout, false, &holding));
    }
    if let Some(Ok((_, proposal))) = w.last_answer().map(|p| &p.answer) {
        page.push(colored("After", MUTED));
        page.push(bar(&proposal.after, true, ""));
    }
    if let (Some(span), Some(size)) = (w.root_span(), w.root_size()) {
        page.push(pair(
            "Root size",
            MUTED,
            format!("{} – {}", human(span.min), human(span.max)),
        ));
        page.push(row(vec![
            expand(Node::Slider {
                value: size,
                min: span.min,
                max: span.max,
                step: MIB,
                enabled: true,
                grab: app.grab,
                on_input: Some(Msg::Size),
            }),
            wide(120, line_edit(app, Field::Size, "Size")),
        ]));
    }
    page.push(colored("Options", MUTED));
    if w.system().is_some_and(|s| s.medium.source) {
        page.push(label("Where /src/slopos fetches from and pushes to"));
        page.push(line_edit(app, Field::Remote, DEFAULT_REMOTE));
    }
    page.push(Node::Checkbox {
        checked: answers.first,
        label: "Put SlopOS first in the firmware boot order".into(),
        on_toggle: Some(Msg::First),
        enabled: true,
    });
    page.extend(w.issue().map(|issue| wrapped(words(issue), RED)));
    page
}

fn tag_color(tag: Tag) -> Color32 {
    match tag {
        Tag::Erase | Tag::Format | Tag::Reuse => RED,
        Tag::New => GREEN,
        Tag::Share => YELLOW,
        Tag::Keep => MUTED,
    }
}

fn review(app: &InstallerApp) -> Vec<Node<Msg>> {
    let w = &app.wizard;
    let Some(proposal) = w.proposal() else {
        return Vec::new();
    };
    let summary = w
        .summary_lines()
        .into_iter()
        .map(|(subject, text)| pair(subject, MUTED, text));
    let actions = proposal
        .action_lines()
        .into_iter()
        .map(|(tag, text)| pair(tag.word(), tag_color(tag), text));
    let mut page = vec![
        label("Review the install"),
        column(4, summary.collect()),
        colored("What happens to each partition", MUTED),
        column(4, actions.collect()),
    ];
    page.extend(proposal.left_out().map(|line| wrapped(line, YELLOW)));
    page.push(Node::Checkbox {
        checked: w.confirmed(),
        label: w.confirmation(),
        on_toggle: Some(Msg::Confirm),
        enabled: true,
    });
    page.extend(w.issue().map(|issue| wrapped(words(issue), RED)));
    page
}

/// As tall as the steps, which the log takes the place of.
const LOG_HEIGHT: i32 = 212;

fn steps(app: &InstallerApp) -> Node<Msg> {
    let rows = app
        .wizard
        .steps()
        .into_iter()
        .map(|(title, state)| {
            let (said, color) = match state {
                StepState::Pending => ("", FAINT),
                StepState::Running => ("now", ACCENT),
                StepState::Done => ("done", GREEN),
                StepState::Failed => ("failed", RED),
            };
            pair(said, color, title)
        })
        .collect();
    column(4, rows)
}

fn log(app: &InstallerApp) -> Node<Msg> {
    let lines = app
        .wizard
        .progress()
        .log
        .iter()
        .map(|line| Node::Label {
            text: line.clone(),
            alignment: TextAlignment::Start,
            wrap: false,
            max_lines: Some(1),
        })
        .collect();
    Node::SizedBox {
        width: None,
        height: Some(Length::Px(LOG_HEIGHT)),
        child: Box::new(Node::ScrollView {
            child: Box::new(column(0, lines)),
            direction: ScrollDirection::Vertical,
            show_scrollbar: ScrollbarVisibility::WhenNeeded,
            scroll_y: app.log_scroll,
            on_scroll: Some(Msg::LogScroll),
            follow_end: true,
        }),
    }
}

fn install(app: &InstallerApp) -> Vec<Node<Msg>> {
    let w = &app.wizard;
    let mut page: Vec<Node<Msg>> = w.outcome().into_iter().map(label).collect();
    page.push(if app.show_log { log(app) } else { steps(app) });
    let progress = w.progress();
    page.push(colored(progress.detail.clone().unwrap_or_default(), MUTED));
    page.push(match progress.bytes {
        Some((done, total)) => Node::ProgressBar {
            value: Some((done.saturating_mul(100) / total.max(1)) as u32),
            label: format!("{} of {} copied", human(done), human(total)),
            color: Some(ACCENT),
        },
        None => Node::ProgressBar {
            value: None,
            label: String::new(),
            color: Some(ACCENT),
        },
    });
    page.push(Node::Checkbox {
        checked: app.show_log,
        label: "Show the log".into(),
        on_toggle: Some(Msg::ShowLog),
        enabled: true,
    });
    page
}

fn done(app: &InstallerApp) -> Vec<Node<Msg>> {
    app.wizard.outcome().into_iter().map(label).collect()
}

fn error(app: &InstallerApp) -> Vec<Node<Msg>> {
    let w = &app.wizard;
    let outcome = w.outcome();
    let mut page = Vec::new();
    let mut lines = outcome.into_iter();
    page.extend(lines.next().map(label));
    page.extend(lines.map(|line| wrapped(line, RED)));
    page.extend(note(app));
    page.push(log(app));
    page
}

pub(super) fn window(app: &InstallerApp) -> Node<Msg> {
    let (body, scrolls) = match app.page {
        Page::Welcome => (welcome(app), false),
        Page::Disk => (disk(app), true),
        Page::How => (how(app), true),
        Page::Review => (review(app), true),
        Page::Install => (install(app), false),
        Page::Done => (done(app), false),
        Page::Error => (error(app), false),
    };
    let body = Node::Padding {
        padding: EdgeInsets::symmetric(24, 16),
        child: Box::new(column(12, body)),
    };
    let body = if scrolls {
        Node::ScrollView {
            child: Box::new(body),
            direction: ScrollDirection::Vertical,
            show_scrollbar: ScrollbarVisibility::WhenNeeded,
            scroll_y: app.page_scroll,
            on_scroll: Some(Msg::PageScroll),
            follow_end: false,
        }
    } else {
        body
    };
    column(
        0,
        vec![
            header(app),
            Node::Divider,
            expand(body),
            Node::Divider,
            footer(app),
        ],
    )
}
