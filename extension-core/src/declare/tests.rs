use super::*;
use alloc::vec;
use slopos_abi::caps;

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
    assert!(!reset.unasked());
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
            unasked: vec!["greet".into()],
            users: vec!["hello".into(), "hello-worker".into()],
            commands: vec!["hello".into()],
        }
    );
    assert_eq!(d.unasked_caps(), caps::POWER);
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
fn an_action_any_person_reaches_unasked_counts_as_unasked() {
    let declare = "\
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
action asked
  program  /opt/hello/libexec/c
  args     c
  caps     RawBlock
  allow    user confirm
  allow    admin
  allow    paired
  describe \"C\"
";
    let d = Declaration::parse("hello", declare).unwrap();
    assert_eq!(d.grant().unasked, ["alone", "shell"]);
    assert_eq!(d.unasked_caps(), caps::POWER | caps::MOUNT);
}
