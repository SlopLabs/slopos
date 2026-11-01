use std::sync::Once;

use slopos_appkit::{Key, LineEditInput, Modifiers, NamedKey, Node};
use slopos_settings_core::network::Settings as NetSettings;
use slopos_settings_core::prefs::{Key as PrefKey, Prefs};
use slopos_theme_core::words;

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
            },
            keyboard::Layout {
                name: "us".into(),
                title: "US English (QWERTY)".into(),
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
    let mut manual = read(Panel::Network, admin());
    manual.update(Msg::Network(network::Msg::Method(1)));
    type_into(
        &mut manual,
        |i| Msg::Network(network::Msg::Address(i)),
        "192.168.1.300",
    );
    manual.update(Msg::Network(network::Msg::DnsMethod(1)));
    type_into(
        &mut manual,
        |i| Msg::Network(network::Msg::Dns(i)),
        "1.1.1.1 nonsense",
    );
    out.push(("network manual, wrong".into(), manual));

    for (name, open) in [
        ("users add", users::Msg::Add),
        ("users password", users::Msg::Password),
        ("users rename", users::Msg::Rename),
    ] {
        let mut app = read(Panel::Users, admin());
        app.update(Msg::Users(open));
        out.push((name.into(), app));
    }
    let mut remove = read(Panel::Users, admin());
    remove.update(Msg::Users(users::Msg::Choose(1)));
    remove.update(Msg::Users(users::Msg::Remove));
    out.push(("users remove".into(), remove));

    let mut computer = read(Panel::Keyboard, admin());
    computer.update(Msg::Keyboard(keyboard::Msg::Change));
    out.push(("keyboard computer".into(), computer));

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
        Node::Button(button) => push(Kind::Label, &button.label),
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

#[test]
fn applying_a_manual_address_asks_netctl_to_load_the_settings_as_one() {
    let mut app = read(Panel::Network, admin());
    app.update(Msg::Network(network::Msg::Method(1)));
    type_into(
        &mut app,
        |i| Msg::Network(network::Msg::Address(i)),
        "192.168.1.20/24",
    );
    type_into(
        &mut app,
        |i| Msg::Network(network::Msg::Gateway(i)),
        "192.168.1.1",
    );
    let (_, effects) = app.update(Msg::Network(network::Msg::Apply));
    let [Effect::Ask(ask)] = &effects[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(
        (ask.action, ask.args.as_slice()),
        (network::ACTION, &["load".to_string()][..])
    );
    let loaded = NetSettings::parse(std::str::from_utf8(&ask.input).unwrap()).unwrap();
    let eth0 = slopos_net_core::Mac([0x52, 0x54, 0, 0x12, 0x34, 0x56]);
    let governing = loaded.governing("eth0", eth0).unwrap();
    assert_eq!(
        governing.ipv4,
        slopos_settings_core::network::Ipv4Method::manual(
            slopos_net_core::Cidr::from_str_bytes(b"192.168.1.20/24").unwrap(),
            Some(slopos_net_core::Ipv4([192, 168, 1, 1]))
        )
        .unwrap()
    );
    assert_eq!(
        loaded.adapters.len(),
        2,
        "the block for an adapter not here stays"
    );
    let (_, again) = app.update(Msg::Network(network::Msg::Apply));
    assert!(again.is_empty(), "one request at a time");
}

fn loaded(effects: &[Effect]) -> NetSettings {
    let [Effect::Ask(ask)] = effects else {
        panic!("{effects:?}");
    };
    NetSettings::parse(std::str::from_utf8(&ask.input).unwrap()).unwrap()
}

#[test]
fn an_edit_goes_over_what_was_kept_meanwhile() {
    let mut app = read(Panel::Network, admin());
    app.update(Msg::Network(network::Msg::Networking));
    let (mut kept, live) = network::sample();
    kept.dns =
        slopos_settings_core::network::Dns::Manual(vec![slopos_net_core::Ipv4([9, 9, 9, 9])]);
    app.update(Msg::Network(network::Msg::Read(Ok((kept.clone(), live)))));
    let (_, effects) = app.update(Msg::Network(network::Msg::Apply));
    let wanted = loaded(&effects);
    assert!(!wanted.networking, "the person's edit");
    assert_eq!(wanted.dns, kept.dns, "what a terminal kept meanwhile");
}

#[test]
fn reading_again_with_no_adapter_keeps_what_was_typed() {
    let mut app = read(Panel::Network, admin());
    let (kept, mut live) = network::sample();
    live.adapters.retain(|a| a.loopback);
    app.update(Msg::Network(network::Msg::Read(Ok((
        kept.clone(),
        live.clone(),
    )))));
    app.update(Msg::Network(network::Msg::DnsMethod(1)));
    type_into(&mut app, |i| Msg::Network(network::Msg::Dns(i)), "1.1.1.1");
    app.update(Msg::Network(network::Msg::Read(Ok((kept, live)))));
    let (_, effects) = app.update(Msg::Network(network::Msg::Apply));
    assert_eq!(
        loaded(&effects).dns,
        slopos_settings_core::network::Dns::Manual(vec![slopos_net_core::Ipv4([1, 1, 1, 1])])
    );
}

#[test]
fn forget_takes_the_settings_chosen_whatever_row_they_move_to() {
    let mut app = read(Panel::Network, admin());
    let (_, live) = network::sample();
    let kept = NetSettings::parse(
        "adapter 52:54:00:00:00:0b\n  ipv4 auto\nadapter 52:54:00:00:00:0c\n  off\n",
    )
    .unwrap();
    app.update(Msg::Network(network::Msg::Read(Ok((
        kept.clone(),
        live.clone(),
    )))));
    app.update(Msg::Network(network::Msg::Kept(1)));
    let mut moved = kept.clone();
    moved.adapters.insert(
        0,
        NetSettings::parse("adapter eth7\n")
            .unwrap()
            .adapters
            .remove(0),
    );
    app.update(Msg::Network(network::Msg::Read(Ok((moved, live)))));
    let (_, effects) = app.update(Msg::Network(network::Msg::Forget));
    let left: Vec<String> = loaded(&effects)
        .adapters
        .iter()
        .map(|a| a.id.to_string())
        .collect();
    assert_eq!(left, ["eth7", "52:54:00:00:00:0b"]);
}

#[test]
fn a_field_hidden_with_its_adapter_holds_nothing_up() {
    let mut app = read(Panel::Network, admin());
    app.update(Msg::Network(network::Msg::Method(1)));
    app.update(Msg::Network(network::Msg::Enabled));
    let (_, effects) = app.update(Msg::Network(network::Msg::Apply));
    let eth0 = slopos_net_core::Mac([0x52, 0x54, 0, 0x12, 0x34, 0x56]);
    let governing = loaded(&effects).governing("eth0", eth0).cloned().unwrap();
    assert!(!governing.enabled);
    assert_eq!(
        governing.ipv4,
        slopos_settings_core::network::Ipv4Method::Auto
    );
}

#[test]
fn a_wrong_address_keeps_apply_from_asking() {
    let mut app = read(Panel::Network, admin());
    app.update(Msg::Network(network::Msg::Method(1)));
    type_into(
        &mut app,
        |i| Msg::Network(network::Msg::Address(i)),
        "192.168.1.20",
    );
    let (_, effects) = app.update(Msg::Network(network::Msg::Apply));
    assert!(effects.is_empty());
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
    app.update(Msg::Users(users::Msg::Choose(1)));
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
    app.update(Msg::Users(users::Msg::Choose(0)));
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
