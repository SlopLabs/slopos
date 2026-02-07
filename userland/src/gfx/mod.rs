pub mod font;

pub use slopos_abi::Canvas;
pub use slopos_abi::damage::{self, DamageRect, MAX_DAMAGE_REGIONS};
pub use slopos_abi::pixel::PixelFormat;
pub use slopos_gfx::DrawBuffer;
pub use slopos_gfx::damage::DamageTracker;

pub use slopos_gfx::canvas_ops::{
    circle as draw_circle, circle_filled as draw_circle_filled, fill_rect, line as draw_line,
    rect as draw_rect,
};
