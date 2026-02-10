// LAPIC register offsets
pub const LAPIC_ID: u32 = 0x020;
pub const LAPIC_VERSION: u32 = 0x030;
pub const LAPIC_EOI: u32 = 0x0B0;
pub const LAPIC_SPURIOUS: u32 = 0x0F0;
pub const LAPIC_ESR: u32 = 0x280;
pub const LAPIC_ICR_LOW: u32 = 0x300;
pub const LAPIC_ICR_HIGH: u32 = 0x310;
pub const LAPIC_LVT_TIMER: u32 = 0x320;
pub const LAPIC_LVT_PERFCNT: u32 = 0x340;
pub const LAPIC_LVT_LINT0: u32 = 0x350;
pub const LAPIC_LVT_LINT1: u32 = 0x360;
pub const LAPIC_LVT_ERROR: u32 = 0x370;
pub const LAPIC_TIMER_ICR: u32 = 0x380;
pub const LAPIC_TIMER_CCR: u32 = 0x390;
pub const LAPIC_TIMER_DCR: u32 = 0x3E0;

// LAPIC control flags
pub const LAPIC_SPURIOUS_ENABLE: u32 = 1 << 8;
pub const LAPIC_LVT_MASKED: u32 = 1 << 16;
pub const LAPIC_LVT_DELIVERY_MODE_EXTINT: u32 = 0x7 << 8;

// Timer configuration
pub const LAPIC_TIMER_PERIODIC: u32 = 0x0002_0000;
pub const LAPIC_TIMER_DIV_16: u32 = 0x3;

// IPI command flags
pub const LAPIC_ICR_DELIVERY_FIXED: u32 = 0 << 8;
pub const LAPIC_ICR_DEST_PHYSICAL: u32 = 0 << 11;
pub const LAPIC_ICR_LEVEL_ASSERT: u32 = 1 << 14;
pub const LAPIC_ICR_TRIGGER_EDGE: u32 = 0 << 15;
pub const LAPIC_ICR_DEST_BROADCAST: u32 = 0xFF << 24;
pub const LAPIC_ICR_DELIVERY_STATUS: u32 = 1 << 12;
