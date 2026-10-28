use slopos_appkit::embed::Embedded;
use slopos_appkit::{Node, Size};
use slopos_chrome_core::clock::WallTime;
use slopos_chrome_core::gate::{Checked, Gate};
use slopos_chrome_core::key::Key;
use slopos_chrome_core::menu::{Confirm, Ending, Menu};
use slopos_chrome_core::netstate::{IfaceKind, IfaceRow, NetPanelModel};
use slopos_slibc::test_harness::{TestStatus, report};
use slopos_userland::apps::compositor::views;

/// What the audit finds in `node` at a phone's, a laptop's and a monitor's
/// width, each finding named after `name`.
fn audited<M: Clone + 'static>(name: &str, node: &Node<M>) -> Vec<String> {
    slopos_appkit::audit::audit_node(node, &[360, 640, 960], 720)
        .into_iter()
        .map(|found| format!("{name}: {found}"))
        .collect()
}

/// What the audit finds in the panel `node` at the size the compositor
/// lays it out at under the menu bar, and on a screen too narrow for that,
/// with the room round it the audit keeps for a focus ring.
fn audited_panel<M: Clone + 'static>(name: &str, node: &Node<M>) -> Vec<String> {
    let mut found = audited(name, node);
    let room = 2 * slopos_theme_core::space::SM;
    for most in [views::PANEL_WIDTH, 240] {
        let size = Embedded::natural(node, Size::new(most, 720)).rect();
        found.extend(
            slopos_appkit::audit::audit_node(node, &[size.width + room], size.height + room)
                .into_iter()
                .map(|found| format!("{name} at {}x{}: {found}", size.width, size.height)),
        );
    }
    found
}

fn typed(gate: &mut Gate, text: &str) {
    for ch in text.chars() {
        gate.key(Key::Text(ch));
    }
}

fn the_compositor_s_surfaces_read_whole_at_every_width() {
    let now = WallTime::from_unix(1_793_085_900);
    let mut found = Vec::new();

    let mut lock = Gate::lock("carol".into());
    found.extend(audited("lock", &views::gate(&lock, None)));
    typed(&mut lock, "secret");
    found.extend(audited("lock typed", &views::gate(&lock, now.as_ref())));
    for checked in [Checked::Wrong, Checked::Unchecked] {
        lock.key(Key::Press);
        lock.checked(checked);
        found.extend(audited(
            &format!("lock {checked:?}"),
            &views::gate(&lock, now.as_ref()),
        ));
    }

    let mut login = Gate::login(vec!["user".into(), "bob".into(), "carol".into()]);
    typed(&mut login, "car");
    found.extend(audited("login", &views::gate(&login, now.as_ref())));
    login.key(Key::Press);
    login.key(Key::Press);
    login.checked(Checked::Passed);
    found.extend(audited(
        "login starting",
        &views::gate(&login, now.as_ref()),
    ));
    let mut crowded = Gate::login((0..20).map(|n| format!("person{n:02}")).collect());
    typed(&mut crowded, "person17");
    found.extend(audited(
        "login with twenty people",
        &views::gate(&crowded, now.as_ref()),
    ));
    found.extend(audited(
        "login with nobody",
        &views::gate(&Gate::login(Vec::new()), now.as_ref()),
    ));

    for notice in [false, true] {
        found.extend(audited_panel(
            &format!("menu, notice {notice}"),
            &views::system_menu(&Menu::new(notice), "carol"),
        ));
    }

    let mut model = NetPanelModel::EMPTY;
    found.extend(audited_panel(
        "network, nothing",
        &views::network_panel(&model, None),
    ));
    model.enabled = true;
    let mut up = IfaceRow::named(b"eth0", IfaceKind::Ethernet);
    up.admin_up = true;
    up.carrier = true;
    up.ipv4 = [10, 0, 2, 15];
    up.prefix_len = 24;
    model.push_iface(up);
    model.push_iface(IfaceRow::named(b"eth1", IfaceKind::Ethernet));
    model.gateway = [10, 0, 2, 2];
    found.extend(audited_panel(
        "network",
        &views::network_panel(&model, Some(true)),
    ));

    found.extend(audited_panel(
        "clock",
        &views::clock_panel::<()>(now.as_ref()),
    ));
    found.extend(audited_panel(
        "clock unset",
        &views::clock_panel::<()>(None),
    ));

    for ending in Ending::ALL {
        found.extend(audited(
            &format!("{ending:?}"),
            &views::confirm(&Confirm::new(ending)),
        ));
        found.extend(audited(
            &format!("{ending:?} failed"),
            &views::failed(ending),
        ));
    }

    assert!(found.is_empty(), "\n  {}", found.join("\n  "));
}

const CHROME_CASES: &[(&str, fn())] = &[(
    "the_compositor_s_surfaces_read_whole_at_every_width",
    the_compositor_s_surfaces_read_whole_at_every_width,
)];

/// A failing case aborts the process, so each case's name goes to stderr
/// before it runs: the serial log names the one that died.
fn main() {
    let cases = slopos_appkit::tests::cases();
    let mut passed = 0usize;

    for &(name, func) in cases.iter().chain(CHROME_CASES) {
        eprintln!("appkit_test: running {name}");
        func();
        report(TestStatus::Pass, name, "");
        passed += 1;
    }

    eprintln!(
        "appkit_test: {passed}/{} cases passed",
        cases.len() + CHROME_CASES.len()
    );
    std::process::exit(0);
}
