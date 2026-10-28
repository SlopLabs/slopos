//! Image Viewer: one picture, fitted to the window or zoomed and panned.

use std::env;
use std::io::ErrorKind;
use std::path::Path;

mod checker;

use slopos_abi::damage::DamageRect;
use slopos_appkit::embed::Embedded;
use slopos_appkit::{EmptyState, Icon, Node, Rect, StatusBar, Surface, document_title};
use slopos_gfx::DrawBuffer;
use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};
use slopos_image::{DecodeOptions, Image, ImageError, PngError};
use slopos_windowing::{ControlFlow, Event, Window, WindowedApp};

pub const NAME: &str = "Image Viewer";

const DEFAULT_IMAGE: &str = "/usr/share/slopos/wallpapers/default.png";
const MIN_ZOOM: f64 = 0.05;
const MAX_ZOOM: f64 = 32.0;

struct Unshown {
    title: &'static str,
    body: String,
}

fn unshown(name: &str, error: &ImageError) -> Unshown {
    let (title, body) = match error {
        ImageError::Io(e) if e.kind() == ErrorKind::NotFound => (
            "This picture isn't there",
            format!("{name} was moved or deleted. Open it again from where it is now."),
        ),
        ImageError::Io(e) if e.kind() == ErrorKind::PermissionDenied => (
            "You can't open this picture",
            format!("{name} belongs to someone else. Ask them to share it with you."),
        ),
        ImageError::Io(_) => (
            "This picture couldn't be read",
            format!("Something went wrong reading {name}. Try opening it again."),
        ),
        ImageError::Png(PngError::BadSignature) => (
            "This isn't a PNG picture",
            format!(
                "{NAME} opens PNG pictures, and {name} is another kind of file. Open a PNG picture instead."
            ),
        ),
        ImageError::Png(PngError::Unsupported(_)) => (
            "This PNG can't be shown yet",
            format!("{name} uses a kind of PNG {NAME} can't show. Save it again as a plain PNG."),
        ),
        ImageError::Png(PngError::OutputOverflow | PngError::ResourceLimitExceeded) => (
            "This picture is too large",
            format!("{name} is larger than {NAME} can show. Open a smaller picture."),
        ),
        ImageError::Png(_) => (
            "This picture is damaged",
            format!("{name} is incomplete or damaged. Open another copy of it."),
        ),
    };
    Unshown { title, body }
}

fn embed(fb: &mut DrawBuffer<'_>, node: &Node<()>, rect: Rect) {
    Embedded::new(node, rect).paint(fb, rect);
}

struct ImageViewer {
    path: String,
    loaded: Result<Image, Unshown>,
    fit_mode: bool,
    zoom: f64,
    pan_x: i32,
    pan_y: i32,
    dragging: bool,
    last_x: i32,
    last_y: i32,
}

impl ImageViewer {
    fn new(path: String) -> Self {
        let loaded = slopos_image::load_path(&path, DecodeOptions::default()).map_err(|e| {
            eprintln!("image_viewer: {path}: {e}");
            unshown(file_name(&path), &e)
        });
        Self {
            path,
            loaded,
            fit_mode: true,
            zoom: 1.0,
            pan_x: 0,
            pan_y: 0,
            dragging: false,
            last_x: 0,
            last_y: 0,
        }
    }

    fn view_height(height: u32) -> i32 {
        (height as i32 - StatusBar::HEIGHT).max(1)
    }

    fn fit_zoom_for(&self, view_w: i32, view_h: i32) -> f64 {
        let Ok(image) = self.loaded.as_ref() else {
            return 1.0;
        };
        if image.width == 0 || image.height == 0 || view_w <= 0 || view_h <= 0 {
            return 1.0;
        }
        let zx = view_w as f64 / image.width as f64;
        let zy = view_h as f64 / image.height as f64;
        zx.min(zy).clamp(MIN_ZOOM, MAX_ZOOM)
    }

    fn active_zoom(&self, view_w: i32, view_h: i32) -> f64 {
        if self.fit_mode {
            self.fit_zoom_for(view_w, view_h)
        } else {
            self.zoom.clamp(MIN_ZOOM, MAX_ZOOM)
        }
    }

    fn reset_fit(&mut self) {
        self.fit_mode = true;
        self.pan_x = 0;
        self.pan_y = 0;
    }

    fn reset_actual(&mut self) {
        self.fit_mode = false;
        self.zoom = 1.0;
        self.pan_x = 0;
        self.pan_y = 0;
    }

