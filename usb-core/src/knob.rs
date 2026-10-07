//! The `usb=` command-line knob.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    /// Take every controller over from the firmware and run it.
    #[default]
    On = 0,
    /// Bind no controller: each stays the firmware's.
    Off = 1,
    /// Log each controller's capabilities, protocols and ports, then leave
    /// it to the firmware.
    Report = 2,
}

/// What the command line asked of USB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Knob<'a> {
    pub mode: Mode,
    /// The last `usb=` value naming no mode.
    pub ignored: Option<&'a str>,
}

impl<'a> Knob<'a> {
    /// The last `usb=` token naming a mode decides, since the built-in
    /// command line is appended to the loader's.
    pub fn parse(cmdline: &'a str) -> Self {
        let mut knob = Self::default();
        for value in cmdline
            .split_ascii_whitespace()
            .filter_map(|token| token.strip_prefix("usb="))
        {
            match value {
                "on" => knob.mode = Mode::On,
                "off" => knob.mode = Mode::Off,
                "report" => knob.mode = Mode::Report,
                other => knob.ignored = Some(other),
            }
        }
        knob
    }
}

impl Mode {
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Off,
            2 => Self::Report,
            _ => Self::On,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Knob, Mode};

    #[test]
    fn whole_tokens_only_and_the_last_wins() {
        let mode = |cmdline| Knob::parse(cmdline).mode;
        assert_eq!(mode(""), Mode::On);
        assert_eq!(mode("usb=off"), Mode::Off);
        assert_eq!(mode("quiet usb=report root=auto"), Mode::Report);
        assert_eq!(mode("usb=off usb=on"), Mode::On);
        assert_eq!(mode("xusb=off usb.settle_ms=0"), Mode::On);
    }

    #[test]
    fn a_value_it_does_not_know_is_reported_and_changes_nothing() {
        assert_eq!(
            Knob::parse("usb=report usb=of"),
            Knob {
                mode: Mode::Report,
                ignored: Some("of"),
            }
        );
        assert_eq!(Knob::parse("usb=OFF").ignored, Some("OFF"));
        assert_eq!(Knob::parse("usb=off").ignored, None);
    }

    #[test]
    fn round_trips_through_its_byte() {
        for mode in [Mode::On, Mode::Off, Mode::Report] {
            assert_eq!(Mode::from_u8(mode as u8), mode);
        }
        assert_eq!(Mode::from_u8(0xff), Mode::On);
    }
}
