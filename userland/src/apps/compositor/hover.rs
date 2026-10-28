//! The chrome's hover regions, registered each frame and compared with the
//! last frame's, so a hover that moves repaints only what it left and reached.

use crate::gfx::DamageRect;

const MAX_HOVER_REGIONS: usize = 64;

/// Plus a window's id: the hover id of its buttons' group.
pub const HOVER_BUTTONS_BASE: u32 = 0x0006_0000;
/// Plus a status item's kind: its hover id.
pub const HOVER_STATUS_ITEM_BASE: u32 = 0x0007_0000;

#[derive(Copy, Clone)]
struct HoverRegion {
    id: u32,
    rect: DamageRect,
    hovered: bool,
}

impl HoverRegion {
    const fn empty() -> Self {
        Self {
            id: 0,
            rect: DamageRect::invalid(),
            hovered: false,
        }
    }
}

pub struct HoverRegistry {
    current: [HoverRegion; MAX_HOVER_REGIONS],
    current_count: usize,
    previous: [HoverRegion; MAX_HOVER_REGIONS],
    previous_count: usize,
}

impl HoverRegistry {
    pub fn new() -> Self {
        Self {
            current: [HoverRegion::empty(); MAX_HOVER_REGIONS],
            current_count: 0,
            previous: [HoverRegion::empty(); MAX_HOVER_REGIONS],
            previous_count: 0,
        }
    }

    pub fn begin_frame(&mut self) {
        self.previous = self.current;
        self.previous_count = self.current_count;
        self.current_count = 0;
    }

    pub fn register(&mut self, id: u32, rect: DamageRect, hovered: bool) {
        if self.current_count >= MAX_HOVER_REGIONS {
            return;
        }
        self.current[self.current_count] = HoverRegion { id, rect, hovered };
        self.current_count += 1;
    }

    /// Hands `damage` each region hovered or left since the last frame, or
    /// that came or went while hovered.
    pub fn changed_regions(&self, mut damage: impl FnMut(DamageRect)) {
        let mut add = |rect: DamageRect| {
            if rect.is_valid() {
                damage(rect);
            }
        };
        for cur in &self.current[..self.current_count] {
            match self.find_previous(cur.id) {
                Some(prev) if cur.hovered != prev.hovered => {
                    add(prev.rect);
                    add(cur.rect);
                }
                Some(_) => {}
                None if cur.hovered => add(cur.rect),
                None => {}
            }
        }
        for prev in &self.previous[..self.previous_count] {
            if prev.hovered && self.find_current(prev.id).is_none() {
                add(prev.rect);
            }
        }
    }

    fn find_previous(&self, id: u32) -> Option<&HoverRegion> {
        for i in 0..self.previous_count {
            if self.previous[i].id == id {
                return Some(&self.previous[i]);
            }
        }
        None
    }

    fn find_current(&self, id: u32) -> Option<&HoverRegion> {
        for i in 0..self.current_count {
            if self.current[i].id == id {
                return Some(&self.current[i]);
            }
        }
        None
    }
}
