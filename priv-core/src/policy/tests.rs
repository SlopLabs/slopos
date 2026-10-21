use super::*;
use alloc::format;
use alloc::vec;
use slopos_abi::caps;
use slopos_abi::fs::SESSION_UID;

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
  keep     300
  allow    root
  allow    admin confirm
  allow    paired
  describe \"Install a kernel into a boot slot\"

action org.slopos.net.toggle
  program /bin/ip
  args net on
  caps NetAdmin
  uid 0
  allow program /bin/compositor
  allow user password
  exclusive
  describe \"Turn networking on or off\"
";

fn args(words: &[&'static str]) -> Vec<&'static [u8]> {
    words.iter().map(|w| w.as_bytes()).collect()
}

const USER: u32 = SESSION_UID;
const ALICE: u32 = 1001;
const BOB: u32 = 1002;
const CAROL: u32 = 1003;

fn person(uid: u32, name: &str, admin: bool, password: bool) -> Person {
    Person {
        uid,
        name: name.into(),
        admin,
        password,
    }
}

/// The one person a fresh install has: an administrator with no password,
/// logged in at the desktop in front.
fn one_person() -> People {
    People {
        people: vec![person(USER, "user", true, false)],
        logged_in: vec![USER],
    }
}

/// Beside the first person, an administrator with a password, a standard
/// user with one and a standard user without, nobody logged in.
fn four() -> People {
    People {
        people: vec![
            person(USER, "user", true, false),
            person(ALICE, "alice", true, true),
            person(BOB, "bob", false, true),
            person(CAROL, "carol", false, false),
        ],
        logged_in: Vec::new(),
    }
}

/// A shell of `uid`'s in the session in front, the first person's desktop.
fn shell(uid: u32) -> Requester<'static> {
    at(uid, USER)
}

/// A shell of `uid`'s in `front`'s desktop, which is in front.
fn at(uid: u32, front: u32) -> Requester<'static> {
    Requester::new(uid, Some("/bin/shell"), false).reached(Reach::Screen {
        theirs: uid == front,
    })
}

fn asks(verdict: Verdict) -> Option<(Kind, String, Vec<String>)> {
    match verdict {
        Verdict::Ask(q) => Some((q.kind, q.person.name, q.admins)),
        _ => None,
    }
}

const NO: Verdict = Verdict::Refuse(Refused::NotAllowed);

#[test]
fn the_example_parses_into_what_it_says() {
    let policy = Policy::parse(EXAMPLE).unwrap();
    assert_eq!(policy.actions().len(), 2);
    let install = policy.action("org.slopos.boot.install").unwrap();
    assert_eq!(install.program, "/bin/bootctl");
    assert_eq!(install.caps, caps::RAW_BLOCK | caps::POWER);
    assert_eq!(install.runs_as, RunsAs::Uid(0));
    assert!(install.cwd);
    assert_eq!(install.exclusive_group(), Some("boot-slots"));
    assert_eq!(install.keep, Some(300));
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
                level: Level::None
            },
            Allow {
                principal: Principal::Admin,
                level: Level::Confirm
            },
            Allow {
                principal: Principal::Paired,
                level: Level::None
            },
        ]
    );
    let toggle = policy.action("org.slopos.net.toggle").unwrap();
    assert!(!toggle.cwd);
    assert_eq!(toggle.keep, None);
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
    let people = one_person();
    let root = shell(0);
    let root_outside = Requester::new(0, None, false);
    let session = shell(USER);
    let outsider = Requester::new(USER, None, false).reached(Reach::Screen { theirs: true });
    let compositor = Requester::new(USER, Some("/bin/compositor"), false);
    let paired = Requester::new(USER, Some(REMOTED), true).reached(Reach::Screen { theirs: true });
    let unpaired_remoted =
        Requester::new(USER, Some(REMOTED), false).reached(Reach::Screen { theirs: true });
    let stranger = Requester::new(4242, None, false);
    let confirm = Some((Kind::Confirm, "user".into(), vec![]));

    assert_eq!(install.admits(&root, &people), Verdict::Run);
    assert_eq!(
        install.admits(&root_outside, &people),
        NO,
        "uid 0 outside the base is no principal"
    );
    assert_eq!(asks(install.admits(&session, &people)), confirm);
    assert_eq!(asks(install.admits(&outsider, &people)), confirm);
    assert_eq!(install.admits(&paired, &people), Verdict::Run);
    assert_eq!(asks(install.admits(&unpaired_remoted, &people)), confirm);
    assert_eq!(install.admits(&stranger, &people), NO);
    assert_eq!(
        install.admits(&session, &People::default()),
        NO,
        "a person is one the accounts name"
    );

    assert_eq!(toggle.admits(&compositor, &people), Verdict::Run);
    assert_eq!(
        toggle.admits(
            &Requester::new(4242, Some("/bin/compositor"), false),
            &people
        ),
        Verdict::Run,
        "a program principal names the program, whatever runs it"
    );
    assert_eq!(
        asks(toggle.admits(&session, &people)),
        confirm,
        "a person without a password confirms where another would type one"
    );
    assert_eq!(toggle.admits(&root, &people), NO);
    assert_eq!(toggle.admits(&stranger, &people), NO);
    assert_eq!(
        toggle.admits(&paired, &people),
        NO,
        "a paired connection is only ever paired"
    );
}

