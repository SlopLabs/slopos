//! The whole screen: the first frame, which is the boot splash's last, and
//! what each moment after it repaints, with the damage the present must
//! show.
//!
//! The ring is the splash's ([`slopos_splash_core::Dial`]). Each segment's
//! pixels are measured once, so a frame rewrites only the segments whose ink
//! changed. Numbers and words are set over black, their box cleared first, so
//! dimming one is drawing it again darker.

extern crate alloc;

use alloc::vec::Vec;

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32, EncodedPixel};
use slopos_abi::pixel::PixelFormat;
use slopos_font::FontRenderer;
use slopos_splash_core::{self as splash, ALL, BACKDROP, SEGMENTS};
use slopos_theme_core::{self as theme, Theme};

use crate::layout::Layout;
use crate::spin::{self, Spin, ease, progress};
use crate::wheel::{self, POCKETS};

/// How much of the theme's secondary text a pocket's number shows, out of
/// 255, which holds it to 4.5:1 on black, and how far towards
/// [`splash::lit`] the light passing lifts it.
const NUMERAL: u8 = 172;
const NUMERAL_LIFT: u8 = 170;

/// The screen-sized buffer `/bin/roulette` presents from: four bytes a pixel.
pub struct Screen<'a> {
    pub px: &'a mut [u8],
    pub width: i32,
    pub height: i32,
    pub pitch: usize,
    pub format: PixelFormat,
}

impl Screen<'_> {
    fn write(&mut self, at: usize, pixel: u32) {
        self.px[at..at + 4].copy_from_slice(&pixel.to_le_bytes());
    }

    /// Black out `area`, which lies on the screen or is empty.
    fn clear(&mut self, area: DamageRect) {
        if !area.is_valid() {
            return;
        }
        let black = self.format.encode(BACKDROP).0.to_le_bytes();
        for y in area.y0..=area.y1 {
            let row = y as usize * self.pitch;
            let span = &mut self.px[row + area.x0 as usize * 4..row + (area.x1 as usize + 1) * 4];
            for px in span.chunks_exact_mut(4) {
                px.copy_from_slice(&black);
            }
        }
    }
}

impl Canvas for Screen<'_> {
    fn width(&self) -> u32 {
        self.width as u32
    }

    fn height(&self) -> u32 {
        self.height as u32
    }

    fn pitch_bytes(&self) -> usize {
        self.pitch
    }

    fn bytes_per_pixel(&self) -> u8 {
        4
    }

    fn pixel_format(&self) -> PixelFormat {
        self.format
    }

    fn write_encoded_at(&mut self, byte_offset: usize, pixel: EncodedPixel) {
        if let Some(p) = self.px.get_mut(byte_offset..byte_offset + 4) {
            p.copy_from_slice(&pixel.0.to_le_bytes());
        }
    }

    fn read_encoded_at(&self, byte_offset: usize) -> u32 {
        self.px
            .get(byte_offset..byte_offset + 4)
            .map_or(0, |p| u32::from_le_bytes([p[0], p[1], p[2], p[3]]))
    }
}

/// Where a line of text is set, and the box its ink stays within.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Label {
    pub x: i32,
    pub y: i32,
    pub size: u16,
    pub bold: bool,
    pub area: DamageRect,
}

/// The two faces the screen is set in; without them the wheel still runs,
/// unlabelled.
pub struct Fonts<'a> {
    pub regular: Option<FontRenderer<'a>>,
    pub semibold: Option<FontRenderer<'a>>,
}

impl<'a> Fonts<'a> {
    pub fn new(regular: Option<&'a [u8]>, semibold: Option<&'a [u8]>) -> Self {
        Self {
            regular: regular.and_then(FontRenderer::new),
            semibold: semibold.and_then(FontRenderer::new),
        }
    }

    fn face(&mut self, bold: bool) -> Option<&mut FontRenderer<'a>> {
        if bold && self.semibold.is_some() {
            self.semibold.as_mut()
        } else {
            self.regular.as_mut().or(self.semibold.as_mut())
        }
    }

    /// `text` centred on `cx`, the top of its line at `y`.
    fn label(&mut self, bold: bool, text: &str, size: u16, cx: i32, y: i32) -> Option<Label> {
        let face = self.face(bold)?;
        let w = face.text_width(text, size);
        let x = cx - w / 2;
        Some(Label {
            x,
            y,
            size,
            bold,
            area: DamageRect {
                x0: x - 2,
                y0: y - 2,
                x1: x + w + 2,
                y1: y + face.line_height(size) + 2,
            },
        })
    }

    /// `text` centred on `(cx, cy)` by its cap height.
    fn label_on(&mut self, bold: bool, text: &str, size: u16, cx: i32, cy: i32) -> Option<Label> {
        let ascent = self.face(bold)?.ascent(size);
        let cap = size as i32 * 72 / 100;
        self.label(bold, text, size, cx, cy - ascent + cap / 2)
    }

    fn draw(&mut self, screen: &mut Screen, label: &Label, text: &str, color: Color32) {
        if let Some(face) = self.face(label.bold) {
            face.draw_text(
                screen,
                label.x,
                label.y,
                text,
                label.size,
                color,
                Color32::TRANSPARENT,
            );
        }
    }
}

