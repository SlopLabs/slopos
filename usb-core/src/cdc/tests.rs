use super::ntb::{self, Parameters};
use super::*;
use crate::device::descriptor::tests::{
    ECM_ETHERNET, configuration, endpoint, interface, qemu_ecm, qemu_rndis,
};
use std::vec::Vec;

const HEADER: [u8; 5] = [5, 0x24, 0x00, 0x10, 0x01];
const UNION_0_1: [u8; 5] = [5, 0x24, 0x06, 0, 1];
const NCM_DESCRIPTOR: [u8; 6] = [6, 0x24, 0x1a, 0x00, 0x01, 0x21];

fn ncm(functionals: &[&[u8]], data: &[&[u8]]) -> Vec<u8> {
    let control = interface(0, 0, 1, [2, 0x0d, 0]);
    let notify = endpoint(0x83, 3, 16, 9);
    let mut body: Vec<&[u8]> = Vec::from([&control[..]]);
    body.extend_from_slice(functionals);
    body.push(&notify);
    body.extend_from_slice(data);
    configuration(1, 0, &body)
}

fn ncm_data() -> [[u8; 9]; 2] {
    [
        interface(1, 0, 0, [10, 0, 1]),
        interface(1, 1, 2, [10, 0, 1]),
    ]
}

fn parse(bytes: &[u8], control: u8) -> Result<Function, Refusal> {
    Function::parse(&Configuration::parse(bytes).unwrap(), control)
}

#[test]
fn qemu_usb_net_ecm_is_an_ecm_function() {
    let function = parse(&qemu_ecm(), 0).unwrap();
    assert_eq!(function.model, Model::Ecm);
    assert_eq!(
        (function.control, function.data, function.alternate),
        (0, 1, 1)
    );
    assert_eq!(
        (function.bulk_in.address, function.bulk_in.max_packet),
        (0x82, 64)
    );
    assert_eq!(
        (function.bulk_out.address, function.bulk_out.max_packet),
        (0x02, 64)
    );
    let notify = function.notify.unwrap();
    assert_eq!((notify.address, notify.max_packet), (0x81, 16));
    assert_eq!(
        (
            function.mac_string,
            function.max_segment,
            function.capabilities
        ),
        (3, 1514, 0)
    );
}

#[test]
fn an_ncm_function_carries_its_capabilities() {
    let [alt0, alt1] = ncm_data();
    let bulk_in = endpoint(0x81, 2, 512, 0);
    let bulk_out = endpoint(0x02, 2, 512, 0);
    let bytes = ncm(
        &[&HEADER, &UNION_0_1, &ECM_ETHERNET, &NCM_DESCRIPTOR],
        &[&alt0, &alt1, &bulk_in, &bulk_out],
    );
    let function = parse(&bytes, 0).unwrap();
    assert_eq!(function.model, Model::Ncm);
    assert_eq!(function.capabilities, 0x21);
    assert_eq!(function.alternate, 1);
    assert_eq!(
        (function.bulk_in.address, function.bulk_out.address),
        (0x81, 0x02)
    );
    assert_eq!(function.notify.unwrap().address, 0x83);
}

