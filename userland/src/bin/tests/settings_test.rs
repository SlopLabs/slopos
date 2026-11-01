//! The tools Settings changes the system through, as a terminal runs them:
//! `netctl` (with `--network`, which brings the network back to DHCP before
//! it ends), `prefs`, `keymap`'s own layout and `users list --porcelain`,
//! the last three as root and as the first person holding nothing.

use std::process::Command;
use std::time::{Duration, Instant};

use slopos_settings_core::network::{self, Ipv4, Mac, Scope, Settings};
use slopos_settings_core::prefs::{Key, Prefs};
use slopos_userland::network_settings as kept;

const NETCTL: &str = "/bin/netctl";
const IP: &str = "/bin/ip";
const PREFS: &str = "/bin/prefs";
const KEYMAP: &str = "/bin/keymap";
const USERS: &str = "/bin/users";
const PICTURE: &str = "/usr/share/slopos/wallpapers/default.png";
const SETTLE: Duration = Duration::from_secs(20);

struct Ran {
    status: Option<i32>,
    out: String,
    err: String,
}

impl Ran {
    fn ok(&self) -> bool {
        self.status == Some(0)
    }

    fn shown(&self) -> String {
        format!("{:?}: {}{}", self.status, self.out, self.err)
    }
}

/// A home of the test's own, apart for each uid it runs as.
fn home() -> &'static str {
    static HOME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let uid = slopos_userland::syscall::process::geteuid();
        format!("/tmp/settings-test-home-{uid}")
    })
}

