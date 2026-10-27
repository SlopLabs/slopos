//! The compositor's own surfaces as trees of appkit's components: the lock
//! and login screens, the system menu, the network and clock panels, and the
//! question a choice that ends something asks. Each is the view of a model
//! the compositor holds, which takes the keys; the tree takes the pointer.

use slopos_appkit::constraints::TextAlignment;
use slopos_appkit::{
    Button, ButtonRow, CrossAxisAlignment, Length, List, ListRow, Node, Notice, PropertyList, Text,
};
use slopos_chrome_core::clock::{self, WallTime};
use slopos_chrome_core::gate::{Gate, LOG_IN, PASSWORD, PEOPLE_SHOWN, SWITCH_USER, Target, UNLOCK};
use slopos_chrome_core::menu::{
    Answer, CLOSE, Choice, Confirm, Ending, Menu, RESTART_NOTICE, TRY_AGAIN,
};
use slopos_chrome_core::netstate::{
    GATEWAY_LABEL, IFACE_NO_CARRIER_LABEL, IFACE_OFF_LABEL, IFACE_SEPARATOR, IfaceState,
    NETWORKING, NO_ADDRESS_LABEL, NetPanelModel, PANEL_TITLE, iface_state, indicator_label,
    indicator_state_for,
};
use slopos_net_core::Ipv4;
use slopos_net_core::render::iface_kind;
use slopos_priv_core::words::CANCEL;
use slopos_theme_core::{Icon, Surface, TextRole, Tone, inset, space};

/// How wide the lock and login screens' column is at most.
const GATE_WIDTH: i32 = 360;
/// How wide a panel under the menu bar is at most.
pub const PANEL_WIDTH: i32 = 320;

fn column<M>(children: Vec<Node<M>>, spacing: i32, align: CrossAxisAlignment) -> Node<M> {
    Node::VStack {
        children,
        spacing,
        align,
    }
}

fn centred(text: Text) -> Text {
    text.align(TextAlignment::Center)
}

/// A panel's card, `content` inside its inset.
fn panel<M>(content: Node<M>) -> Node<M> {
    Node::Card {
        surface: Surface::Raised,
        border: true,
        shadow: false,
        child: Box::new(Node::Padding {
            padding: inset::CARD,
            child: Box::new(content),
        }),
    }
}

/// The [`PEOPLE_SHOWN`] people of the login screen the model shows.
fn people(gate: &Gate) -> Node<Target> {
    let first = gate.first_shown();
    let shown = gate.people().iter().skip(first).take(PEOPLE_SHOWN);
    let rows = shown
        .map(|name| ListRow::new(name.clone()).avatar())
        .collect();
    List::new(rows)
        .selected(gate.chosen().and_then(|at| at.checked_sub(first)))
        .on_select(Target::Person)
        .into()
}

/// The lock screen of a desktop, or the login screen, as `gate` stands:
/// the time and date over the person or the people, the password field and
/// the buttons, in a column centred on the screen.
pub fn gate(gate: &Gate, now: Option<&WallTime>) -> Node<Target> {
    let mut form: Vec<Node<Target>> = Vec::new();
    if let Some(now) = now {
        form.push(centred(Text::new(now.time()).role(TextRole::Hero)).into());
        form.push(centred(Text::new(now.day()).role(TextRole::Title)).into());
        form.push(Node::Spacer {
            size: Length::Px(space::XL),
        });
    }
    match (gate.greeter(), gate.person()) {
        (true, _) if !gate.people().is_empty() => form.push(people(gate)),
        (false, Some(name)) => {
            form.push(column(
                vec![Node::Avatar { name: name.into() }],
                0,
                CrossAxisAlignment::Center,
            ));
            form.push(centred(Text::new(name).role(TextRole::Title)).into());
        }
        _ => {}
    }
    form.push(Node::LineEdit {
        text: slopos_appkit::text::ui::BULLET.repeat(gate.typed_chars()),
        placeholder: String::from(PASSWORD),
        caret: gate.typed_chars(),
        focused: false,
        icon: Some(Icon::KeyRound),
        suffix: String::new(),
        invalid: gate.message_is_trouble(),
        tab_stop: true,
        on_input: Some(|_| Target::Field),
    });
    if let Some(message) = gate.message() {
        let tone = match gate.message_is_trouble() {
            true => Tone::Danger,
            false => Tone::Info,
        };
        form.push(Notice::new(tone, message).into());
    }
    let submit = Button::new(gate.submit_label()).on_press(Target::Submit);
    let footer = match gate.greeter() {
        true => ButtonRow::new().confirm(submit),
        false => ButtonRow::new()
            .aside(Button::new(SWITCH_USER).on_press(Target::Switch))
            .confirm(submit),
    };
    form.push(footer.into());
    let form = Node::SizedBox {
        width: Some(Length::Px(GATE_WIDTH)),
        height: None,
        child: Box::new(column(form, space::MD, CrossAxisAlignment::Stretch)),
    };
    column(
        vec![
            Node::Spacer { size: Length::FILL },
            form,
            Node::Spacer { size: Length::FILL },
        ],
        0,
        CrossAxisAlignment::Center,
    )
}

