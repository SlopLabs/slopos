//! The question a choice that ends something asks before it acts, or what says
//! it did not happen; while one is up it takes every key and every press.

use slopos_abi::InputEvent;
use slopos_appkit::Rect;
use slopos_appkit::traits::Role;
use slopos_chrome_core::key::Key;
use slopos_chrome_core::menu::{Answer, CLOSE, Confirm, Ending};

use super::overlay::Overlay;
use super::views;
use crate::gfx::{DamageRect, DrawBuffer};

enum Up {
    Question(Confirm),
    Failed(Ending),
}

pub struct Asking {
    asked: Option<Up>,
    overlay: Overlay,
    screen: Rect,
    answered: Option<Ending>,
}

impl Asking {
    pub fn new() -> Asking {
        Asking {
            asked: None,
            overlay: Overlay::new(),
            screen: Rect::new(0, 0, 0, 0),
            answered: None,
        }
    }

    pub fn ask(&mut self, ending: Ending) {
        self.show(Up::Question(Confirm::new(ending)));
    }

    /// Say that `ending`, which the person went ahead with, did not happen.
    pub fn failed(&mut self, ending: Ending) {
        self.show(Up::Failed(ending));
    }

    fn show(&mut self, up: Up) {
        self.asked = Some(up);
        self.overlay.changed();
    }

    pub fn active(&self) -> bool {
        self.asked.is_some()
    }

    pub fn withdraw(&mut self) {
        if self.asked.take().is_some() {
            self.overlay.hide();
        }
    }

    pub fn take_answer(&mut self) -> Option<Ending> {
        self.answered.take()
    }

    /// What to repaint, the question laid out again on `screen` when it
    /// changed.
    pub fn take_damage(
        &mut self,
        screen: Rect,
        pointer: (i32, i32),
    ) -> impl Iterator<Item = DamageRect> + use<> {
        if let Some(asked) = &self.asked
            && (self.overlay.stale() || self.screen != screen)
        {
            let (node, focus) = match asked {
                Up::Question(asked) => (views::confirm(asked), views::confirm_focus_name(asked)),
                Up::Failed(ending) => (views::failed(*ending), CLOSE),
            };
            self.overlay
                .show(&node, screen, pointer, Some((Role::Button, focus)));
            self.screen = screen;
        }
        self.overlay.take_damage()
    }

    fn answer(&mut self, answer: Answer) {
        let Some(asked) = self.asked.take() else {
            return;
        };
        self.overlay.hide();
        if let (Up::Question(asked), Answer::Go) = (asked, answer) {
            self.answered = Some(asked.ending());
        }
    }

    /// Every key while it is up, and a repeat of one pressed into it.
    pub fn takes_press(&mut self, event: &InputEvent) -> bool {
        let Some(asked) = self.asked.as_mut() else {
            return self.overlay.taken.repeats(event);
        };
        self.overlay.taken.take(event);
        let answer = Key::read(event).and_then(|key| match asked {
            Up::Question(asked) => {
                let was = asked.focus();
                let answer = asked.key(key);
                if asked.focus() != was {
                    self.overlay.changed();
                }
                answer
            }
            Up::Failed(_) => {
                matches!(key, Key::Press | Key::Dismiss | Key::Text(' ')).then_some(Answer::Cancel)
            }
        });
        if let Some(answer) = answer {
            self.answer(answer);
        }
        true
    }

    pub fn takes_release(&mut self, keycode: u16) -> bool {
        self.overlay.taken.release(keycode)
    }

    pub fn click(&mut self, x: i32, y: i32, pressed: bool) {
        let answers: Vec<Answer> = self.overlay.click(x, y, pressed);
        if let Some(&answer) = answers.first() {
            self.answer(answer);
        }
    }

    pub fn hover(&mut self, x: i32, y: i32) {
        self.overlay.hover(x, y);
    }

    pub fn draw(&self, buf: &mut DrawBuffer, clip: &DamageRect) {
        self.overlay.draw(buf, clip);
    }
}
