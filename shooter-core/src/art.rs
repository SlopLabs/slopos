//! Every picture the game shows, drawn from shapes and noise at start: the
//! walls' textures and the sprites of what stands on the floor. Pixels are
//! `0xAARRGGBB`; a sprite's transparent pixels are 0.

use crate::game::Mode;
use crate::level::{EnemyKind, ItemKind, Lock, WallKind};

pub const TEX: usize = 64;
pub const SPRITE: usize = 64;

pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    0xff00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Each channel times `f`, which may brighten past 1.
pub fn scale(c: u32, f: f32) -> u32 {
    let ch = |shift: u32| (((c >> shift) & 0xff) as f32 * f).clamp(0.0, 255.0) as u32;
    0xff00_0000 | ch(16) << 16 | ch(8) << 8 | ch(0)
}

/// `a` toward `b` by `t` in `0..=1`.
pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |shift: u32| {
        let (x, y) = (((a >> shift) & 0xff) as f32, ((b >> shift) & 0xff) as f32);
        (x + (y - x) * t).clamp(0.0, 255.0) as u32
    };
    0xff00_0000 | ch(16) << 16 | ch(8) << 8 | ch(0)
}

/// Value noise in `0..1`, the same for the same cell.
fn hash(x: i32, y: i32, salt: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x27d4_eb2d)
        ^ (y as u32).wrapping_mul(0x1656_67b1)
        ^ salt.wrapping_mul(0x9e37_79b9);
    h ^= h >> 15;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    (h & 0xffff) as f32 / 65535.0
}

pub struct Texture(pub Vec<u32>);

impl Texture {
    fn paint(f: impl Fn(usize, usize) -> u32) -> Self {
        Self((0..TEX * TEX).map(|i| f(i % TEX, i / TEX)).collect())
    }

    pub fn at(&self, u: usize, v: usize) -> u32 {
        self.0[(v & (TEX - 1)) * TEX + (u & (TEX - 1))]
    }
}

fn plating(x: usize, y: usize) -> u32 {
    let base = rgb(92, 98, 108);
    let grain = 0.88 + hash(x as i32, y as i32, 1) * 0.16;
    let (px, py) = (x % 32, y % 32);
    let seam = px == 0 || py == 0;
    let lit = px == 1 || py == 1;
    let rivet = (px == 4 || px == 27) && (py == 4 || py == 27);
    let c = if seam {
        rgb(40, 44, 50)
    } else if rivet {
        rgb(170, 176, 184)
    } else if lit {
        scale(base, 1.25)
    } else {
        base
    };
    scale(c, grain)
}

fn block(x: usize, y: usize) -> u32 {
    let row = y / 16;
    let shift = if row % 2 == 0 { 0 } else { 12 };
    let (bx, by) = ((x + shift) % 24, y % 16);
    let stone = hash(((x + shift) / 24) as i32, row as i32, 2);
    let base = mix(rgb(84, 70, 60), rgb(116, 96, 78), stone);
    let speck = 0.85 + hash(x as i32, y as i32, 3) * 0.25;
    if bx == 0 || by == 0 {
        rgb(36, 30, 28)
    } else if by == 1 || bx == 1 {
        scale(base, 1.18 * speck)
    } else {
        scale(base, speck)
    }
}

fn pipes(x: usize, y: usize) -> u32 {
    let back = scale(rgb(54, 66, 60), 0.9 + hash(x as i32, y as i32, 4) * 0.15);
    let pipe = |centre: f32, width: f32, colour: u32| {
        let d = (x as f32 + 0.5 - centre).abs() / width;
        (d < 1.0).then(|| scale(colour, 0.55 + (1.0 - d * d).sqrt() * 0.7))
    };
    let band = y % 32 < 3;
    let c = pipe(12.0, 7.0, rgb(150, 120, 70))
        .or_else(|| pipe(34.0, 5.0, rgb(90, 130, 120)))
        .or_else(|| pipe(52.0, 6.0, rgb(150, 120, 70)));
    match (c, band) {
        (Some(c), true) => scale(c, 0.6),
        (Some(c), false) => c,
        (None, _) => back,
    }
}

