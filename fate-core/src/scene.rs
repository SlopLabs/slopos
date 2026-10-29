//! The whole screen: what is painted once, what each frame repaints, and the
//! damage each frame hands the present.
//!
//! The wheel lives in two pixmaps the size of [`Layout::wheel_box`]: `base`,
//! everything that never moves (backdrop, shadow, rim, hub), and `frame`,
//! `base` with the turning parts laid over it. A frame rewrites only what
//! turns — the pockets, the pegs, the numbers and the flapper — so its cost is
//! the band of pockets and not the screen.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::f32::consts::TAU;

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32, EncodedPixel};
use slopos_abi::pixel::PixelFormat;
use slopos_font::FontRenderer;
use slopos_theme_core::{self as theme, Theme};

use crate::layout::{Layout, Rect};
use crate::paint::{Mask, Pixmap, Place, blit, mix, weight};
use crate::spin::{self, Phase, Spin};
use crate::wheel::{self, POCKETS, TURN};

/// Radial shading steps across the band of pockets.
const SHADES: usize = 32;
/// How long the pocket the wheel stopped on takes to light, and the hub's
/// number to come up.
const GLOW_MS: u32 = 300;
const REVEAL_MS: u32 = 240;
/// Steps the hub's number fades in by; each is a repaint of the hub alone.
const REVEAL_STEPS: u32 = 6;
/// Half a divider's width plus half a pixel of edge, in eighths of a pixel:
/// a 1.25 px line.
const DIVIDER_REACH: u32 = 9;
/// The flapper's furthest swing, in radians.
const FLAP_SWING: f32 = 0.42;

/// The screen-sized buffer `/bin/roulette` presents from: four bytes a pixel.
pub struct Screen<'a> {
    pub px: &'a mut [u8],
    pub width: i32,
    pub height: i32,
    pub pitch: usize,
    pub format: PixelFormat,
}

impl Screen<'_> {
    /// `from`, whose top-left goes at `(x, y)`, clipped to the screen.
    fn put(&mut self, from: &Pixmap, x: i32, y: i32) {
        let x0 = x.max(0);
        let x1 = (x + from.w).min(self.width);
        if x0 >= x1 {
            return;
        }
        for row in y.max(0)..(y + from.h).min(self.height) {
            let src = ((row - y) * from.w + (x0 - x)) as usize;
            let src = &from.px[src..src + (x1 - x0) as usize];
            let dst = row as usize * self.pitch + x0 as usize * 4;
            let dst = &mut self.px[dst..dst + src.len() * 4];
            for (d, s) in dst.chunks_exact_mut(4).zip(src) {
                d.copy_from_slice(&s.to_le_bytes());
            }
        }
    }

    /// The pixels under `rect`, which lies on the screen.
    fn take(&self, rect: DamageRect) -> Pixmap {
        let mut out = Pixmap::new(rect.x1 - rect.x0 + 1, rect.y1 - rect.y0 + 1, self.format);
        for row in 0..out.h {
            let src = (rect.y0 + row) as usize * self.pitch + rect.x0 as usize * 4;
            let dst = (row * out.w) as usize;
            for (i, p) in self.px[src..src + out.w as usize * 4]
                .chunks_exact(4)
                .enumerate()
            {
                out.px[dst + i] = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
            }
        }
        out
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

/// The two faces the screen is set in; without them the wheel still turns,
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

    /// `text` centred on `cx`, its line's top at `y`, over what is there.
    fn centred<T: Canvas>(
        &mut self,
        target: &mut T,
        bold: bool,
        cx: i32,
        y: i32,
        text: &str,
        size: u16,
        color: Color32,
    ) {
        if let Some(face) = self.face(bold) {
            let x = cx - face.text_width(text, size) / 2;
            face.draw_text(target, x, y, text, size, color, Color32::TRANSPARENT);
        }
    }

    /// `text` centred on `(cx, cy)` by its cap height.
    fn centred_on<T: Canvas>(
        &mut self,
        target: &mut T,
        cx: i32,
        cy: i32,
        text: &str,
        size: u16,
        color: Color32,
    ) {
        if let Some(face) = self.face(true) {
            let cap = size as i32 * 72 / 100;
            let y = cy - face.ascent(size) + cap / 2;
            let x = cx - face.text_width(text, size) / 2;
            face.draw_text(target, x, y, text, size, color, Color32::TRANSPARENT);
        }
    }
}