/// `uid requester` runs a mechanism as whoever asked, which only a person or
/// root may be, holding no more than the ceiling.
#[test]
fn an_action_run_as_its_requester_stays_under_the_ceiling() {
    let text = |caps: &str, allows: &str| {
        format!(
            "action org.slopos.x\n  program /bin/x\n  args *\n  caps {caps}\n  uid requester\n\
             {allows}  describe \"x\"\n"
        )
    };
    let all = "Mount NetAdmin ConsoleConfig SysInspect Clock DacOverride";
    let parsed = Policy::parse(&text(
        all,
        "  allow root\n  allow user\n  allow admin confirm\n",
    ))
    .unwrap();
    let action = &parsed.actions()[0];
    assert_eq!(action.runs_as, RunsAs::Requester);
    assert_eq!(action.caps, crate::REQUESTER_CEILING);
    for beyond in [
        "Power",
        "RawBlock",
        "Seal",
        "BootEntry",
        "BaseUpdate",
        "ExtensionUpdate",
    ] {
        assert_eq!(
            refusal(&text(&format!("Mount {beyond}"), "  allow root\n")).msg,
            "an action run as its requester holds no more than the ceiling",
            "{beyond}"
        );
    }
    for allow in ["  allow paired\n", "  allow program /bin/compositor\n"] {
        assert_eq!(
            refusal(&text("Mount", &format!("  allow admin\n{allow}"))).msg,
            "an action run as its requester allows only root, user and admin",
            "{allow}"
        );
    }
}

#[test]
fn the_ceiling_reaches_no_disk_base_extension_seal_boot_variable_or_identity() {
    use crate::REQUESTER_CEILING;
    assert_eq!(REQUESTER_CEILING & !DELEGABLE, 0);
    for never in [
        caps::POWER,
        caps::RAW_BLOCK,
        caps::BASE_UPDATE,
        caps::EXTENSION_UPDATE,
        caps::SEAL,
        caps::BOOT_ENTRY,
        caps::SET_ID,
        caps::PROC_SIGNAL,
    ] {
        assert_eq!(REQUESTER_CEILING & never, 0, "{never:#x}");
    }
}