fn grate(x: usize, y: usize) -> u32 {
    let (gx, gy) = (x % 8, y % 8);
    if gx < 2 || gy < 2 {
        scale(rgb(110, 112, 104), 0.9 + hash(x as i32, y as i32, 5) * 0.2)
    } else {
        let glow = 0.5 + 0.5 * ((y as f32 / 9.0).sin() * 0.5 + 0.5);
        mix(rgb(14, 18, 16), rgb(30, 70, 50), glow * 0.5)
    }
}

fn door(lock: Lock) -> impl Fn(usize, usize) -> u32 {
    let stripe = match lock {
        Lock::Open => rgb(200, 200, 205),
        Lock::Blue => rgb(60, 120, 230),
        Lock::Amber => rgb(235, 160, 40),
    };
    move |x, y| {
        let base = scale(rgb(70, 76, 86), 0.92 + hash(x as i32, y as i32, 6) * 0.12);
        if x < 3 || x >= TEX - 3 || y < 3 || y >= TEX - 3 {
            rgb(32, 34, 40)
        } else if (28..36).contains(&y) {
            if (x + y) % 12 < 6 {
                stripe
            } else {
                scale(stripe, 0.45)
            }
        } else if (x - 3) % 15 == 0 {
            scale(base, 0.7)
        } else {
            base
        }
    }
}

fn exit_panel(x: usize, y: usize) -> u32 {
    let frame = plating(x, y);
    let (sx, sy) = (x as i32 - 14, y as i32 - 12);
    if !(0..36).contains(&sx) || !(0..26).contains(&sy) {
        return frame;
    }
    if sx == 0 || sy == 0 || sx == 35 || sy == 25 {
        return rgb(20, 22, 26);
    }
    let scan = if sy % 2 == 0 { 1.0 } else { 0.75 };
    let arrow = (sy - 13).abs() <= 3 && (8..22).contains(&sx)
        || (sx >= 20 && sx <= 28 && (sy - 13).abs() <= 28 - sx);
    let c = if arrow {
        rgb(120, 240, 150)
    } else {
        rgb(20, 70, 40)
    };
    scale(c, scan)
}

fn floor(x: usize, y: usize) -> u32 {
    let tile = ((x / 16) + (y / 16)) % 2 == 0;
    let base = if tile {
        rgb(58, 60, 64)
    } else {
        rgb(50, 52, 56)
    };
    let grime = hash(x as i32 / 2, y as i32 / 2, 7);
    if x % 16 == 0 || y % 16 == 0 {
        rgb(34, 35, 38)
    } else {
        scale(base, 0.85 + grime * 0.25)
    }
}

fn ceiling(x: usize, y: usize) -> u32 {
    let (dx, dy) = (x as i32 - 32, y as i32 - 32);
    if dx.abs() < 10 && dy.abs() < 4 {
        rgb(232, 230, 206)
    } else if dx.abs() < 12 && dy.abs() < 6 {
        rgb(70, 72, 70)
    } else if x % 32 == 0 || y % 32 == 0 {
        rgb(28, 30, 34)
    } else {
        scale(rgb(44, 46, 52), 0.9 + hash(x as i32, y as i32, 8) * 0.12)
    }
}

pub struct Textures {
    pub plating: Texture,
    pub block: Texture,
    pub pipes: Texture,
    pub grate: Texture,
    pub door: Texture,
    pub door_blue: Texture,
    pub door_amber: Texture,
    pub exit: Texture,
    pub floor: Texture,
    pub ceiling: Texture,
}

impl Textures {
    pub fn new() -> Self {
        Self {
            plating: Texture::paint(plating),
            block: Texture::paint(block),
            pipes: Texture::paint(pipes),
            grate: Texture::paint(grate),
            door: Texture::paint(door(Lock::Open)),
            door_blue: Texture::paint(door(Lock::Blue)),
            door_amber: Texture::paint(door(Lock::Amber)),
            exit: Texture::paint(exit_panel),
            floor: Texture::paint(floor),
            ceiling: Texture::paint(ceiling),
        }
    }

    pub fn wall(&self, kind: WallKind) -> &Texture {
        match kind {
            WallKind::Plating => &self.plating,
            WallKind::Block => &self.block,
            WallKind::Pipes => &self.pipes,
            WallKind::Grate => &self.grate,
        }
    }

