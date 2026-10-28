use slopos_theme_core::{Icon, TextRole, Tone, space};

use super::FixedSizeWidget;
use crate::audit::{Arrangement, Rule, audit, audit_node};
use crate::catalogue::{Catalogue, Msg, PAGES};
use crate::components::{
    Button, ButtonRow, Dialog, Disclosure, List, ListRow, PropertyList, Tag, Text, WindowSpec,
};
use crate::constraints::{BoxConstraints, CrossAxisAlignment, Rect, Size};
use crate::event::{
    EventResponse, Key, MessageSink, Modifiers, NamedKey, PointerButton, WidgetEvent,
};
use crate::layout::HStackWidget;
use crate::node::{App, Node};
use crate::paint::PaintContext;
use crate::style::StyleSheet;
use crate::traits::{
    FocusPolicy, Kept, KeptAt, MeasureCtx, Widget, WidgetCore, measure_widget, place_widget,
};
use crate::tree::build_widget_tree;
use crate::widgets::line_edit::LineEditInput;

const WIDTHS: [i32; 3] = [360, 640, 960];

fn style() -> StyleSheet {
    StyleSheet::current()
}

/// Laid out and painted, as a window's first frame is, which tells each
/// widget what it cut short.
fn built<M: Clone + 'static>(node: impl Into<Node<M>>, size: Size) -> Box<dyn Widget> {
    let mut root = build_widget_tree(&node.into());
    crate::tree::layout_tree(root.as_mut(), size, &style());
    crate::audit::paint_recording(root.as_ref(), size, &style());
    root
}

fn rules(findings: &[crate::audit::Finding]) -> Vec<Rule> {
    findings.iter().map(|f| f.rule).collect()
}

fn assert_clean(what: &str, found: Vec<String>) {
    assert!(found.is_empty(), "{what}:\n  {}", found.join("\n  "));
}

/// A widget that draws `text` at its rect's top-left in `ink`, however wide
/// its rect, and may take focus without showing it.
struct Scribble {
    core: WidgetCore,
    text: &'static str,
    ink: fn(&slopos_theme_core::Theme) -> slopos_abi::draw::Color32,
    size: Size,
    focus: FocusPolicy,
}

impl Scribble {
    fn new(text: &'static str, width: i32) -> Self {
        Self {
            core: WidgetCore::new(),
            text,
            ink: |t| t.text,
            size: Size::new(width, 20),
            focus: FocusPolicy::None,
        }
    }
}

impl Widget for Scribble {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }
    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        constraints.constrain(self.size)
    }
    fn paint(&self, ctx: &mut PaintContext) {
        let r = self.layout_rect();
        let ink = (self.ink)(ctx.theme());
        ctx.draw_text(r.x, r.y, self.text, TextRole::Body, ink);
    }
    fn event(&mut self, _: &WidgetEvent, _: &mut MessageSink) -> EventResponse {
        EventResponse::Ignored
    }
    fn focus_policy(&self) -> FocusPolicy {
        self.focus
    }
}

/// A container that lays its children out at the rects it was given, which
/// may cross each other or its own edge.
struct Placed {
    core: WidgetCore,
    children: Vec<Box<dyn Widget>>,
    rects: Vec<Rect>,
    /// Paints each child clipped to its own rect.
    clip_each: bool,
}

impl Widget for Placed {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        for child in &mut self.children {
            measure_widget(child.as_mut(), BoxConstraints::UNBOUNDED, ctx);
        }
        constraints.constrain(Size::new(100, 100))
    }
    fn layout(&mut self, rect: Rect) {
        for (child, r) in self.children.iter_mut().zip(&self.rects) {
            place_widget(
                child.as_mut(),
                Rect::new(rect.x + r.x, rect.y + r.y, r.width, r.height),
            );
        }
    }
    fn paint(&self, ctx: &mut PaintContext) {
        for child in &self.children {
            match self.clip_each {
                true => ctx.with_clip(child.layout_rect(), |ctx| child.paint(ctx)),
                false => child.paint(ctx),
            }
        }
    }
    fn event(&mut self, _: &WidgetEvent, _: &mut MessageSink) -> EventResponse {
        EventResponse::Ignored
    }
    fn arrangement(&self) -> Arrangement {
        Arrangement::Flow
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        &self.children
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.children
    }
}

fn placed(children: Vec<Box<dyn Widget>>, rects: Vec<Rect>) -> Placed {
    Placed {
        core: WidgetCore::new(),
        children,
        rects,
        clip_each: false,
    }
}

pub(super) fn test_audit_finds_text_past_its_widget() {
    let mut root = placed(
        vec![Box::new(Scribble::new("A sentence wider than its box", 40))],
        vec![Rect::new(0, 0, 40, 20)],
    );
    let found = audit(&mut root, Size::new(400, 100), &style());
    assert!(rules(&found).contains(&Rule::Clipped), "{found:?}");

    let mut fitted = built::<()>(
        Node::SizedBox {
            width: Some(crate::constraints::Length::Px(40)),
            height: None,
            child: Box::new(Text::new("A sentence wider than its box").into()),
        },
        Size::new(400, 100),
    );
    let found = audit(fitted.as_mut(), Size::new(400, 100), &style());
    assert!(found.is_empty(), "an ellipsis is no clipping: {found:?}");

    let mut low = placed(
        vec![Box::new(Scribble::new("Too tall", 200))],
        vec![Rect::new(0, 0, 200, 8)],
    );
    let found = audit(&mut low, Size::new(400, 100), &style());
    assert!(rules(&found).contains(&Rule::Clipped), "{found:?}");

    let mut cut = placed(
        vec![Box::new(Scribble::new("Cut at the foot", 200))],
        vec![Rect::new(0, 0, 200, 8)],
    );
    cut.clip_each = true;
    let found = audit(&mut cut, Size::new(400, 100), &style());
    assert!(rules(&found).contains(&Rule::Clipped), "{found:?}");

    let mut over = placed(
        vec![
            Box::new(Scribble::new("Above", 200)),
            Box::new(Scribble::new("Below", 200)),
        ],
        vec![Rect::new(0, 0, 200, 8), Rect::new(0, 8, 200, 20)],
    );
    let found = audit(&mut over, Size::new(400, 100), &style());
    assert!(rules(&found).contains(&Rule::Overlap), "{found:?}");

    let field: Node<()> = Node::SizedBox {
        width: Some(crate::constraints::Length::Px(160)),
        height: None,
        child: Box::new(line_edit(
            "/usr/local/share/slopos/a/path/longer/than/its/field",
        )),
    };
    assert_clean("a field scrolls its text", audit_node(&field, &WIDTHS, 100));
}

