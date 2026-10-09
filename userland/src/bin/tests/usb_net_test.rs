//! The network half of `just test-usb`: the echo peer on the USB NIC's own
//! network, which only `eth1`'s route reaches, answers over TCP. One segment
//! per write, of every length from 1 to 128 bytes, so some frames are whole
//! numbers of the adapter's 64-byte packets and need a zero-length packet to
//! end them.

use slopos_userland as _;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

use slopos_slibc::test_harness::note;

const PEER: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(10, 0, 3, 100), 9999);
/// The NIC was plugged back just before this runs and leases again.
const LEASE_WAIT: Duration = Duration::from_secs(120);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const LONGEST: usize = 128;

fn connect() -> Option<TcpStream> {
    let deadline = Instant::now() + LEASE_WAIT;
    loop {
        match TcpStream::connect(PEER) {
            Ok(stream) => return Some(stream),
            Err(e) if Instant::now() >= deadline => {
                note(&format!("connecting to {PEER}: {e}"));
                return None;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

fn the_usb_nic_carries_tcp() -> bool {
    let Some(mut stream) = connect() else {
        return false;
    };
    let configured = stream.set_nodelay(true).is_ok()
        && stream.set_read_timeout(Some(IO_TIMEOUT)).is_ok()
        && stream.set_write_timeout(Some(IO_TIMEOUT)).is_ok();
    if !configured {
        note("could not configure the socket");
        return false;
    }
    let mut echoed = [0u8; LONGEST];
    for length in 1..=LONGEST {
        let sent: Vec<u8> = (0..length).map(|i| (i * 7 + length) as u8).collect();
        if let Err(e) = stream.write_all(&sent) {
            note(&format!("writing {length} bytes: {e}"));
            return false;
        }
        if let Err(e) = stream.read_exact(&mut echoed[..length]) {
            note(&format!("reading {length} bytes back: {e}"));
            return false;
        }
        if echoed[..length] != sent[..] {
            note(&format!("{length} bytes came back changed"));
            return false;
        }
    }
    true
}

fn main() {
    slopos_slibc::test_harness::run(&[("the_usb_nic_carries_tcp", the_usb_nic_carries_tcp)]);
}
