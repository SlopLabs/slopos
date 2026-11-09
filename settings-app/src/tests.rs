use std::sync::Once;

use slopos_appkit::embed::Embedded;
use slopos_appkit::{
    Key, KeyboardMap, LineEditInput, Modifiers, NamedKey, Node, Rect, Role, Size, Widget,
};
use slopos_keymap_core::LayoutTable;
use slopos_keymap_core::board::Board;
use slopos_settings_core::network::Settings as NetSettings;
use slopos_settings_core::prefs::{Key as PrefKey, Prefs};
use slopos_theme_core::words;

use slopos_accounts_core::listing::Listed;

use super::*;

fn fonts() {
    static FONTS: Once = Once::new();
    FONTS.call_once(|| {
        slopos_appkit::text::fonts_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/fonts"));
    });
}

fn admin() -> Person {
    Person {
        uid: 1000,
        name: "user".into(),
        admin: true,
    }
}

fn standard() -> Person {
    Person {
        uid: 1001,
        name: "ada".into(),
        admin: false,
    }
}

/// Settings as the window has it once every read is answered.
fn read(panel: Panel, person: Person) -> Settings {
    let mut app = Settings::new(panel, person);
    let (kept, live) = network::sample();
    app.update(Msg::Network(network::Msg::Read(Ok((kept, live)))));
    app.update(Msg::Users(users::Msg::Listed(users::sample(), true)));
    app.update(Msg::Keyboard(keyboard::Msg::Read(keyboard::Read {
        layouts: vec![
            keyboard::Layout {
                name: "de_CH".into(),
                title: "Swiss German (QWERTZ)".into(),
                board: Some(board(include_str!("../../assets/keymaps/de_CH.layout"))),
            },
            keyboard::Layout {
                name: "us".into(),
                title: "US English (QWERTY)".into(),
                board: Some(board(include_str!("../../assets/keymaps/us.layout"))),
            },
        ],
        yours: None,
        computer: "us".into(),
    })));
    let mut prefs = Prefs::default();
    prefs
        .set(
            PrefKey::BackgroundPicture,
            "/home/user/.local/share/backgrounds/dunes.png",
        )
        .unwrap();
    app.update(Msg::Desktop(desktop::Msg::Prefs(prefs)));
    app.update(Msg::Desktop(desktop::Msg::Pictures(vec![
        desktop::Picture {
            path: "/home/user/.local/share/backgrounds/dunes.png".into(),
            name: "Dunes".into(),
            own: true,
        },
    ])));
    app.update(Msg::About(about::Msg::Read(about::Facts {
        version: "SlopOS 0.1.0".into(),
        processor: "QEMU Virtual CPU version 2.5+".into(),
        processors: 4,
        memory: 6 << 30,
        storage: Some((32 << 30, 20 << 30)),
        name: "slopos".into(),
    })));
    app
}

fn board(file: &str) -> Board {
    let mut table = Box::new(LayoutTable::empty());
    slopos_keymap_core::parse(file.as_bytes(), &mut table).unwrap();
    Board::of(&table)
}

fn typed(text: &str) -> Vec<LineEditInput> {
    text.chars()
        .map(|character| LineEditInput::Text { character })
        .collect()
}

fn type_into(app: &mut Settings, wrap: fn(LineEditInput) -> Msg, text: &str) {
    for input in typed(text) {
        app.update(wrap(input));
    }
}

fn try_typing(app: &mut Settings, text: &str) {
    app.update(Msg::Keyboard(keyboard::Msg::Try(LineEditInput::Focus)));
    type_into(app, |i| Msg::Keyboard(keyboard::Msg::Try(i)), text);
}

/// Every state a panel has a page or a dialog for, by name.
fn states() -> Vec<(String, Settings)> {
    let mut out = Vec::new();
    for panel in Panel::ALL {
        out.push((
            format!("{} unread", panel.id()),
            Settings::new(panel, admin()),
        ));
        out.push((panel.id().to_string(), read(panel, admin())));
        out.push((format!("{} standard", panel.id()), read(panel, standard())));
    }
    out.extend(network_states());

    out.extend(users_states());

    let mut computer = read(Panel::Keyboard, admin());
    computer.update(Msg::Keyboard(keyboard::Msg::Change));
    out.push(("keyboard computer".into(), computer));
    for (name, kept) in [
        ("keyboard yours", Ok(())),
        ("keyboard later", Err(keyboard::Kept::Later)),
        (
            "keyboard not kept",
            Err(keyboard::Kept::Not("Your layout couldn't be kept.".into())),
        ),
    ] {
        let mut app = read(Panel::Keyboard, standard());
        app.update(Msg::Keyboard(keyboard::Msg::Yours(1)));
        app.update(Msg::Keyboard(keyboard::Msg::Kept(kept)));
        out.push((name.into(), app));
    }
    let mut tried = read(Panel::Keyboard, standard());
    tried.update(Msg::Keyboard(keyboard::Msg::Yours(1)));
    try_typing(&mut tried, "Grüezi z");
    tried.update(Msg::Keyboard(keyboard::Msg::Try(LineEditInput::Blur)));
    out.push(("keyboard tried".into(), tried));
    let mut refused = read(Panel::Keyboard, standard());
    refused.update(Msg::Keyboard(keyboard::Msg::Change));
    refused.update(Msg::Keyboard(keyboard::Msg::Computer(0)));
    refused.update(Msg::Keyboard(keyboard::Msg::Confirm));
    refused.update(Msg::Answered(
        Purpose::Keyboard,
        Asked::Refused {
            why: "You didn't allow this.".into(),
            by_person: true,
        },
    ));
    out.push(("keyboard computer refused".into(), refused));

    let mut add = read(Panel::Desktop, admin());
    add.update(Msg::Desktop(desktop::Msg::Add));
    type_into(
        &mut add,
        |i| Msg::Desktop(desktop::Msg::Path(i)),
        "beach.png",
    );
    out.push(("desktop add, wrong".into(), add));
    let mut remove = read(Panel::Desktop, admin());
    remove.update(Msg::Desktop(desktop::Msg::Pictures(vec![
        desktop::Picture {
            path: "/home/user/.local/share/backgrounds/dunes.png".into(),
            name: "Dunes".into(),
            own: true,
        },
    ])));
    remove.update(Msg::Desktop(desktop::Msg::Remove));
    out.push(("desktop remove".into(), remove));

    let mut nothing = read(Panel::Network, admin());
    type_into(&mut nothing, Msg::Search, "zzz");
    out.push(("search finds nothing".into(), nothing));
    out
}

