use std::any::Any;

use slopos_theme_core::{TextRole, Tone};

use super::components::{Button, ButtonRow, Text};
use super::constraints::{BoxConstraints, Rect, Size};
use super::event::{EventPhase, Key, MessageSink, Modifiers, WidgetEvent};
use super::node::{ButtonStyle, Node};
use super::paint::PaintContext;
use super::style::StyleSheet;
use super::traits::{MeasureCtx, Scrolled, Widget, measure_widget, place_widget};

/// Build a retained widget tree from a declarative `Node<M>` tree, bridging the
/// app's message type `M` into the widgets' type-erased callback storage.
pub fn build_widget_tree<M: Clone + 'static>(node: &Node<M>) -> Box<dyn Widget> {
    use super::widgets;

    match node {
        Node::Text(text) => Box::new(widgets::text::TextWidget::new(text)),
        Node::Icon { icon, role, tone } => {
            Box::new(widgets::icon::IconWidget::new(*icon, *role, *tone))
        }
        Node::IconButton(b) => Box::new(widgets::icon_button::IconButtonWidget::new(
            b.icon,
            b.label.clone(),
            erase(&b.on_press),
            b.enabled,
            b.active,
        )),
        Node::ButtonRow(row) => build_button_row(row, row.ends),
        Node::PropertyList(rows) => {
            let mut cells: Vec<Box<dyn Widget>> = Vec::with_capacity(rows.len() * 2);
            for (label, value) in rows {
                let name = Text::new(label.clone()).tone(Tone::Secondary).wrap();
                cells.push(Box::new(widgets::text::TextWidget::new(&name)));
                cells.push(build_widget_tree(value));
            }
            Box::new(widgets::property_list::PropertyListWidget::new(cells))
        }
        Node::List(list) => Box::new(widgets::list::ListWidget::new(
            list.sections.clone(),
            list.selected,
            erase_with(list.on_select),
            erase_with(list.on_activate),
        )),
        Node::Tag(tag) => Box::new(widgets::tag::TagWidget::new(tag.text.clone(), tag.tone)),
        Node::Disclosure(d) => Box::new(widgets::disclosure::DisclosureWidget::new(
            d.title.clone(),
            d.expanded,
            erase(&d.on_toggle),
            d.expanded.then(|| build_widget_tree(&d.content)),
        )),
        Node::Button {
            label,
            on_press,
            style,
            enabled,
        } => Box::new(widgets::button::ButtonWidget::new(
            label.clone(),
            erase(on_press),
            *style,
            *enabled,
        )),
        Node::TextField {
            text,
            placeholder,
            on_change,
            max_length,
            read_only,
        } => Box::new(widgets::text_field::TextFieldWidget::new(
            text.clone(),
            placeholder.clone(),
            erase_with(*on_change),
            *max_length,
            *read_only,
        )),
        Node::Checkbox {
            checked,
            label,
            on_toggle,
            enabled,
        } => Box::new(widgets::checkbox::CheckboxWidget::new(
            *checked,
            label.clone(),
            erase(on_toggle),
            *enabled,
        )),
        Node::RadioGroup {
            options,
            selected,
            on_select,
        } => Box::new(widgets::radio_group::RadioGroupWidget::new(
            options.clone(),
            *selected,
            erase_with(*on_select),
        )),
        Node::Slider {
            value,
            min,
            max,
            step,
            enabled,
            grab,
            on_input,
        } => Box::new(widgets::slider::SliderWidget::new(
            *value,
            *min,
            *max,
            *step,
            *enabled,
            *grab,
            erase_with(*on_input),
        )),
        Node::Divider => Box::new(widgets::separator::SeparatorWidget::new()),
        Node::Image {
            image,
            scale,
            sampling,
        } => Box::new(widgets::image::ImageWidget::new(
            image.clone(),
            *scale,
            *sampling,
        )),
        Node::ScrollView {
            child,
            direction,
            show_scrollbar,
            scroll_y,
            on_scroll,
            follow_end,
        } => {
            let child_widget = build_widget_tree(child);
            Box::new(
                widgets::scroll_view::ScrollViewWidget::with_scroll(
                    child_widget,
                    *direction,
                    *show_scrollbar,
                    *scroll_y,
                    erase_with(*on_scroll),
                )
                .following_end(*follow_end),
            )
        }
        Node::ListView {
            item_height,
            selected,
            on_select,
            items,
        } => {
            let item_widgets: Vec<Box<dyn Widget>> =
                items.iter().map(|n| build_widget_tree(n)).collect();
            Box::new(widgets::list_view::ListViewWidget::new(
                *item_height,
                *selected,
                erase_with(*on_select),
                item_widgets,
            ))
        }
        Node::TabBar {
            tabs,
            active,
            on_change,
            content,
        } => {
            let content_widgets: Vec<Box<dyn Widget>> =
                content.iter().map(|n| build_widget_tree(n)).collect();
            Box::new(widgets::tab_bar::TabBarWidget::new(
                tabs.clone(),
                *active,
                erase_with(*on_change),
                content_widgets,
            ))
        }
        Node::Menu { items, on_action } => Box::new(widgets::menu::MenuWidget::new(
            items.clone(),
            erase_with(*on_action),
        )),
        Node::Table {
            columns,
            rows,
            row_height,
            selected,
            on_select,
            on_header_click,
            on_context_menu,
            shows,
        } => {
            let row_widgets: Vec<Vec<Box<dyn Widget>>> = rows
                .iter()
                .map(|row| row.iter().map(|n| build_widget_tree(n)).collect())
                .collect();
            Box::new(
                widgets::table::TableWidget::new(
                    columns.clone(),
                    row_widgets,
                    *row_height,
                    *selected,
                    erase_with(*on_select),
                    erase_with(*on_header_click),
                    erase_with(*on_context_menu),
                )
                .showing(*shows),
            )
        }
        Node::Dialog {
            title,
            content,
            footer,
            on_dismiss,
        } => {
            let title = Text::new(title.clone()).role(TextRole::Title).wrap();
            Box::new(widgets::dialog::DialogWidget::new(
                Box::new(widgets::text::TextWidget::new(&title)),
                build_widget_tree(content),
                build_button_row(footer, true),
                erase(on_dismiss),
            ))
        }
        Node::Popup {
            x,
            y,
            child,
            on_dismiss,
        } => {
            let child_widget = build_widget_tree(child);
            Box::new(widgets::popup::PopupWidget::new(
                *x,
                *y,
                child_widget,
                erase(on_dismiss),
            ))
        }
        Node::ProgressBar { value, label, tone } => Box::new(
            widgets::progress_bar::ProgressBarWidget::new(*value, label.clone(), *tone),
        ),
        Node::PartitionBar {
            segments,
            highlight,
        } => Box::new(widgets::partition_bar::PartitionBarWidget::new(
            segments.clone(),
            *highlight,
        )),
        Node::VStack {
            children,
            spacing,
            align,
        } => {
            let child_widgets: Vec<Box<dyn Widget>> =
                children.iter().map(|n| build_widget_tree(n)).collect();
            Box::new(super::layout::VStackWidget::new(
                child_widgets,
                *spacing,
                *align,
            ))
        }
        Node::HStack {
            children,
            spacing,
            align,
        } => {
            let child_widgets: Vec<Box<dyn Widget>> =
                children.iter().map(|n| build_widget_tree(n)).collect();
            Box::new(super::layout::HStackWidget::new(
                child_widgets,
                *spacing,
                *align,
            ))
        }
        Node::ZStack { children } => {
            let child_widgets: Vec<Box<dyn Widget>> =
                children.iter().map(|n| build_widget_tree(n)).collect();
            Box::new(super::layout::ZStackWidget::new(child_widgets))
        }
        Node::Padding { padding, child } => {
            let child_widget = build_widget_tree(child);
            Box::new(super::layout::PaddingWidget::new(*padding, child_widget))
        }
        Node::Spacer { size } => Box::new(super::layout::SpacerWidget::new(*size)),
        Node::Expand { weight, child } => {
            let child_widget = build_widget_tree(child);
            Box::new(super::layout::ExpandWidget::new(*weight, child_widget))
        }
        Node::Background { surface, child } => {
            let child_widget = build_widget_tree(child);
            Box::new(super::layout::BackgroundWidget::new(*surface, child_widget))
        }
        Node::Card {
            surface,
            border,
            shadow,
            child,
        } => {
            let child_widget = build_widget_tree(child);
            Box::new(widgets::card::CardWidget::new(
                *surface,
                *border,
                *shadow,
                child_widget,
            ))
        }
        Node::SizedBox {
            width,
            height,
            child,
        } => {
            let child_widget = build_widget_tree(child);
            Box::new(super::layout::SizedBoxWidget::new(
                *width,
                *height,
                child_widget,
            ))
        }
        Node::CodeView {
            lines,
            first_line,
            total_lines,
            first_col,
            tab_width,
            cursor,
            selection,
            show_line_numbers,
            focused,
            selecting,
            scroll_dragging,
            on_input,
        } => Box::new(widgets::code_view::CodeViewWidget::new(
            lines.clone(),
            *first_line,
            *total_lines,
            *first_col,
            *tab_width,
            *cursor,
            *selection,
            *show_line_numbers,
            *focused,
            *selecting,
            *scroll_dragging,
            erase_with(*on_input),
        )),
        Node::TreeView {
            rows,
            first_row,
            total_rows,
            selected,
            focused,
            scroll_dragging,
            on_input,
        } => Box::new(widgets::tree_view::TreeViewWidget::new(
            rows.clone(),
            *first_row,
            *total_rows,
            *selected,
            *focused,
            *scroll_dragging,
            erase_with(*on_input),
        )),
        Node::EditorTabs {
            tabs,
            active,
            on_input,
        } => Box::new(widgets::editor_tabs::EditorTabsWidget::new(
            tabs.clone(),
            *active,
            erase_with(*on_input),
        )),
        Node::MenuBar {
            titles,
            open,
            on_input,
        } => Box::new(widgets::menu_bar::MenuBarWidget::new(
            titles.clone(),
            *open,
            erase_with(*on_input),
        )),
        Node::LineEdit {
            text,
            placeholder,
            caret,
            focused,
            icon,
            suffix,
            invalid,
            tab_stop,
            on_input,
        } => Box::new(
            widgets::line_edit::LineEditWidget::new(
                text.clone(),
                placeholder.clone(),
                *caret,
                *focused,
                *icon,
                suffix.clone(),
                *invalid,
                erase_with(*on_input),
            )
            .with_tab_stop(*tab_stop),
        ),
        Node::DragHandle {
            orientation,
            active,
            on_drag,
        } => Box::new(widgets::drag_handle::DragHandleWidget::new(
            *orientation,
            *active,
            erase_with(*on_drag),
        )),
        Node::Canvas { .. } | Node::Empty => Box::new(super::layout::SpacerWidget::new(
            super::constraints::Length::Px(0),
        )),
    }
}