/// What [`gate`] names the control that has focus by: its role's name.
pub fn gate_focus_name(gate: &Gate) -> &str {
    use slopos_chrome_core::gate::Focus;
    match gate.focus() {
        Focus::People => gate
            .people()
            .get(gate.first_shown())
            .map_or(PASSWORD, String::as_str),
        Focus::Field => PASSWORD,
        Focus::Switch => SWITCH_USER,
        Focus::Submit if gate.greeter() => LOG_IN,
        Focus::Submit => UNLOCK,
    }
}

fn choice_icon(choice: Choice) -> Icon {
    match choice {
        Choice::RestartDesktop => Icon::RefreshCw,
        Choice::Lock => Icon::Lock,
        Choice::SwitchUser => Icon::Users,
        Choice::LogOut => Icon::LogOut,
        Choice::Restart => Icon::RotateCcw,
        Choice::PowerOff => Icon::Power,
    }
}

/// Where the menu's entries split into what ends the session and what ends
/// the computer's running.
const SESSION_CHOICES: usize = 3;

/// The system menu of `person`'s desktop: who they are, the update notice
/// when one is due, and the choices, the keyboard's on a row of its own.
pub fn system_menu(menu: &Menu, person: &str) -> Node<Choice> {
    let mut parts: Vec<Node<Choice>> = vec![List::new(vec![ListRow::new(person).avatar()]).into()];
    if menu.notice() {
        parts.push(
            Notice::new(Tone::Info, RESTART_NOTICE)
                .action(
                    Button::new(Choice::RestartDesktop.label()).on_press(Choice::RestartDesktop),
                )
                .into(),
        );
    }
    let cursor = menu
        .cursor()
        .and_then(|c| Choice::MENU.iter().position(|&m| m == c));
    let (session, computer) = Choice::MENU.split_at(SESSION_CHOICES);
    let rows = |choices: &[Choice]| {
        choices
            .iter()
            .map(|&c| ListRow::new(c.label()).icon(choice_icon(c)))
            .collect()
    };
    parts.push(Node::Divider);
    parts.push(
        List::new(rows(session))
            .selected(cursor.filter(|&c| c < SESSION_CHOICES))
            .on_activate(|i| Choice::MENU[i])
            .into(),
    );
    parts.push(Node::Divider);
    parts.push(
        List::new(rows(computer))
            .selected(cursor.and_then(|c| c.checked_sub(SESSION_CHOICES)))
            .on_activate(|i| Choice::MENU[SESSION_CHOICES + i])
            .into(),
    );
    panel(column(parts, space::SM, CrossAxisAlignment::Stretch))
}

/// The role and name [`system_menu`] gives what the keyboard is on.
pub fn menu_focus_name(menu: &Menu) -> Option<(slopos_appkit::traits::Role, &'static str)> {
    use slopos_appkit::traits::Role;
    let at = menu.cursor()?;
    if at == Choice::RestartDesktop {
        return Some((Role::Button, at.label()));
    }
    let first = match Choice::MENU.iter().position(|&c| c == at)? < SESSION_CHOICES {
        true => Choice::MENU[0],
        false => Choice::MENU[SESSION_CHOICES],
    };
    Some((Role::List, first.label()))
}

/// What a press on the network panel asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Toggle;

fn iface_tone(state: IfaceState) -> Tone {
    match state {
        IfaceState::Off => Tone::Disabled,
        IfaceState::NoCarrier => Tone::Danger,
        IfaceState::NoAddress => Tone::Warning,
        IfaceState::Up => Tone::Success,
    }
}

