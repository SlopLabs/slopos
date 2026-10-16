use super::*;
use alloc::format;
use alloc::vec;

const SHIPPED: &str = include_str!("../../../assets/privd/actions");

const EXAMPLE: &str = "\
# a comment
action org.slopos.boot.install
  program  /bin/bootctl
  args     install *
  args     status
  caps     RawBlock Power
  uid      0
  cwd
  exclusive boot-slots
  allow    root
  allow    session consent
  allow    paired
  describe \"Install a kernel into a boot slot\"

action org.slopos.net.toggle
  program /bin/ip
  args net on
  caps NetAdmin
  uid 0
  allow program /bin/compositor
  allow session consent
  exclusive
  describe \"Turn networking on or off\"
";

fn args(words: &[&'static str]) -> Vec<&'static [u8]> {
    words.iter().map(|w| w.as_bytes()).collect()
}

#[test]
fn the_example_parses_into_what_it_says() {
    let policy = Policy::parse(EXAMPLE).unwrap();
    assert_eq!(policy.actions().len(), 2);
    let install = policy.action("org.slopos.boot.install").unwrap();
    assert_eq!(install.program, "/bin/bootctl");
    assert_eq!(install.caps, caps::RAW_BLOCK | caps::POWER);
    assert_eq!(install.uid, 0);
    assert!(install.cwd);
    assert_eq!(install.exclusive_group(), Some("boot-slots"));
    assert_eq!(install.describe, "Install a kernel into a boot slot");
    assert_eq!(
        install.shapes,
        vec![
            Shape {
                words: vec!["install".into()],
                open: true
            },
            Shape {
                words: vec!["status".into()],
                open: false
            },
        ]
    );
    assert_eq!(
        install.allows,
        vec![
            Allow {
                principal: Principal::Root,
                consent: false
            },
            Allow {
                principal: Principal::Session,
                consent: true
            },
            Allow {
                principal: Principal::Paired,
                consent: false
            },
        ]
    );
    let toggle = policy.action("org.slopos.net.toggle").unwrap();
    assert!(!toggle.cwd);
    assert_eq!(toggle.exclusive_group(), Some("org.slopos.net.toggle"));
    assert_eq!(policy.action("org.slopos.nothing"), None);
}

#[test]
fn shapes_match_fixed_words_then_anything_after_a_star() {
    let policy = Policy::parse(EXAMPLE).unwrap();
    let install = policy.action("org.slopos.boot.install").unwrap();
    assert!(install.accepts(&args(&["install"])));
    assert!(install.accepts(&args(&["install", "b", "/k", "/b"])));
    assert!(install.accepts(&args(&["status"])));
    assert!(!install.accepts(&args(&["status", "x"])));
    assert!(!install.accepts(&args(&[])));
    assert!(!install.accepts(&args(&["installx"])));
    assert!(!install.accepts(&args(&["--boot", "/dev/x", "install"])));
    assert!(!install.accepts(&[b"install\0".as_slice()]));
    let toggle = policy.action("org.slopos.net.toggle").unwrap();
    assert!(toggle.accepts(&args(&["net", "on"])));
    assert!(!toggle.accepts(&args(&["net", "off"])));
    assert!(!toggle.accepts(&args(&["net", "on", "now"])));
    assert!(!toggle.accepts(&args(&["net"])));
}

#[test]
fn the_first_matching_principal_decides() {
    let policy = Policy::parse(EXAMPLE).unwrap();
    let install = policy.action("org.slopos.boot.install").unwrap();
    let toggle = policy.action("org.slopos.net.toggle").unwrap();
    let root = Requester::new(0, Some("/bin/shell"), false);
    let root_outside = Requester::new(0, None, false);
    let session = Requester::new(SESSION_UID, Some("/bin/shell"), false);
    let outsider = Requester::new(SESSION_UID, None, false);
    let compositor = Requester::new(SESSION_UID, Some("/bin/compositor"), false);
    let paired = Requester::new(SESSION_UID, Some(REMOTED), true);
    let unpaired_remoted = Requester::new(SESSION_UID, Some(REMOTED), false);
    let stranger = Requester::new(4242, None, false);

    assert_eq!(install.admits(&root), Verdict::Run);
    assert_eq!(
        install.admits(&root_outside),
        Verdict::Refuse,
        "uid 0 outside the base is no principal"
    );
    assert_eq!(install.admits(&session), Verdict::Ask);
    assert_eq!(install.admits(&outsider), Verdict::Ask);
    assert_eq!(install.admits(&paired), Verdict::Run);
    assert_eq!(install.admits(&unpaired_remoted), Verdict::Ask);
    assert_eq!(install.admits(&stranger), Verdict::Refuse);

    assert_eq!(toggle.admits(&compositor), Verdict::Run);
    assert_eq!(
        toggle.admits(&Requester::new(4242, Some("/bin/compositor"), false)),
        Verdict::Run,
        "a program principal names the program, whatever runs it"
    );
    assert_eq!(toggle.admits(&session), Verdict::Ask);
    assert_eq!(toggle.admits(&root), Verdict::Refuse);
    assert_eq!(toggle.admits(&stranger), Verdict::Refuse);
    assert_eq!(
        toggle.admits(&paired),
        Verdict::Refuse,
        "a paired connection is only ever paired"
    );
}

#[test]
fn only_remoted_on_a_paired_base_is_paired() {
    assert!(Requester::new(SESSION_UID, Some(REMOTED), true).is_paired());
    assert!(Requester::new(0, Some(REMOTED), true).is_paired());
    assert!(!Requester::new(SESSION_UID, Some(REMOTED), false).is_paired());
    assert!(!Requester::new(SESSION_UID, Some("/bin/shell"), true).is_paired());
    assert!(!Requester::new(SESSION_UID, None, true).is_paired());
}

fn refusal(text: &str) -> PolicyError {
    Policy::parse(text).expect_err(text)
}

fn with(line: &str) -> String {
    format!(
        "action org.slopos.x\n  program /bin/x\n  args *\n  caps Power\n  uid 0\n  \
         allow root\n  describe \"x\"\n{line}\n"
    )
}

#[test]
fn every_rule_of_the_grammar_refuses() {
    for (text, msg) in [
        ("program /bin/x\n", "a setting outside an action"),
        (
            "action org\n",
            "an action is named by lowercase words joined by dots",
        ),
        (
            "action Org.x\n",
            "an action is named by lowercase words joined by dots",
        ),
        (
            "action org..x\n",
            "an action is named by lowercase words joined by dots",
        ),
        (
            "action org.1x\n",
            "an action is named by lowercase words joined by dots",
        ),
        (
            "action org.x y\n",
            "an action is named by lowercase words joined by dots",
        ),
        ("action org.x\n", "the action lists no args shape"),
    ] {
        assert_eq!(refusal(text).msg, msg, "{text}");
    }
    for (line, msg) in [
        ("  program /bin/y", "the setting is given twice"),
        ("  uid 0", "the setting is given twice"),
        ("  caps Power", "the setting is given twice"),
        ("  describe \"y\"", "the setting is given twice"),
        ("  cwd\n  cwd", "the setting is given twice"),
        ("  exclusive\n  exclusive", "the setting is given twice"),
        ("  medium\n  medium", "the setting is given twice"),
        ("  args *", "the shape is already listed"),
        ("  args * more", "* ends a shape"),
        ("  args a\"b", "an argument word is printable and unquoted"),
        ("  args a*b", "an argument word is printable and unquoted"),
        ("  allow root", "the principal is already allowed"),
        (
            "  allow root consent",
            "root and paired requesters are never asked about",
        ),
        (
            "  allow paired consent",
            "root and paired requesters are never asked about",
        ),
        (
            "  allow session maybe",
            "allow ends with consent or nothing",
        ),
        (
            "  allow session consent now",
            "allow ends with consent or nothing",
        ),
        (
            "  allow everyone",
            "allow names root, session, paired or program <path>",
        ),
        ("  allow program", "allow program takes a path"),
        (
            "  allow program bin/x",
            "allow program names a base program's canonical path",
        ),
        (
            "  allow program /tmp/x",
            "allow program names a base program's canonical path",
        ),
        ("  exclusive two words", "one word at most"),
        ("  frobnicate", "unknown setting"),
        ("  cwd please", "unknown setting"),
        ("  medium please", "unknown setting"),
    ] {
        assert_eq!(refusal(&with(line)).msg, msg, "{line}");
    }
    let bare = |omit: &str| {
        let full = "action org.slopos.x\n  program /bin/x\n  args *\n  caps Power\n  uid 0\n  \
                    allow root\n  describe \"x\"\n";
        full.lines()
            .filter(|l| !l.trim_start().starts_with(omit))
            .collect::<Vec<_>>()
            .join("\n")
    };
    for (omit, msg) in [
        ("program", "the action names no program"),
        ("caps", "the action names no caps"),
        ("uid", "the action names no uid"),
        ("describe", "the action has no describe"),
        ("allow", "the action allows nobody"),
    ] {
        assert_eq!(refusal(&bare(omit)).msg, msg, "{omit}");
    }
}

#[test]
fn values_outside_their_grammar_refuse() {
    for (line, setting) in [
        ("program /bin/x", "program"),
        ("program x", "program"),
        ("program /etc/x", "program"),
        ("program /bin/../x", "program"),
        ("program /bin//x", "program"),
        ("program /bin/x/", "program"),
        ("program /bin/./x", "program"),
        ("program /bin/x /bin/y", "program"),
        ("caps Launch", "caps"),
        ("caps SetId", "caps"),
        ("caps ConsoleIo", "caps"),
        ("caps Power Power", "caps"),
        ("uid -1", "uid"),
        ("uid +1", "uid"),
        ("uid 0x0", "uid"),
        ("uid 4294967296", "uid"),
        ("uid", "uid"),
        ("uid 0 0", "uid"),
        ("uid 1000", "uid 0"),
        ("describe x", "describe"),
        ("describe \"\"", "describe"),
        ("describe \"a\"b\"", "describe"),
        ("describe \"tab\there\"", "describe"),
        ("describe \"caf\u{e9}\"", "describe"),
    ] {
        let text = format!("action org.slopos.x\n  {}\n", line);
        let err = Policy::parse(&text);
        if setting == "program" && line == "program /bin/x" {
            assert_eq!(err.unwrap_err().msg, "the action lists no args shape");
            continue;
        }
        assert!(err.is_err(), "{line} parsed");
        assert_eq!(err.unwrap_err().line, 2, "{line}");
    }
    let long = format!("describe \"{}\"", "x".repeat(MAX_DESCRIBE + 1));
    assert_eq!(refusal(&with(&long)).line, 8);
    let long_name = format!("action org.{}\n", "x".repeat(MAX_NAME));
    assert_eq!(refusal(&long_name).line, 1);
}

#[test]
fn errors_name_the_line_they_are_on() {
    let text = "\n# comment\naction org.slopos.a\n  program /bin/a\n  bogus\n";
    assert_eq!(
        refusal(text),
        PolicyError {
            line: 5,
            msg: "unknown setting"
        }
    );
    let duplicate = format!("{}{}", with(""), with(""));
    assert_eq!(refusal(&duplicate).line, 9);
    // A missing setting is reported at the action that lacks it.
    let incomplete = format!("{}action org.slopos.y\n  args *\n", with(""));
    assert_eq!(refusal(&incomplete).line, 9);
}

#[test]
fn a_medium_action_exists_only_on_a_boot_of_the_medium() {
    let text = format!("{EXAMPLE}{}", with("  medium"));
    let policy = Policy::parse(&text).unwrap();
    assert!(policy.action("org.slopos.x").unwrap().medium);
    assert!(!policy.action("org.slopos.boot.install").unwrap().medium);
    let installed = policy.clone().for_boot(false);
    assert_eq!(installed.action("org.slopos.x"), None);
    assert_eq!(installed.actions().len(), 2);
    assert_eq!(policy.clone().for_boot(true), policy);
}

#[test]
fn every_capability_name_is_one_privd_holds() {
    for (name, bit) in CAP_NAMES {
        let policy =
            Policy::parse(&with("").replace("caps Power", &format!("caps {name}"))).unwrap();
        assert_eq!(policy.actions()[0].caps, bit);
        assert_eq!(bit & DELEGABLE, bit);
    }
    let all: Vec<&str> = CAP_NAMES.iter().map(|(n, _)| *n).collect();
    let policy =
        Policy::parse(&with("").replace("caps Power", &format!("caps {}", all.join(" ")))).unwrap();
    assert_eq!(policy.actions()[0].caps, DELEGABLE);
    let none = Policy::parse(&with("").replace("caps Power", "caps")).unwrap();
    assert_eq!(none.actions()[0].caps, 0);
}

/// Every prefix, every line dropped and every byte replaced: the parser
/// answers each, a policy or the line it stopped at, and never panics.
#[test]
fn mutated_policies_parse_or_name_a_line() {
    let lines = EXAMPLE.lines().count();
    let check = |text: &str| match Policy::parse(text) {
        Ok(policy) => assert!(
            policy
                .actions()
                .iter()
                .all(|a| !a.shapes.is_empty() && !a.allows.is_empty() && a.caps & !DELEGABLE == 0)
        ),
        Err(e) => assert!((1..=lines + 1).contains(&e.line), "{e} in {text:?}"),
    };
    for cut in 0..=EXAMPLE.len() {
        if EXAMPLE.is_char_boundary(cut) {
            check(&EXAMPLE[..cut]);
        }
    }
    let all: Vec<&str> = EXAMPLE.lines().collect();
    for skip in 0..all.len() {
        let text: Vec<&str> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != skip)
            .map(|(_, l)| *l)
            .collect();
        check(&text.join("\n"));
    }
    let bytes = EXAMPLE.as_bytes();
    for at in 0..bytes.len() {
        for b in [b' ', b'\n', b'"', b'*', b'#', b'x', b'.', 0u8, 0x7f] {
            let mut mutated = bytes.to_vec();
            mutated[at] = b;
            if let Ok(text) = core::str::from_utf8(&mutated) {
                check(text);
            }
        }
    }
}

/// The policy the base carries, held to the rules the access model states.
#[test]
fn the_shipped_policy_says_what_the_access_model_does() {
    let policy = Policy::parse(SHIPPED).unwrap();
    let root = Requester::new(0, Some("/bin/shell"), false);
    let session = Requester::new(SESSION_UID, Some("/bin/shell"), false);
    let paired = Requester::new(SESSION_UID, Some(REMOTED), true);
    let paired_list = [
        "org.slopos.boot.",
        "org.slopos.base.",
        "org.slopos.power.",
        "org.slopos.cpufreq.set",
        "org.slopos.kconsole",
        "org.slopos.prof",
        "org.slopos.console.keymap",
    ];
    for action in policy.actions() {
        assert_eq!(action.uid, 0, "{}", action.name);
        assert_eq!(action.admits(&root), Verdict::Run, "{}", action.name);
        assert_eq!(
            action.admits(&Requester::new(0, None, false)),
            Verdict::Refuse,
            "{}: uid 0 outside the base",
            action.name
        );
        assert_ne!(action.admits(&session), Verdict::Refuse, "{}", action.name);
        let pairable = paired_list.iter().any(|p| action.name.starts_with(p));
        assert_eq!(
            action.admits(&paired) == Verdict::Run,
            pairable,
            "{} and a pairing",
            action.name
        );
        for allow in &action.allows {
            if let Principal::Program(path) = &allow.principal {
                assert_eq!(path, "/bin/compositor", "a program principal is static");
            }
        }
    }
    let install = policy.action("org.slopos.install").unwrap();
    assert_eq!(install.program, "/bin/installer");
    assert_eq!(install.admits(&session), Verdict::Ask);
    assert_eq!(install.admits(&paired), Verdict::Refuse);
    assert!(install.exclusive_group().is_some());
    for (name, args) in [
        ("org.slopos.power.poweroff", &["halt"][..]),
        ("org.slopos.power.reboot", &["reboot"]),
        ("org.slopos.boot.install", &["install", "b", "/k"]),
        ("org.slopos.base.install", &["base", "install", "/b"]),
        ("org.slopos.base.rollback", &["base", "rollback"]),
        ("org.slopos.net.toggle", &["net", "off"]),
        ("org.slopos.net.configure", &["link", "set", "lo", "up"]),
        ("org.slopos.cpufreq.set", &["set", "hwp"]),
    ] {
        assert!(
            policy.action(name).unwrap().accepts(&args_of(args)),
            "{name}"
        );
    }
    for (name, args) in [
        ("org.slopos.power.reboot", &["halt"][..]),
        ("org.slopos.base.rollback", &["base", "rollback", "/b"]),
        ("org.slopos.base.install", &["install", "b", "/k"]),
        ("org.slopos.boot.install", &["base", "install", "/b"]),
    ] {
        assert!(
            !policy.action(name).unwrap().accepts(&args_of(args)),
            "{name} {args:?}"
        );
    }
    let slots = policy
        .action("org.slopos.boot.install")
        .unwrap()
        .exclusive_group();
    for name in ["org.slopos.base.install", "org.slopos.base.rollback"] {
        let action = policy.action(name).unwrap();
        assert_eq!(action.exclusive_group(), slots, "{name}");
        assert_ne!(
            action.caps & slopos_abi::caps::BASE_UPDATE,
            0,
            "{name} runs the base it writes"
        );
    }
    let toggle = policy.action("org.slopos.net.toggle").unwrap();
    assert_eq!(
        toggle.admits(&Requester::new(SESSION_UID, Some("/bin/compositor"), false)),
        Verdict::Run
    );
    for name in [
        "org.slopos.boot.install",
        "org.slopos.base.install",
        "org.slopos.base.rollback",
        "org.slopos.boot.set-default",
        "org.slopos.net.configure",
    ] {
        assert_eq!(
            policy.action(name).unwrap().admits(&session),
            Verdict::Ask,
            "{name}"
        );
    }
    let installed = policy.clone().for_boot(false);
    let medium: Vec<&str> = policy
        .actions()
        .iter()
        .filter(|a| installed.action(&a.name).is_none())
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(medium, ["org.slopos.rescue"]);
    let rescue = policy.action("org.slopos.rescue").unwrap();
    assert_eq!(rescue.program, "/bin/shell");
    assert_eq!(rescue.caps, DELEGABLE);
    assert_eq!(rescue.admits(&session), Verdict::Ask);
    assert!(rescue.accepts(&args_of(&["--rescue"])));
    assert!(!rescue.accepts(&args_of(&["--rescue", "-c", "id"])));
}

fn args_of<'a>(words: &[&'a str]) -> Vec<&'a [u8]> {
    words.iter().map(|w| w.as_bytes()).collect()
}
