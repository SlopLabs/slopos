//! One port, root or hub, from connect to a device in a slot. The machine
//! decides; the tree carries its [`Action`]s out.

use super::Failure;
use crate::device::Speed;
use crate::hub::{PortStatus, change};
use crate::xhci::ring::Ticket;

/// TATTDB (USB 2.0 §7.1.7.3).
pub const DEBOUNCE_MS: u64 = 100;
pub const DEBOUNCE_LIMIT_MS: u64 = 2000;
pub const RESET_MS: u64 = 800;
/// A reset's change may go unreported.
pub const RESET_POLL_MS: u64 = 20;
/// TRSTRCY (USB 2.0 §7.1.7.5).
pub const RECOVERY_MS: u64 = 10;
pub const MAX_FAILURES: u8 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    /// Not yet read, or a hub port not yet powered.
    #[default]
    Idle,
    Empty,
    Debounce {
        until: u64,
        give_up: u64,
    },
    /// Waiting for the default-state turn.
    Ready,
    Resetting {
        deadline: u64,
        poll: u64,
    },
    /// The device is in the default state.
    Recovering {
        until: u64,
        speed: Speed,
    },
    /// `pulled` once the device left meanwhile.
    Slotting {
        ticket: Ticket,
        deadline: u64,
        speed: Speed,
        pulled: bool,
    },
    Attached {
        slot: u8,
    },
    /// Given up until unplugged.
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Read,
    Reset,
    Disable,
    EnableSlot(Speed),
    Detach(u8),
    /// Counted against the port.
    Failed(Failure),
    GiveUp,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Port {
    pub state: State,
    pub failures: u8,
    /// The last connection change.
    pub changed_at: u64,
    pub connected: bool,
    pub enabled: bool,
    pub speed: Option<Speed>,
}

impl Port {
    pub fn holds_default(&self) -> bool {
        matches!(
            self.state,
            State::Resetting { .. } | State::Recovering { .. } | State::Slotting { .. }
        )
    }

    /// Nothing left to do until the port changes again.
    pub fn is_quiet(&self) -> bool {
        matches!(
            self.state,
            State::Idle | State::Empty | State::Attached { .. } | State::Disabled
        )
    }

    fn connect(&mut self, now: u64) -> State {
        self.changed_at = now;
        State::Debounce {
            until: now + DEBOUNCE_MS,
            give_up: now + DEBOUNCE_LIMIT_MS,
        }
    }

    pub fn on_status(&mut self, now: u64, status: PortStatus) -> Action {
        self.connected = status.connected;
        self.enabled = status.enabled;
        self.speed = status.speed;
        let replaced = status.changed(change::CONNECT);
        let gone = !status.connected;
        if gone || replaced {
            self.changed_at = now;
        }
        let mut action = Action::None;
        self.state = match self.state {
            State::Idle | State::Empty | State::Disabled if gone => State::Empty,
            State::Idle | State::Empty => self.connect(now),
            State::Disabled if replaced => {
                self.failures = 0;
                self.connect(now)
            }
            State::Disabled => State::Disabled,
            State::Debounce { until, give_up } => {
                if !replaced && !gone && now >= until {
                    State::Ready
                } else if !replaced && gone && now >= until {
                    State::Empty
                } else if !replaced && !gone {
                    State::Debounce { until, give_up }
                } else if now < give_up {
                    State::Debounce {
                        until: now + DEBOUNCE_MS,
                        give_up,
                    }
                } else if gone {
                    State::Empty
                } else {
                    action = Action::Failed(Failure::Debounce);
                    State::Ready
                }
            }
            State::Slotting {
                ticket,
                deadline,
                speed,
                ..
            } if gone || replaced => State::Slotting {
                ticket,
                deadline,
                speed,
                pulled: true,
            },
            slotting @ State::Slotting { .. } => slotting,
            State::Attached { slot } if gone || replaced => {
                action = Action::Detach(slot);
                if gone {
                    State::Empty
                } else {
                    self.connect(now)
                }
            }
            attached @ State::Attached { .. } => attached,
            _ if gone => State::Empty,
            _ if replaced => self.connect(now),
            State::Resetting { deadline, poll } => {
                if status.resetting {
                    State::Resetting { deadline, poll }
                } else if status.enabled {
                    State::Recovering {
                        until: now + RECOVERY_MS,
                        speed: status.speed.unwrap_or(Speed::Full),
                    }
                } else if status.changed(change::RESET) {
                    action = Action::Failed(Failure::Reset);
                    State::Ready
                } else {
                    State::Resetting { deadline, poll }
                }
            }
            other => other,
        };
        if self.state == State::Empty {
            self.failures = 0;
        }
        action
    }