pub(super) fn test_audit_finds_overlap_and_escape() {
    let mut root = placed(
        vec![
            Box::new(FixedSizeWidget::new(10, 10)),
            Box::new(FixedSizeWidget::new(10, 10)),
        ],
        vec![Rect::new(0, 0, 40, 20), Rect::new(30, 10, 40, 20)],
    );
    let found = audit(&mut root, Size::new(400, 100), &style());
    assert!(rules(&found).contains(&Rule::Overlap), "{found:?}");

    let mut root = placed(
        vec![Box::new(FixedSizeWidget::new(10, 10))],
        vec![Rect::new(80, 0, 40, 20)],
    );
    let found = audit(&mut root, Size::new(100, 100), &style());
    assert!(rules(&found).contains(&Rule::Outside), "{found:?}");
}

pub(super) fn test_audit_finds_a_row_off_its_baseline() {
    let row = |align| {
        HStackWidget::new(
            vec![
                build_widget_tree::<()>(&Text::new("Title").role(TextRole::Title).into()),
                build_widget_tree::<()>(&Text::new("caption").role(TextRole::Caption).into()),
            ],
            8,
            align,
        )
    };
    let mut centred = row(CrossAxisAlignment::Center);
    let found = audit(&mut centred, Size::new(400, 100), &style());
    assert!(rules(&found).contains(&Rule::Baseline), "{found:?}");

    let mut by_baseline = row(CrossAxisAlignment::Baseline);
    let found = audit(&mut by_baseline, Size::new(400, 100), &style());
    assert!(found.is_empty(), "{found:?}");
}

pub(super) fn test_audit_finds_focus_that_does_not_show() {
    let mut quiet = Scribble::new("x", 20);
    quiet.focus = FocusPolicy::StrongFocus;
    let mut root = placed(vec![Box::new(quiet)], vec![Rect::new(0, 0, 20, 20)]);
    let found = audit(&mut root, Size::new(100, 100), &style());
    assert!(rules(&found).contains(&Rule::Focus), "{found:?}");
    assert!(rules(&found).contains(&Rule::Target), "{found:?}");

    let button = build_widget_tree::<()>(&Button::new("Cut short").on_press(()).into());
    let mut cut = placed(vec![button], vec![Rect::new(20, 20, 100, 28)]);
    cut.clip_each = true;
    let found = audit(&mut cut, Size::new(200, 100), &style());
    assert!(
        rules(&found).contains(&Rule::Focus),
        "a ring cut to its control: {found:?}"
    );

    let button: Node<()> = Button::new("Shows its focus").on_press(()).into();
    assert_clean("a button", audit_node(&button, &WIDTHS, 100));
}

pub(super) fn test_audit_finds_text_that_does_not_read() {
    let mut faint = Scribble::new("faint", 80);
    faint.ink = |t| t.border;
    let mut root = placed(vec![Box::new(faint)], vec![Rect::new(0, 0, 80, 20)]);
    let found = audit(&mut root, Size::new(100, 100), &style());
    assert!(rules(&found).contains(&Rule::Contrast), "{found:?}");

    let disabled: Node<()> = Text::new("Disabled").tone(Tone::Disabled).into();
    assert_clean("disabled text", audit_node(&disabled, &WIDTHS, 100));
}

pub(super) fn test_every_catalogue_page_passes_the_audit() {
    let catalogue = Catalogue::new(0);
    for (i, name) in PAGES.iter().enumerate() {
        assert_clean(name, audit_node(&catalogue.page(i), &WIDTHS, 720));
    }
}

pub(super) fn test_the_catalogue_window_passes_the_audit() {
    for page in 0..PAGES.len() {
        let catalogue = Catalogue::new(page);
        let spec = catalogue.window();
        for size in [spec.size, spec.min_size] {
            let mut root = build_widget_tree(&catalogue.view());
            let found = audit(root.as_mut(), size, &style());
            let found: Vec<String> = found.iter().map(|f| f.to_string()).collect();
            assert_clean(&format!("{} at {size:?}", PAGES[page]), found);
        }
    }
}

pub(super) fn test_the_catalogue_filter_narrows_its_pages() {
    let mut catalogue = Catalogue::new(0);
    let type_in = |catalogue: &mut Catalogue, text: &str| {
        for character in text.chars() {
            catalogue.update(Msg::Filter(LineEditInput::Text { character }));
        }
    };
    let page = |name| PAGES.iter().position(|p| *p == name);
    type_in(&mut catalogue, "DIA");
    catalogue.update(Msg::Show(0));
    assert_eq!(Some(catalogue.shown_page()), page("dialog"));
    catalogue.update(Msg::Step(false));
    assert_eq!(Some(catalogue.shown_page()), page("icons"));
    type_in(&mut catalogue, "zz");
    catalogue.update(Msg::Show(0));
    assert_eq!(Some(catalogue.shown_page()), page("icons"));
    let mut root = build_widget_tree(&catalogue.view());
    let found = audit(root.as_mut(), catalogue.window().min_size, &style());
    let found: Vec<String> = found.iter().map(|f| f.to_string()).collect();
    assert_clean("no page matches", found);
}

