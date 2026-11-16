//! An install's steps drawn in place on a terminal; nothing off one.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use slopos_extension_core::image::Sink;
use slopos_theme_core::words::Bytes;

use crate::syscall::fs;

const REDRAW: Duration = Duration::from_millis(100);
const BAR_MIN: usize = 8;
const BAR_MAX: usize = 30;
const TICKER_STACK: usize = 64 << 10;

pub struct Progress {
    shared: Option<Arc<Shared>>,
    ticker: Option<JoinHandle<()>>,
    started: Instant,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    step: &'static str,
    since: Option<Instant>,
    copy: Option<Copy>,
    done: bool,
    /// Redrawn only when it changes.
    shown: Option<String>,
}

struct Copy {
    total: u64,
    done: u64,
    since: Instant,
}

impl Progress {
    pub fn on_terminal() -> Progress {
        let mut progress = Progress {
            shared: None,
            ticker: None,
            started: Instant::now(),
        };
        if !fs::isatty(2) {
            return progress;
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        });
        let ticking = Arc::clone(&shared);
        let ticker = thread::Builder::new()
            .stack_size(TICKER_STACK)
            .spawn(move || tick(&ticking));
        if let Ok(ticker) = ticker {
            progress.shared = Some(shared);
            progress.ticker = Some(ticker);
        }
        progress
    }

    pub fn step(&self, step: &'static str) {
        self.change(|state| {
            state.step = step;
            state.since = Some(Instant::now());
            state.copy = None;
        });
    }

    pub fn copying(&self, total: u64) {
        self.change(|state| {
            state.step = "copying";
            state.since = Some(Instant::now());
            state.copy = Some(Copy {
                total,
                done: 0,
                since: Instant::now(),
            });
        });
    }

    pub fn counting<'a>(&'a self, sink: &'a mut (dyn Sink + Send)) -> Counted<'a> {
        Counted {
            sink,
            progress: self,
        }
    }

    pub fn took(&self) -> String {
        let taken = self.started.elapsed();
        match taken.as_secs() {
            0..10 => format!("{:.1} s", taken.as_secs_f64()),
            _ => duration(taken),
        }
    }

    /// Drawn at once, so every step shows.
    fn change(&self, change: impl FnOnce(&mut State)) {
        if let Some(shared) = &self.shared
            && let Ok(mut state) = shared.state.lock()
        {
            change(&mut state);
            draw(&mut state);
        }
    }

    fn copied(&self, bytes: u64) {
        let Some(shared) = &self.shared else {
            return;
        };
        if let Ok(mut state) = shared.state.lock()
            && let Some(copy) = &mut state.copy
        {
            copy.done = (copy.done + bytes).min(copy.total);
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        let Some(shared) = &self.shared else {
            return;
        };
        if let Ok(mut state) = shared.state.lock() {
            state.done = true;
            if state.shown.is_some() {
                let _ = write("\r\x1b[K");
            }
        }
        shared.wake.notify_one();
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
    }
}

pub struct Counted<'a> {
    sink: &'a mut (dyn Sink + Send),
    progress: &'a Progress,
}

impl Sink for Counted<'_> {
    fn buffer(&mut self) -> Result<Vec<u8>, String> {
        self.sink.buffer()
    }

    fn put(&mut self, at: u64, bytes: Vec<u8>) -> Result<(), String> {
        let len = bytes.len() as u64;
        self.sink.put(at, bytes)?;
        self.progress.copied(len);
        Ok(())
    }
}

fn tick(shared: &Shared) {
    let Ok(mut state) = shared.state.lock() else {
        return;
    };
    loop {
        state = match shared.wake.wait_timeout(state, REDRAW) {
            Ok((state, _)) => state,
            Err(_) => return,
        };
        if state.done {
            return;
        }
        draw(&mut state);
    }
}

fn draw(state: &mut State) {
    if state.done {
        return;
    }
    let cols = fs::tiocgwinsz(2)
        .ok()
        .map(|size| usize::from(size.ws_col))
        .filter(|&cols| cols > 0)
        .unwrap_or(80)
        .max(20);
    let line = render(state, cols);
    if line.is_some() && line != state.shown {
        if let Some(line) = &line {
            let _ = write(&format!("\r{line}\x1b[K"));
        }
        state.shown = line;
    }
}

fn write(text: &str) -> Result<(), ()> {
    let mut bytes = text.as_bytes();
    while !bytes.is_empty() {
        match fs::write_slice(2, bytes) {
            Ok(n) if n > 0 => bytes = &bytes[n..],
            Err(crate::syscall::SyscallError::EINTR) => {}
            _ => return Err(()),
        }
    }
    Ok(())
}

/// The bar goes first, then the rate and time left, as room runs out.
fn render(state: &State, cols: usize) -> Option<String> {
    if state.step.is_empty() {
        return None;
    }
    let head = format!("extctl: {}", state.step);
    let Some(copy) = &state.copy else {
        let ran = state.since.map_or(Duration::ZERO, |since| since.elapsed());
        let line = match ran.as_secs() {
            0 => format!("{head}…"),
            _ => format!("{head}… {}", duration(ran)),
        };
        return Some(cut(line, cols));
    };
    let percent = copy.done * 100 / copy.total.max(1);
    let sizes = format!("{} of {}", Bytes(copy.done), Bytes(copy.total));
    let ran = copy.since.elapsed().as_secs_f64();
    let mut tails = Vec::with_capacity(3);
    if ran >= 1.0 && copy.done > 0 {
        let rate = copy.done as f64 / ran;
        let left = Duration::from_secs_f64((copy.total - copy.done) as f64 / rate);
        tails.push(format!(
            "{percent:>3}%  {sizes}, {}/s, {} left",
            Bytes(rate as u64),
            duration(left)
        ));
    }
    tails.push(format!("{percent:>3}%  {sizes}"));
    tails.push(format!("{percent:>3}%"));
    for tail in &tails {
        let room = cols.saturating_sub(head.chars().count() + tail.chars().count() + 4);
        if room >= BAR_MIN {
            return Some(format!("{head} {}  {tail}", bar(copy, room.min(BAR_MAX))));
        }
    }
    Some(cut(format!("{head} {percent}%"), cols))
}

fn bar(copy: &Copy, width: usize) -> String {
    let filled = (copy.done.saturating_mul(width as u64) / copy.total.max(1)) as usize;
    let mut bar = "█".repeat(filled);
    bar.push_str(&"░".repeat(width - filled));
    bar
}

/// So the terminal never wraps the line.
fn cut(line: String, cols: usize) -> String {
    line.chars().take(cols.saturating_sub(1)).collect()
}

fn duration(taken: Duration) -> String {
    let secs = taken.as_secs();
    match secs {
        0..60 => format!("{secs} s"),
        _ => format!("{} min {} s", secs / 60, secs % 60),
    }
}