type Emit = Box<dyn Fn() -> Box<dyn Any>>;

fn erase<M: Clone + 'static>(msg: &Option<M>) -> Option<Emit> {
    msg.clone()
        .map(|m| Box::new(move || Box::new(m.clone()) as Box<dyn Any>) as Emit)
}

fn erase_with<M: 'static, A: 'static>(
    f: Option<fn(A) -> M>,
) -> Option<Box<dyn Fn(A) -> Box<dyn Any>>> {
    f.map(|f| {
        Box::new(move |a: A| Box::new(f(a)) as Box<dyn Any>) as Box<dyn Fn(A) -> Box<dyn Any>>
    })
}

pub(crate) fn build_button_row<M: Clone + 'static>(
    row: &ButtonRow<M>,
    ends: bool,
) -> Box<dyn Widget> {
    let buttons = row
        .leading
        .iter()
        .chain(&row.trailing)
        .map(|b| build_widget_tree(&Node::from(b.clone())))
        .collect();
    let pressable = |b: &Button<M>| b.enabled.then(|| erase(&b.on_press)).flatten();
    let on_enter = row
        .default
        .filter(|_| ends)
        .and_then(|i| row.trailing.get(i))
        .filter(|b| b.style != ButtonStyle::Destructive)
        .and_then(pressable);
    let on_escape = row
        .escape
        .filter(|_| ends)
        .and_then(|(leading, i)| {
            if leading {
                row.leading.get(i)
            } else {
                row.trailing.get(i)
            }
        })
        .map(pressable);
    Box::new(super::widgets::button_row::ButtonRowWidget::new(
        buttons,
        row.leading.len(),
        on_enter,
        on_escape,
    ))
}

