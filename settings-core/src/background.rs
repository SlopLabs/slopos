//! The desktop's background: a gradient of the theme's, and over it, when a
//! person chose one, a picture placed on the screen as [`Fit`] says.

use alloc::vec::Vec;

use slopos_theme_core::Gradient;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Background<'a> {
    pub gradient: &'a Gradient,
    pub picture: Option<&'a str>,
    pub fit: Fit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// Covers the screen, cut at the edges it overhangs.
    Fill,
    /// Wholly on the screen, the gradient at the sides it does not reach.
    Fit,
    /// At its own size in the middle.
    Center,
    Stretch,
    /// At its own size, repeated from the top left.
    Tile,
}

/// The most copies a tiled picture is drawn in, past which it fills the
/// screen instead: a picture a few pixels across would take longer to tile
/// than to look at.
pub const MOST_TILES: usize = 4096;

impl Fit {
    pub const ALL: [Fit; 5] = [Fit::Fill, Fit::Fit, Fit::Center, Fit::Stretch, Fit::Tile];

    pub const fn name(self) -> &'static str {
        match self {
            Fit::Fill => "fill",
            Fit::Fit => "fit",
            Fit::Center => "center",
            Fit::Stretch => "stretch",
            Fit::Tile => "tile",
        }
    }

    pub fn named(name: &str) -> Option<Fit> {
        Fit::ALL.into_iter().find(|fit| fit.name() == name)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Fit::Fill => "Fill the screen",
            Fit::Fit => "Fit inside the screen",
            Fit::Center => "Center",
            Fit::Stretch => "Stretch",
            Fit::Tile => "Tile",
        }
    }
}

/// How a copy of the picture is scaled into its [`Place`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    Cover,
    Contain,
    Stretch,
    Actual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: Scale,
}

/// Where the copies of a `picture`-sized picture go on a `screen`, each
/// clipped to it by whoever draws them.
pub fn places(fit: Fit, picture: (u32, u32), screen: (u32, u32)) -> Vec<Place> {
    let (pw, ph) = (picture.0 as i32, picture.1 as i32);
    let (sw, sh) = (screen.0 as i32, screen.1 as i32);
    if pw <= 0 || ph <= 0 || sw <= 0 || sh <= 0 {
        return Vec::new();
    }
    let whole = |scale| Place {
        x: 0,
        y: 0,
        width: sw,
        height: sh,
        scale,
    };
    match fit {
        Fit::Fill => alloc::vec![whole(Scale::Cover)],
        Fit::Fit => alloc::vec![whole(Scale::Contain)],
        Fit::Stretch => alloc::vec![whole(Scale::Stretch)],
        Fit::Center => alloc::vec![Place {
            x: (sw - pw) / 2,
            y: (sh - ph) / 2,
            width: pw,
            height: ph,
            scale: Scale::Actual,
        }],
        Fit::Tile => {
            let (across, down) = ((sw + pw - 1) / pw, (sh + ph - 1) / ph);
            if across as usize * down as usize > MOST_TILES {
                return places(Fit::Fill, picture, screen);
            }
            (0..down)
                .flat_map(|row| {
                    (0..across).map(move |column| Place {
                        x: column * pw,
                        y: row * ph,
                        width: pw,
                        height: ph,
                        scale: Scale::Actual,
                    })
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fit_is_named_and_read_back() {
        for fit in Fit::ALL {
            assert_eq!(Fit::named(fit.name()), Some(fit));
            slopos_theme_core::words::label(fit.label(), &[]).unwrap();
        }
        assert_eq!(Fit::named("zoom"), None);
    }

    #[test]
    fn a_centred_picture_sits_in_the_middle_even_when_larger() {
        assert_eq!(
            places(Fit::Center, (100, 50), (1000, 500)),
            [Place {
                x: 450,
                y: 225,
                width: 100,
                height: 50,
                scale: Scale::Actual
            }]
        );
        assert_eq!(places(Fit::Center, (1200, 600), (1000, 500))[0].x, -100);
    }

    #[test]
    fn tiles_cover_the_screen_from_its_top_left() {
        let tiles = places(Fit::Tile, (300, 200), (1000, 500));
        assert_eq!(tiles.len(), 4 * 3);
        assert_eq!((tiles[0].x, tiles[0].y), (0, 0));
        let last = tiles.last().unwrap();
        assert!(last.x + last.width >= 1000 && last.y + last.height >= 500);
    }

    #[test]
    fn a_tiny_tile_fills_the_screen_instead() {
        assert_eq!(
            places(Fit::Tile, (2, 2), (1920, 1080)),
            places(Fit::Fill, (2, 2), (1920, 1080))
        );
    }

    #[test]
    fn nothing_to_place_places_nothing() {
        assert!(places(Fit::Fill, (0, 10), (100, 100)).is_empty());
        assert!(places(Fit::Tile, (10, 10), (0, 100)).is_empty());
    }
}
