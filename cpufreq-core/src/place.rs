//! Where the scheduler puts a task on a part whose CPUs are not alike.
//!
//! A hybrid part's P-cores run a thread faster than its E-cores, and a P-core
//! thread whose sibling is busy shares that core's execution resources. With
//! an idle choice of all three, a task goes first to a P-core with nothing on
//! it, then to an E-core, and only then beside a busy sibling — the order Linux
//! settled on for Alder Lake. Among CPUs of one tier the higher HWP highest
//! level wins, which is how a part names its favoured cores.

use crate::cpuid::CoreType;

/// Whether placement reads the scores at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Every idle CPU is as good as another.
    Flat,
    /// Idle CPUs are ranked by [`score`].
    Ranked,
}

impl Placement {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Flat),
            1 => Some(Self::Ranked),
            _ => None,
        }
    }

    pub const fn raw(self) -> u32 {
        match self {
            Self::Flat => 0,
            Self::Ranked => 1,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Ranked => "ranked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuClass {
    Performance,
    Efficiency,
}

impl CpuClass {
    /// Every CPU of a part that is not hybrid is a performance CPU.
    pub const fn of(hybrid: bool, core_type: CoreType) -> Self {
        if hybrid && matches!(core_type, CoreType::Atom) {
            Self::Efficiency
        } else {
            Self::Performance
        }
    }
}

/// Higher is better. `sibling_busy` is whether another hardware thread of the
/// same core is running something now.
pub const fn score(class: CpuClass, sibling_busy: bool, highest_perf: u8) -> u32 {
    let tier = match (class, sibling_busy) {
        (CpuClass::Performance, false) => 3,
        (CpuClass::Efficiency, false) => 2,
        (_, true) => 1,
    };
    (tier << 8) | highest_perf as u32
}

/// One CPU a task may go to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub cpu: usize,
    pub idle: bool,
    pub score: u32,
}

/// The idle candidate with the best score, `prefer` when it is idle and
/// scores as well as the best (its caches are warm), otherwise the first of
/// the best in the order given — the caller rotates that order to spread
/// ties. `None` when no candidate is idle.
pub fn pick_idle(
    candidates: impl Iterator<Item = Candidate>,
    prefer: Option<usize>,
) -> Option<usize> {
    let mut best: Option<Candidate> = None;
    let mut preferred: Option<Candidate> = None;
    for candidate in candidates.filter(|c| c.idle) {
        if Some(candidate.cpu) == prefer {
            preferred = Some(candidate);
        }
        if best.is_none_or(|b| candidate.score > b.score) {
            best = Some(candidate);
        }
    }
    let best = best?;
    match preferred {
        Some(p) if p.score >= best.score => Some(p.cpu),
        _ => Some(best.cpu),
    }
}