/// What the screen says, as it says it.
pub mod words {
    pub const TITLE: &str = "Wheel of Fate";
    pub const RULE: &str = "Odd starts \u{b7} Even restarts";
    pub const SPINNING: &str = "Spinning\u{2026}";
    pub const WIN: &str = "You win.";
    pub const LOSE: &str = "The house wins.";
    /// Under the ring once it has landed.
    pub const ODD_STARTS: &str = "Odd \u{b7} starting SlopOS";
    pub const EVEN_RESTARTS: &str = "Even \u{b7} restarting";
    /// On a console, after the verdict, when the computer restarts.
    pub const RESTARTING: &str = "Restarting for another spin\u{2026}";
    pub const ODD: &str = "odd";
    pub const EVEN: &str = "even";
}

/// What the line under the ring says once the wheel has landed.
pub const fn caption(starts: bool) -> &'static str {
    if starts {
        words::ODD_STARTS
    } else {
        words::EVEN_RESTARTS
    }
}

/// `n` in decimal.
pub fn decimal(n: u8, buf: &mut [u8; 3]) -> &str {
    let mut at = buf.len();
    let mut n = n;
    loop {
        at -= 1;
        buf[at] = b'0' + n % 10;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    core::str::from_utf8(&buf[at..]).unwrap_or("")
}

/// `"7 \u{2014} odd. You win."` into `buf`.
pub fn verdict(number: u8, buf: &mut [u8; 64]) -> &str {
    let mut digits = [0u8; 3];
    let starts = wheel::starts(number);
    let parts = [
        decimal(number, &mut digits),
        " \u{2014} ",
        if starts { words::ODD } else { words::EVEN },
        ". ",
        if starts { words::WIN } else { words::LOSE },
    ];
    let mut len = 0;
    for part in parts {
        let end = len + part.len();
        if end > buf.len() {
            break;
        }
        buf[len..end].copy_from_slice(part.as_bytes());
        len = end;
    }
    core::str::from_utf8(&buf[..len]).unwrap_or("")
}

/// `0..=256` as `0..=255`.
const fn level(v: u32) -> u8 {
    (if v > 256 { 256 } else { v } * 255 / 256) as u8
}

/// `a` of `b`, both out of 255.
const fn of(a: u8, b: u8) -> u8 {
    ((a as u32 * b as u32 + 127) / 255) as u8
}

/// A pocket's number at rest, between the unlit ring and the words.
pub fn numeral_ink() -> Color32 {
    splash::over_backdrop(theme::current().text_secondary, NUMERAL)
}

struct Palette {
    lit: Color32,
    unlit: Color32,
    numeral: Color32,
    secondary: Color32,
    win: Color32,
    lose: Color32,
}

impl Palette {
    fn new(t: &Theme) -> Self {
        Self {
            lit: splash::lit(),
            unlit: splash::unlit(),
            numeral: numeral_ink(),
            secondary: t.text_secondary,
            win: t.success,
            lose: t.danger,
        }
    }

    fn tone(&self, starts: bool) -> Color32 {
        if starts { self.win } else { self.lose }
    }
}

/// The inks a moment shows, compared frame to frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Inks {
    ring: [Color32; SEGMENTS],
    numerals: [Color32; POCKETS],
    number: Color32,
    /// Which line is under the ring, the rule or the verdict, and its ink.
    caption: (bool, Color32),
    logo: u8,
}

/// What a frame changed, for the present.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Damage {
    pub dial: Option<DamageRect>,
    pub caption: Option<DamageRect>,
    pub logo: Option<DamageRect>,
}

fn join(a: Option<DamageRect>, b: Option<DamageRect>) -> Option<DamageRect> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.union(&b)),
        (a, b) => a.or(b),
    }
}

impl Damage {
    pub fn is_empty(&self) -> bool {
        self.dial.is_none() && self.caption.is_none() && self.logo.is_none()
    }

    pub fn rects(&self) -> impl Iterator<Item = DamageRect> {
        self.dial.into_iter().chain(self.caption).chain(self.logo)
    }