/// `priv` runs anything an administrator asks for once they click, keeping
/// the answer for that command and directory but never for a shell, whose
/// input says what it runs, as them and holding the ceiling; a standard
/// user's is answered by an administrator's password, and no program or
/// pairing reaches it.
#[test]
fn the_shipped_priv_is_an_administrator_s_and_keeps_only_a_command_s_answer() {
    let policy = Policy::parse(SHIPPED).unwrap();
    let shell_action = policy.action("org.slopos.priv.shell").unwrap();
    assert_eq!(shell_action.keep, None);
    assert!(shell_action.accepts(&args(&["-s"])));
    assert!(!shell_action.accepts(&args(&["-s", "-c", "id"])));
    let action = policy.action("org.slopos.priv").unwrap();
    for action in [action, shell_action] {
        assert_eq!(action.program, "/bin/priv");
        assert_eq!(action.runs_as, RunsAs::Requester);
        assert_eq!(action.caps, crate::REQUESTER_CEILING);
        assert!(action.cwd);
        assert_eq!(action.exclusive_group(), None);
        assert_eq!(action.allows, shell_action.allows);
    }
    assert_eq!(action.keep, Some(300));
    assert!(action.accepts(&args(&["/bin/ls", "-l"])));
    assert!(
        action.accepts(&args(&["-s"])),
        "the copy privd starts, not the policy, refuses a shell asked for as a command"
    );
    assert_eq!(action.admits(&shell(0), &one_person()), Verdict::Run);
    assert_eq!(
        asks(action.admits(&shell(USER), &one_person())),
        Some((Kind::Confirm, "user".into(), vec![]))
    );
    assert_eq!(
        asks(action.admits(&shell(BOB), &four())),
        Some((Kind::Admin, "bob".into(), vec!["alice".into()]))
    );
    assert_eq!(
        asks(action.admits(&shell(ALICE), &four())),
        Some((Kind::Password, "alice".into(), vec![])),
        "an administrator away from their own screen types their password"
    );
    let paired = Requester::new(USER, Some(REMOTED), true);
    assert_eq!(action.admits(&paired, &one_person()), NO);
    let compositor = Requester::new(USER, Some("/bin/compositor"), false)
        .reached(Reach::Screen { theirs: true });
    assert_eq!(
        asks(action.admits(&compositor, &one_person())).map(|(kind, ..)| kind),
        Some(Kind::Confirm),
        "any program a person runs is the person"
    );
}

fn action(allows: &str) -> Action {
    let text = format!(
        "action org.slopos.x\n  program /bin/x\n  args *\n  caps Power\n  uid 0\n{allows}  \
         describe \"x\"\n"
    );
    Policy::parse(&text).unwrap().actions()[0].clone()
}

#[test]
fn only_a_person_s_line_reads_the_people() {
    assert!(!action("  allow root\n  allow paired\n").names_people());
    assert!(action("  allow root\n  allow admin\n").names_people());
    assert!(action("  allow user alone\n").names_people());
}

#[test]
fn a_person_answers_for_themselves_by_a_click_at_their_screen_or_their_password() {
    let people = four();
    let confirm = action("  allow user confirm\n");
    let password = action("  allow user password\n");
    let unasked = action("  allow user\n");
    let mine = |kind, name: &str| Some((kind, name.into(), vec![]));
    assert_eq!(
        asks(confirm.admits(&shell(USER), &people)),
        mine(Kind::Confirm, "user")
    );
    assert_eq!(
        asks(confirm.admits(&shell(BOB), &people)),
        mine(Kind::Password, "bob"),
        "a click at another's screen answers for nobody else"
    );
    assert_eq!(
        confirm.admits(&shell(CAROL), &people),
        Verdict::Refuse(Refused::Away("carol".into()))
    );
    assert_eq!(
        asks(password.admits(&shell(USER), &people)),
        mine(Kind::Confirm, "user"),
        "without a password, a click where it would be typed"
    );
    assert_eq!(
        asks(password.admits(&shell(ALICE), &people)),
        mine(Kind::Password, "alice")
    );
    assert_eq!(
        asks(password.admits(&shell(BOB), &people)),
        mine(Kind::Password, "bob")
    );
    assert_eq!(
        password.admits(&shell(CAROL), &people),
        Verdict::Refuse(Refused::Away("carol".into()))
    );
    assert_eq!(
        asks(confirm.admits(&at(ALICE, ALICE), &people)),
        mine(Kind::Confirm, "alice")
    );
    assert_eq!(
        asks(confirm.admits(&at(USER, ALICE), &people)),
        None,
        "the first person has no password to type at alice's screen"
    );
    for uid in [USER, ALICE, BOB, CAROL] {
        assert_eq!(unasked.admits(&shell(uid), &people), Verdict::Run);
    }
}

