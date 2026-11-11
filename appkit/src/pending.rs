//! What a control shows while the system has yet to do what it asked.

use std::collections::VecDeque;

struct Asked<K, V> {
    what: K,
    value: V,
    answered: bool,
}

/// Changes asked of the system and not yet shown by it, oldest first: what
/// each changes, and to what. Answers come in the order asked.
pub struct Pending<K, V> {
    asked: VecDeque<Asked<K, V>>,
}

impl<K, V> Default for Pending<K, V> {
    fn default() -> Self {
        Self {
            asked: VecDeque::new(),
        }
    }
}

impl<K: PartialEq + Clone, V: Clone> Pending<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ask(&mut self, what: K, value: V) {
        self.asked.push_back(Asked {
            what,
            value,
            answered: false,
        });
    }

    /// The value last asked of `what`, or `actual` when none is.
    pub fn shown(&self, what: &K, actual: V) -> V {
        let last = self.asked.iter().rev().find(|asked| asked.what == *what);
        last.map_or(actual, |asked| asked.value.clone())
    }

    /// An answer is still owed.
    pub fn waiting(&self) -> bool {
        self.asked.iter().any(|asked| !asked.answered)
    }

    /// An answer is still owed for a change of `what`.
    pub fn waiting_for(&self, what: &K) -> bool {
        let owed = |asked: &Asked<K, V>| !asked.answered && asked.what == *what;
        self.asked.iter().any(owed)
    }

    /// Nothing asked is still shown.
    pub fn is_empty(&self) -> bool {
        self.asked.is_empty()
    }

    /// The oldest change owed an answer. One `made` stays shown until
    /// [`settle`](Self::settle); any other is dropped.
    pub fn answer(&mut self, made: bool) -> Option<(K, V)> {
        let at = self.asked.iter().position(|asked| !asked.answered)?;
        match made {
            true => {
                let asked = &mut self.asked[at];
                asked.answered = true;
                Some((asked.what.clone(), asked.value.clone()))
            }
            false => self.asked.remove(at).map(|asked| (asked.what, asked.value)),
        }
    }

    /// Drops the oldest change made, once the system was read again.
    pub fn settle(&mut self) -> Option<(K, V)> {
        let at = self.asked.iter().position(|asked| asked.answered)?;
        self.asked.remove(at).map(|asked| (asked.what, asked.value))
    }

    /// Gives up on everything asked.
    pub fn clear(&mut self) {
        self.asked.clear();
    }
}
