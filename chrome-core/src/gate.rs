//! The login and lock screens as a person drives them: typing at the list
//! chooses a person, anywhere else types the password, which Escape clears.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use slopos_priv_core::consent::Secret;
use slopos_priv_core::words::privd::UNCHECKED;

use crate::key::Key;

pub const UNLOCK: &str = "Unlock";
pub const LOG_IN: &str = "Log in";
pub const SWITCH_USER: &str = "Switch user…";
pub const WRONG: &str = "Wrong password. Try again.";
pub const CHECKING: &str = "Checking the password…";
/// How many people the login screen lists at once.
pub const PEOPLE_SHOWN: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    People,
    Field,
    Switch,
    Submit,
}

/// What a press on the screen landed on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// The person at that place in the list as it is shown.
    Person(usize),
    Field,
    Switch,
    Submit,
}

/// What the screen asks the compositor for.
#[derive(PartialEq, Eq, Debug)]
pub enum Asks {
    /// Check `secret` as the password of `person`, and log them in from the
    /// login screen or unlock the desktop.
    Check { person: String, secret: Secret },
    /// Bring the login screen in front.
    SwitchUser,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Checked {
    Passed,
    Wrong,
    Unchecked,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Said {
    Nothing,
    Checking,
    Wrong,
    Unchecked,
    Starting(String),
}

#[derive(Debug)]
pub struct Gate {
    people: Vec<String>,
    greeter: bool,
    chosen: usize,
    /// The first person the list shows.
    first: usize,
    focus: Focus,
    secret: Secret,
    said: Said,
    typed_ahead: String,
}

impl Gate {
    pub fn login(people: Vec<String>) -> Gate {
        let focus = match people.is_empty() {
            true => Focus::Field,
            false => Focus::People,
        };
        Gate::new(people, true, focus)
    }

    pub fn lock(person: String) -> Gate {
        Gate::new(alloc::vec![person], false, Focus::Field)
    }

    fn new(people: Vec<String>, greeter: bool, focus: Focus) -> Gate {
        Gate {
            people,
            greeter,
            chosen: 0,
            first: 0,
            focus,
            secret: Secret::default(),
            said: Said::Nothing,
            typed_ahead: String::new(),
        }
    }

    pub fn greeter(&self) -> bool {
        self.greeter
    }

    pub fn people(&self) -> &[String] {
        &self.people
    }

    pub fn chosen(&self) -> Option<usize> {
        (self.chosen < self.people.len()).then_some(self.chosen)
    }

    /// The first of the [`PEOPLE_SHOWN`] people the list shows.
    pub fn first_shown(&self) -> usize {
        self.first
    }

    /// Choose the next person or the one before, the list moving with the
    /// choice, as the wheel does.
    pub fn scroll(&mut self, down: bool) {
        let Some(last) = self.people.len().checked_sub(1) else {
            return;
        };
        if !self.busy() {
            self.choose(match down {
                true => (self.chosen + 1).min(last),
                false => self.chosen.saturating_sub(1),
            });
        }
    }

    fn choose(&mut self, at: usize) {
        self.chosen = at;
        if at < self.first {
            self.first = at;
        } else if at >= self.first + PEOPLE_SHOWN {
            self.first = at + 1 - PEOPLE_SHOWN;
        }
    }

    pub fn person(&self) -> Option<&str> {
        self.people.get(self.chosen).map(String::as_str)
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn typed_chars(&self) -> usize {
        self.secret.chars()
    }

    /// Checking a password, or starting the desktop of whoever passed: no
    /// key or press changes anything until the screen is reset.
    fn busy(&self) -> bool {
        matches!(self.said, Said::Checking | Said::Starting(_))
    }

    /// The button that checks the password.
    pub fn submit_label(&self) -> &'static str {
        match self.greeter {
            true => LOG_IN,
            false => UNLOCK,
        }
    }

    /// The order Tab moves focus in, which is the order they are drawn in.
    fn stops(&self) -> &'static [Focus] {
        match (self.greeter, self.people.is_empty()) {
            (true, false) => &[Focus::People, Focus::Field, Focus::Submit],
            (true, true) => &[Focus::Field, Focus::Submit],
            (false, _) => &[Focus::Field, Focus::Switch, Focus::Submit],
        }
    }

    /// What the screen says under the field, if anything.
    pub fn message(&self) -> Option<String> {
        match &self.said {
            Said::Nothing => None,
            Said::Checking => Some(String::from(CHECKING)),
            Said::Wrong => Some(String::from(WRONG)),
            Said::Unchecked => Some(String::from(UNCHECKED)),
            Said::Starting(name) => Some(format!("Starting {name}'s desktop…")),
        }
    }

    pub fn message_is_trouble(&self) -> bool {
        matches!(self.said, Said::Wrong | Said::Unchecked)
    }

    /// Back as it was put up, with `people` listed on the login screen.
    pub fn reset(&mut self, people: Vec<String>) {
        if self.greeter {
            self.people = people;
            self.chosen = 0;
            self.first = 0;
        }
        let first = self.stops()[0];
        self.focus = first;
        self.secret.clear();
        self.said = Said::Nothing;
        self.typed_ahead.clear();
    }

    pub fn key(&mut self, key: Key) -> Option<Asks> {
        if self.busy() {
            return None;
        }
        let key = match (key, self.focus) {
            (Key::Text(' '), Focus::People | Focus::Switch | Focus::Submit) => Key::Press,
            (key, _) => key,
        };
        if !matches!(key, Key::Text(_)) {
            self.typed_ahead.clear();
        }
        match key {
            Key::Move { back } => {
                let stops = self.stops();
                let at = stops.iter().position(|&f| f == self.focus).unwrap_or(0);
                let n = stops.len();
                self.focus = stops[(at + if back { n - 1 } else { 1 }) % n];
                None
            }
            Key::Arrow { right } => {
                self.focus = match (self.focus, right) {
                    (Focus::Switch, true) => Focus::Submit,
                    (Focus::Submit, false) if !self.greeter => Focus::Switch,
                    (other, _) => other,
                };
                None
            }
            Key::Choose { next } if self.focus == Focus::People => {
                self.scroll(next);
                None
            }
            Key::Choose { .. } => None,
            Key::Press => match self.focus {
                Focus::People => {
                    self.focus = Focus::Field;
                    None
                }
                Focus::Switch => Some(Asks::SwitchUser),
                Focus::Field | Focus::Submit => self.check(),
            },
            Key::Dismiss => {
                self.secret.clear();
                None
            }
            Key::Erase => {
                if self.focus == Focus::Field {
                    self.secret.pop();
                }
                None
            }
            Key::Text(ch) if ch.is_control() => None,
            Key::Text(ch) if self.focus == Focus::People => {
                self.typed_ahead.push(ch);
                let starting = |ahead: &str| {
                    let ahead = ahead.to_lowercase();
                    self.people
                        .iter()
                        .position(|p| p.to_lowercase().starts_with(&ahead))
                };
                let found = match starting(&self.typed_ahead) {
                    Some(at) => Some(at),
                    None => {
                        self.typed_ahead = String::from(ch);
                        starting(&self.typed_ahead)
                    }
                };
                if let Some(at) = found {
                    self.choose(at);
                }
                None
            }
            Key::Text(ch) => {
                self.focus = Focus::Field;
                self.secret.push(ch);
                None
            }
        }
    }

    pub fn click(&mut self, target: Target) -> Option<Asks> {
        if self.busy() {
            return None;
        }
        self.typed_ahead.clear();
        match target {
            Target::Person(at) if self.first + at < self.people.len() => {
                self.choose(self.first + at);
                self.focus = Focus::Field;
                None
            }
            Target::Person(_) => None,
            Target::Field => {
                self.focus = Focus::Field;
                None
            }
            Target::Switch if !self.greeter => {
                self.focus = Focus::Switch;
                Some(Asks::SwitchUser)
            }
            Target::Switch => None,
            Target::Submit => {
                self.focus = Focus::Submit;
                self.check()
            }
        }
    }

    fn check(&mut self) -> Option<Asks> {
        let person = self.person()?.into();
        self.said = Said::Checking;
        Some(Asks::Check {
            person,
            secret: core::mem::take(&mut self.secret),
        })
    }

    /// The check [`Asks::Check`] asked for came out `checked`.
    pub fn checked(&mut self, checked: Checked) {
        self.said = match checked {
            Checked::Passed if self.greeter => {
                Said::Starting(self.person().unwrap_or_default().into())
            }
            Checked::Passed => Said::Nothing,
            Checked::Wrong => Said::Wrong,
            Checked::Unchecked => Said::Unchecked,
        };
        self.focus = Focus::Field;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn people() -> Vec<String> {
        vec!["user".into(), "bob".into(), "carol".into()]
    }

    fn typed(gate: &mut Gate, text: &str) {
        for ch in text.chars() {
            assert_eq!(gate.key(Key::Text(ch)), None);
        }
    }

    fn checks(asks: Option<Asks>, who: &str, password: &[u8]) {
        match asks {
            Some(Asks::Check { person, secret }) => {
                assert_eq!(person, who);
                assert_eq!(secret.bytes(), password);
            }
            other => panic!("asked {other:?}"),
        }
    }

    #[test]
    fn a_name_typed_tab_and_a_password_log_that_person_in() {
        let mut gate = Gate::login(people());
        assert_eq!(gate.focus(), Focus::People);
        typed(&mut gate, "carol");
        assert_eq!(gate.person(), Some("carol"));
        gate.key(Key::Move { back: false });
        typed(&mut gate, "carol-pw");
        checks(gate.key(Key::Press), "carol", b"carol-pw");
        assert_eq!(gate.message().as_deref(), Some(CHECKING));
        assert_eq!(gate.typed_chars(), 0);
    }

    #[test]
    fn enter_on_the_list_moves_to_the_field_and_an_empty_password_is_asked() {
        let mut gate = Gate::login(vec!["user".into()]);
        typed(&mut gate, "user");
        assert_eq!(gate.key(Key::Press), None);
        assert_eq!(gate.focus(), Focus::Field);
        checks(gate.key(Key::Press), "user", b"");
    }

    #[test]
    fn the_arrows_choose_within_the_list_and_stop_at_its_ends() {
        let mut gate = Gate::login(people());
        gate.key(Key::Choose { next: false });
        assert_eq!(gate.chosen(), Some(0));
        for _ in 0..5 {
            gate.key(Key::Choose { next: true });
        }
        assert_eq!(gate.person(), Some("carol"));
        gate.key(Key::Move { back: false });
        gate.key(Key::Choose { next: false });
        assert_eq!(gate.person(), Some("carol"));
    }

    #[test]
    fn the_list_shows_the_choice_as_it_moves() {
        let crowd: Vec<String> = (0..9).map(|n| alloc::format!("p{n}")).collect();
        let mut gate = Gate::login(crowd);
        typed(&mut gate, "p6");
        assert_eq!(gate.first_shown(), 6 + 1 - PEOPLE_SHOWN);
        for _ in 0..4 {
            gate.key(Key::Choose { next: false });
        }
        assert_eq!((gate.first_shown(), gate.person()), (2, Some("p2")));
        for _ in 0..9 {
            gate.scroll(true);
        }
        assert_eq!((gate.first_shown(), gate.person()), (4, Some("p8")));
        gate.click(Target::Person(1));
        assert_eq!((gate.first_shown(), gate.person()), (4, Some("p5")));
    }

    #[test]
    fn a_name_typed_ahead_that_matches_nobody_starts_again_from_its_last_letter() {
        let mut gate = Gate::login(people());
        typed(&mut gate, "bq");
        assert_eq!(gate.person(), Some("bob"));
        typed(&mut gate, "c");
        assert_eq!(gate.person(), Some("carol"));
        gate.key(Key::Choose { next: false });
        typed(&mut gate, "U");
        assert_eq!(gate.person(), Some("user"));
    }

    #[test]
    fn tab_goes_round_the_screen_in_the_order_it_is_drawn() {
        let mut gate = Gate::lock("user".into());
        let mut seen = vec![gate.focus()];
        for _ in 0..3 {
            gate.key(Key::Move { back: false });
            seen.push(gate.focus());
        }
        assert_eq!(
            seen,
            [Focus::Field, Focus::Switch, Focus::Submit, Focus::Field]
        );
        gate.key(Key::Move { back: true });
        assert_eq!(gate.focus(), Focus::Submit);
        gate.key(Key::Arrow { right: false });
        assert_eq!(gate.focus(), Focus::Switch);
        gate.key(Key::Arrow { right: false });
        assert_eq!(gate.focus(), Focus::Switch);
        assert_eq!(gate.key(Key::Text(' ')), Some(Asks::SwitchUser));
    }

    #[test]
    fn typing_on_a_button_types_the_password() {
        let mut gate = Gate::lock("user".into());
        gate.key(Key::Move { back: false });
        typed(&mut gate, "pw");
        assert_eq!(gate.focus(), Focus::Field);
        assert_eq!(gate.typed_chars(), 2);
        gate.key(Key::Erase);
        assert_eq!(gate.typed_chars(), 1);
        gate.key(Key::Dismiss);
        assert_eq!(gate.typed_chars(), 0);
        assert_eq!(gate.key(Key::Text('\u{7}')), None);
        assert_eq!(gate.typed_chars(), 0);
    }

    #[test]
    fn nothing_is_taken_while_a_password_is_checked() {
        let mut gate = Gate::lock("user".into());
        typed(&mut gate, "pw");
        checks(gate.key(Key::Press), "user", b"pw");
        assert_eq!(gate.key(Key::Press), None);
        assert_eq!(gate.click(Target::Submit), None);
        typed(&mut gate, "more");
        assert_eq!(gate.typed_chars(), 0);
        gate.checked(Checked::Wrong);
        assert_eq!(gate.message().as_deref(), Some(WRONG));
        assert!(gate.message_is_trouble());
        assert_eq!(gate.focus(), Focus::Field);
        gate.checked(Checked::Unchecked);
        assert_eq!(gate.message().as_deref(), Some(UNCHECKED));
    }

    #[test]
    fn a_click_on_a_person_chooses_them_for_the_password() {
        let mut gate = Gate::login(people());
        assert_eq!(gate.click(Target::Person(1)), None);
        assert_eq!((gate.person(), gate.focus()), (Some("bob"), Focus::Field));
        assert_eq!(gate.click(Target::Person(9)), None);
        assert_eq!(gate.person(), Some("bob"));
        assert_eq!(gate.click(Target::Switch), None);
        checks(gate.click(Target::Submit), "bob", b"");
        gate.checked(Checked::Passed);
        assert_eq!(gate.message().as_deref(), Some("Starting bob's desktop…"));
        assert!(!gate.message_is_trouble());
        assert_eq!(gate.key(Key::Press), None);
        assert_eq!(gate.click(Target::Submit), None);
        gate.reset(people());
        assert_eq!(gate.message(), None);
    }

    #[test]
    fn a_login_screen_without_people_has_nobody_to_check() {
        let mut gate = Gate::login(Vec::new());
        assert_eq!(gate.focus(), Focus::Field);
        assert_eq!(gate.key(Key::Press), None);
        assert_eq!(gate.message(), None);
    }

    #[test]
    fn a_reset_forgets_what_was_typed_and_said() {
        let mut gate = Gate::login(people());
        typed(&mut gate, "b");
        gate.key(Key::Press);
        typed(&mut gate, "pw");
        gate.key(Key::Press);
        gate.checked(Checked::Wrong);
        gate.reset(vec!["dave".into()]);
        assert_eq!(gate.people(), ["dave"]);
        assert_eq!((gate.person(), gate.focus()), (Some("dave"), Focus::People));
        assert_eq!((gate.typed_chars(), gate.message()), (0, None));
    }

    #[test]
    fn every_word_on_the_screens_speaks_plainly() {
        use slopos_priv_core::words::PASSWORD;
        use slopos_theme_core::words::{label, sentence};
        for text in [PASSWORD, UNLOCK, LOG_IN, SWITCH_USER] {
            assert_eq!(label(text, &[]), Ok(()), "{text}");
        }
        let mut gate = Gate::login(people());
        gate.key(Key::Press);
        gate.key(Key::Press);
        assert_eq!(label(&gate.message().unwrap(), &[]), Ok(()));
        gate.checked(Checked::Passed);
        assert_eq!(label(&gate.message().unwrap(), &[]), Ok(()));
        for checked in [Checked::Wrong, Checked::Unchecked] {
            gate.checked(checked);
            assert_eq!(
                sentence(&gate.message().unwrap(), &[]),
                Ok(()),
                "{checked:?}"
            );
        }
    }
}
