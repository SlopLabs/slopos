use super::*;
use std::format;

#[test]
fn a_label_is_capitalised_and_unstopped() {
    for good in ["Log out…", "Erase disk and install", "Ready to install"] {
        assert_eq!(label(good, &[]), Ok(()), "{good}");
    }
    for bad in ["log out", "Log out.", "Log out...", "", "Reboot now"] {
        assert!(label(bad, &[]).is_err(), "{bad:?}");
    }
}

#[test]
fn a_sentence_is_capitalised_and_stopped() {
    for good in [
        "This disk is empty. Choose Erase disk and install instead.",
        "Restart the computer now?",
    ] {
        assert_eq!(sentence(good, &[]), Ok(()), "{good}");
    }
    for bad in ["this disk is empty.", "This disk is empty", ""] {
        assert!(sentence(bad, &[]).is_err(), "{bad:?}");
    }
}

#[test]
fn the_glossary_s_words_stay_off_the_screen() {
    for (term, instead) in GLOSSARY {
        let said = format!("Use the {term} now");
        assert!(plain(&said, &[]).is_err(), "{said}");
        assert_eq!(
            plain(&format!("Use the {instead} now"), &[]),
            Ok(()),
            "{instead}"
        );
    }
    for bad in [
        "Two machines",
        "The drive's label",
        "Switch Person",
        "Rebooting now",
        "It rebooted",
        "The computer halted",
        "Shutting down",
        "Shut down",
        "Two firmware entries",
        "Pass --mode erase",
        "Wait...",
    ] {
        assert!(plain(bad, &[]).is_err(), "{bad:?}");
    }
    for good in [
        "Shut the lid and go down",
        "Switch to this person",
        "A driver",
        "Programmable keys",
    ] {
        assert_eq!(plain(good, &[]), Ok(()), "{good}");
    }
}

#[test]
fn a_surface_keeps_its_trade_to_its_details() {
    assert!(plain("Copy the toolchain", &["toolchain"]).is_err());
    assert!(plain("Two toolchains", &["toolchain"]).is_err());
    assert_eq!(plain("Copy the developer tools", &["toolchain"]), Ok(()));
}

#[test]
fn a_size_says_its_unit_and_one_decimal_at_most() {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * KIB;
    for (bytes, said) in [
        (0, "0 bytes"),
        (1, "1 byte"),
        (1023, "1023 bytes"),
        (KIB, "1 KiB"),
        (1536, "1.5 KiB"),
        (8 * MIB, "8 MiB"),
        (MIB - 52, "1023.9 KiB"),
        (MIB - 51, "1 MiB"),
        (5 << 30, "5 GiB"),
        (3 << 40, "3 TiB"),
        (2048 << 40, "2048 TiB"),
        (u64::MAX, "16777216 TiB"),
    ] {
        assert_eq!(format!("{}", Bytes(bytes)), said, "{bytes}");
    }
}
