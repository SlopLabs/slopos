//! Offsets and bit values in the 256-byte register window of the
//! RTL8111/8168 family (the first memory BAR).

pub const MAC0: usize = 0x00;
pub const MAC4: usize = 0x04;
pub const MAR0: usize = 0x08;
pub const TX_DESC_LOW: usize = 0x20;
pub const TX_DESC_HIGH: usize = 0x24;
pub const CHIP_CMD: usize = 0x37;
pub const TX_POLL: usize = 0x38;
pub const INTR_MASK: usize = 0x3c;
pub const INTR_STATUS: usize = 0x3e;
pub const TX_CONFIG: usize = 0x40;
pub const RX_CONFIG: usize = 0x44;
pub const CFG9346: usize = 0x50;
pub const CONFIG2: usize = 0x53;
pub const CONFIG3: usize = 0x54;
pub const CONFIG5: usize = 0x56;
pub const CSI_DATA: usize = 0x64;
pub const CSI_ACCESS: usize = 0x68;
pub const PHY_STATUS: usize = 0x6c;
pub const PMCH: usize = 0x6f;
pub const ERI_DATA: usize = 0x70;
pub const ERI_ACCESS: usize = 0x74;
pub const EEE_LED: usize = 0x1b;
pub const EPHY_ACCESS: usize = 0x80;
pub const MAC_OCP: usize = 0xb0;
pub const PHY_OCP: usize = 0xb8;
pub const DLLPR: usize = 0xd0;
pub const MCU: usize = 0xd3;
pub const RX_MAX_SIZE: usize = 0xda;
pub const CPLUS_CMD: usize = 0xe0;
pub const INTR_MITIGATE: usize = 0xe2;
pub const RX_DESC_LOW: usize = 0xe4;
pub const RX_DESC_HIGH: usize = 0xe8;
pub const MAX_TX_PACKET_SIZE: usize = 0xec;
pub const MISC: usize = 0xf0;
pub const MISC_1: usize = 0xf2;

pub const CMD_RESET: u8 = 1 << 4;
pub const CMD_RX_ENABLE: u8 = 1 << 3;
pub const CMD_TX_ENABLE: u8 = 1 << 2;

/// TxPoll: the normal-priority queue has descriptors to fetch.
pub const POLL_NORMAL_QUEUE: u8 = 1 << 6;

pub const CFG9346_LOCK: u8 = 0x00;
pub const CFG9346_UNLOCK: u8 = 0xc0;

pub const CONFIG2_CLKREQ_ENABLE: u8 = 1 << 7;
pub const CONFIG3_READY_TO_L23: u8 = 1 << 1;
pub const CONFIG5_ASPM_ENABLE: u8 = 1 << 0;

pub const PMCH_D3COLD_NO_PLL_DOWN: u8 = 1 << 7;
pub const PMCH_D3HOT_NO_PLL_DOWN: u8 = 1 << 6;

pub const DLLPR_TX_10M_PS_ENABLE: u8 = 1 << 7;
pub const DLLPR_PFM_ENABLE: u8 = 1 << 6;
pub const MISC_1_PFM_D3COLD_ENABLE: u8 = 1 << 6;

pub const MCU_NOW_IS_OOB: u8 = 1 << 7;
pub const MCU_TX_EMPTY: u8 = 1 << 5;
pub const MCU_RX_EMPTY: u8 = 1 << 4;
pub const MCU_LINK_LIST_READY: u8 = 1 << 1;

pub const MISC_RXDV_GATED: u32 = 1 << 19;

pub const TX_CONFIG_EMPTY: u32 = 1 << 11;
pub const TX_CONFIG_AUTO_FIFO: u32 = 1 << 7;
pub const TX_CONFIG_DMA_UNLIMITED: u32 = 7 << 8;
pub const TX_CONFIG_IFG_SHORTEST: u32 = 3 << 24;

pub const RX_CONFIG_128_INT: u32 = 1 << 15;
pub const RX_CONFIG_MULTI: u32 = 1 << 14;
pub const RX_CONFIG_EARLY_OFF: u32 = 1 << 11;
pub const RX_CONFIG_DMA_UNLIMITED: u32 = 7 << 8;
pub const RX_ACCEPT_ERROR: u32 = 1 << 5;
pub const RX_ACCEPT_RUNT: u32 = 1 << 4;
pub const RX_ACCEPT_BROADCAST: u32 = 1 << 3;
pub const RX_ACCEPT_MULTICAST: u32 = 1 << 2;
pub const RX_ACCEPT_MY_PHYS: u32 = 1 << 1;
pub const RX_ACCEPT_ALL_PHYS: u32 = 1 << 0;

pub const CPLUS_NORMAL_MODE: u16 = 1 << 13;
pub const CPLUS_INTT: u16 = 0b11;

pub const INTR_LINK_CHANGE: u16 = 1 << 5;
pub const INTR_RX_OVERFLOW: u16 = 1 << 4;
pub const INTR_TX_ERROR: u16 = 1 << 3;
pub const INTR_TX_OK: u16 = 1 << 2;
pub const INTR_RX_ERROR: u16 = 1 << 1;
pub const INTR_RX_OK: u16 = 1 << 0;

pub const PHY_STATUS_1000: u8 = 1 << 4;
pub const PHY_STATUS_100: u8 = 1 << 3;
pub const PHY_STATUS_10: u8 = 1 << 2;
pub const PHY_STATUS_LINK: u8 = 1 << 1;
pub const PHY_STATUS_FULL_DUPLEX: u8 = 1 << 0;

/// The flag bit every indirect access port (ERI, OCP, EPHY, CSI) shares: set
/// by the driver on a write and cleared by the chip when it lands; clear on
/// a read and set by the chip when the data is ready.
pub const ACCESS_FLAG: u32 = 1 << 31;
