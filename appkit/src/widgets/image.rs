use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};

use crate::constraints::{BoxConstraints, ImageScale, Size};
use crate::event::{EventResponse, MessageSink, WidgetEvent};
use crate::node::ImageData;
use crate::paint::PaintContext;
use crate::traits::{MeasureCtx, Widget, WidgetCore};

pub struct ImageWidget {
    core: WidgetCore,
    image: ImageData,
    scale: ImageScale,
    sampling: ImageSampling,
}

impl ImageWidget {
    pub fn new(image: ImageData, scale: ImageScale, sampling: ImageSampling) -> Self {
        Self {
            core: WidgetCore::new(),
            image,
            scale,
            sampling,
        }
    }

    fn natural_size(&self) -> Size {
        Size::new(self.image.width as i32, self.image.height as i32)
    }
}

impl Widget for ImageWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let natural = self.natural_size();
        let size = match self.scale {
            ImageScale::None => natural,
            ImageScale::Fit => {
                if natural.width == 0 || natural.height == 0 {
                    return constraints.constrain(Size::ZERO);
                }
                let max_w = constraints.max_width;
                let max_h = constraints.max_height;
                let scale_w = max_w as f64 / natural.width as f64;
                let scale_h = max_h as f64 / natural.height as f64;
                let scale = scale_w.min(scale_h).min(1.0);
                Size::new(
                    (natural.width as f64 * scale) as i32,
                    (natural.height as f64 * scale) as i32,
                )
            }
            ImageScale::Cover | ImageScale::Fill => Size::new(
                if constraints.is_width_bounded() {
                    constraints.max_width
                } else {
                    natural.width
                },
                if constraints.is_height_bounded() {
                    constraints.max_height
                } else {
                    natural.height
                },
            ),
        };
        constraints.constrain(size)
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let Some(bitmap) = BitmapRef::new(
            self.image.width,
            self.image.height,
            self.image.pixels.as_ref(),
        ) else {
            return;
        };
        let fit = match self.scale {
            ImageScale::None => ImageFit::Actual,
            ImageScale::Fit => ImageFit::Contain,
            ImageScale::Cover => ImageFit::Cover,
            ImageScale::Fill => ImageFit::Stretch,
        };
        let (x, y, w, h) = (rect.x, rect.y, rect.width, rect.height);
        ctx.draw_image(bitmap, x, y, w, h, fit, self.sampling);
    }

    fn event(&mut self, _event: &WidgetEvent, _sink: &mut MessageSink) -> EventResponse {
        EventResponse::Ignored
    }
}
