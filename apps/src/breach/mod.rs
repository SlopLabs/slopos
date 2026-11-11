//! Breach: a first-person shooter. The game is `slopos-shooter-core`; this is
//! its window, which turns held keys into the game's input, runs it on the
//! tick and shows each frame it draws.

mod frame;

use std::time::Instant;

use slopos_appkit::theme::space;
use slopos_appkit::{
    Action, App, Button, ButtonRow, Dialog, ImageData, Key, NamedKey, Node, Page, PropertyList,
    Section, StatusBar, Text, Tone, WindowSpec, column,
};
use slopos_shooter_core::{Game, HEIGHT, Input, Outcome, Renderer, WIDTH};

pub const NAME: &str = "Breach";
const FRAME_MS: u64 = 33;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Title,
    Playing,
    Paused,
}

#[derive(Clone, Debug)]
pub enum Msg {
    Start,
    Resume,
    Retry,
    Continue,
    Quit,
}

pub struct Breach {
    game: Game,
    renderer: Renderer,
    screen: Screen,
    held: Input,
    pixels: Vec<u32>,
    shown: ImageData,
    last: Instant,
    window: (u32, u32),
}

impl Breach {
    pub fn new() -> Self {
        let game = Game::default();
        let mut renderer = Renderer::new();
        let mut pixels = vec![0; WIDTH * HEIGHT];
        renderer.draw(&game, &mut pixels);
        let shown = frame::picture(&pixels);
        let window = WINDOW.limits().0;
        Self {
            game,
            renderer,
            screen: Screen::Title,
            held: Input::default(),
            pixels,
            shown,
            last: Instant::now(),
            window: (window.width as u32, window.height as u32),
        }
    }

    fn redraw(&mut self) {
        self.renderer.draw(&self.game, &mut self.pixels);
        self.shown = frame::picture(&self.pixels);
    }

    /// Back to play: nothing counts as held from before, and the time spent
    /// away is not simulated.
    fn play(&mut self) -> Action {
        self.held = Input::default();
        self.last = Instant::now();
        self.screen = Screen::Playing;
        self.redraw();
        Action::Rebuild
    }

    /// The largest whole multiple of the frame that fits, so pixels stay
    /// square and sharp; a window smaller than the frame shrinks it.
    fn zoom(&self) -> f64 {
        let width = self.window.0 as f64 / WIDTH as f64;
        let height = (self.window.1 as f64 - StatusBar::HEIGHT as f64) / HEIGHT as f64;
        let fit = width.min(height);
        if fit >= 1.0 {
            fit.floor()
        } else {
            fit.max(0.25)
        }
    }

    fn status(&self) -> StatusBar {
        let p = &self.game.player;
        let mut cards = Vec::new();
        if p.blue {
            cards.push("blue");
        }
        if p.amber {
            cards.push("amber");
        }
        let hud = match cards.is_empty() {
            true => format!("Health {} · Rivets {}", p.health, p.rivets),
            false => format!(
                "Health {} · Rivets {} · Keycards: {}",
                p.health,
                p.rivets,
                cards.join(", ")
            ),
        };
        let text = self.game.message().map_or(hud, String::from);
        let tone = if p.health <= 25 {
            Tone::Danger
        } else {
            Tone::Normal
        };
        let level = &self.game.level;
        StatusBar::new(text).tone(tone).end(format!(
            "{} · {} of {} · {} left",
            level.name,
            self.game.level_index + 1,
            Game::level_count(),
            self.game.enemies_left()
        ))
    }

    fn arena(&self) -> Node<Msg> {
        column(
            0,
            vec![
                Node::Expand {
                    weight: 1,
                    child: Box::new(Node::Picture {
                        image: self.shown.clone(),
                        zoom: self.zoom(),
                        pan: (0, 0),
                        grabbing: false,
                        focused: false,
                        on_input: None,
                    }),
                },
                self.status().into(),
            ],
        )
    }

    fn dialog(&self) -> Option<Dialog<Msg>> {
        let g = &self.game;
        let minutes = (g.time / 60.0) as u32;
        let seconds = g.time as u32 % 60;
        let stats = || {
            PropertyList::new()
                .row("Machines stopped", g.kills.to_string())
                .row("Time on this deck", format!("{minutes}:{seconds:02}"))
        };
        let dialog = match (self.screen, g.outcome) {
            (Screen::Paused, _) => Dialog::new("Paused")
                .content(Text::new("Press Escape or Resume to carry on.").wrap())
                .footer(
                    ButtonRow::new()
                        .aside(Button::new("Quit").on_press(Msg::Quit))
                        .confirm(Button::new("Resume").on_press(Msg::Resume)),
                ),
            (_, Outcome::Dead) => Dialog::new("You were taken down")
                .content(Text::new("Start this deck again with full health.").wrap())
                .footer(
                    ButtonRow::new()
                        .aside(Button::new("Quit").on_press(Msg::Quit))
                        .confirm(Button::new("Try again").on_press(Msg::Retry)),
                ),
            (_, Outcome::Cleared) => Dialog::new(format!("{} cleared", g.level.name))
                .content(stats())
                .footer(ButtonRow::new().confirm(Button::new("Next deck").on_press(Msg::Continue))),
            (_, Outcome::Won) => Dialog::new("The station is yours")
                .content(column(
                    space::LG,
                    vec![
                        Text::new("Every deck is clear and the core is shut down.")
                            .wrap()
                            .into(),
                        stats().into(),
                    ],
                ))
                .footer(
                    ButtonRow::new()
                        .aside(Button::new("Quit").on_press(Msg::Quit))
                        .confirm(Button::new("Play again").on_press(Msg::Retry)),
                ),
            (_, Outcome::Playing) => return None,
        };
        Some(dialog)
    }

