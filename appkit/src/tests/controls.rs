use std::any::Any;

use slopos_abi::draw::Color32;
use slopos_abi::pixel::PixelFormat;
use slopos_gfx::DrawBuffer;

use super::{FixedSizeWidget, key};
use crate::constraints::{BoxConstraints, CrossAxisAlignment, Rect, Size};
use crate::event::{EventPhase, MessageSink, NamedKey, PointerButton, WidgetEvent};
use crate::focus::FocusManager;
use crate::layout::VStackWidget;
use crate::node::{BarSegment, RadioOption, SegmentFill};
use crate::paint::PaintContext;
use crate::sender::AppSender;
use crate::style::StyleSheet;
use crate::traits::{FocusPolicy, MeasureCtx, Widget, WidgetId, measure_widget, place_widget};
use crate::widgets::partition_bar::{MIN_SEGMENT_WIDTH, PartitionBarWidget, segment_widths};
use crate::widgets::progress_bar::sweep;
use crate::widgets::radio_group::RadioGroupWidget;
use crate::widgets::slider::{Range, SliderGrab, SliderInput, SliderWidget};
use slopos_windowing::UiQueue;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn lay_out(widget: &mut dyn Widget, constraints: BoxConstraints, at: (i32, i32)) -> Size {
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    let size = measure_widget(widget, constraints, &mut ctx);
    place_widget(widget, Rect::new(at.0, at.1, size.width, size.height));
    size
}

fn deliver(widget: &mut dyn Widget, event: WidgetEvent) -> (bool, MessageSink) {
    let mut sink = MessageSink::new();
    let consumed = widget
        .event(&event, EventPhase::Target, &mut sink)
        .is_consumed();
    (consumed, sink)
}

fn press(x: i32, y: i32) -> WidgetEvent {
    WidgetEvent::PointerDown {
        x,
        y,
        button: PointerButton::Left,
        modifiers: Default::default(),
    }
}

fn option(label: &str, detail: &str, enabled: bool) -> RadioOption {
    RadioOption {
        label: String::from(label),
        detail: String::from(detail),
        enabled,
    }
}

fn radio_group(options: Vec<RadioOption>, selected: Option<usize>) -> RadioGroupWidget {
    let mut group = RadioGroupWidget::new(
        options,
        selected,
        Some(Box::new(|i: usize| Box::new(i) as Box<dyn Any>)),
    );
    lay_out(
        &mut group,
        BoxConstraints::loose(Size::new(400, 400)),
        (10, 20),
    );
    group
}

fn focused(widget: &mut dyn Widget) {
    widget.event(
        &WidgetEvent::FocusGained,
        EventPhase::Target,
        &mut MessageSink::new(),
    );
}

fn chosen(sink: &mut MessageSink) -> Vec<usize> {
    sink.drain_typed::<usize>()
}

/// Rows stack in order with the group's spacing; a detail is a second line
/// under its label.
pub(super) fn test_radio_group_stacks_its_options() {
    let style = StyleSheet::dark();
    let line = crate::text::ui::line_height(style.font_size as u16);
    let label = style.checkbox_size.max(line);
    let mut group = radio_group(
        vec![
            option("Erase", "", true),
            option("Reinstall", "keeps what nvme0n1p3 holds", true),
            option("Reuse", "", true),
        ],
        Some(0),
    );
    let size = group.measured_size();
    assert_eq!(size.height, label * 3 + line + style.spacing_sm * 2);
    assert!(size.width > style.checkbox_size + style.checkbox_gap);
    assert_eq!(
        group.layout_rect(),
        Rect::new(10, 20, size.width, size.height)
    );

    let detail_bottom = 20 + label * 2 + line + style.spacing_sm - 1;
    let (consumed, mut sink) = deliver(&mut group, press(12, detail_bottom));
    assert!(consumed);
    assert_eq!(chosen(&mut sink), [1]);
    let (consumed, _) = deliver(&mut group, press(12, detail_bottom + 1));
    assert!(!consumed, "the gap between options is no option's");
    let third_row = 20 + label * 2 + line + style.spacing_sm * 2;
    let (consumed, mut sink) = deliver(&mut group, press(12, third_row + 1));
    assert!(consumed);
    assert_eq!(chosen(&mut sink), [2]);
    assert_eq!(group.selected(), Some(2));
}

/// A disabled option takes no click, and the press goes no further.
pub(super) fn test_radio_group_click_on_a_disabled_option_chooses_nothing() {
    let style = StyleSheet::dark();
    let line = crate::text::ui::line_height(style.font_size as u16);
    let label = style.checkbox_size.max(line);
    let mut group = radio_group(
        vec![
            option("Erase", "", true),
            option("Beside", "too small", false),
        ],
        Some(0),
    );
    let (consumed, mut sink) = deliver(&mut group, press(12, 20 + label + style.spacing_sm + 1));
    assert!(consumed);
    assert!(chosen(&mut sink).is_empty());
    assert_eq!(group.selected(), Some(0));
}

/// The arrows move the choice, skipping disabled options and wrapping at
/// both ends.
pub(super) fn test_radio_group_arrows_skip_disabled_and_wrap() {
    let mut group = radio_group(
        vec![
            option("a", "", true),
            option("b", "", false),
            option("c", "", true),
        ],
        Some(0),
    );
    focused(&mut group);
    let steps = [
        (NamedKey::Down, 2),
        (NamedKey::Down, 0),
        (NamedKey::Up, 2),
        (NamedKey::Left, 0),
        (NamedKey::Right, 2),
    ];
    for (named, expected) in steps {
        let (consumed, mut sink) = deliver(&mut group, key(named, false));
        assert!(consumed);
        assert_eq!(chosen(&mut sink), [expected]);
        assert_eq!(group.selected(), Some(expected));
    }
}

