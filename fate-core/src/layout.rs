//! Where everything goes on a screen of a given size: the boot's ring and
//! logo where the splash put them, the pockets' numbers round the ring, the
//! landed number in its middle and one line of words under it.

use slopos_splash_core::Layout as Boot;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    pub boot: Boot,
    /// The pockets' numbers: the radius their centres sit on, and their size.
    pub numerals: Option<(i32, u16)>,
    /// The landed number's size, set in the ring's middle.
    pub number: u16,
    /// The line of words: the top of its line, and its size.
    pub caption: Option<(i32, u16)>,
}

pub fn layout(width: u32, height: u32) -> Layout {
    let boot = Boot::for_screen(width, height);
    let dial = boot.dial;
    let r = dial.radius;
    let size = (r * 13 / 100).clamp(11, 40);
    let gap = (r / 16).max(4);
    // A number's box reaches about its size out from its centre, so this
    // keeps every number clear of the ring.
    let at = r + dial.width / 2 + gap + size;
    let below = dial.cy + at + size + 2 * gap;
    let line = size * 13 / 10;
    let fits = |x0: i32, y0: i32, x1: i32, y1: i32| {
        x0 >= 0 && y0 >= 0 && x1 < width as i32 && y1 < boot.logo.y - gap
    };
    let numerals = fits(
        dial.cx - at - size,
        dial.cy - at - size,
        dial.cx + at + size,
        dial.cy + at + size,
    )
    .then_some((at, size as u16));
    let caption =
        (numerals.is_some() && fits(0, below, 0, below + line)).then_some((below, size as u16));
    Layout {
        boot,
        numerals,
        number: (r * 62 / 100).clamp(12, 220) as u16,
        caption,
    }
}