/// What the screen says, as it says it.
pub mod words {
    pub const TITLE: &str = "Wheel of Fate";
    pub const RULE: &str = "Odd numbers start SlopOS. Even numbers restart the computer.";
    pub const HINT: &str = "Esc shows the boot log";
    pub const SPINNING: &str = "Spinning\u{2026}";
    pub const WIN: &str = "You win.";
    pub const LOSE: &str = "The house wins.";
    pub const STARTING: &str = "Starting SlopOS\u{2026}";
    pub const RESTARTING: &str = "Restarting for another spin\u{2026}";
    pub const ODD: &str = "odd";
    pub const EVEN: &str = "even";
    pub const UNKNOWN: &str = "?";
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
    let parts = [
        decimal(number, &mut digits),
        " \u{2014} ",
        if wheel::starts(number) {
            words::ODD
        } else {
            words::EVEN
        },
        ". ",
        if wheel::starts(number) {
            words::WIN
        } else {
            words::LOSE
        },
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

/// The theme's colours for each part, and the pixels they encode to.
struct Palette {
    backdrop: Color32,
    spotlight: Color32,
    text: Color32,
    secondary: Color32,
    hint: Color32,
    win: Color32,
    lose: Color32,
    track: Color32,
    hub: Color32,
    hub_ring: Color32,
    /// Each pocket's colour at each shade from hub to rim.
    pockets: [[u32; SHADES]; POCKETS],
    /// What every pocket blurs into, shade by shade.
    smear: [u32; SHADES],
    lit: u32,
    dim: u32,
    divider: u32,
    rim: u32,
    rim_edge: u32,
    rim_inner: u32,
    shadow: u32,
    peg: u32,
    number: u32,
    flapper: u32,
    flapper_edge: u32,
}

/// A pocket's colour: the theme's success for a number that starts the
/// system, its danger for one that restarts it, each sunk toward the backdrop
/// so a dark number reads on both.
pub fn pocket_color(t: &Theme, starts: bool) -> Color32 {
    if starts {
        theme::mix(t.success, t.surface_sunken, 64)
    } else {
        theme::mix(t.danger, t.surface_sunken, 104)
    }
}

fn shade(color: Color32, step: usize) -> Color32 {
    let lift = 205 + 50 * step as u32 / (SHADES as u32 - 1);
    let ch = |c: u8| (u32::from(c) * lift / 255) as u8;
    Color32::rgb(ch(color.red()), ch(color.green()), ch(color.blue()))
}

impl Palette {
    fn new(t: &Theme, format: PixelFormat) -> Self {
        let enc = |c: Color32| format.encode(c).0;
        let (win, lose) = (pocket_color(t, true), pocket_color(t, false));
        let mut pockets = [[0; SHADES]; POCKETS];
        let mut smear = [0; SHADES];
        for (k, row) in pockets.iter_mut().enumerate() {
            let base = pocket_color(t, wheel::starts(wheel::NUMBERS[k]));
            for (s, px) in row.iter_mut().enumerate() {
                *px = enc(shade(base, s));
            }
        }
        for (s, px) in smear.iter_mut().enumerate() {
            *px = enc(shade(theme::mix(win, lose, 128), s));
        }
        Self {
            backdrop: t.surface_sunken,
            spotlight: theme::mix(t.surface, t.surface_raised, 96),
            text: t.text,
            secondary: t.text_secondary,
            hint: t.text_disabled,
            win: t.success,
            lose: t.danger,
            track: t.surface_raised,
            hub: t.surface,
            hub_ring: t.border,
            pockets,
            smear,
            lit: enc(Color32::WHITE),
            dim: enc(t.surface_sunken),
            divider: enc(t.text),
            rim: enc(theme::mix(t.border, t.surface_raised, 96)),
            rim_edge: enc(t.border_strong),
            rim_inner: enc(t.surface_sunken),
            shadow: enc(Color32::BLACK),
            peg: enc(t.text),
            number: enc(t.text_on_accent),
            flapper: enc(t.warning),
            flapper_edge: enc(t.text_on_accent),
        }
    }

    fn tone(&self, starts: bool) -> Color32 {
        if starts { self.win } else { self.lose }
    }
}

/// A pixel of the band of pockets, packed into eight bytes: the band is most
/// of a frame's work and, at a few hundred thousand pixels, most of the
/// memory the wheel touches before it is first seen.
#[derive(Copy, Clone)]
struct BandPixel {
    at: u32,
    /// The screen angle, clockwise from straight up in 65536ths of a turn,
    /// over a pocket's width at this radius in eighths of a pixel (11 bits)
    /// over the shade (5 bits).
    polar: u32,
}

impl BandPixel {
    fn new(at: u32, theta: u16, arc: f32, shade: usize) -> Self {
        let arc = ((arc * 8.0) as u32).min(0x7FF);
        Self {
            at,
            polar: (u32::from(theta) << 16) | (arc << 5) | shade as u32,
        }
    }

    #[inline(always)]
    fn theta(self) -> u16 {
        (self.polar >> 16) as u16
    }

    #[inline(always)]
    fn arc(self) -> u32 {
        (self.polar >> 5) & 0x7FF
    }

    #[inline(always)]
    fn shade(self) -> usize {
        (self.polar & 0x1F) as usize
    }
}

/// `atan2` folded to an octant and fitted by an odd polynomial: within
/// 0.0001 rad, which is a hundredth of a pixel at the rim.
pub(crate) fn turn_of(x: f32, y: f32) -> u16 {
    let (ax, ay) = (libm::fabsf(x), libm::fabsf(y));
    let (lo, hi) = if ax < ay { (ax, ay) } else { (ay, ax) };
    if hi == 0.0 {
        return 0;
    }
    let z = lo / hi;
    let z2 = z * z;
    let mut a = z
        * (0.999_866
            + z2 * (-0.330_299_5 + z2 * (0.180_141 + z2 * (-0.085_133 + z2 * 0.020_835_1))));
    if ax < ay {
        a = core::f32::consts::FRAC_PI_2 - a;
    }
    if x < 0.0 {
        a = core::f32::consts::PI - a;
    }
    if y < 0.0 {
        a = -a;
    }
    // `a` is measured from +x towards +y; the wheel counts clockwise from up.
    let mut turn = a / TAU + 0.25;
    if turn < 0.0 {
        turn += 1.0;
    }
    (turn * 65536.0) as u32 as u16
}

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// A disc of radius `r`, centred in its mask.
fn circle(r: f32) -> Mask {
    let w = (2.0 * r) as i32 + 4;
    let c = w as f32 / 2.0;
    Mask::from_sdf(w, w, |x, y| {
        let (dx, dy) = (x - c, y - c);
        libm::sqrtf(dx * dx + dy * dy) - r
    })
}

struct Flapper {
    body: Mask,
    edge: Mask,
    rivet: Mask,
    /// The pivot, in the masks' pixels.
    anchor: (f32, f32),
    rivet_anchor: (f32, f32),
    /// Where the pivot sits, in the wheel's pixmaps.
    pivot: (f32, f32),
}

impl Flapper {
    fn new(layout: &Layout, origin: (i32, i32)) -> Self {
        let r = layout.flapper as f32 / 2.0;
        let len = (layout.tip_y - layout.pivot_y) as f32;
        let edge = (r / 6.0).clamp(1.0, 2.5);
        // A teardrop: the pivot's circle and the cone from its tangents to the
        // tip. Inside the cone the larger of the distances to its three sides.
        let tx = r * libm::sqrtf(1.0 - (r / len) * (r / len));
        let ty = r * r / len;
        let side = libm::sqrtf((len - ty) * (len - ty) + tx * tx);
        let (nx, ny) = ((len - ty) / side, tx / side);
        let teardrop = move |x: f32, y: f32| {
            let circle = libm::sqrtf(x * x + y * y) - r;
            let cone = (libm::fabsf(x) - tx) * nx + (y - ty) * ny;
            circle.min(cone.max(ty - y))
        };
        let tip_out = edge * len / r;
        let w = (2.0 * (r + edge)) as i32 + 4;
        let h = (r + edge + len + tip_out) as i32 + 4;
        let anchor = (w as f32 / 2.0, r + edge + 2.0);
        let body = Mask::from_sdf(w, h, |x, y| teardrop(x - anchor.0, y - anchor.1));
        let edge_mask = Mask::from_sdf(w, h, |x, y| teardrop(x - anchor.0, y - anchor.1) - edge);
        let rivet = circle(r * 0.38);
        let rivet_anchor = (rivet.w as f32 / 2.0, rivet.h as f32 / 2.0);
        Self {
            body,
            edge: edge_mask,
            rivet,
            anchor,
            rivet_anchor,
            pivot: (
                (layout.cx - origin.0) as f32,
                (layout.pivot_y - origin.1) as f32,
            ),
        }
    }

    fn place(&self, flap: u32) -> Place {
        Place {
            anchor: self.anchor,
            at: self.pivot,
            angle: -(flap as f32 / 256.0) * FLAP_SWING,
        }
    }
}

/// What a moment of the spin shows on the wheel.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Pose {
    rotation: u32,
    blur: u32,
    glow: u32,
    flap: u32,
}

/// What the column under the wheel shows.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Words {
    landed: bool,
    verdict: bool,
    bar: i32,
}