#[test]
fn each_refusal_is_reached() {
    let [alt0, alt1] = ncm_data();
    let bulk_in = endpoint(0x81, 2, 512, 0);
    let bulk_out = endpoint(0x02, 2, 512, 0);
    let data: [&[u8]; 4] = [&alt0, &alt1, &bulk_in, &bulk_out];
    let refusal = |functionals: &[&[u8]], data: &[&[u8]]| parse(&ncm(functionals, data), 0);

    assert_eq!(parse(&qemu_rndis(), 0), Err(Refusal::Model));
    assert_eq!(parse(&qemu_ecm(), 1), Err(Refusal::Model));
    assert_eq!(parse(&qemu_ecm(), 7), Err(Refusal::Model));
    assert_eq!(
        refusal(&[&HEADER, &ECM_ETHERNET, &NCM_DESCRIPTOR], &data),
        Err(Refusal::Union)
    );
    assert_eq!(
        refusal(
            &[&HEADER, &[5, 0x24, 6, 1, 1], &ECM_ETHERNET, &NCM_DESCRIPTOR],
            &data
        ),
        Err(Refusal::Union)
    );
    assert_eq!(
        refusal(
            &[&HEADER, &[5, 0x24, 6, 0, 2], &ECM_ETHERNET, &NCM_DESCRIPTOR],
            &data
        ),
        Err(Refusal::Data)
    );
    let vendor = interface(1, 0, 2, [0xff, 0, 0]);
    assert_eq!(
        refusal(
            &[&HEADER, &UNION_0_1, &ECM_ETHERNET, &NCM_DESCRIPTOR],
            &[&vendor, &bulk_in, &bulk_out]
        ),
        Err(Refusal::Data)
    );
    assert_eq!(
        refusal(
            &[&HEADER, &UNION_0_1, &ECM_ETHERNET, &NCM_DESCRIPTOR],
            &[&alt0, &alt1, &bulk_in, &endpoint(0x03, 3, 64, 1)]
        ),
        Err(Refusal::Bulk)
    );
    assert_eq!(
        refusal(&[&HEADER, &UNION_0_1, &NCM_DESCRIPTOR], &data),
        Err(Refusal::Ethernet)
    );
    let mut no_mac = ECM_ETHERNET;
    no_mac[3] = 0;
    assert_eq!(
        refusal(&[&HEADER, &UNION_0_1, &no_mac, &NCM_DESCRIPTOR], &data),
        Err(Refusal::Ethernet)
    );
    let mut short = ECM_ETHERNET;
    short[0] = 12;
    assert_eq!(
        refusal(&[&HEADER, &UNION_0_1, &short[..12], &NCM_DESCRIPTOR], &data),
        Err(Refusal::Ethernet)
    );
    let mut small = ECM_ETHERNET;
    small[8..10].copy_from_slice(&1500u16.to_le_bytes());
    assert_eq!(
        refusal(&[&HEADER, &UNION_0_1, &small, &NCM_DESCRIPTOR], &data),
        Err(Refusal::Segment(1500))
    );
    assert_eq!(
        refusal(&[&HEADER, &UNION_0_1, &ECM_ETHERNET], &data),
        Err(Refusal::Ncm)
    );
    assert_eq!(
        std::format!("{}", Refusal::Segment(1500)),
        "maximum segment 1500 under 1514"
    );
}

fn string(text: &str) -> Vec<u8> {
    let mut bytes = Vec::from([0, 3]);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes[0] = bytes.len() as u8;
    bytes
}

#[test]
fn a_mac_string_names_a_unicast_address() {
    assert_eq!(
        mac_address(&string("400102030405")),
        Some([0x40, 1, 2, 3, 4, 5])
    );
    assert_eq!(
        mac_address(&string("02aBcDeF0102")),
        Some([0x02, 0xab, 0xcd, 0xef, 1, 2])
    );
    let mut padded = string("400102030405");
    padded.extend_from_slice(&[0, 0]);
    assert_eq!(mac_address(&padded), Some([0x40, 1, 2, 3, 4, 5]));

    assert_eq!(mac_address(&string("40010203040")), None);
    assert_eq!(mac_address(&string("4001020304050")), None);
    assert_eq!(mac_address(&string("40010203040g")), None);
    assert_eq!(mac_address(&string("4001020304 5")), None);
    assert_eq!(mac_address(&string("410102030405")), None, "group bit");
    assert_eq!(mac_address(&string("000000000000")), None);
    let mut kind = string("400102030405");
    kind[1] = 2;
    assert_eq!(mac_address(&kind), None);
    let wide = string("40010203040\u{0135}");
    assert_eq!(mac_address(&wide), None);
    let whole = string("400102030405");
    assert_eq!(mac_address(&whole[..whole.len() - 1]), None);
    assert_eq!(mac_address(&[]), None);
}

