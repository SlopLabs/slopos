//! What each of the compositor's own surfaces draws: an appkit tree, which
//! takes the pointer while the surface's model takes the keys.

use slopos_appkit::embed::Embedded;
use slopos_appkit::traits::Role;
use slopos_appkit::{Node, Rect, Size};
use slopos_chrome_core::key::Taken;

use crate::gfx::{DamageRect, DrawBuffer};

pub struct Overlay {
    tree: Option<Embedded>,
    stale: bool,
    dirty: bool,
    painted: Option<Rect>,
    /// Where the primary button is held down on the tree, which a rebuild
    /// presses again so a relayout mid-click keeps the click.
    held: Option<(i32, i32)>,
    /// Keys pressed into the surface, whose repeats and releases are its own.
    pub taken: Taken,
}

impl Overlay {
    pub fn new() -> Overlay {
        Overlay {
            tree: None,
            stale: false,
            dirty: false,
            painted: None,
            held: None,
            taken: Taken::default(),
        }
    }

    pub fn rect(&self) -> Option<Rect> {
        self.tree.as_ref().map(Embedded::rect)
    }

    /// What it shows changed since it was laid out, or it never was.
    pub fn stale(&self) -> bool {
        self.stale || self.tree.is_none()
    }

    pub fn changed(&mut self) {
        self.stale = true;
        self.dirty = true;
    }

    /// `node` laid out in `rect`, each scroll view kept where it was, lit
    /// where the pointer is and showing `focus`.
    pub fn show<M: Clone + 'static>(
        &mut self,
        node: &Node<M>,
        rect: Rect,
        pointer: (i32, i32),
        focus: Option<(Role, &str)>,
    ) {
        match &mut self.tree {
            Some(tree) => tree.rebuild(node, rect),
            None => self.tree = Some(Embedded::new(node, rect)),
        }
        self.settle(pointer, focus);
    }

    /// `node` at the size it takes within `most`, where `place` puts it.
    pub fn show_natural<M: Clone + 'static>(
        &mut self,
        node: &Node<M>,
        most: Size,
        place: impl FnOnce(Size) -> Rect,
        pointer: (i32, i32),
        focus: Option<(Role, &str)>,
    ) {
        let natural = Embedded::natural(node, most).rect();
        let rect = place(Size::new(natural.width, natural.height));
        self.show(node, rect, pointer, focus);
    }

    fn settle(&mut self, (x, y): (i32, i32), focus: Option<(Role, &str)>) {
        if let Some(tree) = &mut self.tree {
            if let Some((hx, hy)) = self.held {
                let _: Vec<()> = tree.click(hx, hy, true);
            }
            tree.hover(x, y);
            if let Some((role, name)) = focus {
                tree.focus(role, name);
            }
        }
        self.stale = false;
        self.dirty = true;
    }

    pub fn hide(&mut self) {
        self.stale = false;
        self.held = None;
        if self.tree.take().is_some() {
            self.dirty = true;
        }
    }

    pub fn click<M: 'static>(&mut self, x: i32, y: i32, pressed: bool) -> Vec<M> {
        let Some(tree) = self.tree.as_mut() else {
            return Vec::new();
        };
        self.dirty = true;
        self.held = pressed.then_some((x, y));
        tree.click(x, y, pressed)
    }

    pub fn hover(&mut self, x: i32, y: i32) {
        if let Some(tree) = self.tree.as_mut() {
            self.dirty |= tree.hover(x, y);
        }
    }

    pub fn wheel<M: 'static>(&mut self, x: i32, y: i32, value_v120: i32) -> Vec<M> {
        let Some(tree) = self.tree.as_mut() else {
            return Vec::new();
        };
        self.dirty = true;
        tree.wheel(x, y, value_v120)
    }

    /// The primary button went up where the surface does not show.
    pub fn release_unseen(&mut self) {
        self.held = None;
        if let Some(tree) = self.tree.as_mut() {
            tree.release_unseen();
        }
    }

    /// Where it was painted and where it is now, once either changed.
    pub fn take_damage(&mut self) -> impl Iterator<Item = DamageRect> + use<> {
        let now = self.rect();
        let damage = match std::mem::take(&mut self.dirty) {
            true => [self.painted, now],
            false => [None, None],
        };
        self.painted = now;
        damage
            .into_iter()
            .flatten()
            .map(|rect| rect.to_damage_rect())
    }

    pub fn draw(&self, buf: &mut DrawBuffer, clip: &DamageRect) {
        let Some(tree) = &self.tree else {
            return;
        };
        let clip = Rect::new(
            clip.x0,
            clip.y0,
            clip.x1 - clip.x0 + 1,
            clip.y1 - clip.y0 + 1,
        );
        if let Some(clip) = clip.intersect(&tree.rect()) {
            tree.paint(buf, clip);
        }
    }
}