pub struct Scene<'a> {
    layout: Layout,
    spin: Spin,
    fonts: Fonts<'a>,
    palette: Palette,
    origin: (i32, i32),
    centre: (f32, f32),
    base: Pixmap,
    frame: Pixmap,
    /// The band's pixels it covers whole, and those along its two edges with
    /// their coverage.
    band: Vec<BandPixel>,
    band_edge: Vec<(BandPixel, u8)>,
    rim: Vec<u32>,
    hub_box: DamageRect,
    /// The pixels the hub covers, which a reveal repaints.
    hub_pixels: Vec<u32>,
    numbers: [Mask; POCKETS],
    /// Where in a number's mask its cap height is centred.
    number_middle: f32,
    peg: Mask,
    flapper: Flapper,
    status_base: Option<Pixmap>,
    pose: Option<Pose>,
    flapper_reach: DamageRect,
    reveal: Option<u32>,
    words: Option<Words>,
}

/// What a frame changed, for the present.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Damage {
    pub wheel: Option<DamageRect>,
    pub column: Option<DamageRect>,
}

impl Damage {
    pub fn is_empty(&self) -> bool {
        self.wheel.is_none() && self.column.is_none()
    }

    pub fn rects(&self) -> impl Iterator<Item = DamageRect> {
        self.wheel.into_iter().chain(self.column)
    }