pub(super) fn test_text_keeps_to_its_box() {
    let shown = |node: Node<()>, size: Size| {
        let column = Node::VStack {
            children: vec![node],
            spacing: 0,
            align: CrossAxisAlignment::Start,
        };
        let root = built(column, size);
        let runs = crate::audit::paint_recording(root.as_ref(), size, &style());
        let drawn = runs
            .into_iter()
            .filter(|r| r.mark != crate::audit::Mark::Dropped);
        drawn.map(|r| r.text).collect::<Vec<_>>().join("")
    };
    let squeezed = Node::SizedBox {
        width: Some(crate::constraints::Length::Px(120)),
        height: Some(crate::constraints::Length::Px(30)),
        child: Box::new(Text::new("one two three four five six seven").wrap().into()),
    };
    assert_clean("squeezed", audit_node(&squeezed, &WIDTHS, 100));
    assert!(shown(squeezed, Size::new(400, 100)).ends_with('…'));

    let lines: Node<()> = Text::new("first\nsecond\nthird").max_lines(2).into();
    assert_eq!(shown(lines, Size::new(400, 100)), "firstsecond…");

    let wide_first = Node::SizedBox {
        width: Some(crate::constraints::Length::Px(120)),
        height: None,
        child: Box::new(Text::new("a first line far too wide to fit\nshort").into()),
    };
    assert!(shown(wide_first, Size::new(400, 100)).ends_with("…short"));

    let narrow = || Node::SizedBox {
        width: Some(crate::constraints::Length::Px(6)),
        height: None,
        child: Box::new(Text::new("Nothing of this fits").into()),
    };
    let held = Node::VStack {
        children: vec![narrow()],
        spacing: 0,
        align: CrossAxisAlignment::Start,
    };
    let found = audit_node(&held, &WIDTHS, 100);
    assert!(
        found.iter().all(|f| f.contains("no room")) && !found.is_empty(),
        "{found:?}"
    );
    assert_eq!(shown(narrow(), Size::new(400, 100)), "");
}

pub(super) fn test_list_leaves_enter_to_the_page() {
    let rows = || vec![ListRow::new("One"), ListRow::new("Two")];
    let enter = key_down(NamedKey::Enter);
    let mut sink = MessageSink::new();
    let mut chooses =
        build_widget_tree(&List::new(rows()).selected(Some(1)).on_select(|i| i).into());
    chooses.event(&WidgetEvent::FocusGained, &mut sink);
    let resp = chooses.event(&enter, &mut sink);
    assert_eq!(resp, EventResponse::Ignored);

    let list = List::new(rows())
        .selected(Some(1))
        .on_select(|i| i)
        .on_activate(|i| i + 10);
    let mut opens = build_widget_tree(&list.into());
    opens.event(&WidgetEvent::FocusGained, &mut sink);
    let resp = opens.event(&enter, &mut sink);
    assert_eq!(resp, EventResponse::Consumed);
    assert_eq!(sink.drain_typed::<usize>(), vec![11]);

    let list = List::new(rows()).on_activate(|i| i + 10);
    let mut only_opens = built(list, Size::new(300, 200));
    only_opens.event(&WidgetEvent::FocusGained, &mut sink);
    let down = key_down(NamedKey::Down);
    only_opens.event(&down, &mut sink);
    only_opens.event(&enter, &mut sink);
    assert_eq!(
        sink.drain_typed::<usize>(),
        vec![11],
        "the arrows chose the second"
    );
    let first = only_opens.layout_rect();
    let press = WidgetEvent::PointerDown {
        x: first.x + 10,
        y: first.y + 10,
        button: PointerButton::Left,
        modifiers: Modifiers::default(),
    };
    only_opens.event(&press, &mut sink);
    assert_eq!(sink.drain_typed::<usize>(), vec![10], "a click opens");
}

fn dialog_over(page: Node<()>) -> Node<()> {
    Node::ZStack {
        children: vec![
            page,
            Dialog::new("Erase this disk?")
                .content(Text::new("Everything on it goes."))
                .footer(
                    ButtonRow::new()
                        .cancel(Button::new("Cancel").on_press(()))
                        .destructive(Button::new("Erase").on_press(())),
                )
                .into(),
        ],
    }
}

pub(super) fn test_arrows_move_along_a_button_row() {
    let root = built(dialog_over(Node::Empty), Size::new(640, 480));
    let chain = crate::focus::FocusManager::tab_chain_of(root.as_ref());
    let [cancel, erase] = chain[..] else {
        panic!("the dialog's two buttons: {chain:?}");
    };
    let beside = |id, forward| crate::focus::beside_in_toolbar(root.as_ref(), id, forward);
    assert_eq!(beside(cancel, true), Some(erase));
    assert_eq!(beside(erase, true), Some(cancel));
    assert_eq!(beside(cancel, false), Some(erase));
}

pub(super) fn test_no_tooltip_through_a_scrim() {
    let tools: Node<()> = crate::components::Toolbar::new()
        .start(crate::components::IconButton::new(
            Icon::Trash,
            "Move to trash",
        ))
        .into();
    let root = built(dialog_over(tools), Size::new(640, 480));
    fn named(w: &dyn Widget) -> Option<Rect> {
        if w.accessible_name() == Some("Move to trash") {
            return Some(w.layout_rect());
        }
        w.children().iter().find_map(|c| named(c.as_ref()))
    }
    let r = named(root.as_ref()).expect("the toolbar holds the button");
    let (x, y) = (r.x + r.width / 2, r.y + r.height / 2);
    assert_eq!(crate::tooltip::find(root.as_ref(), x, y), None);
}

pub(super) fn test_a_tall_dialog_scrolls() {
    let long = "A sentence that goes on. ".repeat(80);
    let dialog: Node<()> = Dialog::new("Read this first")
        .content(Text::new(long).wrap())
        .footer(ButtonRow::new().confirm(Button::new("Close").on_press(())))
        .into();
    assert_clean("a tall dialog", audit_node(&dialog, &WIDTHS, 400));
    let root = built(dialog, Size::new(640, 400));
    let chain = crate::focus::FocusManager::tab_chain_of(root.as_ref());
    assert_eq!(chain.len(), 2, "the content scrolls, then Close: {chain:?}");
}

