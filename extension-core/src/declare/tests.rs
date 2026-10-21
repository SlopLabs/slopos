use super::*;
use alloc::vec;
use slopos_abi::caps;
use slopos_abi::fs::SESSION_UID;

const FULL: &str = "\
# everything once
user
user worker
state
config
runtime
command hello
app hello
service hellod
  program /opt/hello/bin/hellod
  args    --quiet -v
  user    worker
  caps    NetAdmin SchedHigh
  after   other
  reads   /etc/hello/
  reads   /opt/hello/share/defaults
  listen  tcp 7000
  listen  unix control
  restart always
service other
  program /opt/hello/bin/other
service tray
  program /opt/hello/bin/tray
  session
action greet
  program  /opt/hello/libexec/greet
  args     greet *
  caps     Power
  cwd
  exclusive lane
  allow    user
  allow    program /bin/compositor
  describe \"Greet\"
action reset
  program  /opt/hello/libexec/reset
  args     reset
  caps     Mount
  allow    admin confirm
  allow    paired
  describe \"Reset\"
";

#[test]
fn everything_the_grammar_states_reads_as_written() {
    let d = Declaration::parse("hello", FULL).unwrap();
    assert_eq!(d.users, ["hello", "hello-worker"]);
    assert!(d.state && d.config && d.runtime);
    assert_eq!(d.commands, ["hello"]);
    assert_eq!(d.apps, ["hello"]);
    let hellod = &d.services[0];
    assert_eq!(hellod.program, "/opt/hello/bin/hellod");
    assert_eq!(hellod.args, ["--quiet", "-v"]);
    assert_eq!(hellod.user.as_deref(), Some("hello-worker"));
    assert_eq!(hellod.caps, caps::NET_ADMIN | caps::SCHED_HIGH);
    assert_eq!(hellod.after, ["other"]);
    assert_eq!(hellod.reads, ["/etc/hello/", "/opt/hello/share/defaults"]);
    assert_eq!(
        hellod.listen,
        [Listen::Tcp(7000), Listen::Unix("control".into())]
    );
    assert_eq!(hellod.restart, Restart::Always);
    let other = &d.services[1];
    assert_eq!(
        (other.user.as_deref(), other.restart, other.caps),
        (Some("hello"), Restart::OnFailure, 0)
    );
    assert_eq!(d.services[2].user, None);
    let greet = &d.actions[0];
    assert_eq!(greet.name, "ext.hello.greet");
    assert_eq!(greet.short(), "greet");
    assert!(greet.unasked() && greet.cwd);
    assert_eq!(greet.exclusive.as_deref(), Some("ext.hello.lane"));
    assert_eq!(
        greet.allows[1].principal,
        Principal::Program("/bin/compositor".into())
    );
    let reset = &d.actions[1];
    assert!(reset.unasked(), "the paired agent is never asked");
    assert_eq!(
        reset.allows[0],
        Allow {
            principal: Principal::Admin,
            level: Level::Confirm
        }
    );
    assert_eq!(
        d.grant(),
        Grant {
            caps: caps::NET_ADMIN | caps::SCHED_HIGH | caps::POWER | caps::MOUNT,
            unasked: vec!["greet".into(), "reset".into()],
            users: vec!["hello".into(), "hello-worker".into()],
            commands: vec!["hello".into()],
        }
    );
    assert_eq!(d.unasked_caps(), caps::POWER | caps::MOUNT);
    let programs: Vec<String> = d.programs("hello").collect();
    assert_eq!(
        programs,
        [
            "/opt/hello/bin/hellod",
            "/opt/hello/bin/other",
            "/opt/hello/bin/tray",
            "/opt/hello/libexec/greet",
            "/opt/hello/libexec/reset",
            "/opt/hello/bin/hello",
        ]
    );
    assert_eq!(Declaration::parse("hello", ""), Ok(Declaration::default()));
}

fn refused(text: &str) -> usize {
    match Declaration::parse("hello", text) {
        Err(e) => e.line,
        Ok(d) => panic!("{text:?} parsed as {d:?}"),
    }
}

