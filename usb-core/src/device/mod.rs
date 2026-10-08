//! The USB device framework (USB 2.0 and USB 3.2, chapter 9).

pub mod descriptor;
pub mod request;
pub mod string;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Speed {
    Low,
    #[default]
    Full,
    High,
    Super,
    SuperPlus,
}

impl Speed {
    /// The Protocol Speed ID when the Supported Protocol capability lists no
    /// table (xHCI 1.2 §7.2.2.1.1).
    pub fn default_psiv(self) -> u8 {
        match self {
            Speed::Full => 1,
            Speed::Low => 2,
            Speed::High => 3,
            Speed::Super => 4,
            Speed::SuperPlus => 5,
        }
    }

    pub fn from_bits_per_second(bits_per_second: u64) -> Self {
        match bits_per_second {
            0..=1_500_000 => Speed::Low,
            1_500_001..=12_000_000 => Speed::Full,
            12_000_001..=480_000_000 => Speed::High,
            480_000_001..=5_000_000_000 => Speed::Super,
            _ => Speed::SuperPlus,
        }
    }

    pub fn is_super(self) -> bool {
        self >= Speed::Super
    }

    /// EP0's packet size before the device descriptor is read: 8 reads a
    /// full-speed device whose packet size is any of 8 to 64.
    pub fn initial_max_packet(self) -> u16 {
        match self {
            Speed::Low | Speed::Full => 8,
            Speed::High => 64,
            Speed::Super | Speed::SuperPlus => 512,
        }
    }

    /// What a bus-powered hub's port offers, in milliamps.
    pub fn unit_load_ma(self) -> u32 {
        if self.is_super() { 150 } else { 100 }
    }

    pub fn name(self) -> &'static str {
        match self {
            Speed::Low => "low speed",
            Speed::Full => "full speed",
            Speed::High => "high speed",
            Speed::Super => "SuperSpeed",
            Speed::SuperPlus => "SuperSpeedPlus",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Speed;

    #[test]
    fn speeds_map_from_rates_and_to_default_ids() {
        assert_eq!(Speed::from_bits_per_second(1_500_000), Speed::Low);
        assert_eq!(Speed::from_bits_per_second(12_000_000), Speed::Full);
        assert_eq!(Speed::from_bits_per_second(480_000_000), Speed::High);
        assert_eq!(Speed::from_bits_per_second(5_000_000_000), Speed::Super);
        assert_eq!(
            Speed::from_bits_per_second(10_000_000_000),
            Speed::SuperPlus
        );
        let ids = [Speed::Full, Speed::Low, Speed::High, Speed::Super].map(Speed::default_psiv);
        assert_eq!(ids, [1, 2, 3, 4]);
        assert_eq!(Speed::High.initial_max_packet(), 64);
        assert_eq!(Speed::Super.initial_max_packet(), 512);
        assert!(Speed::SuperPlus.is_super() && !Speed::High.is_super());
        assert_eq!(Speed::Super.unit_load_ma(), 150);
    }
}