/// Keys carry no position, so a group that is not focused leaves them to
/// whatever is.
pub(super) fn test_radio_group_keys_only_reach_a_focused_group() {
    let mut group = radio_group(vec![option("a", "", true), option("b", "", true)], Some(0));
    let (consumed, mut sink) = deliver(&mut group, key(NamedKey::Down, false));
    assert!(!consumed);
    assert!(chosen(&mut sink).is_empty());
    assert_eq!(group.selected(), Some(0));
}

/// With nothing chosen, Space chooses the first option that can be; with
/// one chosen it changes nothing.
pub(super) fn test_radio_group_space_picks_the_first_enabled() {
    let mut group = radio_group(
        vec![
            option("a", "", false),
            option("b", "", true),
            option("c", "", true),
        ],
        None,
    );
    focused(&mut group);
    let (consumed, mut sink) = deliver(&mut group, key(NamedKey::Space, false));
    assert!(consumed);
    assert_eq!(chosen(&mut sink), [1]);
    let (consumed, mut sink) = deliver(&mut group, key(NamedKey::Space, false));
    assert!(consumed);
    assert!(chosen(&mut sink).is_empty());
}

/// The group is one tab stop however many options it has, and none when no
/// option can be chosen.
pub(super) fn test_radio_group_is_one_tab_stop() {
    let chain = |options: Vec<RadioOption>| {
        let children: Vec<Box<dyn Widget>> = vec![
            Box::new(FixedSizeWidget::focusable(10, 10)),
            Box::new(RadioGroupWidget::new(options, None, None)),
            Box::new(FixedSizeWidget::focusable(10, 10)),
        ];
        let stack = VStackWidget::new(children, 0, CrossAxisAlignment::Start);
        let mut focus = FocusManager::new();
        focus.rebuild_tab_chain(&stack);
        focus.move_focus_next();
        let first = focus.focused();
        (1..)
            .find(|_| {
                focus.move_focus_next();
                focus.focused() == first
            })
            .unwrap()
    };
    let three = vec![
        option("a", "", true),
        option("b", "", true),
        option("c", "", true),
    ];
    assert_eq!(chain(three), 3);
    assert_eq!(
        chain(vec![option("a", "", false), option("b", "", false)]),
        2
    );
}

/// Values are `min + k * step` and `max`: a snap goes to the nearest, a step
/// to the next one along, a page a tenth of the range on.
pub(super) fn test_slider_range_snaps_and_steps_on_its_grid() {
    let range = Range::new(1, 11, 3);
    assert_eq!(range.snap(0), 1);
    assert_eq!(range.snap(5), 4);
    assert_eq!(range.snap(6), 7);
    assert_eq!(range.snap(10), 10);
    assert_eq!(range.snap(11), 11);
    assert_eq!(range.snap(99), 11);

    assert_eq!(range.moved(1, 1, false), 4);
    assert_eq!(range.moved(10, 1, false), 11);
    assert_eq!(range.moved(11, -1, false), 10);
    assert_eq!(range.moved(5, 1, false), 7);
    assert_eq!(range.moved(5, -1, false), 4);
    assert_eq!(range.moved(4, -1, false), 1);
    assert_eq!(range.moved(1, -1, false), 1);
    assert_eq!(range.moved(4, 100, false), 11);

    let wide = Range::new(0, 1000, 5);
    assert_eq!(wide.moved(500, 1, true), 600);
    assert_eq!(wide.moved(950, 1, true), 1000);
    assert_eq!(wide.moved(50, -1, true), 0);
    // A range of fewer than ten steps pages by one.
    assert_eq!(range.moved(4, 1, true), 7);
    // A step of zero is a step of one, and a maximum under the minimum is
    // the minimum.
    assert_eq!(Range::new(5, 2, 0).snap(9), 5);
    assert_eq!(Range::new(0, 4, 0).moved(2, 1, false), 3);
}

fn slider(value: u64, grab: Option<SliderGrab>) -> SliderWidget {
    let mut slider = SliderWidget::new(
        value,
        0,
        100,
        5,
        true,
        grab,
        Some(Box::new(|i: SliderInput| Box::new(i) as Box<dyn Any>)),
    );
    let style = StyleSheet::dark();
    let width = 200 + style.checkbox_size;
    lay_out(
        &mut slider,
        BoxConstraints::loose(Size::new(width, 100)),
        (0, 0),
    );
    slider
}

fn inputs(sink: &mut MessageSink) -> Vec<SliderInput> {
    sink.drain_typed::<SliderInput>()
}

