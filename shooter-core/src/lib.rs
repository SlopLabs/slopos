//! Breach: a first-person shooter on a grid of cells, drawn by casting one
//! ray per screen column. Everything here is pure: the app feeds it input and
//! elapsed time and shows the frame it draws.

#![forbid(unsafe_code)]

pub mod art;
pub mod game;
pub mod level;
pub mod render;
mod rng;

pub use game::{Game, Input, Outcome};
pub use render::{HEIGHT, Renderer, WIDTH};
