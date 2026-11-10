//! Which of a surface's two buffers a frame may be drawn into, and what each
//! holds.
//!
//! The reader shows the buffer last committed and may read the one before it
//! until it releases it, so a frame is begun only in a buffer that is neither.

use slopos_abi::damage::DamageRect;

use crate::damage::Rects;

pub const BUFFERS: usize = 2;

const AGE_UNDEFINED: u32 = u32::MAX;

/// What a buffer holds, measured against the one shown.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Behind {
    Unknown,
    /// The shown frame everywhere but in these rects.
    By(Rects),
}

/// A frame begun over the one shown: draw into `slot` once `stale` has been
/// copied into it from the other, and it holds the shown frame whole.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OverShown {
    pub slot: usize,
    pub stale: Rects,
}

/// A frame committed: `slot` is shown now, and `first_use` says the reader
/// has not been handed it before.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    pub slot: usize,
    pub first_use: bool,
}

#[derive(Clone, Debug)]
pub struct Swap {
    /// Whether the reader has been handed this slot's buffer yet.
    registered: [bool; BUFFERS],
    /// Whether the reader may still read this slot.
    busy: [bool; BUFFERS],
    /// Releases still to come for the buffers [`Swap::replace_buffers`]
    /// replaced, which free nothing of the ones that took their slots.
    stale_releases: [u32; BUFFERS],
    /// Frames since each slot last held a committed frame.
    age: [u32; BUFFERS],
    behind: [Behind; BUFFERS],
    shown: Option<usize>,
    drawing: Option<usize>,
}

impl Default for Swap {
    fn default() -> Self {
        Self::new()
    }
}

impl Swap {
    pub const fn new() -> Self {
        Self {
            registered: [false; BUFFERS],
            busy: [false; BUFFERS],
            stale_releases: [0; BUFFERS],
            age: [AGE_UNDEFINED; BUFFERS],
            behind: [Behind::Unknown; BUFFERS],
            shown: None,
            drawing: None,
        }
    }

    /// Both slots hold new buffers the reader has not seen, as after a
    /// resize; it may still release the ones they replace.
    pub fn replace_buffers(&mut self) {
        let mut stale_releases = self.stale_releases;
        for (slot, stale) in stale_releases.iter_mut().enumerate() {
            if self.busy[slot] && self.shown != Some(slot) {
                *stale += 1;
            }
        }
        *self = Self {
            stale_releases,
            ..Self::new()
        };
    }

    /// The slot a frame may be drawn into now.
    pub fn free(&self) -> Option<usize> {
        (0..BUFFERS).find(|&slot| !self.busy[slot] && self.shown != Some(slot))
    }

    pub fn shown(&self) -> Option<usize> {
        self.shown
    }

    pub fn drawing(&self) -> Option<usize> {
        self.drawing
    }

    /// Age of the buffer the next frame is begun in, per the
    /// `EGL_EXT_buffer_age` convention: `n > 0` means it still holds the frame
    /// committed `n` frames ago; `0` means undefined contents, or no buffer.
    pub fn age(&self) -> u32 {
        match self.free().map(|slot| self.age[slot]) {
            None | Some(AGE_UNDEFINED) => 0,
            Some(age) => age,
        }
    }

    /// Takes back the buffer the reader was shown before this one though it
    /// never said it let it go, as a release lost would leave it for good.
    pub fn reclaim(&mut self) {
        for slot in 0..BUFFERS {
            if self.shown != Some(slot) {
                self.busy[slot] = false;
            }
        }
    }

    /// The reader let `slot` go.
    pub fn release(&mut self, slot: usize) {
        match self.stale_releases.get_mut(slot) {
            Some(0) => self.busy[slot] = false,
            Some(stale) => *stale -= 1,
            None => {}
        }
    }

    /// Begins a frame in a buffer whose contents are as old as
    /// [`age`](Self::age) says; `None` while none is free.
    pub fn begin(&mut self) -> Option<usize> {
        let slot = self.free()?;
        self.behind[slot] = Behind::Unknown;
        self.drawing = Some(slot);
        Some(slot)
    }

    /// Begins a frame to be drawn over the one shown; `None` while no buffer
    /// is free, nothing is shown, or what the free buffer holds is unknown.
    pub fn begin_over_shown(&mut self) -> Option<OverShown> {
        let slot = self.free()?;
        self.shown?;
        // Unknown until it is committed: a frame dropped half drawn is none.
        let Behind::By(stale) = core::mem::replace(&mut self.behind[slot], Behind::Unknown) else {
            return None;
        };
        self.drawing = Some(slot);
        Some(OverShown { slot, stale })
    }