pub(super) fn test_a_dialog_takes_focus_from_the_page() {
    let page = || -> Node<()> {
        ButtonRow::new()
            .back(Button::new("Back").on_press(()))
            .confirm(Button::new("Erase disk").on_press(()))
            .into()
    };
    let size = Size::new(640, 480);
    let mut focus = crate::focus::FocusManager::new();
    let before = built(page(), size);
    focus.rebuild_tab_chain(before.as_ref());
    let chain = crate::focus::FocusManager::tab_chain_of(before.as_ref());
    focus.set_focused(Some(chain[1]));
    let after = built(dialog_over(page()), size);
    assert_eq!(focus.rebuild_tab_chain(after.as_ref()), None);
    assert_eq!(focus.focused(), None);

    let closed = built(page(), size);
    let chain = crate::focus::FocusManager::tab_chain_of(closed.as_ref());
    assert_eq!(
        focus.rebuild_tab_chain(closed.as_ref()),
        Some(chain[1]),
        "back where it was"
    );

    focus.set_focused(Some(chain[1]));
    focus.rebuild_tab_chain(built(dialog_over(page()), size).as_ref());
    focus.leave_page();
    let another = built(page(), size);
    assert_eq!(
        focus.rebuild_tab_chain(another.as_ref()),
        None,
        "not on another page"
    );
}

pub(super) fn test_a_rebuild_keeps_what_was_scrolled() {
    let page = || -> Node<()> {
        let lines = (0..60).map(|i| Text::new(format!("Line {i}")).into());
        crate::components::Page::new("Long")
            .content(Node::VStack {
                children: lines.collect(),
                spacing: 0,
                align: CrossAxisAlignment::Stretch,
            })
            .into()
    };
    let table = |rows: usize, shows: u64| -> Node<()> {
        Node::Table {
            columns: vec![crate::node::TableColumn {
                label: String::from("Name"),
                width: crate::node::TableColumnWidth::Flex(1),
                sort_indicator: None,
            }],
            rows: (0..rows)
                .map(|i| vec![Text::new(format!("{i}")).into()])
                .collect(),
            selected: None,
            on_select: None,
            on_activate: None,
            on_header_click: None,
            on_context_menu: None,
            shows,
        }
    };
    let size = Size::new(400, 300);
    let wheel = WidgetEvent::Scroll {
        x: 200,
        y: 150,
        delta_x: 0,
        delta_y: 120,
    };
    let rebuilt = |before: Node<()>, after: Node<()>| {
        let mut before = built(before, size);
        before.event(&wheel, &mut MessageSink::new());
        let mut kept = Vec::new();
        crate::tree::kept(before.as_ref(), &mut kept);
        assert!(kept.iter().any(|k| scrolled_down(k) > 0), "{kept:?}");
        let mut after = build_widget_tree(&after);
        crate::tree::restore_kept(after.as_mut(), &kept);
        crate::tree::layout_tree(after.as_mut(), size, &style());
        let mut now = Vec::new();
        crate::tree::kept(after.as_ref(), &mut now);
        (kept[0].at, now[0].at)
    };
    let (was, is) = rebuilt(page(), page());
    assert_eq!(is, was);
    let (was, is) = rebuilt(page(), dialog_over(page()));
    assert_eq!(is, was, "a dialog over the page moves nothing beneath it");
    let (was, is) = rebuilt(table(100, 1), table(99, 1));
    assert_eq!(is, was, "a row more or less moves nothing");
    let (_, is) = rebuilt(table(100, 1), table(100, 2));
    assert_eq!(is, KeptAt::Scroll(0), "another table starts at the top");
}

fn scrolled_down(kept: &Kept) -> i32 {
    match kept.at {
        KeptAt::Scroll(y) => y,
        KeptAt::Cursor(_) => 0,
    }
}

pub(super) fn test_the_wheel_moves_the_innermost_view() {
    let table = Node::Table {
        columns: vec![crate::node::TableColumn {
            label: String::from("Name"),
            width: crate::node::TableColumnWidth::Flex(1),
            sort_indicator: None,
        }],
        rows: (0..100)
            .map(|i| vec![Text::new(format!("{i}")).into()])
            .collect(),
        selected: None,
        on_select: None,
        on_activate: None,
        on_header_click: None,
        on_context_menu: None,
        shows: 0,
    };
    let page: Node<()> = crate::components::Page::new("Processes")
        .content(Node::SizedBox {
            width: None,
            height: Some(crate::constraints::Length::Px(400)),
            child: Box::new(table),
        })
        .into();
    let size = Size::new(400, 300);
    let mut root = built(page, size);
    fn table_at(w: &dyn Widget) -> Option<Rect> {
        if w.role() == crate::traits::Role::List {
            return Some(w.layout_rect());
        }
        w.children().iter().find_map(|c| table_at(c.as_ref()))
    }
    let r = table_at(root.as_ref()).expect("the page holds the table");
    let wheel = WidgetEvent::Scroll {
        x: r.x + r.width / 2,
        y: r.y + r.height / 2,
        delta_x: 0,
        delta_y: 120,
    };
    root.event(&wheel, &mut MessageSink::new());
    let mut kept = Vec::new();
    crate::tree::kept(root.as_ref(), &mut kept);
    let offsets: Vec<i32> = kept.iter().map(scrolled_down).collect();
    assert_eq!(offsets, vec![0, 120], "the page stays, the table moves");
}

