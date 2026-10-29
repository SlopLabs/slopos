//! The shipped icons, all by one set of rules: white glyphs of a few filled
//! shapes on the 64 grid, depth only from a shape behind at [`BEHIND`] or a
//! cut-out, each inside a keyline of one area.

use slopos_abi::draw::Color32;
use slopos_appkit::app_icon;
use slopos_theme_core::AppTile;

use crate::raster::{Canvas, Shape, capsule, circle, polygon, polyline, rrect};

/// In 64ths of the tile's side.
const WEIGHT: f32 = 5.0;
const ROUND: f32 = 3.0;
const BEHIND: f32 = 0.6;

#[derive(Clone, Copy, Debug)]
pub enum Art {
    Files,
    Editor,
    ImageViewer,
    Terminal,
    Installer,
    Sysmon,
}

struct Layer(Shape, f32);

impl Art {
    fn tile(self) -> AppTile {
        match self {
            Art::Files => AppTile::Blue,
            Art::Editor => AppTile::Violet,
            Art::ImageViewer => AppTile::Orange,
            Art::Terminal => AppTile::Graphite,
            Art::Installer => AppTile::Teal,
            Art::Sysmon => AppTile::Green,
        }
    }

    fn layers(self) -> Vec<Layer> {
        match self {
            Art::Files => vec![
                Layer(
                    rrect(13.0, 21.0, 51.0, 46.0, ROUND)
                        .union(rrect(13.0, 17.0, 29.0, 25.0, ROUND)),
                    BEHIND,
                ),
                Layer(rrect(13.0, 26.0, 51.0, 46.0, ROUND), 1.0),
            ],
            Art::Editor => {
                let line = |x0: f32, x1: f32, y: f32| rrect(x0, y, x1, y + 4.0, 2.0);
                let lines = line(23.0, 41.0, 19.0)
                    .union(line(27.0, 38.0, 26.0))
                    .union(line(27.0, 35.0, 33.0))
                    .union(line(23.0, 39.0, 40.0));
                vec![Layer(
                    rrect(17.0, 12.0, 47.0, 52.0, ROUND).minus(lines),
                    1.0,
                )]
            }
            Art::ImageViewer => {
                let hills = polygon(&[
                    (19.0, 41.0),
                    (27.0, 31.0),
                    (33.0, 37.0),
                    (37.0, 33.0),
                    (45.0, 41.0),
                ])
                .grow(1.0);
                let sun = circle(40.5, 25.5, 3.5);
                vec![Layer(
                    rrect(13.0, 17.0, 51.0, 47.0, ROUND).minus(hills.union(sun)),
                    1.0,
                )]
            }
            Art::Terminal => vec![
                Layer(capsule((34.0, 43.0), (48.0, 43.0), WEIGHT), BEHIND),
                Layer(
                    polyline(&[(18.0, 20.0), (29.5, 31.5), (18.0, 43.0)], WEIGHT),
                    1.0,
                ),
            ],
            Art::Installer => vec![
                Layer(
                    rrect(15.0, 37.0, 49.0, 49.0, ROUND).minus(circle(42.5, 43.0, 2.0)),
                    BEHIND,
                ),
                Layer(
                    capsule((32.0, 17.5), (32.0, 27.0), WEIGHT)
                        .union(polygon(&[(24.0, 25.0), (40.0, 25.0), (32.0, 34.0)]).grow(1.0)),
                    1.0,
                ),
            ],
            Art::Sysmon => {
                let points = [
                    (15.5, 38.0),
                    (24.0, 29.0),
                    (31.0, 34.0),
                    (40.0, 22.5),
                    (48.5, 27.5),
                ];
                let mut outline = points.to_vec();
                outline.extend([(48.5, 46.0), (15.5, 46.0)]);
                let area = polygon(&outline).and(rrect(13.0, 17.0, 51.0, 46.0, ROUND));
                vec![Layer(area, BEHIND), Layer(polyline(&points, WEIGHT), 1.0)]
            }
        }
    }
}

pub fn render(art: Art, size: u32) -> Vec<Color32> {
    let mut canvas = Canvas::over(size, &app_icon::tile(size, art.tile()));
    for Layer(shape, opacity) in art.layers() {
        canvas.white(&shape, opacity);
    }
    canvas.pixels()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(x0, y0, x1, y1)` in 64ths, of about one area each.
    const LANDSCAPE: (f32, f32, f32, f32) = (13.0, 17.0, 51.0, 47.0);
    const PORTRAIT: (f32, f32, f32, f32) = (17.0, 12.0, 47.0, 52.0);
    const SQUARE: (f32, f32, f32, f32) = (15.0, 15.0, 49.0, 49.0);

    const ALL: [Art; 6] = [
        Art::Files,
        Art::Editor,
        Art::ImageViewer,
        Art::Terminal,
        Art::Installer,
        Art::Sysmon,
    ];

    fn keyline(art: Art) -> (f32, f32, f32, f32) {
        match art {
            Art::Files | Art::ImageViewer | Art::Terminal | Art::Sysmon => LANDSCAPE,
            Art::Editor => PORTRAIT,
            Art::Installer => SQUARE,
        }
    }

    fn glyph_box(art: Art, size: u32) -> Option<(f32, f32, f32, f32)> {
        let tile = app_icon::tile(size, art.tile());
        let unit = size as f32 / 64.0;
        let mut found: Option<(f32, f32, f32, f32)> = None;
        for (i, (under, over)) in tile.iter().zip(&render(art, size)).enumerate() {
            if under == over {
                continue;
            }
            let (x, y) = ((i as u32 % size) as f32, (i as u32 / size) as f32);
            let (x0, y0, x1, y1) = (x / unit, y / unit, (x + 1.0) / unit, (y + 1.0) / unit);
            found = Some(match found {
                None => (x0, y0, x1, y1),
                Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
            });
        }
        found
    }

    #[test]
    fn every_glyph_fills_its_keyline_and_stays_inside_it() {
        for art in ALL {
            let (x0, y0, x1, y1) = keyline(art);
            for size in [64, 128] {
                let (gx0, gy0, gx1, gy1) = glyph_box(art, size).expect("a glyph");
                assert!(
                    gx0 >= x0 && gy0 >= y0 && gx1 <= x1 && gy1 <= y1,
                    "{art:?} at {size}: {:?} past {:?}",
                    (gx0, gy0, gx1, gy1),
                    (x0, y0, x1, y1)
                );
                let filled = (gx1 - gx0) * (gy1 - gy0) / ((x1 - x0) * (y1 - y0));
                assert!(filled >= 0.8, "{art:?} fills {filled:.2} of its keyline");
            }
        }
    }

    #[test]
    fn a_glyph_is_white_and_nothing_else() {
        for art in ALL {
            let tile = app_icon::tile(128, art.tile());
            for (under, over) in tile.iter().zip(render(art, 128)) {
                assert_eq!(under.alpha(), over.alpha(), "{art:?}");
                for (u, o) in [
                    (under.red(), over.red()),
                    (under.green(), over.green()),
                    (under.blue(), over.blue()),
                ] {
                    assert!(o >= u, "{art:?}: {under:?} became {over:?}");
                }
            }
        }
    }
}
