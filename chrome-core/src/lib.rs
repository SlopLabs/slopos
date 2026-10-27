//! Pure geometry and state for the desktop's chrome: the system bar's status
//! items and the indicators drawn in them, where windows open, the clock,
//! the system menu and the lock and login screens a person drives.
//!
//! Split out of the compositor crate because `userland/Cargo.toml` sets
//! `test = false` on its `[lib]` target, so a `#[cfg(test)]` there never
//! compiles.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod clock;
pub mod gate;
pub mod indicator;
pub mod key;
pub mod menu;
pub mod netstate;
pub mod placement;
pub mod positioner;
pub mod status;

pub use indicator::indicator_for;
pub use key::Key;
pub use netstate::{
    IfaceKind, IfaceRow, NO_ADDRESS_LABEL, NetIndicatorState, NetPanelModel, PANEL_TITLE,
    indicator_label, indicator_state_for,
};
pub use positioner::{Anchor, ConstraintAdjustment, Gravity, Positioner, Rect, Size, position};
pub use status::{
    BAR_HEIGHT, BAR_ITEM_GAP, BAR_PADDING_X, MAX_STATUS_ITEMS, StatusItemSpec, StatusKind,
    StatusLayout, StatusSlot, hit_status_item, layout_status_items,
};
