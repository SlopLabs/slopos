//! The question a choice that ends something asks before it acts, and what
//! takes its place when that did not happen, drawn over the desktop as a
//! dialog of appkit's components. While one is up it takes every key and
//! every press, as any dialog does.

use slopos_abi::InputEvent;
use slopos_appkit::Rect;
use slopos_appkit::embed::Embedded;
use slopos_appkit::traits::Role;
use slopos_chrome_core::Key;
use slopos_chrome_core::menu::{Answer, CLOSE, Confirm, Ending};

use super::views;
use crate::gfx::{DamageRect, DrawBuffer};

enum Up {
    Question(Confirm),
    Failed(Ending),
}

pub struct Asking {
    asked: Option<Up>,
    tree: Option<Embedded>,
    screen: Rect,
    stale: bool,
    dirty: bool,
    taken: slopos_chrome_core::key::Taken,
    answered: Option<Ending>,
}

impl Asking {
    pub fn new() -> Asking {
        Asking {
            asked: None,
            tree: None,
            screen: Rect::new(0, 0, 0, 0),
            stale: false,
            dirty: false,
            taken: slopos_chrome_core::key::Taken::default(),
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
        self.stale = true;
        self.dirty = true;
    }

    pub fn active(&self) -> bool {
        self.asked.is_some()
    }

    /// Take the question down unanswered.
    pub fn withdraw(&mut self) {
        if self.asked.take().is_some() {
            self.tree = None;
            self.dirty = true;
        }
    }

    /// What the person went ahead with, once.
    pub fn take_answer(&mut self) -> Option<Ending> {
        self.answered.take()
    }

    /// Whether the question appeared, changed or went since the last frame,
    /// laid out again on `screen` when it did.
    pub fn take_dirty(&mut self, screen: Rect) -> bool {
        if let Some(asked) = &self.asked
            && (self.stale || self.tree.is_none() || self.screen != screen)
        {
            let (node, focus) = match asked {
                Up::Question(asked) => (views::confirm(asked), views::confirm_focus_name(asked)),
                Up::Failed(ending) => (views::failed(*ending), CLOSE),
            };
            match &mut self.tree {
                Some(tree) => tree.rebuild(&node, screen),
                None => self.tree = Some(Embedded::new(&node, screen)),
            }
            if let Some(tree) = &mut self.tree {
                tree.focus(Role::Button, focus);
            }
            self.screen = screen;
            self.stale = false;
            self.dirty = true;
        }
        std::mem::take(&mut self.dirty)
    }

    fn answer(&mut self, answer: Answer) {
        let Some(asked) = self.asked.take() else {
            return;
        };
        self.tree = None;
        self.dirty = true;
        if let (Up::Question(asked), Answer::Go) = (asked, answer) {
            self.answered = Some(asked.ending());
        }
    }

    /// Whether a key press is the question's: every one while it is up, and
    /// a repeat of one pressed into it.
    pub fn takes_press(&mut self, event: &InputEvent) -> bool {
        let Some(asked) = self.asked.as_mut() else {
            return self.taken.repeats(event);
        };
        self.taken.take(event);
        let answer = Key::read(event).and_then(|key| match asked {
            Up::Question(asked) => {
                let was = asked.focus();
                let answer = asked.key(key);
                if asked.focus() != was {
                    self.stale = true;
                    self.dirty = true;
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
        self.taken.release(keycode)
    }

    /// The primary button went down or up at `(x, y)`.
    pub fn click(&mut self, x: i32, y: i32, pressed: bool) {
        let Some(tree) = self.tree.as_mut() else {
            return;
        };
        let answers: Vec<Answer> = tree.click(x, y, pressed);
        self.dirty = true;
        if let Some(&answer) = answers.first() {
            self.answer(answer);
        }
    }

    pub fn hover(&mut self, x: i32, y: i32) {
        if let Some(tree) = self.tree.as_mut() {
            self.dirty |= tree.hover(x, y);
        }
    }

    pub fn draw(&self, buf: &mut DrawBuffer, clip: &DamageRect) {
        let Some(tree) = &self.tree else {
            return;
        };
        let clip = Rect::new(
            clip.x0,
            clip.y0,
            clip.x1 - clip.x0 + 1,
            clip.y1 - clip.y0 + 1,
        );
        tree.paint(buf, clip);
    }
}
