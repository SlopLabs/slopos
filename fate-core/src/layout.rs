//! Where everything goes on a screen of a given size.
//!
//! One column, centred: the title and the rule, the wheel with its flapper
//! above it, the verdict and its bar, and a hint at the foot of the screen.
//! The wheel takes what the text leaves, up to [`MAX_RADIUS`].

use slopos_abi::damage::DamageRect;

pub const MAX_RADIUS: i32 = 360;
const MIN_RADIUS: i32 = 40;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn damage(self) -> DamageRect {
        DamageRect {
            x0: self.x,
            y0: self.y,
            x1: self.x + self.w - 1,
            y1: self.y + self.h - 1,
        }
    }
}

/// Pixel sizes of the faces each line is set in.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Sizes {
    pub title: u16,
    pub rule: u16,
    pub status: u16,
    pub detail: u16,
    pub hint: u16,
    pub pocket: u16,
    pub hub: u16,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    pub width: i32,
    pub height: i32,
    /// The wheel's centre, on a pixel corner.
    pub cx: i32,
    pub cy: i32,
    /// The pockets' outer edge.
    pub radius: i32,
    /// The band around the pockets that carries the pegs.
    pub rim: i32,
    /// The hub's edge, inside the pockets.
    pub hub: i32,
    /// The flapper hangs from a pivot above the rim to a tip just inside it.
    pub pivot_y: i32,
    pub tip_y: i32,
    /// The flapper's width at the pivot.
    pub flapper: i32,
    pub sizes: Sizes,
    /// Tops of the text lines; `None` where the screen has no room for one.
    pub title_y: Option<i32>,
    pub rule_y: Option<i32>,
    pub status_y: i32,
    pub detail_y: Option<i32>,
    pub hint_y: Option<i32>,
    pub bar: Rect,
    /// What the wheel, its shadow and its flapper can touch.
    pub wheel_box: DamageRect,
    /// What the verdict, its detail and its bar can touch.
    pub status_box: DamageRect,
}

fn scaled(radius: i32, per_mille: i32, lo: i32, hi: i32) -> i32 {
    (radius * per_mille / 1000).clamp(lo, hi)
}

fn line(size: i32) -> i32 {
    size * 13 / 10
}

struct Stack {
    title: bool,
    rule: bool,
    detail: bool,
    hint: bool,
}

const STACKS: [Stack; 3] = [
    Stack {
        title: true,
        rule: true,
        detail: true,
        hint: true,
    },
    Stack {
        title: true,
        rule: false,
        detail: true,
        hint: false,
    },
    Stack {
        title: false,
        rule: false,
        detail: false,
        hint: false,
    },
];

fn sizes(radius: i32) -> Sizes {
    let hub = radius * 34 / 100;
    Sizes {
        title: scaled(radius, 140, 20, 44) as u16,
        rule: scaled(radius, 62, 13, 19) as u16,
        status: scaled(radius, 100, 15, 30) as u16,
        detail: scaled(radius, 62, 13, 19) as u16,
        hint: scaled(radius, 48, 12, 14) as u16,
        pocket: scaled(radius, 140, 9, 42) as u16,
        hub: (hub * 11 / 10).clamp(12, 96) as u16,
    }
}

fn rim(radius: i32) -> i32 {
    scaled(radius, 75, 4, 24)
}

fn margin(width: i32, height: i32) -> i32 {
    (width.min(height) / 25).clamp(8, 48)
}

/// The column above and below the wheel, top to bottom, for `radius`.
fn heights(radius: i32, stack: &Stack) -> (i32, i32) {
    let s = sizes(radius);
    let (title, rule, status, detail) = (
        s.title as i32,
        s.rule as i32,
        s.status as i32,
        s.detail as i32,
    );
    let gap = scaled(radius, 110, 12, 40);
    let mut above = gap + overhang(radius);
    if stack.title {
        above += line(title);
    }
    if stack.rule {
        above += title / 4 + line(rule);
    }
    let mut below = below_gap(radius) + line(status) + bar_height(radius) + status / 2;
    if stack.detail {
        below += status / 4 + line(detail);
    }
    (above, below)
}

/// How far the flapper reaches above the rim.
fn overhang(radius: i32) -> i32 {
    let tip = radius - radius * 6 / 100;
    let pivot = tip + pointer_len(radius);
    pivot + flapper(radius) / 2 - (radius + rim(radius))
}