    pub fn union(self, other: Damage) -> Damage {
        Damage {
            dial: join(self.dial, other.dial),
            caption: join(self.caption, other.caption),
            logo: join(self.logo, other.logo),
        }
    }
}

pub struct Scene<'a> {
    layout: Layout,
    spin: Spin,
    fonts: Fonts<'a>,
    palette: Palette,
    size: (i32, i32),
    /// Each segment's pixels, as a byte offset into the screen and how much
    /// of the pixel the segment covers.
    band: [Vec<(u32, u8)>; SEGMENTS],
    segment_box: [DamageRect; SEGMENTS],
    numerals: [Option<Label>; POCKETS],
    number: Option<Label>,
    rule: Option<Label>,
    verdict: Option<Label>,
    shown: Inks,
}

impl<'a> Scene<'a> {
    pub fn new(layout: Layout, screen: &Screen, fate: u32, mut fonts: Fonts<'a>) -> Self {
        let spin = Spin::new(fate);
        let (w, h) = (screen.width, screen.height);
        let dial = layout.boot.dial;
        let mut band: [Vec<(u32, u8)>; SEGMENTS] = Default::default();
        dial.for_each_pixel(dial.bounds().clip(w, h), |x, y, k, cover| {
            band[k].push(((y as usize * screen.pitch + x as usize * 4) as u32, cover));
        });
        let numerals = core::array::from_fn(|k| {
            let (at, size) = layout.numerals?;
            let a = k as f32 * core::f32::consts::TAU / POCKETS as f32;
            let x = dial.cx + libm::roundf(at as f32 * libm::sinf(a)) as i32;
            let y = dial.cy - libm::roundf(at as f32 * libm::cosf(a)) as i32;
            let mut digits = [0u8; 3];
            fonts.label_on(false, decimal(wheel::number(k), &mut digits), size, x, y)
        });
        let mut digits = [0u8; 3];
        let number = fonts.label_on(
            false,
            decimal(spin.number(), &mut digits),
            layout.number,
            dial.cx,
            dial.cy,
        );
        let line = |fonts: &mut Fonts, text| {
            let (y, size) = layout.caption?;
            fonts.label(false, text, size, dial.cx, y)
        };
        let rule = line(&mut fonts, words::RULE);
        let verdict = line(&mut fonts, caption(spin.starts()));
        Self {
            segment_box: core::array::from_fn(|k| dial.segment_bounds(k).clip(w, h)),
            band,
            numerals,
            number,
            rule,
            verdict,
            spin,
            fonts,
            palette: Palette::new(theme::current()),
            size: (w, h),
            layout,
            shown: Inks {
                ring: splash::boot_inks(100),
                numerals: [BACKDROP; POCKETS],
                number: BACKDROP,
                caption: (false, BACKDROP),
                logo: u8::MAX,
            },
        }
    }

