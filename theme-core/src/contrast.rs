//! WCAG 2.2's relative luminance and contrast ratio.

use super::Color32;

/// 1.4.3's floor for text.
pub const TEXT: f64 = 4.5;
/// 1.4.11's floor for a control's edge, a focus indicator and large text.
pub const NON_TEXT: f64 = 3.0;

/// From 1 to 21; the order of the two does not matter. Both are read opaque:
/// composite a translucent colour with [`over`] first.
pub fn ratio(a: Color32, b: Color32) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

pub fn luminance(c: Color32) -> f64 {
    0.2126 * linear(c.red()) + 0.7152 * linear(c.green()) + 0.0722 * linear(c.blue())
}

/// `fg` composited onto opaque `bg` by `fg`'s alpha.
pub const fn over(fg: Color32, bg: Color32) -> Color32 {
    super::with_alpha(super::mix(bg, fg, fg.alpha()), 0xff)
}

fn linear(channel: u8) -> f64 {
    let c = channel as f64 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        // x^2.4 is x^2 times the fifth root of x^2: `core` has no `powf`.
        let base = (c + 0.055) / 1.055;
        let square = base * base;
        square * fifth_root(square)
    }
}

/// Newton's method from above, which `x^5` being convex makes monotone, for
/// `a` in (0, 1].
fn fifth_root(a: f64) -> f64 {
    let mut y = 1.0;
    loop {
        let y4 = y * y * y * y;
        let next = (4.0 * y + a / y4) / 5.0;
        if next >= y {
            return y;
        }
        y = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linearisation_matches_the_power_curve() {
        for channel in 0..=255u8 {
            let c = channel as f64 / 255.0;
            let want = if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            };
            assert!((linear(channel) - want).abs() < 1e-12, "channel {channel}");
        }
    }

    #[test]
    fn ratio_spans_one_to_twenty_one() {
        assert_eq!(ratio(Color32::BLACK, Color32::WHITE), 21.0);
        assert_eq!(ratio(Color32::WHITE, Color32::BLACK), 21.0);
        let grey = Color32::rgb(0x77, 0x77, 0x77);
        assert_eq!(ratio(grey, grey), 1.0);
    }

    /// The old disabled text on the old window surface.
    #[test]
    fn ratio_reproduces_a_known_pair() {
        let r = ratio(
            Color32::rgb(0x6b, 0x71, 0x7d),
            Color32::rgb(0x28, 0x2c, 0x33),
        );
        assert!((r - 2.86).abs() < 0.005, "{r}");
    }

    #[test]
    fn over_composites_by_alpha() {
        let half_white = Color32::new(0xff, 0xff, 0xff, 0x80);
        assert_eq!(
            over(half_white, Color32::BLACK),
            Color32::rgb(0x80, 0x80, 0x80)
        );
        assert_eq!(over(Color32::WHITE, Color32::BLACK), Color32::WHITE);
        assert_eq!(over(Color32::TRANSPARENT, Color32::BLACK), Color32::BLACK);
    }
}
