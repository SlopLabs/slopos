//! The system menu under the menu bar's mark, and the question each choice
//! that ends something asks before it acts. A question has Cancel focused
//! first, so Enter alone never ends anything.

use crate::Key;

pub const RESTART_NOTICE: &str = "Restart the desktop to finish updating";
pub const CLOSE: &str = "Close";
/// What the place of a question says once what it ended did not happen.
pub const TRY_AGAIN: &str = slopos_priv_core::words::privd::FAULT;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    /// A new base changed the desktop's programs, which run on from the
    /// base they started from until the desktop restarts.
    RestartDesktop,
    Lock,
    SwitchUser,
    LogOut,
    Restart,
    PowerOff,
}

impl Choice {
    pub const MENU: [Choice; 5] = [
        Choice::Lock,
        Choice::SwitchUser,
        Choice::LogOut,
        Choice::Restart,
        Choice::PowerOff,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Choice::RestartDesktop => "Restart desktop…",
            Choice::Lock => "Lock screen",
            Choice::SwitchUser => "Switch user…",
            Choice::LogOut => "Log out…",
            Choice::Restart => "Restart…",
            Choice::PowerOff => "Power off…",
        }
    }

    /// The question it asks before it acts, if it asks one.
    pub fn ending(self) -> Option<Ending> {
        match self {
            Choice::RestartDesktop => Some(Ending::RestartDesktop),
            Choice::LogOut => Some(Ending::LogOut),
            Choice::Restart => Some(Ending::Restart),
            Choice::PowerOff => Some(Ending::PowerOff),
            Choice::Lock | Choice::SwitchUser => None,
        }
    }
}

/// What a key or a click on the menu comes to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Picked {
    Choice(Choice),
    Close,
}

/// The entries of the system menu and the one the keyboard is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Menu {
    notice: bool,
    cursor: Option<usize>,
}

impl Menu {
    /// The menu, led by the update notice when `notice`.
    pub fn new(notice: bool) -> Menu {
        Menu {
            notice,
            cursor: None,
        }
    }

    pub fn notice(&self) -> bool {
        self.notice
    }

    /// Its entries in their order, the notice's button first.
    pub fn entries(&self) -> impl Iterator<Item = Choice> + '_ {
        self.notice
            .then_some(Choice::RestartDesktop)
            .into_iter()
            .chain(Choice::MENU)
    }

    fn len(&self) -> usize {
        Choice::MENU.len() + usize::from(self.notice)
    }

    /// The entry the keyboard is on, once a key has moved it.
    pub fn cursor(&self) -> Option<Choice> {
        self.entries().nth(self.cursor?)
    }

    pub fn key(&mut self, key: Key) -> Option<Picked> {
        let forward = match key {
            Key::Choose { next } => next,
            Key::Move { back } => !back,
            Key::Press | Key::Text(' ') => return self.cursor().map(Picked::Choice),
            Key::Dismiss => return Some(Picked::Close),
            Key::Arrow { .. } | Key::Erase | Key::Text(_) => return None,
        };
        let last = self.len() - 1;
        self.cursor = Some(match (self.cursor, forward) {
            (None, true) => 0,
            (None, false) => last,
            (Some(at), true) => (at + 1).min(last),
            (Some(at), false) => at.saturating_sub(1),
        });
        None
    }
}

/// A choice that ends what the person is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ending {
    RestartDesktop,
    LogOut,
    Restart,
    PowerOff,
}

impl Ending {
    pub const ALL: [Ending; 4] = [
        Ending::RestartDesktop,
        Ending::LogOut,
        Ending::Restart,
        Ending::PowerOff,
    ];

    pub fn question(self) -> &'static str {
        match self {
            Ending::RestartDesktop => "Restart the desktop?",
            Ending::LogOut => "Log out?",
            Ending::Restart => "Restart the computer?",
            Ending::PowerOff => "Power off the computer?",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Ending::RestartDesktop | Ending::LogOut => {
                "Your apps will close, and anything that isn't saved will be lost."
            }
            Ending::Restart | Ending::PowerOff => "Anything that isn't saved will be lost.",
        }
    }

    pub fn verb(self) -> &'static str {
        match self {
            Ending::RestartDesktop => "Restart desktop",
            Ending::LogOut => "Log out",
            Ending::Restart => "Restart",
            Ending::PowerOff => "Power off",
        }
    }

    /// What the place of the question says once this did not happen.
    pub fn failed(self) -> &'static str {
        match self {
            Ending::RestartDesktop => "The desktop didn't restart",
            Ending::LogOut => "You weren't logged out",
            Ending::Restart => "The computer didn't restart",
            Ending::PowerOff => "The computer didn't power off",
        }
    }
}

