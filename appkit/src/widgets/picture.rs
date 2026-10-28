//! A picture the person zooms and pans, over the checks its transparent
//! pixels are judged against. It reports what the pointer does over it, and
//! the app decides what that makes the zoom and the pan.

use std::any::Any;

use slopos_abi::draw::Color32;
use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, MessageSink, PointerButton, WidgetEvent};
use crate::node::ImageData;
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Widget, WidgetCore};

const CHECK: i32 = 16;
/// Light on any theme, since they stand for no colour at all.
const CHECK_LIGHT: Color32 = Color32::rgb(0xcf, 0xd4, 0xda);
const CHECK_LIGHTER: Color32 = Color32::rgb(0xf2, 0xf4, 0xf6);

/// What the pointer and the keyboard do over a picture; positions are from
/// the view's top left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PictureInput {
    /// The pointer is over the view, or anywhere while a drag is live.
    Pointer {
        x: i32,
        y: i32,
    },
    /// A press on the view began a drag.
    Grab {
        x: i32,
        y: i32,
    },
    Release,
    /// The wheel turned by `delta` pixels, as a scroll view takes it:
    /// positive scrolls down.
    Wheel {
        x: i32,
        y: i32,
        delta: i32,
    },
    Text(char),
}

type PictureCallback = Box<dyn Fn(PictureInput) -> Box<dyn Any>>;

pub struct PictureWidget {
    core: WidgetCore,
    image: ImageData,
    zoom: f64,
    pan: (i32, i32),
    /// Given back by the app, since the tree is rebuilt between a drag's
    /// events, and set at the press, since its release may come before that.
    grabbing: bool,
    focused: bool,
    on_input: Option<PictureCallback>,
}

impl PictureWidget {
    pub fn new(
        image: ImageData,
        zoom: f64,
        pan: (i32, i32),
        grabbing: bool,
        focused: bool,
        on_input: Option<PictureCallback>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            image,
            zoom,
            pan,
            grabbing,
            focused,
            on_input,
        }
    }

    fn emit(&self, input: PictureInput, sink: &mut MessageSink) -> bool {
        match &self.on_input {
            Some(cb) => {
                sink.emit_raw(cb(input));
                true
            }
            None => false,
        }
    }

    fn local(&self, x: i32, y: i32) -> (i32, i32) {
        let rect = self.layout_rect();
        (x - rect.x, y - rect.y)
    }

    fn paint_checks(ctx: &mut PaintContext, rect: Rect) {
        let cols = (rect.width + CHECK - 1) / CHECK;
        let rows = (rect.height + CHECK - 1) / CHECK;
        for row in 0..rows {
            for col in 0..cols {
                let color = match (row + col) & 1 {
                    0 => CHECK_LIGHT,
                    _ => CHECK_LIGHTER,
                };
                ctx.fill_rect(
                    rect.x + col * CHECK,
                    rect.y + row * CHECK,
                    CHECK,
                    CHECK,
                    color,
                );
            }
        }
    }
}

/// `len` at `zoom`, never under a pixel.
pub fn scaled(len: u32, zoom: f64) -> i32 {
    ((len as f64 * zoom).round() as i32).max(1)
}

impl Widget for PictureWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        constraints.constrain(Size::new(constraints.max_width, constraints.max_height))
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
        let (w, h) = (
            scaled(self.image.width, self.zoom),
            scaled(self.image.height, self.zoom),
        );
        let x = rect.x + (rect.width - w) / 2 + self.pan.0;
        let y = rect.y + (rect.height - h) / 2 + self.pan.1;
        let sampling = ImageSampling::for_resize(self.image.width, self.image.height, w, h);
        ctx.with_clip(rect, |ctx| {
            Self::paint_checks(ctx, rect);
            ctx.draw_image(bitmap, x, y, w, h, ImageFit::Stretch, sampling);
        });
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        let inside = |x: i32, y: i32| self.layout_rect().contains(x, y);
        match *event {
            WidgetEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
                ..
            } if inside(x, y) => {
                self.grabbing = true;
                let (x, y) = self.local(x, y);
                self.emit(PictureInput::Grab { x, y }, sink);
                EventResponse::Consumed
            }
            // A release reaches every widget the pointer missed.
            WidgetEvent::PointerUp { .. } if self.grabbing => {
                self.grabbing = false;
                self.emit(PictureInput::Release, sink);
                EventResponse::Consumed
            }
            WidgetEvent::PointerMove { x, y } if self.grabbing || inside(x, y) => {
                let (x, y) = self.local(x, y);
                self.emit(PictureInput::Pointer { x, y }, sink);
                EventResponse::Ignored
            }
            WidgetEvent::Scroll { x, y, delta_y, .. } if delta_y != 0 && inside(x, y) => {
                let (x, y) = self.local(x, y);
                let wheel = PictureInput::Wheel {
                    x,
                    y,
                    delta: delta_y,
                };
                match self.emit(wheel, sink) {
                    true => EventResponse::Consumed,
                    false => EventResponse::Ignored,
                }
            }
            WidgetEvent::TextInput { character } if self.focused => {
                match self.emit(PictureInput::Text(character), sink) {
                    true => EventResponse::Consumed,
                    false => EventResponse::Ignored,
                }
            }
            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                EventResponse::Ignored
            }
            _ => EventResponse::Ignored,
        }
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::ClickFocus
    }

    fn declares_focus(&self) -> bool {
        self.focused
    }
}
