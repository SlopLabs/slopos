use slopos_theme_core::{TextRole, radius, space, stroke};

use crate::components::KeyCap;
use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{MeasureCtx, Role, Widget, WidgetCore};

const ROLE: TextRole = TextRole::Caption;
const LINE: i32 = ROLE.style().line_height;
const KEY_HEIGHT: i32 = LINE * 2 + space::XS;
const GAP: i32 = space::XS;
const PLATE: i32 = space::XS;
const PAD: i32 = space::XS;
const LETTER: u32 = 4;
const MAX_LETTER: i32 = KEY_HEIGHT * 3 / 2;
const KEY_RADIUS: i32 = radius::CONTROL / 2;

fn map_height(rows: usize) -> i32 {
    if rows == 0 {
        return 0;
    }
    let rows = rows as i32;
    rows * KEY_HEIGHT + (rows - 1) * GAP + PLATE * 2
}

pub struct KeyboardMapWidget {
    core: WidgetCore,
    label: String,
    rows: Vec<Vec<KeyCap>>,
}

impl KeyboardMapWidget {
    pub fn new(label: String, rows: Vec<Vec<KeyCap>>) -> Self {
        Self {
            core: WidgetCore::new(),
            label,
            rows,
        }
    }

    fn quarters(&self) -> u32 {
        self.rows
            .iter()
            .map(|row| row.iter().map(|k| u32::from(k.quarters)).sum())
            .max()
            .unwrap_or(0)
    }

    fn natural_width(&self) -> i32 {
        let q = self.quarters() as i32;
        if q == 0 {
            return 0;
        }
        (MAX_LETTER + GAP) * q / LETTER as i32 - GAP + PLATE * 2
    }

    fn keys(&self, inner: Rect) -> Vec<(Rect, &KeyCap)> {
        let q = self.quarters() as i64;
        let mut out = Vec::new();
        if q == 0 {
            return out;
        }
        let span = i64::from(inner.width + GAP);
        let at = |quarters: u32| inner.x + (i64::from(quarters) * span / q) as i32;
        for (r, row) in self.rows.iter().enumerate() {
            let top = inner.y + r as i32 * (KEY_HEIGHT + GAP);
            let mut q0 = 0;
            for key in row {
                let q1 = q0 + u32::from(key.quarters);
                let (x0, x1) = (at(q0), at(q1) - GAP);
                let y = if key.joins_above && r > 0 {
                    top - KEY_HEIGHT - GAP
                } else {
                    top
                };
                out.push((Rect::new(x0, y, x1 - x0, top + KEY_HEIGHT - y), key));
                q0 = q1;
            }
        }
        out
    }

    /// A key too narrow for the inset centres a legend instead of dropping it.
    fn legends(ctx: &mut PaintContext, rect: Rect, key: &KeyCap) {
        let theme = ctx.theme();
        let (ink, alt_ink) = if key.lit {
            (theme.text_on_accent, theme.text_on_accent)
        } else {
            (theme.text, theme.text_secondary)
        };
        let bottom = rect.y + rect.height;
        let upper = ui::centred_top(bottom - KEY_HEIGHT + space::XS / 2, LINE, ROLE);
        let lower = ui::centred_top(bottom - space::XS / 2 - LINE, LINE, ROLE);
        let fits = |text: &str| {
            let width = ui::width(text, ROLE);
            (!text.is_empty() && width <= rect.width - 2 * stroke::EDGE).then_some(width)
        };
        let left = |width: i32| rect.x + PAD.min((rect.width - width) / 2);
        if let Some(width) = fits(&key.shift) {
            ctx.draw_text(left(width), upper, &key.shift, ROLE, ink);
        }
        let base = fits(&key.base);
        if let Some(width) = base {
            ctx.draw_text(left(width), lower, &key.base, ROLE, ink);
        }
        let beside = base.map_or(0, |width| width + PAD);
        if let Some(alt) = fits(&key.alt_gr)
            && beside + alt + PAD * 2 <= rect.width
        {
            let at = rect.x + rect.width - PAD - alt;
            ctx.draw_text(at, lower, &key.alt_gr, ROLE, alt_ink);
        }
    }
}

impl Widget for KeyboardMapWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            self.natural_width()
        };
        constraints.constrain(Size::new(width, map_height(self.rows.len())))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        if self.rows.is_empty() {
            return;
        }
        let rect = self.layout_rect();
        let width = rect.width.min(self.natural_width());
        let plate = Rect::new(
            rect.x + (rect.width - width) / 2,
            rect.y,
            width,
            map_height(self.rows.len()),
        );
        let theme = ctx.theme();
        ctx.fill_rounded_rect(plate, radius::CONTROL, theme.surface_sunken);
        let inner = Rect::new(
            plate.x + PLATE,
            plate.y + PLATE,
            plate.width - PLATE * 2,
            plate.height - PLATE * 2,
        );
        let keys = self.keys(inner);
        // Every fill before any legend: a joined key would cover the one above.
        for &(rect, key) in &keys {
            let fill = if key.lit {
                theme.accent_fill
            } else if key.fixed {
                theme.surface
            } else {
                theme.control
            };
            if rect.width > 0 {
                ctx.fill_rounded_rect(rect, KEY_RADIUS, fill);
            }
        }
        for &(rect, key) in &keys {
            if !key.fixed {
                Self::legends(ctx, rect, key);
            }
        }
    }

    fn event(&mut self, _event: &WidgetEvent, _sink: &mut MessageSink) -> EventResponse {
        EventResponse::Ignored
    }

    fn role(&self) -> Role {
        Role::Group
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.label)
    }
}
