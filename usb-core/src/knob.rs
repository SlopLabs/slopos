//! The `usb=` and `usb.settle_ms=` command-line knobs.

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

/// How long a boot step waits for the bus to settle when the device it
/// names is absent.
pub const SETTLE_MS: u32 = 5000;

/// What the command line asked of USB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Knob<'a> {
    pub mode: Mode,
    /// The last `usb=` value naming no mode.
    pub ignored: Option<&'a str>,
    pub settle_ms: u32,
}

impl Default for Knob<'_> {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            ignored: None,
            settle_ms: SETTLE_MS,
        }
    }
}

impl<'a> Knob<'a> {
    /// The last `usb=` token naming a mode decides, since the built-in
    /// command line is appended to the loader's.
    pub fn parse(cmdline: &'a str) -> Self {
        let mut knob = Self::default();
        for token in cmdline.split_ascii_whitespace() {
            if let Some(ms) = token
                .strip_prefix("usb.settle_ms=")
                .and_then(|v| v.parse().ok())
            {
                knob.settle_ms = ms;
            }
            let Some(value) = token.strip_prefix("usb=") else {
                continue;
            };
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
    use super::{Knob, Mode, SETTLE_MS};

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
                settle_ms: SETTLE_MS,
            }
        );
        assert_eq!(Knob::parse("usb=OFF").ignored, Some("OFF"));
        assert_eq!(Knob::parse("usb=off").ignored, None);
    }

    #[test]
    fn the_settle_wait_takes_the_last_number() {
        let settle = |cmdline| Knob::parse(cmdline).settle_ms;
        assert_eq!(settle(""), SETTLE_MS);
        assert_eq!(settle("usb.settle_ms=0"), 0);
        assert_eq!(settle("usb.settle_ms=250 usb.settle_ms=9000"), 9000);
        assert_eq!(settle("usb.settle_ms=soon"), SETTLE_MS);
        assert_eq!(Knob::parse("usb.settle_ms=7 usb=off").mode, Mode::Off);
    }

    #[test]
    fn round_trips_through_its_byte() {
        for mode in [Mode::On, Mode::Off, Mode::Report] {
            assert_eq!(Mode::from_u8(mode as u8), mode);
        }
        assert_eq!(Mode::from_u8(0xff), Mode::On);
    }
}