#[test]
fn a_standard_user_at_an_admin_line_is_answered_by_an_administrator_s_password() {
    let admin = action("  allow admin\n");
    let confirm = action("  allow admin confirm\n");
    let password = action("  allow admin password\n");
    let people = four();
    let by_alice = Some((Kind::Admin, "bob".into(), vec!["alice".into()]));
    for action in [&admin, &confirm, &password] {
        assert_eq!(asks(action.admits(&shell(BOB), &people)), by_alice);
        assert_eq!(
            asks(action.admits(&shell(CAROL), &people)).map(|(k, _, a)| (k, a)),
            Some((Kind::Admin, vec!["alice".into()]))
        );
    }
    assert_eq!(admin.admits(&shell(USER), &people), Verdict::Run);
    assert_eq!(admin.admits(&shell(ALICE), &people), Verdict::Run);
    assert_eq!(
        asks(confirm.admits(&shell(USER), &people)).map(|(k, ..)| k),
        Some(Kind::Confirm)
    );
    assert_eq!(
        asks(password.admits(&shell(ALICE), &people)).map(|(k, ..)| k),
        Some(Kind::Password)
    );
    let no_password = People {
        people: vec![
            person(USER, "user", true, false),
            person(BOB, "bob", false, true),
        ],
        ..four()
    };
    assert_eq!(
        admin.admits(&shell(BOB), &no_password),
        Verdict::Refuse(Refused::NoAdminPassword),
        "an administrator without a password answers for nobody else"
    );
    let many = People {
        people: (0..40)
            .map(|i| person(2000 + i, &format!("admin{i}"), true, true))
            .chain([person(BOB, "bob", false, true)])
            .collect(),
        ..four()
    };
    let listed = asks(admin.admits(&shell(BOB), &many)).map(|(_, _, a)| a.len());
    assert_eq!(listed, Some(crate::consent::MAX_ADMINS));
    let unlisted = action("  allow root\n");
    assert_eq!(unlisted.admits(&shell(BOB), &people), NO);
    let user_first = action("  allow admin password\n  allow user\n");
    assert_eq!(user_first.admits(&shell(BOB), &people), Verdict::Run);
    assert_eq!(
        asks(user_first.admits(&shell(ALICE), &people)).map(|(k, ..)| k),
        Some(Kind::Password)
    );
}

