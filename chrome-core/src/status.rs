//! The menu bar's status items, right to left; [`layout_status_items`] alone
//! knows where one is, so drawing, hit-testing and damage cannot disagree.

use slopos_theme_core::space;

/// Past this the bar, one strip, ignores the rest.
pub const MAX_STATUS_ITEMS: usize = 8;

/// From either edge of the screen to the bar's outermost content.
pub const BAR_PADDING_X: i32 = space::MD;

const BAR_ITEM_GAP: i32 = space::LG;

/// Stated here alone, where the items' hit regions are, so the compositor
/// cannot draw an item away from where it is clicked. The bar's border row
/// lies at `y == BAR_HEIGHT`, outside every item.
pub const BAR_HEIGHT: i32 = 24;

/// The discriminants index the compositor's per-item state.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusKind {
    Clock = 0,
    Network = 1,
}

/// `width` is measured by whoever draws the item; `revision` moves whenever
/// its content does, which is what the bar's damage pass repaints on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusItemSpec {
    pub kind: StatusKind,
    pub present: bool,
    pub width: i32,
    pub revision: u32,
}

impl StatusItemSpec {
    /// Takes no width and no gap, so hiding an item closes its hole.
    pub const fn hidden(kind: StatusKind) -> Self {
        Self {
            kind,
            present: false,
            width: 0,
            revision: 0,
        }
    }

    /// An item of no width counts as hidden, or it would take a gap and
    /// paint nothing.
    #[inline]
    pub const fn is_placeable(&self) -> bool {
        self.present && self.width > 0
    }
}

/// `idx` is the item's index in what [`layout_status_items`] was handed, not
/// its slot's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSlot {
    pub kind: StatusKind,
    pub x: i32,
    pub w: i32,
    pub idx: usize,
}

impl StatusSlot {
    const EMPTY: Self = Self {
        kind: StatusKind::Clock,
        x: 0,
        w: 0,
        idx: 0,
    };

    #[inline]
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        y >= 0 && y < BAR_HEIGHT && x >= self.x && x < self.x + self.w
    }

    #[inline]
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusLayout {
    pub slots: [StatusSlot; MAX_STATUS_ITEMS],
    pub slot_count: usize,
    /// The leftmost item's x less one gap, or the screen's right edge less
    /// [`BAR_PADDING_X`] with none.
    pub app_name_limit: i32,
    pub hovered: Option<usize>,
    /// Of each item's kind, presence and width and how many were offered, but
    /// not of a revision, so a change of content is no change of layout.
    pub signature: u64,
}

impl StatusLayout {
    /// Right to left.
    #[inline]
    pub fn slots(&self) -> &[StatusSlot] {
        &self.slots[..self.slot_count]
    }

    pub fn slot_for(&self, kind: StatusKind) -> Option<&StatusSlot> {
        self.slots().iter().find(|slot| slot.kind == kind)
    }
}

/// The cursor only selects [`StatusLayout::hovered`].
pub fn layout_status_items(
    items: &[StatusItemSpec],
    screen_width: i32,
    cursor_x: i32,
    cursor_y: i32,
) -> StatusLayout {
    let mut slots = [StatusSlot::EMPTY; MAX_STATUS_ITEMS];
    let mut slot_count = 0usize;
    let mut cur = screen_width - BAR_PADDING_X;

    let offered = items.len().min(MAX_STATUS_ITEMS);
    let mut sig = Fnv::new();
    sig.write_u64(offered as u64);

    for (idx, item) in items.iter().take(MAX_STATUS_ITEMS).enumerate() {
        sig.write_u64(item.kind as u64);
        sig.write_u64(item.present as u64);
        sig.write_u64(item.width as u32 as u64);

        if !item.is_placeable() {
            continue;
        }

        cur -= item.width;
        slots[slot_count] = StatusSlot {
            kind: item.kind,
            x: cur,
            w: item.width,
            idx,
        };
        slot_count += 1;
        cur -= BAR_ITEM_GAP;
    }

    let hovered = hit_slot(&slots[..slot_count], cursor_x, cursor_y);

    StatusLayout {
        slots,
        slot_count,
        app_name_limit: cur,
        hovered,
        signature: sig.finish(),
    }
}

