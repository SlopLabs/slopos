use core::ffi::c_void;

use crate::syscall::{UserNetInfo, core::exit_with_code, fs, net::net_info, tty};

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

fn write_u16_dec(mut value: u16, out: &mut [u8], idx: &mut usize) {
    let mut tmp = [0u8; 5];
    let mut n = 0usize;
    loop {
        tmp[n] = b'0' + (value % 10) as u8;
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

fn write_ipv4(ip: [u8; 4], out: &mut [u8], idx: &mut usize) {
    write_u8_dec(ip[0], out, idx);
    out[*idx] = b'.';
    *idx += 1;
    write_u8_dec(ip[1], out, idx);
    out[*idx] = b'.';
    *idx += 1;
    write_u8_dec(ip[2], out, idx);
    out[*idx] = b'.';
    *idx += 1;
    write_u8_dec(ip[3], out, idx);
}

pub fn ifconfig_main(_arg: *mut c_void) -> ! {
    let mut info = UserNetInfo::default();
    if net_info(&mut info) != 0 {
        write_out(b"ifconfig: net_info syscall failed\n");
        exit_with_code(1);
    }

    if info.nic_ready == 0 {
        write_out(b"ifconfig: no network interface\n");
        exit_with_code(1);
    }

    let mut line = [0u8; 196];
    let mut i = 0usize;

    line[i..i + 8].copy_from_slice(b"virtio0:");
    i += 8;
    line[i..i + 8].copy_from_slice(b" flags=<");
    i += 8;
    if info.link_up != 0 {
        line[i..i + 2].copy_from_slice(b"UP");
        i += 2;
    } else {
        line[i..i + 4].copy_from_slice(b"DOWN");
        i += 4;
    }
    line[i..i + 7].copy_from_slice(b">  mtu ");
    i += 7;
    write_u16_dec(info.mtu, &mut line, &mut i);
    line[i] = b'\n';
    i += 1;

    line[i..i + 11].copy_from_slice(b"           ");
    i += 11;
    line[i..i + 5].copy_from_slice(b"inet ");
    i += 5;
    write_ipv4(info.ipv4, &mut line, &mut i);
    line[i..i + 10].copy_from_slice(b"  netmask ");
    i += 10;
    write_ipv4(info.subnet_mask, &mut line, &mut i);
    line[i..i + 10].copy_from_slice(b"  gateway ");
    i += 10;
    write_ipv4(info.gateway, &mut line, &mut i);
    line[i] = b'\n';
    i += 1;

    line[i..i + 11].copy_from_slice(b"           ");
    i += 11;
    line[i..i + 6].copy_from_slice(b"ether ");
    i += 6;
    let mut m = 0usize;
    while m < info.mac.len() {
        write_hex_byte(info.mac[m], &mut line, &mut i);
        if m + 1 < info.mac.len() {
            line[i] = b':';
            i += 1;
        }
        m += 1;
    }
    line[i] = b'\n';
    i += 1;

    line[i..i + 11].copy_from_slice(b"           ");
    i += 11;
    line[i..i + 4].copy_from_slice(b"dns ");
    i += 4;
    write_ipv4(info.dns, &mut line, &mut i);
    line[i] = b'\n';
    i += 1;

    write_out(&line[..i]);
    crate::syscall::core::exit();
}
