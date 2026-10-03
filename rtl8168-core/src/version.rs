//! The MAC versions this driver brings up, told apart by the hardware
//! version id in TxConfig.

/// One version the driver knows: TxConfig's id matches when
/// `id & mask == value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChipVersion {
    pub name: &'static str,
    pub value: u16,
    pub mask: u16,
}

/// RTL8168h/8111h and the RTL8168M, which is the same MAC under another
/// name. Every other id is declined: the bring-up in [`crate::chip`] is
/// this one version's sequence.
pub const VERSIONS: [ChipVersion; 2] = [
    ChipVersion {
        name: "RTL8168h/8111h",
        value: 0x541,
        mask: 0x7cf,
    },
    ChipVersion {
        name: "RTL8168M",
        value: 0x6c0,
        mask: 0x7cf,
    },
];

/// The hardware version id TxConfig carries in bits 20..32, reserved bits
/// 24 and 25 dropped.
pub fn xid(tx_config: u32) -> u16 {
    ((tx_config >> 20) & 0xfcf) as u16
}

pub fn identify(tx_config: u32) -> Option<&'static ChipVersion> {
    let id = xid(tx_config);
    VERSIONS.iter().find(|v| id & v.mask == v.value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx_config(id: u16) -> u32 {
        u32::from(id) << 20 | 0x0000_0700
    }

    #[test]
    fn every_entry_is_identified_by_its_own_id() {
        for v in &VERSIONS {
            assert_eq!(identify(tx_config(v.value)), Some(v));
        }
    }

    #[test]
    fn bits_outside_the_mask_do_not_matter() {
        let h = &VERSIONS[0];
        assert_eq!(identify(tx_config(0x541 | 0x800)), Some(h));
        assert_eq!(identify(tx_config(0x541) | 0b11 << 24), Some(h));
        assert_eq!(identify(tx_config(0x541) | 0x000f_ffff), Some(h));
    }

    #[test]
    fn neighbouring_versions_are_declined() {
        for id in [
            0x4c0, 0x509, 0x5c8, 0x502, 0x54a, 0x54b, 0x481, 0x2c8, 0x609, 0x641, 0x540, 0x543,
            0x545, 0x501, 0x6c1, 0x6c9, 0x2c0, 0x3c0, 0x7c8, 0x000,
        ] {
            assert_eq!(identify(tx_config(id)), None, "xid {id:#x}");
        }
    }

    #[test]
    fn a_failed_read_is_declined() {
        assert_eq!(xid(u32::MAX), 0xfcf);
        assert_eq!(identify(u32::MAX), None);
    }
}
