//! The frame: one ray per column through the grid for the walls, a ray per
//! floor row for the floor and ceiling, then the sprites far to near against
//! the walls' depths, the gun and the tints.

use crate::art::{Pose, SPRITE, Sprite, Sprites, TEX, Texture, Textures, mix, rgb, scale};
use crate::game::Game;
use crate::level::Cell;

pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 200;
/// Half the field of view's tangent: about 66 degrees across.
const PLANE: f32 = 0.66;
const FOG: u32 = rgb(10, 11, 14);

pub struct Renderer {
    textures: Textures,
    sprites: Sprites,
    depth: Vec<f32>,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

/// How much of a colour survives `distance` cells of haze.
fn light(distance: f32) -> f32 {
    (1.15 / (1.0 + distance * 0.16)).min(1.0)
}

struct Hit<'a> {
    distance: f32,
    u: usize,
    side: f32,
    texture: &'a Texture,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            textures: Textures::new(),
            sprites: Sprites::new(),
            depth: vec![f32::INFINITY; WIDTH],
        }
    }

    /// Draws `game` into `out`, `WIDTH * HEIGHT` pixels `0xffRRGGBB`.
    pub fn draw(&mut self, game: &Game, out: &mut [u32]) {
        assert_eq!(out.len(), WIDTH * HEIGHT);
        let p = &game.player;
        let (sin, cos) = p.angle.sin_cos();
        let dir = (cos, sin);
        let plane = (-sin * PLANE, cos * PLANE);
        self.floor_and_ceiling(game, dir, plane, out);
        for x in 0..WIDTH {
            let camera = 2.0 * x as f32 / WIDTH as f32 - 1.0;
            let ray = (dir.0 + plane.0 * camera, dir.1 + plane.1 * camera);
            self.column(game, x, ray, out);
        }
        self.things(game, dir, plane, out);
        self.gun(game, out);
        self.tint(game, out);
    }

    fn floor_and_ceiling(&self, game: &Game, dir: (f32, f32), plane: (f32, f32), out: &mut [u32]) {
        let p = &game.player;
        let left = (dir.0 - plane.0, dir.1 - plane.1);
        let right = (dir.0 + plane.0, dir.1 + plane.1);
        let half = HEIGHT as f32 / 2.0;
        for y in HEIGHT / 2 + 1..HEIGHT {
            let row = half / (y as f32 - half);
            let step = (
                row * (right.0 - left.0) / WIDTH as f32,
                row * (right.1 - left.1) / WIDTH as f32,
            );
            let mut at = (p.x + row * left.0, p.y + row * left.1);
            let shade = light(row);
            let mirror = HEIGHT - 1 - y;
            for x in 0..WIDTH {
                let u = ((at.0 * TEX as f32) as i32 & (TEX as i32 - 1)) as usize;
                let v = ((at.1 * TEX as f32) as i32 & (TEX as i32 - 1)) as usize;
                out[y * WIDTH + x] = mix(FOG, self.textures.floor.at(u, v), shade);
                out[mirror * WIDTH + x] = mix(FOG, self.textures.ceiling.at(u, v), shade);
                at = (at.0 + step.0, at.1 + step.1);
            }
        }
    }

    fn column(&mut self, game: &Game, x: usize, ray: (f32, f32), out: &mut [u32]) {
        let Some(hit) = cast(&self.textures, game, ray) else {
            self.depth[x] = f32::INFINITY;
            return;
        };
        self.depth[x] = hit.distance;
        let height = HEIGHT as f32 / hit.distance;
        let top = HEIGHT as f32 / 2.0 - height / 2.0;
        let shade = light(hit.distance);
        let y0 = top.max(0.0) as usize;
        let y1 = ((top + height).ceil() as usize).min(HEIGHT);
        for y in y0..y1 {
            let v = (((y as f32 - top) / height * TEX as f32) as usize).min(TEX - 1);
            let c = scale(hit.texture.at(hit.u, v), hit.side);
            out[y * WIDTH + x] = mix(FOG, c, shade);
        }
    }

    fn things(&self, game: &Game, dir: (f32, f32), plane: (f32, f32), out: &mut [u32]) {
        let p = &game.player;
        let mut shown: Vec<(f32, f32, &Sprite, f32)> = Vec::new();
        for e in &game.enemies {
            let pose = Pose::of(e.mode, e.stride);
            let size = match e.kind {
                crate::level::EnemyKind::Warden => 1.15,
                _ => 0.9,
            };
            shown.push((e.x, e.y, self.sprites.enemy(e.kind, pose), size));
        }
        for item in game.items.iter().filter(|i| !i.taken) {
            shown.push((item.x, item.y, self.sprites.item(item.kind), 0.7));
        }
        let away = |&(x, y, _, _): &(f32, f32, &Sprite, f32)| (x - p.x).powi(2) + (y - p.y).powi(2);
        shown.sort_by(|a, b| away(b).total_cmp(&away(a)));
        let inverse = 1.0 / (plane.0 * dir.1 - dir.0 * plane.1);
        for (x, y, sprite, size) in shown {
            let (rx, ry) = (x - p.x, y - p.y);
            let across = inverse * (dir.1 * rx - dir.0 * ry);
            let depth = inverse * (-plane.1 * rx + plane.0 * ry);
            if depth <= 0.1 {
                continue;
            }
            let centre = WIDTH as f32 / 2.0 * (1.0 + across / depth);
            let side = HEIGHT as f32 / depth * size;
            let floor = HEIGHT as f32 / 2.0 + HEIGHT as f32 / depth / 2.0;
            let top = floor - side;
            let left = centre - side / 2.0;
            let shade = light(depth);
            let x0 = left.max(0.0) as usize;
            let x1 = ((left + side).ceil().max(0.0) as usize).min(WIDTH);
            let y0 = top.max(0.0) as usize;
            let y1 = (floor.ceil().max(0.0) as usize).min(HEIGHT);
            for sx in x0..x1 {
                if depth >= self.depth[sx] {
                    continue;
                }
                let u = (((sx as f32 - left) / side * SPRITE as f32) as usize).min(SPRITE - 1);
                for sy in y0..y1 {
                    let v = (((sy as f32 - top) / side * SPRITE as f32) as usize).min(SPRITE - 1);
                    let c = sprite.0[v * SPRITE + u];
                    if c != 0 {
                        out[sy * WIDTH + sx] = mix(FOG, c, shade);
                    }
                }
            }
        }
    }

    /// The rivet gun, low and right of centre, bobbing as the player walks.
    fn gun(&self, game: &Game, out: &mut [u32]) {
        let p = &game.player;
        let bob_x = (p.stride * 3.0).sin() * 4.0;
        let bob_y = (p.stride * 6.0).sin().abs() * 3.0;
        let kick = if p.flash > 0.0 { 6.0 } else { 0.0 };
        let cx = WIDTH as f32 / 2.0 + 18.0 + bob_x;
        let base = HEIGHT as f32 + 4.0 - bob_y + kick;
        let body = rgb(96, 100, 108);
        let grip = rgb(60, 50, 44);
        let mut fill = |x0: f32, y0: f32, x1: f32, y1: f32, c: u32| {
            for y in (y0.max(0.0) as usize)..(y1.min(HEIGHT as f32) as usize) {
                for x in (x0.max(0.0) as usize)..(x1.min(WIDTH as f32) as usize) {
                    let lit = 1.2 - (y as f32 - y0) / (y1 - y0).max(1.0) * 0.5;
                    out[y * WIDTH + x] = scale(c, lit);
                }
            }
        };
        fill(cx + 4.0, base - 26.0, cx + 26.0, base, grip);
        fill(cx - 14.0, base - 44.0, cx + 22.0, base - 24.0, body);
        fill(
            cx - 8.0,
            base - 58.0,
            cx + 6.0,
            base - 44.0,
            scale(body, 0.8),
        );
        fill(
            cx - 5.0,
            base - 62.0,
            cx + 3.0,
            base - 58.0,
            rgb(40, 42, 46),
        );
        fill(
            cx + 10.0,
            base - 40.0,
            cx + 18.0,
            base - 30.0,
            rgb(230, 170, 50),
        );
        if p.flash > 0.0 {
            let (fx, fy) = (cx - 1.0, base - 66.0);
            for y in (fy - 9.0) as usize..(fy + 9.0) as usize {
                for x in (fx - 9.0) as usize..(fx + 9.0) as usize {
                    let d = ((x as f32 - fx).powi(2) + (y as f32 - fy).powi(2)).sqrt() / 9.0;
                    if d < 1.0 && y < HEIGHT && x < WIDTH {
                        let i = y * WIDTH + x;
                        out[i] = mix(out[i], rgb(255, 220, 140), 1.0 - d);
                    }
                }
            }
        }
        let (mx, my) = (WIDTH / 2, HEIGHT / 2);
        for (dx, dy) in [
            (-4, 0),
            (-3, 0),
            (3, 0),
            (4, 0),
            (0, -4),
            (0, -3),
            (0, 3),
            (0, 4),
        ] {
            let i = (my as i32 + dy) as usize * WIDTH + (mx as i32 + dx) as usize;
            out[i] = rgb(220, 240, 220);
        }
    }

    fn tint(&self, game: &Game, out: &mut [u32]) {
        let p = &game.player;
        let red = if p.health <= 0 {
            0.55
        } else {
            (p.hurt / 0.3).min(1.0) * 0.35
        };
        if red > 0.0 {
            for c in out.iter_mut() {
                *c = mix(*c, rgb(180, 20, 10), red);
            }
        }
    }
}

