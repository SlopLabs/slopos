//! Network: whether the computer uses the network, how each adapter gets its
//! address and which DNS servers it asks. What the page shows of an adapter
//! is read from the kernel every few seconds; what it changes is kept in
//! `/etc/network` by `netctl`, which privd runs once allowed, so the change
//! is the same one `netctl` makes at a terminal and lasts across restarts.

use slopos_appkit::{
    Button, ButtonRow, LineEditInput, List, ListRow, Node, Notice, PropertyList, Section, Text,
    Tone, column,
};
use slopos_net_core::Ipv4;
use slopos_net_core::Mac;
use slopos_settings_core::network::{
    self as net, AdapterId, Dns, Invalid, Ipv4Method, Live, LiveAdapter, Settings,
};
use slopos_theme_core::{Icon, TextRole, space};

use crate::field::{Field, Left, caption_text, with_error};
use crate::{Ask, Asked, Effect as AppEffect, Msg as AppMsg, Panel, Purpose};

pub const ACTION: &str = "org.slopos.network.settings";

/// How often the page reads the network again while it shows it.
pub const REFRESH_MS: u64 = 2000;

#[derive(Clone, Debug)]
pub enum Msg {
    /// What `/etc/network` keeps and the network now, or why either could
    /// not be read.
    Read(Result<(Settings, Live), String>),
    Adapter(usize),
    Networking,
    Enabled,
    Method(usize),
    Address(LineEditInput),
    Gateway(LineEditInput),
    DnsMethod(usize),
    Dns(LineEditInput),
    Apply,
    Revert,
    Kept(usize),
    Forget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Read the settings kept and the network; answer [`Msg::Read`].
    Read,
}

/// What the page holds of the whole network as the person edits it.
#[derive(Clone, Debug, Default)]
struct Whole {
    networking: bool,
    dns_manual: bool,
    dns: Field,
}

impl Whole {
    fn of(kept: &Settings) -> Whole {
        let (dns_manual, dns) = match &kept.dns {
            Dns::Auto => (false, String::new()),
            Dns::Manual(servers) => (true, joined(servers)),
        };
        Whole {
            networking: kept.networking,
            dns_manual,
            dns: Field::with(dns),
        }
    }

    fn same_dns(&self, other: &Whole) -> bool {
        self.dns_manual == other.dns_manual && self.dns.text().trim() == other.dns.text().trim()
    }
}

/// What the page holds of the adapter chosen as the person edits it.
#[derive(Clone, Debug, Default)]
struct OneAdapter {
    enabled: bool,
    manual: bool,
    address: Field,
    gateway: Field,
}

impl OneAdapter {
    fn of(kept: &Settings, adapter: Option<&LiveAdapter>) -> OneAdapter {
        let governing = adapter.and_then(|a| kept.governing(&a.name, a.mac));
        let (enabled, ipv4) = governing.map_or((true, Ipv4Method::Auto), |a| (a.enabled, a.ipv4));
        let (manual, address, gateway) = match ipv4 {
            Ipv4Method::Auto => (false, String::new(), String::new()),
            Ipv4Method::Manual { address, gateway } => (
                true,
                address.to_string(),
                gateway.map(|g| g.to_string()).unwrap_or_default(),
            ),
        };
        OneAdapter {
            enabled,
            manual,
            address: Field::with(address),
            gateway: Field::with(gateway),
        }
    }

    fn same(&self, other: &OneAdapter) -> bool {
        self.enabled == other.enabled
            && self.manual == other.manual
            && self.address.text().trim() == other.address.text().trim()
            && self.gateway.text().trim() == other.gateway.text().trim()
    }
}

pub struct Network {
    admin: bool,
    kept: Option<Settings>,
    live: Option<Live>,
    problem: Option<String>,
    /// By name and hardware address, since a name passes to the next
    /// adapter once its holder leaves.
    chosen: Option<(String, Mac)>,
    /// The edits, and what they were made over, so a change kept from
    /// elsewhere meanwhile stays where the person changed nothing.
    whole: Whole,
    whole_was: Whole,
    adapter: OneAdapter,
    adapter_was: OneAdapter,
    absent: Option<AdapterId>,
    asking: bool,
    /// The next read takes what is kept as it is, edits and all.
    settling: bool,
    said: Option<(Tone, String)>,
}

fn kind_name(adapter: &LiveAdapter) -> String {
    format!("Wired network {}", adapter.name)
}