/// Where each scroll view the app does not steer has scrolled to, in tree
/// order.
pub(crate) fn kept_scrolls(widget: &dyn Widget, kept: &mut Vec<Scrolled>) {
    kept.extend(widget.kept_scroll());
    for child in widget.children() {
        kept_scrolls(child.as_ref(), kept);
    }
}

/// Hands each of a rebuilt tree's scroll views the offset of the next one in
/// `kept` that showed the same, in tree order, so a dialog opened or closed
/// between them moves none; its layout then clamps each to what it holds.
pub(crate) fn restore_scrolls(root: &mut dyn Widget, kept: &[Scrolled]) {
    fn walk(widget: &mut dyn Widget, kept: &[Scrolled], next: &mut usize) {
        if let Some(now) = widget.kept_scroll()
            && let Some(at) = kept[*next..].iter().position(|k| k.shows == now.shows)
        {
            widget.keep_scroll(kept[*next + at].offset);
            *next += at + 1;
        }
        for child in widget.children_mut() {
            walk(child.as_mut(), kept, next);
        }
    }
    walk(root, kept, &mut 0);
}

/// Offers a key nothing took to each widget of the modal layer, as Enter
/// reaches a page's default button.
pub fn offer_unhandled(
    root: &mut dyn Widget,
    key: Key,
    modifiers: Modifiers,
    sink: &mut MessageSink,
) -> bool {
    fn walk(
        widget: &mut dyn Widget,
        key: Key,
        modifiers: Modifiers,
        sink: &mut MessageSink,
    ) -> bool {
        widget.take_unhandled_key(key, modifiers, sink)
            || widget
                .children_mut()
                .iter_mut()
                .any(|child| walk(child.as_mut(), key, modifiers, sink))
    }
    walk(super::focus::modal_layer_mut(root), key, modifiers, sink)
}

/// Sends `event` to the widget `id` names; answers whether one did.
pub(crate) fn send_to_id(
    widget: &mut dyn Widget,
    id: super::traits::WidgetId,
    event: &WidgetEvent,
    sink: &mut MessageSink,
) -> bool {
    if widget.id() == id {
        widget.event(event, EventPhase::Target, sink);
        return true;
    }
    widget
        .children_mut()
        .iter_mut()
        .any(|child| send_to_id(child.as_mut(), id, event, sink))
}

pub fn layout_tree(root: &mut dyn Widget, window_size: Size, style: &StyleSheet) {
    let constraints = BoxConstraints::tight(window_size);
    let mut ctx = MeasureCtx { style };
    measure_widget(root, constraints, &mut ctx);
    place_widget(root, Rect::new(0, 0, window_size.width, window_size.height));
}

pub fn paint_tree(root: &dyn Widget, ctx: &mut PaintContext) {
    root.paint(ctx);
}
