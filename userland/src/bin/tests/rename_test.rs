//! A name `rename(2)` replaces stays visible throughout, as POSIX has it: an
//! open of it reaches the file it named or the one that took its place, never
//! neither. Every file rewritten whole rests on that — `/run/sessions`, the
//! user database — so it is held on a RAM filesystem and on the disk.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const ROUNDS: usize = 400;

fn note(why: &str) {
    eprintln!("rename_test: {why}");
}

/// Replace `dir/name` [`ROUNDS`] times through a sibling while another thread
/// opens and reads it.
fn replaced_while_read(dir: &str) -> bool {
    let _ = std::fs::remove_dir_all(dir);
    let target = format!("{dir}/name");
    let sibling = format!("{dir}/.name");
    if std::fs::create_dir_all(dir).is_err() || std::fs::write(&target, b"0").is_err() {
        note(&format!("{dir}: could not lay out the fixture"));
        return false;
    }
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let (done, target) = (done.clone(), target.clone());
        std::thread::spawn(move || {
            let (mut whole, mut missing, mut torn) = (0u32, 0u32, 0u32);
            let mut why = None;
            while !done.load(Ordering::Acquire) {
                match std::fs::read(&target) {
                    Ok(bytes) if bytes.len() == 1 => whole += 1,
                    Ok(_) => torn += 1,
                    Err(error) => {
                        missing += 1;
                        why = Some(error);
                    }
                }
            }
            (whole, missing, torn, why)
        })
    };
    let replaced = (0..ROUNDS)
        .filter(|round| {
            std::fs::write(&sibling, [b'0' + (round % 10) as u8]).is_ok()
                && std::fs::rename(&sibling, &target).is_ok()
        })
        .count();
    done.store(true, Ordering::Release);
    let read = reader.join();
    let _ = std::fs::remove_dir_all(dir);
    let Ok((whole, missing, torn, why)) = read else {
        note(&format!("{dir}: the reader panicked"));
        return false;
    };
    if replaced != ROUNDS || whole == 0 || missing != 0 || torn != 0 {
        note(&format!(
            "{dir}: {replaced} of {ROUNDS} replaced; read {whole} whole, {missing} missing \
             ({why:?}), {torn} short"
        ));
        return false;
    }
    true
}

fn a_replaced_name_stays_visible_on_a_ram_filesystem() -> bool {
    replaced_while_read("/run/rename_test")
}

fn a_replaced_name_stays_visible_on_the_disk() -> bool {
    replaced_while_read("/var/tmp/rename_test")
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "a_replaced_name_stays_visible_on_a_ram_filesystem",
        a_replaced_name_stays_visible_on_a_ram_filesystem,
    ),
    (
        "a_replaced_name_stays_visible_on_the_disk",
        a_replaced_name_stays_visible_on_the_disk,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