/// The network panel: what the indicator means, the switch when the person
/// may move it, each interface by its state and the gateway.
pub fn network_panel(model: &NetPanelModel, switch: Option<bool>) -> Node<Toggle> {
    let mut parts: Vec<Node<Toggle>> = vec![
        Text::new(PANEL_TITLE)
            .role(TextRole::Label)
            .tone(Tone::Secondary)
            .into(),
        Text::new(indicator_label(indicator_state_for(model)))
            .role(TextRole::Heading)
            .wrap()
            .into(),
    ];
    if let Some(on) = switch {
        parts.push(Node::Checkbox {
            checked: on,
            label: String::from(NETWORKING),
            on_toggle: Some(Toggle),
            enabled: true,
        });
    }
    let ifaces: Vec<Node<Toggle>> = model
        .listed_ifaces()
        .map(|iface| {
            let state = iface_state(iface);
            let mut detail = format!("{}{}", iface_kind(iface.kind.abi()), IFACE_SEPARATOR);
            match state {
                IfaceState::Off => detail.push_str(IFACE_OFF_LABEL),
                IfaceState::NoCarrier => detail.push_str(IFACE_NO_CARRIER_LABEL),
                IfaceState::NoAddress => detail.push_str(NO_ADDRESS_LABEL),
                IfaceState::Up => {
                    detail.push_str(&format!("{}/{}", Ipv4(iface.ipv4), iface.prefix_len))
                }
            }
            Node::HStack {
                children: vec![
                    Node::Icon {
                        icon: Icon::Network,
                        role: TextRole::Body,
                        tone: iface_tone(state),
                    },
                    Node::Expand {
                        weight: 1,
                        child: Box::new(column(
                            vec![
                                Text::new(iface.name_str()).into(),
                                Text::new(detail)
                                    .role(TextRole::Caption)
                                    .tone(Tone::Secondary)
                                    .into(),
                            ],
                            0,
                            CrossAxisAlignment::Stretch,
                        )),
                    },
                ],
                spacing: space::SM,
                align: CrossAxisAlignment::Start,
            }
        })
        .collect();
    if !ifaces.is_empty() || model.gateway != [0; 4] {
        parts.push(Node::Divider);
    }
    parts.extend(ifaces);
    if model.gateway != [0; 4] {
        parts.push(
            PropertyList::new()
                .row(GATEWAY_LABEL, format!("{}", Ipv4(model.gateway)))
                .into(),
        );
    }
    panel(column(parts, space::SM, CrossAxisAlignment::Stretch))
}

/// The date under the clock, and what the clock is set in.
pub fn clock_panel<M: Clone + 'static>(now: Option<&WallTime>) -> Node<M> {
    let date = now.map_or_else(|| String::from(clock::DATE_UNKNOWN), WallTime::date);
    panel(column(
        vec![
            Text::new(date).role(TextRole::Heading).into(),
            Text::new(clock::ZONE)
                .role(TextRole::Caption)
                .tone(Tone::Secondary)
                .into(),
        ],
        space::XS,
        CrossAxisAlignment::Stretch,
    ))
}

/// The question `asked` puts before what it ends, Cancel first.
pub fn confirm(asked: &Confirm) -> Node<Answer> {
    let ending = asked.ending();
    Node::Dialog {
        title: String::from(ending.question()),
        icon: None,
        content: Box::new(Text::new(ending.detail()).wrap().into()),
        footer: ButtonRow::new()
            .cancel(Button::new(CANCEL).on_press(Answer::Cancel))
            .destructive(Button::new(ending.verb()).on_press(Answer::Go)),
        on_dismiss: Some(Answer::Cancel),
        anchor: None,
    }
}

/// The name [`confirm`] gives the button that has focus.
pub fn confirm_focus_name(asked: &Confirm) -> &'static str {
    match asked.focus() {
        Answer::Cancel => CANCEL,
        Answer::Go => asked.ending().verb(),
    }
}

/// What takes the question's place once `ending` did not happen.
pub fn failed(ending: Ending) -> Node<Answer> {
    Node::Dialog {
        title: String::from(ending.failed()),
        icon: None,
        content: Box::new(Text::new(TRY_AGAIN).wrap().into()),
        footer: ButtonRow::new().confirm(Button::new(CLOSE).on_press(Answer::Cancel)),
        on_dismiss: Some(Answer::Cancel),
        anchor: None,
    }
}