    /// Every box a number or a line of words inks.
    #[cfg(test)]
    pub(crate) fn label_areas(&self) -> impl Iterator<Item = DamageRect> + '_ {
        self.numerals
            .iter()
            .chain([&self.number, &self.rule, &self.verdict])
            .flatten()
            .map(|l| l.area)
    }

    /// Paint the whole screen as the boot splash left it: black, the ring
    /// lit all round and the logo.
    pub fn paint_all(&mut self, screen: &mut Screen) {
        let boot = self.layout.boot;
        screen.clear(DamageRect {
            x0: 0,
            y0: 0,
            x1: self.size.0 - 1,
            y1: self.size.1 - 1,
        });
        boot.dial.paint(screen, &splash::boot_inks(100), ALL);
        boot.logo.paint(screen, u8::MAX);
        self.shown = Inks {
            ring: splash::boot_inks(100),
            numerals: [BACKDROP; POCKETS],
            number: BACKDROP,
            caption: (false, BACKDROP),
            logo: u8::MAX,
        };
    }

    /// Out of 255, how much of everything is still on screen `t_ms` in.
    fn fade(t_ms: u32) -> u8 {
        255 - level(ease(progress(
            t_ms.saturating_sub(spin::FADE_AT),
            spin::FADE_MS,
        )))
    }

    /// Out of 255, how far the landed pocket has taken the verdict's colour.
    fn tint(t_ms: u32) -> u8 {
        if t_ms < spin::LAND_MS {
            0
        } else {
            level(ease(progress(t_ms - spin::LAND_MS, spin::SETTLE_MS)))
        }
    }

    fn inks(&self, t_ms: u32) -> Inks {
        let p = &self.palette;
        let fade = Self::fade(t_ms);
        let tint = Self::tint(t_ms);
        let landed = |k: usize| t_ms >= spin::LAND_MS && k == self.spin.pocket();
        let tone = p.tone(self.spin.starts());
        let appear = level(ease(progress(t_ms, spin::INTRO_MS)));

        let ring = core::array::from_fn(|k| {
            let lift = if t_ms >= spin::INTRO_MS {
                (self.spin.glow(t_ms, k) * 255.0 + 0.5) as u8
            } else if k == 0 {
                u8::MAX
            } else {
                u8::MAX - appear
            };
            let mut ink = theme::mix(p.unlit, p.lit, lift);
            if landed(k) {
                ink = theme::mix(p.lit, tone, tint);
            }
            splash::over_backdrop(ink, fade)
        });

        let numerals = core::array::from_fn(|k| {
            // In sixteen steps, so a passing light repaints a number a few
            // times rather than every frame.
            let glow = (self.spin.glow(t_ms, k) * 16.0) as u32;
            let mut ink = theme::mix(p.numeral, p.lit, (glow * NUMERAL_LIFT as u32 / 16) as u8);
            if landed(k) {
                ink = theme::mix(ink, tone, tint);
            }
            splash::over_backdrop(ink, of(appear, fade))
        });

        let number = splash::over_backdrop(p.lit, of(tint, fade));

        let caption = if t_ms < spin::LAND_MS {
            (false, splash::over_backdrop(p.secondary, appear))
        } else {
            let into = t_ms - spin::LAND_MS;
            let half = spin::SETTLE_MS / 2;
            if into < half {
                let out = 255 - level(ease(progress(into, half)));
                (false, splash::over_backdrop(p.secondary, out))
            } else {
                let came = level(ease(progress(into - half, half)));
                (true, splash::over_backdrop(tone, of(came, fade)))
            }
        };

        Inks {
            ring,
            numerals,
            number,
            caption,
            logo: fade,
        }
    }

    /// Clear `label`'s box and set `text` in it in `ink`, unless the ink is
    /// the backdrop's.
    fn set(&mut self, screen: &mut Screen, label: Label, text: &str, ink: Color32) -> DamageRect {
        let area = label.area.clip(self.size.0, self.size.1);
        screen.clear(area);
        if ink != BACKDROP {
            self.fonts.draw(screen, &label, text, ink);
        }
        area
    }

    /// Repaint what `t_ms` changes; the damage is what the present must show.
    pub fn paint(&mut self, screen: &mut Screen, t_ms: u32) -> Damage {
        let now = self.inks(t_ms);
        let was = self.shown;
        let mut damage = Damage::default();
        let format = screen.format;

        for k in 0..SEGMENTS {
            if now.ring[k] == was.ring[k] {
                continue;
            }
            for &(at, cover) in &self.band[k] {
                let ink = format.encode(splash::over_backdrop(now.ring[k], cover)).0;
                screen.write(at as usize, ink);
            }
            damage.dial = join(damage.dial, Some(self.segment_box[k]));
        }

        for k in 0..POCKETS {
            if now.numerals[k] == was.numerals[k] {
                continue;
            }
            if let Some(label) = self.numerals[k] {
                let mut digits = [0u8; 3];
                let text = decimal(wheel::number(k), &mut digits);
                let area = self.set(screen, label, text, now.numerals[k]);
                damage.dial = join(damage.dial, Some(area));
            }
        }

        if now.number != was.number
            && let Some(label) = self.number
        {
            let mut digits = [0u8; 3];
            let text = decimal(self.spin.number(), &mut digits);
            let area = self.set(screen, label, text, now.number);
            damage.dial = join(damage.dial, Some(area));
        }

        if now.caption != was.caption {
            // The rule and the verdict share the line: clear both, set one.
            let area = join(self.rule.map(|l| l.area), self.verdict.map(|l| l.area));
            if let Some(area) = area.map(|a| a.clip(self.size.0, self.size.1)) {
                screen.clear(area);
                damage.caption = Some(area);
            }
            let (verdict, ink) = now.caption;
            let (label, text) = if verdict {
                (self.verdict, caption(self.spin.starts()))
            } else {
                (self.rule, words::RULE)
            };
            if let Some(label) = label
                && ink != BACKDROP
            {
                self.fonts.draw(screen, &label, text, ink);
            }
        }

        if now.logo != was.logo {
            let logo = self.layout.boot.logo;
            logo.paint(screen, now.logo);
            damage.logo = Some(logo.bounds().clip(self.size.0, self.size.1));
        }

        self.shown = now;
        damage
    }
}
