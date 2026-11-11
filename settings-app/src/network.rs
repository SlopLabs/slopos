//! Network: whether this computer is online, each adapter, its address and
//! the DNS servers. A switch is one [`SWITCH`] request, run at once; an
//! address or the DNS servers change in a dialog saved as one [`SETTINGS`]
//! `netctl load`.

use slopos_appkit::{
    ActionRow, Button, ButtonRow, Dialog, Group, LineEditInput, List, ListRow, Node, Notice,
    PropertyList, Section, Switch, Text, Tone, column, row,
};
use slopos_net_core::{Ipv4, Mac};
use slopos_settings_core::network::{
    self as net, AdapterId, Dns, Invalid, Ipv4Method, Live, LiveAdapter, Settings,
};
use slopos_theme_core::{Icon, TextRole, space};

use crate::field::{Field, Left, caption_text, locked_text, with_caption};
use crate::{Ask, Asked, Effect as AppEffect, Input, Msg as AppMsg, Panel, Purpose};

/// On or off, which an administrator runs unasked.
pub const SWITCH: &str = "org.slopos.network.switch";
/// Every other change, which an administrator confirms.
pub const SETTINGS: &str = "org.slopos.network.settings";

/// How often the page reads the network again while it shows it.
pub const REFRESH_MS: u64 = 2000;