fn pointer_len(radius: i32) -> i32 {
    radius * 26 / 100
}

fn flapper(radius: i32) -> i32 {
    (radius * 15 / 100).max(8)
}

fn bar_height(radius: i32) -> i32 {
    scaled(radius, 20, 4, 8)
}

fn shadow(radius: i32) -> i32 {
    scaled(radius, 90, 4, 28)
}

/// How far the shadow sits below the wheel.
fn drop(radius: i32) -> i32 {
    (radius * 25 / 1000).max(1)
}

/// From the rim to the verdict: clear of the shadow, which the wheel's frames
/// repaint.
fn below_gap(radius: i32) -> i32 {
    shadow(radius) + drop(radius) + scaled(radius, 40, 6, 16)
}

fn hint_height(radius: i32) -> i32 {
    line(sizes(radius).hint as i32)
}

pub fn layout(width: u32, height: u32) -> Layout {
    let (width, height) = (width as i32, height as i32);
    let margin = margin(width, height);
    let across = (width - 2 * margin) / 2;
    for stack in &STACKS {
        let mut radius = MAX_RADIUS.min(across * 100 / 112);
        while radius >= MIN_RADIUS {
            let (above, below) = heights(radius, stack);
            let hint = if stack.hint {
                hint_height(radius) + margin
            } else {
                0
            };
            let wheel = 2 * (radius + rim(radius));
            if above + wheel + below + hint + 2 * margin <= height {
                return place(width, height, radius, stack, above, below, hint);
            }
            radius -= (radius / 32).max(1);
        }
    }
    let radius = MIN_RADIUS.min((width.min(height) / 2 - 4).max(8) * 100 / 112);
    let (above, below) = heights(radius, &STACKS[2]);
    place(width, height, radius, &STACKS[2], above, below, 0)
}

fn place(
    width: i32,
    height: i32,
    radius: i32,
    stack: &Stack,
    above: i32,
    below: i32,
    hint: i32,
) -> Layout {
    let s = sizes(radius);
    let rim = rim(radius);
    let block = above + 2 * (radius + rim) + below;
    let top = ((height - hint - block) / 2).max(0);
    let cx = width / 2;

    let mut y = top;
    let title_y = stack.title.then(|| {
        let at = y;
        y += line(s.title as i32);
        at
    });
    let rule_y = stack.rule.then(|| {
        y += s.title as i32 / 4;
        let at = y;
        y += line(s.rule as i32);
        at
    });
    let cy = top + above + radius + rim;
    let tip_y = cy - radius + radius * 6 / 100;
    let pivot_y = tip_y - pointer_len(radius);
    let flapper = flapper(radius);

    y = cy + radius + rim + below_gap(radius);
    let status_y = y;
    y += line(s.status as i32);
    let detail_y = stack.detail.then(|| {
        y += s.status as i32 / 4;
        let at = y;
        y += line(s.detail as i32);
        at
    });
    y += s.status as i32 / 2;
    let bar_w = (radius * 2).min(width - 2 * margin(width, height)).max(16);
    let bar = Rect {
        x: cx - bar_w / 2,
        y,
        w: bar_w,
        h: bar_height(radius),
    };
    let hint_y = stack
        .hint
        .then(|| height - margin(width, height) - hint_height(radius));

    let reach = radius + rim + shadow(radius);
    let wheel_box = DamageRect {
        x0: (cx - reach).max(0),
        y0: (pivot_y - flapper).min(cy - reach).max(0),
        x1: (cx + reach).min(width - 1),
        y1: (cy + reach + drop(radius) + 1).min(height - 1),
    };
    let status_box = DamageRect {
        x0: margin(width, height),
        y0: status_y,
        x1: width - 1 - margin(width, height),
        y1: (bar.y + bar.h).min(height - 1),
    };

    Layout {
        width,
        height,
        cx,
        cy,
        radius,
        rim,
        hub: radius * 34 / 100,
        pivot_y,
        tip_y,
        flapper,
        sizes: s,
        title_y,
        rule_y,
        status_y,
        detail_y,
        hint_y,
        bar,
        wheel_box,
        status_box,
    }
}

impl Layout {
    /// How far the drop shadow spreads past the rim.
    pub fn shadow(&self) -> i32 {
        shadow(self.radius)
    }

    /// How far the drop shadow sits below the wheel.
    pub fn drop(&self) -> i32 {
        drop(self.radius)
    }
}
