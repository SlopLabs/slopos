//! Network: whether this computer is online, each adapter, its address and
//! the DNS servers. A switch is one [`SWITCH`] request, run at once and
//! shown pending until the network is read again; an address or the DNS
//! servers change in a dialog saved as one [`SETTINGS`] `netctl load`.

use slopos_appkit::{
    ActionRow, Button, ButtonRow, Dialog, Group, LineEditInput, List, ListRow, Node, Notice,
    Pending, PropertyList, Section, Switch, Text, Tone, column, row,
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

/// How often the page reads the network again while it shows it, where
/// nothing says when the network changed.
pub const REFRESH_MS: u64 = 2000;

/// Shown for an address or gateway an adapter lacks, so its rows stay put.
const NONE: &str = "None";

#[derive(Clone, Debug)]
pub enum Msg {
    /// What `/etc/network` keeps and the network now, or why either could
    /// not be read.
    Read(Result<(Settings, Live), String>),
    /// The same, read once a request was made.
    Settled(Result<(Settings, Live), String>),
    Changed,
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
    /// Read them once a request was made; answer [`Msg::Settled`].
    Settle,
}

/// What a request changes. A switch's is asked with what it was turned to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Request {
    Networking,
    Adapter { name: String, mac: Mac },
    Forget(AdapterId),
    Save,
}

