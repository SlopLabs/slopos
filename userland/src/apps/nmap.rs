use core::ffi::c_void;

use crate::syscall::{fs, net::net_scan, tty, UserNetMember, USER_NET_MAX_MEMBERS};

fn write_out(buf: &[u8]) {
    if fs::write_slice(1, buf).is_err() {
        let _ = tty::write(buf);
    }
}

fn write_u8_dec(mut value: u8, out: &mut [u8], idx: &mut usize) {
    let mut tmp = [0u8; 3];
    let mut n = 0usize;
    loop {
        tmp[n] = b'0' + (value % 10);
        value /= 10;
        n += 1;
        if value == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        if *idx < out.len() {
            out[*idx] = tmp[n];
            *idx += 1;
        }
    }
}

fn write_hex_byte(value: u8, out: &mut [u8], idx: &mut usize) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    if *idx + 1 < out.len() {
        out[*idx] = HEX[(value >> 4) as usize];
        out[*idx + 1] = HEX[(value & 0x0f) as usize];
        *idx += 2;
    }
}

fn print_member(member: &UserNetMember) {
    let mut line = [0u8; 128];
    let mut i = 0usize;

    let prefix = b"host ";
    line[i..i + prefix.len()].copy_from_slice(prefix);
    i += prefix.len();

    write_u8_dec(member.ipv4[0], &mut line, &mut i);
    line[i] = b'.';
    i += 1;
    write_u8_dec(member.ipv4[1], &mut line, &mut i);
    line[i] = b'.';
    i += 1;
    write_u8_dec(member.ipv4[2], &mut line, &mut i);
    line[i] = b'.';
    i += 1;
    write_u8_dec(member.ipv4[3], &mut line, &mut i);

    let mid = b"  mac ";
    line[i..i + mid.len()].copy_from_slice(mid);
    i += mid.len();

    let mut m = 0usize;
    while m < member.mac.len() {
        write_hex_byte(member.mac[m], &mut line, &mut i);
        if m + 1 < member.mac.len() {
            line[i] = b':';
            i += 1;
        }
        m += 1;
    }

    line[i] = b'\n';
    i += 1;
    write_out(&line[..i]);
}

pub fn nmap_main(_arg: *mut c_void) -> ! {
    write_out(b"nmap: scanning...\n");

    let mut members = [UserNetMember::default(); USER_NET_MAX_MEMBERS];
    let count = net_scan(&mut members, true);

    if count < 0 {
        write_out(b"nmap: scan syscall failed\n");
        crate::syscall::core::exit_with_code(1);
    }

    if count == 0 {
        write_out(b"nmap: no members discovered\n");
        crate::syscall::core::exit_with_code(1);
    }

    write_out(b"nmap: discovered members\n");
    let mut idx = 0usize;
    while idx < count as usize && idx < members.len() {
        print_member(&members[idx]);
        idx += 1;
    }

    crate::syscall::core::exit();
}
