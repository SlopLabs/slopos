use slopos_appkit::theme::space;
use slopos_appkit::{
    BarSegment, Button, ButtonRow, CrossAxisAlignment, Disclosure, Icon, Length, LineEditInput,
    List, ListRow, Node, Notice, PropertyList, Section, SegmentFill, Tag as TagChip, Text,
    TextRole, Tone, column, row,
};
use slopos_boot_core::disk_id::Bus;
use slopos_boot_core::install::Role;
use slopos_installer_core::wizard::{Kind, Note, Page, Row, StepState, holding, say};
use slopos_installer_core::{DEFAULT_REMOTE, Failure, Layout, Piece, Tag, disk_size, parse_size};

use super::{Field, InstallerApp, Msg};

const MIB: u64 = 1 << 20;
const ROOT: usize = 0;
const ESP: usize = 1;
const BOOT: usize = 2;
const CRASH: usize = 3;
const OTHER_SYSTEMS: [usize; 3] = [4, 5, 6];

fn caption(text: impl Into<String>) -> Node<Msg> {
    Text::new(text)
        .role(TextRole::Caption)
        .tone(Tone::Secondary)
        .wrap()
        .into()
}

fn expand(child: Node<Msg>) -> Node<Msg> {
    Node::Expand {
        weight: 1,
        child: Box::new(child),
    }
}

fn notice(note: Note) -> Node<Msg> {
    Notice::new(note.tone, note.title).body(note.body).into()
}

fn notes(app: &InstallerApp) -> impl Iterator<Item = Node<Msg>> {
    app.wizard.notes().into_iter().map(notice)
}

fn choices<K: PartialEq>(
    rows: &[Row<K>],
    key: Option<K>,
    icon: impl Fn(&Row<K>) -> Option<Icon>,
    on: fn(usize) -> Msg,
) -> Node<Msg> {
    let list_rows = rows
        .iter()
        .map(|r| {
            let mut list_row = ListRow::new(r.title.clone()).detail(r.detail.clone());
            if let Some(icon) = icon(r) {
                list_row = list_row.icon(icon);
            }
            match &r.reason {
                Some(reason) => list_row.disabled(reason.clone()),
                None => list_row,
            }
        })
        .collect();
    let selected = key.and_then(|key| rows.iter().position(|r| r.key == key));
    List::new(list_rows).selected(selected).on_select(on).into()
}