    pub fn union(self, other: Damage) -> Damage {
        let join = |a: Option<DamageRect>, b: Option<DamageRect>| match (a, b) {
            (Some(a), Some(b)) => Some(a.union(&b)),
            (a, b) => a.or(b),
        };
        Damage {
            wheel: join(self.wheel, other.wheel),
            column: join(self.column, other.column),
        }
    }
}

impl<'a> Scene<'a> {
    pub fn new(layout: Layout, format: PixelFormat, fate: u32, fonts: Fonts<'a>) -> Self {
        let palette = Palette::new(theme::current(), format);
        let b = layout.wheel_box;
        let origin = (b.x0, b.y0);
        let (w, h) = (b.x1 - b.x0 + 1, b.y1 - b.y0 + 1);
        let centre = ((layout.cx - origin.0) as f32, (layout.cy - origin.1) as f32);
        let hub = layout.hub;
        let hub_reach = hub + 2;
        let hub_box = DamageRect {
            x0: (centre.0 as i32 - hub_reach).max(0),
            y0: (centre.1 as i32 - hub_reach).max(0),
            x1: (centre.0 as i32 + hub_reach).min(w - 1),
            y1: (centre.1 as i32 + hub_reach).min(h - 1),
        };
        let flapper = Flapper::new(&layout, origin);
        let mut scene = Self {
            spin: Spin::new(fate),
            fonts,
            palette,
            origin,
            centre,
            base: Pixmap::new(w, h, format),
            frame: Pixmap::new(w, h, format),
            band: Vec::new(),
            band_edge: Vec::new(),
            rim: Vec::new(),
            hub_box,
            hub_pixels: Vec::new(),
            numbers: core::array::from_fn(|_| Mask::new(0, 0)),
            number_middle: 0.0,
            peg: Mask::new(0, 0),
            flapper_reach: flapper.place(0).reach(&flapper.edge),
            flapper,
            status_base: None,
            pose: None,
            reveal: None,
            words: None,
            layout,
        };
        scene.measure_band();
        scene.make_masks();
        scene
    }

    /// Paint the whole screen as the spin begins.
    pub fn paint_all(&mut self, screen: &mut Screen) {
        self.paint_backdrop(screen);
        let l = self.layout;
        let p = &self.palette;
        let (text, secondary, hint) = (p.text, p.secondary, p.hint);
        if let Some(y) = l.title_y {
            self.fonts
                .centred(screen, true, l.cx, y, words::TITLE, l.sizes.title, text);
        }
        if let Some(y) = l.rule_y {
            self.fonts
                .centred(screen, false, l.cx, y, words::RULE, l.sizes.rule, secondary);
        }
        if let Some(y) = l.hint_y {
            self.fonts
                .centred(screen, false, l.cx, y, words::HINT, l.sizes.hint, hint);
        }
        let status = l.status_box;
        if status.is_valid() {
            self.status_base = Some(screen.take(status));
        }

        let b = self.layout.wheel_box;
        self.base = screen.take(b);
        self.paint_wheel_base();
        self.paint_hub(None);
        self.frame.px.copy_from_slice(&self.base.px);
        self.pose = None;
        self.words = None;
        let _ = self.paint(screen, 0);
    }

