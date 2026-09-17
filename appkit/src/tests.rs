use super::constraints::{
    BoxConstraints, CrossAxisAlignment, EdgeInsets, Length, MAX_EXTENT, Rect, Size,
};
use super::event::{EventPhase, EventResponse, MessageSink, Modifiers, WidgetEvent, hit_test};
use super::focus::FocusManager;
use super::layout::{HStackWidget, PaddingWidget, SpacerWidget, VStackWidget};
use super::paint::PaintContext;
use super::style::StyleSheet;
use super::traits::{FocusPolicy, MeasureCtx, Widget, WidgetCore, measure_widget, place_widget};

/// Fixed measure size, so layout tests need no font.
struct FixedSizeWidget {
    core: WidgetCore,
    size: Size,
    focus: FocusPolicy,
}

impl FixedSizeWidget {
    fn new(width: i32, height: i32) -> Self {
        Self {
            core: WidgetCore::new(),
            size: Size::new(width, height),
            focus: FocusPolicy::None,
        }
    }

    fn focusable(width: i32, height: i32) -> Self {
        Self {
            core: WidgetCore::new(),
            size: Size::new(width, height),
            focus: FocusPolicy::StrongFocus,
        }
    }
}

impl Widget for FixedSizeWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }
    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        constraints.constrain(self.size)
    }
    fn paint(&self, _ctx: &mut PaintContext) {}
    fn event(
        &mut self,
        _event: &WidgetEvent,
        _phase: EventPhase,
        _sink: &mut MessageSink,
    ) -> EventResponse {
        EventResponse::Ignored
    }
    fn focus_policy(&self) -> FocusPolicy {
        self.focus
    }
}

/// A button stand-in that records which one was pressed.
struct ProbeButton {
    core: WidgetCore,
    name: &'static str,
    size: Size,
}

impl ProbeButton {
    fn new(name: &'static str, width: i32, height: i32) -> Self {
        Self {
            core: WidgetCore::new(),
            name,
            size: Size::new(width, height),
        }
    }
}

impl Widget for ProbeButton {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }
    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        constraints.constrain(self.size)
    }
    fn paint(&self, _ctx: &mut PaintContext) {}
    fn event(
        &mut self,
        event: &WidgetEvent,
        _phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        let activated = match event {
            WidgetEvent::PointerDown { .. } => true,
            WidgetEvent::KeyDown { key, .. } => matches!(
                key,
                super::event::Key::Named(super::event::NamedKey::Enter)
                    | super::event::Key::Named(super::event::NamedKey::Space)
            ),
            _ => false,
        };
        if activated {
            sink.emit_raw(Box::new(String::from(self.name)) as Box<dyn std::any::Any>);
            EventResponse::Consumed
        } else {
            EventResponse::Ignored
        }
    }
    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::StrongFocus
    }
}

fn test_tight_constraints() {
    let c = BoxConstraints::tight(Size::new(100, 50));
    assert!(c.is_tight());
    assert_eq!(c.min_width, 100);
    assert_eq!(c.max_width, 100);
    assert_eq!(c.min_height, 50);
    assert_eq!(c.max_height, 50);
    assert_eq!(c.constrain(Size::new(200, 200)), Size::new(100, 50));
    assert_eq!(c.constrain(Size::new(10, 5)), Size::new(100, 50));
}

fn test_loose_constraints() {
    let c = BoxConstraints::loose(Size::new(300, 200));
    assert!(!c.is_tight());
    assert_eq!(c.min_width, 0);
    assert_eq!(c.max_width, 300);
    assert_eq!(c.min_height, 0);
    assert_eq!(c.max_height, 200);
    assert_eq!(c.constrain(Size::new(150, 100)), Size::new(150, 100));
    assert_eq!(c.constrain(Size::ZERO), Size::ZERO);
}

fn test_constrain_clamps() {
    let c = BoxConstraints {
        min_width: 50,
        max_width: 200,
        min_height: 30,
        max_height: 100,
    };
    assert_eq!(c.constrain(Size::new(10, 5)), Size::new(50, 30));
    assert_eq!(c.constrain(Size::new(999, 999)), Size::new(200, 100));
    assert_eq!(c.constrain(Size::new(100, 60)), Size::new(100, 60));
}

fn test_deflate() {
    let c = BoxConstraints {
        min_width: 100,
        max_width: 400,
        min_height: 50,
        max_height: 300,
    };
    let insets = EdgeInsets::all(10);
    let d = c.deflate(insets);
    assert_eq!(d.min_width, 80);
    assert_eq!(d.max_width, 380);
    assert_eq!(d.min_height, 30);
    assert_eq!(d.max_height, 280);
}

fn test_unbounded() {
    let c = BoxConstraints::UNBOUNDED;
    assert_eq!(c.min_width, 0);
    assert_eq!(c.max_width, MAX_EXTENT);
    assert_eq!(c.min_height, 0);
    assert_eq!(c.max_height, MAX_EXTENT);
    assert!(!c.is_width_bounded());
    assert!(!c.is_height_bounded());
    assert_eq!(c.constrain(Size::new(9999, 9999)), Size::new(9999, 9999));
}

fn test_rect_contains() {
    let r = Rect::new(10, 20, 100, 50);
    assert!(r.contains(10, 20));
    assert!(r.contains(50, 40));
    assert!(r.contains(109, 69));
    // The upper edges are exclusive.
    assert!(!r.contains(110, 20));
    assert!(!r.contains(10, 70));
    assert!(!r.contains(9, 20));
    assert!(!r.contains(10, 19));
    assert!(!r.contains(200, 200));
}

fn test_rect_intersect() {
    let a = Rect::new(0, 0, 100, 100);
    let b = Rect::new(50, 50, 100, 100);
    let i = a.intersect(&b).expect("should intersect");
    assert_eq!(i, Rect::new(50, 50, 50, 50));
}

fn test_rect_no_intersect() {
    let a = Rect::new(0, 0, 50, 50);
    let b = Rect::new(100, 100, 50, 50);
    assert!(a.intersect(&b).is_none());
    // Touching edges are not an overlap.
    let c = Rect::new(50, 0, 50, 50);
    assert!(a.intersect(&c).is_none());
}

fn make_measure_ctx(style: &StyleSheet) -> MeasureCtx<'_> {
    MeasureCtx { style }
}

fn test_vstack_measure() {
    let style = StyleSheet::dark();
    let mut ctx = make_measure_ctx(&style);
    let children: Vec<Box<dyn Widget>> = vec![
        Box::new(FixedSizeWidget::new(80, 20)),
        Box::new(FixedSizeWidget::new(60, 30)),
        Box::new(FixedSizeWidget::new(100, 10)),
    ];
    let spacing = 5;
    let mut vstack = VStackWidget::new(children, spacing, CrossAxisAlignment::Start);
    let size = vstack.measure(BoxConstraints::UNBOUNDED, &mut ctx);
    assert_eq!(size, Size::new(100, 70));
}

fn test_hstack_measure() {
    let style = StyleSheet::dark();
    let mut ctx = make_measure_ctx(&style);
    let children: Vec<Box<dyn Widget>> = vec![
        Box::new(FixedSizeWidget::new(40, 20)),
        Box::new(FixedSizeWidget::new(60, 30)),
    ];
    let spacing = 10;
    let mut hstack = HStackWidget::new(children, spacing, CrossAxisAlignment::Start);
    let size = hstack.measure(BoxConstraints::UNBOUNDED, &mut ctx);
    assert_eq!(size, Size::new(110, 30));
}