pub(super) fn test_long_choices_end_in_an_ellipsis() {
    let long = "Erase the whole disk, every partition on it, and install SlopOS";
    let choices: Node<()> = Node::VStack {
        children: vec![
            Node::Checkbox {
                checked: false,
                label: String::from(long),
                on_toggle: Some(()),
                enabled: true,
            },
            List::new(vec![ListRow::new(long).detail(long)])
                .selected(Some(0))
                .on_select(|_| ())
                .into(),
        ],
        spacing: 0,
        align: CrossAxisAlignment::Stretch,
    };
    assert_clean("long choices", audit_node(&choices, &[360], 200));
    let root = built(choices, Size::new(360, 200));
    for row in root.children() {
        let r = row.layout_rect();
        let at = (r.x + r.width / 2, r.y + 10);
        assert_eq!(
            crate::tooltip::find(row.as_ref(), at.0, at.1).as_deref(),
            Some(long)
        );
    }
}

pub(super) fn test_a_page_shows_the_chosen_row() {
    let rows = (0..40).map(|i| ListRow::new(format!("Row {i}"))).collect();
    let list: Node<usize> = List::new(rows).selected(Some(30)).on_select(|i| i).into();
    let detailed = (0..20)
        .map(|i| ListRow::new(format!("Option {i}")).detail("With a line of detail under it."))
        .collect();
    let options = List::new(detailed)
        .selected(Some(15))
        .on_select(|i| i)
        .into();
    for content in [list, options] {
        let page: Node<usize> = crate::components::Page::new("Choices")
            .content(content)
            .into();
        let size = Size::new(400, 300);
        let mut root = built(page, size);
        let chain = crate::focus::FocusManager::tab_chain_of(root.as_ref());
        let choice = *chain.last().expect("the choice is a tab stop");
        let shown = crate::focus::reveal(root.as_mut(), choice, &mut MessageSink::new());
        let row = shown.expect("the choice is in the tree");
        assert!(row.y >= 0 && row.y + row.height <= size.height, "{row:?}");
    }
}

pub(super) fn test_a_dialog_shows_its_controls_whole() {
    let content = Node::VStack {
        children: vec![
            Node::Checkbox {
                checked: true,
                label: String::from("Keep my files"),
                on_toggle: Some(()),
                enabled: true,
            },
            line_edit("A name"),
            Button::new("Choose a disk…").on_press(()).into(),
        ],
        spacing: slopos_theme_core::space::SM,
        align: CrossAxisAlignment::Stretch,
    };
    let lines = (0..20).map(|i| Text::new(format!("Line {i} of the page beneath")).into());
    let page = Node::Padding {
        padding: slopos_theme_core::EdgeInsets::new(0, 0, 0, 150),
        child: Box::new(Node::VStack {
            children: lines.collect(),
            spacing: 0,
            align: CrossAxisAlignment::Start,
        }),
    };
    let dialog = Node::ZStack {
        children: vec![
            page,
            Dialog::new("Install")
                .content(content)
                .footer(ButtonRow::new().confirm(Button::new("Close").on_press(())))
                .into(),
        ],
    };
    assert_clean("a dialog's controls", audit_node(&dialog, &WIDTHS, 600));
}

pub(super) fn test_a_table_names_a_cut_cell() {
    let name = "a-very-long-name-that-does-not-fit-its-column";
    let table: Node<()> = Node::Table {
        columns: vec![crate::node::TableColumn {
            label: String::from("Name"),
            width: crate::node::TableColumnWidth::Fixed(90),
            sort_indicator: None,
        }],
        rows: vec![vec![Text::new(name).into()]],
        selected: None,
        on_select: None,
        on_activate: None,
        on_header_click: None,
        on_context_menu: None,
        shows: 0,
    };
    let root = built(table, Size::new(300, 200));
    let r = root.layout_rect();
    let found = crate::tooltip::find(root.as_ref(), r.x + 20, r.y + 28 + 14);
    assert_eq!(found.as_deref(), Some(name));
}

fn key_down(named: NamedKey) -> WidgetEvent {
    WidgetEvent::KeyDown {
        key: Key::Named(named),
        modifiers: Modifiers::default(),
    }
}

pub(super) fn test_button_row_answers_enter_and_escape() {
    let enter = Key::Named(NamedKey::Enter);
    let escape = Key::Named(NamedKey::Escape);
    let none = Modifiers::default();
    let row = ButtonRow::new()
        .back(Button::new("Back").on_press("back"))
        .confirm(Button::new("Continue").on_press("go"));
    let mut sink = MessageSink::new();
    let mut inside = crate::tree::build_button_row(&row, false);
    assert!(!inside.take_unhandled_key(enter, none, &mut sink));
    let page: Node<&str> = crate::components::Page::new("A page")
        .content(Node::from(row.clone()))
        .footer(row)
        .into();
    let mut page = build_widget_tree(&page);
    fn ends(w: &mut dyn Widget, key: Key, sink: &mut MessageSink) -> usize {
        let here = usize::from(w.take_unhandled_key(key, Modifiers::default(), sink));
        here + w
            .children_mut()
            .iter_mut()
            .map(|c| ends(c.as_mut(), key, sink))
            .sum::<usize>()
    }
    assert_eq!(ends(page.as_mut(), enter, &mut sink), 1, "only the footer");
    assert!(ends(page.as_mut(), escape, &mut sink) == 1);
    assert_eq!(sink.drain_typed::<&str>(), vec!["go", "back"]);

    let row = ButtonRow::new()
        .cancel(Button::new("Cancel").on_press("cancel").enabled(false))
        .destructive(Button::new("Erase").on_press("erase"));
    let mut widget = crate::tree::build_button_row(&row, true);
    assert!(!widget.take_unhandled_key(enter, none, &mut sink));
    assert!(
        widget.take_unhandled_key(escape, none, &mut sink),
        "a way back that is disabled still answers Escape, by doing nothing"
    );
    assert!(sink.drain_typed::<&str>().is_empty());
}

