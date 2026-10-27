//! The network indicator's glyph of the icon font and the tone it is drawn
//! in, by what the network's state means to the person.

use slopos_theme_core::{Icon, Tone};

use crate::netstate::NetIndicatorState;

/// A link that carries something is the network glyph, a link that carries
/// nothing the unplugged one; a fault takes a fault's tone, and switched off
/// is disabled rather than a fault.
pub const fn indicator_for(state: NetIndicatorState) -> (Icon, Tone) {
    match state {
        NetIndicatorState::Connected => (Icon::Network, Tone::Normal),
        NetIndicatorState::Limited => (Icon::Network, Tone::Warning),
        NetIndicatorState::Configuring => (Icon::Network, Tone::Secondary),
        NetIndicatorState::NoCarrier => (Icon::Unplug, Tone::Danger),
        NetIndicatorState::Disconnected => (Icon::Unplug, Tone::Secondary),
        NetIndicatorState::Disabled => (Icon::Network, Tone::Disabled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netstate::ALL_INDICATOR_STATES;

    #[test]
    fn every_state_looks_its_own() {
        for (i, &a) in ALL_INDICATOR_STATES.iter().enumerate() {
            for &b in &ALL_INDICATOR_STATES[i + 1..] {
                assert_ne!(indicator_for(a), indicator_for(b), "{a:?} and {b:?}");
            }
        }
    }

    #[test]
    fn only_a_fault_takes_a_fault_s_tone() {
        for &state in ALL_INDICATOR_STATES {
            let (_, tone) = indicator_for(state);
            let fault = matches!(tone, Tone::Danger | Tone::Warning);
            let expected = matches!(
                state,
                NetIndicatorState::NoCarrier | NetIndicatorState::Limited
            );
            assert_eq!(fault, expected, "{state:?}");
        }
    }
}
