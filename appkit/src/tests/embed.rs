use slopos_gfx::DrawBuffer;
use slopos_theme_core::{Icon, TextRole, Tone};

use crate::audit::audit_node;
use crate::components::{Button, ButtonRow, Text};
use crate::constraints::Rect;
use crate::embed::Embedded;
use crate::node::Node;
use crate::traits::{Role, Widget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Said {
    Cancel,
    Allow,
}

const SCREEN: Rect = Rect::new(0, 0, 800, 600);

fn dialog(anchor: Option<Rect>) -> Node<Said> {
    Node::Dialog {
        title: String::from("Terminal wants to change the network settings."),
        icon: Some(Box::new(Node::Icon {
            icon: Icon::Terminal,
            role: TextRole::Display,
            tone: Tone::Secondary,
        })),
        content: Box::new(Text::new("Nothing else changes.").wrap().into()),
        footer: ButtonRow::new()
            .cancel(Button::new("Cancel").on_press(Said::Cancel))
            .confirm(Button::new("Change").on_press(Said::Allow)),
        on_dismiss: None,
        anchor,
    }
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

/// A click that starts and ends on a button answers with its message; one
/// that ends elsewhere, or where the tree is not shown, answers nothing.
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

/// Focus shows where the holder says, and moves when it says so again; a
/// rebuild shows none until it does.
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

/// Moving the pointer changes what is drawn only as it enters or leaves a
/// control.
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

/// The card centres over its anchor where it fits, and stays on the screen
/// when the anchor is near an edge or off it.
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

/// A dialog with an icon beside its title passes the audit at every width,
/// and is named by its title.
pub(super) fn test_a_dialog_with_an_icon_reads_at_every_width() {
    let found = audit_node(&dialog(None), &[360, 640, 960], 480);
    assert!(found.is_empty(), "{}", found.join("\n"));
    let tree = Embedded::new(&dialog(None), SCREEN);
    assert_eq!(
        tree.root().accessible_name(),
        Some("Terminal wants to change the network settings.")
    );
}