fn cast<'a>(textures: &'a Textures, game: &Game, ray: (f32, f32)) -> Option<Hit<'a>> {
    let p = &game.player;
    let (mut cx, mut cy) = (p.x.floor() as i32, p.y.floor() as i32);
    let delta = (
        if ray.0 == 0.0 {
            f32::INFINITY
        } else {
            (1.0 / ray.0).abs()
        },
        if ray.1 == 0.0 {
            f32::INFINITY
        } else {
            (1.0 / ray.1).abs()
        },
    );
    let (step_x, mut side_x) = if ray.0 < 0.0 {
        (-1, (p.x - cx as f32) * delta.0)
    } else {
        (1, (cx as f32 + 1.0 - p.x) * delta.0)
    };
    let (step_y, mut side_y) = if ray.1 < 0.0 {
        (-1, (p.y - cy as f32) * delta.1)
    } else {
        (1, (cy as f32 + 1.0 - p.y) * delta.1)
    };
    let limit = (game.level.width + game.level.height) * 2;
    for _ in 0..limit {
        let across_x = side_x < side_y;
        let distance = if across_x {
            side_x += delta.0;
            cx += step_x;
            side_x - delta.0
        } else {
            side_y += delta.1;
            cy += step_y;
            side_y - delta.1
        };
        let cell = game.level.cell(cx, cy);
        if cell.is_floor() {
            continue;
        }
        let along = if across_x {
            p.y + distance * ray.1
        } else {
            p.x + distance * ray.0
        };
        let mut along = along - along.floor();
        if (across_x && ray.0 > 0.0) || (!across_x && ray.1 < 0.0) {
            along = 1.0 - along;
        }
        let texture = match cell {
            Cell::Wall(kind) => textures.wall(kind),
            Cell::Exit => &textures.exit,
            Cell::Door(lock) => {
                let open = game.door_at(cx, cy).map_or(0.0, |d| d.open);
                if along < open {
                    continue;
                }
                along -= open;
                textures.door(lock)
            }
            Cell::Floor => continue,
        };
        let u = ((along * TEX as f32) as usize).min(TEX - 1);
        let side = if across_x { 1.0 } else { 0.78 };
        return Some(Hit {
            distance: distance.max(1e-3),
            u,
            side,
            texture,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::level::parse;

    fn frame(game: &Game) -> Vec<u32> {
        let mut out = vec![0; WIDTH * HEIGHT];
        Renderer::new().draw(game, &mut out);
        out
    }

    #[test]
    fn every_pixel_is_drawn_and_opaque() {
        let out = frame(&Game::new(1));
        assert!(out.iter().all(|&c| c >> 24 == 0xff));
    }

    #[test]
    fn a_near_wall_fills_more_of_the_column_than_a_far_one() {
        let near = Game::with_level(parse("t", &["####", "#>.#", "####"]).unwrap(), 1);
        let far = Game::with_level(
            parse("t", &["##########", "#>.......#", "##########"]).unwrap(),
            1,
        );
        let rows = |game: &Game| {
            let mut r = Renderer::new();
            let mut out = vec![0; WIDTH * HEIGHT];
            r.draw(game, &mut out);
            r.depth[WIDTH / 2]
        };
        assert!(rows(&near) < rows(&far));
        assert!((rows(&near) - 1.5).abs() < 0.01, "{}", rows(&near));
    }

    #[test]
    fn an_enemy_in_view_changes_the_frame_and_one_behind_a_wall_does_not() {
        let empty = parse("t", &["##########", "#>.......#", "##########"]).unwrap();
        let seen = parse("t", &["##########", "#>....c..#", "##########"]).unwrap();
        let hidden = parse("t", &["##########", "#>..#.c..#", "##########"]).unwrap();
        let wall = parse("t", &["##########", "#>..#....#", "##########"]).unwrap();
        assert_ne!(
            frame(&Game::with_level(empty, 1)),
            frame(&Game::with_level(seen, 1))
        );
        assert_eq!(
            frame(&Game::with_level(wall, 1)),
            frame(&Game::with_level(hidden, 1))
        );
    }
}
