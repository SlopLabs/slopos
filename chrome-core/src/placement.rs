//! Where a new window opens; sizes and rects are its whole frame, title bar
//! included.

use crate::positioner::{Rect, Size};

/// The frame a window of `size`, which lays out no smaller than `min`, opens
/// at in `area`: centred, or `step` down and right of `after`, its app's
/// latest window, back at the area's edge before it would cross it; shrunk
/// to the area or to `min`, whichever is larger.
pub fn place(area: Rect, step: i32, size: Size, min: Size, after: Option<Rect>) -> Rect {
    let w = fit(size.w, area.w, min.w);
    let h = fit(size.h, area.h, min.h);
    let (x, y) = match after {
        None => (area.x + (area.w - w) / 2, area.y + (area.h - h) / 2),
        Some(latest) => {
            let mut x = latest.x + step;
            let mut y = latest.y + step;
            if y + h > area.bottom() {
                y = area.y;
            }
            if x + w > area.right() {
                x = area.x;
            }
            (x, y)
        }
    };
    Rect::new(x.max(area.x), y.max(area.y), w, h)
}

fn fit(wanted: i32, room: i32, least: i32) -> i32 {
    match wanted > room {
        true => room.max(least),
        false => wanted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEP: i32 = 28;
    const AREA: Rect = Rect::new(0, 25, 1920, 980);

    fn sizes() -> impl Iterator<Item = Size> {
        [120, 480, 900, 1280, 1919, 1920, 2400]
            .into_iter()
            .flat_map(|w| [90, 400, 700, 979, 980, 1400].map(|h| Size { w, h }))
    }

    fn inside(frame: Rect, area: Rect) -> bool {
        frame.x >= area.x
            && frame.y >= area.y
            && frame.right() <= area.right()
            && frame.bottom() <= area.bottom()
    }

    #[test]
    fn a_first_window_that_fits_opens_centred() {
        for size in sizes().filter(|s| s.w <= AREA.w && s.h <= AREA.h) {
            let frame = place(AREA, STEP, size, Size { w: 1, h: 1 }, None);
            assert_eq!((frame.w, frame.h), (size.w, size.h));
            let (cx, cy) = (frame.x * 2 + frame.w, frame.y * 2 + frame.h);
            let (ax, ay) = (AREA.x * 2 + AREA.w, AREA.y * 2 + AREA.h);
            assert!((cx - ax).abs() <= 1 && (cy - ay).abs() <= 1, "{frame:?}");
        }
    }

    #[test]
    fn a_later_window_cascades_by_the_step() {
        let size = Size { w: 640, h: 480 };
        let first = place(AREA, STEP, size, size, None);
        let second = place(AREA, STEP, size, size, Some(first));
        assert_eq!((second.x, second.y), (first.x + STEP, first.y + STEP));
        assert_eq!((second.w, second.h), (first.w, first.h));
    }

    #[test]
    fn a_cascade_never_crosses_the_dock_and_wraps_to_the_top() {
        let min = Size { w: 1, h: 1 };
        for size in sizes().filter(|s| s.w <= AREA.w && s.h <= AREA.h) {
            let mut latest = place(AREA, STEP, size, min, None);
            let mut wrapped = false;
            for _ in 0..200 {
                let next = place(AREA, STEP, size, min, Some(latest));
                assert!(inside(next, AREA), "{size:?} after {latest:?}: {next:?}");
                wrapped |= next.y < latest.y + STEP;
                latest = next;
            }
            assert!(wrapped, "{size:?} never wrapped");
        }
    }

    #[test]
    fn a_window_larger_than_the_area_is_shrunk_to_fit() {
        for size in sizes() {
            let frame = place(AREA, STEP, size, Size { w: 200, h: 150 }, None);
            assert_eq!(frame.w, size.w.min(AREA.w), "{size:?}");
            assert_eq!(frame.h, size.h.min(AREA.h), "{size:?}");
            assert!(inside(frame, AREA), "{size:?}: {frame:?}");
        }
    }

    #[test]
    fn a_minimum_larger_than_the_area_wins_and_keeps_the_title_bar_reachable() {
        let min = Size { w: 2000, h: 1100 };
        let frame = place(AREA, STEP, Size { w: 2400, h: 1400 }, min, None);
        assert_eq!((frame.w, frame.h), (2000, 1100));
        assert_eq!((frame.x, frame.y), (AREA.x, AREA.y));
        let next = place(AREA, STEP, Size { w: 2400, h: 1400 }, min, Some(frame));
        assert_eq!((next.x, next.y), (AREA.x, AREA.y));
    }

    #[test]
    fn a_cascade_from_a_window_dragged_off_the_area_starts_inside_it() {
        let size = Size { w: 640, h: 480 };
        let dragged = Rect::new(-500, -300, 640, 480);
        let next = place(AREA, STEP, size, size, Some(dragged));
        assert!(inside(next, AREA), "{next:?}");
    }
}
