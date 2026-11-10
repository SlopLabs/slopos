//! Where two frames differ, as the rects a surface commits as damage.

use slopos_abi::damage::{DamageRect, MAX_DAMAGE_REGIONS};

/// As far apart, in pixels, as the changes of two rows one under the other
/// may lie and still share a band.
const BAND_SLACK: i32 = 32;

/// At most [`MAX_DAMAGE_REGIONS`] rects, as a commit carries.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rects {
    rects: [DamageRect; MAX_DAMAGE_REGIONS],
    count: usize,
}

impl Rects {
    pub const NONE: Self = Self {
        rects: [DamageRect::invalid(); MAX_DAMAGE_REGIONS],
        count: 0,
    };

    /// `rects`, or their bounds when there are more than fit.
    pub fn of(rects: &[DamageRect]) -> Self {
        let mut out = Self::NONE;
        if rects.len() <= MAX_DAMAGE_REGIONS {
            out.rects[..rects.len()].copy_from_slice(rects);
            out.count = rects.len();
        } else if let Some(bounds) = rects.iter().copied().reduce(|a, b| a.union(&b)) {
            out.rects[0] = bounds;
            out.count = 1;
        }
        out
    }

    #[inline]
    pub fn as_slice(&self) -> &[DamageRect] {
        &self.rects[..self.count]
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Joins a row's changed columns to the band above when they lie under it.
    fn add_row(&mut self, y: i32, x0: i32, x1: i32) {
        if let Some(last) = self.rects[..self.count].last_mut()
            && last.y1 + 1 == y
            && x0 <= last.x1 + BAND_SLACK
            && last.x0 <= x1 + BAND_SLACK
        {
            last.y1 = y;
            last.x0 = last.x0.min(x0);
            last.x1 = last.x1.max(x1);
            return;
        }
        if self.count == MAX_DAMAGE_REGIONS {
            self.merge_cheapest();
        }
        self.rects[self.count] = DamageRect {
            x0,
            y0: y,
            x1,
            y1: y,
        };
        self.count += 1;
    }

    /// Joins the two neighbouring bands whose union adds the fewest pixels.
    fn merge_cheapest(&mut self) {
        let rects = &mut self.rects[..self.count];
        let added = |a: &DamageRect, b: &DamageRect| a.combined_area(b) - a.area() - b.area();
        let Some(at) =
            (0..rects.len().saturating_sub(1)).min_by_key(|&i| added(&rects[i], &rects[i + 1]))
        else {
            return;
        };
        rects[at] = rects[at].union(&rects[at + 1]);
        rects.copy_within(at + 2.., at + 1);
        self.count -= 1;
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    /// Bytes a row.
    pub pitch: usize,
    pub bytes_pp: u8,
}

impl Geometry {
    /// Every pixel of a frame.
    pub fn whole(&self) -> DamageRect {
        DamageRect {
            x0: 0,
            y0: 0,
            x1: self.width as i32 - 1,
            y1: self.height as i32 - 1,
        }
    }

    /// Whether a frame of `len` bytes holds every row.
    fn holds(&self, len: usize) -> bool {
        let row_bytes = self.width as usize * usize::from(self.bytes_pp);
        let rows = self.height as usize;
        rows == 0
            || self
                .pitch
                .checked_mul(rows - 1)
                .and_then(|above| above.checked_add(row_bytes))
                .is_some_and(|needed| self.pitch >= row_bytes && len >= needed)
    }
}

/// Where `now` and `was` differ: bands of rows, top to bottom, none sharing
/// a row, that together hold every pixel that differs. A frame too short for
/// its geometry differs everywhere.
pub fn changed(now: &[u8], was: &[u8], geometry: Geometry) -> Rects {
    changed_within(now, was, geometry, geometry.whole())
}

/// [`changed`] of what lies in `within` alone.
pub fn changed_within(now: &[u8], was: &[u8], geometry: Geometry, within: DamageRect) -> Rects {
    let within = within.clip(geometry.width as i32, geometry.height as i32);
    if !within.is_valid() {
        return Rects::NONE;
    }
    if !geometry.holds(now.len()) || !geometry.holds(was.len()) {
        return Rects::of(&[within]);
    }
    let bytes_pp = usize::from(geometry.bytes_pp);
    let (left, len) = (
        within.x0 as usize * bytes_pp,
        (within.x1 - within.x0 + 1) as usize * bytes_pp,
    );
    let mut changed = Rects::NONE;
    for y in within.y0..=within.y1 {
        let at = y as usize * geometry.pitch + left;
        let (a, b) = (&now[at..at + len], &was[at..at + len]);
        if a == b {
            continue;
        }
        let differs = |(p, q): (&u8, &u8)| p != q;
        let first = a.iter().zip(b).position(differs).unwrap_or(0);
        let last = a.iter().zip(b).rposition(differs).unwrap_or(len - 1);
        let column = |byte: usize| within.x0 + (byte / bytes_pp) as i32;
        changed.add_row(y, column(first), column(last));
    }
    changed
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    const W: u32 = 96;
    const H: u32 = 80;
    const PITCH: usize = W as usize * 4 + 8;

    fn frame() -> Vec<u8> {
        vec![0x40; PITCH * H as usize]
    }

    fn poke(frame: &mut [u8], x: u32, y: u32) {
        frame[y as usize * PITCH + x as usize * 4 + 1] ^= 0xff;
    }

    const GEOMETRY: Geometry = Geometry {
        width: W,
        height: H,
        pitch: PITCH,
        bytes_pp: 4,
    };

    fn diff(now: &[u8], was: &[u8]) -> Rects {
        changed(now, was, GEOMETRY)
    }

    fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> DamageRect {
        DamageRect { x0, y0, x1, y1 }
    }

    #[test]
    fn equal_frames_change_nothing() {
        assert!(diff(&frame(), &frame()).is_empty());
    }

    #[test]
    fn row_padding_is_no_change() {
        let (mut now, was) = (frame(), frame());
        now[PITCH - 1] ^= 0xff;
        assert!(diff(&now, &was).is_empty());
    }

    #[test]
    fn one_pixel_is_one_rect_of_one_pixel() {
        let (mut now, was) = (frame(), frame());
        poke(&mut now, 17, 33);
        assert_eq!(diff(&now, &was).as_slice(), [rect(17, 33, 17, 33)]);
    }

    #[test]
    fn rows_one_under_another_share_a_band_as_wide_as_they_reach() {
        let (mut now, was) = (frame(), frame());
        for y in 10..20 {
            poke(&mut now, 30 + y, y);
            poke(&mut now, 60, y);
        }
        assert_eq!(diff(&now, &was).as_slice(), [rect(40, 10, 60, 19)]);
    }

    #[test]
    fn changes_apart_stay_apart() {
        let (mut now, was) = (frame(), frame());
        poke(&mut now, 2, 5);
        poke(&mut now, 90, 6);
        poke(&mut now, 40, 70);
        assert_eq!(
            diff(&now, &was).as_slice(),
            [rect(2, 5, 2, 5), rect(90, 6, 90, 6), rect(40, 70, 40, 70)]
        );
    }

    #[test]
    fn more_bands_than_fit_merge_and_still_hold_every_change() {
        let (mut now, was) = (frame(), frame());
        let mut seed = 0x2545_f491u32;
        let mut next = |below: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % below
        };
        let mut poked = Vec::new();
        for _ in 0..60 {
            let (x, y) = (next(W), next(H));
            poke(&mut now, x, y);
            poked.push((x as i32, y as i32));
        }
        let found = diff(&now, &was);
        assert!(found.as_slice().len() <= MAX_DAMAGE_REGIONS);
        for y in 0..H as i32 {
            for x in 0..W as i32 {
                let at = y as usize * PITCH + x as usize * 4;
                if now[at..at + 4] != was[at..at + 4] {
                    assert!(
                        found.as_slice().iter().any(|r| r.contains(x, y)),
                        "({x}, {y}) differs outside every rect"
                    );
                }
            }
        }
        for (i, r) in found.as_slice().iter().enumerate() {
            assert!(r.is_valid() && r.x1 < W as i32 && r.y1 < H as i32);
            assert!(found.as_slice()[..i].iter().all(|above| above.y1 < r.y0));
        }
        assert!(!poked.is_empty());
    }

    #[test]
    fn a_frame_too_short_for_its_geometry_differs_everywhere() {
        let was = frame();
        let now = &was[..was.len() - PITCH];
        assert_eq!(diff(now, &was).as_slice(), [rect(0, 0, 95, 79)]);
    }

    #[test]
    fn three_byte_pixels_are_counted_in_pixels() {
        let geometry = Geometry {
            pitch: W as usize * 3,
            bytes_pp: 3,
            ..GEOMETRY
        };
        let was = vec![0u8; geometry.pitch * H as usize];
        let mut now = was.clone();
        now[4 * geometry.pitch + 10 * 3 + 2] = 1;
        assert_eq!(
            changed(&now, &was, geometry).as_slice(),
            [rect(10, 4, 10, 4)]
        );
    }

    #[test]
    fn only_what_lies_within_is_looked_at() {
        let (mut now, was) = (frame(), frame());
        poke(&mut now, 5, 5);
        poke(&mut now, 40, 20);
        poke(&mut now, 44, 21);
        poke(&mut now, 90, 70);
        let within = rect(30, 10, 60, 30);
        assert_eq!(
            changed_within(&now, &was, GEOMETRY, within).as_slice(),
            [rect(40, 20, 44, 21)]
        );
        let off = rect(200, 200, 300, 300);
        assert!(changed_within(&now, &was, GEOMETRY, off).is_empty());
        let over = rect(-50, -50, 5, 5);
        assert_eq!(
            changed_within(&now, &was, GEOMETRY, over).as_slice(),
            [rect(5, 5, 5, 5)]
        );
    }

    #[test]
    fn more_rects_than_fit_are_their_bounds() {
        let many: [DamageRect; MAX_DAMAGE_REGIONS + 1] =
            core::array::from_fn(|i| rect(i as i32 * 10, 0, i as i32 * 10 + 4, 4));
        let reach = MAX_DAMAGE_REGIONS as i32 * 10 + 4;
        assert_eq!(Rects::of(&many).as_slice(), [rect(0, 0, reach, 4)]);
        assert_eq!(Rects::of(&many[..3]).as_slice(), &many[..3]);
    }
}