pub(super) fn test_button_row_stacks_when_narrow() {
    let row: Node<()> = ButtonRow::new()
        .back(Button::new("Back to the start"))
        .cancel(Button::new("Cancel"))
        .confirm(Button::new("Install SlopOS"))
        .into();
    let widget = built(row, Size::new(260, 200));
    let buttons = widget.children();
    let (back, install) = (buttons[0].layout_rect(), buttons[2].layout_rect());
    assert!(back.y < install.y, "{back:?} {install:?}");
    assert_clean(
        "a narrow row",
        audit_node(&wide_row(), &[200, 260, 640], 200),
    );
}

fn wide_row() -> Node<()> {
    ButtonRow::new()
        .back(Button::new("Back to the start"))
        .cancel(Button::new("Cancel"))
        .confirm(Button::new("Install SlopOS"))
        .into()
}

fn click(x: i32, y: i32) -> WidgetEvent {
    WidgetEvent::PointerDown {
        x,
        y,
        button: PointerButton::Left,
        modifiers: Modifiers::default(),
    }
}

pub(super) fn test_list_skips_what_cannot_be_chosen() {
    let list: Node<usize> = List::new(vec![
        ListRow::new("One"),
        ListRow::new("Two").disabled("Too small."),
        ListRow::new("Three").icon(Icon::HardDrive).trailing("1 TB"),
    ])
    .selected(Some(0))
    .on_select(|i| i)
    .into();
    let mut widget = built(list, Size::new(300, 400));
    let mut sink = MessageSink::new();
    widget.event(&WidgetEvent::FocusGained, &mut sink);
    widget.event(&key_down(NamedKey::Down), &mut sink);
    assert_eq!(sink.drain_typed::<usize>(), vec![2]);

    let rect = widget.layout_rect();
    let second = rect.y + 60;
    widget.event(&click(rect.x + 10, second), &mut sink);
    assert!(sink.drain_typed::<usize>().is_empty());
}

pub(super) fn test_list_rows_pass_the_audit() {
    let list: Node<usize> = List::new(vec![
        ListRow::new("A disk with a long model name that will not fit")
            .icon(Icon::HardDrive)
            .detail("nvme0n1")
            .trailing("1 TB"),
        ListRow::new("Too small").disabled("Too small. SlopOS needs at least 2.5 GB."),
    ])
    .selected(Some(0))
    .on_select(|i| i)
    .into();
    assert_clean("a list", audit_node(&list, &[200, 360, 640], 400));
}

pub(super) fn test_avatars_show_an_initial_and_pass_the_audit() {
    let people: Node<usize> = Node::VStack {
        children: vec![
            Node::Avatar {
                name: String::from("carol"),
            },
            List::new(vec![
                ListRow::new("carol").avatar(),
                ListRow::new("ölund").avatar().detail("Logged in"),
            ])
            .selected(Some(1))
            .on_select(|i| i)
            .into(),
        ],
        spacing: slopos_theme_core::space::SM,
        align: CrossAxisAlignment::Center,
    };
    assert_clean("people", audit_node(&people, &WIDTHS, 400));
    let size = Size::new(200, 200);
    let root = built(people, size);
    let runs = crate::audit::paint_recording(root.as_ref(), size, &style());
    let texts: Vec<&str> = runs.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(texts, ["C", "C", "carol", "Ö", "ölund", "Logged in"]);
}

pub(super) fn test_a_long_placeholder_is_cut_short() {
    let field: Node<LineEditInput> = crate::components::SearchField::new("", 0)
        .placeholder("Every command, file and setting on this computer")
        .into();
    let size = Size::new(160, 60);
    let root = built(field, size);
    let runs = crate::audit::paint_recording(root.as_ref(), size, &style());
    let cut = runs.iter().filter(|r| r.mark == crate::audit::Mark::Elided);
    let shown: String = cut.map(|r| r.text.as_str()).collect();
    assert!(
        shown.starts_with("Every") && shown.ends_with('…'),
        "{shown}"
    );
}

pub(super) fn test_the_pointer_leaving_forgets_the_tooltip() {
    use slopos_protocol::types::{Event as ProtocolEvent, SurfaceId};
    use slopos_windowing::Event;
    let leave = ProtocolEvent::PointerLeave {
        surface: SurfaceId::NONE,
    };
    assert!(matches!(
        Event::from_protocol(&leave),
        Some(Event::PointerLeave)
    ));
    let mut tip = crate::run::Tip::default();
    tip.note(&WidgetEvent::PointerMove { x: 5, y: 5 }, (5, 5), 0);
    assert!(!tip.clear());
    assert!(!tip.ripe(crate::tooltip::DELAY_MS));
}

pub(super) fn test_property_list_lines_names_and_values_up() {
    let list: Node<()> = PropertyList::new()
        .row("Disk", "KINGSTON SNV3S1000G")
        .row(
            "Startup order",
            "After the systems already on this computer",
        )
        .row_with("Root", Tag::new("New", Tone::Success))
        .into();
    let widget = built(list, Size::new(400, 400));
    let cells = widget.children();
    let x = cells[1].layout_rect().x;
    for pair in cells.chunks(2) {
        assert_eq!(pair[1].layout_rect().x, x);
        let base = |w: &dyn Widget| w.layout_rect().y + w.baseline().unwrap();
        assert_eq!(base(pair[0].as_ref()), base(pair[1].as_ref()));
    }
    let list: Node<()> = PropertyList::new()
        .row(
            "Startup order",
            "After the systems already on this computer",
        )
        .into();
    assert_clean("a property list", audit_node(&list, &WIDTHS, 200));
}

