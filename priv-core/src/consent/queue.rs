use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use super::{Answer, Ask, EXPIRY_MS, Kind, Refusal, asks_more};

/// How many questions may wait behind the one on screen.
pub const WAITING_MAX: usize = 8;
/// How many of them one uid's, so no one person keeps another's waiting.
pub const WAITING_EACH: usize = 4;
/// After the person refuses, what that uid had waiting is refused too, and so
/// is what it asks this soon after: a refusal is not answered with another
/// question.
pub const DECLINED_MS: u64 = 5_000;

/// privd's questions for the person, one on screen at a time, each with its
/// asker `T`. Times are milliseconds on any clock that only goes forward.
///
/// The agent takes a question only after a `READY` it sent since the last
/// one, so privd never has more than one question in flight to it: whatever
/// privd writes fits the channel whole, and a frame is never left torn.
pub struct Questions<T> {
    agent: u32,
    ready: bool,
    waiting: VecDeque<(Ask, T)>,
    showing: Option<Showing<T>>,
    /// Each uid refused lately, and until when.
    declined: Vec<(u32, u64)>,
    next_id: u64,
}

struct Showing<T> {
    id: u64,
    uid: u32,
    asker: T,
    expires: u64,
    kind: Kind,
    admins: Vec<String>,
    shown: bool,
}

impl<T> Default for Questions<T> {
    fn default() -> Self {
        Self {
            agent: 0,
            ready: false,
            waiting: VecDeque::new(),
            showing: None,
            declined: Vec::new(),
            next_id: 1,
        }
    }
}

impl<T> Questions<T> {
    /// The agent can draw a question now, in `version`: it says so once it
    /// can draw, and again each time the question it was shown goes.
    pub fn ready(&mut self, version: u32) {
        self.agent = version;
        self.ready = true;
    }

    /// The version the agent speaks, 0 while there is none.
    pub fn version(&self) -> u32 {
        self.agent
    }

    /// Queue `ask`, or hand its asker back with why it is refused at once.
    pub fn offer(&mut self, ask: Ask, asker: T, now: u64) -> Result<(), (T, Refusal)> {
        let refusal = if self.agent == 0 {
            Refusal::NoAgent
        } else if !asks_more(self.agent) && ask.kind != Kind::Confirm {
            Refusal::Outdated
        } else if self
            .declined
            .iter()
            .any(|&(uid, until)| uid == ask.uid && now < until)
        {
            Refusal::Declined
        } else if self.waiting.len() >= WAITING_MAX
            || self
                .waiting
                .iter()
                .filter(|(a, _)| a.uid == ask.uid)
                .count()
                >= WAITING_EACH
        {
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
        self.ready = false;
        ask.id = self.next_id;
        self.next_id += 1;
        let showing = self.showing.insert(Showing {
            id: ask.id,
            uid: ask.uid,
            asker,
            expires: now + EXPIRY_MS,
            kind: ask.kind,
            admins: ask.admins.clone(),
            shown: false,
        });
        Some((ask, &showing.asker))
    }

    /// The asker of question `id`, the first time the agent says it is on
    /// screen.
    pub fn shown(&mut self, id: u64) -> Option<&T> {
        let showing = self.showing.as_mut().filter(|s| s.id == id && !s.shown)?;
        showing.shown = true;
        Some(&showing.asker)
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

    /// Every asker the person's answer settles, with its result; `Err` for
    /// an answer in another form than its question takes, which ends the
    /// agent.
    #[allow(clippy::type_complexity)]
    pub fn answer(
        &mut self,
        answer: Answer,
        now: u64,
    ) -> Result<Vec<(T, Result<Answer, Refusal>)>, ()> {
        let mut settled = Vec::new();
        let Some(showing) = self.showing.take_if(|s| s.id == answer.id) else {
            return Ok(settled);
        };
        if !answer.fits(showing.kind, &showing.admins) {
            self.showing = Some(showing);
            return Err(());
        }
        if answer.allow {
            settled.push((showing.asker, Ok(answer)));
            return Ok(settled);
        }
        settled.push((showing.asker, Err(Refusal::Refused)));
        let uid = showing.uid;
        let (refused, kept): (VecDeque<_>, VecDeque<_>) =
            self.waiting.drain(..).partition(|(ask, _)| ask.uid == uid);
        self.waiting = kept;
        settled.extend(
            refused
                .into_iter()
                .map(|(_, asker)| (asker, Err(Refusal::Declined))),
        );
        self.declined
            .retain(|&(other, until)| other != uid && now < until);
        self.declined.push((uid, now + DECLINED_MS));
        Ok(settled)
    }

    /// The agent has gone, or another took its place, and nothing it was
    /// asked can be answered: every asker, to be refused.
    pub fn lost(&mut self) -> Vec<T> {
        self.agent = 0;
        self.ready = false;
        let mut askers: Vec<T> = self.showing.take().map(|s| s.asker).into_iter().collect();
        askers.extend(self.waiting.drain(..).map(|(_, asker)| asker));
        askers
    }
}

#[cfg(test)]
mod tests;
