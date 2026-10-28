use slopos_theme_core::space;

use crate::constraints::{BoxConstraints, MAX_EXTENT, Rect, Size};
use crate::event::{EventResponse, MessageSink, WidgetEvent, route};
use crate::paint::PaintContext;
use crate::traits::{MeasureCtx, Widget, WidgetCore, measure_widget, place_widget};

/// Names and their values: the names in a column as wide as the widest, up to
/// a third, each value on its name's first baseline.
pub struct PropertyListWidget {
    core: WidgetCore,
    /// Name then value, row after row.
    cells: Vec<Box<dyn Widget>>,
    names_width: i32,
    /// Each row's top and the shift down of its name and of its value, which
    /// put the two first baselines on one row.
    rows: Vec<(i32, i32, i32)>,
}

impl PropertyListWidget {
    pub fn new(cells: Vec<Box<dyn Widget>>) -> Self {
        Self {
            core: WidgetCore::new(),
            cells,
            names_width: 0,
            rows: Vec::new(),
        }
    }

    fn values_width(&self, width: i32) -> i32 {
        (width - self.names_width - space::LG).max(0)
    }
}

impl Widget for PropertyListWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            MAX_EXTENT
        };
        let loose = BoxConstraints::loose(Size::new(MAX_EXTENT, MAX_EXTENT));
        let widest = self
            .cells
            .iter_mut()
            .step_by(2)
            .map(|name| measure_widget(name.as_mut(), loose, ctx).width)
            .max()
            .unwrap_or(0);
        self.names_width = widest.min(width / 3);
        let names = BoxConstraints::loose(Size::new(self.names_width, MAX_EXTENT));
        let values = BoxConstraints::loose(Size::new(self.values_width(width), MAX_EXTENT));
        self.rows.clear();
        let mut y = 0;
        let mut widest_value = 0;
        for pair in self.cells.chunks_mut(2) {
            let [name, value] = pair else { continue };
            let n = measure_widget(name.as_mut(), names, ctx);
            let v = measure_widget(value.as_mut(), values, ctx);
            widest_value = widest_value.max(v.width);
            let nb = name.baseline().unwrap_or(n.height);
            let vb = value.baseline().unwrap_or(v.height);
            let line = nb.max(vb);
            let (dn, dv) = (line - nb, line - vb);
            self.rows.push((y, dn, dv));
            y += (n.height + dn).max(v.height + dv) + space::SM;
        }
        let height = (y - space::SM).max(0);
        let natural = self.names_width + space::LG + widest_value;
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            natural
        };
        constraints.constrain(Size::new(width, height))
    }

    fn layout(&mut self, rect: Rect) {
        let values_x = rect.x + self.names_width + space::LG;
        let values_w = self.values_width(rect.width);
        for (pair, &(top, dn, dv)) in self.cells.chunks_mut(2).zip(&self.rows) {
            let [name, value] = pair else { continue };
            let n = name.measured_size();
            let v = value.measured_size();
            place_widget(
                name.as_mut(),
                Rect::new(rect.x, rect.y + top + dn, self.names_width, n.height),
            );
            place_widget(
                value.as_mut(),
                Rect::new(values_x, rect.y + top + dv, v.width.min(values_w), v.height),
            );
        }
    }

    fn paint(&self, ctx: &mut PaintContext) {
        for cell in &self.cells {
            cell.paint(ctx);
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        route(&mut self.cells, event, sink)
    }

    fn baseline(&self) -> Option<i32> {
        let (_, dn, _) = *self.rows.first()?;
        self.cells.first()?.baseline().map(|b| b + dn)
    }

    fn children(&self) -> &[Box<dyn Widget>] {
        &self.cells
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.cells
    }
}