    pub fn door(&self, lock: Lock) -> &Texture {
        match lock {
            Lock::Open => &self.door,
            Lock::Blue => &self.door_blue,
            Lock::Amber => &self.door_amber,
        }
    }
}

impl Default for Textures {
    fn default() -> Self {
        Self::new()
    }
}

/// A square picture, its base on the floor.
pub struct Sprite(pub Vec<u32>);

struct Pen(Vec<u32>);

impl Pen {
    fn new() -> Self {
        Self(vec![0; SPRITE * SPRITE])
    }

    fn put(&mut self, x: i32, y: i32, c: u32) {
        if (0..SPRITE as i32).contains(&x) && (0..SPRITE as i32).contains(&y) {
            self.0[y as usize * SPRITE + x as usize] = c;
        }
    }

    /// Lit from the top left, as everything in the game is.
    fn ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32, c: u32) {
        for y in (cy - ry).floor() as i32..=(cy + ry).ceil() as i32 {
            for x in (cx - rx).floor() as i32..=(cx + rx).ceil() as i32 {
                let (dx, dy) = ((x as f32 + 0.5 - cx) / rx, (y as f32 + 0.5 - cy) / ry);
                let d = dx * dx + dy * dy;
                if d <= 1.0 {
                    let shade = 1.15 - (dx + dy) * 0.18 - d * 0.25;
                    self.put(x, y, scale(c, shade));
                }
            }
        }
    }

    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: u32) {
        for y in y0..y1 {
            for x in x0..x1 {
                let edge = x == x0 || y == y0;
                self.put(x, y, if edge { scale(c, 1.25) } else { c });
            }
        }
    }

    fn line(&mut self, (x0, y0): (f32, f32), (x1, y1): (f32, f32), width: f32, c: u32) {
        let steps = ((x1 - x0).abs().max((y1 - y0).abs()) * 2.0).ceil() as i32;
        for i in 0..=steps {
            let t = i as f32 / steps.max(1) as f32;
            self.ellipse(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t, width, width, c);
        }
    }
}

/// Which frame of a walk, an attack or a fall an enemy shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pose {
    Stand,
    StepA,
    StepB,
    Strike,
    Pain,
    Wreck,
}

impl Pose {
    pub fn of(mode: Mode, stride: f32) -> Self {
        match mode {
            Mode::Idle => Pose::Stand,
            Mode::Chase if (stride * 2.5) as i32 % 2 == 0 => Pose::StepA,
            Mode::Chase => Pose::StepB,
            Mode::Attack => Pose::Strike,
            Mode::Pain => Pose::Pain,
            Mode::Dead => Pose::Wreck,
        }
    }

    const ALL: [Pose; 6] = [
        Pose::Stand,
        Pose::StepA,
        Pose::StepB,
        Pose::Strike,
        Pose::Pain,
        Pose::Wreck,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|&p| p == self).unwrap_or(0)
    }
}

/// A six-legged maintenance drone, low to the floor.
fn crawler(pose: Pose) -> Sprite {
    let mut p = Pen::new();
    let shell = rgb(150, 160, 70);
    let leg = rgb(60, 62, 58);
    if pose == Pose::Wreck {
        p.ellipse(32.0, 59.0, 20.0, 4.0, scale(shell, 0.45));
        p.line((16.0, 60.0), (8.0, 63.0), 1.5, leg);
        p.line((46.0, 59.0), (56.0, 62.0), 1.5, leg);
        return Sprite(p.0);
    }
    let lift = match pose {
        Pose::StepA => -1.0,
        Pose::StepB => 1.0,
        _ => 0.0,
    };
    let legs = [-1.0f32, 1.0]
        .iter()
        .flat_map(|&s| (0..3usize).map(move |k| (s, k)));
    for (i, (s, k)) in legs.enumerate() {
        let knee_x = 32.0 + s * (14.0 + k as f32 * 4.0);
        let up = if (i + k) % 2 == 0 { lift } else { -lift };
        let knee = (knee_x, 42.0 + up * 2.0);
        let foot = (32.0 + s * (20.0 + k as f32 * 6.0), 62.0);
        p.line((32.0 + s * 6.0, 50.0), knee, 1.4, leg);
        p.line(knee, foot, 1.2, leg);
    }
    let tint = if pose == Pose::Pain {
        scale(shell, 1.6)
    } else {
        shell
    };
    p.ellipse(32.0, 49.0, 15.0, 9.0, tint);
    p.ellipse(32.0, 45.0, 9.0, 4.0, scale(tint, 1.2));
    let eye = if pose == Pose::Strike {
        rgb(255, 120, 60)
    } else {
        rgb(255, 60, 40)
    };
    p.ellipse(26.0, 52.0, 2.2, 2.2, eye);
    p.ellipse(38.0, 52.0, 2.2, 2.2, eye);
    if pose == Pose::Strike {
        p.line((24.0, 56.0), (20.0, 62.0), 1.6, rgb(220, 220, 230));
        p.line((40.0, 56.0), (44.0, 62.0), 1.6, rgb(220, 220, 230));
    }
    Sprite(p.0)
}

