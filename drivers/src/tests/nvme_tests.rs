//! What only the NVMe driver does: the controller it identified, the formats
//! its namespaces report, and the polled queue pair it keeps for the panic
//! path, driven here on the scratch namespace at 7 MiB, clear of every other
//! test's region.

use slopos_fs::blockdev::BlockDevice;
use slopos_ostd::KVec;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_ok, assert_test, fail, pass};

use crate::block;
use crate::block::engine::PAGE_SIZE;
use crate::nvme;

const PANIC_REGION: u64 = 7 << 20;

pub fn test_nvme_controller_identified() -> TestResult {
    let Some((controller, nsid)) = nvme::controller_of(b"nvme0n2") else {
        return fail!("nvme0n2 names no controller");
    };
    assert_eq_test!(nsid, 2, "the name's namespace ID");
    assert_test!(
        controller.volatile_write_cache(),
        "QEMU's controller reports a volatile write cache"
    );
    assert_eq_test!(
        controller.host_memory_bytes(),
        0,
        "QEMU's controller asks for no host memory"
    );
    assert_test!(
        controller.panic_queue().is_some(),
        "a controller with a spare queue pair keeps it for the panic path"
    );
    assert_test!(
        nvme::controller_of(b"vda").is_none() && nvme::controller_of(b"nvme7n1").is_none(),
        "only a probed NVMe name resolves"
    );
    pass!()
}

pub fn test_nvme_namespace_formats() -> TestResult {
    let expect = [
        (&b"nvme0n1"[..], 512),
        (&b"nvme0n2"[..], 512),
        (&b"nvme1n1"[..], 4096),
        (&b"nvme1n2"[..], 4096),
    ];
    for (name, block) in expect {
        let Some(disk) = block::disk(name) else {
            return fail!("{:?} is not registered", core::str::from_utf8(name));
        };
        assert_eq_test!(disk.logical_block_size(), block, "logical block size");
        assert_test!(
            disk.engine().max_transfer() >= PAGE_SIZE
                && disk.engine().max_transfer() <= block::engine::MAX_XFER,
            "the largest transfer is within what a slot's pages hold"
        );
    }
    pass!()
}

/// A write and a read through the panic queue, polled with no interrupt, land
/// where the ordinary queue sees them; a second taker is turned away while
/// the first holds it.
pub fn test_nvme_panic_queue_round_trip() -> TestResult {
    let Some((controller, _)) = nvme::controller_of(b"nvme0n2") else {
        return fail!("nvme0n2 names no controller");
    };
    let Some(disk) = block::disk(b"nvme0n2") else {
        return fail!("nvme0n2 is not registered");
    };
    let Some(queue) = controller.panic_queue() else {
        return fail!("no panic queue");
    };
    // The ordinary path's claim keeps anything else off the region.
    let _claim = match block::claim(b"nvme0n2") {
        Ok(c) => c,
        Err(e) => return fail!("claiming the scratch failed: {:?}", e),
    };
    let ns = disk.namespace();
    // A session holds a spinlock with interrupts off: nothing is allocated
    // while one is open.
    let mut data = assert_ok!(KVec::<u8>::zeroed(3 * 4096), "pattern");
    let mut back = assert_ok!(KVec::<u8>::zeroed(3 * 4096), "readback");
    let too_big = assert_ok!(
        KVec::<u8>::zeroed(queue.max_transfer() + 512),
        "oversized buffer"
    );
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(13) ^ 0x5C;
    }

    let Some(mut session) = queue.take() else {
        return fail!("the panic queue was already taken");
    };
    assert_test!(queue.take().is_none(), "a second taker must be turned away");
    if let Err(e) = session.write(ns, PANIC_REGION, &data) {
        return fail!("the polled write failed: {:?}", e);
    }
    if let Err(e) = session.flush(ns) {
        return fail!("the polled flush failed: {:?}", e);
    }
    if let Err(e) = session.read(ns, PANIC_REGION, &mut back) {
        return fail!("the polled read failed: {:?}", e);
    }
    assert_test!(
        back[..] == data[..],
        "the polled read must return the write"
    );
    assert_test!(
        session.write(ns, PANIC_REGION + 1, &data[..512]).is_err(),
        "an unaligned polled write must be refused"
    );
    assert_test!(
        session.write(ns, PANIC_REGION, &too_big).is_err(),
        "a polled write past the queue's pages must be refused"
    );
    drop(session);
    assert_test!(queue.take().is_some(), "a dropped session frees the queue");

    back.fill(0);
    assert_test!(
        disk.read_at(PANIC_REGION, &mut back).is_ok() && back[..] == data[..],
        "the ordinary queue must see what the panic queue wrote"
    );
    pass!()
}

/// The shutdown a poweroff sends: the host memory taken back, the queues
/// deleted, CC.SHN written and CSTS waited on; afterwards the controller's
/// disks refuse I/O, and a second shutdown is a no-op.
pub fn test_nvme_shutdown_notification() -> TestResult {
    use crate::driver_core::shutdown::DeviceShutdown;

    let Some((controller, _)) = nvme::controller_of(b"nvme2n1") else {
        return fail!("the spare controller is not attached");
    };
    let Some(disk) = block::disk(b"nvme2n1") else {
        return fail!("nvme2n1 is not registered");
    };
    let mut buf = [0u8; 512];
    assert_test!(disk.read_at(0, &mut buf).is_ok(), "a read before shutdown");
    controller.shutdown();
    assert_test!(
        controller.shutdown_complete(),
        "CSTS must report the shutdown complete"
    );
    assert_test!(
        disk.read_at(0, &mut buf).is_err(),
        "a shut-down controller's disk must refuse I/O"
    );
    controller.shutdown();
    assert_test!(
        controller.shutdown_complete(),
        "a second shutdown must leave the first's state"
    );
    pass!()
}

slopos_testing::stest!(name = test_nvme_controller_identified, suite = nvme);
slopos_testing::stest!(name = test_nvme_shutdown_notification, suite = nvme);
slopos_testing::stest!(name = test_nvme_namespace_formats, suite = nvme);
slopos_testing::stest!(name = test_nvme_panic_queue_round_trip, suite = nvme);