impl Request {
    fn failed(&self, on: bool) -> &'static str {
        match (self, on) {
            (Request::Networking, true) => "Networking couldn't be turned on",
            (Request::Networking, false) => "Networking couldn't be turned off",
            (Request::Adapter { .. }, true) => "The adapter couldn't be turned on",
            (Request::Adapter { .. }, false) => "The adapter couldn't be turned off",
            (Request::Forget(_), _) => "The settings couldn't be forgotten",
            (Request::Save, _) => "The settings couldn't be saved",
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
    asked: Pending<Request, bool>,
    trouble: Option<(Request, bool, String)>,
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

fn turned(on: bool) -> &'static str {
    match on {
        true => "on",
        false => "off",
    }
}

/// 0 off, 1 no cable, 2 still being given what it lacks, 3 its own network
/// only, 4 online.
fn reach(adapter: &LiveAdapter) -> u8 {
    match (adapter.up, adapter.carrier) {
        (false, _) => 0,
        (true, false) => 1,
        _ if adapter.leasing || addresses(adapter).is_empty() => 2,
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
            asked: Pending::new(),
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

    fn usable(&self) -> bool {
        self.kept.is_some() && self.problem.is_none()
    }

    /// What the computer does now, which `ip net` may have left unlike what
    /// is kept.
    fn networking_now(&self) -> bool {
        match &self.live {
            Some(live) => live.networking,
            None => self.kept.as_ref().is_some_and(|k| k.networking),
        }
    }

    fn networking_on(&self) -> bool {
        self.asked
            .shown(&Request::Networking, self.networking_now())
    }

    fn adapter_kept(&self, name: &str, mac: Mac) -> bool {
        self.kept.as_ref().is_none_or(|k| governing(k, name, mac).0)
    }

    fn adapter_on(&self, name: &str, mac: Mac) -> bool {
        let what = Request::Adapter {
            name: String::from(name),
            mac,
        };
        self.asked.shown(&what, self.adapter_kept(name, mac))
    }

    fn saving(&self) -> bool {
        self.asked.shown(&Request::Save, false)
    }

    fn ask(
        &mut self,
        request: Request,
        on: bool,
        action: &'static str,
        args: &[&str],
        input: Input,
    ) -> Vec<AppEffect> {
        self.trouble = None;
        self.asked.ask(request, on);
        vec![AppEffect::Ask(Ask {
            purpose: Purpose::Network,
            action,
            args: args.iter().map(|arg| String::from(*arg)).collect(),
            input,
        })]
    }

    fn turn_networking(&mut self) -> Vec<AppEffect> {
        if !self.usable() {
            return Vec::new();
        }
        let on = !self.networking_on();
        let args = ["networking", turned(on)];
        self.ask(Request::Networking, on, SWITCH, &args, Input::default())
    }

    fn turn_adapter(&mut self, name: String, mac: Mac) -> Vec<AppEffect> {
        if !self.usable() {
            return Vec::new();
        }
        let on = !self.adapter_on(&name, mac);
        let adapter = match mac.is_zero() {
            true => name.clone(),
            false => mac.to_string(),
        };
        let args = ["set", adapter.as_str(), turned(on)];
        let what = Request::Adapter { name, mac };
        self.ask(what, on, SWITCH, &args, Input::default())
    }

    fn forget(&mut self, id: AdapterId) -> Vec<AppEffect> {
        let kept = |kept: &Settings| kept.adapters.iter().any(|a| a.id == id);
        let what = Request::Forget(id.clone());
        if !self.usable() || !self.kept.as_ref().is_some_and(kept) || self.asked.shown(&what, false)
        {
            return Vec::new();
        }
        let args = ["forget", &id.to_string()];
        self.ask(what, true, SETTINGS, &args, Input::default())
    }

    fn open(&mut self, form: Form) {
        if self.usable() {
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

    /// The whole file goes in, so it waits for every change already asked.
    fn save(&mut self) -> Vec<AppEffect> {
        if !self.usable() || !self.asked.is_empty() {
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
        if wanted == *kept {
            self.editing = None;
            return Vec::new();
        }
        let input = wanted.render().into();
        self.ask(Request::Save, true, SETTINGS, &["load"], input)
    }

    pub fn update(&mut self, msg: Msg) -> Vec<AppEffect> {
        match msg {
            Msg::Read(Ok((kept, live))) => {
                self.kept = Some(kept);
                self.live = Some(live);
                self.problem = None;
            }
            Msg::Read(Err(why)) => self.problem = Some(why),
            Msg::Settled(read) => {
                if let Some((Request::Save, _)) = self.asked.settle() {
                    self.editing = None;
                }
                return self.update(Msg::Read(read));
            }
            Msg::Changed => return vec![AppEffect::Network(Effect::Read)],
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
                if !self.saving() {
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

    /// A request made stays shown as asked until the network is read again;
    /// a declined one says nothing.
    pub fn answered(&mut self, asked: Asked) -> Vec<AppEffect> {
        let declined = matches!(
            asked,
            Asked::Refused {
                by_person: true,
                ..
            }
        );
        let Some(why) = asked.trouble() else {
            self.asked.answer(true);
            return vec![AppEffect::Network(Effect::Settle)];
        };
        match self.asked.answer(false) {
            _ if declined => {}
            Some((Request::Save, _)) => {
                if let Some(editing) = &mut self.editing {
                    editing.trouble = Some(why);
                }
            }
            Some((request, on)) => self.trouble = Some((request, on, why)),
            None => {}
        }
        vec![AppEffect::Network(Effect::Read)]
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
        let (request, on, why) = self.trouble.as_ref().filter(|(r, ..)| asked(r))?;
        Some(
            Notice::new(Tone::Danger, request.failed(*on))
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
            let on = self.networking_on();
            group = group.row(
                Switch::new("Networking", on)
                    .detail("Every adapter stops while it is off.")
                    .pending(on != self.networking_now())
                    .on_toggle(AppMsg::Network(Msg::Networking))
                    .enabled(self.usable()),
            );
        }
        let mut parts: Vec<Node<AppMsg>> = vec![group.into()];
        parts.extend(self.trouble_where(|r| matches!(r, Request::Networking)));
        if !self.admin && self.kept.is_some() {
            parts.push(locked_text("Changes ask for an administrator's password."));
        }
        column(space::SM, parts)
    }

    fn edit_button(&self, msg: Msg) -> Button<AppMsg> {
        Button::new("Edit…")
            .on_press(AppMsg::Network(msg))
            .enabled(self.usable())
    }

    fn adapter_section(&self, adapter: &LiveAdapter) -> Node<AppMsg> {
        let (name, mac) = (adapter.name.clone(), adapter.mac);
        let on = self.adapter_on(&name, mac);
        let switch = Switch::new("Use this adapter", on)
            .detail(self.adapter_status(adapter))
            .pending(on != self.adapter_kept(&name, mac))
            .on_toggle(AppMsg::Network(Msg::Adapter(name.clone(), mac)))
            .enabled(self.usable());
        let held = match addresses(adapter) {
            held if held.is_empty() => String::from(NONE),
            held => held.join(", "),
        };
        let gateway = gateway(adapter).map_or_else(|| String::from(NONE), |g| g.to_string());
        let mut facts = PropertyList::new()
            .row("Address", held)
            .row("Gateway", gateway);
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
        let group = Group::new()
            .row(switch)
            .row(facts)
            .row(how.trailing(self.edit_button(Msg::EditAddress(name.clone(), mac))));
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
                .enabled(self.usable());
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
                Notice::new(Tone::Danger, Request::Save.failed(true))
                    .body(trouble.clone())
                    .into(),
            );
        }
        let footer = ButtonRow::new()
            .cancel(
                Button::new("Cancel")
                    .on_press(AppMsg::Network(Msg::Cancel))
                    .enabled(!self.saving()),
            )
            .confirm(
                Button::new("Save")
                    .on_press(AppMsg::Network(Msg::Save))
                    .enabled(self.asked.is_empty() && self.problem.is_none()),
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
                leasing: false,
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
                leasing: false,
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
