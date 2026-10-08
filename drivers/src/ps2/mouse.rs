use slopos_hid_core::pointer::{Axis, Motion};
use slopos_ostd::klog_info;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

use crate::input_event::{self, PointerSource, get_timestamp_ms};
use crate::ps2;

struct MouseState {
    packet_byte: u8,
    packet: [u8; 4],
    packet_size: u8,
    mouse_type: u8,
}

impl MouseState {
    const fn new() -> Self {
        Self {
            packet_byte: 0,
            packet: [0; 4],
            packet_size: 3,
            mouse_type: 0,
        }
    }
}

static STATE: SpinLock<MouseState> = SpinLock::new(
    MouseState::new(),
    lock_class!("ps2mouse.STATE", LOCK_LEVEL_RESOURCE),
);

/// Magic sequence: SET_SAMPLE_RATE 200, 100, 80 → GET_ID → expect 3.
fn probe_intellimouse() -> u8 {
    ps2::write_aux_set_sample_rate(200);
    ps2::write_aux_set_sample_rate(100);
    ps2::write_aux_set_sample_rate(80);

    if !ps2::write_aux_acked(ps2::DEV_CMD_GET_ID) {
        return 0;
    }
    match ps2::read_aux_data() {
        Some(3) => 3,
        _ => 0,
    }
}

/// Requires ImPS/2 already active.
/// Magic sequence: SET_SAMPLE_RATE 200, 200, 80 → GET_ID → expect 4.
fn probe_intellimouse_explorer() -> u8 {
    ps2::write_aux_set_sample_rate(200);
    ps2::write_aux_set_sample_rate(200);
    ps2::write_aux_set_sample_rate(80);

    if !ps2::write_aux_acked(ps2::DEV_CMD_GET_ID) {
        return 3;
    }
    match ps2::read_aux_data() {
        Some(4) => 4,
        _ => 3,
    }
}

/// Requires `ps2::init_controller()` to have run already. Commands go through
/// the AUX-aware ACK path so a keyboard byte is never consumed as a mouse ACK.
pub fn init() {
    klog_info!("PS/2 mouse: initialising device");

    ps2::write_aux_acked(ps2::DEV_CMD_DEFAULTS);

    // Must happen before reporting is enabled.
    let mut mouse_type: u8 = probe_intellimouse();
    if mouse_type == 3 {
        mouse_type = probe_intellimouse_explorer();
    }

    let packet_size: u8 = if mouse_type >= 3 { 4 } else { 3 };
    klog_info!(
        "PS/2 mouse: detected type {} ({}), {}-byte packets",
        mouse_type,
        match mouse_type {
            0 => "standard",
            3 => "IntelliMouse (scroll wheel)",
            4 => "IntelliMouse Explorer (scroll + buttons 4/5)",
            _ => "unknown",
        },
        packet_size,
    );

    ps2::write_aux_acked(ps2::DEV_CMD_ENABLE);

    // The mouse may have sent trailing bytes during init.
    ps2::flush();

    {
        let mut state = STATE.lock();
        state.mouse_type = mouse_type;
        state.packet_size = packet_size;
        state.packet_byte = 0;
    }

    klog_info!("PS/2 mouse: initialised");
}

pub fn handle_irq(data: u8) {
    let mut state = STATE.lock();
    let byte_num = state.packet_byte;

    // Byte 0 sync: bit 3 is the PS/2 protocol marker and must be set.
    if byte_num == 0 && data & 0x08 == 0 {
        return;
    }

    state.packet[byte_num as usize] = data;
    state.packet_byte += 1;

    if state.packet_byte < state.packet_size {
        return;
    }
    state.packet_byte = 0;

    let packet_flags = state.packet[0];
    let dx_raw = state.packet[1];
    let dy_raw = state.packet[2];

    // Bits 7:6 are the overflow flags; discard the whole packet when set.
    if packet_flags & 0xC0 != 0 {
        return;
    }

    let buttons = packet_flags & 0x07;

    let mut dx = dx_raw as i16;
    if packet_flags & 0x10 != 0 {
        dx -= 256;
    }

    let mut dy = dy_raw as i16;
    if packet_flags & 0x20 != 0 {
        dy -= 256;
    }

    let mut z_toward_user: i32 = 0;
    let mut dw: i32 = 0;

    if state.mouse_type >= 3 && state.packet_size == 4 {
        let b3 = state.packet[3];
        match state.mouse_type {
            3 => {
                // ImPS/2: lower 4 bits are signed Z scroll
                let mut z = (b3 & 0x0F) as i8;
                if b3 & 0x08 != 0 {
                    z |= -16_i8;
                }
                z_toward_user = z as i32;
            }
            4 => {
                // ImExPS/2: upper 2 bits select encoding
                match b3 & 0xC0 {
                    0x00 | 0xC0 => {
                        // Standard: bits 3:0 = 4-bit signed Z
                        let mut z = (b3 & 0x0F) as i8;
                        if b3 & 0x08 != 0 {
                            z |= -16_i8;
                        }
                        z_toward_user = z as i32;
                    }
                    0x80 => {
                        // Vertical scroll (IM 4.0): bits 5:0 = 6-bit signed
                        let mut z = (b3 & 0x3F) as i8;
                        if b3 & 0x20 != 0 {
                            z |= -64_i8;
                        }
                        z_toward_user = z as i32;
                    }
                    0x40 => {
                        // Horizontal scroll (IM 4.0): bits 5:0 = 6-bit signed
                        let mut w = (b3 & 0x3F) as i8;
                        if b3 & 0x20 != 0 {
                            w |= -64_i8;
                        }
                        dw = -(w as i32);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    drop(state);

    let motion = Motion {
        buttons: buttons.into(),
        reported: u32::MAX,
        x: Some(Axis::Relative(dx.into())),
        y: Some(Axis::Relative((-dy).into())),
        wheel: -z_toward_user,
        pan: dw,
    };
    input_event::pointer_report(PointerSource::PS2, &motion, get_timestamp_ms());
}