#[test]
fn alone_runs_unasked_only_while_nobody_else_is_logged_in() {
    let alone = action("  allow user alone\n  allow admin\n");
    let bare = action("  allow user alone\n");
    let logged_in = |uids: &[u32]| People {
        logged_in: uids.to_vec(),
        ..four()
    };
    for (people, bob) in [
        (logged_in(&[]), Verdict::Run),
        (logged_in(&[BOB]), Verdict::Run),
        (logged_in(&[BOB, BOB]), Verdict::Run),
    ] {
        assert_eq!(alone.admits(&shell(BOB), &people), bob);
    }
    let others = logged_in(&[USER, BOB]);
    let by_alice = Some((Kind::Admin, "bob".into(), vec!["alice".into()]));
    assert_eq!(asks(alone.admits(&shell(BOB), &others)), by_alice);
    assert_eq!(asks(bare.admits(&shell(BOB), &others)), by_alice);
    assert_eq!(alone.admits(&shell(USER), &others), Verdict::Run);
    assert_eq!(bare.admits(&shell(USER), &others), Verdict::Run);
    let without = People {
        people: vec![
            person(USER, "user", true, false),
            person(BOB, "bob", false, true),
        ],
        logged_in: vec![USER, BOB],
    };
    assert_eq!(
        alone.admits(&shell(BOB), &without),
        Verdict::Refuse(Refused::NoAdminPassword),
        "a power-off while another is logged in, with no administrator's password to type"
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

#[test]
fn an_extension_s_program_is_a_principal_only_of_its_own_activation() {
    use slopos_abi::extension::Label;
    let client = "/opt/hello/bin/client";
    let label = Label::new(b"hello", [7; 32]).unwrap();
    let mut policy = Policy::parse(EXAMPLE).unwrap();
    let mut greet = policy.action("org.slopos.net.toggle").unwrap().clone();
    greet.name = "ext.hello.greet".into();
    greet.allows = vec![Allow {
        principal: Principal::Extension(client.into(), label),
        level: Level::None,
    }];
    assert!(policy.add(greet.clone()));
    assert!(!policy.add(greet), "one name is one action");
    let greet = policy.action("ext.hello.greet").unwrap();
    let people = one_person();
    for uid in [SESSION_UID, 0, 4242] {
        assert_eq!(
            greet.admits(&Requester::extension(uid, client, label), &people),
            Verdict::Run
        );
    }
    for other in [
        Requester::extension(SESSION_UID, client, Label::new(b"hello", [8; 32]).unwrap()),
        Requester::extension(SESSION_UID, client, Label::new(b"other", [7; 32]).unwrap()),
        Requester::extension(SESSION_UID, "/opt/hello/bin/x", label),
        Requester::new(SESSION_UID, Some(client), false),
        Requester::new(SESSION_UID, None, false),
    ] {
        assert_eq!(greet.admits(&other, &people), NO, "{other:?}");
    }
    let install = policy.action("org.slopos.boot.install").unwrap();
    assert_eq!(
        install.admits(&Requester::extension(0, client, label), &people),
        NO,
        "uid 0 running an extension's program is no root"
    );
    assert_eq!(
        asks(
            install.admits(
                &Requester::extension(SESSION_UID, client, label)
                    .reached(Reach::Screen { theirs: true }),
                &people
            )
        )
        .map(|(k, ..)| k),
        Some(Kind::Confirm),
        "a person running an extension's program is that person"
    );
    assert!(!Requester::extension(SESSION_UID, REMOTED, label).is_paired());
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
        ("  uid requester", "the setting is given twice"),
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
            "  allow user\n  allow user confirm",
            "the principal is already allowed",
        ),
        (
            "  allow root confirm",
            "the level is not one that principal takes",
        ),
        (
            "  allow paired password",
            "the level is not one that principal takes",
        ),
        (
            "  allow admin alone",
            "the level is not one that principal takes",
        ),
        (
            "  allow program /bin/x confirm",
            "the level is not one that principal takes",
        ),
        (
            "  allow user consent",
            "the level is not one that principal takes",
        ),
        ("  allow user confirm now", "allow ends with its level"),
        (
            "  allow session",
            "allow names root, greeter, user, admin, paired or program <path>",
        ),
        (
            "  allow everyone",
            "allow names root, greeter, user, admin, paired or program <path>",
        ),
        (
            "  keep 0",
            "keep takes the seconds an answer is kept, 1 to 900",
        ),
        (
            "  keep 901",
            "keep takes the seconds an answer is kept, 1 to 900",
        ),
        (
            "  keep +5",
            "keep takes the seconds an answer is kept, 1 to 900",
        ),
        (
            "  keep 5 6",
            "keep takes the seconds an answer is kept, 1 to 900",
        ),
        ("  keep 5\n  keep 6", "the setting is given twice"),
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
        ("uid Requester", "uid"),
        ("uid requester x", "uid"),
        ("uid requester requester", "uid"),
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
    for (name, bit) in crate::CAPABILITIES {
        let parsed = Policy::parse(&with("").replace("caps Power", &format!("caps {name}")));
        if bit & DELEGABLE == 0 {
            assert!(parsed.is_err(), "{name} is no capability privd holds");
            continue;
        }
        assert_eq!(parsed.unwrap().actions()[0].caps, bit);
    }
    let all: Vec<&str> = crate::capability_names(DELEGABLE).collect();
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
    let people = one_person();
    let root = shell(0);
    let session = shell(USER);
    let paired = Requester::new(USER, Some(REMOTED), true);
    let paired_list = [
        "org.slopos.boot.",
        "org.slopos.base.",
        "org.slopos.power.",
        "org.slopos.cpufreq.set",
        "org.slopos.kconsole",
        "org.slopos.prof",
        "org.slopos.console.keymap",
        "org.slopos.extension.",
        "org.slopos.software.install",
    ];
    let login = policy.action("org.slopos.login").unwrap();
    let greeter = Requester::new(slopos_abi::fs::GREETER_UID, Some("/bin/compositor"), false);
    assert_eq!(login.admits(&greeter, &people), Verdict::Run);
    assert_eq!(
        login.admits(&session, &people),
        NO,
        "a person logs in only at the login screen"
    );
    let unlock = "org.slopos.session.unlock";
    assert_eq!(
        policy.action(unlock).unwrap().admits(&session, &people),
        NO,
        "only a desktop's compositor asks to unlock it"
    );
    for action in policy
        .actions()
        .iter()
        .filter(|a| a.name != login.name && a.name != unlock)
    {
        let greets = action.admits(&greeter, &People::default()) == Verdict::Run;
        assert_eq!(
            greets,
            action.name.ends_with(".reboot") || action.name.ends_with(".poweroff"),
            "{}: the login screen restarts or powers off a machine nobody is logged in to",
            action.name
        );
        assert_eq!(action.admits(&greeter, &people), NO, "{}", action.name);
    }
    for action in policy.actions() {
        let requester = action.program == "/bin/priv";
        assert_eq!(
            action.runs_as,
            if requester {
                RunsAs::Requester
            } else {
                RunsAs::Uid(0)
            },
            "{}",
            action.name
        );
        assert_eq!(
            action.admits(&root, &people),
            Verdict::Run,
            "{}",
            action.name
        );
        assert_eq!(
            action.admits(&Requester::new(0, None, false), &people),
            NO,
            "{}: uid 0 outside the base",
            action.name
        );
        assert!(
            action.name == login.name
                || action.name == unlock
                || matches!(
                    action.admits(&session, &people),
                    Verdict::Run
                        | Verdict::Ask(Question {
                            kind: Kind::Confirm,
                            ..
                        })
                ),
            "{}: the one person runs it or clicks",
            action.name
        );
        let pairable = paired_list.iter().any(|p| action.name.starts_with(p));
        assert_eq!(
            action.admits(&paired, &people) == Verdict::Run,
            pairable,
            "{} and a pairing",
            action.name
        );
        assert!(
            action.name == unlock
                || !action
                    .allows
                    .iter()
                    .any(|a| matches!(a.principal, Principal::Program(_))),
            "{}: every person's compositor is one program",
            action.name
        );
        assert_eq!(
            action.keep,
            (action.name == "org.slopos.priv").then_some(300),
            "{}: only priv's commands keep an answer",
            action.name
        );
    }
    let install = policy.action("org.slopos.install").unwrap();
    assert_eq!(install.program, "/bin/installer");
    assert!(asks(install.admits(&session, &people)).is_some());
    assert_eq!(install.admits(&paired, &people), NO);
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
    let compositor = Requester::new(USER, Some("/bin/compositor"), false);
    let toggle = policy.action("org.slopos.net.toggle").unwrap();
    assert_eq!(toggle.admits(&compositor, &people), Verdict::Run);
    for name in [
        "org.slopos.boot.install",
        "org.slopos.base.install",
        "org.slopos.base.rollback",
        "org.slopos.boot.set-default",
        "org.slopos.net.configure",
        "org.slopos.software.install",
        "org.slopos.accounts.manage",
        "org.slopos.accounts.password",
    ] {
        assert_eq!(
            asks(policy.action(name).unwrap().admits(&session, &people)),
            Some((Kind::Confirm, "user".into(), vec![])),
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
    assert!(asks(rescue.admits(&session, &people)).is_some());
    assert!(rescue.accepts(&args_of(&["--rescue"])));
    assert!(!rescue.accepts(&args_of(&["--rescue", "-c", "id"])));
}

/// What goes into `/usr/local` once it is the system's is installed by uid 0
/// holding nothing, which reads only what uid 0 may by mode, one install at a
/// time, from the requester's directory.
#[test]
fn the_software_action_installs_as_uid_0_holding_nothing() {
    let policy = Policy::parse(SHIPPED).unwrap();
    let action = policy.action("org.slopos.software.install").unwrap();
    assert_eq!(action.program, "/bin/tree-install");
    assert_eq!(action.caps, 0);
    assert_eq!(action.runs_as, RunsAs::Uid(0));
    assert!(action.cwd);
    assert!(action.exclusive_group().is_some());
    assert!(action.accepts(&args_of(&["apps", "builddir/apps", "/usr/local"])));
}

/// With a second person, the system's actions are an administrator's: a
/// standard user is answered by an administrator's password, may see what
/// boots and power the machine off while nobody else is logged in, and
/// changes their own password by confirming it.
#[test]
fn the_shipped_policy_reads_a_standard_user_s_requests_as_an_administrator_s_to_answer() {
    let policy = Policy::parse(SHIPPED).unwrap();
    let bob = shell(BOB);
    let by_alice = Some((Kind::Admin, "bob".into(), vec!["alice".into()]));
    let alone = People {
        logged_in: vec![BOB],
        ..four()
    };
    let shared = People {
        logged_in: vec![USER, BOB],
        ..four()
    };
    for action in policy.actions() {
        let verdict = action.admits(&bob, &alone);
        let unasked = [
            "org.slopos.boot.status",
            "org.slopos.boot.spare",
            "org.slopos.power.poweroff",
            "org.slopos.power.reboot",
            "org.slopos.boot.reboot",
            "org.slopos.console.keymap",
        ];
        match action.name.as_str() {
            name if unasked.contains(&name) => assert_eq!(verdict, Verdict::Run, "{name}"),
            "org.slopos.login" | "org.slopos.session.unlock" => assert_eq!(verdict, NO),
            "org.slopos.accounts.password" => {
                assert_eq!(asks(verdict).map(|(k, ..)| k), Some(Kind::Password))
            }
            name => assert_eq!(asks(verdict), by_alice, "{name}"),
        }
    }
    for name in [
        "org.slopos.power.poweroff",
        "org.slopos.power.reboot",
        "org.slopos.boot.reboot",
        "org.slopos.console.keymap",
    ] {
        let action = policy.action(name).unwrap();
        assert_eq!(asks(action.admits(&bob, &shared)), by_alice, "{name}");
        assert_eq!(action.admits(&shell(USER), &shared), Verdict::Run, "{name}");
    }
}

/// The extension actions hold what changes the store and the activations,
/// or for a configuration the seal alone, and nothing that could rewrite or
/// cover the root; only what a key signed goes in for an administrator
/// unasked, and they run one at a time.
#[test]
fn the_extension_actions_hold_the_store_and_nothing_wider() {
    let policy = Policy::parse(SHIPPED).unwrap();
    let people = one_person();
    let session = shell(USER);
    let asked = |verdict: Verdict| asks(verdict).is_some();
    let names = [
        (
            "org.slopos.extension.install",
            true,
            &["install", "/x.ext"][..],
        ),
        (
            "org.slopos.extension.install-signed",
            false,
            &["install-signed", "/x.ext"],
        ),
        (
            "org.slopos.extension.trust",
            true,
            &["key", "remove", "SHA256:x"],
        ),
        ("org.slopos.extension.manage", true, &["rollback", "x"]),
    ];
    let config = [
        (
            "org.slopos.extension.config",
            true,
            &["config", "install", "x", "x.conf", "/x"][..],
        ),
        (
            "org.slopos.extension.config-signed",
            false,
            &["config", "apply", "/x"],
        ),
    ];
    let store = caps::EXTENSION_UPDATE | caps::SEAL;
    for (caps, (name, for_session, args)) in names
        .map(|n| (store, n))
        .into_iter()
        .chain(config.map(|n| (caps::SEAL, n)))
    {
        let action = policy.action(name).unwrap();
        assert_eq!(action.program, "/bin/extctl", "{name}");
        assert_eq!(action.caps, caps, "{name}");
        assert_eq!(
            asked(action.admits(&session, &people)),
            for_session,
            "{name}"
        );
        assert_eq!(action.exclusive_group(), Some("extensions"), "{name}");
        assert!(action.accepts(&args_of(args)), "{name}");
    }
    let manage = policy.action("org.slopos.extension.manage").unwrap();
    assert!(!manage.accepts(&args_of(&["install", "x"])));
    for name in [
        "org.slopos.extension.config",
        "org.slopos.extension.config-signed",
    ] {
        let action = policy.action(name).unwrap();
        assert!(!action.accepts(&args_of(&["config", "edit", "x", "x.conf"])));
        assert!(!action.accepts(&args_of(&["--root", "/mnt", "disable", "x"])));
    }
    assert_eq!(
        policy.action("org.slopos.rescue").unwrap().caps & caps::EXTENSION_UPDATE,
        caps::EXTENSION_UPDATE
    );
}

fn args_of<'a>(words: &[&'a str]) -> Vec<&'a [u8]> {
    words.iter().map(|w| w.as_bytes()).collect()
}

