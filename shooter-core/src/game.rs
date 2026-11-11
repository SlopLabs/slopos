//! The simulation: the player, the doors, the enemies and what they do to
//! each other, advanced by [`Game::step`].

use std::f32::consts::PI;

use crate::level::{self, Cell, EnemyKind, ItemKind, Level, Lock, Thing};
use crate::rng::Rng;

const RADIUS: f32 = 0.25;
const WALK: f32 = 3.2;
const TURN: f32 = 2.6;
const REACH: f32 = 1.1;
const DOOR_SPEED: f32 = 1.8;
const DOOR_HOLD: f32 = 3.5;
const FIRE_DELAY: f32 = 0.32;
const HEARING: f32 = 9.0;
const SIGHT: f32 = 14.0;
pub const MAX_HEALTH: i32 = 100;
pub const MAX_RIVETS: i32 = 99;
const MESSAGE_SECONDS: f32 = 2.5;

/// What the person holds down this step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Input {
    pub forward: bool,
    pub back: bool,
    pub turn_left: bool,
    pub turn_right: bool,
    pub strafe_left: bool,
    pub strafe_right: bool,
    pub fire: bool,
    /// Opens a door or works the exit panel in front; acts on the press.
    pub use_: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Playing,
    Dead,
    /// The exit panel was used; [`Game::next_level`] goes on.
    Cleared,
    /// The last level was cleared.
    Won,
}

#[derive(Clone, Debug)]
pub struct Player {
    pub x: f32,
    pub y: f32,
    pub angle: f32,
    pub health: i32,
    pub rivets: i32,
    pub blue: bool,
    pub amber: bool,
    cooldown: f32,
    /// Seconds the muzzle flash still shows.
    pub flash: f32,
    /// Seconds the hurt tint still shows.
    pub hurt: f32,
    /// Distance walked, which bobs the gun.
    pub stride: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Idle,
    Chase,
    Attack,
    Pain,
    Dead,
}

#[derive(Clone, Debug)]
pub struct Enemy {
    pub kind: EnemyKind,
    pub x: f32,
    pub y: f32,
    pub health: i32,
    pub mode: Mode,
    timer: f32,
    cooldown: f32,
    /// Distance walked, which picks its stride's frame.
    pub stride: f32,
}

impl Enemy {
    pub fn alive(&self) -> bool {
        self.mode != Mode::Dead
    }
}

struct Stats {
    health: i32,
    speed: f32,
    reach: f32,
    damage: (i32, i32),
    delay: f32,
    ranged: bool,
}

