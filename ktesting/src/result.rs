//! Outcome of running a single test entry.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TestResult {
    Pass = 0,
    Fail = 1,
    Panic = 2,
    Skipped = 3,
    OverTime = 4,
}

impl TestResult {
    #[inline]
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Fail | Self::Panic)
    }
}