fn test_padding_measure() {
    let style = StyleSheet::dark();
    let mut ctx = make_measure_ctx(&style);
    let child = Box::new(FixedSizeWidget::new(50, 30));
    let insets = EdgeInsets::new(5, 10, 15, 20);
    let mut padding = PaddingWidget::new(insets, child);
    let size = padding.measure(BoxConstraints::UNBOUNDED, &mut ctx);
    assert_eq!(size, Size::new(80, 50));
}

fn test_spacer_measure() {
    let style = StyleSheet::dark();
    let mut ctx = make_measure_ctx(&style);
    let mut spacer = SpacerWidget::new(Length::Px(16));
    let size = spacer.measure(BoxConstraints::UNBOUNDED, &mut ctx);
    assert_eq!(size, Size::new(16, 16));
}

fn test_focus_next() {
    let a = FixedSizeWidget::focusable(10, 10);
    let b = FixedSizeWidget::focusable(10, 10);
    let c = FixedSizeWidget::focusable(10, 10);
    let id_a = a.id();
    let id_b = b.id();

    let children: Vec<Box<dyn Widget>> = vec![Box::new(a), Box::new(b), Box::new(c)];
    let vstack = VStackWidget::new(children, 0, CrossAxisAlignment::Start);
    let mut fm = FocusManager::new();
    fm.rebuild_tab_chain(&vstack);

    fm.move_focus_next();
    assert_eq!(fm.focused(), Some(id_a));

    fm.move_focus_next();
    assert_eq!(fm.focused(), Some(id_b));
}

fn test_focus_prev() {
    let a = FixedSizeWidget::focusable(10, 10);
    let b = FixedSizeWidget::focusable(10, 10);
    let c = FixedSizeWidget::focusable(10, 10);
    let id_c = c.id();

    let children: Vec<Box<dyn Widget>> = vec![Box::new(a), Box::new(b), Box::new(c)];
    let vstack = VStackWidget::new(children, 0, CrossAxisAlignment::Start);
    let mut fm = FocusManager::new();
    fm.rebuild_tab_chain(&vstack);

    // With nothing focused, Shift+Tab lands on the last widget.
    fm.move_focus_prev();
    assert_eq!(fm.focused(), Some(id_c));
}

fn test_focus_wrap() {
    let a = FixedSizeWidget::focusable(10, 10);
    let b = FixedSizeWidget::focusable(10, 10);
    let id_a = a.id();
    let id_b = b.id();

    let children: Vec<Box<dyn Widget>> = vec![Box::new(a), Box::new(b)];
    let vstack = VStackWidget::new(children, 0, CrossAxisAlignment::Start);
    let mut fm = FocusManager::new();
    fm.rebuild_tab_chain(&vstack);

    fm.set_focused(Some(id_b));

    fm.move_focus_next();
    assert_eq!(fm.focused(), Some(id_a));

    fm.move_focus_prev();
    assert_eq!(fm.focused(), Some(id_b));
}

fn test_focus_scope() {
    let a = FixedSizeWidget::focusable(10, 10);
    let b = FixedSizeWidget::focusable(10, 10);
    let c = FixedSizeWidget::focusable(10, 10);
    let id_a = a.id();
    let id_b = b.id();
    let id_c = c.id();

    let children: Vec<Box<dyn Widget>> = vec![Box::new(a), Box::new(b), Box::new(c)];
    let vstack = VStackWidget::new(children, 0, CrossAxisAlignment::Start);
    let mut fm = FocusManager::new();
    fm.rebuild_tab_chain(&vstack);

    fm.set_focused(Some(id_a));
    assert_eq!(fm.focused(), Some(id_a));

    fm.push_scope(vec![id_b, id_c]);
    assert_eq!(fm.focused(), Some(id_b));

    fm.move_focus_next();
    assert_eq!(fm.focused(), Some(id_c));

    // Wraps inside the scope rather than escaping to A.
    fm.move_focus_next();
    assert_eq!(fm.focused(), Some(id_b));

    fm.pop_scope();
    assert_eq!(fm.focused(), Some(id_a));
}

fn test_hit_test_leaf() {
    let mut w = FixedSizeWidget::new(100, 50);
    place_widget(&mut w, Rect::new(10, 20, 100, 50));
    let result = hit_test(&w, 50, 40);
    assert!(result.is_some());
    let ht = result.unwrap();
    assert_eq!(ht.target, w.id());
    assert_eq!(ht.chain.len(), 1);
}

fn test_hit_test_miss() {
    let mut w = FixedSizeWidget::new(100, 50);
    place_widget(&mut w, Rect::new(10, 20, 100, 50));
    let result = hit_test(&w, 0, 0);
    assert!(result.is_none());
    let result2 = hit_test(&w, 200, 200);
    assert!(result2.is_none());
}

fn test_edge_insets_symmetric() {
    let insets = EdgeInsets::symmetric(10, 5);
    assert_eq!(insets.horizontal(), 20);
    assert_eq!(insets.vertical(), 10);
    assert_eq!(insets.left, 10);
    assert_eq!(insets.right, 10);
    assert_eq!(insets.top, 5);
    assert_eq!(insets.bottom, 5);
}

fn test_box_constraints_loosen() {
    let c = BoxConstraints {
        min_width: 50,
        max_width: 200,
        min_height: 30,
        max_height: 100,
    };
    let l = c.loosen();
    assert_eq!(l.min_width, 0);
    assert_eq!(l.max_width, 200);
    assert_eq!(l.min_height, 0);
    assert_eq!(l.max_height, 100);
}

fn test_deflate_unbounded() {
    let c = BoxConstraints::UNBOUNDED;
    let insets = EdgeInsets::all(10);
    let d = c.deflate(insets);
    assert_eq!(d.max_width, MAX_EXTENT);
    assert_eq!(d.max_height, MAX_EXTENT);
    assert_eq!(d.min_width, 0);
    assert_eq!(d.min_height, 0);
}

use super::node::{ContextMenuAt, TableColumn, TableColumnWidth};
use super::widgets::popup::PopupWidget;
use super::widgets::table::TableWidget;

fn table_column(label: &str) -> TableColumn {
    TableColumn {
        label: String::from(label),
        width: TableColumnWidth::Flex(1),
        sort_indicator: None,
    }
}

/// A 3-row, 1-column table laid out at (0,0,200,200) with 20px rows.
fn context_table(selected: Option<usize>) -> TableWidget {
    let rows: Vec<Vec<Box<dyn Widget>>> = (0..3)
        .map(|_| vec![Box::new(FixedSizeWidget::new(50, 20)) as Box<dyn Widget>])
        .collect();
    let mut table = TableWidget::new(
        vec![table_column("Name")],
        rows,
        20,
        selected,
        Some(Box::new(|i: usize| Box::new(i) as Box<dyn std::any::Any>)),
        Some(Box::new(|i: usize| {
            Box::new(format!("header{i}")) as Box<dyn std::any::Any>
        })),
        Some(Box::new(|at: ContextMenuAt| {
            Box::new(at) as Box<dyn std::any::Any>
        })),
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(
        &mut table,
        BoxConstraints::tight(Size::new(200, 200)),
        &mut ctx,
    );
    place_widget(&mut table, Rect::new(0, 0, 200, 200));
    // A table answers keys only when it holds the focus, which is what stops it
    // taking the arrows from whatever else is on screen.
    let mut sink = MessageSink::new();
    table.event(&WidgetEvent::FocusGained, EventPhase::Target, &mut sink);
    table
}

fn press(x: i32, y: i32, button: super::event::PointerButton) -> WidgetEvent {
    WidgetEvent::PointerDown {
        x,
        y,
        button,
        modifiers: Modifiers::default(),
    }
}

fn test_table_right_click_emits_context_menu() {
    let mut table = context_table(None);
    let mut sink = MessageSink::new();

    // Row 1 spans y=[40,60) once the 20px header is skipped.
    let resp = table.event(
        &press(10, 45, super::event::PointerButton::Right),
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());

    let requests = sink.drain_typed::<ContextMenuAt>();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].row, 1);
    assert_eq!((requests[0].x, requests[0].y), (10, 45));
}