#[test]
fn a_person_is_asked_only_where_their_session_is_reached() {
    let confirm = action("  allow user confirm\n");
    let admin = action("  allow admin confirm\n");
    let people = four();
    let terminal = |uid| Requester::new(uid, Some("/bin/shell"), false).reached(Reach::Terminal);
    let nowhere = |uid| Requester::new(uid, Some("/bin/shell"), false);
    assert_eq!(
        asks(confirm.admits(&terminal(BOB), &people)),
        Some((Kind::Password, "bob".into(), vec![])),
        "a click at a terminal cannot be told from a script's"
    );
    assert_eq!(
        asks(confirm.admits(&terminal(USER), &people)),
        None,
        "and a person without a password answers nothing there"
    );
    assert_eq!(
        confirm.admits(&terminal(USER), &people),
        Verdict::Refuse(Refused::NoPasswordToType("user".into()))
    );
    assert_eq!(
        admin.admits(&terminal(BOB), &people),
        Verdict::Refuse(Refused::AdminAtTerminal),
        "an administrator's password is never typed at a requester's terminal"
    );
    assert_eq!(
        asks(admin.admits(&terminal(ALICE), &people)).map(|(k, ..)| k),
        Some(Kind::Password),
        "an administrator answers for themselves there by their own"
    );
    for uid in [USER, ALICE, BOB] {
        assert_eq!(
            confirm.admits(&nowhere(uid), &people),
            Verdict::Refuse(Refused::Unreached),
            "a session behind, locked or of no person is asked nothing"
        );
    }
    assert_eq!(
        admin.admits(&nowhere(BOB), &people),
        Verdict::Refuse(Refused::Unreached)
    );
    assert_eq!(
        action("  allow admin\n").admits(&nowhere(USER), &people),
        Verdict::Run,
        "what runs unasked needs no reach"
    );
}