    /// The frame drawn turned out to be the one shown, and is not committed.
    pub fn drop_unchanged(&mut self) {
        if let Some(slot) = self.drawing.take() {
            self.behind[slot] = Behind::By(Rects::NONE);
        }
    }

    /// Commits the frame drawn, which differs from the one shown within
    /// `damage` alone; `None` when no frame was begun.
    pub fn commit(&mut self, damage: &[DamageRect]) -> Option<Committed> {
        let slot = self.drawing.take()?;
        let first_use = !core::mem::replace(&mut self.registered[slot], true);
        self.busy[slot] = true;
        // The frame shown until now is this one everywhere but in the damage,
        // and the reader's to read until it says it has moved on.
        if let Some(was) = self.shown.replace(slot) {
            self.behind[was] = Behind::By(Rects::of(damage));
            self.busy[was] = true;
        }
        self.behind[slot] = Behind::By(Rects::NONE);
        for (other, age) in self.age.iter_mut().enumerate() {
            *age = match other == slot {
                true => 1,
                false => age.saturating_add(1),
            };
        }
        Some(Committed { slot, first_use })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopos_abi::damage::MAX_DAMAGE_REGIONS;

    fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> DamageRect {
        DamageRect { x0, y0, x1, y1 }
    }

    const WHOLE: DamageRect = DamageRect {
        x0: 0,
        y0: 0,
        x1: 99,
        y1: 99,
    };

    fn frame(swap: &mut Swap, damage: &[DamageRect]) -> Committed {
        swap.begin().expect("a free buffer");
        swap.commit(damage).expect("a frame begun")
    }

    #[test]
    fn the_first_frames_take_each_buffer_once_and_hand_it_over() {
        let mut swap = Swap::new();
        assert_eq!(swap.age(), 0);
        let first = frame(&mut swap, &[WHOLE]);
        assert!(first.first_use);
        let second = frame(&mut swap, &[WHOLE]);
        assert!(second.first_use);
        assert_ne!(first.slot, second.slot);
        swap.release(first.slot);
        assert!(!frame(&mut swap, &[WHOLE]).first_use);
    }

    #[test]
    fn no_frame_begins_while_the_reader_holds_both_buffers() {
        let mut swap = Swap::new();
        let first = frame(&mut swap, &[WHOLE]);
        let second = frame(&mut swap, &[WHOLE]);
        // The first is still the reader's until it says so.
        assert_eq!(swap.free(), None);
        assert_eq!(swap.begin(), None);
        assert_eq!(swap.begin_over_shown(), None);
        assert_eq!(swap.age(), 0);
        swap.release(first.slot);
        assert_eq!(swap.free(), Some(first.slot));
        assert_eq!(swap.shown(), Some(second.slot));
    }

    #[test]
    fn the_buffer_shown_is_never_drawn_into_even_released() {
        let mut swap = Swap::new();
        let first = frame(&mut swap, &[WHOLE]);
        swap.release(first.slot);
        assert_eq!(swap.free(), Some(1 - first.slot));
        let second = frame(&mut swap, &[WHOLE]);
        swap.release(second.slot);
        assert_eq!(swap.free(), None, "the one before it is not released");
    }

    #[test]
    fn a_commit_without_a_frame_begun_is_nothing() {
        let mut swap = Swap::new();
        assert_eq!(swap.commit(&[WHOLE]), None);
        frame(&mut swap, &[WHOLE]);
        assert_eq!(swap.commit(&[WHOLE]), None);
    }

    #[test]
    fn a_buffer_is_as_old_as_the_frames_since_it_was_committed() {
        let mut swap = Swap::new();
        let first = frame(&mut swap, &[WHOLE]);
        assert_eq!(swap.age(), 0, "the other has held no frame");
        let second = frame(&mut swap, &[WHOLE]);
        swap.release(first.slot);
        assert_eq!(swap.age(), 2);
        frame(&mut swap, &[WHOLE]);
        swap.release(second.slot);
        assert_eq!(swap.age(), 2);
    }

    #[test]
    fn a_frame_over_the_shown_one_is_owed_the_last_damage() {
        let mut swap = Swap::new();
        let first = frame(&mut swap, &[WHOLE]);
        assert_eq!(swap.begin_over_shown(), None, "the other holds nothing");
        let damage = [rect(1, 2, 3, 4), rect(10, 10, 20, 20)];
        let second = frame(&mut swap, &damage);
        swap.release(first.slot);
        let over = swap.begin_over_shown().expect("a frame to draw over");
        assert_eq!((over.slot, swap.shown()), (first.slot, Some(second.slot)));
        assert_eq!(over.stale.as_slice(), damage);
        let third = swap.commit(&[rect(5, 5, 6, 6)]).unwrap();
        swap.release(second.slot);
        let over = swap.begin_over_shown().unwrap();
        assert_eq!((over.slot, swap.shown()), (second.slot, Some(third.slot)));
        assert_eq!(over.stale.as_slice(), [rect(5, 5, 6, 6)]);
    }

    #[test]
    fn a_frame_begun_and_never_committed_leaves_its_buffer_unknown() {
        let mut swap = Swap::new();
        let first = frame(&mut swap, &[WHOLE]);
        frame(&mut swap, &[rect(0, 0, 9, 9)]);
        swap.release(first.slot);
        swap.begin_over_shown().expect("a frame to draw over");
        // Dropped half drawn.
        assert_eq!(swap.begin_over_shown(), None);
        assert_eq!(swap.begin(), Some(first.slot));
    }

    #[test]
    fn a_frame_dropped_unchanged_is_level_with_the_one_shown() {
        let mut swap = Swap::new();
        let first = frame(&mut swap, &[WHOLE]);
        let second = frame(&mut swap, &[WHOLE]);
        swap.release(first.slot);
        swap.begin().unwrap();
        swap.drop_unchanged();
        assert_eq!(swap.shown(), Some(second.slot));
        assert_eq!(swap.free(), Some(first.slot), "it was not committed");
        let over = swap.begin_over_shown().expect("it holds the shown frame");
        assert!(over.stale.as_slice().is_empty());
    }

    #[test]
    fn more_damage_than_fits_is_owed_as_its_bounds() {
        let mut swap = Swap::new();
        let first = frame(&mut swap, &[WHOLE]);
        let many: [DamageRect; MAX_DAMAGE_REGIONS + 1] =
            core::array::from_fn(|i| rect(i as i32 * 10, 0, i as i32 * 10 + 4, 4));
        frame(&mut swap, &many);
        swap.release(first.slot);
        let over = swap.begin_over_shown().unwrap();
        let reach = MAX_DAMAGE_REGIONS as i32 * 10 + 4;
        assert_eq!(over.stale.as_slice(), [rect(0, 0, reach, 4)]);
    }

    #[test]
    fn a_buffer_never_released_can_be_taken_back_but_not_the_one_shown() {
        let mut swap = Swap::new();
        frame(&mut swap, &[WHOLE]);
        let second = frame(&mut swap, &[rect(1, 1, 2, 2)]);
        assert_eq!(swap.free(), None);
        swap.reclaim();
        assert_eq!(swap.free(), Some(1 - second.slot));
        assert_eq!(swap.shown(), Some(second.slot));
        let over = swap
            .begin_over_shown()
            .expect("what it is behind by is known");
        assert_eq!(over.stale.as_slice(), [rect(1, 1, 2, 2)]);
    }

    #[test]
    fn a_release_of_a_replaced_buffer_frees_nothing_of_its_successor() {
        let mut swap = Swap::new();
        let first = frame(&mut swap, &[WHOLE]);
        let second = frame(&mut swap, &[WHOLE]);
        // The first's release is on its way as the buffers are replaced.
        swap.replace_buffers();
        assert_eq!(swap.shown(), None);
        let third = frame(&mut swap, &[WHOLE]);
        assert!(third.first_use);
        assert_eq!(third.slot, first.slot, "the slot is used again");
        let fourth = frame(&mut swap, &[WHOLE]);
        assert_eq!(fourth.slot, second.slot);
        // The release of the buffer the first slot held before.
        swap.release(first.slot);
        assert_eq!(swap.free(), None, "its successor is still the reader's");
        swap.release(first.slot);
        assert_eq!(swap.free(), Some(first.slot));
    }

    #[test]
    fn a_late_release_of_the_buffer_shown_does_not_free_it_once_replaced() {
        let mut swap = Swap::new();
        let first = frame(&mut swap, &[WHOLE]);
        swap.release(first.slot);
        let second = frame(&mut swap, &[WHOLE]);
        // Shown, so it was not free; replaced, it waits for a release of its
        // own, whatever was released before.
        assert_eq!(swap.free(), None);
        swap.release(first.slot);
        assert_eq!(swap.free(), Some(first.slot));
        frame(&mut swap, &[WHOLE]);
        assert_eq!(swap.free(), None);
        swap.release(second.slot);
        assert_eq!(swap.free(), Some(second.slot));
    }
}