pub(super) fn test_text_wraps_or_ends_in_an_ellipsis() {
    let prose: Node<()> = Text::new("one two three four five six").wrap().into();
    let widget = built(prose, Size::new(60, 400));
    assert!(widget.layout_rect().height > TextRole::Body.style().line_height);

    let line: Node<()> = Text::new("one two three four five six").into();
    let widget = built(line, Size::new(60, 400));
    let r = widget.layout_rect();
    assert_eq!(
        widget.tooltip(r.x, r.y),
        Some("one two three four five six")
    );
    for node in [
        Text::new("/usr/local/lib/rustlib/x86_64-unknown-slopos/lib").wrap_anywhere(),
        Text::new("supercalifragilisticexpialidocious").wrap(),
        Text::new("cut short at two lines however long it runs on and on")
            .wrap()
            .max_lines(2),
    ] {
        assert_clean(
            "text",
            audit_node(&Node::<()>::from(node), &[40, 120, 300], 400),
        );
    }
}

pub(super) fn test_disclosure_opens_and_closes() {
    let closed: Node<&str> = Disclosure::new("Details", Text::new("inside"))
        .on_toggle("toggle")
        .into();
    let mut widget = built(closed, Size::new(300, 200));
    assert!(widget.children().is_empty());
    let r = widget.layout_rect();
    let mut sink = MessageSink::new();
    widget.event(&click(r.x + 4, r.y + 4), &mut sink);
    let up = WidgetEvent::PointerUp {
        x: r.x + 4,
        y: r.y + 4,
        button: PointerButton::Left,
    };
    widget.event(&up, &mut sink);
    assert_eq!(sink.drain_typed::<&str>(), vec!["toggle"]);

    let open: Node<&str> = Disclosure::new("Details", Text::new("inside"))
        .expanded(true)
        .into();
    assert_eq!(built(open, Size::new(300, 200)).children().len(), 1);
}

pub(super) fn test_tooltip_names_an_icon_button() {
    let tools: Node<()> = crate::components::Toolbar::new()
        .start(crate::components::IconButton::new(
            Icon::Trash,
            "Move to trash",
        ))
        .into();
    let widget = built(tools, Size::new(300, 100));
    let at = |w: &dyn Widget| {
        let r = w.layout_rect();
        (r.x + r.width / 2, r.y + r.height / 2)
    };
    fn first_button(w: &dyn Widget) -> Option<&dyn Widget> {
        if w.accessible_name() == Some("Move to trash") {
            return Some(w);
        }
        w.children().iter().find_map(|c| first_button(c.as_ref()))
    }
    let button = first_button(widget.as_ref()).expect("the toolbar holds the button");
    let (x, y) = at(button);
    assert_eq!(
        crate::tooltip::find(widget.as_ref(), x, y).as_deref(),
        Some("Move to trash")
    );
}

pub(super) fn test_a_drag_follows_the_pointer_out_of_its_part() {
    use crate::widgets::slider::{SliderGrab, SliderInput};
    fn slider() -> Node<SliderInput> {
        Node::Slider {
            value: 50,
            min: 0,
            max: 100,
            step: 1,
            enabled: true,
            grab: Some(SliderGrab(0)),
            on_input: Some(|input| input),
        }
    }
    fn slider_at(w: &dyn Widget) -> Option<Rect> {
        if w.role() == crate::traits::Role::Slider {
            return Some(w.layout_rect());
        }
        w.children().iter().find_map(|c| slider_at(c.as_ref()))
    }
    let in_dialog: Node<SliderInput> = Dialog::new("Size")
        .content(slider())
        .footer(ButtonRow::new().confirm(Button::new("Done").on_press(SliderInput::End)))
        .into();
    let in_list = PropertyList::new().row_with("Root size", slider()).into();
    for (holder, node) in [("dialog", in_dialog), ("property list", in_list)] {
        let mut root = built(node, Size::new(800, 400));
        let r = slider_at(root.as_ref()).expect("a slider");
        let past = WidgetEvent::PointerMove {
            x: r.x - 60,
            y: r.y + r.height / 2,
        };
        let mut sink = MessageSink::new();
        root.event(&past, &mut sink);
        assert!(
            matches!(
                sink.drain_typed::<SliderInput>()[..],
                [SliderInput::Change(0)]
            ),
            "in a {holder}"
        );
    }
}

pub(super) fn test_a_cut_heading_shows_whole() {
    let long = "Everything this machine has installed beside the base system";
    let disclosure: Node<()> = Disclosure::new(long, Text::new("More")).into();
    let list: Node<()> =
        List::sectioned(vec![(String::from(long), vec![ListRow::new("Toolchain")])]).into();
    for node in [disclosure, list] {
        let root = built(node, Size::new(200, 200));
        let at = root.layout_rect();
        let tip = crate::tooltip::find(root.as_ref(), at.x + 40, at.y + 10);
        assert_eq!(tip.as_deref(), Some(long));
    }
}

pub(super) fn test_window_spec_fixes_or_floors_the_size() {
    let spec = WindowSpec::new(800, 600);
    assert_eq!(spec.limits(), (Size::new(400, 300), None));
    let fixed = WindowSpec::new(800, 600).fixed().min_size(10, 10);
    let size = Size::new(800, 600);
    assert_eq!(fixed.limits(), (size, Some(size)));
}

fn line_edit<M>(text: &str) -> Node<M> {
    Node::LineEdit {
        text: String::from(text),
        placeholder: String::new(),
        caret: 0,
        focused: false,
        icon: None,
        suffix: String::new(),
        invalid: false,
        tab_stop: true,
        on_input: None,
    }
}

pub(super) fn test_a_click_focuses_fields_but_not_buttons() {
    use crate::components::{IconButton, SearchField, column};
    let controls: Node<()> = column(
        slopos_theme_core::space::SM,
        vec![
            Button::new("Save").on_press(()).into(),
            IconButton::new(Icon::Trash, "Move to trash")
                .on_press(())
                .into(),
            Node::Checkbox {
                checked: false,
                label: String::from("Show hidden files"),
                on_toggle: Some(()),
                enabled: true,
            },
            Disclosure::new("Details", Text::new("More"))
                .on_toggle(())
                .into(),
            SearchField::new("", 0).on_input(|_| ()).into(),
            List::new(vec![ListRow::new("One")])
                .on_select(|_| ())
                .into(),
        ],
    );
    let root = built(controls, Size::new(400, 600));
    let clicks: Vec<bool> = root
        .children()
        .iter()
        .map(|child| {
            let r = child.layout_rect();
            let at = crate::event::control_at(root.as_ref(), r.x + 4, r.y + 4);
            assert_eq!(at.map(|w| w.id()), Some(child.id()));
            assert!(child.focus_policy().is_tab_focusable());
            child.focus_policy().is_click_focusable()
        })
        .collect();
    assert_eq!(clicks, [false, false, false, false, true, true]);
}

