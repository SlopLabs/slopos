use crate::syscall::{DisplayInfo, core as sys_core, roulette, tty, window};
use core::ffi::c_void;

#[unsafe(link_section = ".user_text")]
fn text_fallback(fate: u32) {
    const HDR: &[u8] = b"ROULETTE: framebuffer unavailable, using text fallback\n";
    const LBL: &[u8] = b"Fate number: ";
    tty::write(HDR);
    tty::write(LBL);

    let mut digits = [0u8; 32];
    let mut idx = 0usize;
    if fate == 0 {
        digits[idx] = b'0';
        idx += 1;
    } else {
        let mut n = fate;
        let mut tmp = [0u8; 32];
        let mut t = 0usize;
        while n != 0 && t < tmp.len() {
            tmp[t] = b'0' + (n % 10) as u8;
            n /= 10;
            t += 1;
        }
        while t > 0 {
            idx += 1;
            digits[idx - 1] = tmp[t - 1];
            t -= 1;
        }
    }
    tty::write(&digits[..idx]);
    tty::write(b"\n");
}

#[unsafe(link_section = ".user_rodata")]
static MSG_START: [u8; 16] = *b"ROULETTE: start\n";
#[unsafe(link_section = ".user_rodata")]
static MSG_FB_INFO_OK: [u8; 36] = *b"ROULETTE: fb_info ok, drawing wheel\n";

#[unsafe(link_section = ".user_text")]
pub fn roulette_user_main(_arg: *mut c_void) {
    let _ = tty::write(&MSG_START);
    let spin = roulette::spin();
    let fate = spin as u32;

    let mut info = DisplayInfo::default();
    let fb_rc = window::fb_info(&mut info);
    let fb_ok = fb_rc == 0 && info.width != 0 && info.height != 0;

    if !fb_ok {
        text_fallback(fate);
    } else {
        let _ = tty::write(&MSG_FB_INFO_OK);
        let _ = roulette::draw(fate);
    }

    sys_core::sleep_ms(3000);
    roulette::result(spin);
    sys_core::sleep_ms(500);
    sys_core::exit();
}