/// The track spans the width less the knob, so both ends of the range are
/// reachable with the knob wholly inside.
pub(super) fn test_slider_lays_its_track_inside_its_width() {
    let style = StyleSheet::dark();
    let half = style.checkbox_size / 2;
    let s = slider(25, None);
    assert_eq!(s.measured_size().width, 200 + style.checkbox_size);
    assert_eq!(
        s.measured_size().height,
        style.checkbox_size + (style.focus_ring_width + style.focus_ring_offset) * 2
    );
    assert_eq!(s.thumb_x(), half + 50);
    assert_eq!(slider(0, None).thumb_x(), half);
    assert_eq!(slider(100, None).thumb_x(), half + 200);
    assert_eq!(s.value_at(half + 100), 50);
    assert_eq!(s.value_at(-50), 0);
    assert_eq!(s.value_at(10_000), 100);
    // Snapped to the grid: 103 px is 51.5, nearest 50.
    assert_eq!(s.value_at(half + 103), 50);
    assert_eq!(s.value_at(half + 106), 55);
}

fn moves(slider: &mut SliderWidget, xs: &[i32]) -> Vec<SliderInput> {
    xs.iter()
        .flat_map(|&x| inputs(&mut deliver(slider, WidgetEvent::PointerMove { x, y: 50 }).1))
        .collect()
}

/// A press begins a drag at the value under the pointer and the widget it
/// reached follows the moves until the release, which can reach it before
/// the application's answer to the press rebuilds it; a widget built
/// dragging follows them too.
pub(super) fn test_slider_press_drag_and_release() {
    let half = StyleSheet::dark().checkbox_size / 2;
    let release = WidgetEvent::PointerUp {
        x: 1000,
        y: 1000,
        button: PointerButton::Left,
    };
    let mut s = slider(25, None);
    let (consumed, mut sink) = deliver(&mut s, press(half + 150, 5));
    assert!(consumed);
    assert_eq!(inputs(&mut sink), [SliderInput::Begin(75, SliderGrab(0))]);
    assert_eq!(
        moves(&mut s, &[half + 20, half + 21, half + 400]),
        [SliderInput::Change(10), SliderInput::Change(100)]
    );
    let (consumed, mut sink) = deliver(&mut s, release.clone());
    assert!(consumed);
    assert_eq!(inputs(&mut sink), [SliderInput::End]);
    assert!(moves(&mut s, &[half + 10]).is_empty());
    let (consumed, _) = deliver(&mut s, release.clone());
    assert!(!consumed, "a release with no drag live is someone else's");

    let mut rebuilt = slider(75, Some(SliderGrab(0)));
    assert_eq!(moves(&mut rebuilt, &[half + 20]), [SliderInput::Change(10)]);
    let (_, mut sink) = deliver(&mut rebuilt, release);
    assert_eq!(inputs(&mut sink), [SliderInput::End]);
}

/// A drag holds the knob where it was taken: a press on the knob keeps its
/// value, so a click there changes nothing, and neither a press on the track
/// nor a move then jumps the knob's centre to the pointer.
pub(super) fn test_slider_drag_holds_the_knob_where_it_was_taken() {
    let mut s = SliderWidget::new(
        25,
        0,
        100,
        1,
        true,
        None,
        Some(Box::new(|i: SliderInput| Box::new(i) as Box<dyn Any>)),
    );
    let width = 200 + StyleSheet::dark().checkbox_size;
    lay_out(&mut s, BoxConstraints::loose(Size::new(width, 100)), (0, 0));
    let x = s.thumb_x() + 5;
    assert_ne!(s.value_at(x), 25);
    let (consumed, mut sink) = deliver(&mut s, press(x, 5));
    assert!(consumed);
    assert_eq!(inputs(&mut sink), [SliderInput::Begin(25, SliderGrab(5))]);
    assert!(moves(&mut s, &[x]).is_empty());
    assert_eq!(moves(&mut s, &[x + 20]), [SliderInput::Change(35)]);

    let half = StyleSheet::dark().checkbox_size / 2;
    let mut track = slider(25, None);
    let (_, mut sink) = deliver(&mut track, press(half + 103, 5));
    assert_eq!(inputs(&mut sink), [SliderInput::Begin(50, SliderGrab(3))]);
    assert!(moves(&mut track, &[half + 106]).is_empty());
}

/// Arrows on either axis step, PageUp and PageDown page, Home and End go to
/// the ends; a key at an end changes nothing and still belongs to the slider.
pub(super) fn test_slider_keys_step_page_and_reach_the_ends() {
    let mut s = slider(50, None);
    let (consumed, _) = deliver(&mut s, key(NamedKey::Right, false));
    assert!(!consumed, "a slider without focus takes no key");
    focused(&mut s);
    let steps = [
        (NamedKey::Right, 55),
        (NamedKey::Up, 60),
        (NamedKey::Left, 55),
        (NamedKey::Down, 50),
        (NamedKey::PageUp, 60),
        (NamedKey::PageDown, 50),
        (NamedKey::End, 100),
        (NamedKey::Home, 0),
    ];
    for (named, value) in steps {
        let (consumed, mut sink) = deliver(&mut s, key(named, false));
        assert!(consumed);
        assert_eq!(inputs(&mut sink), [SliderInput::Change(value)]);
        assert_eq!(s.value(), value);
    }
    let (consumed, mut sink) = deliver(&mut s, key(NamedKey::Down, false));
    assert!(consumed);
    assert!(inputs(&mut sink).is_empty());
    let (consumed, _) = deliver(&mut s, key(NamedKey::Enter, false));
    assert!(!consumed);
}