/// A turret on a tripod with one wide lens.
fn sentry(pose: Pose) -> Sprite {
    let mut p = Pen::new();
    let body = rgb(170, 90, 60);
    let metal = rgb(80, 84, 90);
    p.line((32.0, 44.0), (20.0, 63.0), 1.6, metal);
    p.line((32.0, 44.0), (44.0, 63.0), 1.6, metal);
    p.line((32.0, 44.0), (32.0, 63.0), 1.4, scale(metal, 0.8));
    if pose == Pose::Wreck {
        p.ellipse(30.0, 58.0, 12.0, 5.0, scale(body, 0.4));
        p.ellipse(40.0, 60.0, 4.0, 3.0, scale(metal, 0.6));
        return Sprite(p.0);
    }
    let tint = if pose == Pose::Pain {
        scale(body, 1.6)
    } else {
        body
    };
    p.rect(18, 24, 46, 44, tint);
    p.rect(44, 30, 56, 36, metal);
    p.ellipse(30.0, 34.0, 7.0, 7.0, rgb(30, 30, 36));
    let lens = if pose == Pose::Strike {
        rgb(255, 230, 120)
    } else {
        rgb(90, 200, 255)
    };
    p.ellipse(30.0, 34.0, 4.0, 4.0, lens);
    if pose == Pose::Strike {
        p.ellipse(59.0, 33.0, 4.0, 4.0, rgb(255, 200, 80));
    }
    Sprite(p.0)
}

/// The station's security frame: tall, two arms, a slit visor.
fn warden(pose: Pose) -> Sprite {
    let mut p = Pen::new();
    let armour = rgb(110, 70, 140);
    let joint = rgb(50, 44, 60);
    if pose == Pose::Wreck {
        p.ellipse(32.0, 58.0, 24.0, 6.0, scale(armour, 0.4));
        p.ellipse(16.0, 57.0, 7.0, 5.0, scale(joint, 0.8));
        p.ellipse(48.0, 60.0, 6.0, 3.0, scale(armour, 0.5));
        return Sprite(p.0);
    }
    let swing = match pose {
        Pose::StepA => 3.0,
        Pose::StepB => -3.0,
        _ => 0.0,
    };
    p.line((26.0, 40.0), (24.0 + swing, 63.0), 3.0, joint);
    p.line((38.0, 40.0), (40.0 - swing, 63.0), 3.0, joint);
    let tint = if pose == Pose::Pain {
        scale(armour, 1.6)
    } else {
        armour
    };
    p.ellipse(32.0, 30.0, 14.0, 13.0, tint);
    p.rect(22, 38, 42, 44, scale(tint, 0.8));
    p.ellipse(32.0, 12.0, 8.0, 7.0, scale(tint, 1.1));
    p.rect(26, 11, 38, 14, rgb(255, 80, 200));
    let reach = if pose == Pose::Strike { 10.0 } else { 0.0 };
    p.line((18.0, 24.0), (8.0, 36.0 - reach), 3.0, joint);
    p.line((46.0, 24.0), (56.0, 36.0 - reach), 3.0, joint);
    p.ellipse(8.0, 37.0 - reach, 4.0, 4.0, scale(tint, 0.9));
    p.ellipse(56.0, 37.0 - reach, 4.0, 4.0, scale(tint, 0.9));
    if pose == Pose::Strike {
        p.ellipse(56.0, 28.0 - reach, 4.0, 4.0, rgb(255, 160, 240));
    }
    Sprite(p.0)
}

