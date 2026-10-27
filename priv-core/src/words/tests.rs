use super::*;
use alloc::string::ToString;

#[test]
fn a_describe_completes_wants_to() {
    for good in [
        "install SlopOS on a disk",
        "power off the computer",
        "change your password",
        "run a command that can read and change every file",
    ] {
        assert_eq!(describe(good), Ok(()), "{good}");
    }
    for bad in [
        "Install SlopOS on a disk",
        "install SlopOS on a disk.",
        "restart the computer?",
        "",
        "2 disks",
        &"x".repeat(DESCRIBE_MAX + 1),
    ] {
        assert!(describe(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_detail_is_one_sentence() {
    assert_eq!(
        detail("Everything on the disk you choose will be deleted."),
        Ok(())
    );
    for bad in [
        "everything on it will be deleted.",
        "Everything on it will be deleted",
        &format!("A{}.", "x".repeat(DETAIL_MAX)),
    ] {
        assert!(detail(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_label_is_capitalised_and_unstopped() {
    for good in [
        "Log out…",
        "Restart the desktop to finish updating",
        "Power off the computer?",
    ] {
        assert_eq!(label(good), Ok(()), "{good}");
    }
    for bad in [
        "log out",
        "Log out.",
        "Log out...",
        "Switch person",
        "Reboot now",
        "",
    ] {
        assert!(label(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_verb_is_one_the_dialog_knows() {
    for good in ["Install", "Power off", "Go back", "Allow"] {
        assert_eq!(verb(good), Ok(()), "{good}");
    }
    for bad in [
        "install",
        "Install.",
        "Power  off",
        "",
        "Cancel",
        "CANCEL",
        "Show Details",
        "Show more",
        "Deny",
        "Disallow",
        "Block",
        "Stop",
        "Details",
        "Help",
    ] {
        assert!(verb(bad).is_err(), "{bad:?}");
    }
}

/// Every verb fits the button, is capitalised and plain, and says what no
/// other control of the dialog says, in any case.
#[test]
fn every_verb_is_its_own_button() {
    for said in VERBS {
        assert!(said.len() <= VERB_MAX, "{said}");
        assert!(said.starts_with(|c: char| c.is_ascii_uppercase()), "{said}");
        assert_eq!(plain(said), Ok(()), "{said}");
        for label in [CANCEL, SHOW_DETAILS, SHOW_LESS] {
            assert!(!label.eq_ignore_ascii_case(said), "{said}");
        }
    }
}

#[test]
fn the_policy_s_vocabulary_stays_off_the_screen() {
    for bad in [
        "install a kernel into a boot slot",
        "replace the base every boot slot boots",
        "restart the machine",
        "reboot the computer",
        "change the boot order",
        "shut down the computer",
        "let the requester choose",
        "write a disk with RawBlock",
        "give it Mount",
        "run a program",
        "change the startup order -- now",
        "wait...",
        "change what the admins may do",
    ] {
        assert!(describe(bad).is_err(), "{bad:?}");
    }
    assert!(detail("It can mount disks and set the clock.").is_ok());
    assert!(detail("It holds Mount and Clock.").is_err());
    assert!(detail("RawBlock lets it write any disk.").is_err());
    assert!(detail("Power runs out at night.").is_ok());
    assert!(verb("Power off").is_ok());
    assert!(describe("update the database").is_ok());
}

/// What privd says back is held to the same rules a detail is: sentences
/// addressed to the person, none of the policy's words.
#[test]
fn every_refusal_speaks_to_the_person() {
    use crate::Refused;
    use crate::consent::Refusal;
    let refusals = [
        Refusal::NoAgent,
        Refusal::Outdated,
        Refusal::Busy,
        Refusal::Declined,
        Refusal::Refused,
        Refusal::Expired,
        Refusal::Withdrawn,
        Refusal::Interrupted,
    ]
    .map(|r| r.to_string());
    let refused = [
        Refused::NotAllowed,
        Refused::NoAdminPassword,
        Refused::Away,
        Refused::Unreached,
        Refused::NoPasswordToType,
        Refused::AdminAtTerminal,
    ]
    .map(|r| r.to_string());
    let privd = [
        privd::LAPSED,
        privd::RESTARTING,
        privd::NO_ACTION,
        privd::NOT_THAT_WAY,
        privd::NO_FOLDER,
        privd::UNWANTED_FOLDER,
        privd::FOLDER_GONE,
        privd::NOT_A_FOLDER,
        privd::FOLDER_MOVED,
        privd::UNREADABLE,
        privd::CROWDED,
        privd::BUSY,
        privd::INHERITED,
        privd::TOO_LONG,
        privd::INCOMPLETE,
        privd::UNCHECKED,
        privd::ANSWERED_ELSEWHERE,
        privd::NO_ACCOUNT,
        privd::EXTENSION_CHANGED,
        privd::FAULT,
        crate::consent::WRONG_PASSWORD,
    ]
    .map(str::to_string);
    for said in refusals.iter().chain(&refused).chain(&privd) {
        assert_eq!(detail(said), Ok(()), "{said}");
        assert!(!said.contains("person at the machine"), "{said}");
    }
}