    /// Repaint what `t_ms` changes; the damage is what the present must show.
    pub fn paint(&mut self, screen: &mut Screen, t_ms: u32) -> Damage {
        let mut damage = Damage::default();
        let phase = spin::phase(t_ms);
        let landed = phase != Phase::Spinning;
        let after = t_ms.saturating_sub(spin::LAND_MS);

        let pose = if landed {
            Pose {
                rotation: self.spin.rotation(spin::LAND_MS),
                blur: 0,
                glow: spin::progress(after, GLOW_MS),
                flap: 0,
            }
        } else {
            let rotation = self.spin.rotation(t_ms);
            Pose {
                rotation,
                blur: spin::blur(self.spin.speed(t_ms)),
                glow: 0,
                flap: spin::flap(rotation),
            }
        };
        let mut wheel = false;
        if self.pose != Some(pose) {
            self.render(pose);
            self.pose = Some(pose);
            wheel = true;
        }

        let reveal = landed.then(|| spin::progress(after, REVEAL_MS) * REVEAL_STEPS / 256);
        if self.reveal != reveal {
            self.paint_hub(reveal);
            for &at in &self.hub_pixels {
                self.frame.px[at as usize] = self.base.px[at as usize];
            }
            self.reveal = reveal;
            wheel = true;
        }
        if wheel {
            screen.put(&self.frame, self.origin.0, self.origin.1);
            damage.wheel = Some(self.layout.wheel_box);
        }

        let verdict = matches!(phase, Phase::Verdict | Phase::Done);
        let bar = if verdict {
            let into = t_ms.saturating_sub(spin::LAND_MS + spin::SETTLE_MS);
            (spin::progress(into, spin::VERDICT_MS) as i32 * self.layout.bar.w) >> 8
        } else {
            -1
        };
        let words = Words {
            landed,
            verdict,
            bar,
        };
        if self.words != Some(words) && self.status_base.is_some() {
            self.paint_column(screen, words);
            self.words = Some(words);
            damage.column = Some(self.layout.status_box);
        }
        damage
    }

    fn paint_backdrop(&self, screen: &mut Screen) {
        let p = &self.palette;
        let enc = |c: Color32| screen.format.encode(c).0;
        let (dark, light) = (enc(p.backdrop), enc(p.spotlight));
        let reach = (self.layout.radius as i64 * 34 / 10).max(1);
        let reach2 = reach * reach;
        let mut lut = [0u32; 257];
        for (i, px) in lut.iter_mut().enumerate() {
            // Smoothstep on the squared distance: bright at the wheel, gone
            // by `reach`.
            let d = i as f32 / 256.0;
            let f = 1.0 - d * d * (3.0 - 2.0 * d);
            *px = mix(dark, light, (f * 256.0) as u32);
        }
        // The squared distance steps by an odd number a pixel, and a multiply
        // by its reciprocal stands in for the divide.
        let per = (256u64 << 32) / reach2 as u64;
        let (cx, cy) = (self.layout.cx as i64, self.layout.cy as i64);
        for y in 0..screen.height {
            let dy = y as i64 - cy;
            let row = y as usize * screen.pitch;
            let mut d2 = (cx * cx + dy * dy) as u64;
            let mut step = 1 - 2 * cx;
            for px in screen.px[row..row + screen.width as usize * 4].chunks_exact_mut(4) {
                let i = ((d2 * per) >> 32).min(256) as usize;
                px.copy_from_slice(&lut[i].to_le_bytes());
                d2 = (d2 as i64 + step) as u64;
                step += 2;
            }
        }
    }