/// Which button of a [`Confirm`] has focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    Cancel,
    Go,
}

/// The question an [`Ending`] asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Confirm {
    ending: Ending,
    focus: Answer,
}

impl Confirm {
    pub fn new(ending: Ending) -> Confirm {
        Confirm {
            ending,
            focus: Answer::Cancel,
        }
    }

    pub fn ending(&self) -> Ending {
        self.ending
    }

    pub fn focus(&self) -> Answer {
        self.focus
    }

    /// The answer a key gives, once it gives one.
    pub fn key(&mut self, key: Key) -> Option<Answer> {
        match key {
            Key::Move { .. } => {
                self.focus = match self.focus {
                    Answer::Cancel => Answer::Go,
                    Answer::Go => Answer::Cancel,
                };
                None
            }
            Key::Arrow { right } => {
                self.focus = match right {
                    true => Answer::Go,
                    false => Answer::Cancel,
                };
                None
            }
            Key::Press | Key::Text(' ') => Some(self.focus),
            Key::Dismiss => Some(Answer::Cancel),
            Key::Choose { .. } | Key::Erase | Key::Text(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn the_menu_offers_its_choices_with_the_notice_first() {
        let plain: Vec<Choice> = Menu::new(false).entries().collect();
        assert_eq!(plain, Choice::MENU);
        let noted: Vec<Choice> = Menu::new(true).entries().collect();
        assert_eq!(noted[0], Choice::RestartDesktop);
        assert_eq!(&noted[1..], Choice::MENU);
    }

    #[test]
    fn the_keyboard_walks_the_menu_and_stops_at_its_ends() {
        let mut menu = Menu::new(true);
        assert_eq!(menu.key(Key::Press), None);
        menu.key(Key::Choose { next: true });
        assert_eq!(menu.cursor(), Some(Choice::RestartDesktop));
        for _ in 0..9 {
            menu.key(Key::Choose { next: true });
        }
        assert_eq!(menu.cursor(), Some(Choice::PowerOff));
        menu.key(Key::Move { back: true });
        assert_eq!(menu.key(Key::Press), Some(Picked::Choice(Choice::Restart)));
        assert_eq!(menu.key(Key::Dismiss), Some(Picked::Close));
        let mut menu = Menu::new(false);
        menu.key(Key::Choose { next: false });
        assert_eq!(menu.cursor(), Some(Choice::PowerOff));
    }

    #[test]
    fn what_ends_something_asks_first_and_the_rest_acts() {
        for choice in Menu::new(true).entries() {
            let asks = choice.label().ends_with('…') && choice != Choice::SwitchUser;
            assert_eq!(choice.ending().is_some(), asks, "{choice:?}");
        }
    }

    #[test]
    fn enter_alone_cancels_a_question() {
        for ending in Ending::ALL {
            let mut asked = Confirm::new(ending);
            assert_eq!(asked.key(Key::Press), Some(Answer::Cancel));
            assert_eq!(asked.key(Key::Text('y')), None);
            assert_eq!(asked.key(Key::Move { back: false }), None);
            assert_eq!(asked.key(Key::Press), Some(Answer::Go));
            asked.key(Key::Arrow { right: false });
            asked.key(Key::Arrow { right: false });
            assert_eq!(asked.focus(), Answer::Cancel);
            assert_eq!(asked.key(Key::Text(' ')), Some(Answer::Cancel));
            asked.key(Key::Arrow { right: true });
            assert_eq!(asked.key(Key::Dismiss), Some(Answer::Cancel));
        }
    }

    #[test]
    fn every_word_on_the_menu_and_its_questions_speaks_plainly() {
        use slopos_priv_core::words::{detail, label};
        for text in Menu::new(true)
            .entries()
            .map(Choice::label)
            .chain([RESTART_NOTICE, CLOSE])
        {
            assert_eq!(label(text), Ok(()), "{text}");
        }
        assert_eq!(detail(TRY_AGAIN), Ok(()));
        for ending in Ending::ALL {
            assert_eq!(label(ending.question()), Ok(()), "{ending:?}");
            assert_eq!(label(ending.verb()), Ok(()), "{ending:?}");
            assert_eq!(label(ending.failed()), Ok(()), "{ending:?}");
            assert_eq!(detail(ending.detail()), Ok(()), "{ending:?}");
        }
    }
}