fn item(kind: ItemKind) -> Sprite {
    let mut p = Pen::new();
    match kind {
        ItemKind::RepairKit => {
            p.rect(18, 46, 46, 63, rgb(220, 222, 226));
            p.rect(29, 49, 35, 61, rgb(40, 170, 150));
            p.rect(24, 52, 40, 58, rgb(40, 170, 150));
        }
        ItemKind::Rivets => {
            p.rect(20, 50, 44, 63, rgb(120, 96, 50));
            for (x, y) in [
                (25.0, 51.0),
                (31.0, 49.5),
                (37.0, 51.0),
                (28.0, 47.5),
                (34.0, 47.5),
            ] {
                p.ellipse(x, y, 3.0, 2.0, rgb(220, 190, 90));
            }
        }
        ItemKind::BlueCard | ItemKind::AmberCard => {
            let c = if kind == ItemKind::BlueCard {
                rgb(60, 120, 230)
            } else {
                rgb(235, 160, 40)
            };
            p.rect(22, 48, 42, 62, c);
            p.rect(25, 51, 33, 54, rgb(240, 240, 240));
            p.rect(25, 57, 39, 59, scale(c, 0.6));
        }
    }
    Sprite(p.0)
}

pub struct Sprites {
    enemies: Vec<Sprite>,
    items: Vec<Sprite>,
}

const ENEMIES: [EnemyKind; 3] = [EnemyKind::Crawler, EnemyKind::Sentry, EnemyKind::Warden];
const ITEMS: [ItemKind; 4] = [
    ItemKind::RepairKit,
    ItemKind::Rivets,
    ItemKind::BlueCard,
    ItemKind::AmberCard,
];

impl Sprites {
    pub fn new() -> Self {
        let enemies = ENEMIES
            .iter()
            .flat_map(|&kind| {
                Pose::ALL.iter().map(move |&pose| match kind {
                    EnemyKind::Crawler => crawler(pose),
                    EnemyKind::Sentry => sentry(pose),
                    EnemyKind::Warden => warden(pose),
                })
            })
            .collect();
        Self {
            enemies,
            items: ITEMS.iter().map(|&k| item(k)).collect(),
        }
    }

    pub fn enemy(&self, kind: EnemyKind, pose: Pose) -> &Sprite {
        let k = ENEMIES.iter().position(|&e| e == kind).unwrap_or(0);
        &self.enemies[k * Pose::ALL.len() + pose.index()]
    }

    pub fn item(&self, kind: ItemKind) -> &Sprite {
        &self.items[ITEMS.iter().position(|&i| i == kind).unwrap_or(0)]
    }
}

impl Default for Sprites {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sprite_shows_something_and_stands_on_the_floor() {
        let sprites = Sprites::new();
        let all = ENEMIES
            .iter()
            .flat_map(|&k| Pose::ALL.iter().map(move |&p| (k, p)))
            .map(|(k, p)| sprites.enemy(k, p))
            .chain(ITEMS.iter().map(|&k| sprites.item(k)));
        for sprite in all {
            assert!(sprite.0.iter().filter(|&&c| c != 0).count() > 40);
            let bottom = &sprite.0[(SPRITE - 4) * SPRITE..];
            assert!(bottom.iter().any(|&c| c != 0), "floats above the floor");
        }
    }

    #[test]
    fn poses_of_one_enemy_differ() {
        let sprites = Sprites::new();
        for kind in ENEMIES {
            let stand = &sprites.enemy(kind, Pose::Stand).0;
            for pose in [Pose::Strike, Pose::Pain, Pose::Wreck] {
                assert_ne!(stand, &sprites.enemy(kind, pose).0, "{kind:?} {pose:?}");
            }
        }
    }

    #[test]
    fn textures_are_opaque() {
        let t = Textures::new();
        for tex in [
            &t.plating, &t.block, &t.pipes, &t.grate, &t.door, &t.exit, &t.floor,
        ] {
            assert!(tex.0.iter().all(|&c| c >> 24 == 0xff));
        }
    }
}