fn network_states() -> Vec<(String, Settings)> {
    let mut out = Vec::new();
    let reread = |change: fn(&mut NetSettings, &mut slopos_settings_core::network::Live)| {
        let mut app = read(Panel::Network, admin());
        let (mut kept, mut live) = network::sample();
        change(&mut kept, &mut live);
        app.update(net(network::Msg::Read(Ok((kept, live)))));
        app
    };
    out.push((
        "network off".into(),
        reread(|kept, live| {
            kept.networking = false;
            live.networking = false;
            for adapter in &mut live.adapters {
                adapter.up = adapter.loopback;
                adapter.addresses.clear();
                adapter.gateway = None;
            }
        }),
    ));
    out.push((
        "network unplugged".into(),
        reread(|_, live| {
            for adapter in live.adapters.iter_mut().filter(|a| !a.loopback) {
                adapter.carrier = false;
                adapter.addresses.clear();
                adapter.gateway = None;
            }
        }),
    ));
    out.push((
        "network no adapter".into(),
        reread(|_, live| live.adapters.retain(|a| a.loopback)),
    ));
    out.push((
        "network two adapters".into(),
        reread(|kept, live| {
            let mut usb = live.adapters[1].clone();
            usb.ifindex = 3;
            usb.name = "eth1".into();
            usb.mac = slopos_net_core::Mac([0x52, 0x54, 0, 0x12, 0x34, 0x58]);
            usb.carrier = false;
            usb.addresses.clear();
            usb.gateway = None;
            kept.set_for(&usb.name, usb.mac, false, manual("192.168.7.2/24", None));
            usb.up = false;
            live.adapters.push(usb);
        }),
    ));

    let mut asking = read(Panel::Network, admin());
    asking.update(net(network::Msg::Networking));
    out.push(("network turning off".into(), asking));
    let mut failed = read(Panel::Network, admin());
    failed.update(net(network::Msg::Networking));
    failed.update(ran(1, "netctl: eth0 would not go down"));
    out.push(("network not turned off".into(), failed));

    let mut address = read(Panel::Network, admin());
    edit_eth0(&mut address);
    out.push(("network address".into(), address));
    let mut pinned = read(Panel::Network, standard());
    edit_eth0(&mut pinned);
    pinned.update(net(network::Msg::Method(1)));
    out.push(("network address, manual".into(), pinned));
    let mut wrong = unaddressed();
    edit_eth0(&mut wrong);
    wrong.update(net(network::Msg::Method(1)));
    type_into(
        &mut wrong,
        |i| net(network::Msg::Address(i)),
        "192.168.1.300",
    );
    type_into(&mut wrong, |i| net(network::Msg::Gateway(i)), "10.0.0.1");
    wrong.update(net(network::Msg::Save));
    out.push(("network address, wrong".into(), wrong));
    let mut dns = read(Panel::Network, admin());
    dns.update(net(network::Msg::EditDns));
    dns.update(net(network::Msg::Method(1)));
    type_into(
        &mut dns,
        |i| net(network::Msg::Servers(i)),
        "1.1.1.1 nonsense",
    );
    dns.update(net(network::Msg::Save));
    out.push(("network dns, wrong".into(), dns));
    out
}