fn status(adapter: &LiveAdapter) -> &'static str {
    match (adapter.up, adapter.carrier) {
        (false, _) => "Off",
        (true, true) => "Connected",
        (true, false) => "No cable",
    }
}

fn addresses(adapter: &LiveAdapter) -> String {
    let all: Vec<String> = adapter
        .addresses
        .iter()
        .filter(|a| !a.link_local)
        .map(|a| a.address.to_string())
        .collect();
    match all.is_empty() {
        true => String::from("None"),
        false => all.join(", "),
    }
}

fn joined(servers: &[Ipv4]) -> String {
    servers
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl Network {
    pub fn new(admin: bool) -> Network {
        Network {
            admin,
            kept: None,
            live: None,
            problem: None,
            chosen: None,
            whole: Whole::default(),
            whole_was: Whole::default(),
            adapter: OneAdapter::default(),
            adapter_was: OneAdapter::default(),
            absent: None,
            asking: false,
            settling: false,
            said: None,
        }
    }

    fn adapters(&self) -> Vec<&LiveAdapter> {
        self.live
            .as_ref()
            .map(|live| live.adapters.iter().filter(|a| !a.loopback).collect())
            .unwrap_or_default()
    }

    fn chosen(&self) -> Option<&LiveAdapter> {
        let (name, mac) = self.chosen.as_ref()?;
        self.adapters()
            .into_iter()
            .find(|a| a.name == *name && a.mac == *mac)
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

    fn reset_whole(&mut self) {
        if let Some(kept) = &self.kept {
            let focused = self.whole.dns.focused;
            self.whole = Whole::of(kept);
            self.whole_was = self.whole.clone();
            self.whole.dns.focused = focused;
        }
    }

    fn reset_adapter(&mut self) {
        if let Some(kept) = &self.kept {
            let focused = (self.adapter.address.focused, self.adapter.gateway.focused);
            self.adapter = OneAdapter::of(kept, self.chosen());
            self.adapter_was = self.adapter.clone();
            (self.adapter.address.focused, self.adapter.gateway.focused) = focused;
        }
    }

    /// Whether the address and gateway fields show, and so count.
    fn addressed(&self) -> bool {
        self.whole.networking && self.adapter.enabled && self.adapter.manual
    }

    fn address_error(&self) -> Option<Invalid> {
        if !self.addressed() {
            return None;
        }
        net::parse_address(self.adapter.address.text()).err()
    }

    fn gateway_error(&self) -> Option<Invalid> {
        let typed = self.adapter.gateway.text().trim();
        if !self.addressed() || typed.is_empty() {
            return None;
        }
        let gateway = match net::parse_gateway(typed) {
            Ok(gateway) => gateway,
            Err(why) => return Some(why),
        };
        let address = net::parse_address(self.adapter.address.text()).ok()?;
        net::check_gateway(address, gateway).err()
    }

    fn dns_error(&self) -> Option<Invalid> {
        if !self.whole.dns_manual {
            return None;
        }
        match net::parse_dns(self.whole.dns.text()) {
            Ok(Dns::Auto) => Some(Invalid::DnsServer),
            Ok(Dns::Manual(_)) => None,
            Err(why) => Some(why),
        }
    }

    /// What is kept with the person's edits over it, `None` while a field
    /// is wrong.
    fn wanted(&self) -> Option<Settings> {
        let mut wanted = self.kept.clone()?;
        if self.address_error().is_some()
            || self.gateway_error().is_some()
            || self.dns_error().is_some()
        {
            return None;
        }
        if self.whole.networking != self.whole_was.networking {
            wanted.networking = self.whole.networking;
        }
        if !self.whole.same_dns(&self.whole_was) {
            wanted.dns = match self.whole.dns_manual {
                true => net::parse_dns(self.whole.dns.text()).ok()?,
                false => Dns::Auto,
            };
        }
        if let Some(adapter) = self.chosen()
            && !self.adapter.same(&self.adapter_was)
        {
            let (kept_enabled, kept) = wanted
                .governing(&adapter.name, adapter.mac)
                .map_or((true, Ipv4Method::Auto), |a| (a.enabled, a.ipv4));
            let ipv4 = match (self.adapter.manual, self.addressed()) {
                (false, _) => Ipv4Method::Auto,
                (true, false) => kept,
                (true, true) => {
                    let address = net::parse_address(self.adapter.address.text()).ok()?;
                    let typed = self.adapter.gateway.text().trim();
                    let gateway = match typed.is_empty() {
                        true => None,
                        false => Some(net::parse_gateway(typed).ok()?),
                    };
                    Ipv4Method::manual(address, gateway).ok()?
                }
            };
            if (self.adapter.enabled, ipv4) != (kept_enabled, kept) {
                wanted.set_for(&adapter.name, adapter.mac, self.adapter.enabled, ipv4);
            }
        }
        Some(wanted)
    }

    fn changed(&self) -> bool {
        match (&self.kept, self.wanted()) {
            (Some(kept), Some(wanted)) => *kept != wanted,
            _ => self.kept.is_some(),
        }
    }

    fn apply(&mut self, wanted: Settings) -> Vec<AppEffect> {
        self.asking = true;
        self.said = None;
        vec![AppEffect::Ask(Ask {
            purpose: Purpose::Network,
            action: ACTION,
            args: vec![String::from("load")],
            input: wanted.render().into(),
        })]
    }

    fn read(&mut self, kept: Settings, live: Live) {
        let fresh = self.kept.is_none() || std::mem::take(&mut self.settling);
        self.kept = Some(kept);
        self.live = Some(live);
        self.problem = None;
        if fresh || self.chosen().is_none() || self.adapter.same(&self.adapter_was) {
            if self.chosen().is_none() {
                self.chosen = self.adapters().first().map(|a| (a.name.clone(), a.mac));
            }
            self.reset_adapter();
        }
        let networking_edited = self.whole.networking != self.whole_was.networking;
        let dns_edited = !self.whole.same_dns(&self.whole_was);
        let mine = self.whole.clone();
        self.reset_whole();
        if !fresh && networking_edited {
            self.whole.networking = mine.networking;
        }
        if !fresh && dns_edited {
            self.whole.dns_manual = mine.dns_manual;
            self.whole.dns = mine.dns;
        }
        let gone = self
            .absent
            .as_ref()
            .is_some_and(|id| !self.absent_kept().iter().any(|a| a.id == *id));
        if gone {
            self.absent = None;
        }
    }

    pub fn update(&mut self, msg: Msg) -> Vec<AppEffect> {
        match msg {
            Msg::Read(Ok((kept, live))) => self.read(kept, live),
            Msg::Read(Err(why)) => self.problem = Some(why),
            Msg::Adapter(row) => {
                let next = self.adapters().get(row).map(|a| (a.name.clone(), a.mac));
                if next.is_some() && next != self.chosen {
                    self.chosen = next;
                    self.reset_adapter();
                }
            }
            Msg::Networking => self.whole.networking = !self.whole.networking,
            Msg::Enabled => self.adapter.enabled = !self.adapter.enabled,
            Msg::Method(row) => self.adapter.manual = row == 1,
            Msg::DnsMethod(row) => self.whole.dns_manual = row == 1,
            Msg::Address(input) => return self.typed(|n| &mut n.adapter.address, input),
            Msg::Gateway(input) => return self.typed(|n| &mut n.adapter.gateway, input),
            Msg::Dns(input) => return self.typed(|n| &mut n.whole.dns, input),
            Msg::Apply => return self.update_apply(),
            Msg::Revert => {
                self.reset_whole();
                self.reset_adapter();
                self.said = None;
            }
            Msg::Kept(row) => self.absent = self.absent_kept().get(row).map(|a| a.id.clone()),
            Msg::Forget => {
                if self.changed() || self.asking || self.problem.is_some() {
                    return Vec::new();
                }
                let (Some(mut wanted), Some(id)) = (self.kept.clone(), self.absent.take()) else {
                    return Vec::new();
                };
                wanted.adapters.retain(|a| a.id != id);
                return self.apply(wanted);
            }
        }
        Vec::new()
    }

    fn typed(
        &mut self,
        field: fn(&mut Network) -> &mut Field,
        input: LineEditInput,
    ) -> Vec<AppEffect> {
        match field(self).edit(input) {
            Left::Enter => self.update_apply(),
            Left::Nothing => Vec::new(),
        }
    }

    /// Whether Apply may ask now: something changed, every field holds, and
    /// what it goes over was read.
    fn can_apply(&self) -> bool {
        self.changed() && self.wanted().is_some() && !self.asking && self.problem.is_none()
    }

    /// Whether `read` is what the page shows already, so reading it again
    /// changes nothing.
    pub fn holds(&self, (kept, live): &(Settings, Live)) -> bool {
        self.problem.is_none()
            && !self.settling
            && self.kept.as_ref() == Some(kept)
            && self.live.as_ref() == Some(live)
    }

    pub fn set_admin(&mut self, admin: bool) {
        self.admin = admin;
    }

    fn update_apply(&mut self) -> Vec<AppEffect> {
        match self.wanted().filter(|_| self.can_apply()) {
            Some(wanted) => self.apply(wanted),
            None => Vec::new(),
        }
    }

    pub fn answered(&mut self, asked: Asked) -> Vec<AppEffect> {
        self.asking = false;
        self.settling = matches!(asked, Asked::Ran { .. });
        self.said = Some(match asked.trouble() {
            None => (
                Tone::Success,
                String::from("The network settings are kept and in use."),
            ),
            Some(why) => (Tone::Danger, why),
        });
        vec![AppEffect::Network(Effect::Read)]
    }

    pub fn dialog(&self) -> Option<Node<AppMsg>> {
        None
    }

    pub fn view(&self) -> Node<AppMsg> {
        let about = "How this computer reaches other computers and the internet.";
        let footer = ButtonRow::new()
            .aside(
                Button::new("Revert")
                    .on_press(AppMsg::Network(Msg::Revert))
                    .enabled(self.changed() && !self.asking),
            )
            .confirm(
                Button::new("Apply")
                    .on_press(AppMsg::Network(Msg::Apply))
                    .enabled(self.can_apply()),
            );
        if self.kept.is_none() {
            let words = self
                .problem
                .clone()
                .unwrap_or_else(|| String::from("Reading the network settings."));
            let notice = match self.problem {
                Some(_) => {
                    Notice::new(Tone::Danger, "The network settings can't be read").body(words)
                }
                None => Notice::new(Tone::Info, words),
            };
            return crate::page(Panel::Network, about, vec![notice.into()], None);
        }
        let mut parts: Vec<Node<AppMsg>> = Vec::new();
        if let Some(problem) = &self.problem {
            let notice = Notice::new(Tone::Danger, "The network settings can't be read again");
            parts.push(notice.body(problem.clone()).into());
        }
        if let Some((tone, said)) = &self.said {
            parts.push(Notice::new(*tone, said.clone()).into());
        }
        if !self.admin {
            parts.push(
                Notice::new(
                    Tone::Info,
                    "Changing these takes an administrator's password",
                )
                .into(),
            );
        }
        parts.push(
            Section::new(
                "Networking",
                Node::Checkbox {
                    checked: self.whole.networking,
                    label: String::from("Use the network"),
                    on_toggle: Some(AppMsg::Network(Msg::Networking)),
                    enabled: true,
                },
            )
            .into(),
        );
        parts.push(self.adapters_section());
        if self.whole.networking
            && let Some(adapter) = self.chosen()
        {
            parts.push(self.adapter_section(adapter));
        }
        parts.push(self.dns_section());
        let absent = self.absent_kept();
        if !absent.is_empty() {
            parts.push(self.absent_section(&absent));
        }
        crate::page(Panel::Network, about, parts, Some(footer))
    }

    fn adapters_section(&self) -> Node<AppMsg> {
        let adapters = self.adapters();
        if adapters.is_empty() {
            return Section::new(
                "Adapters",
                Text::new("This computer has no network adapter.")
                    .tone(Tone::Secondary)
                    .wrap(),
            )
            .into();
        }
        let rows = adapters
            .iter()
            .map(|a| {
                let detail = match (a.up, a.carrier) {
                    (true, true) => format!("{} · {}", status(a), addresses(a)),
                    _ => String::from(status(a)),
                };
                ListRow::new(kind_name(a))
                    .icon(Icon::EthernetPort)
                    .detail(detail)
            })
            .collect();
        let chosen = adapters
            .iter()
            .position(|a| self.chosen.as_ref() == Some(&(a.name.clone(), a.mac)));
        Section::new(
            "Adapters",
            List::new(rows)
                .selected(chosen)
                .on_select(|row| AppMsg::Network(Msg::Adapter(row))),
        )
        .into()
    }

    fn adapter_section(&self, adapter: &LiveAdapter) -> Node<AppMsg> {
        let mut facts = PropertyList::new()
            .row("Status", status(adapter))
            .row("Hardware address", adapter.mac.to_string())
            .row("Address", addresses(adapter));
        if let Some(gateway) = adapter.gateway.filter(|g| !g.is_unspecified()) {
            facts = facts.row("Gateway", gateway.to_string());
        }
        let mut parts: Vec<Node<AppMsg>> = vec![
            facts.into(),
            Node::Checkbox {
                checked: self.adapter.enabled,
                label: String::from("Use this adapter"),
                on_toggle: Some(AppMsg::Network(Msg::Enabled)),
                enabled: true,
            },
        ];
        if self.adapter.enabled {
            let methods = vec![
                ListRow::new("Automatic address").detail("The network gives one, by DHCP."),
                ListRow::new("Manual address").detail("You choose the address and the gateway."),
            ];
            parts.push(
                List::new(methods)
                    .selected(Some(usize::from(self.adapter.manual)))
                    .on_select(|row| AppMsg::Network(Msg::Method(row)))
                    .into(),
            );
            if self.adapter.manual {
                let (address_error, gateway_error) = (self.address_error(), self.gateway_error());
                let address =
                    self.adapter
                        .address
                        .checked("192.168.1.20/24", address_error.is_some(), |i| {
                            AppMsg::Network(Msg::Address(i))
                        });
                let gateway = self.adapter.gateway.checked(
                    "192.168.1.1, or nothing",
                    gateway_error.is_some(),
                    |i| AppMsg::Network(Msg::Gateway(i)),
                );
                parts.push(
                    PropertyList::new()
                        .row_with(
                            "Address",
                            with_error(address, address_error.map(Invalid::sentence)),
                        )
                        .row_with(
                            "Gateway",
                            with_error(gateway, gateway_error.map(Invalid::sentence)),
                        )
                        .into(),
                );
            }
        }
        Section::new(kind_name(adapter), column(space::MD, parts)).into()
    }

    fn dns_section(&self) -> Node<AppMsg> {
        let in_use = self
            .live
            .as_ref()
            .map(|live| match live.dns.servers.is_empty() {
                true => String::from("None in use now."),
                false => format!("In use now: {}.", joined(&live.dns.servers)),
            })
            .unwrap_or_default();
        let methods = vec![
            ListRow::new("Automatic").detail("The network names them, with each address."),
            ListRow::new("These servers").detail("The ones you type, for every adapter."),
        ];
        let mut parts: Vec<Node<AppMsg>> = vec![
            List::new(methods)
                .selected(Some(usize::from(self.whole.dns_manual)))
                .on_select(|row| AppMsg::Network(Msg::DnsMethod(row)))
                .into(),
        ];
        if self.whole.dns_manual {
            let error = self.dns_error();
            let field = self
                .whole
                .dns
                .checked("1.1.1.1, 9.9.9.9", error.is_some(), |i| {
                    AppMsg::Network(Msg::Dns(i))
                });
            parts.push(with_error(field, error.map(Invalid::sentence)));
        }
        parts.push(
            Text::new(in_use)
                .role(TextRole::Caption)
                .tone(Tone::Secondary)
                .wrap()
                .into(),
        );
        Section::new("DNS servers", column(space::SM, parts)).into()
    }

    fn absent_section(&self, absent: &[&net::Adapter]) -> Node<AppMsg> {
        let rows = absent
            .iter()
            .map(|a| {
                let how = match a.ipv4 {
                    Ipv4Method::Auto => String::from("Automatic"),
                    Ipv4Method::Manual { address, .. } => format!("Manual, {address}"),
                };
                ListRow::new(a.id.to_string())
                    .icon(Icon::Unplug)
                    .detail(how)
            })
            .collect();
        Section::new(
            "Settings for adapters not here",
            column(
                space::SM,
                vec![
                    List::new(rows)
                        .selected(
                            absent
                                .iter()
                                .position(|a| Some(&a.id) == self.absent.as_ref()),
                        )
                        .on_select(|row| AppMsg::Network(Msg::Kept(row)))
                        .into(),
                    slopos_appkit::row(
                        0,
                        vec![
                            Button::new("Forget these settings")
                                .on_press(AppMsg::Network(Msg::Forget))
                                .enabled(
                                    self.absent.is_some()
                                        && !self.asking
                                        && !self.changed()
                                        && self.problem.is_none(),
                                )
                                .into(),
                        ],
                    ),
                ]
                .into_iter()
                .chain(self.changed().then(|| {
                    caption_text("Apply or revert the changes above first.", Tone::Secondary)
                }))
                .collect(),
            ),
        )
        .into()
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
