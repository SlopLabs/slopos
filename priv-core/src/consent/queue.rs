use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::{Answer, Ask, EXPIRY_MS, Refusal};

/// How many questions may wait behind the one on screen.
pub const WAITING_MAX: usize = 8;
/// After the person refuses, what was waiting is refused too, and so is what
/// asks this soon after: a refusal is not answered with another question.
pub const DECLINED_MS: u64 = 5_000;

/// privd's questions for the person, one on screen at a time, each with its
/// asker `T`. Times are milliseconds on any clock that only goes forward.
pub struct Questions<T> {
    ready: bool,
    waiting: VecDeque<(Ask, T)>,
    showing: Option<Showing<T>>,
    declined_until: Option<u64>,
    next_id: u64,
}

struct Showing<T> {
    id: u64,
    asker: T,
    expires: u64,
}

impl<T> Default for Questions<T> {
    fn default() -> Self {
        Self {
            ready: false,
            waiting: VecDeque::new(),
            showing: None,
            declined_until: None,
            next_id: 1,
        }
    }
}

impl<T> Questions<T> {
    /// The agent can draw a question.
    pub fn ready(&mut self) {
        self.ready = true;
    }

    /// Queue `ask`, or hand its asker back with why it is refused at once.
    pub fn offer(&mut self, ask: Ask, asker: T, now: u64) -> Result<(), (T, Refusal)> {
        let refusal = if !self.ready {
            Refusal::NoAgent
        } else if self.declined_until.is_some_and(|until| now < until) {
            Refusal::Declined
        } else if self.waiting.len() >= WAITING_MAX {
            Refusal::Busy
        } else {
            self.waiting.push_back((ask, asker));
            return Ok(());
        };
        Err((asker, refusal))
    }

    /// The question to put on screen now, numbered, and its asker.
    pub fn next(&mut self, now: u64) -> Option<(Ask, &T)> {
        if !self.ready || self.showing.is_some() {
            return None;
        }
        let (mut ask, asker) = self.waiting.pop_front()?;
        ask.id = self.next_id;
        self.next_id += 1;
        let showing = self.showing.insert(Showing {
            id: ask.id,
            asker,
            expires: now + EXPIRY_MS,
        });
        Some((ask, &showing.asker))
    }

    /// Drop every question whose asker has `left`. The one on screen comes
    /// back with its id, to be taken off it.
    pub fn withdraw(&mut self, left: impl Fn(&T) -> bool) -> Option<(u64, T)> {
        self.waiting.retain(|(_, asker)| !left(asker));
        if self.showing.as_ref().is_some_and(|s| left(&s.asker)) {
            return self.showing.take().map(|s| (s.id, s.asker));
        }
        None
    }

    /// The question on screen, once it has gone unanswered too long.
    pub fn expired(&mut self, now: u64) -> Option<(u64, T)> {
        if self.showing.as_ref().is_some_and(|s| now >= s.expires) {
            return self.showing.take().map(|s| (s.id, s.asker));
        }
        None
    }

    /// Every asker the person's answer settles, with its result.
    pub fn answer(&mut self, answer: Answer, now: u64) -> Vec<(T, Result<(), Refusal>)> {
        let mut settled = Vec::new();
        if !self.showing.as_ref().is_some_and(|s| s.id == answer.id) {
            return settled;
        }
        let showing = self.showing.take().expect("checked");
        if answer.allow {
            settled.push((showing.asker, Ok(())));
            return settled;
        }
        settled.push((showing.asker, Err(Refusal::Refused)));
        settled.extend(
            self.waiting
                .drain(..)
                .map(|(_, asker)| (asker, Err(Refusal::Declined))),
        );
        self.declined_until = Some(now + DECLINED_MS);
        settled
    }

    /// The agent has gone for good: every asker, to be refused.
    pub fn lost(&mut self) -> Vec<T> {
        self.ready = false;
        let mut askers: Vec<T> = self.showing.take().map(|s| s.asker).into_iter().collect();
        askers.extend(self.waiting.drain(..).map(|(_, asker)| asker));
        askers
    }
}

#[cfg(test)]
mod tests;