/// The menu acts on the selection, so opening it must move the selection too.
fn test_table_right_click_selects_row() {
    let mut table = context_table(Some(0));
    let mut sink = MessageSink::new();
    // Row 2 is the last of three: y=[60,80) once the 20px header is skipped.
    table.event(
        &press(10, 70, super::event::PointerButton::Right),
        EventPhase::Target,
        &mut sink,
    );
    let selections = sink.drain_typed::<usize>();
    assert_eq!(selections, vec![2]);
}

fn test_table_left_click_emits_no_context_menu() {
    let mut table = context_table(None);
    let mut sink = MessageSink::new();
    table.event(
        &press(10, 45, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(sink.drain_typed::<ContextMenuAt>().is_empty());
    assert_eq!(sink.drain_typed::<usize>(), vec![1]);
}

fn test_table_right_click_header_is_inert() {
    let mut table = context_table(None);
    let mut sink = MessageSink::new();
    let resp = table.event(
        &press(10, 5, super::event::PointerButton::Right),
        EventPhase::Target,
        &mut sink,
    );
    assert!(!resp.is_consumed());
    assert!(sink.drain_typed::<ContextMenuAt>().is_empty());
    assert!(sink.drain_typed::<String>().is_empty());
}

/// Keyboard parity: the Menu key raises the same request, anchored to the row.
fn test_table_menu_key_emits_context_menu() {
    let mut table = context_table(Some(1));
    let mut sink = MessageSink::new();
    let resp = table.event(
        &WidgetEvent::KeyDown {
            key: super::event::Key::Named(super::event::NamedKey::Menu),
            modifiers: super::event::Modifiers::default(),
            repeat: false,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    let requests = sink.drain_typed::<ContextMenuAt>();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].row, 1);
    // Anchored at the row's bottom-left: header 20 + row 1 ends at y=60.
    assert_eq!((requests[0].x, requests[0].y), (0, 60));
}

fn test_table_menu_key_without_selection_is_inert() {
    let mut table = context_table(None);
    let mut sink = MessageSink::new();
    let resp = table.event(
        &WidgetEvent::KeyDown {
            key: super::event::Key::Named(super::event::NamedKey::Menu),
            modifiers: super::event::Modifiers::default(),
            repeat: false,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert!(!resp.is_consumed());
    assert!(sink.drain_typed::<ContextMenuAt>().is_empty());
}

fn popup_at(x: i32, y: i32, w: i32, h: i32) -> PopupWidget {
    let mut popup = PopupWidget::new(
        x,
        y,
        Box::new(FixedSizeWidget::new(w, h)),
        Some(Box::new(|| {
            Box::new(String::from("dismiss")) as Box<dyn std::any::Any>
        })),
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(
        &mut popup,
        BoxConstraints::tight(Size::new(200, 200)),
        &mut ctx,
    );
    place_widget(&mut popup, Rect::new(0, 0, 200, 200));
    popup
}

fn test_popup_places_child_at_anchor() {
    let popup = popup_at(30, 40, 60, 50);
    let child = popup.children()[0].layout_rect();
    assert_eq!((child.x, child.y), (30, 40));
}

/// Near the right or bottom edge the child flips back over the anchor rather
/// than being clipped, so it never covers the pointer that opened it.
fn test_popup_flips_at_edges() {
    let popup = popup_at(190, 195, 60, 50);
    let child = popup.children()[0].layout_rect();
    assert_eq!((child.x, child.y), (130, 145));
}

/// A child too large to flip is clamped inside the parent instead.
fn test_popup_clamps_oversized_child() {
    let popup = popup_at(190, 190, 300, 300);
    let child = popup.children()[0].layout_rect();
    assert_eq!((child.x, child.y), (0, 0));
}

fn test_popup_click_outside_dismisses() {
    let mut popup = popup_at(30, 40, 60, 50);
    let mut sink = MessageSink::new();
    let resp = popup.event(
        &press(5, 5, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    assert_eq!(sink.drain_typed::<String>().len(), 1);
}

fn test_popup_click_inside_does_not_dismiss() {
    let mut popup = popup_at(30, 40, 60, 50);
    let mut sink = MessageSink::new();
    popup.event(
        &press(35, 45, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(sink.drain_typed::<String>().is_empty());
}

fn test_popup_escape_dismisses() {
    let mut popup = popup_at(30, 40, 60, 50);
    let mut sink = MessageSink::new();
    let resp = popup.event(
        &WidgetEvent::KeyDown {
            key: super::event::Key::Named(super::event::NamedKey::Escape),
            modifiers: super::event::Modifiers::default(),
            repeat: false,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    assert_eq!(sink.drain_typed::<String>().len(), 1);
}

/// A popup is modal: events its child ignored must not reach the tree below.
fn test_popup_swallows_unhandled_events() {
    let mut popup = popup_at(30, 40, 60, 50);
    let mut sink = MessageSink::new();
    let resp = popup.event(
        &WidgetEvent::Scroll {
            x: 40,
            y: 50,
            delta_x: 0,
            delta_y: 10,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
}

use super::widgets::dialog::DialogWidget;

/// A dialog laid out in `window`, with two 80x30 actions and a 200x40 body.
fn dialog_in(window: Size) -> DialogWidget {
    let mut dialog = DialogWidget::new(
        String::from("Kill task?"),
        Box::new(FixedSizeWidget::new(200, 40)),
        vec![
            Box::new(ProbeButton::new("kill", 80, 30)) as Box<dyn Widget>,
            Box::new(ProbeButton::new("cancel", 80, 30)) as Box<dyn Widget>,
        ],
        Some(Box::new(|| {
            Box::new(String::from("dismiss")) as Box<dyn std::any::Any>
        })),
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(&mut dialog, BoxConstraints::tight(window), &mut ctx);
    place_widget(&mut dialog, Rect::new(0, 0, window.width, window.height));
    dialog
}

fn test_dialog_places_children_inside_card() {
    let window = Size::new(640, 444);
    let dialog = dialog_in(window);
    let card = dialog.card_rect();

    assert!(
        card.width > 0 && card.height > 0,
        "card degenerate: {card:?}"
    );
    assert!(
        card.x >= 0 && card.y >= 0,
        "card starts off-screen: {card:?}"
    );
    assert!(
        card.x + card.width <= window.width && card.y + card.height <= window.height,
        "card overflows the window: {card:?}"
    );

    for action in dialog.children() {
        let r = action.layout_rect();
        assert!(
            r.x >= card.x
                && r.y >= card.y
                && r.x + r.width <= card.x + card.width
                && r.y + r.height <= card.y + card.height,
            "action {r:?} outside card {card:?}"
        );
    }
}

fn test_dialog_card_height_covers_content() {
    let dialog = dialog_in(Size::new(640, 444));
    let card = dialog.card_rect();
    // title row + 40px content + 30px actions + padding.
    assert!(card.height >= 40 + 30, "card too short: {card:?}");
}

fn test_dialog_card_is_centered() {
    let window = Size::new(640, 444);
    let dialog = dialog_in(window);
    let card = dialog.card_rect();
    assert_eq!(card.x, (window.width - card.width) / 2);
    assert_eq!(card.y, (window.height - card.height) / 2);
}

fn test_dialog_click_routes_to_action_under_pointer() {
    let mut dialog = dialog_in(Size::new(640, 444));
    let second = dialog.children()[1].layout_rect();
    let (cx, cy) = (second.x + second.width / 2, second.y + second.height / 2);

    let mut sink = MessageSink::new();
    dialog.event(
        &press(cx, cy, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert_eq!(sink.drain_typed::<String>(), vec![String::from("cancel")]);
}

fn test_dialog_backdrop_click_dismisses() {
    let mut dialog = dialog_in(Size::new(640, 444));
    let mut sink = MessageSink::new();
    let resp = dialog.event(
        &press(2, 2, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    assert_eq!(sink.drain_typed::<String>(), vec![String::from("dismiss")]);
}

/// A press inside the card that hits no action is swallowed, not passed down.
fn test_dialog_is_modal_over_its_parent() {
    let mut dialog = dialog_in(Size::new(640, 444));
    let card = dialog.card_rect();
    let mut sink = MessageSink::new();
    let resp = dialog.event(
        &press(card.x + 2, card.y + 2, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    assert!(sink.drain_typed::<String>().is_empty());
}

fn key(named: super::event::NamedKey, shift: bool) -> WidgetEvent {
    WidgetEvent::KeyDown {
        key: super::event::Key::Named(named),
        modifiers: super::event::Modifiers {
            shift,
            ..Default::default()
        },
        repeat: false,
    }
}

fn test_dialog_enter_without_selection_fires_nothing() {
    let mut dialog = dialog_in(Size::new(640, 444));
    let mut sink = MessageSink::new();
    dialog.event(
        &key(super::event::NamedKey::Enter, false),
        EventPhase::Target,
        &mut sink,
    );
    assert!(sink.drain_typed::<String>().is_empty());
}

fn test_dialog_keyboard_selects_then_activates() {
    let mut dialog = dialog_in(Size::new(640, 444));
    let mut sink = MessageSink::new();
    dialog.event(
        &key(super::event::NamedKey::Tab, false),
        EventPhase::Target,
        &mut sink,
    );
    dialog.event(
        &key(super::event::NamedKey::Tab, false),
        EventPhase::Target,
        &mut sink,
    );
    dialog.event(
        &key(super::event::NamedKey::Enter, false),
        EventPhase::Target,
        &mut sink,
    );
    assert_eq!(sink.drain_typed::<String>(), vec![String::from("cancel")]);
}

/// Escape dismisses rather than activating whatever is selected.
fn test_dialog_escape_dismisses() {
    let mut dialog = dialog_in(Size::new(640, 444));
    let mut sink = MessageSink::new();
    let resp = dialog.event(
        &key(super::event::NamedKey::Escape, false),
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    assert_eq!(sink.drain_typed::<String>(), vec![String::from("dismiss")]);
}

fn test_unbounded_extent_survives_padding_arithmetic() {
    let c = BoxConstraints::UNBOUNDED;
    assert!(!c.is_width_bounded());
    assert!(!c.is_height_bounded());
    // The sum a card computes: title + content + two paddings.
    let total = c.max_height + 16 + 16 + 32;
    assert!(total > 0, "unbounded extent wrapped to {total}");
}

/// Deflating an unbounded axis must leave it unbounded, or a scroll view's
/// child suddenly believes it has a finite budget.
fn test_deflate_preserves_unboundedness() {
    let d = BoxConstraints::UNBOUNDED.deflate(EdgeInsets::all(10));
    assert!(!d.is_width_bounded());
    assert!(!d.is_height_bounded());
    assert_eq!(d.max_width, MAX_EXTENT);
}

fn test_measure_widget_records_size() {
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    let mut w = FixedSizeWidget::new(70, 25);
    assert_eq!(w.measured_size(), Size::ZERO);
    let size = measure_widget(&mut w, BoxConstraints::UNBOUNDED, &mut ctx);
    assert_eq!(size, Size::new(70, 25));
    assert_eq!(w.measured_size(), Size::new(70, 25));
}

/// `place_widget` is what records the rect, so `layout_rect` reflects the
/// placement even for a leaf that implements no `layout` at all.
fn test_place_widget_records_rect() {
    let mut w = FixedSizeWidget::new(70, 25);
    place_widget(&mut w, Rect::new(5, 6, 70, 25));
    assert_eq!(w.layout_rect(), Rect::new(5, 6, 70, 25));
}

/// A ZStack's layers all get the full area: an overlay covers its siblings
/// rather than displacing them.
fn test_zstack_layers_share_the_full_rect() {
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    let children: Vec<Box<dyn Widget>> = vec![
        Box::new(FixedSizeWidget::new(50, 20)),
        Box::new(FixedSizeWidget::new(10, 10)),
    ];
    let mut z = super::layout::ZStackWidget::new(children);
    measure_widget(&mut z, BoxConstraints::tight(Size::new(200, 100)), &mut ctx);
    place_widget(&mut z, Rect::new(0, 0, 200, 100));
    for child in z.children() {
        assert_eq!(child.layout_rect(), Rect::new(0, 0, 200, 100));
    }
}

fn test_display_col_expands_tabs() {
    use super::widgets::code_view::display_col;
    // "\tab": the tab advances to the next multiple of four, so 'a' is at 4.
    assert_eq!(display_col("\tab", 0, 4), 0);
    assert_eq!(display_col("\tab", 1, 4), 4);
    assert_eq!(display_col("\tab", 2, 4), 5);
    // Two spaces then a tab: the tab fills the rest of the stop, not four more.
    assert_eq!(display_col("  \tx", 3, 4), 4);
    // A column past the end of the line keeps counting.
    assert_eq!(display_col("ab", 5, 4), 5);
}

fn test_char_col_from_display_inverts_display_col() {
    use super::widgets::code_view::{char_col_from_display, display_col};
    for text in ["plain", "\tab", "  \tx", "a\tb\tc"] {
        let len = text.chars().count();
        for col in 0..=len {
            let display = display_col(text, col, 4);
            assert_eq!(
                char_col_from_display(text, display, 4),
                col,
                "{text:?} col {col}"
            );
        }
    }
}

fn test_click_inside_a_tab_snaps_to_one_side() {
    use super::widgets::code_view::char_col_at_half;
    // A tab spans display columns 0..4, i.e. half cells 0..8; its left half
    // belongs to the tab and its right half to what follows.
    assert_eq!(char_col_at_half("\tx", 2, 4), 0);
    assert_eq!(char_col_at_half("\tx", 6, 4), 1);
}

fn test_click_past_a_glyphs_midpoint_lands_after_it() {
    use super::widgets::code_view::char_col_at_half;
    // Whole-cell resolution cannot express this: the click and the character
    // share a display column.
    assert_eq!(char_col_at_half("abc", 0, 4), 0);
    assert_eq!(char_col_at_half("abc", 1, 4), 1);
    assert_eq!(char_col_at_half("abc", 2, 4), 1);
    assert_eq!(char_col_at_half("abc", 5, 4), 3);
    // Past the end of the line the caret parks at the end.
    assert_eq!(char_col_at_half("abc", 20, 4), 3 + 7);
}

fn test_gutter_width_grows_with_the_line_count() {
    use super::widgets::code_view::gutter_width;
    let one = gutter_width(9, 10, true);
    let three = gutter_width(999, 10, true);
    assert!(three > one);
    assert_eq!(three - one, 20);
    // Numbers off means no gutter to speak of, whatever the file's length.
    assert_eq!(gutter_width(9, 10, false), gutter_width(999_999, 10, false));
}

fn test_visible_line_count_floors() {
    use super::widgets::code_view::visible_line_count;
    assert_eq!(visible_line_count(100, 22), 4);
    assert_eq!(visible_line_count(0, 22), 0);
    assert_eq!(visible_line_count(100, 0), 0);
}

fn code_view(
    lines: Vec<super::widgets::code_view::CodeLine>,
    selecting: bool,
) -> super::widgets::code_view::CodeViewWidget {
    let total = lines.len();
    let mut view = super::widgets::code_view::CodeViewWidget::new(
        lines,
        0,
        total,
        0,
        4,
        Some((0, 0)),
        None,
        true,
        true,
        selecting,
        Some(Box::new(|i: super::widgets::code_view::CodeInput| {
            Box::new(i) as Box<dyn std::any::Any>
        })),
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(
        &mut view,
        BoxConstraints::tight(Size::new(400, 200)),
        &mut ctx,
    );
    place_widget(&mut view, Rect::new(0, 0, 400, 200));
    view
}

fn code_lines(count: usize) -> Vec<super::widgets::code_view::CodeLine> {
    (0..count)
        .map(|number| super::widgets::code_view::CodeLine {
            number,
            text: String::from("some text on a line"),
            spans: Vec::new(),
            highlights: Vec::new(),
        })
        .collect()
}

fn test_code_view_click_reports_a_document_position() {
    use super::widgets::code_view::CodeInput;
    let mut view = code_view(code_lines(5), false);
    let mut sink = MessageSink::new();
    let line_h = super::text::cell_height();
    let resp = view.event(
        &press(200, line_h * 2 + 2, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    let inputs = sink.drain_typed::<CodeInput>();
    assert_eq!(inputs.len(), 1);
    match inputs[0] {
        CodeInput::Click { line, extend, .. } => {
            assert_eq!(line, 2);
            assert!(!extend);
        }
        other => panic!("expected a click, got {other:?}"),
    }
}

fn test_code_view_drags_only_while_selecting() {
    use super::widgets::code_view::CodeInput;
    let mut idle = code_view(code_lines(5), false);
    let mut sink = MessageSink::new();
    let resp = idle.event(
        &WidgetEvent::PointerMove { x: 100, y: 40 },
        EventPhase::Target,
        &mut sink,
    );
    assert!(!resp.is_consumed());
    assert!(sink.drain_typed::<CodeInput>().is_empty());

    // The application says a drag is live, because the widget that saw the
    // press was replaced by the rebuild that press caused.
    let mut dragging = code_view(code_lines(5), true);
    let resp = dragging.event(
        &WidgetEvent::PointerMove { x: 100, y: 40 },
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    assert!(matches!(
        sink.drain_typed::<CodeInput>().first(),
        Some(CodeInput::Drag { .. })
    ));
}

fn test_code_view_shift_click_extends() {
    use super::widgets::code_view::CodeInput;
    let mut view = code_view(code_lines(5), false);
    let mut sink = MessageSink::new();
    let mods = Modifiers {
        shift: true,
        ..Modifiers::default()
    };
    view.event(
        &WidgetEvent::PointerDown {
            x: 120,
            y: 10,
            button: super::event::PointerButton::Left,
            modifiers: mods,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert!(matches!(
        sink.drain_typed::<CodeInput>().first(),
        Some(CodeInput::Click { extend: true, .. })
    ));
}

fn tree_view(rows: usize, selecting_focused: bool) -> super::widgets::tree_view::TreeViewWidget {
    let rows: Vec<super::widgets::tree_view::TreeRow> = (0..rows)
        .map(|i| super::widgets::tree_view::TreeRow {
            label: format!("entry{i}"),
            depth: if i == 0 { 0 } else { 1 },
            is_dir: i % 2 == 0,
            expanded: false,
            active: false,
            modified: false,
        })
        .collect();
    let total = rows.len();
    let mut view = super::widgets::tree_view::TreeViewWidget::new(
        rows,
        0,
        total,
        None,
        selecting_focused,
        Some(Box::new(|i: super::widgets::tree_view::TreeInput| {
            Box::new(i) as Box<dyn std::any::Any>
        })),
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(
        &mut view,
        BoxConstraints::tight(Size::new(220, 200)),
        &mut ctx,
    );
    place_widget(&mut view, Rect::new(0, 0, 220, 200));
    view
}

fn test_tree_click_on_the_twisty_toggles_rather_than_opens() {
    use super::widgets::tree_view::TreeInput;
    let mut view = tree_view(4, true);
    let mut sink = MessageSink::new();
    // Row 0 is a directory at depth 0: its twisty sits at the left padding.
    view.event(
        &press(10, 4, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(matches!(
        sink.drain_typed::<TreeInput>().first(),
        Some(TreeInput::Toggle { row: 0 })
    ));

    // Further right on the same row is the label, which opens it.
    view.event(
        &press(150, 4, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(matches!(
        sink.drain_typed::<TreeInput>().first(),
        Some(TreeInput::Activate { row: 0 })
    ));
}

fn test_tree_keys_only_reach_a_focused_tree() {
    use super::widgets::tree_view::TreeInput;
    let mut blurred = tree_view(4, false);
    let mut sink = MessageSink::new();
    let key = WidgetEvent::KeyDown {
        key: super::event::Key::Named(super::event::NamedKey::Down),
        modifiers: Modifiers::default(),
        repeat: false,
    };
    assert!(
        !blurred
            .event(&key, EventPhase::Target, &mut sink)
            .is_consumed()
    );
    assert!(sink.drain_typed::<TreeInput>().is_empty());

    let mut focused = tree_view(4, true);
    assert!(
        focused
            .event(&key, EventPhase::Target, &mut sink)
            .is_consumed()
    );
    assert!(matches!(
        sink.drain_typed::<TreeInput>().first(),
        Some(TreeInput::Key { .. })
    ));
}

fn test_editor_tabs_close_box_is_distinct_from_the_tab() {
    use super::widgets::editor_tabs::{EditorTab, EditorTabsWidget, TabInput};
    let tabs = vec![
        EditorTab {
            title: String::from("one.rs"),
            modified: false,
        },
        EditorTab {
            title: String::from("two.rs"),
            modified: true,
        },
    ];
    let mut widget = EditorTabsWidget::new(
        tabs,
        0,
        Some(Box::new(|i: TabInput| {
            Box::new(i) as Box<dyn std::any::Any>
        })),
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(
        &mut widget,
        BoxConstraints::tight(Size::new(400, 34)),
        &mut ctx,
    );
    place_widget(&mut widget, Rect::new(0, 0, 400, 34));

    let mut sink = MessageSink::new();
    widget.event(
        &press(20, 17, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(matches!(
        sink.drain_typed::<TabInput>().first(),
        Some(TabInput::Select(0))
    ));

    // Middle click closes wherever it lands on the tab.
    widget.event(
        &press(20, 17, super::event::PointerButton::Middle),
        EventPhase::Target,
        &mut sink,
    );
    assert!(matches!(
        sink.drain_typed::<TabInput>().first(),
        Some(TabInput::Close(0))
    ));
}

fn test_drag_handle_reports_begin_move_end() {
    use super::widgets::drag_handle::{DragHandleWidget, DragInput};
    let make = |active: bool| {
        let mut handle = DragHandleWidget::new(
            super::constraints::Orientation::Vertical,
            active,
            Some(Box::new(|i: DragInput| {
                Box::new(i) as Box<dyn std::any::Any>
            })),
        );
        let style = StyleSheet::dark();
        let mut ctx = MeasureCtx { style: &style };
        measure_widget(
            &mut handle,
            BoxConstraints::tight(Size::new(6, 200)),
            &mut ctx,
        );
        place_widget(&mut handle, Rect::new(200, 0, 6, 200));
        handle
    };

    let mut idle = make(false);
    let mut sink = MessageSink::new();
    idle.event(
        &press(202, 50, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(matches!(
        sink.drain_typed::<DragInput>().first(),
        Some(DragInput::Begin)
    ));
    // A move with no drag live is hover, not a resize.
    idle.event(
        &WidgetEvent::PointerMove { x: 260, y: 50 },
        EventPhase::Target,
        &mut sink,
    );
    assert!(sink.drain_typed::<DragInput>().is_empty());

    let mut active = make(true);
    active.event(
        &WidgetEvent::PointerMove { x: 260, y: 50 },
        EventPhase::Target,
        &mut sink,
    );
    assert!(matches!(
        sink.drain_typed::<DragInput>().first(),
        Some(DragInput::Move(260))
    ));
    active.event(
        &WidgetEvent::PointerUp {
            x: 260,
            y: 50,
            button: super::event::PointerButton::Left,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert!(matches!(
        sink.drain_typed::<DragInput>().first(),
        Some(DragInput::End)
    ));
}

fn test_a_release_outside_a_widget_still_reaches_it() {
    use super::widgets::drag_handle::{DragHandleWidget, DragInput};
    // Ending outside the handle's 6 px is the ordinary way to end a drag;
    // without the release it stays latched.
    let handle = DragHandleWidget::new(
        super::constraints::Orientation::Vertical,
        true,
        Some(Box::new(|i: DragInput| {
            Box::new(i) as Box<dyn std::any::Any>
        })),
    );
    let mut stack = HStackWidget::new(
        vec![
            Box::new(handle) as Box<dyn Widget>,
            Box::new(FixedSizeWidget::new(394, 200)) as Box<dyn Widget>,
        ],
        0,
        CrossAxisAlignment::Stretch,
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(
        &mut stack,
        BoxConstraints::tight(Size::new(400, 200)),
        &mut ctx,
    );
    place_widget(&mut stack, Rect::new(0, 0, 400, 200));

    let mut sink = MessageSink::new();
    stack.event(
        &WidgetEvent::PointerUp {
            x: 380,
            y: 50,
            button: super::event::PointerButton::Left,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert!(matches!(
        sink.drain_typed::<DragInput>().first(),
        Some(DragInput::End)
    ));
}

fn test_scroll_goes_to_what_is_under_the_pointer() {
    // A wheel turn carries a position; without one it goes to whichever child
    // the container visits first, and a code surface consumes every scroll.
    struct Counter {
        core: WidgetCore,
        seen: std::rc::Rc<std::cell::Cell<u32>>,
    }
    impl Widget for Counter {
        fn core(&self) -> &WidgetCore {
            &self.core
        }
        fn core_mut(&mut self) -> &mut WidgetCore {
            &mut self.core
        }
        fn measure(&mut self, c: BoxConstraints, _: &mut MeasureCtx) -> Size {
            // Half the row each, so the two rects are distinguishable.
            c.constrain(Size::new(200, 200))
        }
        fn paint(&self, _: &mut PaintContext) {}
        fn event(
            &mut self,
            event: &WidgetEvent,
            _: EventPhase,
            _: &mut MessageSink,
        ) -> EventResponse {
            if matches!(event, WidgetEvent::Scroll { .. }) {
                self.seen.set(self.seen.get() + 1);
                return EventResponse::Consumed;
            }
            EventResponse::Ignored
        }
    }

    let left = std::rc::Rc::new(std::cell::Cell::new(0));
    let right = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut stack = HStackWidget::new(
        vec![
            Box::new(Counter {
                core: WidgetCore::new(),
                seen: left.clone(),
            }) as Box<dyn Widget>,
            Box::new(Counter {
                core: WidgetCore::new(),
                seen: right.clone(),
            }) as Box<dyn Widget>,
        ],
        0,
        CrossAxisAlignment::Stretch,
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(
        &mut stack,
        BoxConstraints::tight(Size::new(400, 200)),
        &mut ctx,
    );
    place_widget(&mut stack, Rect::new(0, 0, 400, 200));

    let mut sink = MessageSink::new();
    stack.event(
        &WidgetEvent::Scroll {
            x: 40,
            y: 100,
            delta_x: 0,
            delta_y: -3,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert_eq!((left.get(), right.get()), (1, 0), "left half");

    stack.event(
        &WidgetEvent::Scroll {
            x: 360,
            y: 100,
            delta_x: 0,
            delta_y: -3,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert_eq!((left.get(), right.get()), (1, 1), "right half");
}

fn test_only_the_visible_tab_panel_is_reachable() {
    use super::widgets::tab_bar::TabBarWidget;
    // Every panel shares one rect, so exposing them all sends the hit test —
    // and the focus that follows it — to the last.
    let panels: Vec<Box<dyn Widget>> = vec![
        Box::new(FixedSizeWidget::focusable(100, 40)),
        Box::new(FixedSizeWidget::new(100, 40)),
    ];
    let mut bar = TabBarWidget::new(
        vec![String::from("one"), String::from("two")],
        0,
        None,
        panels,
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(
        &mut bar,
        BoxConstraints::tight(Size::new(200, 200)),
        &mut ctx,
    );
    place_widget(&mut bar, Rect::new(0, 0, 200, 200));
    assert_eq!(bar.children().len(), 1);
    // Tab 0 is active, so its panel — the focusable one — is what a hit finds.
    let hit = hit_test(&bar, 50, 150).expect("a hit inside the panel");
    let policy = {
        fn find(w: &dyn Widget, id: super::traits::WidgetId) -> FocusPolicy {
            if w.id() == id {
                return w.focus_policy();
            }
            for child in w.children() {
                let p = find(child.as_ref(), id);
                if p.is_focusable() {
                    return p;
                }
            }
            FocusPolicy::None
        }
        find(&bar, hit.target)
    };
    assert!(policy.is_focusable());
}

fn test_a_popup_swallows_a_press_its_child_ignored() {
    // A press inside the popup that the child did not want is still the
    // popup's; falling through reaches the tree behind the open menu.
    let mut popup = popup_at(30, 40, 60, 50);
    let mut sink = MessageSink::new();
    let resp = popup.event(
        &press(50, 60, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
}

fn test_a_zero_extent_clip_damages_nothing() {
    // `DamageRect`'s bounds are inclusive, so a rect of no extent must come out
    // invalid rather than as the single pixel an exclusive conversion gives.
    assert!(!Rect::new(10, 10, 0, 5).to_damage_rect().is_valid());
    assert!(!Rect::new(10, 10, 5, 0).to_damage_rect().is_valid());
    let dr = Rect::new(10, 20, 4, 3).to_damage_rect();
    assert_eq!((dr.x0, dr.y0, dr.x1, dr.y1), (10, 20, 13, 22));
}

fn test_line_edit_reports_keys_only_when_focused() {
    use super::widgets::line_edit::{LineEditInput, LineEditWidget};
    let make = |focused: bool| {
        let mut edit = LineEditWidget::new(
            String::from("query"),
            String::from("Find"),
            5,
            focused,
            None,
            String::new(),
            false,
            Some(Box::new(|i: LineEditInput| {
                Box::new(i) as Box<dyn std::any::Any>
            })),
        );
        let style = StyleSheet::dark();
        let mut ctx = MeasureCtx { style: &style };
        measure_widget(
            &mut edit,
            BoxConstraints::tight(Size::new(200, 28)),
            &mut ctx,
        );
        place_widget(&mut edit, Rect::new(0, 0, 200, 28));
        edit
    };

    let mut sink = MessageSink::new();
    let text = WidgetEvent::TextInput { character: 'x' };
    let mut blurred = make(false);
    assert!(
        !blurred
            .event(&text, EventPhase::Target, &mut sink)
            .is_consumed()
    );
    assert!(sink.drain_typed::<LineEditInput>().is_empty());

    let mut focused = make(true);
    assert!(
        focused
            .event(&text, EventPhase::Target, &mut sink)
            .is_consumed()
    );
    assert!(matches!(
        sink.drain_typed::<LineEditInput>().first(),
        Some(LineEditInput::Text { character: 'x' })
    ));
}

fn test_text_field_types_a_space() {
    let mut field = super::widgets::text_field::TextFieldWidget::new(
        String::from("ab"),
        String::new(),
        None,
        None,
        false,
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(
        &mut field,
        BoxConstraints::tight(Size::new(200, 28)),
        &mut ctx,
    );
    place_widget(&mut field, Rect::new(0, 0, 200, 28));
    let mut sink = MessageSink::new();
    // A field answers keys only when it holds the focus, which is what stops it
    // taking them from whatever else is on screen.
    field.event(&WidgetEvent::FocusGained, EventPhase::Target, &mut sink);
    field.event(
        &WidgetEvent::KeyDown {
            key: super::event::Key::Named(super::event::NamedKey::End),
            modifiers: Modifiers::default(),
            repeat: false,
        },
        EventPhase::Target,
        &mut sink,
    );
    field.event(
        &WidgetEvent::KeyDown {
            key: super::event::Key::Named(super::event::NamedKey::Space),
            modifiers: Modifiers::default(),
            repeat: false,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert_eq!(field.text(), "ab ");
}

fn test_card_gives_its_child_the_whole_rect() {
    let child = Box::new(FixedSizeWidget::new(40, 20));
    let mut card = super::widgets::card::CardWidget::new(
        slopos_abi::draw::Color32::BLACK,
        None,
        6,
        true,
        child,
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(
        &mut card,
        BoxConstraints::loose(Size::new(200, 100)),
        &mut ctx,
    );
    place_widget(&mut card, Rect::new(10, 10, 120, 60));
    assert_eq!(card.children()[0].layout_rect(), Rect::new(10, 10, 120, 60));
}

fn test_elide_keeps_what_fits() {
    use super::widgets::tree_view::elide;
    // Eight pixels per character, so the arithmetic is the test's rather than
    // the installed font's.
    let width = |t: &str| t.chars().count() as i32 * 8;
    assert_eq!(elide("short.rs", 10_000, width), "short.rs");
    let squeezed = elide("a-very-long-file-name.rs", 40, width);
    assert!(squeezed.starts_with('\u{2026}'));
    assert!(width(&squeezed) <= 40);
    // The tail is what a long file name is distinguished by, so that is what
    // survives: four characters of it, plus the ellipsis, is the whole budget.
    assert!(squeezed.ends_with("e.rs"));
    // No budget at all elides to the ellipsis alone rather than panicking.
    assert_eq!(elide("name.rs", 4, width), "\u{2026}");
}

/// Every appkit unit test, for a host `cargo test` run and for the
/// `/bin/appkit_test` userland binary that reports them over KTAP.
pub fn cases() -> &'static [(&'static str, fn())] {
    &[
        ("display_col_expands_tabs", test_display_col_expands_tabs),
        (
            "char_col_from_display_inverts_display_col",
            test_char_col_from_display_inverts_display_col,
        ),
        (
            "click_inside_a_tab_snaps_to_one_side",
            test_click_inside_a_tab_snaps_to_one_side,
        ),
        (
            "click_past_a_glyphs_midpoint_lands_after_it",
            test_click_past_a_glyphs_midpoint_lands_after_it,
        ),
        (
            "gutter_width_grows_with_the_line_count",
            test_gutter_width_grows_with_the_line_count,
        ),
        ("visible_line_count_floors", test_visible_line_count_floors),
        (
            "code_view_click_reports_a_document_position",
            test_code_view_click_reports_a_document_position,
        ),
        (
            "code_view_drags_only_while_selecting",
            test_code_view_drags_only_while_selecting,
        ),
        (
            "code_view_shift_click_extends",
            test_code_view_shift_click_extends,
        ),
        (
            "tree_click_on_the_twisty_toggles_rather_than_opens",
            test_tree_click_on_the_twisty_toggles_rather_than_opens,
        ),
        (
            "tree_keys_only_reach_a_focused_tree",
            test_tree_keys_only_reach_a_focused_tree,
        ),
        (
            "editor_tabs_close_box_is_distinct_from_the_tab",
            test_editor_tabs_close_box_is_distinct_from_the_tab,
        ),
        (
            "drag_handle_reports_begin_move_end",
            test_drag_handle_reports_begin_move_end,
        ),
        (
            "a_release_outside_a_widget_still_reaches_it",
            test_a_release_outside_a_widget_still_reaches_it,
        ),
        (
            "a_zero_extent_clip_damages_nothing",
            test_a_zero_extent_clip_damages_nothing,
        ),
        (
            "scroll_goes_to_what_is_under_the_pointer",
            test_scroll_goes_to_what_is_under_the_pointer,
        ),
        (
            "a_popup_swallows_a_press_its_child_ignored",
            test_a_popup_swallows_a_press_its_child_ignored,
        ),
        (
            "only_the_visible_tab_panel_is_reachable",
            test_only_the_visible_tab_panel_is_reachable,
        ),
        (
            "line_edit_reports_keys_only_when_focused",
            test_line_edit_reports_keys_only_when_focused,
        ),
        ("text_field_types_a_space", test_text_field_types_a_space),
        (
            "card_gives_its_child_the_whole_rect",
            test_card_gives_its_child_the_whole_rect,
        ),
        ("elide_keeps_what_fits", test_elide_keeps_what_fits),
        ("tight_constraints", test_tight_constraints),
        ("loose_constraints", test_loose_constraints),
        ("constrain_clamps", test_constrain_clamps),
        ("deflate", test_deflate),
        ("unbounded", test_unbounded),
        ("rect_contains", test_rect_contains),
        ("rect_intersect", test_rect_intersect),
        ("rect_no_intersect", test_rect_no_intersect),
        ("vstack_measure", test_vstack_measure),
        ("hstack_measure", test_hstack_measure),
        ("padding_measure", test_padding_measure),
        ("spacer_measure", test_spacer_measure),
        ("focus_next", test_focus_next),
        ("focus_prev", test_focus_prev),
        ("focus_wrap", test_focus_wrap),
        ("focus_scope", test_focus_scope),
        ("hit_test_leaf", test_hit_test_leaf),
        ("hit_test_miss", test_hit_test_miss),
        ("edge_insets_symmetric", test_edge_insets_symmetric),
        ("box_constraints_loosen", test_box_constraints_loosen),
        ("deflate_unbounded", test_deflate_unbounded),
        (
            "table_right_click_emits_context_menu",
            test_table_right_click_emits_context_menu,
        ),
        (
            "table_right_click_selects_row",
            test_table_right_click_selects_row,
        ),
        (
            "table_left_click_emits_no_context_menu",
            test_table_left_click_emits_no_context_menu,
        ),
        (
            "table_right_click_header_is_inert",
            test_table_right_click_header_is_inert,
        ),
        (
            "table_menu_key_emits_context_menu",
            test_table_menu_key_emits_context_menu,
        ),
        (
            "table_menu_key_without_selection_is_inert",
            test_table_menu_key_without_selection_is_inert,
        ),
        (
            "popup_places_child_at_anchor",
            test_popup_places_child_at_anchor,
        ),
        ("popup_flips_at_edges", test_popup_flips_at_edges),
        (
            "popup_clamps_oversized_child",
            test_popup_clamps_oversized_child,
        ),
        (
            "popup_click_outside_dismisses",
            test_popup_click_outside_dismisses,
        ),
        (
            "popup_click_inside_does_not_dismiss",
            test_popup_click_inside_does_not_dismiss,
        ),
        ("popup_escape_dismisses", test_popup_escape_dismisses),
        (
            "popup_swallows_unhandled_events",
            test_popup_swallows_unhandled_events,
        ),
        (
            "dialog_places_children_inside_card",
            test_dialog_places_children_inside_card,
        ),
        (
            "dialog_card_height_covers_content",
            test_dialog_card_height_covers_content,
        ),
        ("dialog_card_is_centered", test_dialog_card_is_centered),
        (
            "dialog_click_routes_to_action_under_pointer",
            test_dialog_click_routes_to_action_under_pointer,
        ),
        (
            "dialog_backdrop_click_dismisses",
            test_dialog_backdrop_click_dismisses,
        ),
        (
            "dialog_is_modal_over_its_parent",
            test_dialog_is_modal_over_its_parent,
        ),
        (
            "unbounded_extent_survives_padding_arithmetic",
            test_unbounded_extent_survives_padding_arithmetic,
        ),
        (
            "deflate_preserves_unboundedness",
            test_deflate_preserves_unboundedness,
        ),
        (
            "measure_widget_records_size",
            test_measure_widget_records_size,
        ),
        ("place_widget_records_rect", test_place_widget_records_rect),
        (
            "zstack_layers_share_the_full_rect",
            test_zstack_layers_share_the_full_rect,
        ),
        (
            "dialog_enter_without_selection_fires_nothing",
            test_dialog_enter_without_selection_fires_nothing,
        ),
        (
            "dialog_keyboard_selects_then_activates",
            test_dialog_keyboard_selects_then_activates,
        ),
        ("dialog_escape_dismisses", test_dialog_escape_dismisses),
    ]
}

pub fn run_all_tests() -> bool {
    let tests = cases();

    let mut passed = 0usize;
    let mut failed = 0usize;

    for (name, func) in tests {
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| func())).is_ok();
        if ok {
            passed += 1;
        } else {
            eprintln!("[FAIL] ui::tests::{}", name);
            failed += 1;
        }
    }

    eprintln!(
        "[ui::tests] {} passed, {} failed, {} total",
        passed,
        failed,
        passed + failed
    );
    failed == 0
}

#[cfg(test)]
mod cfg_tests {
    use super::*;

    #[test]
    fn tight_constraints() {
        test_tight_constraints();
    }
    #[test]
    fn loose_constraints() {
        test_loose_constraints();
    }
    #[test]
    fn constrain_clamps() {
        test_constrain_clamps();
    }
    #[test]
    fn deflate() {
        test_deflate();
    }
    #[test]
    fn unbounded() {
        test_unbounded();
    }
    #[test]
    fn rect_contains() {
        test_rect_contains();
    }
    #[test]
    fn rect_intersect() {
        test_rect_intersect();
    }
    #[test]
    fn rect_no_intersect() {
        test_rect_no_intersect();
    }
    #[test]
    fn vstack_measure() {
        test_vstack_measure();
    }
    #[test]
    fn hstack_measure() {
        test_hstack_measure();
    }
    #[test]
    fn padding_measure() {
        test_padding_measure();
    }
    #[test]
    fn spacer_measure() {
        test_spacer_measure();
    }
    #[test]
    fn focus_next() {
        test_focus_next();
    }
    #[test]
    fn focus_prev() {
        test_focus_prev();
    }
    #[test]
    fn focus_wrap() {
        test_focus_wrap();
    }
    #[test]
    fn focus_scope() {
        test_focus_scope();
    }
    #[test]
    fn hit_test_leaf() {
        test_hit_test_leaf();
    }
    #[test]
    fn hit_test_miss() {
        test_hit_test_miss();
    }
    #[test]
    fn edge_insets_symmetric() {
        test_edge_insets_symmetric();
    }
    #[test]
    fn box_constraints_loosen() {
        test_box_constraints_loosen();
    }
    #[test]
    fn deflate_unbounded() {
        test_deflate_unbounded();
    }
    #[test]
    fn table_right_click_emits_context_menu() {
        test_table_right_click_emits_context_menu();
    }
    #[test]
    fn table_right_click_selects_row() {
        test_table_right_click_selects_row();
    }
    #[test]
    fn table_left_click_emits_no_context_menu() {
        test_table_left_click_emits_no_context_menu();
    }
    #[test]
    fn table_right_click_header_is_inert() {
        test_table_right_click_header_is_inert();
    }
    #[test]
    fn table_menu_key_emits_context_menu() {
        test_table_menu_key_emits_context_menu();
    }
    #[test]
    fn table_menu_key_without_selection_is_inert() {
        test_table_menu_key_without_selection_is_inert();
    }
    #[test]
    fn popup_places_child_at_anchor() {
        test_popup_places_child_at_anchor();
    }
    #[test]
    fn popup_flips_at_edges() {
        test_popup_flips_at_edges();
    }
    #[test]
    fn popup_clamps_oversized_child() {
        test_popup_clamps_oversized_child();
    }
    #[test]
    fn popup_click_outside_dismisses() {
        test_popup_click_outside_dismisses();
    }
    #[test]
    fn popup_click_inside_does_not_dismiss() {
        test_popup_click_inside_does_not_dismiss();
    }
    #[test]
    fn popup_escape_dismisses() {
        test_popup_escape_dismisses();
    }
    #[test]
    fn popup_swallows_unhandled_events() {
        test_popup_swallows_unhandled_events();
    }
    #[test]
    fn dialog_places_children_inside_card() {
        test_dialog_places_children_inside_card();
    }
    #[test]
    fn dialog_card_height_covers_content() {
        test_dialog_card_height_covers_content();
    }
    #[test]
    fn dialog_card_is_centered() {
        test_dialog_card_is_centered();
    }
    #[test]
    fn dialog_click_routes_to_action_under_pointer() {
        test_dialog_click_routes_to_action_under_pointer();
    }
    #[test]
    fn dialog_backdrop_click_dismisses() {
        test_dialog_backdrop_click_dismisses();
    }
    #[test]
    fn dialog_is_modal_over_its_parent() {
        test_dialog_is_modal_over_its_parent();
    }
    #[test]
    fn unbounded_extent_survives_padding_arithmetic() {
        test_unbounded_extent_survives_padding_arithmetic();
    }
    #[test]
    fn deflate_preserves_unboundedness() {
        test_deflate_preserves_unboundedness();
    }
    #[test]
    fn measure_widget_records_size() {
        test_measure_widget_records_size();
    }
    #[test]
    fn place_widget_records_rect() {
        test_place_widget_records_rect();
    }
    #[test]
    fn zstack_layers_share_the_full_rect() {
        test_zstack_layers_share_the_full_rect();
    }
    #[test]
    fn dialog_enter_without_selection_fires_nothing() {
        test_dialog_enter_without_selection_fires_nothing();
    }
    #[test]
    fn dialog_keyboard_selects_then_activates() {
        test_dialog_keyboard_selects_then_activates();
    }
    #[test]
    fn dialog_escape_dismisses() {
        test_dialog_escape_dismisses();
    }
}