    fn zoom_around(&mut self, px: i32, py: i32, view_w: i32, view_h: i32, factor: f64) {
        let Ok(image) = self.loaded.as_ref() else {
            return;
        };
        let old_zoom = self.active_zoom(view_w, view_h);
        if self.fit_mode {
            self.zoom = old_zoom;
            self.pan_x = 0;
            self.pan_y = 0;
            self.fit_mode = false;
        }
        let old_w = scaled_len(image.width, old_zoom);
        let old_h = scaled_len(image.height, old_zoom);
        let old_x = (view_w - old_w) / 2 + self.pan_x;
        let old_y = (view_h - old_h) / 2 + self.pan_y;
        let image_x = (px - old_x) as f64 / old_zoom;
        let image_y = (py - old_y) as f64 / old_zoom;

        self.zoom = (old_zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let new_w = scaled_len(image.width, self.zoom);
        let new_h = scaled_len(image.height, self.zoom);
        let base_x = (view_w - new_w) / 2;
        let base_y = (view_h - new_h) / 2;
        self.pan_x = px - base_x - (image_x * self.zoom).round() as i32;
        self.pan_y = py - base_y - (image_y * self.zoom).round() as i32;
    }

    fn status_text(&self, zoom: f64) -> String {
        match &self.loaded {
            Ok(image) => format!(
                "{} · {} × {} · {}%",
                file_name(&self.path),
                image.width,
                image.height,
                (zoom * 100.0).round() as i32
            ),
            Err(_) => String::from(file_name(&self.path)),
        }
    }
}

fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

impl WindowedApp for ImageViewer {
    fn init(&mut self, win: &mut Window) {
        win.set_title(&document_title(file_name(&self.path), NAME));
        win.set_app_id("org.slopos.image-viewer");
        win.request_redraw();
    }

    fn on_event(&mut self, win: &mut Window, event: Event) -> ControlFlow {
        let view_w = win.width() as i32;
        let view_h = Self::view_height(win.height());
        match event {
            Event::CloseRequest => ControlFlow::Exit,
            Event::Configure { .. } => {
                win.request_redraw();
                ControlFlow::Continue
            }
            Event::PointerMotion { x, y } => {
                if self.dragging {
                    self.fit_mode = false;
                    self.pan_x += x - self.last_x;
                    self.pan_y += y - self.last_y;
                    win.request_redraw();
                }
                self.last_x = x;
                self.last_y = y;
                ControlFlow::Continue
            }
            Event::PointerPress { .. } => {
                let (x, y) = win.pointer();
                self.dragging = true;
                self.last_x = x;
                self.last_y = y;
                ControlFlow::Continue
            }
            Event::PointerRelease { .. } => {
                self.dragging = false;
                ControlFlow::Continue
            }
            Event::PointerAxis { value_v120, .. } => {
                let (x, y) = win.pointer();
                let factor = if value_v120 > 0 { 1.2 } else { 1.0 / 1.2 };
                self.zoom_around(x, y.min(view_h - 1), view_w, view_h, factor);
                win.request_redraw();
                ControlFlow::Continue
            }
            Event::KeyPress { codepoint, .. } => {
                match char::from_u32(codepoint) {
                    Some('f' | 'F') => self.reset_fit(),
                    Some('0') => self.reset_actual(),
                    Some('+' | '=') => {
                        let (x, y) = win.pointer();
                        self.zoom_around(x, y.min(view_h - 1), view_w, view_h, 1.2);
                    }
                    Some('-' | '_') => {
                        let (x, y) = win.pointer();
                        self.zoom_around(x, y.min(view_h - 1), view_w, view_h, 1.0 / 1.2);
                    }
                    _ => {}
                }
                win.request_redraw();
                ControlFlow::Continue
            }
            _ => ControlFlow::Continue,
        }
    }

    fn draw(&mut self, fb: &mut DrawBuffer<'_>) {
        let view_h = Self::view_height(fb.height());
        let view_w = fb.width() as i32;
        let zoom = self.active_zoom(view_w, view_h);
        checker::draw(fb, view_h);

        match self.loaded {
            Ok(ref image) => {
                if let Some(bitmap) = BitmapRef::new(image.width, image.height, &image.pixels) {
                    let target_w = scaled_len(image.width, zoom);
                    let target_h = scaled_len(image.height, zoom);
                    let x = (view_w - target_w) / 2 + self.pan_x;
                    let y = (view_h - target_h) / 2 + self.pan_y;
                    let clip = DamageRect {
                        x0: 0,
                        y0: 0,
                        x1: view_w - 1,
                        y1: view_h - 1,
                    };
                    slopos_gfx::image::draw_image_clipped(
                        fb,
                        bitmap,
                        x,
                        y,
                        target_w,
                        target_h,
                        ImageFit::Stretch,
                        ImageSampling::for_resize(image.width, image.height, target_w, target_h),
                        &clip,
                    );
                }
            }
            Err(ref unshown) => {
                let state =
                    EmptyState::<()>::new(Icon::Image, unshown.title).body(unshown.body.clone());
                let state = Node::Background {
                    surface: Surface::Base,
                    child: Box::new(state.into()),
                };
                embed(fb, &state, Rect::new(0, 0, view_w, view_h));
            }
        }

        let status: Node<()> = StatusBar::new(self.status_text(zoom)).into();
        embed(fb, &status, Rect::new(0, view_h, view_w, StatusBar::HEIGHT));
    }
}

pub fn image_viewer_main() {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| String::from(DEFAULT_IMAGE));
    slopos_windowing::run(ImageViewer::new(path), 900, 640);
}

fn scaled_len(value: u32, zoom: f64) -> i32 {
    ((value as f64 * zoom).round() as i32).max(1)
}