/// A disabled slider is no tab stop and takes no press, but a release still
/// ends a drag it was disabled in the middle of.
pub(super) fn test_slider_disabled_takes_nothing_but_ends_a_drag() {
    let make = |grab: Option<SliderGrab>| {
        let mut s = SliderWidget::new(
            10,
            0,
            100,
            1,
            false,
            grab,
            Some(Box::new(|i: SliderInput| Box::new(i) as Box<dyn Any>)),
        );
        lay_out(&mut s, BoxConstraints::loose(Size::new(216, 40)), (0, 0));
        s
    };
    let mut s = make(None);
    assert_eq!(s.focus_policy(), FocusPolicy::None);
    let (consumed, mut sink) = deliver(&mut s, press(100, 5));
    assert!(!consumed);
    assert!(inputs(&mut sink).is_empty());
    let mut s = make(Some(SliderGrab(0)));
    let (_, mut sink) = deliver(&mut s, WidgetEvent::PointerMove { x: 150, y: 5 });
    assert!(inputs(&mut sink).is_empty());
    let (consumed, mut sink) = deliver(
        &mut s,
        WidgetEvent::PointerUp {
            x: 0,
            y: 0,
            button: PointerButton::Left,
        },
    );
    assert!(consumed);
    assert_eq!(inputs(&mut sink), [SliderInput::End]);
}

/// A range of bytes, more values than pixels, reaches both of its ends
/// exactly, and a press on its knob that moves only up or down leaves it be.
pub(super) fn test_slider_reaches_both_ends_of_a_range_of_bytes() {
    let (min, max) = (7 * GIB, 510 * GIB + 3 * MIB);
    let mut s = SliderWidget::new(
        200 * GIB,
        min,
        max,
        MIB,
        true,
        None,
        Some(Box::new(|i: SliderInput| Box::new(i) as Box<dyn Any>)),
    );
    lay_out(&mut s, BoxConstraints::loose(Size::new(700, 40)), (30, 0));
    let x = s.thumb_x();
    assert_ne!(s.value_at(x), 200 * GIB);
    let (_, mut sink) = deliver(&mut s, press(x, 10));
    assert_eq!(
        inputs(&mut sink),
        [SliderInput::Begin(200 * GIB, SliderGrab(0))]
    );
    let (_, mut sink) = deliver(&mut s, WidgetEvent::PointerMove { x, y: 11 });
    assert!(inputs(&mut sink).is_empty());
    let right = WidgetEvent::PointerUp {
        x,
        y: 11,
        button: PointerButton::Right,
    };
    let (consumed, _) = deliver(&mut s, right);
    assert!(!consumed, "only the left button's release ends the drag");
    assert_eq!(s.value_at(0), min);
    assert_eq!(s.value_at(10_000), max);
    let mid = s.value_at(30 + 350);
    assert_eq!((mid - min) % MIB, 0);
    focused(&mut s);
    deliver(&mut s, key(NamedKey::End, false));
    assert_eq!(s.value(), max);
    deliver(&mut s, key(NamedKey::Left, false));
    assert_eq!(s.value(), 510 * GIB + 2 * MIB);
}

/// Every segment gets the floor and the rest in proportion, one of no size
/// nothing; the widths add up to the bar's, and a narrow bar shares itself
/// out evenly.
pub(super) fn test_partition_bar_widths_keep_a_floor_and_add_up() {
    let sizes = [512 * MIB, 380 * GIB, 130 * GIB, 0];
    let widths = segment_widths(&sizes, 600);
    assert_eq!(widths.iter().sum::<i32>(), 600);
    assert!(widths[..3].iter().all(|&w| w >= MIN_SEGMENT_WIDTH));
    assert_eq!(widths[3], 0);
    assert!(widths[1] > widths[2] && widths[2] > widths[0]);
    let rest = 600 - MIN_SEGMENT_WIDTH * 3;
    let share = (rest as u64 * 380 * GIB / sizes.iter().sum::<u64>()) as i32;
    assert!((widths[1] - MIN_SEGMENT_WIDTH - share).abs() <= 1);

    assert_eq!(segment_widths(&[0, 0, 0], 31), [11, 10, 10]);
    assert_eq!(segment_widths(&[1, 1000, 1], 10), [3, 4, 3]);
    assert_eq!(segment_widths(&[5], 0), [0]);
    assert!(segment_widths(&[], 100).is_empty());
}