#[test]
fn every_rule_of_the_grammar_refuses_with_its_line() {
    let service =
        |settings: &str| format!("user\nservice s\n  program /opt/hello/bin/s\n{settings}");
    let action = |settings: &str| {
        format!(
            "action a\n  program /opt/hello/bin/a\n  args a\n  caps Power\n  describe \"A\"\n{settings}"
        )
    };
    for (text, line) in [
        ("user\nuser\n", 2),
        ("user Worker\n", 1),
        ("user a b\n", 1),
        ("user abcdefghijklmnopqrstuvwxyza\n", 1),
        ("state\nstate\n", 2),
        ("state now\n", 1),
        ("command\n", 1),
        ("command a\ncommand a\n", 2),
        ("command a/b\n", 1),
        ("frobnicate\n", 1),
        ("  program /opt/hello/bin/x\n", 1),
        ("service S\n", 1),
        ("service s\n  caps Power\n", 1),
        (&service("  program /opt/hello/bin/t\n"), 4),
        (&service("  program /opt/other/bin/s\n"), 4),
        (&service("  program /opt/hello/lib/s\n"), 4),
        (&action("  allow program /opt/hello/lib/x\n"), 6),
        (&service("  user ghost\n"), 2),
        (&service("  caps Wizard\n"), 4),
        (&service("  listen tcp 0\n"), 4),
        (&service("  listen tcp 70000\n"), 4),
        (&service("  listen unix sock\n"), 2),
        (&service("  listen sctp 1\n"), 4),
        (&service("  reads /etc/hello/\n"), 2),
        (&service("  reads /etc/other\n"), 4),
        (&service("  reads /opt/hello/../x\n"), 4),
        (&service("  restart sometimes\n"), 4),
        (&service("  session\n  user\n"), 2),
        (&service("  session\n  listen tcp 1\n"), 2),
        (&service("  frobnicate\n"), 4),
        ("service s\n  program /opt/hello/bin/s\n", 0),
        ("service s\n  program /opt/hello/bin/s\n  session\n", 0),
        ("state\n", 0),
        ("runtime\n", 0),
        (
            &(0..=MAX_SERVICES)
                .map(|i| format!("service s{i}\n  program /opt/hello/bin/s\n"))
                .fold("user\n".to_string(), |all, s| all + &s),
            0,
        ),
        (&service("  after s\n"), 0),
        (&service("  after ghost\n"), 0),
        (
            &format!(
                "{}service t\n  program /opt/hello/bin/t\n  after s\n",
                service("  after t\n")
            ),
            0,
        ),
        (&action("  allow root\n"), 6),
        (&action("  allow session\n"), 6),
        (&action("  allow user maybe\n"), 6),
        (&action("  allow admin alone\n"), 6),
        (&action("  allow paired confirm\n"), 6),
        (&action("  allow user\n  allow user confirm\n"), 7),
        (&action("  keep 0\n"), 6),
        (&action("  keep 60\n  keep 60\n"), 7),
        (&action("  allow program /opt/other/bin/x\n"), 6),
        (&action("  allow user extra words\n"), 6),
        (
            "action a\n  program /opt/hello/bin/a\n  args a\n  caps Power\n  describe \"A\"\n",
            1,
        ),
        (
            "action a\n  program /opt/hello/bin/a\n  args a\n  caps SchedHigh\n",
            4,
        ),
        (
            "action a\n  program /opt/hello/bin/a\n  args a\n  caps SetId\n",
            4,
        ),
        (
            "action a\n  args a\n  caps Power\n  allow user\n  describe \"A\"\n",
            1,
        ),
        (
            &action(
                "  allow user\naction a\n  program /opt/hello/bin/a\n  args a\n  caps Power\n  allow user\n  describe \"A\"\n",
            ),
            7,
        ),
        (&action("  allow user\n  exclusive Lane\n"), 7),
        (&action("  allow user\n"), 0),
    ] {
        assert_eq!(refused(text), line, "{text:?}");
    }
    let long = "abcdefghijklmnopqrstuvwxyz012345";
    let text = format!(
        "action {long}\n  program /opt/{long}/bin/a\n  args a\n  caps Power\n  allow user\n  describe \"A\"\n"
    );
    assert_eq!(
        Declaration::parse(long, &text).map_err(|e| e.line),
        Err(1),
        "ext.<name>.<action> is past privd's 64 bytes"
    );
    let text = "action a\n  program /opt/2fa/bin/a\n  args a\n  caps Power\n  allow user\n  describe \"A\"\n";
    assert_eq!(
        Declaration::parse("2fa", text).map_err(|e| e.msg),
        Err("privd names no action by a word that starts with a digit")
    );
}

#[test]
fn every_prefix_and_every_byte_replaced_parses_or_names_a_line() {
    let lines: Vec<&str> = FULL.lines().collect();
    for cut in 0..=lines.len() {
        let _ = Declaration::parse("hello", &lines[..cut].join("\n"));
    }
    for at in 0..FULL.len() {
        for byte in [b' ', b'\n', b'x', b'/', b'"', 0] {
            let mut text = FULL.as_bytes().to_vec();
            text[at] = byte;
            if let Ok(text) = core::str::from_utf8(&text) {
                let _ = Declaration::parse("hello", text);
            }
        }
    }
}

#[test]
fn an_action_that_runs_with_nobody_asked_counts_as_unasked() {
    let declare = "\
user
action alone
  program  /opt/hello/libexec/a
  args     a
  caps     Power
  allow    user alone
  describe \"A\"
action shell
  program  /opt/hello/libexec/b
  args     b
  caps     Mount
  allow    program /bin/shell
  describe \"B\"
action admin
  program  /opt/hello/libexec/d
  args     d
  caps     Clock
  allow    admin
  describe \"D\"
action asked
  program  /opt/hello/libexec/c
  args     c
  caps     RawBlock
  allow    user confirm
  allow    admin password
  describe \"C\"
action remote
  program  /opt/hello/libexec/e
  args     e
  caps     NetAdmin
  allow    paired
  describe \"E\"
";
    let d = Declaration::parse("hello", declare).unwrap();
    assert_eq!(d.grant().unasked, ["alone", "shell", "admin", "remote"]);
    assert_eq!(
        d.unasked_caps(),
        caps::POWER | caps::MOUNT | caps::CLOCK | caps::NET_ADMIN
    );
}

