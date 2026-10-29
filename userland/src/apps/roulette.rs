//! `/bin/roulette`: the Wheel of Fate.
//!
//! The kernel picks the fate (`roulette`) and acts on it (`roulette_result`);
//! this program only shows it. It draws the wheel into a memfd the size of the
//! screen and presents each frame's damage with `fb_flip` from the virtcon
//! seat, which outranks the compositor's, pacing frames by the clock: a slow
//! machine shows fewer frames of the same seven seconds.

use std::time::{Duration, Instant};

use slopos_fate_core::layout;
use slopos_fate_core::scene::{self, Damage, Fonts, Scene, Screen};
use slopos_fate_core::spin::{self, Phase};

use crate::gfx::DamageRect;
use crate::syscall::{DisplayInfo, ShmBuffer, roulette, tty, window};

macro_rules! log {
    ($($arg:tt)*) => {{
        let line = std::format!("ROULETTE: {}\n", format_args!($($arg)*));
        let _ = tty::write(line.as_bytes());
    }};
}

const FRAME: Duration = Duration::from_millis(16);
/// A present refused for longer than this is the kernel log on screen, not a
/// GPU still busy with the last frame: the spin waits for the person reading
/// it rather than finishing behind it.
const HELD: Duration = Duration::from_millis(120);
const RETRY: Duration = Duration::from_millis(50);

/// The most a frame may move the spin on: a process kept off the CPU — by the
/// kernel log redrawing, say — resumes where it was rather than skipping
/// ahead past what nobody saw.
const MAX_STEP: Duration = Duration::from_millis(100);

/// Milliseconds of spin shown, which stand still while the screen is held.
struct Clock {
    shown: Duration,
    last: Instant,
    refused_since: Option<Instant>,
}

impl Clock {
    fn new() -> Self {
        Self {
            shown: Duration::ZERO,
            last: Instant::now(),
            refused_since: None,
        }
    }

    /// Advance to now and answer the spin's time.
    fn tick(&mut self) -> u32 {
        let now = Instant::now();
        let step = (now - self.last).min(MAX_STEP);
        self.last = now;
        if !self.holding() {
            self.shown += step;
        }
        self.shown.as_millis().min(u32::MAX as u128) as u32
    }

    fn shown(&mut self) {
        self.refused_since = None;
    }

    fn refused(&mut self) {
        self.refused_since.get_or_insert_with(Instant::now);
    }

    fn holding(&self) -> bool {
        self.refused_since
            .is_some_and(|since| since.elapsed() > HELD)
    }
}

#[derive(Default)]
struct Stats {
    frames: u32,
    shown: u32,
    refused: u32,
    paint_total: Duration,
    paint_max: Duration,
}

enum Outcome {
    Shown(Stats),
    /// The screen went away mid-spin; the fate stands regardless.
    Lost(&'static str),
}

fn spin_on_screen(fate: u32, info: &DisplayInfo, started: Instant) -> Outcome {
    let size = info.buffer_size();
    let Ok(mut buffer) = ShmBuffer::create(size) else {
        return Outcome::Lost("no memory for the frame");
    };
    let fd = buffer.fd() as u32;
    let layout = layout::layout(info.width, info.height);
    let fonts = Fonts::new(
        slopos_appkit::text::load_font("sans"),
        slopos_appkit::text::load_font("sans-semibold"),
    );
    let mut scene = Scene::new(layout, info.format, fate, fonts);
    let mut screen = Screen {
        px: buffer.as_mut_slice(),
        width: info.width as i32,
        height: info.height as i32,
        pitch: info.pitch as usize,
        format: info.format,
    };
    scene.paint_all(&mut screen);
    let mut stats = Stats::default();
    loop {
        match window::fb_flip(fd) {
            0 => break,
            1 => {
                stats.refused += 1;
                std::thread::sleep(RETRY);
            }
            _ => return Outcome::Lost("the screen refused a frame"),
        }
    }
    stats.shown += 1;
    log!(
        "{}x{}, wheel radius {}, on screen {} ms after start",
        info.width,
        info.height,
        layout.radius,
        started.elapsed().as_millis()
    );

    // The spin starts once it can be seen.
    let mut clock = Clock::new();
    let mut pending = Damage::default();
    loop {
        let began = Instant::now();
        let t = clock.tick();
        let painted = scene.paint(&mut screen, t);
        let took = began.elapsed();
        if !painted.is_empty() {
            stats.frames += 1;
            stats.paint_total += took;
            stats.paint_max = stats.paint_max.max(took);
        }
        pending = pending.union(painted);

        if !pending.is_empty() {
            let rects: Vec<DamageRect> = pending.rects().collect();
            match window::fb_flip_damage(fd, &rects) {
                0 => {
                    pending = Damage::default();
                    clock.shown();
                    stats.shown += 1;
                }
                1 => {
                    clock.refused();
                    stats.refused += 1;
                }
                _ => return Outcome::Lost("the screen refused a frame"),
            }
        }

        if spin::phase(t) == Phase::Done && pending.is_empty() {
            return Outcome::Shown(stats);
        }
        let next = if clock.holding() { RETRY } else { FRAME };
        if let Some(rest) = next.checked_sub(began.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// No screen to draw on: say it on the console instead.
fn spin_in_words(fate: u32) {
    let number = slopos_fate_core::wheel::landing(fate).number();
    let mut buf = [0u8; 64];
    eprintln!("{}: {}", scene::words::TITLE, scene::words::RULE);
    std::thread::sleep(Duration::from_millis(600));
    eprintln!("{}", scene::verdict(number, &mut buf));
    std::thread::sleep(Duration::from_millis(400));
}

pub fn roulette_user_main() {
    let started = Instant::now();
    let packed = roulette::spin();
    let fate = packed as u32;

    let mut info = DisplayInfo::default();
    let drawable = window::fb_info(&mut info) == 0
        && info.width != 0
        && info.height != 0
        && info.bytes_per_pixel() == 4;
    // The virtcon seat outranks the compositor's: the wheel draws before a
    // compositor exists, and takes the screen back if one is already up.
    let outcome = if !drawable {
        Outcome::Lost("no framebuffer it can draw on")
    } else if window::screen_acquire(window::SEAT_VIRTCON) < 0 {
        Outcome::Lost("the screen is held")
    } else {
        spin_on_screen(fate, &info, started)
    };
    match outcome {
        Outcome::Shown(s) => {
            let avg = s.paint_total / s.frames.max(1);
            log!(
                "{} frames, paint avg {} us max {} us, {} presents, {} held",
                s.frames,
                avg.as_micros(),
                s.paint_max.as_micros(),
                s.shown,
                s.refused
            );
        }
        Outcome::Lost(why) => {
            log!("{why}; spinning in words");
            spin_in_words(fate);
        }
    }

    roulette::result(packed);
    std::process::exit(0);
}