fn pixel(data: &[u8], width: usize, x: i32, y: i32) -> u32 {
    let at = (y as usize * width + x as usize) * 4;
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// A solid segment is its colour, free space is striped, segments are
/// parted by a line, and the highlighted one is ringed.
pub(super) fn test_partition_bar_paints_fills_hatch_and_highlight() {
    let style = StyleSheet::dark();
    let fill = Color32::rgb(0x20, 0x60, 0xa0);
    let segment = |size, fill| BarSegment {
        label: String::new(),
        size,
        fill,
    };
    let mut bar = PartitionBarWidget::new(
        vec![
            segment(1, SegmentFill::Solid(fill)),
            segment(1, SegmentFill::Hatched),
            segment(1, SegmentFill::Solid(fill)),
        ],
        Some(2),
    );
    let (width, height) = (240usize, 40usize);
    lay_out(
        &mut bar,
        BoxConstraints::loose(Size::new(width as i32, height as i32)),
        (0, 0),
    );
    let bar_h = bar.layout_rect().height;
    let mut data = vec![0u8; width * height * 4];
    {
        let mut buffer =
            DrawBuffer::new(&mut data, width as u32, height as u32, width * 4, 4).unwrap();
        let mut ctx = PaintContext::new(&mut buffer, &style);
        bar.paint(&mut ctx);
    }
    let encode = |c: Color32| PixelFormat::Argb8888.encode(c).to_u32();
    let mid = bar_h / 2;
    assert_eq!(pixel(&data, width, 40, mid), encode(fill));
    assert_eq!(pixel(&data, width, 80, mid), encode(style.bg_primary));
    let hatch: Vec<u32> = (81..159).map(|x| pixel(&data, width, x, mid)).collect();
    assert!(hatch.contains(&encode(style.bg_secondary)));
    assert!(hatch.contains(&encode(style.bg_tertiary)));
    assert_eq!(pixel(&data, width, 160, mid), encode(style.text_primary));
    assert_eq!(pixel(&data, width, 161, mid), encode(style.text_primary));
    assert_eq!(pixel(&data, width, 200, mid), encode(fill));
    assert_eq!(pixel(&data, width, 200, 1), encode(style.text_primary));
    assert_eq!(pixel(&data, width, 0, mid), encode(style.border_default));
}

/// The indeterminate block is a quarter of the bar, goes across and back
/// in one sweep, and never leaves the bar.
pub(super) fn test_indeterminate_progress_sweeps_to_and_fro() {
    assert_eq!(sweep(400, 0), (0, 100));
    assert_eq!(sweep(400, 500), (150, 100));
    assert_eq!(sweep(400, 1000), (300, 100));
    assert_eq!(sweep(400, 1500), (150, 100));
    assert_eq!(sweep(400, 2000), (0, 100));
    for ms in (0..4000).step_by(37) {
        let (at, len) = sweep(400, ms);
        assert!(at >= 0 && at + len <= 400);
    }
    assert_eq!(sweep(2, 700), (0, 1));
    assert_eq!(sweep(0, 700), (0, 0));
}

#[derive(Debug, PartialEq)]
struct Posted(u32);

/// Messages a thread sends arrive whole and in order, and the first one
/// since the last drain makes the wakeup readable.
pub(super) fn test_a_threads_messages_arrive_in_order_with_a_wakeup() {
    let queue = UiQueue::new().expect("pipe");
    assert!(!queue.drain_wakeup());
    let sender = AppSender::<Posted>::new(queue.sender());
    let worker = sender.clone();
    std::thread::spawn(move || (1..=3).for_each(|n| worker.send(Posted(n))))
        .join()
        .unwrap();
    assert!(queue.drain_wakeup());
    assert!(!queue.drain_wakeup());
    let got: Vec<Posted> = queue
        .take_messages()
        .into_iter()
        .map(|m| *m.downcast::<Posted>().expect("the app's type"))
        .collect();
    assert_eq!(got, [Posted(1), Posted(2), Posted(3)]);
    assert!(queue.take_messages().is_empty());

    sender.send(Posted(4));
    assert!(queue.drain_wakeup());
    assert_eq!(queue.take_messages().len(), 1);
}

enum Control {
    Group,
    On(&'static str),
    Off(&'static str),
}

fn page(controls: &[Control]) -> (VStackWidget, Vec<WidgetId>) {
    use crate::node::ButtonStyle;
    use crate::widgets::button::ButtonWidget;
    let widgets: Vec<Box<dyn Widget>> = controls
        .iter()
        .map(|control| -> Box<dyn Widget> {
            let button = |label: &str, enabled| {
                ButtonWidget::new(String::from(label), None, ButtonStyle::Primary, enabled)
            };
            match control {
                Control::Group => Box::new(RadioGroupWidget::new(
                    vec![option("a", "", true), option("b", "", true)],
                    Some(0),
                    None,
                )),
                Control::On(label) => Box::new(button(label, true)),
                Control::Off(label) => Box::new(button(label, false)),
            }
        })
        .collect();
    let ids = widgets.iter().map(|w| w.id()).collect();
    (
        VStackWidget::new(widgets, 0, CrossAxisAlignment::Start),
        ids,
    )
}

/// A rebuild whose chain changed length finds the focused control again by
/// its role and name, counting those like it before it, wherever the change
/// was; one relabelled in a chain of the same length keeps focus by position,
/// and one gone from a chain of another length loses it.
pub(super) fn test_focus_follows_a_control_whose_chain_changed_shape() {
    use Control::{Group, Off, On};
    let mut focus = FocusManager::new();
    let rebuild = |focus: &mut FocusManager, controls: &[Control]| {
        let (tree, ids) = page(controls);
        (focus.rebuild_tab_chain(&tree), ids)
    };

    let (_, ids) = rebuild(&mut focus, &[On("Back"), Group, On("Review")]);
    focus.set_focused(Some(ids[1]));
    let (restored, ids) = rebuild(&mut focus, &[On("Back"), Group, Off("Review")]);
    assert_eq!(restored, Some(ids[1]));
    let (restored, ids) = rebuild(&mut focus, &[Off("Back"), Group, On("Review")]);
    assert_eq!(restored, Some(ids[1]));

    let (_, ids) = rebuild(&mut focus, &[On("Back"), On("OK"), On("OK")]);
    focus.set_focused(Some(ids[2]));
    let (restored, ids) = rebuild(&mut focus, &[Off("Back"), On("OK"), On("OK")]);
    assert_eq!(restored, Some(ids[2]));
    let (restored, ids) = rebuild(&mut focus, &[Off("Back"), On("OK"), On("Done")]);
    assert_eq!(restored, Some(ids[2]));
    let (restored, _) = rebuild(
        &mut focus,
        &[On("Back"), On("Cancel"), On("OK"), Off("Done")],
    );
    assert_eq!(restored, None);
    assert_eq!(focus.focused(), None);

    let (_, ids) = rebuild(&mut focus, &[On("Back"), On("OK")]);
    focus.set_focused(Some(ids[1]));
    let (restored, _) = rebuild(&mut focus, &[On("Back"), Group]);
    assert_eq!(
        restored, None,
        "the same position holds another kind of control"
    );
}

/// What a scroll view holds is laid out beside its scrollbar and takes the
/// presses, moves and releases the bar does not, so a control in one can be
/// clicked and dragged; the bar's own drag reports every offset it scrolls to.
pub(super) fn test_a_scroll_view_hands_pointer_input_to_its_content() {
    use crate::constraints::{ScrollDirection, ScrollbarVisibility};
    use crate::widgets::checkbox::CheckboxWidget;
    use crate::widgets::scroll_view::ScrollViewWidget;
    let style = StyleSheet::dark();
    let checkbox = CheckboxWidget::new(
        false,
        String::from("I understand"),
        Some(Box::new(|| {
            Box::new(String::from("ticked")) as Box<dyn Any>
        })),
        true,
    );
    let knob = SliderWidget::new(
        0,
        0,
        100,
        1,
        true,
        None,
        Some(Box::new(|i: SliderInput| Box::new(i) as Box<dyn Any>)),
    );
    let content: Vec<Box<dyn Widget>> = vec![
        Box::new(checkbox),
        Box::new(knob),
        Box::new(FixedSizeWidget::new(10, 400)),
    ];
    let mut view = ScrollViewWidget::with_scroll(
        Box::new(VStackWidget::new(content, 0, CrossAxisAlignment::Stretch)),
        ScrollDirection::Vertical,
        ScrollbarVisibility::WhenNeeded,
        0,
        Some(Box::new(|y: i32| Box::new(y) as Box<dyn Any>)),
    );
    lay_out(
        &mut view,
        BoxConstraints::tight(Size::new(240, 120)),
        (0, 0),
    );
    let stack = &view.children()[0];
    assert_eq!(stack.layout_rect().width, 240 - style.scrollbar_width);
    let tick = stack.children()[0].layout_rect();
    let slide = stack.children()[1].layout_rect();

    let (consumed, mut sink) = deliver(&mut view, press(tick.x + 2, tick.y + tick.height / 2));
    assert!(consumed);
    assert_eq!(sink.drain_typed::<String>(), ["ticked"]);

    let y = slide.y + slide.height / 2;
    let (_, mut sink) = deliver(&mut view, press(slide.x + 100, y));
    assert!(matches!(
        inputs(&mut sink).as_slice(),
        [SliderInput::Begin(_, _)]
    ));
    let (_, mut sink) = deliver(&mut view, WidgetEvent::PointerMove { x: 1000, y: 500 });
    assert_eq!(inputs(&mut sink), [SliderInput::Change(100)]);
    let release = WidgetEvent::PointerUp {
        x: 1000,
        y: 500,
        button: PointerButton::Left,
    };
    let (_, mut sink) = deliver(&mut view, release.clone());
    assert_eq!(inputs(&mut sink), [SliderInput::End]);

    let bar = 240 - style.scrollbar_width / 2;
    let (consumed, sink) = deliver(&mut view, press(bar, 2));
    assert!(consumed);
    assert!(!sink.has_messages(), "the thumb's press is the bar's alone");
    let (_, mut sink) = deliver(&mut view, WidgetEvent::PointerMove { x: bar, y: 42 });
    let offsets = sink.drain_typed::<i32>();
    assert!(matches!(offsets.as_slice(), [y] if *y > 0), "{offsets:?}");
    deliver(&mut view, release);
}

/// A bar with no value draws a quarter of itself in its colour, wherever the
/// clock has the block.
pub(super) fn test_indeterminate_progress_paints_its_block() {
    use crate::widgets::progress_bar::ProgressBarWidget;
    let style = StyleSheet::dark();
    let color = Color32::rgb(0x12, 0x9a, 0x56);
    let mut bar = ProgressBarWidget::new(None, String::new(), Some(color));
    let (width, height) = (200usize, 40usize);
    lay_out(
        &mut bar,
        BoxConstraints::tight(Size::new(width as i32, 24)),
        (0, 0),
    );
    let mut data = vec![0u8; width * height * 4];
    {
        let mut buffer =
            DrawBuffer::new(&mut data, width as u32, height as u32, width * 4, 4).unwrap();
        let mut ctx = PaintContext::new(&mut buffer, &style);
        bar.paint(&mut ctx);
    }
    let block = PixelFormat::Argb8888.encode(color).to_u32();
    let lit = (0..width as i32)
        .filter(|&x| pixel(&data, width, x, 12) == block)
        .count();
    assert!((48..=50).contains(&lit), "{lit} pixels of the block");
}

/// While a button is down, a rebuild that the gesture did not ask for waits
/// for its release and runs once then, or after three seconds, for a release
/// the compositor dropped.
pub(super) fn test_a_rebuild_waits_for_the_pointer_release() {
    use crate::node::Action;
    use crate::run::Gesture;
    let mut gesture = Gesture::default();
    assert_eq!(gesture.hold(Action::Rebuild, 0), Action::Rebuild);
    assert!(!gesture.released(0));
    assert_eq!(gesture.wait_ms(0), None);

    gesture.note(&press(0, 0), 100);
    gesture.note(
        &WidgetEvent::PointerDown {
            x: 0,
            y: 0,
            button: PointerButton::Right,
            modifiers: Default::default(),
        },
        200,
    );
    assert_eq!(gesture.hold(Action::Rebuild, 300), Action::None);
    assert_eq!(gesture.hold(Action::Repaint, 300), Action::Repaint);
    assert_eq!(gesture.hold(Action::Exit, 300), Action::Exit);
    assert_eq!(gesture.wait_ms(300), Some(2800));
    let up = |button| WidgetEvent::PointerUp { x: 0, y: 0, button };
    gesture.note(&up(PointerButton::Left), 400);
    assert!(!gesture.released(400), "the right button is still down");
    gesture.note(&up(PointerButton::Right), 500);
    assert!(gesture.released(500));
    assert!(!gesture.released(500));

    gesture.note(&press(0, 0), 1000);
    assert_eq!(gesture.hold(Action::Rebuild, 1000), Action::None);
    assert!(!gesture.released(3999));
    assert!(
        gesture.released(4000),
        "a press holds three seconds at most"
    );
    assert_eq!(gesture.hold(Action::Rebuild, 4001), Action::Rebuild);
}

fn luma(pixel: u32) -> u32 {
    let [b, g, r, _] = pixel.to_le_bytes();
    (299 * r as u32 + 587 * g as u32 + 114 * b as u32) / 1000
}

/// A label is drawn whole in a segment it fits and not at all in one it does
/// not, dark over a light fill and light over a dark one.
pub(super) fn test_partition_bar_labels_fit_whole_in_a_readable_ink() {
    let style = StyleSheet::dark();
    let light = Color32::rgb(0xe0, 0xe0, 0xa0);
    let dark = Color32::rgb(0x20, 0x20, 0x50);
    let segment = |label: &str, size, color| BarSegment {
        label: String::from(label),
        size,
        fill: SegmentFill::Solid(color),
    };
    let mut bar = PartitionBarWidget::new(
        vec![
            segment("Windows", 10, light),
            segment("data", 10, dark),
            segment("SlopOS root 200 GiB", 1, light),
        ],
        None,
    );
    let (width, height) = (420usize, 40usize);
    lay_out(
        &mut bar,
        BoxConstraints::loose(Size::new(width as i32, height as i32)),
        (0, 0),
    );
    let widths = segment_widths(&[10, 10, 1], width as i32);
    let bar_h = bar.layout_rect().height;
    let mut data = vec![0u8; width * height * 4];
    {
        let mut buffer =
            DrawBuffer::new(&mut data, width as u32, height as u32, width * 4, 4).unwrap();
        let mut ctx = PaintContext::new(&mut buffer, &style);
        bar.paint(&mut ctx);
    }
    let band = |from: i32, to: i32| -> Vec<u32> {
        (2..bar_h - 2)
            .flat_map(|y| (from + 2..to - 2).map(move |x| (x, y)))
            .map(|(x, y)| pixel(&data, width, x, y))
            .collect()
    };
    let first = band(0, widths[0]);
    let second = band(widths[0], widths[0] + widths[1]);
    let third = band(widths[0] + widths[1], width as i32);
    let fill = |c: Color32| PixelFormat::Argb8888.encode(c).to_u32();
    assert!(first.iter().any(|&p| luma(p) + 80 < luma(fill(light))));
    assert!(second.iter().any(|&p| luma(p) > luma(fill(dark)) + 80));
    assert!(
        third.iter().all(|&p| p == fill(light)),
        "a label that does not fit"
    );
}

/// A field that is a tab stop is reached and left by Tab, and says so; one
/// that takes Tab itself, as a find bar does, keeps it.
pub(super) fn test_a_tab_stop_field_reports_focus_and_lets_tab_through() {
    use crate::widgets::line_edit::{LineEditInput, LineEditWidget};
    let make = |tab_stop: bool, focused: bool| {
        let mut edit = LineEditWidget::new(
            String::from("https://"),
            String::from("Remote"),
            8,
            focused,
            None,
            String::new(),
            false,
            Some(Box::new(|i: LineEditInput| Box::new(i) as Box<dyn Any>)),
        )
        .with_tab_stop(tab_stop);
        lay_out(&mut edit, BoxConstraints::tight(Size::new(200, 28)), (0, 0));
        edit
    };
    let tab = key(NamedKey::Tab, false);
    let text = WidgetEvent::TextInput { character: 'x' };

    let mut field = make(true, false);
    assert_eq!(field.focus_policy(), FocusPolicy::StrongFocus);
    let (_, mut sink) = deliver(&mut field, WidgetEvent::FocusGained);
    assert_eq!(sink.drain_typed::<LineEditInput>(), [LineEditInput::Focus]);
    let (consumed, mut sink) = deliver(&mut field, text.clone());
    assert!(consumed, "focused by Tab, it takes what is typed");
    assert_eq!(
        sink.drain_typed::<LineEditInput>(),
        [LineEditInput::Text { character: 'x' }]
    );
    let (consumed, sink) = deliver(&mut field, tab.clone());
    assert!(!consumed && !sink.has_messages(), "Tab moves focus on");
    let (_, mut sink) = deliver(&mut field, WidgetEvent::FocusLost);
    assert_eq!(sink.drain_typed::<LineEditInput>(), [LineEditInput::Blur]);
    let (consumed, _) = deliver(&mut field, text);
    assert!(!consumed);

    let mut find = make(false, true);
    assert_eq!(find.focus_policy(), FocusPolicy::ClickFocus);
    let (_, sink) = deliver(&mut find, WidgetEvent::FocusGained);
    assert!(!sink.has_messages());
    let (consumed, mut sink) = deliver(&mut find, tab);
    assert!(consumed);
    assert!(matches!(
        sink.drain_typed::<LineEditInput>().as_slice(),
        [LineEditInput::Key { .. }]
    ));
}

/// A scroll view that follows its end reports it as `i32::MAX`, and given
/// that back keeps to the end of whatever it holds; one that does not reports
/// the offset.
pub(super) fn test_a_scroll_view_that_follows_its_end_reports_it_as_the_largest_offset() {
    use crate::constraints::{ScrollDirection, ScrollbarVisibility};
    use crate::widgets::scroll_view::ScrollViewWidget;
    let view_of = |height: i32, scroll_y: i32| {
        let mut view = ScrollViewWidget::with_scroll(
            Box::new(FixedSizeWidget::new(10, height)),
            ScrollDirection::Vertical,
            ScrollbarVisibility::WhenNeeded,
            scroll_y,
            Some(Box::new(|y: i32| Box::new(y) as Box<dyn Any>)),
        )
        .following_end(true);
        lay_out(
            &mut view,
            BoxConstraints::tight(Size::new(240, 120)),
            (0, 0),
        );
        view
    };
    for height in [400, 900] {
        let view = view_of(height, i32::MAX);
        assert_eq!(view.children()[0].layout_rect().y, 120 - height);
    }
    let mut view = view_of(400, 0);
    focused(&mut view);
    let (_, mut sink) = deliver(&mut view, key(NamedKey::End, false));
    assert_eq!(sink.drain_typed::<i32>(), [i32::MAX]);
    let (_, mut sink) = deliver(&mut view, key(NamedKey::Up, false));
    let [up] = sink.drain_typed::<i32>()[..] else {
        panic!("one offset");
    };
    assert!(up > 0 && up < 280, "{up}");
    let (_, mut sink) = deliver(&mut view, key(NamedKey::Home, false));
    assert_eq!(sink.drain_typed::<i32>(), [0]);

    let mut page = ScrollViewWidget::with_scroll(
        Box::new(FixedSizeWidget::new(10, 400)),
        ScrollDirection::Vertical,
        ScrollbarVisibility::WhenNeeded,
        0,
        Some(Box::new(|y: i32| Box::new(y) as Box<dyn Any>)),
    );
    lay_out(
        &mut page,
        BoxConstraints::tight(Size::new(240, 120)),
        (0, 0),
    );
    focused(&mut page);
    let (_, mut sink) = deliver(&mut page, key(NamedKey::End, false));
    assert_eq!(sink.drain_typed::<i32>(), [280], "a page keeps its offset");
}

/// A control reached by Tab inside a scroll view is scrolled into view: its
/// bottom to the view's below it, its top to the view's above it or when it
/// is taller than the view, and nothing moves for one already shown.
pub(super) fn test_a_control_reached_by_tab_is_scrolled_into_view() {
    use crate::constraints::{ScrollDirection, ScrollbarVisibility};
    use crate::widgets::checkbox::CheckboxWidget;
    use crate::widgets::scroll_view::ScrollViewWidget;
    let content: Vec<Box<dyn Widget>> = vec![
        Box::new(FixedSizeWidget::new(10, 300)),
        Box::new(CheckboxWidget::new(
            false,
            String::from("first"),
            None,
            true,
        )),
        Box::new(FixedSizeWidget::new(10, 300)),
    ];
    let mut view = ScrollViewWidget::with_scroll(
        Box::new(VStackWidget::new(content, 0, CrossAxisAlignment::Stretch)),
        ScrollDirection::Vertical,
        ScrollbarVisibility::WhenNeeded,
        0,
        Some(Box::new(|y: i32| Box::new(y) as Box<dyn Any>)),
    );
    lay_out(
        &mut view,
        BoxConstraints::tight(Size::new(240, 120)),
        (0, 0),
    );
    let id_of = |view: &ScrollViewWidget, i: usize| view.children()[0].children()[i].id();
    let rect_of =
        |view: &ScrollViewWidget, i: usize| view.children()[0].children()[i].layout_rect();

    let (field, tall) = (id_of(&view, 1), id_of(&view, 0));
    let mut sink = MessageSink::new();
    crate::focus::reveal(&mut view, field, &mut sink);
    let shown = rect_of(&view, 1);
    assert_eq!(shown.y + shown.height, 120, "{shown:?}");
    assert_eq!(sink.drain_typed::<i32>(), [300 + shown.height - 120]);

    let mut sink = MessageSink::new();
    crate::focus::reveal(&mut view, field, &mut sink);
    assert!(!sink.has_messages(), "a control shown stays where it is");

    let mut sink = MessageSink::new();
    crate::focus::reveal(&mut view, tall, &mut sink);
    assert_eq!(rect_of(&view, 0).y, 0, "a tall one shows its top");
    assert_eq!(sink.drain_typed::<i32>(), [0]);
}