    /// The band's and the rim's pixels. The wheel is symmetric about both
    /// axes through its centre, which lies on a pixel corner, so each row of
    /// one quadrant is measured once and mirrored into the other three.
    fn measure_band(&mut self) {
        let l = &self.layout;
        let (inner, outer) = (l.hub as f32, l.radius as f32);
        let (rim_lo, rim_hi) = (outer - 0.5, outer + l.rim as f32 + 0.5);
        let span = (outer - inner).max(1.0);
        let lo2 = (inner - 1.0).max(0.0) * (inner - 1.0).max(0.0);
        let hi2 = (rim_hi + 1.0) * (rim_hi + 1.0);
        let n = rim_hi as i32 + 2;
        let annulus = |a: f32, b: f32| (core::f32::consts::PI * (b * b - a * a)) as usize;
        self.band
            .reserve(annulus(inner + 0.5, outer - 0.5) + 4 * n as usize);
        self.band_edge.reserve(16 * n as usize);
        self.rim.reserve(annulus(rim_lo, rim_hi) + 8 * n as usize);

        let (w, h) = (self.base.w, self.base.h);
        let (cx, cy) = (self.centre.0 as i32, self.centre.1 as i32);
        let (x0, x1) = ((cx - n).max(0), (cx + n).min(w));
        let mut quadrant = vec![(None, false); n as usize];
        for j in 0..n {
            let dy = j as f32 + 0.5;
            for (i, cell) in quadrant.iter_mut().enumerate() {
                *cell = (None, false);
                let dx = i as f32 + 0.5;
                let r2 = dx * dx + dy * dy;
                if r2 < lo2 || r2 > hi2 {
                    continue;
                }
                let r = libm::sqrtf(r2);
                let cover = clamp01(outer - r + 0.5) * clamp01(r - inner + 0.5);
                let band = (cover > 0.0).then(|| {
                    let shade = clamp01((r - inner) / span) * (SHADES - 1) as f32 + 0.5;
                    let pixel = BandPixel::new(
                        0,
                        turn_of(dx, dy),
                        TAU * r / POCKETS as f32,
                        shade as usize,
                    );
                    (pixel, (cover * 255.0 + 0.5) as u8)
                });
                *cell = (band, r >= rim_lo && r <= rim_hi);
            }
            for y in [cy + j, cy - 1 - j] {
                if y < 0 || y >= h {
                    continue;
                }
                for x in x0..x1 {
                    let i = if x >= cx { x - cx } else { cx - 1 - x };
                    let (band, rim) = quadrant[i as usize];
                    let at = (y * w + x) as u32;
                    if rim {
                        self.rim.push(at);
                    }
                    let Some((b, cover)) = band else {
                        continue;
                    };
                    // Clockwise from up: the right-down quadrant mirrored
                    // left is 1 - θ, up is 1/2 - θ, and both is θ + 1/2.
                    let theta = match (x >= cx, y >= cy) {
                        (true, true) => b.theta(),
                        (false, true) => 0u16.wrapping_sub(b.theta()),
                        (true, false) => 0x8000u16.wrapping_sub(b.theta()),
                        (false, false) => b.theta().wrapping_add(0x8000),
                    };
                    let pixel = BandPixel {
                        at,
                        polar: (u32::from(theta) << 16) | (b.polar & 0xFFFF),
                    };
                    if cover == 255 {
                        self.band.push(pixel);
                    } else {
                        self.band_edge.push((pixel, cover));
                    }
                }
            }
        }
    }

    fn make_masks(&mut self) {
        let l = self.layout;
        self.peg = circle((l.rim as f32 * 0.3).max(1.2));
        let size = l.sizes.pocket;
        let format = self.base.format();
        for (k, mask) in self.numbers.iter_mut().enumerate() {
            let mut digits = [0u8; 3];
            let text = decimal(wheel::NUMBERS[k], &mut digits);
            let Some(face) = self.fonts.face(true) else {
                continue;
            };
            let tw = face.text_width(text, size) + 4;
            let th = face.line_height(size) + 4;
            self.number_middle = 2.0 + face.ascent(size) as f32 - size as f32 * 0.36;
            let mut canvas = Pixmap::new(tw, th, format);
            let black = canvas.encode(Color32::BLACK);
            canvas.px.fill(black);
            face.draw_text(
                &mut canvas,
                2,
                2,
                text,
                size,
                Color32::WHITE,
                Color32::TRANSPARENT,
            );
            if matches!(wheel::NUMBERS[k], 6 | 9) {
                // Turned, a 6 and a 9 are the same glyph: as on a casino's
                // wheel, the bar marks which way up it reads.
                let white = canvas.encode(Color32::WHITE);
                let thick = (size as i32 / 14).max(1);
                let y = 2 + face.ascent(size) + thick;
                let inset = tw / 5;
                for row in y..(y + thick).min(th) {
                    for col in inset..tw - inset {
                        canvas.px[(row * tw + col) as usize] = white;
                    }
                }
            }
            *mask = Mask::from_canvas_red(&canvas);
        }
    }

    fn paint_wheel_base(&mut self) {
        let l = self.layout;
        let p = &self.palette;
        let (cx, cy) = self.centre;
        let outer = l.radius as f32;
        let rim = l.rim as f32;
        let edge = outer + rim;
        let spread = l.shadow() as f32;
        let drop = l.drop() as f32;
        // Inside the rim the band and the hub repaint every pixel.
        let hidden2 = (outer - 1.5) * (outer - 1.5);
        let reach = edge + spread + drop + 1.0;
        for y in 0..self.base.h {
            let dy = y as f32 + 0.5 - cy;
            for x in 0..self.base.w {
                let dx = x as f32 + 0.5 - cx;
                let r2 = dx * dx + dy * dy;
                if r2 < hidden2 || r2 > reach * reach {
                    continue;
                }
                let px = &mut self.base.px[(y * self.base.w + x) as usize];
                let rs = libm::sqrtf(dx * dx + (dy - drop) * (dy - drop));
                if rs < edge + spread {
                    let fade = clamp01((edge + spread - rs) / spread);
                    *px = mix(*px, p.shadow, (fade * fade * 150.0) as u32);
                }
                let r = libm::sqrtf(r2);
                let cover = clamp01(edge - r + 0.5);
                if cover <= 0.0 {
                    continue;
                }
                let mut c = p.rim;
                let outer_line = clamp01(1.6 - libm::fabsf(edge - 0.9 - r));
                c = mix(c, p.rim_edge, (outer_line * 120.0) as u32);
                let inner_line = clamp01(1.8 - libm::fabsf(r - outer - 0.6));
                c = mix(c, p.rim_inner, (inner_line * 200.0) as u32);
                *px = mix(*px, c, (cover * 256.0) as u32);
            }
        }
    }