    fn title_page(&self) -> Node<Msg> {
        let controls = PropertyList::new()
            .row("Walk", "W and S, or Up and Down")
            .row("Step aside", "A and D")
            .row("Turn", "Left and Right")
            .row("Fire the rivet gun", "Space")
            .row("Open a door or work a panel", "E or Enter")
            .row("Pause", "Escape");
        Page::new(NAME)
            .subtitle(
                "The station's repair machines have turned on the crew. Fight through \
                 three decks, find the keycards and shut down the reactor core.",
            )
            .content(Section::new("Controls", controls))
            .footer(ButtonRow::new().confirm(Button::new("Start").primary().on_press(Msg::Start)))
            .into()
    }

    fn hold(&mut self, key: Key, pressed: bool) {
        let h = &mut self.held;
        match key {
            Key::Char(c) => match c.to_ascii_lowercase() {
                'w' => h.forward = pressed,
                's' => h.back = pressed,
                'a' => h.strafe_left = pressed,
                'd' => h.strafe_right = pressed,
                'e' => h.use_ = pressed,
                ' ' => h.fire = pressed,
                _ => {}
            },
            Key::Named(NamedKey::Up) => h.forward = pressed,
            Key::Named(NamedKey::Down) => h.back = pressed,
            Key::Named(NamedKey::Left) => h.turn_left = pressed,
            Key::Named(NamedKey::Right) => h.turn_right = pressed,
            Key::Named(NamedKey::Space) => h.fire = pressed,
            Key::Named(NamedKey::Enter) => h.use_ = pressed,
            _ => {}
        }
    }
}

impl Default for Breach {
    fn default() -> Self {
        Self::new()
    }
}

const WINDOW: WindowSpec = WindowSpec::new(WIDTH as i32 * 3, HEIGHT as i32 * 3 + StatusBar::HEIGHT)
    .min_size(WIDTH as i32, HEIGHT as i32 + StatusBar::HEIGHT);

impl App for Breach {
    type Message = Msg;

    fn view(&self) -> Node<Msg> {
        if self.screen == Screen::Title {
            return self.title_page();
        }
        match self.dialog() {
            Some(dialog) => Node::ZStack {
                children: vec![self.arena(), dialog.into()],
            },
            None => self.arena(),
        }
    }

    fn update(&mut self, msg: Msg) -> Action {
        match msg {
            Msg::Start => {
                self.game = Game::default();
                self.play()
            }
            Msg::Resume => self.play(),
            Msg::Retry if self.game.outcome == Outcome::Won => {
                self.game = Game::default();
                self.play()
            }
            Msg::Retry => {
                self.game.restart_level();
                self.play()
            }
            Msg::Continue => {
                self.game.next_level();
                self.play()
            }
            Msg::Quit => Action::Exit,
        }
    }

    fn page_key(&self) -> u64 {
        match self.screen {
            Screen::Title => 0,
            Screen::Playing | Screen::Paused => 1,
        }
    }

    fn tick_interval_ms(&self) -> Option<u64> {
        let running = self.screen == Screen::Playing && self.game.outcome == Outcome::Playing;
        running.then_some(FRAME_MS)
    }

    fn tick(&mut self) -> Action {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32();
        self.last = now;
        self.game.step(&self.held, dt);
        self.redraw();
        Action::Rebuild
    }

    fn on_key_change(&mut self, key: Key, pressed: bool) -> Action {
        let escape = key == Key::Named(NamedKey::Escape);
        match self.screen {
            Screen::Playing if escape && pressed && self.game.outcome == Outcome::Playing => {
                self.screen = Screen::Paused;
                Action::Rebuild
            }
            Screen::Paused if escape && pressed => self.play(),
            Screen::Playing => {
                self.hold(key, pressed);
                Action::None
            }
            Screen::Title | Screen::Paused => Action::None,
        }
    }

    fn on_resize(&mut self, width: u32, height: u32) -> Action {
        self.window = (width, height);
        Action::Rebuild
    }

    fn title(&self) -> String {
        NAME.into()
    }

    fn app_id(&self) -> &str {
        "org.slopos.breach"
    }

    fn window(&self) -> WindowSpec {
        WINDOW
    }
}

pub fn breach_main() -> ! {
    slopos_appkit::run_app(Breach::new())
}