#[derive(Clone, Debug)]
pub enum Msg {
    /// What `/etc/network` keeps and the network now, or why either could
    /// not be read.
    Read(Result<(Settings, Live), String>),
    Networking,
    /// With its hardware address, since a name passes to the next adapter.
    Adapter(String, Mac),
    EditAddress(String, Mac),
    EditDns,
    Method(usize),
    Address(LineEditInput),
    Gateway(LineEditInput),
    Servers(LineEditInput),
    Save,
    Cancel,
    Forget(AdapterId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Read the settings kept and the network; answer [`Msg::Read`].
    Read,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Request {
    Networking(bool),
    Adapter { name: String, mac: Mac, on: bool },
    Forget(AdapterId),
    Save,
}

impl Request {
    fn failed(&self) -> &'static str {
        match self {
            Request::Networking(true) => "Networking couldn't be turned on",
            Request::Networking(false) => "Networking couldn't be turned off",
            Request::Adapter { on: true, .. } => "The adapter couldn't be turned on",
            Request::Adapter { on: false, .. } => "The adapter couldn't be turned off",
            Request::Forget(_) => "The settings couldn't be forgotten",
            Request::Save => "The settings couldn't be saved",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Typed {
    field: Field,
}

impl Typed {
    fn with(text: String) -> Typed {
        Typed {
            field: Field::with(text),
        }
    }

    fn text(&self) -> &str {
        self.field.text().trim()
    }

    fn edit(&mut self, input: LineEditInput) -> Left {
        self.field.edit(input)
    }
}

#[derive(Debug)]
enum Form {
    Address {
        name: String,
        mac: Mac,
        manual: bool,
        address: Typed,
        gateway: Typed,
    },
    Dns {
        manual: bool,
        servers: Typed,
    },
}

impl Form {
    fn address_error(&self) -> Option<Invalid> {
        match self {
            Form::Address {
                manual: true,
                address,
                ..
            } => net::parse_address(address.text()).err(),
            _ => None,
        }
    }

    fn gateway_error(&self) -> Option<Invalid> {
        let Form::Address {
            manual: true,
            address,
            gateway,
            ..
        } = self
        else {
            return None;
        };
        if gateway.text().is_empty() {
            return None;
        }
        let gateway = match net::parse_gateway(gateway.text()) {
            Ok(gateway) => gateway,
            Err(why) => return Some(why),
        };
        let address = net::parse_address(address.text()).ok()?;
        net::check_gateway(address, gateway).err()
    }

    fn servers_error(&self) -> Option<Invalid> {
        let Form::Dns {
            manual: true,
            servers,
        } = self
        else {
            return None;
        };
        match net::parse_dns(servers.text()) {
            Ok(Dns::Auto) => Some(Invalid::DnsServer),
            Ok(Dns::Manual(_)) => None,
            Err(why) => Some(why),
        }
    }

    fn over(&self, kept: &Settings) -> Option<Settings> {
        if self.address_error().is_some()
            || self.gateway_error().is_some()
            || self.servers_error().is_some()
        {
            return None;
        }
        let mut wanted = kept.clone();
        match self {
            Form::Address {
                name,
                mac,
                manual,
                address,
                gateway,
            } => {
                let (enabled, was) = governing(kept, name, *mac);
                let ipv4 = match manual {
                    false => Ipv4Method::Auto,
                    true => {
                        let address = net::parse_address(address.text()).ok()?;
                        let gateway = match gateway.text().is_empty() {
                            true => None,
                            false => Some(net::parse_gateway(gateway.text()).ok()?),
                        };
                        Ipv4Method::manual(address, gateway).ok()?
                    }
                };
                if ipv4 != was {
                    wanted.set_for(name, *mac, enabled, ipv4);
                }
            }
            Form::Dns { manual, servers } => {
                wanted.dns = match manual {
                    true => net::parse_dns(servers.text()).ok()?,
                    false => Dns::Auto,
                };
            }
        }
        Some(wanted)
    }
}

struct Editing {
    form: Form,
    tried: bool,
    trouble: Option<String>,
}

pub struct Network {
    admin: bool,
    kept: Option<Settings>,
    live: Option<Live>,
    problem: Option<String>,
    /// With the settings it keeps once done.
    asking: Option<(Request, Settings)>,
    trouble: Option<(Request, String)>,
    editing: Option<Editing>,
}

fn governing(kept: &Settings, name: &str, mac: Mac) -> (bool, Ipv4Method) {
    kept.governing(name, mac)
        .map_or((true, Ipv4Method::Auto), |a| (a.enabled, a.ipv4))
}

fn kind_name(adapter: &LiveAdapter) -> String {
    format!("Wired network {}", adapter.name)
}

fn addresses(adapter: &LiveAdapter) -> Vec<String> {
    adapter
        .addresses
        .iter()
        .filter(|a| !a.link_local)
        .map(|a| a.address.to_string())
        .collect()
}

fn gateway(adapter: &LiveAdapter) -> Option<Ipv4> {
    adapter.gateway.filter(|g| !g.is_unspecified())
}

fn joined(servers: &[Ipv4]) -> String {
    servers
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn reach(adapter: &LiveAdapter) -> u8 {
    match (adapter.up, adapter.carrier) {
        (false, _) => 0,
        (true, false) => 1,
        _ if addresses(adapter).is_empty() => 2,
        _ if gateway(adapter).is_none() => 3,
        _ => 4,
    }
}

struct Standing {
    icon: Icon,
    tone: Tone,
    title: String,
    detail: String,
}

impl Standing {
    fn new(icon: Icon, tone: Tone, title: &str, detail: impl Into<String>) -> Standing {
        Standing {
            icon,
            tone,
            title: String::from(title),
            detail: detail.into(),
        }
    }
}

impl Network {
    pub fn new(admin: bool) -> Network {
        Network {
            admin,
            kept: None,
            live: None,
            problem: None,
            asking: None,
            trouble: None,
            editing: None,
        }
    }

    fn adapters(&self) -> Vec<&LiveAdapter> {
        self.live
            .as_ref()
            .map(|live| live.adapters.iter().filter(|a| !a.loopback).collect())
            .unwrap_or_default()
    }

    fn adapter(&self, name: &str, mac: Mac) -> Option<&LiveAdapter> {
        self.adapters()
            .into_iter()
            .find(|a| a.name == name && a.mac == mac)
    }

    /// The settings kept for adapters that are not here.
    fn absent_kept(&self) -> Vec<&net::Adapter> {
        let (Some(kept), Some(live)) = (&self.kept, &self.live) else {
            return Vec::new();
        };
        kept.adapters
            .iter()
            .filter(|k| !live.adapters.iter().any(|a| k.id.names(&a.name, a.mac)))
            .collect()
    }

    fn idle(&self) -> bool {
        self.kept.is_some() && self.asking.is_none() && self.problem.is_none()
    }

    fn pending(&self) -> Option<&Request> {
        self.asking.as_ref().map(|(request, _)| request)
    }

    /// What the computer does now, which `ip net` may have left unlike what
    /// is kept.
    fn networking_on(&self) -> bool {
        match (self.pending(), &self.live) {
            (Some(Request::Networking(on)), _) => *on,
            (_, Some(live)) => live.networking,
            (_, None) => self.kept.as_ref().is_some_and(|k| k.networking),
        }
    }

    fn adapter_on(&self, adapter: &LiveAdapter) -> bool {
        match self.pending() {
            Some(Request::Adapter { name, mac, on })
                if *name == adapter.name && *mac == adapter.mac =>
            {
                *on
            }
            _ => self
                .kept
                .as_ref()
                .is_none_or(|k| governing(k, &adapter.name, adapter.mac).0),
        }
    }

    fn ask(&mut self, request: Request, wanted: Settings) -> Vec<AppEffect> {
        self.trouble = None;
        let turned = |on: bool| String::from(if on { "on" } else { "off" });
        let (action, args, input) = match &request {
            Request::Networking(on) => (
                SWITCH,
                vec![String::from("networking"), turned(*on)],
                Input::default(),
            ),
            Request::Adapter { name, mac, on } => {
                let adapter = match mac.is_zero() {
                    true => name.clone(),
                    false => mac.to_string(),
                };
                (
                    SWITCH,
                    vec![String::from("set"), adapter, turned(*on)],
                    Input::default(),
                )
            }
            Request::Forget(_) | Request::Save => {
                (SETTINGS, vec![String::from("load")], wanted.render().into())
            }
        };
        self.asking = Some((request, wanted));
        vec![AppEffect::Ask(Ask {
            purpose: Purpose::Network,
            action,
            args,
            input,
        })]
    }

    fn to_change(&self) -> Option<Settings> {
        self.kept.clone().filter(|_| self.idle())
    }

    fn turn_networking(&mut self) -> Vec<AppEffect> {
        let Some(mut wanted) = self.to_change() else {
            return Vec::new();
        };
        let on = !self.networking_on();
        wanted.networking = on;
        self.ask(Request::Networking(on), wanted)
    }

    fn turn_adapter(&mut self, name: String, mac: Mac) -> Vec<AppEffect> {
        let Some(mut wanted) = self.to_change() else {
            return Vec::new();
        };
        let (enabled, ipv4) = governing(&wanted, &name, mac);
        wanted.set_for(&name, mac, !enabled, ipv4);
        self.ask(
            Request::Adapter {
                name,
                mac,
                on: !enabled,
            },
            wanted,
        )
    }

    fn forget(&mut self, id: AdapterId) -> Vec<AppEffect> {
        let Some(mut wanted) = self.to_change() else {
            return Vec::new();
        };
        wanted.adapters.retain(|a| a.id != id);
        match self.kept.as_ref() == Some(&wanted) {
            true => Vec::new(),
            false => self.ask(Request::Forget(id), wanted),
        }
    }

    fn open(&mut self, form: Form) {
        if self.idle() {
            self.editing = Some(Editing {
                form,
                tried: false,
                trouble: None,
            });
        }
    }

    fn open_address(&mut self, name: String, mac: Mac) {
        let Some(kept) = &self.kept else {
            return;
        };
        let form = match governing(kept, &name, mac).1 {
            Ipv4Method::Auto => Form::Address {
                name,
                mac,
                manual: false,
                address: Typed::default(),
                gateway: Typed::default(),
            },
            Ipv4Method::Manual { address, gateway } => Form::Address {
                name,
                mac,
                manual: true,
                address: Typed::with(address.to_string()),
                gateway: Typed::with(gateway.map(|g| g.to_string()).unwrap_or_default()),
            },
        };
        self.open(form);
    }

    fn open_dns(&mut self) {
        let Some(kept) = &self.kept else {
            return;
        };
        let form = match &kept.dns {
            Dns::Auto => Form::Dns {
                manual: false,
                servers: Typed::default(),
            },
            Dns::Manual(servers) => Form::Dns {
                manual: true,
                servers: Typed::with(
                    servers
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
            },
        };
        self.open(form);
    }

    /// A manual address starts from the one the adapter holds now.
    fn method(&mut self, chosen: bool) {
        let now = match &self.editing {
            Some(Editing {
                form: Form::Address { name, mac, .. },
                ..
            }) => self.adapter(name, *mac).map(|a| {
                let address = addresses(a).into_iter().next().unwrap_or_default();
                let gateway = gateway(a).map(|g| g.to_string()).unwrap_or_default();
                (address, gateway)
            }),
            _ => None,
        };
        let Some(editing) = &mut self.editing else {
            return;
        };
        match &mut editing.form {
            Form::Address {
                manual,
                address,
                gateway,
                ..
            } => {
                if chosen && !*manual && address.text().is_empty() && gateway.text().is_empty() {
                    if let Some((now_address, now_gateway)) = now {
                        *address = Typed::with(now_address);
                        *gateway = Typed::with(now_gateway);
                    }
                }
                *manual = chosen;
            }
            Form::Dns { manual, .. } => *manual = chosen,
        }
    }

    fn typed(
        &mut self,
        input: LineEditInput,
        which: fn(&mut Form) -> Option<&mut Typed>,
    ) -> Vec<AppEffect> {
        let Some(typed) = self.editing.as_mut().and_then(|e| which(&mut e.form)) else {
            return Vec::new();
        };
        match typed.edit(input) {
            Left::Enter => self.save(),
            Left::Escape => self.update(Msg::Cancel),
            Left::Nothing => Vec::new(),
        }
    }

    fn save(&mut self) -> Vec<AppEffect> {
        if !self.idle() {
            return Vec::new();
        }
        let (Some(kept), Some(editing)) = (&self.kept, &mut self.editing) else {
            return Vec::new();
        };
        editing.tried = true;
        editing.trouble = None;
        let Some(wanted) = editing.form.over(kept) else {
            return Vec::new();
        };
        match wanted == *kept {
            true => {
                self.editing = None;
                Vec::new()
            }
            false => self.ask(Request::Save, wanted),
        }
    }

    pub fn update(&mut self, msg: Msg) -> Vec<AppEffect> {
        match msg {
            Msg::Read(Ok((kept, live))) => {
                self.kept = Some(kept);
                self.live = Some(live);
                self.problem = None;
            }
            Msg::Read(Err(why)) => self.problem = Some(why),
            Msg::Networking => return self.turn_networking(),
            Msg::Adapter(name, mac) => return self.turn_adapter(name, mac),
            Msg::Forget(id) => return self.forget(id),
            Msg::EditAddress(name, mac) => self.open_address(name, mac),
            Msg::EditDns => self.open_dns(),
            Msg::Method(row) => self.method(row == 1),
            Msg::Address(input) => {
                return self.typed(input, |form| match form {
                    Form::Address { address, .. } => Some(address),
                    Form::Dns { .. } => None,
                });
            }
            Msg::Gateway(input) => {
                return self.typed(input, |form| match form {
                    Form::Address { gateway, .. } => Some(gateway),
                    Form::Dns { .. } => None,
                });
            }
            Msg::Servers(input) => {
                return self.typed(input, |form| match form {
                    Form::Dns { servers, .. } => Some(servers),
                    Form::Address { .. } => None,
                });
            }
            Msg::Save => return self.save(),
            Msg::Cancel => {
                if self.asking.is_none() {
                    self.editing = None;
                }
            }
        }
        Vec::new()
    }

    /// Whether `read` is what the page shows already, so reading it again
    /// changes nothing.
    pub fn holds(&self, (kept, live): &(Settings, Live)) -> bool {
        self.problem.is_none()
            && self.kept.as_ref() == Some(kept)
            && self.live.as_ref() == Some(live)
    }

    pub fn set_admin(&mut self, admin: bool) {
        self.admin = admin;
    }

    /// A done request keeps what it asked for, so nothing flicks back before
    /// the next read; a declined one says nothing.
    pub fn answered(&mut self, asked: Asked) -> Vec<AppEffect> {
        let read = vec![AppEffect::Network(Effect::Read)];
        let Some((request, wanted)) = self.asking.take() else {
            return read;
        };
        let declined = matches!(
            asked,
            Asked::Refused {
                by_person: true,
                ..
            }
        );
        match (asked.trouble(), declined) {
            (None, _) => {
                self.kept = Some(wanted);
                match request {
                    Request::Save => self.editing = None,
                    Request::Networking(on) => {
                        if let Some(live) = &mut self.live {
                            live.networking = on;
                        }
                    }
                    Request::Adapter { .. } | Request::Forget(_) => {}
                }
            }
            (Some(_), true) => {}
            (Some(why), false) => match (request, &mut self.editing) {
                (Request::Save, Some(editing)) => editing.trouble = Some(why),
                (Request::Save, None) => {}
                (request, _) => self.trouble = Some((request, why)),
            },
        }
        read
    }

    fn standing(&self) -> Standing {
        let Some(live) = &self.live else {
            return match &self.problem {
                Some(why) => Standing::new(
                    Icon::TriangleAlert,
                    Tone::Danger,
                    "The network settings can't be read",
                    why.clone(),
                ),
                None => Standing::new(Icon::RefreshCw, Tone::Secondary, "Reading the network", ""),
            };
        };
        if !live.networking {
            return Standing::new(
                Icon::Power,
                Tone::Secondary,
                "Networking is off",
                "Turn it on to reach other computers and the internet.",
            );
        }
        let adapters = self.adapters();
        let Some(best) = adapters.iter().rev().max_by_key(|a| reach(a)).copied() else {
            return Standing::new(
                Icon::EthernetPort,
                Tone::Secondary,
                "No network adapter",
                "Plug one in, such as a USB Ethernet adapter.",
            );
        };
        let lone = adapters.len() == 1;
        let name = &best.name;
        let address = best
            .addresses
            .iter()
            .find(|a| !a.link_local)
            .map(|a| a.address.addr.to_string())
            .unwrap_or_default();
        match reach(best) {
            4 => Standing::new(
                Icon::Network,
                Tone::Success,
                "Connected",
                format!("Through {name}, with the address {address}."),
            ),
            3 => Standing::new(
                Icon::Network,
                Tone::Warning,
                "Local network only",
                format!("Only this network can be reached: {name} has no gateway."),
            ),
            2 => Standing::new(
                Icon::RefreshCw,
                Tone::Info,
                "Connecting",
                format!("Waiting for the network to give {name} an address."),
            ),
            1 if lone => Standing::new(
                Icon::Unplug,
                Tone::Warning,
                "Not connected",
                format!("Plug a network cable into {name}."),
            ),
            1 => Standing::new(
                Icon::Unplug,
                Tone::Warning,
                "Not connected",
                "Plug a network cable into an adapter.",
            ),
            _ if lone => Standing::new(
                Icon::Power,
                Tone::Secondary,
                "Not connected",
                format!("The adapter {name} is turned off."),
            ),
            _ => Standing::new(
                Icon::Power,
                Tone::Secondary,
                "Not connected",
                "Every adapter is turned off.",
            ),
        }
    }

    fn adapter_status(&self, adapter: &LiveAdapter) -> &'static str {
        if self.live.as_ref().is_some_and(|l| !l.networking) {
            return "Off while networking is off";
        }
        match reach(adapter) {
            0 => "Off",
            1 => "No cable plugged in",
            2 => "Waiting for an address",
            _ => "Connected",
        }
    }

    fn trouble_where(&self, asked: impl Fn(&Request) -> bool) -> Option<Node<AppMsg>> {
        let (request, why) = self.trouble.as_ref().filter(|(r, _)| asked(r))?;
        Some(
            Notice::new(Tone::Danger, request.failed())
                .body(why.clone())
                .into(),
        )
    }

    pub fn view(&self) -> Node<AppMsg> {
        let about = "Whether this computer is online, and how it connects.";
        let mut parts: Vec<Node<AppMsg>> = Vec::new();
        if let (Some(_), Some(problem)) = (&self.kept, &self.problem) {
            let notice = Notice::new(Tone::Danger, "The network settings can't be read again");
            parts.push(notice.body(problem.clone()).into());
        }
        parts.push(self.overview());
        if self.kept.is_some() {
            for adapter in self.adapters() {
                parts.push(self.adapter_section(adapter));
            }
            parts.push(self.dns_section());
            let absent = self.absent_kept();
            if !absent.is_empty() {
                parts.push(self.absent_section(&absent));
            }
        }
        crate::page(Panel::Network, about, parts, None)
    }

    fn overview(&self) -> Node<AppMsg> {
        let standing = self.standing();
        let mut words: Vec<Node<AppMsg>> = vec![
            Text::new(standing.title)
                .role(TextRole::Title)
                .wrap()
                .into(),
        ];
        if !standing.detail.is_empty() {
            words.push(
                Text::new(standing.detail)
                    .tone(Tone::Secondary)
                    .wrap()
                    .into(),
            );
        }
        let status = row(
            space::LG,
            vec![
                Node::Icon {
                    icon: standing.icon,
                    role: TextRole::Display,
                    tone: standing.tone,
                },
                Node::Expand {
                    weight: 1,
                    child: Box::new(column(space::XS, words)),
                },
            ],
        );
        let mut group = Group::new().row(status);
        if self.kept.is_some() {
            group = group.row(
                Switch::new("Networking", self.networking_on())
                    .detail("Every adapter stops while it is off.")
                    .on_toggle(AppMsg::Network(Msg::Networking))
                    .enabled(self.idle()),
            );
        }
        let mut parts: Vec<Node<AppMsg>> = vec![group.into()];
        parts.extend(self.trouble_where(|r| matches!(r, Request::Networking(_))));
        if !self.admin && self.kept.is_some() {
            parts.push(locked_text("Changes ask for an administrator's password."));
        }
        column(space::SM, parts)
    }

    fn edit_button(&self, msg: Msg) -> Button<AppMsg> {
        Button::new("Edit…")
            .on_press(AppMsg::Network(msg))
            .enabled(self.idle())
    }

    fn adapter_section(&self, adapter: &LiveAdapter) -> Node<AppMsg> {
        let (name, mac) = (adapter.name.clone(), adapter.mac);
        let switch = Switch::new("Use this adapter", self.adapter_on(adapter))
            .detail(self.adapter_status(adapter))
            .on_toggle(AppMsg::Network(Msg::Adapter(name.clone(), mac)))
            .enabled(self.idle());
        let mut facts = PropertyList::new();
        let held = addresses(adapter);
        if !held.is_empty() {
            facts = facts.row("Address", held.join(", "));
        }
        if let Some(gateway) = gateway(adapter) {
            facts = facts.row("Gateway", gateway.to_string());
        }
        if !mac.is_zero() {
            facts = facts.row("Hardware address", mac.to_string());
        }
        let ipv4 = self
            .kept
            .as_ref()
            .map_or(Ipv4Method::Auto, |k| governing(k, &name, mac).1);
        let how = match ipv4 {
            Ipv4Method::Auto => {
                ActionRow::new("Automatic address").detail("The network gives it one.")
            }
            Ipv4Method::Manual { address, gateway } => {
                let detail = match gateway {
                    Some(gateway) => format!("{address}, gateway {gateway}"),
                    None => format!("{address}, no gateway"),
                };
                ActionRow::new("Manual address").detail(detail)
            }
        };
        let mut group = Group::new().row(switch);
        if !facts.rows.is_empty() {
            group = group.row(facts);
        }
        group = group.row(how.trailing(self.edit_button(Msg::EditAddress(name.clone(), mac))));
        let mut parts: Vec<Node<AppMsg>> = vec![group.into()];
        parts.extend(self.trouble_where(
            |r| matches!(r, Request::Adapter { name: n, mac: m, .. } if *n == name && *m == mac),
        ));
        Section::new(kind_name(adapter), column(space::SM, parts)).into()
    }

    fn dns_section(&self) -> Node<AppMsg> {
        let in_use = self
            .live
            .as_ref()
            .filter(|live| !live.dns.servers.is_empty())
            .map_or_else(
                || String::from("None yet"),
                |live| joined(&live.dns.servers),
            );
        let manual = self
            .kept
            .as_ref()
            .is_some_and(|k| matches!(k.dns, Dns::Manual(_)));
        let how = match manual {
            false => ActionRow::new("Automatic").detail("Each network names its own."),
            true => ActionRow::new("Manual")
                .detail("Every adapter asks these, whatever the network names."),
        };
        let group = Group::new()
            .row(PropertyList::new().row("In use", in_use))
            .row(how.trailing(self.edit_button(Msg::EditDns)));
        Section::new("DNS servers", group).into()
    }

    fn absent_section(&self, absent: &[&net::Adapter]) -> Node<AppMsg> {
        let mut group = Group::new();
        for adapter in absent {
            let mut how = match adapter.ipv4 {
                Ipv4Method::Auto => String::from("Automatic address"),
                Ipv4Method::Manual { address, .. } => format!("Manual address {address}"),
            };
            if !adapter.enabled {
                how.push_str(", turned off");
            }
            let forget = Button::new("Forget")
                .on_press(AppMsg::Network(Msg::Forget(adapter.id.clone())))
                .enabled(self.idle());
            group = group.row(
                ActionRow::new(adapter.id.to_string())
                    .icon(Icon::Unplug)
                    .detail(how)
                    .trailing(forget),
            );
        }
        let mut parts: Vec<Node<AppMsg>> = vec![
            group.into(),
            caption_text(
                "Each adapter's settings come back into use when it is plugged in again.",
                Tone::Secondary,
            ),
        ];
        parts.extend(self.trouble_where(|r| matches!(r, Request::Forget(_))));
        Section::new("Adapters that aren't here", column(space::SM, parts)).into()
    }

    pub fn dialog(&self) -> Option<Node<AppMsg>> {
        let editing = self.editing.as_ref()?;
        let shown = |typed: &Typed, error: Option<Invalid>| {
            error.filter(|_| typed.field.left || editing.tried)
        };
        let line = |typed: &Typed,
                    error: Option<Invalid>,
                    placeholder: &str,
                    hint: &'static str,
                    on_input: fn(LineEditInput) -> AppMsg| {
            let error = shown(typed, error);
            let field = typed.field.checked(placeholder, error.is_some(), on_input);
            let caption = match error {
                Some(why) => (why.sentence(), Tone::Danger),
                None => (hint, Tone::Secondary),
            };
            with_caption(field, Some(caption))
        };
        let form = &editing.form;
        let (title, choices, manual, fields) = match form {
            Form::Address {
                name,
                manual,
                address,
                gateway,
                ..
            } => (
                format!("IP settings for {name}"),
                [
                    ("Automatic", "The network gives this adapter an address."),
                    ("Manual", "You type the address and the gateway."),
                ],
                *manual,
                PropertyList::new()
                    .row_with(
                        "Address",
                        line(
                            address,
                            form.address_error(),
                            "192.168.1.20/24",
                            "With its network's prefix length, such as /24.",
                            |i| AppMsg::Network(Msg::Address(i)),
                        ),
                    )
                    .row_with(
                        "Gateway",
                        line(
                            gateway,
                            form.gateway_error(),
                            "192.168.1.1",
                            "Without one, only this network can be reached.",
                            |i| AppMsg::Network(Msg::Gateway(i)),
                        ),
                    ),
            ),
            Form::Dns { manual, servers } => (
                String::from("DNS servers"),
                [
                    ("Automatic", "Each network names its own."),
                    ("Manual", "You type them, and every adapter asks them."),
                ],
                *manual,
                PropertyList::new().row_with(
                    "Servers",
                    line(
                        servers,
                        form.servers_error(),
                        "1.1.1.1 9.9.9.9",
                        "Up to three, separated by spaces.",
                        |i| AppMsg::Network(Msg::Servers(i)),
                    ),
                ),
            ),
        };
        let rows = choices
            .iter()
            .map(|(title, detail)| ListRow::new(*title).detail(*detail))
            .collect();
        let mut parts: Vec<Node<AppMsg>> = vec![
            List::new(rows)
                .selected(Some(usize::from(manual)))
                .on_select(|row| AppMsg::Network(Msg::Method(row)))
                .into(),
        ];
        if manual {
            parts.push(fields.into());
        }
        if let Some(trouble) = &editing.trouble {
            parts.push(
                Notice::new(Tone::Danger, Request::Save.failed())
                    .body(trouble.clone())
                    .into(),
            );
        }
        let idle = self.asking.is_none();
        let footer = ButtonRow::new()
            .cancel(
                Button::new("Cancel")
                    .on_press(AppMsg::Network(Msg::Cancel))
                    .enabled(idle),
            )
            .confirm(
                Button::new("Save")
                    .on_press(AppMsg::Network(Msg::Save))
                    .enabled(idle && self.problem.is_none()),
            );
        Some(
            Dialog::new(title)
                .content(column(space::MD, parts))
                .footer(footer)
                .into(),
        )
    }
}

#[cfg(test)]
pub(crate) fn sample() -> (Settings, Live) {
    use slopos_settings_core::network::{LiveAddress, LiveDns};
    let cidr = |t: &str| slopos_net_core::Cidr::from_str_bytes(t.as_bytes()).unwrap();
    let kept = Settings::parse("adapter 52:54:00:12:34:57\n  ipv4 manual 10.9.0.2/24\n").unwrap();
    let live = Live {
        networking: true,
        adapters: vec![
            LiveAdapter {
                ifindex: 1,
                name: "lo".into(),
                mac: slopos_net_core::Mac::ZERO,
                loopback: true,
                up: true,
                carrier: true,
                dhcp: false,
                addresses: Vec::new(),
                gateway: None,
            },
            LiveAdapter {
                ifindex: 2,
                name: "eth0".into(),
                mac: slopos_net_core::Mac([0x52, 0x54, 0, 0x12, 0x34, 0x56]),
                loopback: false,
                up: true,
                carrier: true,
                dhcp: true,
                addresses: vec![LiveAddress {
                    address: cidr("10.0.2.15/24"),
                    from_dhcp: true,
                    link_local: false,
                }],
                gateway: Some(Ipv4([10, 0, 2, 2])),
            },
        ],
        dns: LiveDns {
            servers: vec![Ipv4([10, 0, 2, 3])],
            pinned: false,
        },
    };
    (kept, live)
}