    /// The hub, with nothing in it before the wheel stops, and its number
    /// fading in `step` of [`REVEAL_STEPS`] after.
    fn paint_hub(&mut self, step: Option<u32>) {
        let l = self.layout;
        let (cx, cy) = self.centre;
        let hub = l.hub as f32;
        let starts = self.spin.starts();
        let p = &self.palette;
        let ring_color = match step {
            Some(s) => theme::mix(p.hub_ring, p.tone(starts), (s * 255 / REVEAL_STEPS) as u8),
            None => p.hub_ring,
        };
        let (fill, ring) = (self.base.encode(p.hub), self.base.encode(ring_color));
        let ring_at = hub - (hub * 0.045).max(2.0);
        let ring_half = (hub * 0.018).max(0.8);
        let b = self.hub_box;
        let measure = self.hub_pixels.is_empty();
        for y in b.y0..=b.y1 {
            let dy = y as f32 + 0.5 - cy;
            for x in b.x0..=b.x1 {
                let dx = x as f32 + 0.5 - cx;
                let r = libm::sqrtf(dx * dx + dy * dy);
                if r > hub + 1.0 {
                    continue;
                }
                let at = (y * self.base.w + x) as u32;
                if measure {
                    self.hub_pixels.push(at);
                }
                let line = clamp01(ring_half + 0.5 - libm::fabsf(r - ring_at));
                self.base.px[at as usize] = mix(fill, ring, (line * 256.0) as u32);
            }
        }
        let (label, color) = match step {
            None => (None, p.hint),
            Some(s) => (
                Some(self.spin.number()),
                theme::mix(p.hub, p.tone(starts), (s * 255 / REVEAL_STEPS) as u8),
            ),
        };
        let mut digits = [0u8; 3];
        let text = label.map_or(words::UNKNOWN, |n| decimal(n, &mut digits));
        let size = if label.is_some() {
            l.sizes.hub
        } else {
            l.sizes.hub * 2 / 3
        };
        self.fonts
            .centred_on(&mut self.base, cx as i32, cy as i32, text, size, color);
    }

