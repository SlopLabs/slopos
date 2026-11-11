//! The game's frame as the picture the window shows: its colours are its
//! content, drawn by the game's renderer.

use std::sync::Arc;

use slopos_appkit::{Color32, ImageData};
use slopos_shooter_core::{HEIGHT, WIDTH};

pub fn picture(pixels: &[u32]) -> ImageData {
    let pixels: Arc<[Color32]> = pixels.iter().map(|&c| Color32(c)).collect();
    ImageData {
        width: WIDTH as u32,
        height: HEIGHT as u32,
        pixels,
    }
}
