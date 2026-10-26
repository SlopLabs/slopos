use slopos_appkit::theme::{inset, space};
use slopos_appkit::{
    BarSegment, ButtonStyle, CrossAxisAlignment, Length, LineEditInput, Node, RadioOption,
    ScrollDirection, ScrollbarVisibility, SegmentFill, Text, Tone,
};
use slopos_boot_core::install::Role;
use slopos_installer_core::wizard::{Kind, Page, Row, StepState, words};
use slopos_installer_core::{Contents, DEFAULT_REMOTE, Layout, Piece, Tag, human, short};

use super::{Field, InstallerApp, Msg, typed_size};

const MIB: u64 = 1 << 20;
const ROOT: usize = 0;
const ESP: usize = 1;
const BOOT: usize = 2;
const CRASH: usize = 3;
const OTHER_SYSTEMS: [usize; 3] = [4, 5, 6];

fn label(text: impl Into<String>) -> Node<Msg> {
    Text::new(text).wrap().into()
}

fn wrapped(text: impl Into<String>, tone: Tone) -> Node<Msg> {
    Text::new(text).tone(tone).wrap().into()
}

fn toned(text: impl Into<String>, tone: Tone) -> Node<Msg> {
    Text::new(text).tone(tone).into()
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
        spacing: space::SM,
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

/// A subject and what it says of it, the subjects in one column, each on the
/// first line of its text however that wraps.
fn pair(subject: &str, tone: Tone, text: impl Into<String>) -> Node<Msg> {
    Node::HStack {
        children: vec![wide(100, toned(subject, tone)), expand(label(text))],
        spacing: space::SM,
        align: CrossAxisAlignment::Baseline,
    }
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
                Piece::Partition { role, .. } => SegmentFill::Series(match role {
                    Some(Role::Esp) => ESP,
                    Some(Role::Boot) => BOOT,
                    Some(Role::Root) => {
                        highlight = highlight_root.then_some(i);
                        ROOT
                    }
                    Some(Role::Crash) => CRASH,
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
            children.push(toned("·", Tone::Disabled));
        }
        let tone = match at {
            Some(at) if i == at => Tone::Accent,
            Some(at) if i < at => Tone::Normal,
            _ => Tone::Secondary,
        };
        children.push(toned(step, tone));
    }
    row(children)
}

fn header(app: &InstallerApp) -> Node<Msg> {
    Node::Padding {
        padding: inset::SECTION,
        child: Box::new(row(vec![
            expand(toned("Install SlopOS", Tone::Secondary)),
            strip(app.page),
        ])),
    }
}

fn footer(app: &InstallerApp) -> Node<Msg> {
    let w = &app.wizard;
    let left = match app.page {
        Page::Welcome => Some(button(
            if app.welcome { "Try SlopOS" } else { "Close" },
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
            if app.welcome {
                "Keep using the live system"
            } else {
                "Close"
            },
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
        (Some(why), _) => wrapped(format!("The restart was refused: {why}"), Tone::Danger),
        (None, _) if app.asking => {
            wrapped("Allow the install in the dialog to go on", Tone::Secondary)
        }
        (None, Page::Welcome) if w.waiting() => wrapped("Starting the installer…", Tone::Secondary),
        (None, Page::Disk | Page::Error) if w.waiting() => {
            wrapped("Reading the disks…", Tone::Secondary)
        }
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
        padding: inset::SECTION,
        child: Box::new(row(children)),
    }
}

fn note(app: &InstallerApp) -> Option<Node<Msg>> {
    app.wizard.note().map(|note| wrapped(note, Tone::Warning))
}

fn welcome(app: &InstallerApp) -> Vec<Node<Msg>> {
    let mut page = if app.welcome {
        vec![
            label("Welcome to SlopOS"),
            label(
                "This system runs from the medium it started from, and keeps nothing you do. \
                 Try it as it is, or install it on a disk of this machine.",
            ),
        ]
    } else {
        vec![label(
            "SlopOS installs from the medium it started from onto a disk of this machine. \
             Nothing is written before you have reviewed what the install does.",
        )]
    };
    page.extend(note(app));
    if app.wizard.waiting() {
        page.push(Node::ProgressBar {
            value: None,
            label: String::new(),
            tone: Tone::Accent,
        });
    }
    page
}

fn disk(app: &InstallerApp) -> Vec<Node<Msg>> {
    let w = &app.wizard;
    let rows = w.disks();
    let mut page = vec![label("Which disk should SlopOS be installed on?")];
    page.extend(
        w.medium_notes()
            .into_iter()
            .map(|n| wrapped(n, Tone::Warning)),
    );
    if rows.is_empty() {
        page.push(wrapped("No disk was found.", Tone::Warning));
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
    page.extend(w.issue().map(|issue| wrapped(words(issue), Tone::Danger)));
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
        page.push(toned("Free region", Tone::Secondary));
        page.push(choices(&regions, place(&regions, w.region()), Msg::Region));
    }
    if answers.kind == Kind::Reuse {
        let roots = w.root_rows();
        page.push(toned("Partition for the root", Tone::Secondary));
        page.push(choices(&roots, place(&roots, answers.root), Msg::Root));
    }
    if let Some(listing) = w.system().and_then(|s| s.disk(&answers.disk)?.listing()) {
        page.push(toned("Now", Tone::Secondary));
        let holding = match &listing.contents {
            Contents::Blank => "blank".into(),
            other => other.holding(),
        };
        page.push(bar(&listing.layout, false, &holding));
    }
    if let Some(Ok((_, proposal))) = w.last_answer().map(|p| &p.answer) {
        page.push(toned("After", Tone::Secondary));
        page.push(bar(&proposal.after, true, ""));
    }
    if let (Some(span), Some(size)) = (w.root_span(), w.root_size()) {
        page.push(pair(
            "Root size",
            Tone::Secondary,
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
    page.push(toned("Options", Tone::Secondary));
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
    page.extend(w.issue().map(|issue| wrapped(words(issue), Tone::Danger)));
    page
}

fn tag_tone(tag: Tag) -> Tone {
    match tag {
        Tag::Erase | Tag::Format | Tag::Reuse => Tone::Danger,
        Tag::New => Tone::Success,
        Tag::Share => Tone::Warning,
        Tag::Keep => Tone::Secondary,
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
        .map(|(subject, text)| pair(subject, Tone::Secondary, text));
    let actions = proposal
        .action_lines()
        .into_iter()
        .map(|(tag, text)| pair(tag.word(), tag_tone(tag), text));
    let mut page = vec![
        label("Review the install"),
        column(space::XS, summary.collect()),
        toned("What happens to each partition", Tone::Secondary),
        column(space::XS, actions.collect()),
    ];
    page.extend(proposal.left_out().map(|line| wrapped(line, Tone::Warning)));
    page.push(Node::Checkbox {
        checked: w.confirmed(),
        label: w.confirmation(),
        on_toggle: Some(Msg::Confirm),
        enabled: true,
    });
    page.extend(w.issue().map(|issue| wrapped(words(issue), Tone::Danger)));
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
            let (said, tone) = match state {
                StepState::Pending => ("", Tone::Disabled),
                StepState::Running => ("now", Tone::Accent),
                StepState::Done => ("done", Tone::Success),
                StepState::Failed => ("failed", Tone::Danger),
            };
            pair(said, tone, title)
        })
        .collect();
    column(space::XS, rows)
}

fn log(app: &InstallerApp) -> Node<Msg> {
    let lines = app
        .wizard
        .progress()
        .log
        .iter()
        .map(|line| Text::new(line.clone()).into())
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
    page.push(toned(
        progress.detail.clone().unwrap_or_default(),
        Tone::Secondary,
    ));
    page.push(match progress.bytes {
        Some((done, total)) => Node::ProgressBar {
            value: Some((done.saturating_mul(100) / total.max(1)) as u32),
            label: format!("{} of {} copied", human(done), human(total)),
            tone: Tone::Accent,
        },
        None => Node::ProgressBar {
            value: None,
            label: String::new(),
            tone: Tone::Accent,
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
    page.extend(lines.map(|line| wrapped(line, Tone::Danger)));
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
        padding: inset::PAGE,
        child: Box::new(column(space::MD, body)),
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
