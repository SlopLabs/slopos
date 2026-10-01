//! Which UEFI variables a caller reaches, and what a write to the firmware's
//! boot manager must look like. The decision alone: a test that wrote
//! `BootOrder` would rewrite the machine's.

use slopos_abi::Errno;
use slopos_boot_core::Guid;
use slopos_boot_core::device_path::{self, HardDrive};
use slopos_boot_core::load_option;
use slopos_boot_core::variables::{GLOBAL, LOADER, PERSISTENT, SLOPOS};
use slopos_ostd::authority::{BootEntry, Cap, Capability, check_mask};
use slopos_testing::{TestResult, fail, pass};

use crate::efivar::admit;

/// A `BootEntry` witness, as the dispatcher mints one for a holder.
fn with_boot_entry<R>(f: impl FnOnce(&Cap<'_, BootEntry>) -> R) -> R {
    let holder = ();
    let cap = check_mask::<BootEntry, _>(&holder, Capability::BootEntry.bit())
        .expect("the BootEntry bit mints a BootEntry witness");
    f(&cap)
}

const LOADER_PATH: &str = r"\EFI\SlopOS\BOOTX64.EFI";
const OPTION_LEN: usize = 6 + 2 * ("SlopOS".len() + 1) + device_path::loader_len(LOADER_PATH);

fn slopos_option(out: &mut [u8; OPTION_LEN]) -> bool {
    let esp = HardDrive {
        partition_number: 1,
        start_lba: 2048,
        blocks: 532_480,
        partition: Guid::from_spelling("6d3c2a91-5b0e-4b6f-9a51-2f0c3e4d5a6b"),
    };
    let mut path = [0u8; device_path::loader_len(LOADER_PATH)];
    device_path::loader(&esp, LOADER_PATH, &mut path).is_ok()
        && load_option::encode(load_option::ACTIVE, "SlopOS", &path, out) == Ok(OPTION_LEN)
}

/// `Power` reaches the loader's namespace and SlopOS's own; the global one
/// needs `BootEntry`, and then only the boot manager's four variables.
fn efivar_namespaces_follow_the_capability() -> TestResult {
    for guid in [LOADER, SLOPOS] {
        if admit(b"LoaderEntryDefault", guid, None, None) != Ok(()) {
            return fail!("{} must stay reachable under Power", guid);
        }
    }
    if admit(b"BootOrder", GLOBAL, None, None) != Err(Errno::EPERM) {
        return fail!("the global namespace must need BootEntry");
    }
    for name in [&b"Boot0003"[..], b"BootOrder", b"BootNext", b"BootCurrent"] {
        if with_boot_entry(|cap| admit(name, GLOBAL, None, Some(cap))) != Ok(()) {
            return fail!("BootEntry must read {:?}", core::str::from_utf8(name));
        }
    }
    for name in [
        &b"PK"[..],
        b"KEK",
        b"db",
        b"OsIndications",
        b"Driver0000",
        b"Boot000a",
    ] {
        if with_boot_entry(|cap| admit(name, GLOBAL, None, Some(cap))) != Err(Errno::EPERM) {
            return fail!("BootEntry must not reach {:?}", core::str::from_utf8(name));
        }
    }
    let shim = Guid::from_spelling("605dab50-e046-4300-abb6-3dd810dd8b23");
    if with_boot_entry(|cap| admit(b"MokList", shim, None, Some(cap))) != Err(Errno::EPERM) {
        return fail!("no other vendor's namespace is reachable");
    }
    pass!()
}

/// Whether a write of `value` to the global `name` is answered `want`.
#[inline(never)]
fn write_answers(name: &[u8], attributes: u32, value: &[u8], want: Result<(), Errno>) -> bool {
    let got = with_boot_entry(|cap| admit(name, GLOBAL, Some((attributes, value)), Some(cap)));
    if got != want {
        slopos_ostd::klog_info!(
            "EFIVAR_TEST: {:?} ({} bytes, attributes {:#x}): {:?}, want {:?}",
            core::str::from_utf8(name),
            value.len(),
            attributes,
            got,
            want
        );
    }
    got == want
}

/// A write reaches the firmware only in the shape it parses on every boot.
fn efivar_boot_manager_writes_are_held_to_their_formats() -> TestResult {
    let mut option = [0u8; OPTION_LEN];
    if !slopos_option(&mut option) {
        return fail!("the fixture option did not encode");
    }
    let nv = PERSISTENT;
    let held = write_answers(b"Boot0003", nv, &option, Ok(()))
        && write_answers(b"Boot0003", nv, &[], Ok(()))
        && write_answers(
            b"Boot0003",
            nv,
            &option[..OPTION_LEN - 4],
            Err(Errno::EINVAL),
        )
        && write_answers(b"Boot0003", nv | 0x20, &option, Err(Errno::EINVAL))
        && write_answers(b"BootOrder", nv, &[3, 0, 1, 0], Ok(()))
        && write_answers(b"BootOrder", nv, &[], Err(Errno::EINVAL))
        && write_answers(b"BootNext", nv, &[3, 0, 0], Err(Errno::EINVAL))
        && write_answers(b"BootCurrent", nv, &[3, 0], Err(Errno::EPERM));
    if !held {
        return fail!("a boot manager write was answered wrongly; see EFIVAR_TEST");
    }
    if admit(b"BootOrder", GLOBAL, Some((nv, &[3, 0])), None) != Err(Errno::EPERM) {
        return fail!("a write without BootEntry is refused before it is parsed");
    }
    pass!()
}

slopos_testing::stest!(
    name = efivar_namespaces_follow_the_capability,
    suite = authority
);
slopos_testing::stest!(
    name = efivar_boot_manager_writes_are_held_to_their_formats,
    suite = authority
);