    pub fn on_timer(&mut self, now: u64) -> Action {
        match self.state {
            State::Debounce { until, .. } if now >= until => Action::Read,
            State::Resetting { deadline, .. } if now >= deadline => {
                self.state = State::Ready;
                Action::Failed(Failure::Reset)
            }
            State::Resetting { deadline, poll } if now >= poll => {
                self.state = State::Resetting {
                    deadline,
                    poll: now + RESET_POLL_MS,
                };
                Action::Read
            }
            State::Recovering { until, speed } if now >= until => Action::EnableSlot(speed),
            _ => Action::None,
        }
    }

    /// A SuperSpeed port its device already enabled skips the reset.
    pub fn take_turn(&mut self, now: u64, needs_reset: bool, speed: Option<Speed>) -> Action {
        match speed {
            Some(speed) if !needs_reset && self.enabled => {
                self.state = State::Recovering { until: now, speed };
                Action::EnableSlot(speed)
            }
            _ => {
                self.state = State::Resetting {
                    deadline: now + RESET_MS,
                    poll: now + RESET_POLL_MS,
                };
                Action::Reset
            }
        }
    }

    pub fn deadline(&self) -> Option<u64> {
        match self.state {
            State::Debounce { until, .. } => Some(until),
            State::Resetting { deadline, poll } => Some(deadline.min(poll)),
            State::Recovering { until, .. } => Some(until),
            State::Slotting { deadline, .. } => Some(deadline),
            _ => None,
        }
    }

