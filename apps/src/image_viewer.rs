//! Image Viewer: one picture, fitted to the window or zoomed and panned.

use std::env;
use std::io::ErrorKind;
use std::path::Path;

use slopos_appkit::widgets::picture::scaled;
use slopos_appkit::{
    Action, App, EmptyState, Icon, ImageData, Node, PictureInput, StatusBar, Surface, WindowSpec,
    column, document_title,
};
use slopos_image::{DecodeOptions, ImageError, PngError};

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

const WINDOW: WindowSpec = WindowSpec::new(900, 640);
const ZOOM_STEP: f64 = 1.2;

#[derive(Clone)]
pub enum Msg {
    Picture(PictureInput),
}

struct ImageViewer {
    path: String,
    loaded: Result<ImageData, Unshown>,
    /// The window's size; the picture's view is all of it but the status bar.
    window: (i32, i32),
    fit_mode: bool,
    zoom: f64,
    pan_x: i32,
    pan_y: i32,
    grabbing: bool,
    /// Where the pointer last was over the view, which the keys zoom around.
    last_x: i32,
    last_y: i32,
}

impl ImageViewer {
    fn new(path: String) -> Self {
        let loaded = slopos_image::load_path(&path, DecodeOptions::default())
            .map_err(|e| {
                eprintln!("image_viewer: {path}: {e}");
                unshown(file_name(&path), &e)
            })
            .and_then(|image| {
                ImageData::new(image.width, image.height, image.pixels).ok_or_else(|| Unshown {
                    title: "This picture is damaged",
                    body: format!(
                        "{} is incomplete or damaged. Open another copy of it.",
                        file_name(&path)
                    ),
                })
            });
        Self {
            path,
            loaded,
            window: (WINDOW.size.width, WINDOW.size.height),
            fit_mode: true,
            zoom: 1.0,
            pan_x: 0,
            pan_y: 0,
            grabbing: false,
            last_x: 0,
            last_y: 0,
        }
    }

    fn view_size(&self) -> (i32, i32) {
        (
            self.window.0.max(1),
            (self.window.1 - StatusBar::HEIGHT).max(1),
        )
    }

    fn fit_zoom(&self) -> f64 {
        let Ok(image) = self.loaded.as_ref() else {
            return 1.0;
        };
        let (view_w, view_h) = self.view_size();
        let zx = view_w as f64 / image.width as f64;
        let zy = view_h as f64 / image.height as f64;
        zx.min(zy).clamp(MIN_ZOOM, MAX_ZOOM)
    }

    fn active_zoom(&self) -> f64 {
        if self.fit_mode {
            self.fit_zoom()
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

    /// Zooms by `factor`, keeping the pixel under `(px, py)` there.
    fn zoom_around(&mut self, px: i32, py: i32, factor: f64) {
        let old_zoom = self.active_zoom();
        let (view_w, view_h) = self.view_size();
        let Ok(image) = self.loaded.as_ref() else {
            return;
        };
        let (width, height) = (image.width, image.height);
        if self.fit_mode {
            self.zoom = old_zoom;
            self.pan_x = 0;
            self.pan_y = 0;
            self.fit_mode = false;
        }
        let old_x = (view_w - scaled(width, old_zoom)) / 2 + self.pan_x;
        let old_y = (view_h - scaled(height, old_zoom)) / 2 + self.pan_y;
        let image_x = (px - old_x) as f64 / old_zoom;
        let image_y = (py - old_y) as f64 / old_zoom;

        self.zoom = (old_zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let base_x = (view_w - scaled(width, self.zoom)) / 2;
        let base_y = (view_h - scaled(height, self.zoom)) / 2;
        self.pan_x = px - base_x - (image_x * self.zoom).round() as i32;
        self.pan_y = py - base_y - (image_y * self.zoom).round() as i32;
    }

    fn picture(&mut self, input: PictureInput) -> Action {
        match input {
            PictureInput::Pointer { x, y } => {
                let moved = (x - self.last_x, y - self.last_y);
                (self.last_x, self.last_y) = (x, y);
                if !self.grabbing {
                    return Action::None;
                }
                self.fit_mode = false;
                self.pan_x += moved.0;
                self.pan_y += moved.1;
            }
            PictureInput::Grab { x, y } => {
                self.grabbing = true;
                (self.last_x, self.last_y) = (x, y);
                return Action::None;
            }
            PictureInput::Release => self.grabbing = false,
            PictureInput::Wheel { x, y, delta } => {
                let factor = if delta > 0 {
                    ZOOM_STEP
                } else {
                    1.0 / ZOOM_STEP
                };
                self.zoom_around(x, y, factor);
            }
            PictureInput::Text('f' | 'F') => self.reset_fit(),
            PictureInput::Text('0') => self.reset_actual(),
            PictureInput::Text('+' | '=') => self.zoom_around(self.last_x, self.last_y, ZOOM_STEP),
            PictureInput::Text('-' | '_') => {
                self.zoom_around(self.last_x, self.last_y, 1.0 / ZOOM_STEP)
            }
            PictureInput::Text(_) => return Action::None,
        }
        Action::Rebuild
    }

    fn status_text(&self) -> String {
        match &self.loaded {
            Ok(image) => format!(
                "{} · {} × {} · {}%",
                file_name(&self.path),
                image.width,
                image.height,
                (self.active_zoom() * 100.0).round() as i32
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

impl App for ImageViewer {
    type Message = Msg;

    fn view(&self) -> Node<Msg> {
        let shown = match &self.loaded {
            Ok(image) => Node::Picture {
                image: image.clone(),
                zoom: self.active_zoom(),
                pan: (self.pan_x, self.pan_y),
                grabbing: self.grabbing,
                focused: true,
                on_input: Some(Msg::Picture),
            },
            Err(unshown) => Node::Background {
                surface: Surface::Base,
                child: Box::new(
                    EmptyState::new(Icon::Image, unshown.title)
                        .body(unshown.body.clone())
                        .into(),
                ),
            },
        };
        column(
            0,
            vec![
                Node::Expand {
                    weight: 1,
                    child: Box::new(shown),
                },
                StatusBar::new(self.status_text()).into(),
            ],
        )
    }

    fn update(&mut self, msg: Msg) -> Action {
        match msg {
            Msg::Picture(input) => self.picture(input),
        }
    }

    fn on_resize(&mut self, width: u32, height: u32) -> Action {
        self.window = (width as i32, height as i32);
        Action::None
    }

    fn title(&self) -> String {
        document_title(file_name(&self.path), NAME)
    }

    fn app_id(&self) -> &str {
        "org.slopos.image-viewer"
    }

    fn window(&self) -> WindowSpec {
        WINDOW
    }
}

pub fn image_viewer_main() -> ! {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| String::from(DEFAULT_IMAGE));
    slopos_appkit::run_app(ImageViewer::new(path))
}