pub fn hit_status_item(layout: &StatusLayout, x: i32, y: i32) -> Option<StatusKind> {
    hit_slot(layout.slots(), x, y).map(|i| layout.slots[i].kind)
}

fn hit_slot(slots: &[StatusSlot], x: i32, y: i32) -> Option<usize> {
    slots.iter().position(|slot| slot.contains(x, y))
}

/// FNV-1a, 64-bit: `core` ships no hasher.
struct Fnv(u64);

impl Fnv {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    const fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn write_u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.0 = (self.0 ^ byte as u64).wrapping_mul(Self::PRIME);
        }
    }

    const fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: StatusKind, width: i32) -> StatusItemSpec {
        StatusItemSpec {
            kind,
            present: true,
            width,
            revision: 0,
        }
    }

    const NO_CURSOR: (i32, i32) = (i32::MIN, i32::MIN);

    fn layout(items: &[StatusItemSpec], sw: i32) -> StatusLayout {
        layout_status_items(items, sw, NO_CURSOR.0, NO_CURSOR.1)
    }

    #[test]
    fn places_index_zero_rightmost() {
        let items = [item(StatusKind::Clock, 64), item(StatusKind::Network, 14)];
        let l = layout(&items, 1920);

        assert_eq!(l.slot_count, 2);
        assert_eq!(l.slots[0].kind, StatusKind::Clock);
        assert_eq!(l.slots[0].x, 1920 - BAR_PADDING_X - 64);
        assert_eq!(l.slots[0].right(), 1920 - BAR_PADDING_X);

        assert_eq!(l.slots[1].kind, StatusKind::Network);
        assert_eq!(l.slots[1].right(), l.slots[0].x - BAR_ITEM_GAP);
        assert_eq!(l.slots[1].x, l.slots[0].x - BAR_ITEM_GAP - 14);

        assert_eq!(l.app_name_limit, l.slots[1].x - BAR_ITEM_GAP);
    }

    #[test]
    fn no_items_leaves_the_whole_bar_to_the_app_name() {
        let l = layout(&[], 1024);
        assert_eq!(l.slot_count, 0);
        assert_eq!(l.app_name_limit, 1024 - BAR_PADDING_X);
        assert_eq!(l.hovered, None);
    }

    #[test]
    fn a_hidden_item_removes_its_gap() {
        let visible = [item(StatusKind::Clock, 64), item(StatusKind::Network, 14)];
        let hidden = [
            item(StatusKind::Clock, 64),
            StatusItemSpec::hidden(StatusKind::Network),
        ];

        let with = layout(&visible, 800);
        let without = layout(&hidden, 800);

        assert_eq!(without.slot_count, 1);
        assert_eq!(without.slots[0].x, with.slots[0].x);
        assert_eq!(
            without.app_name_limit,
            with.app_name_limit + 14 + BAR_ITEM_GAP
        );
    }

    #[test]
    fn a_zero_width_item_is_not_placed() {
        let items = [item(StatusKind::Clock, 64), item(StatusKind::Network, 0)];
        let l = layout(&items, 800);
        assert_eq!(l.slot_count, 1);
        assert_eq!(l.slot_for(StatusKind::Network), None);
    }

    #[test]
    fn slots_are_disjoint_and_clear_the_app_name() {
        let items = [
            item(StatusKind::Clock, 64),
            item(StatusKind::Network, 14),
            item(StatusKind::Clock, 30),
        ];
        let l = layout(&items, 1280);

        for pair in l.slots().windows(2) {
            assert!(pair[1].right() <= pair[0].x, "slots overlap: {pair:?}");
        }
        for slot in l.slots() {
            assert!(
                slot.x > l.app_name_limit,
                "slot {slot:?} crosses app_name_limit {}",
                l.app_name_limit
            );
        }
    }

    #[test]
    fn changing_screen_width_translates_every_slot() {
        let items = [item(StatusKind::Clock, 64), item(StatusKind::Network, 14)];
        let narrow = layout(&items, 800);
        let wide = layout(&items, 1920);
        let delta = 1920 - 800;

        assert_eq!(narrow.slot_count, wide.slot_count);
        assert_eq!(narrow.signature, wide.signature);
        for (n, w) in narrow.slots().iter().zip(wide.slots()) {
            assert_eq!(n.x + delta, w.x);
            assert_eq!(n.w, w.w);
            assert_eq!(n.kind, w.kind);
            assert_eq!(n.idx, w.idx);
        }
        assert_eq!(narrow.app_name_limit + delta, wide.app_name_limit);
    }

    #[test]
    fn layout_and_hit_test_agree_at_every_pixel() {
        let items = [
            item(StatusKind::Clock, 64),
            item(StatusKind::Network, 14),
            item(StatusKind::Clock, 7),
        ];
        let l = layout(&items, 1920);

        for x in 0..1920 {
            let expected = l.slots().iter().find(|s| x >= s.x && x < s.right());
            assert_eq!(
                hit_status_item(&l, x, 12),
                expected.map(|s| s.kind),
                "disagreement at x={x}"
            );
        }

        for slot_pair in l.slots().windows(2) {
            for x in slot_pair[1].right()..slot_pair[0].x {
                assert_eq!(hit_status_item(&l, x, 12), None, "gap pixel {x} hit");
            }
        }
        assert_eq!(hit_status_item(&l, l.app_name_limit, 12), None);
    }

    #[test]
    fn hit_test_rejects_rows_outside_the_strip() {
        let items = [item(StatusKind::Clock, 64)];
        let l = layout(&items, 1920);
        let inside = l.slots[0].x;

        assert_eq!(hit_status_item(&l, inside, 0), Some(StatusKind::Clock));
        assert_eq!(
            hit_status_item(&l, inside, BAR_HEIGHT - 1),
            Some(StatusKind::Clock)
        );
        assert_eq!(hit_status_item(&l, inside, BAR_HEIGHT), None);
        assert_eq!(hit_status_item(&l, inside, -1), None);
    }

    #[test]
    fn hovered_matches_the_hit_test() {
        let items = [item(StatusKind::Clock, 64), item(StatusKind::Network, 14)];
        let probe = layout(&items, 1920);

        for slot_index in 0..probe.slot_count {
            let slot = probe.slots[slot_index];
            let hovered = layout_status_items(&items, 1920, slot.x + slot.w / 2, 12);
            assert_eq!(hovered.hovered, Some(slot_index));
        }

        let elsewhere = layout_status_items(&items, 1920, 4, 12);
        assert_eq!(elsewhere.hovered, None);
    }

    #[test]
    fn signature_tracks_presence_width_and_count_but_not_revision() {
        let base = [item(StatusKind::Clock, 64), item(StatusKind::Network, 14)];

        let mut revised = base;
        revised[0].revision = 7;
        revised[1].revision = 99;
        assert_eq!(
            layout(&base, 1920).signature,
            layout(&revised, 1920).signature,
            "a content change must not read as a layout change"
        );

        let mut hidden = base;
        hidden[1].present = false;
        assert_ne!(
            layout(&base, 1920).signature,
            layout(&hidden, 1920).signature
        );

        let mut wider = base;
        wider[0].width = 65;
        assert_ne!(
            layout(&base, 1920).signature,
            layout(&wider, 1920).signature
        );

        assert_ne!(
            layout(&base, 1920).signature,
            layout(&base[..1], 1920).signature
        );

        let mut swapped = base;
        swapped.swap(0, 1);
        assert_ne!(
            layout(&base, 1920).signature,
            layout(&swapped, 1920).signature,
            "reordering items changes what is drawn where"
        );
    }

    #[test]
    fn items_beyond_the_cap_are_ignored() {
        let items = [item(StatusKind::Network, 10); MAX_STATUS_ITEMS + 3];
        let l = layout(&items, 1920);
        assert_eq!(l.slot_count, MAX_STATUS_ITEMS);
        assert_eq!(
            l.app_name_limit,
            1920 - BAR_PADDING_X - MAX_STATUS_ITEMS as i32 * (10 + BAR_ITEM_GAP)
        );
    }
}