#[test]
fn the_greeter_is_the_login_screen_s_user_running_a_base_program() {
    use slopos_abi::fs::GREETER_UID;
    let login = action("  allow greeter\n");
    let people = four();
    let greeter = Requester::new(GREETER_UID, Some("/bin/compositor"), false);
    assert_eq!(login.admits(&greeter, &people), Verdict::Run);
    assert_eq!(
        login.admits(&Requester::new(GREETER_UID, None, false), &people),
        NO,
        "its uid running a program from outside the base is no greeter"
    );
    for uid in [0, USER, ALICE] {
        assert_eq!(login.admits(&shell(uid), &people), NO);
    }
    assert_eq!(
        refusal(&with("  allow greeter confirm")).msg,
        "the level is not one that principal takes"
    );
    assert_eq!(
        refusal(&with("  allow greeter alone now")).msg,
        "allow ends with its level"
    );
    let off = action("  allow greeter alone\n  allow user alone\n");
    let nobody = People {
        logged_in: Vec::new(),
        ..four()
    };
    assert_eq!(off.admits(&greeter, &nobody), Verdict::Run);
    let beside = People {
        logged_in: vec![BOB],
        ..four()
    };
    assert_eq!(
        off.admits(&greeter, &beside),
        NO,
        "the login screen powers off no machine someone is logged in to"
    );
    let as_requester = "action org.slopos.x\n  program /bin/x\n  args *\n  caps Mount\n  \
                        uid requester\n  allow greeter\n  describe \"x\"\n";
    assert_eq!(
        refusal(as_requester).msg,
        "an action run as its requester allows only root, user and admin"
    );
}