#[test]
fn notifications_report_the_link() {
    assert_eq!(
        notification(&[0xa1, 0, 1, 0, 1, 0, 0, 0]),
        Some(Notification::Connection(true))
    );
    assert_eq!(
        notification(&[0xa1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        Some(Notification::Connection(false))
    );
    let speed = [
        0xa1, 0x2a, 0, 0, 0, 0, 8, 0, 0, 0xe1, 0xf5, 5, 0, 0xe1, 0xf5, 5,
    ];
    assert_eq!(notification(&speed), Some(Notification::Other));
    assert_eq!(notification(&[0xa1, 0, 2, 0, 0, 0, 0, 0]), None);
    assert_eq!(notification(&[0xa1, 0, 1, 0, 0, 0, 1, 0]), None);
    assert_eq!(notification(&[0x21, 0, 1, 0, 0, 0, 0, 0]), None);
    assert_eq!(notification(&[0xa1, 0, 1, 0, 0, 0, 0]), None);
    assert_eq!(notification(&[]), None);
}

#[test]
fn class_requests_lay_out_as_ecm_and_ncm_name_them() {
    assert_eq!(
        Setup::set_ethernet_packet_filter(0, filter::DIRECTED | filter::BROADCAST).bytes(),
        [0x21, 0x43, 0x0c, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        Setup::get_ntb_parameters(2).bytes(),
        [0xa1, 0x80, 0, 0, 2, 0, 28, 0]
    );
    assert_eq!(
        Setup::set_ntb_format_16(1).bytes(),
        [0x21, 0x84, 0, 0, 1, 0, 0, 0]
    );
    assert_eq!(
        Setup::set_ntb_input_size(0, capability::CRC_MODE).bytes(),
        [0x21, 0x86, 0, 0, 0, 0, 4, 0]
    );
    assert_eq!(
        Setup::set_ntb_input_size(0, capability::NTB_INPUT_SIZE_8).bytes(),
        [0x21, 0x86, 0, 0, 0, 0, 8, 0]
    );
    assert_eq!(ntb_input_size(0x4000), [0, 0x40, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        Setup::set_crc_mode_off(0).bytes(),
        [0x21, 0x8a, 0, 0, 0, 0, 0, 0]
    );
}

#[test]
fn a_zero_length_packet_ends_whole_packet_transfers_under_the_limit() {
    assert!(needs_zlp(64, 64, usize::MAX));
    assert!(needs_zlp(1024, 512, usize::MAX));
    assert!(needs_zlp(512, 512, 1024));
    assert!(!needs_zlp(512, 512, 512));
    assert!(!needs_zlp(1024, 512, 512));
    assert!(!needs_zlp(63, 64, usize::MAX));
    assert!(!needs_zlp(65, 64, usize::MAX));
    assert!(!needs_zlp(0, 64, usize::MAX));
    assert!(!needs_zlp(64, 0, usize::MAX));
}

fn parameters(
    formats: u16,
    in_max: u32,
    out_max: u32,
    divisor: u16,
    remainder: u16,
    alignment: u16,
) -> [u8; 28] {
    let mut b = [0u8; 28];
    b[0..2].copy_from_slice(&28u16.to_le_bytes());
    b[2..4].copy_from_slice(&formats.to_le_bytes());
    b[4..8].copy_from_slice(&in_max.to_le_bytes());
    b[8..10].copy_from_slice(&4u16.to_le_bytes());
    b[12..14].copy_from_slice(&4u16.to_le_bytes());
    b[16..20].copy_from_slice(&out_max.to_le_bytes());
    b[20..22].copy_from_slice(&divisor.to_le_bytes());
    b[22..24].copy_from_slice(&remainder.to_le_bytes());
    b[24..26].copy_from_slice(&alignment.to_le_bytes());
    b[26..28].copy_from_slice(&32u16.to_le_bytes());
    b
}

fn out(max: u32, divisor: u16, remainder: u16, alignment: u16) -> ntb::Out {
    Parameters::parse(&parameters(1, 16384, max, divisor, remainder, alignment))
        .unwrap()
        .out
}

#[test]
fn ntb_parameters_parse_and_are_held_sane() {
    let p = Parameters::parse(&parameters(3, 65536, 16384, 4, 0, 4)).unwrap();
    assert_eq!((p.formats, p.in_max, p.out.max), (3, 65536, 16384));
    assert_eq!(p.out_max_datagrams, 32);
    assert!(p.ntb32());
    assert_eq!(p.in_size(), Some(ntb::IN_SIZE));
    let p = Parameters::parse(&parameters(1, 4096, 2048, 4, 0, 4)).unwrap();
    assert!(!p.ntb32());
    assert_eq!(p.in_size(), Some(4096));
    let p = Parameters::parse(&parameters(1, 2047, 2048, 4, 0, 4)).unwrap();
    assert_eq!(p.in_size(), None);

    let good = parameters(1, 16384, 16384, 4, 0, 4);
    assert!(Parameters::parse(&good[..27]).is_none());
    let mut short = good;
    short[0] = 27;
    assert!(Parameters::parse(&short).is_none());
    assert!(Parameters::parse(&parameters(2, 16384, 16384, 4, 0, 4)).is_none());
    assert!(Parameters::parse(&parameters(1, 16384, 2047, 4, 0, 4)).is_none());

    assert_eq!(out(100_000, 4, 0, 4).max, 65535);
    assert_eq!(out(4096, 0, 7, 6), out(4096, 4, 3, 4));
    assert_eq!(out(4096, 4, 0, 2), out(4096, 4, 0, 0));
    assert_ne!(out(4096, 4, 0, 8), out(4096, 4, 0, 4));
}

fn frame(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect()
}

#[test]
fn written_ntbs_read_back_as_their_frame() {
    let layouts = [
        (4, 0, 4),
        (4, 0, 8),
        (512, 0, 4),
        (512, 2, 4),
        (512, 2, 8),
        (6, 5, 16),
    ];
    let mut buffer = [0u8; 2048];
    for (divisor, remainder, alignment) in layouts {
        let out = out(2048, divisor, remainder, alignment);
        for len in ETHERNET_HEADER..=MAX_FRAME {
            let frame = frame(len, divisor as u8);
            let n = ntb::write(&frame, len as u16, &out, &mut buffer).unwrap();
            let ntb = &buffer[..n];
            assert_eq!(&ntb[0..4], b"NCMH");
            assert_eq!(u16::from_le_bytes([ntb[6], ntb[7]]), len as u16);
            assert_eq!(usize::from(u16::from_le_bytes([ntb[8], ntb[9]])), n);
            let ndp = usize::from(u16::from_le_bytes([ntb[10], ntb[11]]));
            assert_eq!(ndp % usize::from(alignment), 0);
            assert!(ndp >= ntb::NTH_LEN);
            let at = n - len;
            assert_eq!(at % usize::from(divisor), usize::from(remainder));
            assert!(at >= ndp + 16);
            let read: Vec<&[u8]> = ntb::datagrams(ntb).collect();
            assert_eq!(read, [&frame[..]]);
        }
    }
}

#[test]
fn a_frame_that_does_not_fit_is_not_written() {
    let out = out(2048, 4, 0, 4);
    let mut big = [0u8; 4096];
    assert_eq!(ntb::write(&[], 0, &out, &mut big), None);
    assert_eq!(ntb::write(&frame(2020, 0), 0, &out, &mut big), Some(2048));
    assert_eq!(ntb::write(&frame(2021, 0), 0, &out, &mut big), None);
    let mut small = [0u8; 100];
    assert_eq!(ntb::write(&frame(72, 0), 0, &out, &mut small), Some(100));
    assert_eq!(ntb::write(&frame(73, 0), 0, &out, &mut small), None);
}

fn put(ntb: &mut [u8], at: usize, value: u16) {
    ntb[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

/// NTH at 0; NDP at 12 with pairs (56, 14), (4, 20), (62, 10), (300, 20),
/// (72, 50) and no terminator, then NDP at 40 with (128, 100), (0, 0).
fn two_ndps() -> Vec<u8> {
    let mut ntb = std::vec![0u8; 256];
    ntb[0..4].copy_from_slice(b"NCMH");
    put(&mut ntb, 4, 12);
    put(&mut ntb, 8, 228);
    put(&mut ntb, 10, 12);
    ntb[12..16].copy_from_slice(b"NCM0");
    put(&mut ntb, 16, 28);
    put(&mut ntb, 18, 40);
    for (k, (index, len)) in [(56, 14), (4, 20), (62, 10), (300, 20), (72, 50)]
        .into_iter()
        .enumerate()
    {
        put(&mut ntb, 20 + 4 * k, index);
        put(&mut ntb, 22 + 4 * k, len);
    }
    ntb[40..44].copy_from_slice(b"NCM0");
    put(&mut ntb, 44, 16);
    put(&mut ntb, 48, 128);
    put(&mut ntb, 50, 100);
    for (i, byte) in ntb[56..228].iter_mut().enumerate() {
        *byte = i as u8;
    }
    ntb
}

#[test]
fn every_ndp_of_an_ntb_is_walked() {
    let ntb = two_ndps();
    let read: Vec<(usize, usize)> = ntb::datagrams(&ntb)
        .map(|d| (d.as_ptr() as usize - ntb.as_ptr() as usize, d.len()))
        .collect();
    assert_eq!(read, [(56, 14), (72, 50), (128, 100)]);
}

#[test]
fn a_wrong_header_or_ndp_yields_nothing_past_it() {
    let count = |ntb: &[u8]| ntb::datagrams(ntb).count();
    let good = two_ndps();
    assert_eq!(count(&good), 3);
    assert_eq!(count(&good[..228]), 3);
    assert_eq!(count(&good[..227]), 0, "block past the bytes");
    let mutate = |at: usize, value: u16| {
        let mut ntb = good.clone();
        put(&mut ntb, at, value);
        count(&ntb)
    };
    assert_eq!(mutate(0, 0x4e43), 0, "NTH signature");
    assert_eq!(mutate(4, 16), 0, "wHeaderLength");
    assert_eq!(mutate(8, 11), 0, "block inside the header");
    assert_eq!(mutate(8, 0), 3, "block 0 is the bytes");
    assert_eq!(mutate(10, 8), 0, "NDP inside the header");
    assert_eq!(mutate(10, 14), 0, "NDP unaligned");
    assert_eq!(mutate(14, 0x314d), 0, "NCM1 carries CRCs");
    assert_eq!(mutate(16, 12), 0, "NDP too short");
    assert_eq!(mutate(16, 30), 0, "NDP length unaligned");
    assert_eq!(mutate(16, 300), 0, "NDP past the block");
    assert_eq!(mutate(18, 0), 2, "no next NDP");
    assert_eq!(mutate(18, 42), 2, "next NDP unaligned");
    assert_eq!(mutate(8, 200), 2, "datagram past the block");
    assert_eq!(mutate(20, 0), 2, "pair (0, 14) is skipped");
    let mut terminated = good.clone();
    put(&mut terminated, 24, 0);
    put(&mut terminated, 26, 0);
    assert_eq!(count(&terminated), 2, "(0, 0) ends the first NDP");
}

#[test]
fn ndp_chains_and_datagram_counts_are_bounded() {
    let mut cycle = two_ndps();
    put(&mut cycle, 46, 12);
    assert_eq!(ntb::datagrams(&cycle).count(), 3 * 4);

    let mut many = std::vec![0u8; 2048];
    many[0..4].copy_from_slice(b"NCMH");
    put(&mut many, 4, 12);
    put(&mut many, 10, 12);
    many[12..16].copy_from_slice(b"NCM0");
    put(&mut many, 16, 8 + 4 * 100);
    for k in 0..100 {
        put(&mut many, 20 + 4 * k, 1024);
        put(&mut many, 22 + 4 * k, 14);
    }
    assert_eq!(ntb::datagrams(&many).count(), 64);
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn mutations(bytes: &[u8], seed: u64, rounds: usize, mut check: impl FnMut(&[u8])) {
    let mut state = seed;
    for len in 0..=bytes.len() {
        check(&bytes[..len]);
    }
    for at in 0..bytes.len() {
        for _ in 0..rounds {
            let r = next(&mut state);
            let mut mutated = bytes.to_vec();
            mutated[at] = r as u8;
            let other = (r >> 16) as usize % bytes.len();
            mutated[other] = (r >> 8) as u8;
            check(&mutated);
            let cut = (r >> 32) as usize % (bytes.len() + 1);
            check(&mutated[..cut]);
        }
    }
}

#[test]
fn mutated_configurations_never_panic() {
    let [alt0, alt1] = ncm_data();
    let bulk_in = endpoint(0x81, 2, 512, 0);
    let bulk_out = endpoint(0x02, 2, 512, 0);
    let ncm = ncm(
        &[&HEADER, &UNION_0_1, &ECM_ETHERNET, &NCM_DESCRIPTOR],
        &[&alt0, &alt1, &bulk_in, &bulk_out],
    );
    for bytes in [qemu_ecm(), ncm] {
        mutations(&bytes, 0x9e37_79b9_7f4a_7c15, 16, |b| {
            if let Ok(config) = Configuration::parse(b) {
                for interface in config.interfaces() {
                    let _ = Function::parse(&config, interface.number);
                }
            }
        });
    }
}

#[test]
fn mutated_strings_notifications_and_parameters_never_panic() {
    mutations(&string("400102030405"), 0x2545_f491_4f6c_dd1d, 32, |b| {
        if let Some(mac) = mac_address(b) {
            assert!(mac[0] & 1 == 0 && mac != [0; 6]);
        }
    });
    mutations(
        &[0xa1, 0x2a, 0, 0, 1, 0, 8, 0, 1, 2, 3, 4, 5, 6, 7, 8],
        0x1234_5678_9abc_def1,
        32,
        |b| {
            let _ = notification(b);
        },
    );
    mutations(
        &parameters(3, 16384, 16384, 512, 2, 8),
        0x0bad_cafe_dead_beef,
        32,
        |b| {
            if let Some(p) = Parameters::parse(b) {
                let _ = p.in_size();
                let mut buffer = [0u8; 2048];
                if let Some(n) = ntb::write(&frame(MAX_FRAME, 1), 0, &p.out, &mut buffer) {
                    assert_eq!(ntb::datagrams(&buffer[..n]).count(), 1);
                }
            }
        },
    );
}

#[test]
fn mutated_ntbs_yield_only_slices_inside_them() {
    let mut written = [0u8; 2048];
    let n = ntb::write(&frame(200, 9), 7, &out(2048, 512, 2, 8), &mut written).unwrap();
    for bytes in [two_ndps(), written[..n].to_vec()] {
        mutations(&bytes, 0xfeed_f00d_1234_5678, 32, |b| {
            let base = b.as_ptr() as usize;
            let mut count = 0;
            for d in ntb::datagrams(b) {
                let at = d.as_ptr() as usize;
                assert!(at >= base + ntb::NTH_LEN && at + d.len() <= base + b.len());
                assert!(d.len() >= ETHERNET_HEADER);
                count += 1;
            }
            assert!(count <= 64);
        });
    }
}