#[test]
fn a_service_naming_no_user_runs_as_the_first_declared_wherever_it_is() {
    let declare = "\
service early
  program /opt/hello/bin/early
user worker
user
";
    let d = Declaration::parse("hello", declare).unwrap();
    assert_eq!(d.services[0].user.as_deref(), Some("hello-worker"));
    let named = "service early\n  program /opt/hello/bin/early\n  user worker\nuser\nuser worker\n";
    let d = Declaration::parse("hello", named).unwrap();
    assert_eq!(d.services[0].user.as_deref(), Some("hello-worker"));
}

#[test]
fn two_services_never_listen_on_one_socket() {
    let declare = |second: &str| {
        format!(
            "user\nruntime\nservice a\n  program /opt/hello/bin/a\n  listen tcp 7000\n  listen unix ctl\n\
             service b\n  program /opt/hello/bin/b\n  {second}\n"
        )
    };
    assert_eq!(refused(&declare("listen tcp 7000")), 7);
    assert_eq!(refused(&declare("listen unix ctl")), 7);
    assert!(Declaration::parse("hello", &declare("listen udp 7000")).is_ok());
}

#[test]
fn privd_reads_an_action_s_principals_and_levels_as_declared() {
    use slopos_abi::extension::Label;
    use slopos_priv_core::consent::Kind;
    use slopos_priv_core::{People, Person, Reach, Requester, Verdict};
    let declare = "\
user
action greet
  program  /opt/hello/libexec/greet
  args     greet *
  caps     Power Mount
  cwd
  exclusive lane
  keep     60
  allow    program /opt/hello/bin/client
  allow    admin password
  allow    user
  allow    program /bin/compositor
  allow    paired
  describe \"Greet\"
action calm
  program  /opt/hello/libexec/greet
  args     calm
  caps     Power
  allow    user alone
  describe \"Calm\"
";
    let d = Declaration::parse("hello", declare).unwrap();
    let label = Label::new(b"hello", [3; 32]).unwrap();
    let greet = d.actions[0].for_privd(105, caps::POWER, label);
    assert_eq!(greet.name, "ext.hello.greet");
    assert_eq!(greet.program, "/opt/hello/libexec/greet");
    assert_eq!(greet.runs_as, policy::RunsAs::Uid(105));
    assert_eq!(greet.caps, caps::POWER, "held within what was granted");
    assert!(greet.cwd);
    assert_eq!(greet.keep, Some(60));
    assert_eq!(greet.exclusive_group(), Some("ext.hello.lane"));
    assert_eq!(greet.describe, "Greet (extension hello)");
    assert_eq!(
        greet
            .allows
            .iter()
            .map(|a| (a.principal.clone(), a.level))
            .collect::<Vec<_>>(),
        vec![
            (
                policy::Principal::Extension("/opt/hello/bin/client".into(), label),
                Level::None
            ),
            (policy::Principal::Admin, Level::Password),
            (policy::Principal::User, Level::None),
            (
                policy::Principal::Program("/bin/compositor".into()),
                Level::None
            ),
            (policy::Principal::Paired, Level::None),
        ]
    );
    let person = |uid, name: &str, admin| Person {
        uid,
        name: name.into(),
        admin,
        password: true,
    };
    let people = People {
        people: vec![
            person(SESSION_UID, "user", true),
            person(SESSION_UID + 1, "bob", false),
        ],
        logged_in: vec![SESSION_UID, SESSION_UID + 1],
    };
    let shown = Reach::Screen { theirs: true };
    let admin = Requester::new(SESSION_UID, None, false).reached(shown);
    let standard = Requester::new(SESSION_UID + 1, None, false).reached(shown);
    assert!(matches!(
        greet.admits(&admin, &people),
        Verdict::Ask(q) if q.kind == Kind::Password
    ));
    assert_eq!(greet.admits(&standard, &people), Verdict::Run);
    assert_eq!(
        greet.admits(
            &Requester::extension(SESSION_UID, "/opt/hello/bin/client", label),
            &people
        ),
        Verdict::Run
    );
    assert!(matches!(
        greet.admits(&Requester::new(0, Some("/bin/shell"), false), &people),
        Verdict::Refuse(_)
    ));
    let calm = d.actions[1].for_privd(105, caps::POWER, label);
    assert_eq!(calm.admits(&admin, &people), Verdict::Run);
    assert!(matches!(
        calm.admits(&standard, &people),
        Verdict::Ask(q) if q.kind == Kind::Admin
    ));
    let bob_alone = People {
        logged_in: vec![SESSION_UID + 1],
        ..people
    };
    assert_eq!(calm.admits(&standard, &bob_alone), Verdict::Run);
}
