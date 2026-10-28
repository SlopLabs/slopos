use slopos_gfx::DrawBuffer;
use slopos_theme_core::{Icon, TextRole, Tone};

use crate::audit::audit_node;
use crate::components::{Button, ButtonRow, Dialog, Disclosure, List, ListRow, Text};
use crate::constraints::{CrossAxisAlignment, Rect};
use crate::embed::Embedded;
use crate::node::Node;
use crate::traits::{Kept, KeptAt, Role, Widget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Said {
    Cancel,
    Allow,
}

const SCREEN: Rect = Rect::new(0, 0, 800, 600);

fn dialog(anchor: Option<Rect>) -> Node<Said> {
    let icon = Node::Icon {
        icon: Icon::Terminal,
        role: TextRole::Display,
        tone: Tone::Secondary,
    };
    Dialog::new("Terminal wants to change the network settings.")
        .icon(icon)
        .content(Text::new("Nothing else changes.").wrap())
        .footer(
            ButtonRow::new()
                .cancel(Button::new("Cancel").on_press(Said::Cancel))
                .confirm(Button::new("Change").on_press(Said::Allow)),
        )
        .anchor(anchor)
        .into()
}

fn named(widget: &dyn Widget, name: &str) -> Option<Rect> {
    if widget.role() == Role::Button && widget.accessible_name() == Some(name) {
        return Some(widget.layout_rect());
    }
    widget
        .children()
        .iter()
        .find_map(|child| named(child.as_ref(), name))
}

fn centre(r: Rect) -> (i32, i32) {
    (r.x + r.width / 2, r.y + r.height / 2)
}

fn painted(tree: &Embedded) -> Vec<u8> {
    let (w, h) = (SCREEN.width as u32, SCREEN.height as u32);
    let mut data = vec![0u8; (w * h * 4) as usize];
    let mut buffer = DrawBuffer::new(&mut data, w, h, w as usize * 4, 4).expect("a buffer");
    tree.paint(&mut buffer, SCREEN);
    data
}

pub(super) fn test_an_embedded_tree_answers_a_click_with_its_message() {
    let mut tree = Embedded::new(&dialog(None), SCREEN);
    let allow = named(tree.root(), "Change").expect("the allowing button");
    let cancel = named(tree.root(), "Cancel").expect("the cancelling button");
    let (x, y) = centre(allow);
    assert!(tree.click::<Said>(x, y, true).is_empty());
    assert_eq!(tree.click::<Said>(x, y, false), vec![Said::Allow]);
    assert!(tree.click::<Said>(x, y, true).is_empty());
    let (cx, cy) = centre(cancel);
    assert!(
        tree.click::<Said>(cx, cy, false).is_empty(),
        "released elsewhere"
    );
    assert!(tree.click::<Said>(cx, cy, true).is_empty());
    assert_eq!(tree.click::<Said>(cx, cy, false), vec![Said::Cancel]);
    assert!(tree.click::<Said>(x, y, true).is_empty());
    tree.release_unseen();
    assert!(
        tree.click::<Said>(x, y, false).is_empty(),
        "a press ended unseen"
    );
}

pub(super) fn test_an_embedded_tree_shows_focus_where_it_is_told() {
    let mut tree = Embedded::new(&dialog(None), SCREEN);
    let unfocused = painted(&tree);
    tree.focus(Role::Button, "Cancel");
    let on_cancel = painted(&tree);
    tree.focus(Role::Button, "Change");
    let on_allow = painted(&tree);
    assert_ne!(unfocused, on_cancel);
    assert_ne!(on_cancel, on_allow);
    tree.focus(Role::Button, "Nothing of that name");
    assert_eq!(painted(&tree), unfocused);
    tree.focus(Role::Button, "Cancel");
    tree.rebuild(&dialog(None), SCREEN);
    assert_eq!(painted(&tree), unfocused);
}

pub(super) fn test_hover_counts_only_between_controls() {
    let mut tree = Embedded::new(&dialog(None), SCREEN);
    let allow = named(tree.root(), "Change").expect("the allowing button");
    let cancel = named(tree.root(), "Cancel").expect("the cancelling button");
    assert!(!tree.hover(1, 1), "the scrim is no control");
    let (x, y) = centre(allow);
    assert!(tree.hover(x, y));
    assert!(!tree.hover(x + 1, y));
    let (cx, cy) = centre(cancel);
    assert!(tree.hover(cx, cy));
    assert!(tree.hover(1, 1));
}

pub(super) fn test_hover_lights_what_a_press_reaches() {
    let disclosure: Node<Said> = Disclosure::new("Details", Text::new("inside"))
        .expanded(true)
        .into();
    let mut tree = Embedded::new(&disclosure, Rect::new(0, 0, 400, 200));
    assert!(tree.hover(10, 10), "onto the header");
    assert!(tree.hover(390, 10), "beside it");
    assert!(!tree.hover(40, 40), "the content is no control");

    let rows: Vec<Node<Said>> = (0..20)
        .map(|i| Text::new(format!("Line {i}")).into())
        .collect();
    let scrolled = Node::ScrollView {
        child: Box::new(Node::VStack {
            children: rows,
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
        }),
        scroll_y: 0,
        on_scroll: None,
        follow_end: false,
    };
    let mut tree = Embedded::new(&scrolled, Rect::new(0, 0, 200, 100));
    assert!(!tree.hover(20, 5));
    assert!(tree.hover(197, 5), "onto the thumb");
    assert!(tree.hover(197, 95), "down the track past it");

    let list = || -> Node<Said> {
        List::new(vec![ListRow::new("One"), ListRow::new("Two")])
            .on_activate(|_| Said::Allow)
            .into()
    };
    let mut tree = Embedded::new(&list(), Rect::new(0, 0, 200, 200));
    assert!(tree.hover(20, 10));
    let covered = Node::ZStack {
        children: vec![
            list(),
            Node::Popup {
                x: 0,
                y: 150,
                child: Box::new(Text::new("Over").into()),
                on_dismiss: None,
            },
        ],
    };
    let mut tree = Embedded::new(&covered, Rect::new(0, 0, 200, 200));
    assert!(!tree.hover(20, 10), "a row beneath the popup");
}

pub(super) fn test_a_dialog_centres_over_its_anchor_where_it_fits() {
    let left = |anchor| {
        let tree = Embedded::new(&dialog(anchor), SCREEN);
        named(tree.root(), "Cancel")
            .expect("the cancelling button")
            .x
    };
    let centred = left(None);
    assert_eq!(left(Some(Rect::new(100, 100, 400, 200))), centred - 100);
    let at_the_edge = left(Some(Rect::new(0, 0, 10, 10)));
    assert_eq!(left(Some(Rect::new(0, 0, 200, 200))), at_the_edge);
    assert!(at_the_edge < centred - 100, "{at_the_edge} {centred}");
    assert_eq!(left(Some(Rect::new(-500, 900, 100, 100))), centred);
}

pub(super) fn test_a_dialog_with_an_icon_reads_at_every_width() {
    let found = audit_node(&dialog(None), &[360, 640, 960], 480);
    assert!(found.is_empty(), "{}", found.join("\n"));
    let tree = Embedded::new(&dialog(None), SCREEN);
    assert_eq!(
        tree.root().accessible_name(),
        Some("Terminal wants to change the network settings.")
    );
}

pub(super) fn test_an_embedded_thumb_drags() {
    let rows: Vec<Node<Said>> = (0..20)
        .map(|i| Text::new(format!("Line {i}")).into())
        .collect();
    let scrolled = Node::ScrollView {
        child: Box::new(Node::VStack {
            children: rows,
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
        }),
        scroll_y: 0,
        on_scroll: None,
        follow_end: false,
    };
    let mut tree = Embedded::new(&scrolled, Rect::new(0, 0, 200, 100));
    let down = |tree: &Embedded| {
        let mut kept = Vec::new();
        crate::tree::kept(tree.root(), &mut kept);
        match kept[..] {
            [
                Kept {
                    at: KeptAt::Scroll(y),
                    ..
                },
            ] => y,
            _ => panic!("one scroll view: {kept:?}"),
        }
    };
    assert!(tree.click::<Said>(197, 1, true).is_empty());
    assert_eq!(down(&tree), 0);
    assert!(tree.hover(197, 99), "the thumb follows the pointer");
    let bottom = down(&tree);
    assert!(bottom > 0);
    assert!(tree.click::<Said>(197, 99, false).is_empty());
    tree.hover(197, 1);
    assert_eq!(down(&tree), bottom, "a move after the release");
}

pub(super) fn test_a_slider_and_a_field_light_under_the_pointer() {
    let slider: Node<Said> = Node::Slider {
        value: 40,
        min: 0,
        max: 100,
        step: 1,
        enabled: true,
        grab: None,
        on_input: None,
    };
    let field: Node<Said> = crate::components::SearchField::new("", 0).into();
    for node in [slider, field] {
        let mut tree = Embedded::new(&node, Rect::new(0, 0, 300, 40));
        let still = painted(&tree);
        let knob = tree.root().layout_rect();
        let at = (knob.x + knob.width * 2 / 5, knob.y + knob.height / 2);
        assert!(tree.hover(at.0, at.1));
        assert_ne!(painted(&tree), still);
    }
}
