use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NapiState {
    Idle = 0,
    Scheduled = 1,
    Polling = 2,
}

pub struct NapiContext {
    state: AtomicU8,
    budget: u32,
    processed: AtomicU32,
}

impl NapiContext {
    pub const fn new(budget: u32) -> Self {
        Self {
            state: AtomicU8::new(NapiState::Idle as u8),
            budget,
            processed: AtomicU32::new(0),
        }
    }

    #[inline]
    pub fn budget(&self) -> u32 {
        self.budget
    }

    #[inline]
    pub fn processed(&self) -> u32 {
        self.processed.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn add_processed(&self, count: u32) {
        self.processed.fetch_add(count, Ordering::Relaxed);
    }

    #[inline]
    pub fn state(&self) -> NapiState {
        match self.state.load(Ordering::Acquire) {
            1 => NapiState::Scheduled,
            2 => NapiState::Polling,
            _ => NapiState::Idle,
        }
    }

    #[inline]
    pub fn is_scheduled(&self) -> bool {
        matches!(self.state(), NapiState::Scheduled)
    }

    pub fn schedule(&self) -> bool {
        self.state
            .compare_exchange(
                NapiState::Idle as u8,
                NapiState::Scheduled as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub fn begin_poll(&self) -> bool {
        self.state
            .compare_exchange(
                NapiState::Scheduled as u8,
                NapiState::Polling as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub fn complete(&self) {
        self.state.store(NapiState::Idle as u8, Ordering::Release);
    }
}