fn stats(kind: EnemyKind) -> Stats {
    match kind {
        EnemyKind::Crawler => Stats {
            health: 24,
            speed: 1.9,
            reach: 0.8,
            damage: (6, 11),
            delay: 0.9,
            ranged: false,
        },
        EnemyKind::Sentry => Stats {
            health: 36,
            speed: 0.0,
            reach: SIGHT,
            damage: (5, 9),
            delay: 1.6,
            ranged: true,
        },
        EnemyKind::Warden => Stats {
            health: 160,
            speed: 1.3,
            reach: SIGHT,
            damage: (9, 15),
            delay: 1.0,
            ranged: true,
        },
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub kind: ItemKind,
    pub x: f32,
    pub y: f32,
    pub taken: bool,
}

#[derive(Clone, Debug)]
pub struct Door {
    pub x: i32,
    pub y: i32,
    pub lock: Lock,
    /// 0 closed, 1 open.
    pub open: f32,
    opening: bool,
    hold: f32,
}

pub struct Game {
    pub level_index: usize,
    pub level: Level,
    pub player: Player,
    pub enemies: Vec<Enemy>,
    pub items: Vec<Item>,
    pub doors: Vec<Door>,
    pub outcome: Outcome,
    pub kills: usize,
    pub time: f32,
    message: Option<(String, f32)>,
    rng: Rng,
    was_using: bool,
}

impl Default for Game {
    fn default() -> Self {
        Self::new(0x5eed_b00c)
    }
}

impl Game {
    pub fn new(seed: u32) -> Self {
        Self::at_level(0, level::load(0), seed, None)
    }

    pub fn with_level(level: Level, seed: u32) -> Self {
        Self::at_level(0, level, seed, None)
    }

    fn at_level(index: usize, level: Level, seed: u32, carried: Option<&Player>) -> Self {
        let (x, y, angle) = level.start;
        let player = Player {
            x,
            y,
            angle,
            health: carried.map_or(MAX_HEALTH, |p| p.health),
            rivets: carried.map_or(30, |p| p.rivets),
            blue: false,
            amber: false,
            cooldown: 0.0,
            flash: 0.0,
            hurt: 0.0,
            stride: 0.0,
        };
        let mut enemies = Vec::new();
        let mut items = Vec::new();
        for &(thing, x, y) in &level.things {
            match thing {
                Thing::Enemy(kind) => enemies.push(Enemy {
                    kind,
                    x,
                    y,
                    health: stats(kind).health,
                    mode: Mode::Idle,
                    timer: 0.0,
                    cooldown: 0.0,
                    stride: 0.0,
                }),
                Thing::Item(kind) => items.push(Item {
                    kind,
                    x,
                    y,
                    taken: false,
                }),
            }
        }
        let mut doors = Vec::new();
        for y in 0..level.height as i32 {
            for x in 0..level.width as i32 {
                if let Cell::Door(lock) = level.cell(x, y) {
                    doors.push(Door {
                        x,
                        y,
                        lock,
                        open: 0.0,
                        opening: false,
                        hold: 0.0,
                    });
                }
            }
        }
        Self {
            level_index: index,
            level,
            player,
            enemies,
            items,
            doors,
            outcome: Outcome::Playing,
            kills: 0,
            time: 0.0,
            message: None,
            rng: Rng::new(seed ^ (index as u32 + 1).wrapping_mul(0x9e37_79b9)),
            was_using: false,
        }
    }

    pub fn level_count() -> usize {
        level::LEVELS.len()
    }

    /// After [`Outcome::Cleared`]: the next level, health and rivets kept.
    pub fn next_level(&mut self) {
        let index = self.level_index + 1;
        if index < Self::level_count() {
            let seed = self.rng.next();
            *self = Self::at_level(index, level::load(index), seed, Some(&self.player));
        }
    }

    /// After [`Outcome::Dead`]: this level again, from full health.
    pub fn restart_level(&mut self) {
        let seed = self.rng.next();
        let index = self.level_index;
        *self = Self::at_level(index, level::load(index), seed, None);
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_ref().map(|(text, _)| text.as_str())
    }

    fn say(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), MESSAGE_SECONDS));
    }

    pub fn enemies_left(&self) -> usize {
        self.enemies.iter().filter(|e| e.alive()).count()
    }

    pub fn door_at(&self, x: i32, y: i32) -> Option<&Door> {
        self.doors.iter().find(|d| d.x == x && d.y == y)
    }

    /// Whether a body may stand in the cell.
    fn passable(&self, x: i32, y: i32) -> bool {
        match self.level.cell(x, y) {
            Cell::Floor => true,
            Cell::Door(_) => self.door_at(x, y).is_some_and(|d| d.open > 0.9),
            Cell::Wall(_) | Cell::Exit => false,
        }
    }

    /// Whether a ray passes through the cell, as sight and shots do.
    pub fn clear(&self, x: i32, y: i32) -> bool {
        match self.level.cell(x, y) {
            Cell::Floor => true,
            Cell::Door(_) => self.door_at(x, y).is_some_and(|d| d.open > 0.6),
            Cell::Wall(_) | Cell::Exit => false,
        }
    }

    fn fits(&self, x: f32, y: f32) -> bool {
        [
            (-RADIUS, -RADIUS),
            (RADIUS, -RADIUS),
            (-RADIUS, RADIUS),
            (RADIUS, RADIUS),
        ]
        .iter()
        .all(|(dx, dy)| self.passable((x + dx).floor() as i32, (y + dy).floor() as i32))
    }

    /// Moves along each axis alone, so a body slides along a wall.
    fn slide(&self, x: f32, y: f32, dx: f32, dy: f32) -> (f32, f32) {
        let nx = if self.fits(x + dx, y) { x + dx } else { x };
        let ny = if self.fits(nx, y + dy) { y + dy } else { y };
        (nx, ny)
    }

    /// Walks the cells between two points, as a ray does.
    pub fn sight(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> bool {
        let (dx, dy) = (x1 - x0, y1 - y0);
        let distance = (dx * dx + dy * dy).sqrt();
        let steps = (distance * 4.0).ceil() as i32;
        (1..steps).all(|i| {
            let t = i as f32 / steps as f32;
            self.clear((x0 + dx * t).floor() as i32, (y0 + dy * t).floor() as i32)
        })
    }

    pub fn step(&mut self, input: &Input, dt: f32) {
        let dt = dt.clamp(0.0, 0.1);
        if self.outcome != Outcome::Playing {
            return;
        }
        self.time += dt;
        if let Some((_, left)) = &mut self.message {
            *left -= dt;
            if *left <= 0.0 {
                self.message = None;
            }
        }
        self.move_player(input, dt);
        if input.use_ && !self.was_using {
            self.use_ahead();
        }
        self.was_using = input.use_;
        self.fire(input, dt);
        self.take_items();
        self.run_doors(dt);
        self.run_enemies(dt);
        let p = &mut self.player;
        p.flash = (p.flash - dt).max(0.0);
        p.hurt = (p.hurt - dt).max(0.0);
        if p.health <= 0 {
            p.health = 0;
            self.outcome = Outcome::Dead;
        }
    }

    fn move_player(&mut self, input: &Input, dt: f32) {
        let turn = (input.turn_right as i32 - input.turn_left as i32) as f32;
        let p = &mut self.player;
        p.angle = (p.angle + turn * TURN * dt).rem_euclid(2.0 * PI);
        let ahead = (input.forward as i32 - input.back as i32) as f32;
        let side = (input.strafe_right as i32 - input.strafe_left as i32) as f32;
        let (sin, cos) = p.angle.sin_cos();
        let (mut dx, mut dy) = (cos * ahead - sin * side, sin * ahead + cos * side);
        let length = (dx * dx + dy * dy).sqrt();
        if length > 0.0 {
            dx *= WALK * dt / length;
            dy *= WALK * dt / length;
            let (x, y) = (p.x, p.y);
            let (nx, ny) = self.slide(x, y, dx, dy);
            let p = &mut self.player;
            p.stride += ((nx - x).powi(2) + (ny - y).powi(2)).sqrt();
            p.x = nx;
            p.y = ny;
        }
    }

    fn use_ahead(&mut self) {
        let p = &self.player;
        let (sin, cos) = p.angle.sin_cos();
        let (cx, cy) = (
            (p.x + cos * REACH).floor() as i32,
            (p.y + sin * REACH).floor() as i32,
        );
        match self.level.cell(cx, cy) {
            Cell::Exit => {
                self.outcome = if self.level_index + 1 < Self::level_count() {
                    Outcome::Cleared
                } else {
                    Outcome::Won
                };
            }
            Cell::Door(lock) => {
                let (blue, amber) = (self.player.blue, self.player.amber);
                let refused = match lock {
                    Lock::Open => None,
                    Lock::Blue if !blue => Some("This door needs the blue keycard."),
                    Lock::Amber if !amber => Some("This door needs the amber keycard."),
                    Lock::Blue | Lock::Amber => None,
                };
                match refused {
                    Some(why) => self.say(why),
                    None => self.open_door(cx, cy),
                }
            }
            Cell::Floor | Cell::Wall(_) => {}
        }
    }

    fn open_door(&mut self, x: i32, y: i32) {
        if let Some(door) = self.doors.iter_mut().find(|d| d.x == x && d.y == y) {
            door.opening = true;
            door.hold = DOOR_HOLD;
        }
    }

    fn fire(&mut self, input: &Input, dt: f32) {
        let p = &mut self.player;
        p.cooldown = (p.cooldown - dt).max(0.0);
        if !input.fire || p.cooldown > 0.0 {
            return;
        }
        p.cooldown = FIRE_DELAY;
        if p.rivets == 0 {
            self.say("Out of rivets.");
            return;
        }
        p.rivets -= 1;
        p.flash = 0.08;
        let (px, py, angle) = (p.x, p.y, p.angle);
        let (sin, cos) = angle.sin_cos();
        let mut target: Option<(usize, f32)> = None;
        for (i, e) in self.enemies.iter().enumerate() {
            if !e.alive() {
                continue;
            }
            let (ex, ey) = (e.x - px, e.y - py);
            let along = ex * cos + ey * sin;
            let across = (ey * cos - ex * sin).abs();
            if along <= 0.0 || across > 0.4 {
                continue;
            }
            if target.is_some_and(|(_, d)| d < along) || !self.sight(px, py, e.x, e.y) {
                continue;
            }
            target = Some((i, along));
        }
        for e in &mut self.enemies {
            let distance = ((e.x - px).powi(2) + (e.y - py).powi(2)).sqrt();
            if e.mode == Mode::Idle && distance < HEARING {
                e.mode = Mode::Chase;
            }
        }
        if let Some((i, distance)) = target {
            let damage = self.rng.range(9, 15) - (distance / 4.0) as i32;
            self.hurt_enemy(i, damage.max(4));
        }
    }

    fn hurt_enemy(&mut self, i: usize, damage: i32) {
        let e = &mut self.enemies[i];
        e.health -= damage;
        if e.health <= 0 {
            e.mode = Mode::Dead;
            self.kills += 1;
        } else if e.kind != EnemyKind::Warden || self.rng.unit() < 0.3 {
            e.mode = Mode::Pain;
            e.timer = 0.25;
        } else {
            e.mode = Mode::Chase;
        }
    }

    fn take_items(&mut self) {
        let (px, py) = (self.player.x, self.player.y);
        let mut said = None;
        for item in &mut self.items {
            if item.taken || (item.x - px).powi(2) + (item.y - py).powi(2) > 0.36 {
                continue;
            }
            let p = &mut self.player;
            said = match item.kind {
                ItemKind::RepairKit if p.health < MAX_HEALTH => {
                    p.health = (p.health + 25).min(MAX_HEALTH);
                    Some("Picked up a repair kit.")
                }
                ItemKind::Rivets if p.rivets < MAX_RIVETS => {
                    p.rivets = (p.rivets + 15).min(MAX_RIVETS);
                    Some("Picked up a box of rivets.")
                }
                ItemKind::BlueCard => {
                    p.blue = true;
                    Some("Picked up the blue keycard.")
                }
                ItemKind::AmberCard => {
                    p.amber = true;
                    Some("Picked up the amber keycard.")
                }
                ItemKind::RepairKit | ItemKind::Rivets => continue,
            };
            item.taken = true;
        }
        if let Some(text) = said {
            self.say(text);
        }
    }

    fn occupied(&self, x: i32, y: i32) -> bool {
        let inside = |bx: f32, by: f32| {
            (bx - RADIUS).floor() as i32 <= x
                && (bx + RADIUS).floor() as i32 >= x
                && (by - RADIUS).floor() as i32 <= y
                && (by + RADIUS).floor() as i32 >= y
        };
        inside(self.player.x, self.player.y)
            || self.enemies.iter().any(|e| e.alive() && inside(e.x, e.y))
    }

    fn run_doors(&mut self, dt: f32) {
        for i in 0..self.doors.len() {
            let (x, y) = (self.doors[i].x, self.doors[i].y);
            let blocked = self.occupied(x, y);
            let door = &mut self.doors[i];
            if door.opening {
                door.open = (door.open + DOOR_SPEED * dt).min(1.0);
                if door.open >= 1.0 {
                    door.opening = false;
                }
            } else if door.open > 0.0 {
                door.hold -= dt;
                if door.hold <= 0.0 && !blocked {
                    door.open = (door.open - DOOR_SPEED * dt).max(0.0);
                }
            }
        }
    }

    fn run_enemies(&mut self, dt: f32) {
        for i in 0..self.enemies.len() {
            self.run_enemy(i, dt);
        }
    }

    fn run_enemy(&mut self, i: usize, dt: f32) {
        let (px, py) = (self.player.x, self.player.y);
        let e = &self.enemies[i];
        if !e.alive() {
            return;
        }
        let st = stats(e.kind);
        let (ex, ey) = (e.x, e.y);
        let distance = ((px - ex).powi(2) + (py - ey).powi(2)).sqrt();
        let sees = distance < SIGHT && self.sight(ex, ey, px, py);
        let e = &mut self.enemies[i];
        e.cooldown = (e.cooldown - dt).max(0.0);
        e.timer = (e.timer - dt).max(0.0);
        match e.mode {
            Mode::Idle => {
                if sees {
                    e.mode = Mode::Chase;
                    e.cooldown = st.delay * 0.5;
                }
                return;
            }
            Mode::Pain | Mode::Attack if e.timer > 0.0 => return,
            Mode::Pain | Mode::Attack => e.mode = Mode::Chase,
            Mode::Chase | Mode::Dead => {}
        }
        let in_reach = distance <= st.reach && (sees || !st.ranged);
        if in_reach && e.cooldown <= 0.0 && sees {
            e.mode = Mode::Attack;
            e.timer = 0.3;
            e.cooldown = st.delay;
            let lands = if st.ranged {
                let chance = (1.0 - distance / 18.0).clamp(0.3, 0.9);
                self.rng.unit() < chance
            } else {
                true
            };
            if lands {
                let damage = self.rng.range(st.damage.0, st.damage.1);
                let p = &mut self.player;
                p.health -= damage;
                p.hurt = 0.3;
            }
            return;
        }
        let keep_away = if st.ranged && sees { 3.0 } else { 0.7 };
        if st.speed <= 0.0 || distance <= keep_away {
            return;
        }
        let (dx, dy) = ((px - ex) / distance, (py - ey) / distance);
        let ahead = (
            (ex + dx * 0.6).floor() as i32,
            (ey + dy * 0.6).floor() as i32,
        );
        if let Cell::Door(Lock::Open) = self.level.cell(ahead.0, ahead.1) {
            self.open_door(ahead.0, ahead.1);
        }
        let step = st.speed * dt;
        let (nx, ny) = self.slide(ex, ey, dx * step, dy * step);
        let e = &mut self.enemies[i];
        e.stride += ((nx - ex).powi(2) + (ny - ey).powi(2)).sqrt();
        e.x = nx;
        e.y = ny;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::level::parse;

    fn game(rows: &[&'static str]) -> Game {
        let rows: &'static [&'static str] = Box::leak(rows.to_vec().into_boxed_slice());
        Game::with_level(parse("test", rows).unwrap(), 7)
    }

    fn held(f: impl Fn(&mut Input)) -> Input {
        let mut input = Input::default();
        f(&mut input);
        input
    }

    fn run(game: &mut Game, input: Input, seconds: f32) {
        for _ in 0..(seconds * 60.0) as usize {
            game.step(&input, 1.0 / 60.0);
        }
    }

    #[test]
    fn walking_forward_stops_at_a_wall() {
        let mut g = game(&["#######", "#>....#", "#######"]);
        run(&mut g, held(|i| i.forward = true), 3.0);
        assert!(
            g.player.x < 6.0 - RADIUS + 0.001 && g.player.x > 5.5,
            "{}",
            g.player.x
        );
        assert!((g.player.y - 1.5).abs() < 1e-4);
    }

    #[test]
    fn a_closed_door_blocks_until_used_and_then_lets_the_player_through() {
        let mut g = game(&["#######", "#>.|..#", "#######"]);
        run(&mut g, held(|i| i.forward = true), 1.5);
        assert!(g.player.x < 3.0);
        run(&mut g, held(|i| i.use_ = true), 0.1);
        run(&mut g, Input::default(), 1.0);
        run(&mut g, held(|i| i.forward = true), 1.5);
        assert!(g.player.x > 4.0, "{}", g.player.x);
    }

    #[test]
    fn a_locked_door_needs_its_card() {
        let mut g = game(&["########", "#>.B...#", "#.b....#", "########"]);
        run(&mut g, held(|i| i.forward = true), 0.5);
        run(&mut g, held(|i| i.use_ = true), 0.1);
        assert_eq!(g.message(), Some("This door needs the blue keycard."));
        assert_eq!(g.door_at(3, 1).unwrap().open, 0.0);
        g.player.x = 2.5;
        g.player.y = 2.5;
        run(&mut g, Input::default(), 0.1);
        assert!(g.player.blue);
        g.player.y = 1.5;
        run(&mut g, Input::default(), 0.1);
        run(&mut g, held(|i| i.use_ = true), 0.1);
        run(&mut g, Input::default(), 1.0);
        assert_eq!(g.door_at(3, 1).unwrap().open, 1.0);
    }

    #[test]
    fn shooting_a_crawler_kills_it_and_spends_rivets() {
        let mut g = game(&["##########", "#>.....c.#", "##########"]);
        let before = g.player.rivets;
        run(&mut g, held(|i| i.fire = true), 3.0);
        assert_eq!(g.enemies_left(), 0);
        assert_eq!(g.kills, 1);
        assert!(g.player.rivets < before);
    }

    #[test]
    fn a_wall_stops_a_shot() {
        let mut g = game(&["##########", "#>..#..c.#", "##########"]);
        let health = g.enemies[0].health;
        run(&mut g, held(|i| i.fire = true), 1.0);
        assert_eq!(g.enemies[0].health, health);
    }

    #[test]
    fn a_crawler_that_sees_the_player_closes_in_and_hurts_them() {
        let mut g = game(&["############", "#>........c#", "############"]);
        run(&mut g, Input::default(), 6.0);
        assert!(g.enemies[0].x < 3.0, "{}", g.enemies[0].x);
        assert!(g.player.health < MAX_HEALTH);
    }

    #[test]
    fn an_enemy_behind_a_wall_stays_idle() {
        let mut g = game(&["##########", "#>..#..c.#", "##########"]);
        run(&mut g, Input::default(), 2.0);
        assert_eq!(g.enemies[0].mode, Mode::Idle);
    }

    #[test]
    fn dying_ends_play_and_a_restart_brings_back_full_health() {
        let mut g = game(&["######", "#>c..#", "######"]);
        g.player.health = 1;
        run(&mut g, Input::default(), 3.0);
        assert_eq!(g.outcome, Outcome::Dead);
        let mut g = Game::new(1);
        g.player.health = 3;
        g.restart_level();
        assert_eq!(g.player.health, MAX_HEALTH);
        assert_eq!(g.outcome, Outcome::Playing);
    }

    #[test]
    fn pickups_are_taken_once_and_capped() {
        let mut g = game(&["######", "#>rh.#", "######"]);
        g.player.health = 90;
        run(&mut g, held(|i| i.forward = true), 1.5);
        assert_eq!(g.player.health, MAX_HEALTH);
        assert_eq!(g.player.rivets, 45);
        assert!(g.items.iter().all(|i| i.taken));
    }

    #[test]
    fn the_exit_panel_clears_the_level_and_the_next_keeps_health_and_rivets() {
        let mut g = Game::new(3);
        let level = parse("test", &["#####", "#.>E#", "#####"]).unwrap();
        g.level = level.clone();
        g.player.x = level.start.0;
        g.player.y = level.start.1;
        g.player.angle = 0.0;
        g.player.health = 55;
        run(&mut g, held(|i| i.use_ = true), 0.1);
        assert_eq!(g.outcome, Outcome::Cleared);
        g.next_level();
        assert_eq!(g.level_index, 1);
        assert_eq!(g.player.health, 55);
        assert_eq!(g.outcome, Outcome::Playing);
    }

    #[test]
    fn the_last_exit_wins() {
        let mut g = Game::new(3);
        g.level_index = Game::level_count() - 1;
        g.level = parse("test", &["#####", "#.>E#", "#####"]).unwrap();
        g.player.x = 2.5;
        g.player.y = 1.5;
        g.player.angle = 0.0;
        run(&mut g, held(|i| i.use_ = true), 0.1);
        assert_eq!(g.outcome, Outcome::Won);
    }
}
