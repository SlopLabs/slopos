//! A shell that hands what it holds on to the programs it runs, each spawned
//! with its whole mask from the process forked for it (`crate::held`): the
//! rescue shell, privd's `org.slopos.rescue`, which exists only on the install
//! medium and holds every capability privd gives, and a shell `priv` runs. A
//! shell started inside one, a script's included, is one too.

use std::sync::OnceLock;

use slopos_abi::caps::UNIVERSAL;

use crate::held::{PRIV_ENV, RESCUE_ENV};
use crate::privd::{self, Input};
use crate::syscall::core as sys_core;

pub const RESCUE_OPTION: &str = "--rescue";
const RESCUE_ACTION: &str = "org.slopos.rescue";

#[derive(Clone, Copy)]
pub enum Held {
    Rescue,
    Priv,
}

impl Held {
    /// The variable that makes every shell this one starts hand on what it
    /// holds too.
    pub fn env(self) -> &'static str {
        match self {
            Held::Rescue => RESCUE_ENV,
            Held::Priv => PRIV_ENV,
        }
    }

    pub fn name(self) -> &'static [u8] {
        match self {
            Held::Rescue => b"rescue",
            Held::Priv => b"priv",
        }
    }
}

static HELD: OnceLock<Held> = OnceLock::new();

/// Return as the rescue shell: the copy privd started for the action, or a
/// process already holding what it names. Anything else has privd run one on
/// a terminal of its own, and exits as that one does.
pub fn enter_rescue() {
    privd::elevate(Some(RESCUE_ACTION), &privd::own_args(), Input::Terminal);
    let _ = HELD.set(Held::Rescue);
}

/// Hand on what this shell holds when a shell that does, or `priv`, started
/// it.
pub fn inherit() {
    if std::env::var_os(RESCUE_ENV).is_some() {
        let _ = HELD.set(Held::Rescue);
    } else if std::env::var_os(PRIV_ENV).is_some() {
        let _ = HELD.set(Held::Priv);
    }
}

pub fn held() -> Option<Held> {
    HELD.get().copied()
}

/// What this shell is while it holds more than every program does, which its
/// programs then hold too.
pub fn holding() -> Option<Held> {
    held().filter(|_| sys_core::caps_drop(0) & !UNIVERSAL != 0)
}