#[test]
fn every_page_reads_whole_at_every_width() {
    fonts();
    let mut found = Vec::new();
    for (name, app) in states() {
        for finding in slopos_appkit::audit::audit_node(&app.view(), &[640, 820, 1100], 720) {
            found.push(format!("{name}: {finding}"));
        }
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
}

fn push(out: &mut Vec<(Kind, String)>, kind: Kind, text: &str) {
    if !text.is_empty() {
        out.push((kind, text.to_string()));
    }
}

fn texts(node: &Node<Msg>, out: &mut Vec<(Kind, String)>) {
    let mut push = |kind, text: &str| push(out, kind, text);
    match node {
        Node::Text(text) => push(Kind::Any, &text.text),
        Node::Checkbox { label, .. } => push(Kind::Label, label),
        Node::Switch(switch) => {
            push(Kind::Label, &switch.label);
            push(Kind::Any, &switch.detail);
        }
        Node::Button(button) => push(Kind::Label, &button.label),
        Node::IconButton(button) => push(Kind::Label, &button.label),
        Node::ButtonRow(row) => {
            for button in row.leading.iter().chain(&row.trailing) {
                push(Kind::Label, &button.label);
            }
        }
        Node::List(list) => {
            for (heading, rows) in &list.sections {
                push(Kind::Label, heading);
                for row in rows {
                    push(Kind::Any, &row.title);
                    push(Kind::Any, &row.detail);
                    push(Kind::Any, row.disabled_reason.as_deref().unwrap_or(""));
                }
            }
        }
        Node::Gallery(gallery) => {
            for tile in &gallery.tiles {
                push(Kind::Label, &tile.label);
            }
        }
        Node::PropertyList(list) => {
            for (name, value) in &list.rows {
                self::push(out, Kind::Label, name);
                texts(value, out);
            }
        }
        Node::LineEdit { placeholder, .. } => push(Kind::Any, placeholder),
        Node::Dialog {
            title,
            content,
            footer,
            ..
        } => {
            self::push(out, Kind::Label, title);
            texts(content, out);
            texts(&Node::ButtonRow(footer.clone()), out);
        }
        Node::VStack { children, .. }
        | Node::HStack { children, .. }
        | Node::ZStack { children } => {
            for child in children {
                texts(child, out);
            }
        }
        Node::Padding { child, .. }
        | Node::Expand { child, .. }
        | Node::Background { child, .. }
        | Node::Card { child, .. }
        | Node::SizedBox { child, .. }
        | Node::ScrollView { child, .. } => texts(child, out),
        _ => {}
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Label,
    Any,
}

#[test]
fn every_word_is_the_desktop_s() {
    let mut said = Vec::new();
    for (name, app) in states() {
        let mut found = Vec::new();
        texts(&app.view(), &mut found);
        said.extend(
            found
                .into_iter()
                .map(|(kind, text)| (name.clone(), kind, text)),
        );
    }
    let mut wrong = Vec::new();
    for (name, kind, text) in &said {
        let shape = match (kind, text.ends_with(['.', '?'])) {
            (_, true) => words::sentence(text, &[]),
            (Kind::Label, false) => words::label(text, &[]),
            (Kind::Any, false) => words::plain(text, &[]),
        };
        if let Err(why) = shape {
            wrong.push(format!("{name}: {text:?}: {why}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    assert!(said.len() > 200, "the walk found only {} texts", said.len());
}

#[test]
fn a_search_narrows_the_panels_and_enter_opens_the_first() {
    let mut app = read(Panel::About, admin());
    type_into(&mut app, Msg::Search, "wall");
    assert_eq!(app.shown(), [Panel::Desktop]);
    app.update(Msg::Search(LineEditInput::Key {
        key: Key::Named(NamedKey::Enter),
        modifiers: Modifiers::default(),
    }));
    assert_eq!(app.panel, Panel::Desktop);
    app.update(Msg::Panel(0));
    assert_eq!(app.panel, Panel::Desktop);
    assert_eq!(Panel::named("keyboard"), Some(Panel::Keyboard));
    assert_eq!(Panel::named("display"), None);
}

fn net(msg: network::Msg) -> Msg {
    Msg::Network(msg)
}

fn eth0() -> (String, slopos_net_core::Mac) {
    (
        String::from("eth0"),
        slopos_net_core::Mac([0x52, 0x54, 0, 0x12, 0x34, 0x56]),
    )
}

fn edit_eth0(app: &mut Settings) {
    let (name, mac) = eth0();
    app.update(net(network::Msg::EditAddress(name, mac)));
}

fn unaddressed() -> Settings {
    let mut app = read(Panel::Network, admin());
    let (kept, mut live) = network::sample();
    for adapter in &mut live.adapters {
        adapter.addresses.clear();
        adapter.gateway = None;
    }
    app.update(net(network::Msg::Read(Ok((kept, live)))));
    app
}

fn loaded(effects: &[Effect]) -> NetSettings {
    let [Effect::Ask(ask)] = effects else {
        panic!("{effects:?}");
    };
    assert_eq!(
        (ask.action, ask.args.as_slice()),
        (network::SETTINGS, &["load".to_string()][..])
    );
    NetSettings::parse(std::str::from_utf8(&ask.input).unwrap()).unwrap()
}

fn switched(effects: &[Effect]) -> Vec<String> {
    let [Effect::Ask(ask)] = effects else {
        panic!("{effects:?}");
    };
    assert_eq!(ask.action, network::SWITCH);
    assert!(ask.input.is_empty(), "a switch sends no settings");
    ask.args.clone()
}

fn manual(address: &str, gateway: Option<[u8; 4]>) -> slopos_settings_core::network::Ipv4Method {
    slopos_settings_core::network::Ipv4Method::manual(
        slopos_net_core::Cidr::from_str_bytes(address.as_bytes()).unwrap(),
        gateway.map(slopos_net_core::Ipv4),
    )
    .unwrap()
}

fn shows(app: &Settings, wanted: &str) -> bool {
    let mut found = Vec::new();
    texts(&app.view(), &mut found);
    found.iter().any(|(_, text)| text == wanted)
}

fn ran(status: i32, said: &str) -> Msg {
    Msg::Answered(
        Purpose::Network,
        Asked::Ran {
            status,
            said: said.into(),
            notes: String::new(),
        },
    )
}

#[test]
fn choosing_manual_pins_the_address_the_adapter_has_now() {
    let mut app = read(Panel::Network, admin());
    edit_eth0(&mut app);
    app.update(net(network::Msg::Method(1)));
    let (_, effects) = app.update(net(network::Msg::Save));
    let wanted = loaded(&effects);
    let (name, mac) = eth0();
    assert_eq!(
        wanted.governing(&name, mac).unwrap().ipv4,
        manual("10.0.2.15/24", Some([10, 0, 2, 2]))
    );
    assert_eq!(
        wanted.adapters.len(),
        2,
        "the block for an adapter not here stays"
    );
    let (_, again) = app.update(net(network::Msg::Save));
    assert!(again.is_empty(), "one request at a time");
}

#[test]
fn a_typed_address_and_gateway_go_in_one_request() {
    let mut app = unaddressed();
    edit_eth0(&mut app);
    app.update(net(network::Msg::Method(1)));
    type_into(
        &mut app,
        |i| net(network::Msg::Address(i)),
        "192.168.1.20/24",
    );
    type_into(&mut app, |i| net(network::Msg::Gateway(i)), "192.168.1.1");
    let (_, effects) = app.update(net(network::Msg::Gateway(LineEditInput::Key {
        key: Key::Named(NamedKey::Enter),
        modifiers: Modifiers::default(),
    })));
    let (name, mac) = eth0();
    assert_eq!(
        loaded(&effects).governing(&name, mac).unwrap().ipv4,
        manual("192.168.1.20/24", Some([192, 168, 1, 1])),
        "Enter in a field saves"
    );
}

#[test]
fn a_wrong_address_says_why_once_left_and_holds_save() {
    let mut app = unaddressed();
    edit_eth0(&mut app);
    app.update(net(network::Msg::Method(1)));
    type_into(&mut app, |i| net(network::Msg::Address(i)), "192.168.1.20");
    let why = slopos_settings_core::network::Invalid::NoPrefix.sentence();
    assert!(!shows(&app, why), "not while it is typed");
    app.update(net(network::Msg::Address(LineEditInput::Blur)));
    assert!(shows(&app, why), "once focus leaves it");
    let (_, effects) = app.update(net(network::Msg::Save));
    assert!(effects.is_empty());
    assert!(app.network.dialog().is_some());

    app.update(net(network::Msg::Method(0)));
    let (_, effects) = app.update(net(network::Msg::Save));
    assert!(effects.is_empty(), "automatic is what is kept");
    assert!(
        app.network.dialog().is_none(),
        "a hidden field holds nothing up"
    );
}

#[test]
fn a_switch_asks_for_what_it_turns_alone_and_stays_turned() {
    let mut app = read(Panel::Network, admin());
    let (_, effects) = app.update(net(network::Msg::Networking));
    assert_eq!(switched(&effects), ["networking", "off"]);
    let (name, mac) = eth0();
    let (_, busy) = app.update(net(network::Msg::Adapter(name.clone(), mac)));
    assert!(busy.is_empty(), "one request at a time");
    let (_, effects) = app.update(ran(0, ""));
    assert_eq!(effects, [Effect::Network(network::Effect::Read)]);
    let (_, effects) = app.update(net(network::Msg::Networking));
    assert_eq!(
        switched(&effects),
        ["networking", "on"],
        "turned again from where it was turned, before the read"
    );
    app.update(ran(0, ""));

    let (_, effects) = app.update(net(network::Msg::Adapter(name, mac)));
    assert_eq!(
        switched(&effects),
        ["set", "52:54:00:12:34:56", "off"],
        "an adapter by its hardware address, which outlasts its name"
    );
}

#[test]
fn the_networking_switch_turns_from_what_the_computer_does_now() {
    let mut app = read(Panel::Network, admin());
    let (kept, mut live) = network::sample();
    live.networking = false;
    app.update(net(network::Msg::Read(Ok((kept, live)))));
    let (_, effects) = app.update(net(network::Msg::Networking));
    assert_eq!(
        switched(&effects),
        ["networking", "on"],
        "off by a terminal's ip net, though kept on"
    );
}

#[test]
fn a_refusal_is_said_where_it_was_asked_and_a_no_is_not() {
    let mut app = read(Panel::Network, admin());
    app.update(net(network::Msg::Networking));
    app.update(Msg::Answered(
        Purpose::Network,
        Asked::Refused {
            why: "You didn't allow it.".into(),
            by_person: true,
        },
    ));
    let (_, effects) = app.update(net(network::Msg::Networking));
    assert_eq!(
        switched(&effects),
        ["networking", "off"],
        "back where it was"
    );
    app.update(ran(1, "netctl: eth0 would not go down"));
    assert!(shows(&app, "Networking couldn't be turned off"));
    assert!(shows(&app, "Eth0 would not go down."));

    edit_eth0(&mut app);
    app.update(net(network::Msg::Method(1)));
    app.update(net(network::Msg::Save));
    app.update(ran(1, "netctl: the address is taken"));
    assert!(
        app.network.dialog().is_some(),
        "the dialog stays to try again"
    );
    assert!(shows(&app, "The address is taken."));
    app.update(net(network::Msg::Cancel));
    assert!(app.network.dialog().is_none());
}

#[test]
fn a_save_goes_over_what_was_kept_meanwhile() {
    let mut app = read(Panel::Network, admin());
    app.update(net(network::Msg::EditDns));
    app.update(net(network::Msg::Method(1)));
    type_into(&mut app, |i| net(network::Msg::Servers(i)), "1.1.1.1");
    let (mut kept, live) = network::sample();
    kept.networking = false;
    app.update(net(network::Msg::Read(Ok((kept, live)))));
    let (_, effects) = app.update(net(network::Msg::Save));
    let wanted = loaded(&effects);
    assert_eq!(
        wanted.dns,
        slopos_settings_core::network::Dns::Manual(vec![slopos_net_core::Ipv4([1, 1, 1, 1])])
    );
    assert!(!wanted.networking, "what a terminal kept meanwhile");
}

#[test]
fn forget_takes_only_the_settings_it_names() {
    let mut app = read(Panel::Network, admin());
    let (_, live) = network::sample();
    let kept = NetSettings::parse(
        "adapter 52:54:00:00:00:0b\n  ipv4 auto\nadapter eth7\n  off\nadapter 52:54:00:00:00:0c\n  off\n",
    )
    .unwrap();
    let id = kept.adapters[2].id.clone();
    app.update(net(network::Msg::Read(Ok((kept, live)))));
    let (_, effects) = app.update(net(network::Msg::Forget(id)));
    let left: Vec<String> = loaded(&effects)
        .adapters
        .iter()
        .map(|a| a.id.to_string())
        .collect();
    assert_eq!(left, ["52:54:00:00:00:0b", "eth7"]);
}

#[test]
fn adding_a_user_sends_the_passwords_on_standard_input_only() {
    let mut app = read(Panel::Users, admin());
    app.update(Msg::Users(users::Msg::Add));
    type_into(&mut app, |i| Msg::Users(users::Msg::Name(i)), "grace");
    type_into(&mut app, |i| Msg::Users(users::Msg::New(i)), "pw-g");
    type_into(&mut app, |i| Msg::Users(users::Msg::Again(i)), "pw-g");
    let (_, none) = app.update(Msg::Users(users::Msg::Confirm));
    assert!(
        none.is_empty(),
        "the first user has no password and must choose one"
    );
    type_into(&mut app, |i| Msg::Users(users::Msg::Own(i)), "pw-u");
    type_into(&mut app, |i| Msg::Users(users::Msg::OwnAgain(i)), "pw-u");
    let (_, effects) = app.update(Msg::Users(users::Msg::Confirm));
    let [Effect::Ask(ask)] = &effects[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(ask.action, users::MANAGE);
    assert_eq!(ask.args, ["add", "grace", "--adder-password", "--password"]);
    assert_eq!(&ask.input[..], b"pw-u\npw-u\npw-g\npw-g\n");
    let refused = Asked::Refused {
        why: String::from("You didn't allow it."),
        by_person: true,
    };
    app.update(Msg::Answered(Purpose::Users(users::Request::Add), refused));
    assert!(
        app.users.dialog().is_some(),
        "what was typed stays to try again"
    );
    let (_, again) = app.update(Msg::Users(users::Msg::Confirm));
    assert!(matches!(&again[..], [Effect::Ask(_)]));
    let ran = Asked::Ran {
        status: 0,
        said: String::new(),
        notes: String::new(),
    };
    app.update(Msg::Answered(Purpose::Users(users::Request::Add), ran));
    assert!(app.users.dialog().is_none());
}

#[test]
fn a_taken_or_bad_name_is_refused_before_privd_is_asked() {
    let mut app = read(Panel::Users, admin());
    app.update(Msg::Users(users::Msg::Add));
    type_into(&mut app, |i| Msg::Users(users::Msg::Name(i)), "ada");
    type_into(&mut app, |i| Msg::Users(users::Msg::Own(i)), "x");
    type_into(&mut app, |i| Msg::Users(users::Msg::OwnAgain(i)), "x");
    assert!(app.update(Msg::Users(users::Msg::Confirm)).1.is_empty());
    type_into(&mut app, |i| Msg::Users(users::Msg::Name(i)), "!");
    assert!(app.update(Msg::Users(users::Msg::Confirm)).1.is_empty());
}

#[test]
fn your_own_password_is_changed_through_its_own_action() {
    let mut app = read(Panel::Users, standard());
    app.update(Msg::Users(users::Msg::Password));
    type_into(&mut app, |i| Msg::Users(users::Msg::Current(i)), "old");
    type_into(&mut app, |i| Msg::Users(users::Msg::New(i)), "new");
    type_into(&mut app, |i| Msg::Users(users::Msg::Again(i)), "new");
    let (_, effects) = app.update(Msg::Users(users::Msg::Confirm));
    let [Effect::Ask(ask)] = &effects[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(
        (ask.action, ask.args.as_slice()),
        (users::OWN_PASSWORD, &["passwd".to_string()][..])
    );
    assert_eq!(&ask.input[..], b"old\nnew\nnew\n");
}

fn people(msg: users::Msg) -> Msg {
    Msg::Users(msg)
}

fn answer(request: users::Request, status: i32, said: &str) -> Msg {
    Msg::Answered(
        Purpose::Users(request),
        Asked::Ran {
            status,
            said: said.into(),
            notes: String::new(),
        },
    )
}

fn users_states() -> Vec<(String, Settings)> {
    let mut out = Vec::new();
    for (name, open) in [
        ("users add", users::Msg::Add),
        ("users password", users::Msg::Password),
        ("users rename", users::Msg::Rename),
    ] {
        let mut app = read(Panel::Users, admin());
        app.update(people(open));
        out.push((name.into(), app));
    }
    let listed = |change: fn(&mut Vec<Listed>), whole: bool| {
        let mut app = read(Panel::Users, admin());
        let mut sample = users::sample();
        change(&mut sample);
        app.update(people(users::Msg::Listed(sample, whole)));
        app
    };
    out.push(("users alone".into(), listed(|all| all.truncate(1), true)));
    out.push(("users partly read".into(), listed(|_| {}, false)));
    out.push((
        "users several".into(),
        listed(
            |all| {
                for (uid, name) in [(1002, "grace"), (1003, "a-name-as-long-as-a-name-may-be")] {
                    let mut more = all[1].clone();
                    (more.uid, more.name, more.admin) = (uid, name.into(), uid == 1002);
                    more.password = uid == 1002;
                    all.push(more);
                }
            },
            true,
        ),
    ));

    let ada = || {
        let mut app = read(Panel::Users, admin());
        app.update(people(users::Msg::Open(0)));
        app
    };
    out.push(("users person".into(), ada()));
    let mut first = read(Panel::Users, standard());
    first.update(people(users::Msg::Open(0)));
    out.push(("users person, the first".into(), first));
    let mut remove = ada();
    remove.update(people(users::Msg::Remove));
    remove.update(people(users::Msg::Files(1)));
    out.push(("users remove".into(), remove));
    let mut set = ada();
    set.update(people(users::Msg::Password));
    out.push(("users password, theirs".into(), set));
    let mut turned = ada();
    turned.update(people(users::Msg::Admin));
    out.push(("users turning".into(), turned));
    let mut kept = ada();
    kept.update(people(users::Msg::Admin));
    kept.update(answer(
        users::Request::Admin,
        1,
        "users: the record is locked",
    ));
    out.push(("users not turned".into(), kept));

    let mut wrong = read(Panel::Users, admin());
    wrong.update(people(users::Msg::Add));
    type_into(&mut wrong, |i| people(users::Msg::Name(i)), "Grace");
    type_into(&mut wrong, |i| people(users::Msg::New(i)), "one");
    type_into(&mut wrong, |i| people(users::Msg::Again(i)), "other");
    wrong.update(people(users::Msg::Kind(1)));
    wrong.update(people(users::Msg::Confirm));
    out.push(("users add, wrong".into(), wrong));
    let mut refused = read(Panel::Users, standard());
    refused.update(people(users::Msg::Password));
    type_into(&mut refused, |i| people(users::Msg::Current(i)), "old");
    type_into(&mut refused, |i| people(users::Msg::New(i)), "new");
    type_into(&mut refused, |i| people(users::Msg::Again(i)), "new");
    refused.update(people(users::Msg::Confirm));
    refused.update(answer(
        users::Request::Password,
        1,
        "users: that is not ada's password",
    ));
    out.push(("users password, refused".into(), refused));
    let mut added = read(Panel::Users, admin());
    added.update(people(users::Msg::Add));
    added.update(Msg::Answered(
        Purpose::Users(users::Request::Add),
        Asked::Ran {
            status: 0,
            said: String::new(),
            notes: "grace is uid 1002; restart before they log in\n".into(),
        },
    ));
    out.push(("users added".into(), added));
    out
}

fn asked(effects: &[Effect]) -> Vec<String> {
    let [Effect::Ask(ask)] = effects else {
        panic!("{effects:?}");
    };
    assert_eq!(ask.action, users::MANAGE);
    ask.args.clone()
}

#[test]
fn another_person_s_row_opens_their_page_and_the_panel_opens_on_everyone() {
    let mut app = read(Panel::Users, admin());
    assert!(shows(&app, "Your account") && !shows(&app, "Remove ada"));
    app.update(people(users::Msg::Open(0)));
    assert!(shows(&app, "Remove ada") && !shows(&app, "Your account"));
    let (_, effects) = app.update(people(users::Msg::Autologin));
    assert_eq!(
        asked(&effects),
        ["autologin", "ada"],
        "the page's switches are the person's it shows"
    );
    app.update(answer(users::Request::Autologin, 0, ""));
    app.update(people(users::Msg::Back));
    assert!(shows(&app, "Standard user · Logs in automatically"));
    let (_, effects) = app.update(people(users::Msg::Autologin));
    assert_eq!(
        asked(&effects),
        ["autologin", "user"],
        "one person at most logs in automatically, so yours was turned off"
    );
    app.update(answer(users::Request::Autologin, 0, ""));

    app.update(people(users::Msg::Open(0)));
    app.update(Msg::Panel(1));
    assert!(
        shows(&app, "Your account"),
        "the sidebar opens the panel on everyone"
    );
}

#[test]
fn a_switch_stays_turned_and_only_a_refusal_not_your_own_is_said() {
    fonts();
    let place = |app: &Settings| placed(app, Role::Switch, "Administrator", 920, 660);
    let mut app = read(Panel::Users, admin());
    app.update(people(users::Msg::Open(0)));
    let before = place(&app);
    let (_, effects) = app.update(people(users::Msg::Admin));
    assert_eq!(asked(&effects), ["promote", "ada"]);
    let (_, busy) = app.update(people(users::Msg::Admin));
    assert!(busy.is_empty(), "one request at a time");
    app.update(answer(users::Request::Admin, 0, ""));
    let (_, effects) = app.update(people(users::Msg::Admin));
    assert_eq!(
        asked(&effects),
        ["demote", "ada"],
        "turned again from where it was turned, before the read"
    );
    app.update(Msg::Answered(
        Purpose::Users(users::Request::Admin),
        Asked::Refused {
            why: "You didn't allow it.".into(),
            by_person: true,
        },
    ));
    assert!(!shows(&app, "The account type couldn't be changed"));
    let (_, effects) = app.update(people(users::Msg::Admin));
    assert_eq!(asked(&effects), ["demote", "ada"], "back where it was");
    app.update(answer(
        users::Request::Admin,
        1,
        "users: the record is locked",
    ));
    assert!(shows(&app, "The account type couldn't be changed"));
    assert!(shows(&app, "The record is locked."));
    assert_eq!(place(&app), before, "what is said moved the switch");
}

#[test]
fn only_another_person_who_is_not_the_first_is_removed() {
    let mut app = read(Panel::Users, admin());
    app.update(people(users::Msg::Remove));
    assert!(app.users.dialog().is_none(), "never yourself");
    let mut app = read(Panel::Users, standard());
    app.update(people(users::Msg::Open(0)));
    app.update(people(users::Msg::Remove));
    assert!(app.users.dialog().is_none(), "nor the first user");

    let mut app = read(Panel::Users, admin());
    app.update(people(users::Msg::Open(0)));
    app.update(people(users::Msg::Remove));
    let (_, effects) = app.update(people(users::Msg::Confirm));
    assert_eq!(asked(&effects), ["remove", "ada"], "their files stay");
    app.update(answer(
        users::Request::Remove,
        1,
        "users: ada: is logged in",
    ));
    assert!(shows(&app, "Ada: is logged in."), "said in the dialog");
    app.update(people(users::Msg::Files(1)));
    let (_, effects) = app.update(people(users::Msg::Confirm));
    assert_eq!(asked(&effects), ["remove", "ada", "--home"]);
    app.update(answer(users::Request::Remove, 0, ""));
    assert!(
        app.users.dialog().is_none() && shows(&app, "The user is removed."),
        "back on everyone's page, where it is said"
    );
}

#[test]
fn what_is_wrong_with_a_field_waits_until_it_is_left_or_add_is_pressed() {
    let differ = "The two passwords differ.";
    let mut app = read(Panel::Users, admin());
    app.update(people(users::Msg::Add));
    type_into(&mut app, |i| people(users::Msg::Name(i)), "grace");
    type_into(&mut app, |i| people(users::Msg::New(i)), "one");
    type_into(&mut app, |i| people(users::Msg::Again(i)), "o");
    assert!(!shows(&app, differ), "not while it is typed");
    app.update(people(users::Msg::Again(LineEditInput::Blur)));
    assert!(shows(&app, differ), "once focus leaves it");
    type_into(&mut app, |i| people(users::Msg::Again(i)), "ne");
    assert!(!shows(&app, differ));
    let own = "Choose a password for yourself first.";
    assert!(!shows(&app, own), "nothing asked for before Add is pressed");
    let (_, effects) = app.update(people(users::Msg::Confirm));
    assert!(effects.is_empty() && shows(&app, own));
}

#[test]
fn choosing_a_layout_keeps_it_as_yours_and_the_first_row_hands_back() {
    let mut app = read(Panel::Keyboard, standard());
    let (_, effects) = app.update(Msg::Keyboard(keyboard::Msg::Yours(1)));
    assert_eq!(
        effects,
        [Effect::Keyboard(keyboard::Effect::Keep(Some(
            "de_CH".into()
        )))]
    );
    let (_, effects) = app.update(Msg::Keyboard(keyboard::Msg::Yours(0)));
    assert_eq!(effects, [Effect::Keyboard(keyboard::Effect::Keep(None))]);
}

#[test]
fn the_computer_s_layout_is_privd_s_to_change() {
    let mut app = read(Panel::Keyboard, admin());
    app.update(Msg::Keyboard(keyboard::Msg::Change));
    app.update(Msg::Keyboard(keyboard::Msg::Computer(0)));
    let (_, effects) = app.update(Msg::Keyboard(keyboard::Msg::Confirm));
    let [Effect::Ask(ask)] = &effects[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(ask.args, ["set", "--computer", "de_CH"]);
}

fn placed(app: &Settings, role: Role, name: &str, width: i32, height: i32) -> Rect {
    fn find(widget: &dyn Widget, role: Role, name: &str) -> Option<Rect> {
        if widget.role() == role && widget.accessible_name() == Some(name) {
            return Some(widget.layout_rect());
        }
        widget
            .children()
            .iter()
            .find_map(|child| find(child.as_ref(), role, name))
    }
    let mut root = slopos_appkit::tree::build_widget_tree(&app.view());
    let style = slopos_appkit::StyleSheet::current();
    slopos_appkit::tree::layout_tree(root.as_mut(), Size::new(width, height), &style);
    find(root.as_ref(), role, name).unwrap_or_else(|| panic!("no {role:?} named {name:?}"))
}

#[test]
fn nothing_said_of_a_choice_moves_what_is_chosen_from() {
    fonts();
    let list = |app: &Settings| {
        [(640, 480), (920, 660)]
            .map(|(w, h)| placed(app, Role::List, "Same as the login screen", w, h))
    };
    let mut app = read(Panel::Keyboard, standard());
    let before = list(&app);
    let answers = [
        Ok(()),
        Err(keyboard::Kept::Later),
        Err(keyboard::Kept::Not("Your layout couldn't be kept.".into())),
    ];
    for (row, kept) in [1, 2, 0].into_iter().zip(answers) {
        app.update(Msg::Keyboard(keyboard::Msg::Yours(row)));
        assert_eq!(list(&app), before, "choosing row {row}");
        app.update(Msg::Keyboard(keyboard::Msg::Kept(kept.clone())));
        assert_eq!(list(&app), before, "row {row} answered {kept:?}");
    }
}

#[test]
fn choosing_a_layout_leaves_the_page_where_it_was_scrolled() {
    fonts();
    let window = Rect::new(0, 0, 920, 480);
    let us_row = |shown: &mut Embedded| {
        (0..window.height).step_by(4).find(|&y| {
            let said = shown.click::<Msg>(600, y, true);
            shown.click::<Msg>(600, y, false);
            matches!(said[..], [Msg::Keyboard(keyboard::Msg::Yours(2))])
        })
    };
    let mut app = read(Panel::Keyboard, standard());
    let mut shown = Embedded::new(&app.view(), window);
    shown.wheel::<Msg>(600, 300, 960);
    let before = us_row(&mut shown).expect("the row is in view once scrolled");
    app.update(Msg::Keyboard(keyboard::Msg::Yours(1)));
    shown.rebuild(&app.view(), window);
    assert_eq!(us_row(&mut shown), Some(before), "a choice moved the page");
}

#[test]
fn your_layout_goes_to_the_login_screen_through_privd() {
    let mut app = read(Panel::Keyboard, standard());
    let (_, effects) = app.update(Msg::Keyboard(keyboard::Msg::UseYours));
    assert_eq!(
        effects,
        [],
        "following the login screen, there is none to give"
    );
    app.update(Msg::Keyboard(keyboard::Msg::Yours(1)));
    let (_, effects) = app.update(Msg::Keyboard(keyboard::Msg::UseYours));
    let [Effect::Ask(ask)] = &effects[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(ask.action, keyboard::ACTION);
    assert_eq!(ask.args, ["set", "--computer", "de_CH"]);
    let (_, effects) = app.update(Msg::Keyboard(keyboard::Msg::UseYours));
    assert_eq!(effects, [], "one question at a time");
}

fn lit(app: &Settings) -> Vec<(usize, usize)> {
    fn map(node: &Node<Msg>) -> Option<&KeyboardMap> {
        match node {
            Node::KeyboardMap(map) => Some(map),
            Node::VStack { children, .. }
            | Node::HStack { children, .. }
            | Node::ZStack { children } => children.iter().find_map(map),
            Node::Padding { child, .. }
            | Node::Expand { child, .. }
            | Node::Background { child, .. }
            | Node::Card { child, .. }
            | Node::SizedBox { child, .. }
            | Node::ScrollView { child, .. } => map(child),
            _ => None,
        }
    }
    let view = app.view();
    let keys = map(&view).expect("the page draws the keyboard");
    keys.rows
        .iter()
        .enumerate()
        .flat_map(|(r, row)| {
            row.iter()
                .enumerate()
                .filter(|(_, key)| key.lit)
                .map(move |(at, _)| (r, at))
        })
        .collect()
}

#[test]
fn the_key_a_character_came_from_is_lit_while_trying_the_layout() {
    let mut app = read(Panel::Keyboard, standard());
    app.update(Msg::Keyboard(keyboard::Msg::Yours(1)));
    try_typing(&mut app, "z");
    assert_eq!(lit(&app), [(1, 6)], "QWERTZ types z where QWERTY has Y");
    try_typing(&mut app, "y");
    assert_eq!(lit(&app), [(3, 2)], "and y beside the key ISO adds");
    try_typing(&mut app, " ");
    assert_eq!(lit(&app), [(4, 3)]);
    app.update(Msg::Keyboard(keyboard::Msg::Try(LineEditInput::Blur)));
    assert_eq!(lit(&app), [], "nothing is lit once the field lets go");
    app.update(Msg::Keyboard(keyboard::Msg::Yours(2)));
    try_typing(&mut app, "y");
    assert_eq!(lit(&app), [(1, 6)]);
}

/// What the file holds once a save's changes go into `base`.
fn saved(base: &Prefs, effects: &[Effect]) -> Prefs {
    let Some(Effect::Desktop(desktop::Effect::Save(changes))) = effects.first() else {
        panic!("{effects:?}");
    };
    let mut prefs = base.clone();
    for (key, value) in changes {
        match value {
            Some(value) => prefs.set(*key, value).unwrap(),
            None => prefs.reset(*key),
        }
    }
    prefs
}

#[test]
fn a_gradient_chosen_takes_the_picture_away() {
    let mut app = read(Panel::Desktop, admin());
    let (_, effects) = app.update(Msg::Desktop(desktop::Msg::Choose(2)));
    let mut elsewhere = Prefs::default();
    elsewhere.set(PrefKey::KeyboardLayout, "de_CH").unwrap();
    let prefs = saved(&elsewhere, &effects);
    assert_eq!(
        prefs.get(PrefKey::BackgroundGradient),
        slopos_theme_core::GRADIENTS[2].id
    );
    assert!(prefs.background().picture.is_none());
    assert_eq!(
        prefs.keyboard_layout(),
        Some("de_CH"),
        "a change made elsewhere stays"
    );
    app.update(Msg::Desktop(desktop::Msg::Saved(Ok(prefs.clone()))));
    let (_, effects) = app.update(Msg::Desktop(desktop::Msg::Clock));
    assert!(saved(&prefs, &effects).twelve_hour_clock());
}

#[test]
fn an_answer_before_the_last_save_s_shows_nothing_undone() {
    let mut app = read(Panel::Desktop, admin());
    app.update(Msg::Desktop(desktop::Msg::Choose(1)));
    let (_, second) = app.update(Msg::Desktop(desktop::Msg::Clock));
    app.update(Msg::Desktop(desktop::Msg::Saved(Ok(Prefs::default()))));
    let (_, third) = app.update(Msg::Desktop(desktop::Msg::Clock));
    assert!(!second.is_empty() && !third.is_empty());
    assert!(
        !saved(&Prefs::default(), &third).twelve_hour_clock(),
        "the clock was turned twice, the first answer notwithstanding"
    );
}

#[test]
fn a_picture_of_your_own_is_removed_only_once_you_say_so() {
    let mut app = read(Panel::Desktop, admin());
    app.update(Msg::Desktop(desktop::Msg::Pictures(vec![
        desktop::Picture {
            path: "/home/user/.local/share/backgrounds/dunes.png".into(),
            name: "Dunes".into(),
            own: true,
        },
    ])));
    let (_, asked) = app.update(Msg::Desktop(desktop::Msg::Remove));
    assert!(asked.is_empty() && app.desktop.dialog().is_some());
    let (_, effects) = app.update(Msg::Desktop(desktop::Msg::ConfirmRemove));
    assert!(effects.contains(&Effect::Desktop(desktop::Effect::Remove(
        "/home/user/.local/share/backgrounds/dunes.png".into()
    ))));
}

#[test]
fn a_picture_is_added_by_its_whole_path_and_then_shown() {
    let mut app = read(Panel::Desktop, admin());
    app.update(Msg::Desktop(desktop::Msg::Add));
    type_into(
        &mut app,
        |i| Msg::Desktop(desktop::Msg::Path(i)),
        "/media/beach.png",
    );
    let (_, effects) = app.update(Msg::Desktop(desktop::Msg::Confirm));
    assert_eq!(
        effects,
        [Effect::Desktop(desktop::Effect::Add(
            "/media/beach.png".into()
        ))]
    );
    let kept = "/home/user/.local/share/backgrounds/beach.png".to_string();
    let (_, effects) = app.update(Msg::Desktop(desktop::Msg::Added(Ok(kept.clone()))));
    assert_eq!(
        saved(&Prefs::default(), &effects).background().picture,
        Some(kept.as_str())
    );
}

#[test]
fn a_picture_s_name_is_read_from_its_file() {
    assert_eq!(desktop::picture_name("beach-at_dawn.png"), "Beach at dawn");
    assert_eq!(desktop::picture_name("default.png"), "Default");
    assert_eq!(desktop::picture_name(".png"), "Picture");
}

#[test]
fn what_a_mechanism_says_wrong_is_shown_as_a_sentence() {
    let refused = Asked::Ran {
        status: 1,
        said: "\nusers: that is not ada's password\n".into(),
        notes: String::new(),
    };
    assert_eq!(
        refused.trouble().as_deref(),
        Some("That is not ada's password.")
    );
    let no = Asked::Refused {
        why: "You didn't allow this.".into(),
        by_person: true,
    };
    assert_eq!(no.trouble().as_deref(), Some("You didn't allow it."));
    assert_eq!(
        Asked::Ran {
            status: 0,
            said: String::new(),
            notes: String::new(),
        }
        .trouble(),
        None
    );
}

#[test]
fn what_an_add_notes_beside_it_is_shown_with_it() {
    let mut app = read(Panel::Users, admin());
    app.update(Msg::Users(users::Msg::Add));
    type_into(&mut app, |i| Msg::Users(users::Msg::Name(i)), "grace");
    app.update(Msg::Users(users::Msg::Confirm));
    let noted = Asked::Ran {
        status: 0,
        said: String::new(),
        notes: "grace is uid 1002, at home in /home/grace; restart before they log in\n".into(),
    };
    app.update(Msg::Answered(Purpose::Users(users::Request::Add), noted));
    let mut said = Vec::new();
    texts(&app.view(), &mut said);
    assert!(
        said.iter().any(|(_, t)| t == "The user is added."),
        "{said:?}"
    );
    assert!(
        said.iter().any(|(_, t)| t == "Restart before they log in."),
        "{said:?}"
    );
}

#[test]
fn you_stay_an_administrator_by_another_s_word_alone() {
    let mut people = users::sample();
    people[1].admin = true;
    let mut app = read(Panel::Users, admin());
    app.update(Msg::Users(users::Msg::Listed(people, true)));
    let mut said = Vec::new();
    texts(&app.view(), &mut said);
    assert!(
        said.iter()
            .any(|(_, t)| t == "Another administrator can make you a standard user."),
        "{said:?}"
    );
}

#[test]
fn a_password_typed_is_never_shown_by_debug() {
    let mut input = Input::new();
    input.line("hunter2");
    assert_eq!(format!("{input:?}"), "<8 bytes>");
}

/// `SETTINGS_SHOTS=<dir> cargo test -p slopos-settings-app shots` writes a
/// PNG of every state to look at.
#[test]
fn shots() {
    let Some(dir) = std::env::var_os("SETTINGS_SHOTS") else {
        return;
    };
    fonts();
    for (name, app) in states() {
        let (w, h) = (980u32, 700u32);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        {
            let mut buffer =
                slopos_gfx::DrawBuffer::new(&mut pixels, w, h, (w * 4) as usize, 4).unwrap();
            let rect = slopos_appkit::Rect::new(0, 0, w as i32, h as i32);
            let embedded = slopos_appkit::embed::Embedded::new(&app.view(), rect);
            embedded.paint(&mut buffer, rect);
        }
        let file = std::path::Path::new(&dir).join(format!("{}.png", name.replace(' ', "-")));
        std::fs::write(file, png(w, h, &pixels)).unwrap();
    }
}

fn png(w: u32, h: u32, argb: &[u8]) -> Vec<u8> {
    fn crc(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
    let mut raw = Vec::new();
    for y in 0..h as usize {
        raw.push(0);
        for x in 0..w as usize {
            let at = (y * w as usize + x) * 4;
            raw.extend_from_slice(&[argb[at + 2], argb[at + 1], argb[at], 255]);
        }
    }
    let mut z = vec![0x78, 0x01];
    for (i, block) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        z.push(u8::from(last));
        z.extend_from_slice(&(block.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc(&body).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}