/// `layout` in proportion, or the whole disk as `holding` when it lays out
/// nothing.
fn bar(layout: &Layout, highlight_root: bool, holding: &str) -> Node<Msg> {
    if layout.segments.is_empty() {
        return Node::PartitionBar {
            segments: vec![BarSegment {
                label: format!("{holding} · {}", disk_size(layout.capacity)),
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
                parse_size(&typed.text).is_none(),
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

fn waiting(said: &str) -> Node<Msg> {
    column(
        space::SM,
        vec![
            Node::ProgressBar {
                value: None,
                label: String::new(),
                tone: Tone::Accent,
            },
            caption(said),
        ],
    )
}

fn busy(app: &InstallerApp) -> Option<Node<Msg>> {
    let w = &app.wizard;
    if app.asking {
        Some(Notice::new(Tone::Info, say::ASKING).into())
    } else if w.starting() {
        Some(waiting(say::STARTING))
    } else {
        w.waiting().then(|| waiting(say::READING))
    }
}

fn welcome(app: &InstallerApp) -> Vec<Node<Msg>> {
    let mut page: Vec<Node<Msg>> = notes(app).collect();
    page.extend(busy(app));
    page
}

fn disk(app: &InstallerApp) -> Vec<Node<Msg>> {
    let w = &app.wizard;
    let mut page: Vec<Node<Msg>> = notes(app).collect();
    let rows = w.disks();
    let icon = |row: &Row<String>| {
        let bus = w.system()?.disk(&row.key)?.bus;
        Some(match bus {
            Some(Bus::Usb) => Icon::Usb,
            _ => Icon::HardDrive,
        })
    };
    if !rows.is_empty() {
        page.push(choices(&rows, w.disk().map(String::from), icon, Msg::Disk));
    }
    if w.waiting() {
        page.push(waiting(say::READING));
    }
    page
}

fn how(app: &InstallerApp) -> Vec<Node<Msg>> {
    let w = &app.wizard;
    let Some(answers) = w.answers() else {
        return Vec::new();
    };
    let mut page = vec![choices(
        &w.tile_rows(),
        Some(answers.kind),
        |_| None,
        Msg::Tile,
    )];
    let regions = w.region_rows();
    if !regions.is_empty() {
        let list = choices(&regions, w.region(), |_| None, Msg::Region);
        page.push(Section::new(say::FREE_SPACE, list).into());
    }
    if answers.kind == Kind::Reuse {
        let list = choices(&w.root_rows(), answers.root, |_| None, Msg::Root);
        page.push(Section::new(say::PARTITION, list).into());
    }
    page.extend(notes(app));
    if let (Some(range), Some(size), Some(span)) = (w.size_range(), w.root_size(), w.root_span()) {
        let slider = Node::Slider {
            value: size,
            min: span.min,
            max: span.max,
            step: MIB,
            enabled: true,
            grab: app.grab,
            on_input: Some(Msg::Size),
        };
        let field = Node::SizedBox {
            width: Some(Length::Px(SIZE_FIELD)),
            height: None,
            child: Box::new(line_edit(app, Field::Size, say::SIZE)),
        };
        let content = column(
            space::XS,
            vec![row(space::SM, vec![expand(slider), field]), caption(range)],
        );
        page.push(Section::new(say::SIZE, content).into());
    }
    if let Some(listing) = w.system().and_then(|s| s.disk(&answers.disk)?.listing()) {
        let holding = holding(listing);
        page.push(Section::new(say::NOW, bar(&listing.layout, false, &holding)).into());
    }
    if let Some(Ok((_, proposal))) = w.last_answer().map(|p| &p.answer) {
        page.push(Section::new(say::AFTER, bar(&proposal.after, true, "")).into());
    }
    let mut options = Vec::new();
    if w.system().is_some_and(|s| s.medium.source) {
        let field = line_edit(app, Field::Remote, DEFAULT_REMOTE);
        let remote = PropertyList::new().row_with(
            say::REMOTE,
            column(space::XS, vec![field, caption(say::REMOTE_CAPTION)]),
        );
        options.push(remote.into());
    }
    options.push(Node::Checkbox {
        checked: answers.first,
        label: say::FIRST.into(),
        on_toggle: Some(Msg::First),
        enabled: true,
    });
    page.push(Section::new(say::OPTIONS, column(space::SM, options)).into());
    page
}

/// What the size box is as wide as: a size such as `99.9 GB` and the
/// field's own padding.
const SIZE_FIELD: i32 = 15 * space::SM;

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
        .fold(PropertyList::new(), |list, (name, value)| {
            list.row(name, value)
        });
    let actions = proposal
        .action_lines()
        .into_iter()
        .map(|(tag, line)| Node::HStack {
            children: vec![
                TagChip::new(tag.word(), tag_tone(tag)).into(),
                expand(Text::new(line).wrap().into()),
            ],
            spacing: space::SM,
            align: CrossAxisAlignment::Baseline,
        })
        .collect();
    let mut page: Vec<Node<Msg>> = notes(app).collect();
    page.push(Section::new(say::SUMMARY, summary).into());
    page.push(Section::new(say::PARTITIONS, column(space::XS, actions)).into());
    page.push(Node::Checkbox {
        checked: w.confirmed(),
        label: w.confirmation(),
        on_toggle: Some(Msg::Confirm),
        enabled: true,
    });
    page
}

/// About as tall as the list of steps.
const LOG_HEIGHT: i32 = 26 * space::SM;

fn steps(app: &InstallerApp) -> Node<Msg> {
    let rows = app
        .wizard
        .steps()
        .into_iter()
        .map(|(title, state)| {
            let (icon, tone) = match state {
                StepState::Pending => (Icon::Circle, Tone::Disabled),
                StepState::Running => (Icon::Dot, Tone::Accent),
                StepState::Done => (Icon::CircleCheck, Tone::Success),
                StepState::Failed => (Icon::CircleX, Tone::Danger),
            };
            let text_tone = match state {
                StepState::Pending => Tone::Secondary,
                _ => Tone::Normal,
            };
            row(
                space::SM,
                vec![
                    Node::Icon {
                        icon,
                        role: TextRole::Body,
                        tone,
                    },
                    Text::new(title).tone(text_tone).into(),
                ],
            )
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
        .map(|line| Text::new(line.clone()).role(TextRole::Mono).into())
        .collect();
    Node::SizedBox {
        width: None,
        height: Some(Length::Px(LOG_HEIGHT)),
        child: Box::new(Node::ScrollView {
            child: Box::new(column(0, lines)),
            scroll_y: app.log_scroll,
            on_scroll: Some(Msg::LogScroll),
            follow_end: true,
        }),
    }
}

fn details(app: &InstallerApp, mut before: Vec<Node<Msg>>) -> Node<Msg> {
    let title = match app.details {
        true => say::HIDE_DETAILS,
        false => say::SHOW_DETAILS,
    };
    before.push(log(app));
    Disclosure::new(title, column(space::SM, before))
        .expanded(app.details)
        .on_toggle(Msg::Details)
        .into()
}

fn install(app: &InstallerApp) -> Vec<Node<Msg>> {
    let progress = app.wizard.progress();
    let bar = match progress.bytes {
        Some((done, total)) => Node::ProgressBar {
            value: Some((done.saturating_mul(100) / total.max(1)) as u32),
            label: format!("{} of {} copied", disk_size(done), disk_size(total)),
            tone: Tone::Accent,
        },
        None => Node::ProgressBar {
            value: None,
            label: String::new(),
            tone: Tone::Accent,
        },
    };
    vec![
        Section::new(say::STEPS, steps(app)).into(),
        bar,
        details(app, Vec::new()),
    ]
}

fn restart_failed(app: &InstallerApp) -> Option<Node<Msg>> {
    app.restart_failed
        .then(|| Notice::new(Tone::Danger, say::NOT_RESTARTED).into())
}

fn done(app: &InstallerApp) -> Vec<Node<Msg>> {
    let mut page: Vec<Node<Msg>> = notes(app).collect();
    page.extend(restart_failed(app));
    let where_to = app
        .wizard
        .done_lines()
        .into_iter()
        .fold(PropertyList::new(), |list, (name, value)| {
            list.row(name, value)
        });
    page.push(details(app, vec![where_to.into()]));
    page
}

fn error(app: &InstallerApp) -> Vec<Node<Msg>> {
    let mut page: Vec<Node<Msg>> = notes(app).collect();
    page.extend(restart_failed(app));
    page.extend(busy(app));
    let why = match app.wizard.failure() {
        Some(Failure::Stopped { why, .. }) => Some(
            Text::new(why.clone())
                .role(TextRole::Mono)
                .wrap_anywhere()
                .into(),
        ),
        _ => None,
    };
    page.push(details(app, why.into_iter().collect()));
    page
}

fn footer(app: &InstallerApp) -> Option<ButtonRow<Msg>> {
    let w = &app.wizard;
    let next = Button::new(w.primary_label())
        .on_press(Msg::Next)
        .enabled(w.can_advance() && !app.restarting);
    let back = || {
        Button::new(say::BACK)
            .on_press(Msg::Back)
            .enabled(w.can_go_back())
    };
    let close = |label| Button::new(label).on_press(Msg::Close);
    Some(match app.page {
        Page::Welcome => ButtonRow::new()
            .aside(close(if app.welcome { say::TRY } else { say::CLOSE }))
            .confirm(next),
        Page::Disk => ButtonRow::new()
            .back(back())
            .aside(
                Button::new(say::SCAN)
                    .on_press(Msg::Rescan)
                    .enabled(!w.waiting()),
            )
            .confirm(next),
        Page::How => ButtonRow::new().back(back()).confirm(next),
        Page::Review => ButtonRow::new().back(back()).destructive(next),
        Page::Install => return None,
        Page::Done => ButtonRow::new()
            .aside(close(if app.welcome {
                say::KEEP_TRYING
            } else {
                say::CLOSE
            }))
            .destructive(next),
        Page::Error => ButtonRow::new()
            .aside(
                Button::new(say::RESTART)
                    .on_press(Msg::Restart)
                    .enabled(!app.restarting),
            )
            .confirm(next),
    })
}

pub(super) fn window(app: &InstallerApp) -> Node<Msg> {
    let heading = app.wizard.heading(app.welcome);
    let mut content = match app.page {
        Page::Welcome => welcome(app),
        Page::Disk => disk(app),
        Page::How => how(app),
        Page::Review => review(app),
        Page::Install => install(app),
        Page::Done => done(app),
        Page::Error => error(app),
    };
    if let Some(disk) = heading.disk {
        content.insert(0, caption(disk));
    }
    let page = slopos_appkit::Page::new(heading.title)
        .subtitle(heading.subtitle)
        .content(column(space::XL, content));
    match footer(app) {
        Some(footer) => page.footer(footer).into(),
        None => page.into(),
    }
}