pub(super) fn test_a_search_field_names_what_it_finds() {
    use crate::components::SearchField;
    let size = Size::new(300, 40);
    let plain = built(SearchField::<()>::new("", 0), size);
    assert_eq!(plain.accessible_name(), Some("Search"));
    assert!(plain.focus_policy().is_tab_focusable());
    let palette = SearchField::<()>::new("", 0)
        .placeholder("File name")
        .tab_stop(false);
    let palette = built(palette, size);
    assert_eq!(palette.accessible_name(), Some("File name"));
    assert!(!palette.focus_policy().is_tab_focusable());
}

pub(super) fn test_a_sorted_heading_shows_a_chevron() {
    use crate::node::{SortIndicator, TableColumn, TableColumnWidth};
    let column = |label: &str, sort_indicator| TableColumn {
        label: String::from(label),
        width: TableColumnWidth::Flex(1),
        sort_indicator,
    };
    let table: Node<()> = Node::Table {
        columns: vec![
            column("Name", Some(SortIndicator::Descending)),
            column("Size", None),
        ],
        rows: vec![vec![
            Text::new("notes.txt").into(),
            Text::new("2 KB").into(),
        ]],
        selected: None,
        on_select: None,
        on_activate: None,
        on_header_click: None,
        on_context_menu: None,
        shows: 0,
    };
    let size = Size::new(400, 200);
    let root = built(table, size);
    let texts: Vec<String> = crate::audit::paint_recording(root.as_ref(), size, &style())
        .into_iter()
        .map(|run| run.text)
        .collect();
    assert_eq!(texts[..2], ["Name", "Size"]);
}

pub(super) fn test_every_label_cut_short_shows_whole() {
    use crate::components::SearchField;
    use crate::node::{MenuItem, MenuItemKind, TableColumn, TableColumnWidth};
    const LONG: &str = "A label far longer than the room it is given";
    let text = |s: &str| -> Node<()> { Text::new(s).into() };
    let column = |label: &str| TableColumn {
        label: String::from(label),
        width: TableColumnWidth::Flex(1),
        sort_indicator: None,
    };
    let table = |heading: &str, cell: Node<()>| Node::Table {
        columns: vec![column(heading)],
        rows: vec![vec![cell]],
        selected: None,
        on_select: None,
        on_activate: None,
        on_header_click: None,
        on_context_menu: None,
        shows: 0,
    };
    let named_file = crate::components::row(
        space::SM,
        vec![
            Node::Icon {
                icon: Icon::File,
                role: TextRole::Body,
                tone: Tone::Secondary,
            },
            Node::Expand {
                weight: 1,
                child: Box::new(text(LONG)),
            },
        ],
    );
    let first_row = style().row_height + 10;
    let item = MenuItem {
        label: LONG,
        shortcut: None,
        enabled: true,
        kind: MenuItemKind::Action,
    };
    let detail = space::SM + TextRole::Body.style().line_height + 2;
    let cases: Vec<(&str, Node<()>, (i32, i32))> = vec![
        (
            "tab",
            Node::TabBar {
                tabs: vec![String::from(LONG), String::from("B")],
                active: 0,
                on_change: None,
                content: vec![text("a"), text("b")],
            },
            (30, 10),
        ),
        (
            "progress",
            Node::ProgressBar {
                value: Some(40),
                label: String::from(LONG),
                tone: Tone::Accent,
            },
            (60, 30),
        ),
        ("tag", Tag::new(LONG, Tone::Info).into(), (60, 30)),
        ("heading", table(LONG, text("a")), (40, 10)),
        ("cell", table("Name", named_file), (60, first_row)),
        (
            "menu row",
            Node::Menu {
                items: vec![item],
                on_action: None,
            },
            (40, 10),
        ),
        (
            "placeholder",
            SearchField::new("", 0).placeholder(LONG).into(),
            (60, 30),
        ),
        (
            "folder",
            List::new(vec![ListRow::new("main.rs").detail_tail(LONG)]).into(),
            (60, detail),
        ),
    ];
    for (what, node, (x, y)) in cases {
        let root = built(node, Size::new(120, 60));
        let tip = crate::tooltip::find(root.as_ref(), x, y);
        assert_eq!(tip.as_deref(), Some(LONG), "{what}");
    }
}

pub(super) fn test_a_line_shown_a_pixel_deep_belongs_to_its_view() {
    let role = TextRole::Body;
    let (lh, fh) = (role.style().line_height, crate::text::ui::font_height(role));
    let lines: Vec<Node<()>> = (0..6)
        .map(|i| Text::new(format!("Line {i}")).into())
        .collect();
    let shown = 2 * lh + (lh - fh) / 2 + 1;
    let view: Node<()> = Node::VStack {
        children: vec![
            Node::SizedBox {
                width: None,
                height: Some(crate::constraints::Length::Px(shown)),
                child: Box::new(Node::ScrollView {
                    child: Box::new(Node::VStack {
                        children: lines,
                        spacing: 0,
                        align: CrossAxisAlignment::Stretch,
                    }),
                    scroll_y: 0,
                    on_scroll: None,
                    follow_end: false,
                }),
            },
            Node::Divider,
            Text::new("Below").into(),
        ],
        spacing: 0,
        align: CrossAxisAlignment::Stretch,
    };
    assert_clean("a line a pixel deep", audit_node(&view, &[360], 200));
}