fn run(program: &str, args: &[&str]) -> Ran {
    let output = Command::new(program)
        .args(args)
        .env("HOME", home())
        .output();
    match output {
        Ok(out) => Ran {
            status: out.status.code(),
            out: String::from_utf8_lossy(&out.stdout).into_owned(),
            err: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Err(e) => Ran {
            status: None,
            out: String::new(),
            err: e.to_string(),
        },
    }
}

fn fail(why: String) -> bool {
    eprintln!("settings_test: {why}");
    false
}

fn fresh_home() -> bool {
    let _ = std::fs::remove_dir_all(home());
    std::fs::create_dir_all(home()).is_ok()
}

fn own_prefs() -> Prefs {
    slopos_userland::prefs::read(&slopos_settings_core::prefs::path(home()))
}

fn prefs_are_kept_typed_and_reset() -> bool {
    if !fresh_home() {
        return fail(format!("{} cannot be made", home()));
    }
    let set = run(PREFS, &["set", "background.fit", "tile"]);
    let got = run(PREFS, &["get", "background.fit"]);
    if !set.ok() || got.out.trim() != "tile" {
        return fail(format!("set {} then get {}", set.shown(), got.shown()));
    }
    let wrong = run(PREFS, &["set", "background.fit", "zoom"]);
    if wrong.ok() || !wrong.err.contains("fill, fit, center, stretch, tile") {
        return fail(format!("a fit no list names: {}", wrong.shown()));
    }
    let picture = run(PREFS, &["set", "background.picture", PICTURE]);
    let not_png = run(PREFS, &["set", "background.picture", "/etc/passwd"]);
    if !picture.ok() || not_png.ok() {
        return fail(format!(
            "a picture {}; not one {}",
            picture.shown(),
            not_png.shown()
        ));
    }
    let prefs = own_prefs();
    if prefs.background().picture != Some(PICTURE) || !prefs.is_set(Key::BackgroundFit) {
        return fail(format!("the file holds {}", prefs.render()));
    }
    let listed = run(PREFS, &[]);
    if !listed.out.contains("clock.format = 24h  (default)") {
        return fail(format!("the list says {}", listed.shown()));
    }
    let reset = run(PREFS, &["reset", "--all"]);
    let after = own_prefs();
    if !reset.ok() || Key::ALL.iter().any(|&k| after.is_set(k)) {
        return fail(format!("reset {} left {}", reset.shown(), after.render()));
    }
    let described = run(PREFS, &["describe", "clock.format"]);
    described.out.contains("takes: 24h, 12h") || fail(format!("described {}", described.shown()))
}

/// A FIFO, a device or a link where a person's preferences belong is read
/// as no preferences at once, by them and by init alike.
fn a_preferences_file_that_is_no_file_is_passed_over() -> bool {
    if !fresh_home() {
        return fail(format!("{} cannot be made", home()));
    }
    let path = slopos_settings_core::prefs::path(home());
    let _ = std::fs::create_dir_all(format!("{}/.config/slopos", home()));
    let fifo = std::ffi::CString::new(path.as_str()).expect("no NUL in a test path");
    // SAFETY: `fifo` is a NUL-terminated string that outlives the call.
    if unsafe { slopos_slibc::ffi::syscalls::mkfifo(fifo.as_ptr(), 0o600) } != 0 {
        return fail(format!("{path}: no FIFO made"));
    }
    let start = Instant::now();
    let theirs = slopos_userland::prefs::of_person(home(), 0);
    let own = own_prefs();
    let _ = std::fs::remove_file(&path);
    let _ = std::os::unix::fs::symlink("/dev/zero", &path);
    let linked = own_prefs();
    let _ = std::fs::remove_file(&path);
    (start.elapsed() < Duration::from_secs(5)
        && [theirs, own, linked].iter().all(|p| *p == Prefs::default()))
        || fail(format!("read in {:?}", start.elapsed()))
}

fn your_layout_is_a_preference_init_follows() -> bool {
    if !fresh_home() {
        return fail(format!("{} cannot be made", home()));
    }
    let before = slopos_userland::keymap::current_name();
    let set = run(KEYMAP, &["set", "de_CH"]);
    let shown = run(KEYMAP, &["show"]);
    let kept = own_prefs().keyboard_layout().map(str::to_string);
    if !set.ok() || kept.as_deref() != Some("de_CH") || !shown.out.contains("Swiss German") {
        return fail(format!(
            "set {}; kept {kept:?}; shown {}",
            set.shown(),
            shown.shown()
        ));
    }
    if slopos_userland::keymap::current_name() != before {
        return fail(String::from(
            "a layout loaded with no desktop of this person's in front",
        ));
    }
    let unknown = run(KEYMAP, &["set", "klingon"]);
    let reset = run(KEYMAP, &["reset"]);
    if unknown.ok() || !reset.ok() || own_prefs().keyboard_layout().is_some() {
        return fail(format!(
            "unknown {}; reset {}",
            unknown.shown(),
            reset.shown()
        ));
    }
    let titled = run(KEYMAP, &["list", "--titles"]);
    let layouts = slopos_userland::keymap::layouts();
    let every = ["us", "de_CH", "gb", "de", "fr", "dvorak", "colemak"];
    let all_listed = every.iter().all(|n| layouts.iter().any(|l| l.name == *n));
    (titled.out.contains("French (AZERTY)") && all_listed)
        || fail(format!("listed {} and {layouts:?}", titled.shown()))
}

fn the_people_are_listed_for_another_program() -> bool {
    let ran = run(USERS, &["list", "--porcelain"]);
    let listed = slopos_accounts_core::listing::parse(&ran.out);
    let first = listed
        .as_ref()
        .and_then(|people| people.iter().find(|p| p.uid == slopos_abi::fs::SESSION_UID));
    (ran.ok() && first.is_some_and(|p| p.admin && !p.withheld))
        || fail(format!("listed {} as {listed:?}", ran.shown()))
}

fn eth0() -> Option<network::LiveAdapter> {
    kept::live()
        .ok()?
        .adapters
        .into_iter()
        .find(|a| a.name == "eth0")
}

/// Waits for `holds` of the network, read again until it does.
fn until(what: &str, holds: impl Fn(&network::Live) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < SETTLE {
        if kept::live().is_ok_and(|live| holds(&live)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    fail(format!("{what} within {SETTLE:?}: {:?}", kept::live()))
}

fn leased(live: &network::Live) -> bool {
    live.adapters
        .iter()
        .find(|a| a.name == "eth0")
        .is_some_and(|a| a.dhcp && a.addresses.iter().any(|x| x.from_dhcp) && a.gateway.is_some())
}

/// What `netctl` leaves behind: nothing kept, eth0 back on a lease.
fn back_to_dhcp() -> bool {
    let _ = std::fs::remove_file(network::PATH);
    let failed = kept::apply(&back_to_auto(), &Scope::Everything);
    failed.is_empty() && until("eth0 leased again", leased)
}

fn back_to_auto() -> Settings {
    let mut settings = Settings::default();
    if let Some(eth0) = eth0() {
        settings.set_for(&eth0.name, eth0.mac, true, network::Ipv4Method::Auto);
    }
    settings
}

fn a_manual_address_is_kept_and_forgotten() -> bool {
    let Some(before) = eth0().filter(|a| a.dhcp) else {
        return fail(format!("eth0 is not on a lease: {:?}", eth0()));
    };
    let Some(lease) = before
        .addresses
        .iter()
        .find(|a| a.from_dhcp)
        .map(|a| a.address)
    else {
        return fail(format!("eth0 holds no lease: {before:?}"));
    };
    let gateway = before.gateway.unwrap_or(Ipv4([10, 0, 2, 2]));
    let address = lease.to_string();
    let gateway_text = gateway.to_string();
    let set = run(
        NETCTL,
        &["set", "eth0", "manual", &address, "gateway", &gateway_text],
    );
    let manual = |live: &network::Live| {
        live.adapters.iter().any(|a| {
            a.name == "eth0"
                && !a.dhcp
                && a.addresses
                    .iter()
                    .any(|x| x.address == lease && !x.from_dhcp)
                && a.gateway == Some(gateway)
        })
    };
    if !set.ok() || !until("eth0 manual", manual) {
        return fail(format!("set {}", set.shown()));
    }
    let file = std::fs::read_to_string(network::PATH).unwrap_or_default();
    let by_mac = format!("adapter {}", before.mac);
    if !file.contains(&by_mac) || !file.contains(&format!("ipv4 manual {address}")) {
        return fail(format!("{} holds {file}", network::PATH));
    }
    let status = run(NETCTL, &["status", "eth0"]);
    if !status.out.contains("settings  manual") || status.out.contains("not yet") {
        return fail(format!("status says {}", status.shown()));
    }
    let again = run(NETCTL, &["apply", "eth0"]);
    if !again.ok() || !kept::live().is_ok_and(|live| manual(&live)) {
        return fail(format!("applying again {}", again.shown()));
    }
    let forget = run(NETCTL, &["forget", "eth0"]);
    let file = std::fs::read_to_string(network::PATH).unwrap_or_default();
    (forget.ok() && !file.contains(&by_mac) && until("eth0 leased again", leased))
        || fail(format!("forget {} left {file}", forget.shown()))
}

fn dns_set_by_hand_is_handed_back_to_the_lease() -> bool {
    let set = run(NETCTL, &["dns", "9.9.9.9", "1.1.1.1"]);
    let pinned = |live: &network::Live| {
        live.dns.pinned && live.dns.servers == [Ipv4([9, 9, 9, 9]), Ipv4([1, 1, 1, 1])]
    };
    if !set.ok() || !until("the DNS servers pinned", pinned) {
        return fail(format!("dns {}", set.shown()));
    }
    let auto = run(NETCTL, &["dns", "auto"]);
    let leased_dns = |live: &network::Live| !live.dns.pinned && !live.dns.servers.is_empty();
    (auto.ok() && until("the lease's DNS servers", leased_dns))
        || fail(format!("dns auto {}", auto.shown()))
}

fn a_wrong_setting_is_refused_before_anything_changes() -> bool {
    let before = std::fs::read_to_string(network::PATH).ok();
    let cases: [(&[&str], i32, &str); 5] = [
        (&["set", "eth0", "manual", "10.0.2.15"], 2, "prefix"),
        (
            &[
                "set",
                "eth0",
                "manual",
                "10.0.2.15/24",
                "gateway",
                "10.9.9.9",
            ],
            2,
            "network",
        ),
        (&["set", "lo", "auto"], 2, "loopback"),
        (&["apply", "eth9"], 1, "no such adapter"),
        (&["dns", "1.1.1.1", "x"], 2, "DNS servers"),
    ];
    for (args, status, said) in cases {
        let ran = run(NETCTL, args);
        if ran.status != Some(status) || !ran.err.contains(said) {
            return fail(format!("netctl {args:?}: {}", ran.shown()));
        }
    }
    let mut load = Command::new(NETCTL);
    load.arg("load")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let loaded = load.spawn().and_then(|mut child| {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(b"adapter eth0\n  ipv4 sideways\n")?;
        }
        child.wait_with_output()
    });
    let refused = loaded.is_ok_and(|out| {
        let said = String::from_utf8_lossy(&out.stderr);
        !out.status.success() && said.contains("line 2: This line isn't one")
    });
    let after = std::fs::read_to_string(network::PATH).ok();
    (refused && before == after) || fail(format!("load wrote {after:?} over {before:?}"))
}

fn a_mac_names_an_adapter_too() -> bool {
    let Some(eth0) = eth0() else {
        return fail(String::from("no eth0"));
    };
    let mac = eth0.mac.to_string();
    let off = run(NETCTL, &["set", &mac, "off"]);
    let down = |live: &network::Live| live.adapters.iter().any(|a| a.name == "eth0" && !a.up);
    if !off.ok() || !until("eth0 down", down) {
        return fail(format!("off {}", off.shown()));
    }
    let on = run(NETCTL, &["set", &mac, "on"]);
    let settings = kept::read().unwrap_or_default();
    let governing = settings.governing("eth0", Mac(eth0.mac.0)).cloned();
    (on.ok() && governing.is_some_and(|g| g.enabled) && until("eth0 leased again", leased))
        || fail(format!("on {} with {}", on.shown(), settings.render()))
}

/// Taking one of two addresses on a network away leaves the route to it.
fn a_second_address_on_the_network_leaves_its_route() -> bool {
    let add = run(IP, &["addr", "add", "10.0.2.99/24", "dev", "eth0"]);
    let del = run(IP, &["addr", "del", "10.0.2.99/24", "dev", "eth0"]);
    let routes = run(IP, &["route"]);
    (add.ok() && del.ok() && routes.out.contains("10.0.2.0/24"))
        || fail(format!(
            "add {}; del {}; routes {}",
            add.shown(),
            del.shown(),
            routes.shown()
        ))
}

const NETWORK_CASES: &[(&str, fn() -> bool)] = &[
    (
        "a_wrong_setting_is_refused_before_anything_changes",
        a_wrong_setting_is_refused_before_anything_changes,
    ),
    (
        "a_manual_address_is_kept_and_forgotten",
        a_manual_address_is_kept_and_forgotten,
    ),
    ("a_mac_names_an_adapter_too", a_mac_names_an_adapter_too),
    (
        "dns_set_by_hand_is_handed_back_to_the_lease",
        dns_set_by_hand_is_handed_back_to_the_lease,
    ),
    (
        "a_second_address_on_the_network_leaves_its_route",
        a_second_address_on_the_network_leaves_its_route,
    ),
    ("the_network_is_back_on_dhcp", back_to_dhcp),
];

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "prefs_are_kept_typed_and_reset",
        prefs_are_kept_typed_and_reset,
    ),
    (
        "a_preferences_file_that_is_no_file_is_passed_over",
        a_preferences_file_that_is_no_file_is_passed_over,
    ),
    (
        "your_layout_is_a_preference_init_follows",
        your_layout_is_a_preference_init_follows,
    ),
    (
        "the_people_are_listed_for_another_program",
        the_people_are_listed_for_another_program,
    ),
];

fn main() {
    let network = std::env::args().nth(1).as_deref() == Some("--network");
    let cases = match network {
        true => NETWORK_CASES,
        false => CASES,
    };
    slopos_slibc::test_harness::run(cases);
}