    fn render(&mut self, pose: Pose) {
        let p = &self.palette;
        let landed = self.spin.landing().pocket;
        let mut pal = p.pockets;
        for (k, row) in pal.iter_mut().enumerate() {
            for (s, px) in row.iter_mut().enumerate() {
                let mut c = mix(*px, p.smear[s], pose.blur);
                if pose.glow > 0 {
                    c = if k == landed {
                        mix(c, p.lit, pose.glow * 52 / 256)
                    } else {
                        mix(c, p.dim, pose.glow * 120 / 256)
                    };
                }
                *px = c;
            }
        }
        let divider_alpha = (256 - pose.blur) * 200 / 256;

        let reach = self.flapper.place(pose.flap).reach(&self.flapper.edge);
        let restore = reach.union(&self.flapper_reach);
        self.frame.copy_rect(&self.base, restore);
        self.flapper_reach = reach;
        for &at in &self.rim {
            self.frame.px[at as usize] = self.base.px[at as usize];
        }

        let rot = pose.rotation as u16;
        let pocket = |b: BandPixel| {
            let pos = u32::from(b.theta().wrapping_sub(rot)) * POCKETS as u32;
            let frac = pos & 0xFFFF;
            let dist = (frac.min(0x1_0000 - frac) * b.arc()) >> 16;
            let c = pal[(pos >> 16) as usize][b.shade()];
            if dist < DIVIDER_REACH {
                let cov = ((DIVIDER_REACH - dist) * 32).min(256);
                mix(c, p.divider, (cov * divider_alpha) >> 8)
            } else {
                c
            }
        };
        let (frame, base) = (&mut self.frame.px, &self.base.px);
        for &b in &self.band {
            frame[b.at as usize] = pocket(b);
        }
        for &(b, cover) in &self.band_edge {
            let under = base[b.at as usize];
            frame[b.at as usize] = mix(under, pocket(b), weight(u32::from(cover)));
        }

        let l = self.layout;
        let (cx, cy) = self.centre;
        let peg_r = l.radius as f32 + l.rim as f32 / 2.0;
        let peg_c = self.peg.w as f32 / 2.0;
        let peg_alpha = 256 - pose.blur / 2;
        for k in 0..POCKETS {
            let a = (wheel::divider(k) + pose.rotation) as f32 / TURN as f32 * TAU;
            let place = Place {
                anchor: (peg_c, peg_c),
                at: (cx + peg_r * libm::sinf(a), cy - peg_r * libm::cosf(a)),
                angle: 0.0,
            };
            blit(&mut self.frame, &self.peg, &place, p.peg, peg_alpha);
        }

        let num_r = l.radius as f32 - (l.radius - l.hub) as f32 * 0.2;
        let shown = 256u32.saturating_sub(pose.blur * 5 / 4);
        for (k, mask) in self.numbers.iter().enumerate() {
            if mask.w == 0 {
                continue;
            }
            let a = (wheel::pocket_centre(k) + pose.rotation) as f32 / TURN as f32 * TAU;
            let place = Place {
                anchor: (mask.w as f32 / 2.0, self.number_middle),
                at: (cx + num_r * libm::sinf(a), cy - num_r * libm::cosf(a)),
                angle: a,
            };
            let alpha = if k == landed || pose.glow == 0 {
                shown
            } else {
                shown * (256 - pose.glow / 2) / 256
            };
            blit(&mut self.frame, mask, &place, p.number, alpha);
        }

        let f = &self.flapper;
        let place = f.place(pose.flap);
        let lift = (l.radius as f32 / 120.0).max(1.0);
        let shadow = Place {
            at: (place.at.0 + lift, place.at.1 + lift * 1.5),
            ..place
        };
        blit(&mut self.frame, &f.edge, &shadow, p.shadow, 90);
        blit(&mut self.frame, &f.edge, &place, p.flapper_edge, 256);
        blit(&mut self.frame, &f.body, &place, p.flapper, 256);
        let rivet = Place {
            anchor: f.rivet_anchor,
            at: f.pivot,
            angle: 0.0,
        };
        blit(&mut self.frame, &f.rivet, &rivet, p.flapper_edge, 256);
    }

    fn paint_column(&mut self, screen: &mut Screen, words: Words) {
        let l = self.layout;
        if let Some(base) = &self.status_base {
            screen.put(base, l.status_box.x0, l.status_box.y0);
        }
        let starts = self.spin.starts();
        let p = &self.palette;
        let (secondary, tone, track) = (p.secondary, p.tone(starts), p.track);
        if !words.landed {
            self.fonts.centred(
                screen,
                true,
                l.cx,
                l.status_y,
                words::SPINNING,
                l.sizes.status,
                secondary,
            );
            return;
        }
        let mut buf = [0u8; 64];
        let line = verdict(self.spin.number(), &mut buf);
        self.fonts
            .centred(screen, true, l.cx, l.status_y, line, l.sizes.status, tone);
        if !words.verdict {
            return;
        }
        if let Some(y) = l.detail_y {
            let detail = if starts {
                words::STARTING
            } else {
                words::RESTARTING
            };
            self.fonts
                .centred(screen, false, l.cx, y, detail, l.sizes.detail, secondary);
        }
        bar(screen, l.bar, words.bar, track, tone);
    }
}

/// A rounded bar `filled` pixels along `rect`.
fn bar(screen: &mut Screen, rect: Rect, filled: i32, track: Color32, fill: Color32) {
    let r = rect.h as f32 / 2.0;
    let enc = |c: Color32| screen.format.encode(c).0;
    let (track, fill) = (enc(track), enc(fill));
    for y in 0..rect.h {
        let sy = rect.y + y;
        if sy < 0 || sy >= screen.height {
            continue;
        }
        let dy = y as f32 + 0.5 - r;
        for x in 0..rect.w {
            let sx = rect.x + x;
            if sx < 0 || sx >= screen.width {
                continue;
            }
            // Distance outside the rounded rectangle, by its end caps.
            let ex = (x as f32 + 0.5).min(rect.w as f32 - x as f32 - 0.5);
            let dx = (r - ex).max(0.0);
            let cover = clamp01(r + 0.5 - libm::sqrtf(dx * dx + dy * dy));
            if cover <= 0.0 {
                continue;
            }
            let at = sy as usize * screen.pitch + sx as usize * 4;
            let under = u32::from_le_bytes([
                screen.px[at],
                screen.px[at + 1],
                screen.px[at + 2],
                screen.px[at + 3],
            ]);
            let ink = if x < filled { fill } else { track };
            let c = mix(under, ink, (cover * 256.0) as u32);
            screen.px[at..at + 4].copy_from_slice(&c.to_le_bytes());
        }
    }
}
