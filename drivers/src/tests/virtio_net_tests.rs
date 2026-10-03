use slopos_net::netdev::{DEVICE_REGISTRY, NetDevice};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::virtio::VIRTQ_DESC_F_NEXT;
use crate::virtio_net::{self, VirtioNetDev};

pub fn test_virtio_net_ready_and_link_up() -> TestResult {
    assert_test!(
        virtio_net::virtio_net_is_ready(),
        "virtio-net should be discovered and initialized"
    );
    assert_test!(
        virtio_net::virtio_net_link_up(),
        "virtio-net link should be reported up"
    );

    let mac = virtio_net::virtio_net_mac().unwrap_or([0; 6]);
    assert_test!(mac != [0; 6], "virtio-net MAC should not be all-zero");

    let Some(dev) = DEVICE_REGISTRY
        .enumerate()
        .iter()
        .find(|(_, m, _)| m.0 == mac)
        .map(|(dev, _, _)| *dev)
    else {
        return fail!("virtio-net is not in the device registry");
    };
    let ipv4 = slopos_net::iface::our_ip(dev).map_or([0; 4], |ip| ip.0);
    assert_test!(ipv4 != [0; 4], "virtio-net should acquire IPv4 via DHCP");
    pass!()
}

/// Asserted by outstanding depth, not a wall clock, which would measure the host.
pub fn test_virtio_net_tx_fire_and_forget() -> TestResult {
    const BURST: u64 = 8;

    if !virtio_net::virtio_net_is_ready() {
        return TestResult::Skipped;
    }

    let before_empty = VirtioNetDev.stats().tx_packets;
    assert_test!(
        virtio_net::virtio_net_transmit(&[]),
        "an empty submit was refused"
    );
    assert_eq_test!(
        VirtioNetDev.stats().tx_packets,
        before_empty,
        "an empty submit reached the ring"
    );

    let before = VirtioNetDev.stats().tx_packets;
    for _ in 0..BURST {
        if !virtio_net::virtio_net_transmit(&[0u8; 64]) {
            return fail!("the device refused a frame while ready");
        }
    }
    let advanced = VirtioNetDev.stats().tx_packets.wrapping_sub(before);
    assert_test!(
        advanced >= BURST,
        "{} submits advanced tx_packets by {} — the submit path is waiting on \
         its own completion",
        BURST,
        advanced
    );
    pass!()
}

/// `transmit_udp_packet` submits straight to the ring, so no test scope can
/// blackhole it — hence a TEST-NET-1 destination no host network routes.
pub fn test_virtio_net_raw_udp_tx() -> TestResult {
    const LOCAL: [u8; 4] = [192, 0, 2, 1];
    const PEER: [u8; 4] = [192, 0, 2, 2];

    let payload = [1u8, 2, 3, 4];
    let before = VirtioNetDev.stats().tx_packets;
    if !virtio_net::transmit_udp_packet(LOCAL, PEER, 50000, 53, &payload) {
        return TestResult::Skipped;
    }
    let advanced = VirtioNetDev.stats().tx_packets.wrapping_sub(before);
    assert_eq_test!(
        advanced,
        1,
        "a successful UDP submit did not advance the device's tx_packets"
    );
    pass!()
}

/// Zero-copy SG TX chain linkage, `[header] -> run0 -> run1` (SLOPRING § 13).
pub fn test_build_tx_chain_links_runs() -> TestResult {
    let slots = [3u16, 4, 5];
    let runs = [(0x0020_0000u64, 1500u32), (0x0030_0000u64, 200u32)];
    let Some(chain) = virtio_net::build_tx_chain_for_test(&slots, 0x1000, 42, &runs) else {
        return TestResult::Fail;
    };
    assert_test!(chain.len() == 3, "chain = header + 2 runs");

    let (s0, a0, l0, f0, n0) = chain[0];
    assert_test!(
        s0 == 3 && a0 == 0x1000 && l0 == 42 && (f0 & VIRTQ_DESC_F_NEXT) != 0 && n0 == 4,
        "header descriptor links to run 0"
    );
    let (s1, a1, l1, f1, n1) = chain[1];
    assert_test!(
        s1 == 4 && a1 == 0x0020_0000 && l1 == 1500 && (f1 & VIRTQ_DESC_F_NEXT) != 0 && n1 == 5,
        "run 0 links to run 1"
    );
    let (s2, a2, l2, f2, n2) = chain[2];
    assert_test!(
        s2 == 5 && a2 == 0x0030_0000 && l2 == 200 && f2 == 0 && n2 == 0,
        "run 1 terminates the chain"
    );

    // Empty datagrams take the inline single-copy path, not SG DMA.
    assert_test!(
        virtio_net::build_tx_chain_for_test(&[3, 4], 0x1000, 42, &runs).is_none(),
        "wrong slot count rejected"
    );
    assert_test!(
        virtio_net::build_tx_chain_for_test(&[3], 0x1000, 42, &[]).is_none(),
        "empty payload rejected"
    );
    TestResult::Pass
}

slopos_testing::stest!(name = test_build_tx_chain_links_runs, suite = virtio_net);
slopos_testing::stest!(name = test_virtio_net_raw_udp_tx, suite = virtio_net);
slopos_testing::stest!(name = test_virtio_net_ready_and_link_up, suite = virtio_net);
slopos_testing::stest!(
    name = test_virtio_net_tx_fire_and_forget,
    suite = virtio_net
);