    /// Another try starts from a reset, unless this was the last.
    pub fn fail(&mut self) -> Action {
        self.failures = self.failures.saturating_add(1);
        if self.failures >= MAX_FAILURES {
            self.state = State::Disabled;
            Action::GiveUp
        } else {
            self.state = State::Ready;
            Action::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected(changes: u16) -> PortStatus {
        PortStatus {
            connected: true,
            powered: true,
            changes,
            ..PortStatus::default()
        }
    }

    fn enabled(speed: Speed) -> PortStatus {
        PortStatus {
            enabled: true,
            speed: Some(speed),
            ..connected(change::RESET)
        }
    }

    #[test]
    fn a_connection_is_believed_once_it_holds_still() {
        let mut port = Port::default();
        assert_eq!(port.on_status(0, connected(change::CONNECT)), Action::None);
        assert_eq!(port.deadline(), Some(DEBOUNCE_MS));
        assert_eq!(port.on_timer(50), Action::None);
        assert_eq!(port.on_status(60, connected(change::CONNECT)), Action::None);
        assert_eq!(port.on_timer(110), Action::None, "the bounce restarted it");
        assert_eq!(port.on_timer(160), Action::Read);
        port.on_status(160, connected(0));
        assert_eq!(port.state, State::Ready);
        assert!(!port.is_quiet());
    }

    #[test]
    fn a_reset_enables_the_port_and_recovery_precedes_the_slot() {
        let mut port = Port {
            state: State::Ready,
            ..Port::default()
        };
        assert_eq!(port.take_turn(0, true, Some(Speed::High)), Action::Reset);
        assert!(port.holds_default());
        assert_eq!(port.on_timer(RESET_POLL_MS), Action::Read);
        let resetting = PortStatus {
            resetting: true,
            ..connected(0)
        };
        assert_eq!(port.on_status(20, resetting), Action::None);
        assert_eq!(port.on_status(30, enabled(Speed::Low)), Action::None);
        assert_eq!(
            port.state,
            State::Recovering {
                until: 30 + RECOVERY_MS,
                speed: Speed::Low
            }
        );
        assert_eq!(port.on_timer(35), Action::None);
        assert_eq!(port.on_timer(40), Action::EnableSlot(Speed::Low));
    }

    #[test]
    fn an_enabled_superspeed_port_needs_no_reset() {
        let mut port = Port::default();
        port.on_status(
            0,
            PortStatus {
                enabled: true,
                ..connected(change::CONNECT)
            },
        );
        port.state = State::Ready;
        assert_eq!(
            port.take_turn(5, false, Some(Speed::Super)),
            Action::EnableSlot(Speed::Super)
        );
        let mut disabled = Port {
            state: State::Ready,
            ..Port::default()
        };
        assert_eq!(
            disabled.take_turn(5, false, Some(Speed::Super)),
            Action::Reset
        );
    }

    #[test]
    fn three_failures_disable_the_port_until_it_is_replugged() {
        let mut port = Port {
            state: State::Ready,
            ..Port::default()
        };
        assert_eq!(port.fail(), Action::None);
        assert_eq!(port.state, State::Ready);
        port.take_turn(0, true, None);
        assert_eq!(port.on_timer(RESET_MS), Action::Failed(Failure::Reset));
        assert_eq!(
            port.state,
            State::Ready,
            "a reset that never ends is a failure"
        );
        assert_eq!(port.fail(), Action::None);
        assert_eq!(port.fail(), Action::GiveUp);
        assert_eq!(port.state, State::Disabled);
        assert_eq!(port.on_status(900, connected(0)), Action::None);
        assert_eq!(port.state, State::Disabled);
        port.on_status(1000, connected(change::CONNECT));
        assert!(matches!(port.state, State::Debounce { .. }));
        assert_eq!(port.failures, 0);
    }

    #[test]
    fn a_device_leaving_mid_enumeration_frees_its_turn() {
        let mut port = Port {
            state: State::Ready,
            ..Port::default()
        };
        port.take_turn(0, true, None);
        port.on_status(5, PortStatus::default());
        assert_eq!(port.state, State::Empty);
        assert!(!port.holds_default());

        let mut attached = Port {
            state: State::Attached { slot: 4 },
            ..Port::default()
        };
        assert_eq!(
            attached.on_status(7, connected(change::CONNECT)),
            Action::Detach(4)
        );
        assert!(matches!(attached.state, State::Debounce { .. }));
        assert_eq!(attached.changed_at, 7);
    }

    #[test]
    fn a_connection_that_never_holds_still_is_a_failure() {
        let mut port = Port::default();
        port.on_status(0, connected(change::CONNECT));
        for t in (50..DEBOUNCE_LIMIT_MS).step_by(50) {
            port.on_status(t, connected(change::CONNECT));
            assert!(matches!(port.state, State::Debounce { .. }));
        }
        assert_eq!(
            port.on_status(DEBOUNCE_LIMIT_MS, connected(change::CONNECT)),
            Action::Failed(Failure::Debounce)
        );
        assert_eq!(port.state, State::Ready);
    }

    #[test]
    fn slotting_survives_a_pull_until_its_command_answers() {
        let ticket = crate::xhci::ring::tests::ticket(3, 9);
        let mut port = Port {
            state: State::Slotting {
                ticket,
                deadline: 100,
                speed: Speed::High,
                pulled: false,
            },
            ..Port::default()
        };
        port.on_status(10, PortStatus::default());
        assert!(matches!(port.state, State::Slotting { pulled: true, .. }));
        assert!(port.holds_default());
    }
}
