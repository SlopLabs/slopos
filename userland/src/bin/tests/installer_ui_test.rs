//! The installer's window on the machine it was written for.
//!
//! The app is driven through the messages its widgets emit and the frames
//! the engine writes, with neither a compositor nor privd in the path: a link
//! that keeps what the window asks for stands in for privd, and the engine's
//! replies reach the window through its own reader, over a pipe.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slopos_userland as _;

use slopos_abi::syscall::O_CLOEXEC;
use slopos_appkit::{
    Action, App, AppSender, Icon, Key, LineEditInput, MessageSink, Modifiers, NamedKey, Node,
    PointerButton, Role, SliderInput, StyleSheet, Widget, WidgetEvent,
};
use slopos_boot_core::install::{Mode as PlanMode, PlanError};
use slopos_boot_core::layout::ESP_TYPE;
use slopos_installer_core::fixtures::{
    Case, GIB, LINUX, MIB, NODE, foreign, gapped, installed, machine, opened, table,
};
use slopos_installer_core::protocol::{Reply, Request, VERSION};
use slopos_installer_core::wizard::{Kind, Page, StepState, say};
use slopos_installer_core::{
    Choices, DiskState, Event, Failure, Issue, Layout, Mode, Opened, Proposal,
    Refusal as NotOffered, Reinstall, Step, Summary, System, capitalised, disk_size,
};
use slopos_priv_core::consent::Refusal;
use slopos_slibc::test_harness::note;
use slopos_userland::apps::installer_ui::{EngineSays, InstallerApp, Link, Msg, relay};
use slopos_userland::syscall::{OwnedFd, fs};
use slopos_windowing::UiQueue;

#[derive(Clone, Debug, PartialEq)]
enum Asked {
    Start,
    Frame(Request),
    Restart,
}

/// privd as far as the window can tell: it keeps what it was asked, each
/// frame read back as the engine would read it.
struct Recorder(Arc<Mutex<Vec<Asked>>>);

impl Link for Recorder {
    fn start(&mut self, _: &AppSender<Msg>) {
        self.0.lock().unwrap().push(Asked::Start);
    }

    fn send(&mut self, request: &Request) {
        let frame = request.frame();
        let payload = &frame[slopos_remote_core::HEADER_LEN..];
        let read = Request::decode(frame[0], payload).expect("a frame the engine reads");
        self.0.lock().unwrap().push(Asked::Frame(read));
    }

    fn restart(&mut self, _: &AppSender<Msg>) {
        self.0.lock().unwrap().push(Asked::Restart);
    }
}

/// The window, what it asked for, the queue its threads send through, and
/// the engine's end of the pipe its reader reads.
struct Window {
    app: InstallerApp,
    asked: Arc<Mutex<Vec<Asked>>>,
    queue: Arc<UiQueue>,
    sender: AppSender<Msg>,
    engine: Option<OwnedFd>,
}

const WAIT: Duration = Duration::from_secs(10);
const GB: u64 = 1_000_000_000;

fn fail(why: impl AsRef<str>) -> bool {
    note(why.as_ref());
    false
}

impl Window {
    /// As init opens it on a live boot.
    fn new() -> Window {
        Window::opened(true)
    }

    fn from_the_dock() -> Window {
        Window::opened(false)
    }

    fn opened(welcome: bool) -> Window {
        let queue = UiQueue::new().expect("a wake pipe");
        let sender = AppSender::new(queue.sender());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let mut app = InstallerApp::new(Box::new(Recorder(asked.clone())), welcome);
        app.on_start(sender.clone());
        Window {
            app,
            asked,
            queue,
            sender,
            engine: None,
        }
    }

    fn page(&self) -> Page {
        self.app.wizard().page()
    }

    /// What the window asked for since this was last called.
    fn asked(&self) -> Vec<Asked> {
        core::mem::take(&mut *self.asked.lock().unwrap())
    }

    fn frames(&self) -> Vec<Request> {
        self.asked()
            .into_iter()
            .filter_map(|asked| match asked {
                Asked::Frame(request) => Some(request),
                _ => None,
            })
            .collect()
    }

    /// privd started the engine: its reader reads a fresh pipe.
    fn started(&mut self) {
        let (from, to) = fs::pipe2(O_CLOEXEC as u32).expect("a pipe");
        let sender = self.sender.clone();
        std::thread::spawn(move || relay(from, &sender));
        self.engine = Some(to);
        self.app.update(Msg::Engine(EngineSays::Started));
    }

    /// Hand the window what its threads sent until `count` of them came
    /// from the engine's reader.
    fn pump(&mut self, count: usize) -> bool {
        let deadline = Instant::now() + WAIT;
        let mut seen = 0;
        while seen < count {
            if Instant::now() > deadline {
                return fail(format!("{seen} of {count} replies reached the window"));
            }
            for message in self.queue.take_messages() {
                if let Ok(msg) = message.downcast::<Msg>() {
                    seen += usize::from(matches!(*msg, Msg::Engine(_)));
                    self.app.update(*msg);
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }

    /// The engine writes `replies`, which the window reads as it does.
    fn says(&mut self, replies: &[Reply]) -> bool {
        let Some(engine) = &self.engine else {
            return fail("no engine runs");
        };
        for reply in replies {
            let frame = reply.frame();
            if fs::write_slice(engine.raw(), &frame) != Ok(frame.len()) {
                return fail("the window's reader went away");
            }
        }
        self.pump(replies.len())
    }

    fn ends(&mut self) -> bool {
        self.engine = None;
        self.pump(1)
    }

    fn view(&self) -> Node<Msg> {
        self.app.view()
    }

    fn button(&self, label: &str) -> Option<(bool, Msg)> {
        let mut found = None;
        walk(&self.view(), &mut |node| {
            let buttons: Vec<(&str, bool, Option<&Msg>)> = match node {
                Node::Button(b) => vec![(b.label.as_str(), b.enabled, b.on_press.as_ref())],
                Node::ButtonRow(row) => row
                    .leading
                    .iter()
                    .chain(&row.trailing)
                    .map(|b| (b.label.as_str(), b.enabled, b.on_press.as_ref()))
                    .collect(),
                _ => Vec::new(),
            };
            for (l, enabled, msg) in buttons {
                if let (true, Some(msg)) = (l == label, msg) {
                    found = Some((enabled, msg.clone()));
                }
            }
        });
        found
    }

    fn enabled(&self, label: &str) -> bool {
        self.button(label).is_some_and(|(enabled, _)| enabled)
    }

    /// Press the button labelled `label`, as a click would: nothing happens
    /// when it is disabled or absent.
    fn press(&mut self, label: &str) -> Option<Action> {
        match self.button(label) {
            Some((true, msg)) => Some(self.app.update(msg)),
            _ => None,
        }
    }

    /// A key nothing focused took, as the window's loop hands it on: to the
    /// footer's buttons first, then the app.
    fn key(&mut self, key: NamedKey) -> Action {
        let mut tree = slopos_appkit::tree::build_widget_tree(&self.view());
        let window = self.app.window().size;
        slopos_appkit::tree::layout_tree(tree.as_mut(), window, &StyleSheet::current());
        let mut sink = MessageSink::new();
        let key = Key::Named(key);
        if slopos_appkit::tree::offer_unhandled(tree.as_mut(), key, Modifiers::default(), &mut sink)
        {
            let mut action = Action::None;
            for msg in sink.drain_typed::<Msg>() {
                action = self.app.update(msg);
            }
            return action;
        }
        self.app.on_key(key, Modifiers::default())
    }

    fn disclose(&mut self) -> Option<bool> {
        let mut toggle = None;
        walk(&self.view(), &mut |node| {
            if let Node::Disclosure(d) = node {
                toggle = d.on_toggle.clone();
            }
        });
        self.app.update(toggle?);
        let mut expanded = None;
        walk(&self.view(), &mut |node| {
            if let Node::Disclosure(d) = node {
                expanded = Some(d.expanded);
            }
        });
        expanded
    }

    fn checkbox(&self, prefix: &str) -> Option<(String, bool)> {
        let mut found = None;
        walk(&self.view(), &mut |node| {
            if let Node::Checkbox { label, checked, .. } = node
                && label.starts_with(prefix)
            {
                found = Some((label.clone(), *checked));
            }
        });
        found
    }

    /// The titles of the list `n`th on the page, each with whether it may
    /// be chosen, and the one chosen.
    fn list(&self, n: usize) -> Option<(Vec<(String, bool)>, Option<usize>)> {
        let mut lists = Vec::new();
        walk(&self.view(), &mut |node| {
            if let Node::List(list) = node {
                let rows = list
                    .sections
                    .iter()
                    .flat_map(|(_, rows)| rows)
                    .map(|r| (r.title.clone(), r.enabled()));
                lists.push((rows.collect(), list.selected));
            }
        });
        lists.into_iter().nth(n)
    }

    fn icons(&self, n: usize) -> Vec<Option<Icon>> {
        let mut lists = Vec::new();
        walk(&self.view(), &mut |node| {
            if let Node::List(list) = node {
                let rows = list.sections.iter().flat_map(|(_, rows)| rows);
                lists.push(rows.map(|r| r.icon).collect());
            }
        });
        lists.into_iter().nth(n).unwrap_or_default()
    }

    fn texts(&self) -> Vec<String> {
        let mut texts = Vec::new();
        walk(&self.view(), &mut |node| match node {
            Node::Text(text) => texts.push(text.text.clone()),
            Node::Checkbox { label, .. } => texts.push(label.clone()),
            Node::Tag(tag) => texts.push(tag.text.clone()),
            Node::PropertyList(list) => {
                texts.extend(list.rows.iter().map(|(name, _)| name.clone()))
            }
            Node::Disclosure(d) => texts.push(d.title.clone()),
            Node::List(list) => {
                for row in list.sections.iter().flat_map(|(_, rows)| rows) {
                    texts.push(row.title.clone());
                    texts.push(row.disabled_reason.clone().unwrap_or(row.detail.clone()));
                }
            }
            _ => {}
        });
        texts
    }

    fn shows(&self, text: &str) -> bool {
        self.texts().iter().any(|t| t == text)
    }

    fn slider(&self) -> Option<(u64, u64, u64)> {
        let mut found = None;
        walk(&self.view(), &mut |node| {
            if let Node::Slider {
                value, min, max, ..
            } = node
            {
                found = Some((*value, *min, *max));
            }
        });
        found
    }

    fn tab_stops(&self) -> Vec<Role> {
        fn stops(widget: &dyn Widget, roles: &mut Vec<Role>) {
            if widget.focus_policy().is_tab_focusable() {
                roles.push(widget.role());
            }
            for child in widget.children() {
                stops(child.as_ref(), roles);
            }
        }
        // Laid out in the window: a page is a tab stop only while it scrolls.
        let mut tree = slopos_appkit::tree::build_widget_tree(&self.view());
        let window = self.app.window().size;
        slopos_appkit::tree::layout_tree(tree.as_mut(), window, &StyleSheet::current());
        let mut roles = Vec::new();
        stops(tree.as_ref(), &mut roles);
        roles
    }

    /// Press the middle of the page's first control of `role`, laid out in
    /// the window, and hand the window what it emits.
    fn press_role(&mut self, role: Role) -> bool {
        fn press(widget: &mut dyn Widget, role: Role, sink: &mut MessageSink) -> bool {
            if widget.role() == role {
                let r = widget.layout_rect();
                let event = WidgetEvent::PointerDown {
                    x: r.x + r.width / 2,
                    y: r.y + r.height / 2,
                    button: PointerButton::Left,
                    modifiers: Modifiers::default(),
                };
                widget.event(&event, sink);
                return true;
            }
            widget
                .children_mut()
                .iter_mut()
                .any(|child| press(child.as_mut(), role, sink))
        }
        let mut tree = slopos_appkit::tree::build_widget_tree(&self.view());
        let window = self.app.window().size;
        slopos_appkit::tree::layout_tree(tree.as_mut(), window, &StyleSheet::current());
        let mut sink = MessageSink::new();
        if !press(tree.as_mut(), role, &mut sink) {
            return false;
        }
        for msg in sink.drain_typed::<Msg>() {
            self.app.update(msg);
        }
        true
    }

    fn grabbed(&self) -> bool {
        let mut grabbed = false;
        walk(&self.view(), &mut |node| {
            if let Node::Slider { grab, .. } = node {
                grabbed |= grab.is_some();
            }
        });
        grabbed
    }

    fn type_in(&mut self, field: fn(LineEditInput) -> Msg, text: &str) {
        self.app.update(field(LineEditInput::Focus));
        for character in text.chars() {
            self.app.update(field(LineEditInput::Text { character }));
        }
    }
}

fn walk(node: &Node<Msg>, visit: &mut dyn FnMut(&Node<Msg>)) {
    visit(node);
    match node {
        Node::VStack { children, .. }
        | Node::HStack { children, .. }
        | Node::ZStack { children } => {
            for child in children {
                walk(child, visit);
            }
        }
        Node::Padding { child, .. }
        | Node::Expand { child, .. }
        | Node::SizedBox { child, .. }
        | Node::ScrollView { child, .. }
        | Node::Background { child, .. }
        | Node::Card { child, .. }
        | Node::Popup { child, .. } => walk(child, visit),
        Node::PropertyList(list) => {
            for (_, value) in &list.rows {
                walk(value, visit);
            }
        }
        Node::Disclosure(d) if d.expanded => walk(&d.content, visit),
        _ => {}
    }
}

/// A disk whose second partition holds a root: 4 GiB of another system's.
fn roomy() -> (slopos_boot_core::gpt::Header, Vec<u8>) {
    table(16 * GIB, &[(1, ESP_TYPE, 1, 101), (2, LINUX, 101, 4096)])
}

fn machine_holding(layout: (slopos_boot_core::gpt::Header, Vec<u8>), open: Opened) -> System {
    let mut system = machine(Some(open));
    if let DiskState::Listed(listing) = &mut system.disks[1].state {
        listing.layout = Layout::of(NODE, &layout.0, &layout.1);
    }
    system
}

/// A window on the Disk page, the engine started and the machine probed.
fn at_disk() -> Option<Window> {
    let mut w = Window::new();
    w.press(say::INSTALL)?;
    if w.asked() != [Asked::Start] {
        return None;
    }
    w.started();
    let hello = [Request::Hello { version: VERSION }, Request::Probe];
    (w.frames() == hello
        && w.says(&[
            Reply::Hello { version: VERSION },
            Reply::System(machine(None)),
        ])
        && w.page() == Page::Disk)
        .then_some(w)
}

/// A window on the How page for [`NODE`], opened as `system` has it and
/// answered with `proposal`; and the choices it proposed.
fn at_how(system: System, proposal: Proposal) -> Option<(Window, Choices)> {
    let mut w = at_disk()?;
    w.press(say::NEXT)?;
    if w.frames() != [Request::Open { disk: NODE.into() }] {
        return None;
    }
    w.says(&[Reply::System(system)]).then_some(())?;
    let [Request::Propose(choices)] = &w.frames()[..] else {
        return None;
    };
    let choices = choices.clone();
    let id = 7;
    w.says(&[Reply::Proposal { id, proposal }]).then_some(())?;
    (w.page() == Page::How).then_some((w, choices))
}

fn free_proposal() -> Proposal {
    Case::on(foreign(), PlanMode::Free { region: None }).proposal()
}

fn at_free() -> Option<(Window, Choices)> {
    at_how(machine(Some(opened(Ok(()), None))), free_proposal())
}

/// Welcome asks privd once, says that the person is being asked, and opens
/// on the Disk page once the engine has probed.
fn welcome_asks_privd_once_and_probes() -> bool {
    let mut w = Window::new();
    if !(w.enabled(say::INSTALL) && w.button(say::TRY).is_some()) {
        return fail("Welcome offers to try or install");
    }
    if !w.asked().is_empty() {
        return fail("the welcome asked privd before the person chose");
    }
    w.press(say::INSTALL);
    if w.asked() != [Asked::Start] || w.enabled(say::INSTALL) {
        return fail("one start, the button waiting on it");
    }
    w.app.update(Msg::Next);
    w.key(NamedKey::Enter);
    if !w.asked().is_empty() {
        return fail("a second Install SlopOS asked privd again");
    }
    w.app.update(Msg::Engine(EngineSays::Asking));
    if !w.shows(say::ASKING) {
        return fail("the person is not told a dialog asks them");
    }
    w.started();
    if w.frames() != [Request::Hello { version: VERSION }, Request::Probe] {
        return fail("the engine was not greeted and asked to probe");
    }
    if w.shows(say::ASKING) {
        return fail("the dialog is still said to ask once the engine runs");
    }
    w.says(&[
        Reply::Hello { version: VERSION },
        Reply::System(machine(None)),
    ]) && w.page() == Page::Disk
}

/// A refused consent leaves the person at Welcome with the reason, free to
/// ask again; Try SlopOS closes the window.
fn a_refusal_leaves_the_person_at_welcome() -> bool {
    let mut w = Window::new();
    w.press(say::INSTALL);
    w.asked();
    let why = &Refusal::Refused.to_string();
    w.app.update(Msg::Engine(EngineSays::Refused(why.into())));
    if w.page() != Page::Welcome || !w.shows(why) {
        return fail("the refusal is not shown at Welcome");
    }
    w.press(say::INSTALL);
    if w.asked() != [Asked::Start] {
        return fail("Install SlopOS does not ask again");
    }
    w.press(say::TRY) == Some(Action::Exit)
}

/// Started from the dock's entry, whose press chose to install, the window
/// asks privd at once and offers no try, since the boot may keep what is
/// done; a refusal leaves the person at Welcome, free to ask again.
fn the_dock_s_entry_asks_privd_at_once() -> bool {
    let mut w = Window::from_the_dock();
    if w.asked() != [Asked::Start] || w.page() != Page::Welcome || w.enabled(say::INSTALL) {
        return fail("the window did not ask privd as it started");
    }
    let live = w.app.wizard().heading(true);
    let says_keeps_nothing = w.shows(&live.title) || w.shows(&live.subtitle);
    if w.button(say::TRY).is_some() || says_keeps_nothing {
        return fail("the dock's window offers to try a system that may keep what is done");
    }
    let why = &Refusal::Refused.to_string();
    w.app.update(Msg::Engine(EngineSays::Refused(why.into())));
    if !w.shows(why) || !w.enabled(say::INSTALL) || !w.asked().is_empty() {
        return fail("the refusal is not left to the person at Welcome");
    }
    w.press(say::CLOSE) == Some(Action::Exit)
}

/// The disks the engine offers come first, the medium last and disabled;
/// Next opens the one chosen, and Scan again probes again.
fn the_disk_page_opens_the_disk_chosen() -> bool {
    let Some(mut w) = at_disk() else {
        return fail("no Disk page");
    };
    let Some((rows, selected)) = w.list(0) else {
        return fail("no list of disks");
    };
    let expected: Vec<(String, bool)> = w
        .app
        .wizard()
        .disks()
        .into_iter()
        .map(|row| (row.title.clone(), row.reason.is_none()))
        .collect();
    let named = expected
        .first()
        .is_some_and(|(title, _)| title.starts_with("QEMU NVMe Ctrl · "));
    if rows != expected || !named || selected != Some(0) || rows[2].1 {
        return fail(format!("disks {rows:?}, chosen {selected:?}"));
    }
    if !w.shows(NotOffered::Medium.words())
        || !w.texts().iter().any(|t| t.starts_with("nvme0n1 · "))
    {
        return fail("a disk does not say what it holds, or why not");
    }
    let drives = Some(Icon::HardDrive);
    if w.icons(0) != [drives, drives, Some(Icon::Usb)] {
        return fail(format!("the disks are drawn as {:?}", w.icons(0)));
    }
    w.app.update(Msg::Disk(2));
    if w.list(0).map(|(_, chosen)| chosen) != Some(Some(0)) {
        return fail("the medium may be chosen");
    }
    w.app.update(Msg::Disk(1));
    if w.list(0).map(|(_, chosen)| chosen) != Some(Some(1)) {
        return fail("choosing vda");
    }
    w.press(say::SCAN);
    if w.frames() != [Request::Probe] || w.enabled(say::NEXT) {
        return fail("Scanning again probes, and Next waits for it");
    }
    if !w.says(&[Reply::System(machine(None))]) {
        return false;
    }
    w.press(say::NEXT);
    w.frames()
        == [Request::Open {
            disk: "/dev/vda".into(),
        }]
}

/// Every answer on How proposes again, one proposal at a time, and Review
/// waits for the answer to the choices on screen.
fn how_proposes_each_change_and_review_waits() -> bool {
    let Some((mut w, first)) = at_free() else {
        return fail("no How page");
    };
    if first.mode != (Mode::Free { region: None }) || !w.enabled(say::REVIEW) {
        return fail(format!("opened as {first:?}"));
    }
    let span = free_proposal().root;
    if w.slider() != Some((span.chosen, span.min, span.max)) {
        return fail(format!("slider {:?} for {span:?}", w.slider()));
    }
    w.app.update(Msg::Size(SliderInput::Change(4 * GIB)));
    let sized = w.frames();
    if !matches!(&sized[..], [Request::Propose(c)] if c.root_size == Some(4 * GIB)) {
        return fail(format!("a slide proposed {sized:?}"));
    }
    if w.enabled(say::REVIEW) || w.slider().map(|s| s.0) != Some(4 * GIB) {
        return fail("Review before the answer, or the knob went back");
    }
    w.type_in(Msg::SizeText, "");
    w.app.update(Msg::SizeText(LineEditInput::Key {
        key: Key::Char('u'),
        modifiers: Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    }));
    w.type_in(Msg::SizeText, "5 GB");
    if !w.frames().is_empty() {
        return fail("a second proposal went out with one in flight");
    }
    if !w.says(&[Reply::Proposal {
        id: 8,
        proposal: free_proposal(),
    }]) {
        return false;
    }
    let typed = w.frames();
    if !matches!(&typed[..], [Request::Propose(c)] if c.root_size == Some(5 * GB)) {
        return fail(format!("the box's size was proposed as {typed:?}"));
    }
    w.says(&[Reply::Proposal {
        id: 9,
        proposal: free_proposal(),
    }]);
    w.type_in(Msg::Remote, "https://git.example/x");
    let remote = w.frames();
    if !matches!(remote.last(), Some(Request::Propose(c)) if c.remote.as_deref() == Some("h")) {
        return fail(format!("the remote's first letter proposed {remote:?}"));
    }
    w.says(&[Reply::Proposal {
        id: 10,
        proposal: free_proposal(),
    }]);
    let remote = w.frames();
    if !matches!(remote.last(), Some(Request::Propose(c)) if c.remote.as_deref() == Some("https://git.example/x"))
    {
        return fail(format!("the remote typed was proposed as {remote:?}"));
    }
    w.says(&[Reply::Proposal {
        id: 11,
        proposal: free_proposal(),
    }]);
    w.enabled(say::REVIEW) && w.frames().is_empty()
}

/// A number typed into the size box without a unit is GB, as the box shows
/// sizes, and one that is no size marks the box and proposes nothing.
fn a_bare_size_is_gb() -> bool {
    let Some((mut w, _)) = at_free() else {
        return fail("no How page");
    };
    w.type_in(Msg::SizeText, "");
    w.app.update(Msg::SizeText(LineEditInput::Key {
        key: Key::Char('u'),
        modifiers: Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    }));
    w.type_in(Msg::SizeText, "9");
    let sized = w.frames();
    if !matches!(&sized[..], [Request::Propose(c)] if c.root_size == Some(9 * GB)) {
        return fail(format!("9 proposed {sized:?}"));
    }
    w.says(&[Reply::Proposal {
        id: 8,
        proposal: free_proposal(),
    }]);
    w.type_in(Msg::SizeText, "X");
    let mut invalid = None;
    walk(&w.view(), &mut |node| {
        if let Node::LineEdit {
            text, invalid: bad, ..
        } = node
            && text == "9X"
        {
            invalid = Some(*bad);
        }
    });
    (invalid == Some(true) && w.frames().is_empty())
        || fail(format!("9X shown invalid: {invalid:?}"))
}

/// The slider's end, or a size typed past it, is all the region gives; a
/// press that changes nothing proposes nothing; Enter leaves a size box that
/// reads no size where it is.
fn the_slider_s_end_is_the_whole_region() -> bool {
    let Some((mut w, _)) = at_free() else {
        return fail("no How page");
    };
    let span = free_proposal().root;
    w.app.update(Msg::Size(SliderInput::Change(span.max)));
    if !w.frames().is_empty() {
        return fail("the size shown already was proposed");
    }
    w.app.update(Msg::Size(SliderInput::Change(4 * GIB)));
    w.frames();
    w.says(&[Reply::Proposal {
        id: 8,
        proposal: free_proposal(),
    }]);
    w.app.update(Msg::Size(SliderInput::Change(span.max)));
    let whole = w.frames();
    if !matches!(&whole[..], [Request::Propose(c)] if c.root_size.is_none())
        || w.slider().map(|s| s.0) != Some(span.max)
    {
        return fail(format!(
            "the end proposed {whole:?}, knob at {:?}",
            w.slider()
        ));
    }
    w.says(&[Reply::Proposal {
        id: 9,
        proposal: free_proposal(),
    }]);
    w.app.update(Msg::Size(SliderInput::Change(4 * GIB)));
    w.frames();
    w.says(&[Reply::Proposal {
        id: 10,
        proposal: free_proposal(),
    }]);
    w.type_in(Msg::SizeText, "");
    w.app.update(Msg::SizeText(LineEditInput::Key {
        key: Key::Char('u'),
        modifiers: Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    }));
    w.type_in(Msg::SizeText, "9999");
    w.frames();
    w.says(&[Reply::Proposal {
        id: 11,
        proposal: free_proposal(),
    }]);
    let typed = w.frames();
    if !matches!(&typed[..], [Request::Propose(c)] if c.root_size.is_none()) {
        return fail(format!("a size past the end proposed {typed:?}"));
    }
    w.says(&[Reply::Proposal {
        id: 12,
        proposal: free_proposal(),
    }]);
    w.app.update(Msg::Size(SliderInput::Change(span.max - MIB)));
    w.frames();
    let smaller = Case::on(gapped(), PlanMode::Free { region: None }).proposal();
    let end = smaller.root.max;
    w.says(&[Reply::Proposal {
        id: 13,
        proposal: smaller,
    }]);
    w.app.update(Msg::Size(SliderInput::Change(end)));
    let whole = w.frames();
    if !matches!(&whole[..], [Request::Propose(c)] if c.root_size.is_none()) {
        return fail(format!("the end of a smaller region proposed {whole:?}"));
    }
    w.says(&[Reply::Proposal {
        id: 14,
        proposal: free_proposal(),
    }]);
    w.app.update(Msg::Size(SliderInput::Change(4 * GIB)));
    w.frames();
    w.says(&[Reply::Proposal {
        id: 15,
        proposal: free_proposal(),
    }]);
    w.type_in(Msg::SizeText, "");
    w.app.update(Msg::SizeText(LineEditInput::Key {
        key: Key::Char('u'),
        modifiers: Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    }));
    w.frames();
    w.type_in(Msg::SizeText, &disk_size(span.max));
    w.frames();
    w.says(&[Reply::Proposal {
        id: 16,
        proposal: free_proposal(),
    }]);
    let shown = w.frames();
    if !matches!(&shown[..], [Request::Propose(c)] if c.root_size.is_none()) {
        return fail(format!(
            "the end typed as the box shows it proposed {shown:?}"
        ));
    }
    w.says(&[Reply::Proposal {
        id: 17,
        proposal: free_proposal(),
    }]);
    w.type_in(Msg::SizeText, "x");
    w.app.update(Msg::SizeText(LineEditInput::Key {
        key: Key::Named(NamedKey::Enter),
        modifiers: Modifiers::default(),
    }));
    (w.page() == Page::How && w.frames().is_empty())
        || fail("Enter went on from a box that reads no size")
}

/// A slider drag that a page change cuts short does not outlive it: the
/// slider is built without its grab when How comes back.
fn a_drag_ends_with_its_page() -> bool {
    let Some((mut w, _)) = at_free() else {
        return fail("no How page");
    };
    if !w.press_role(Role::Slider) || !w.grabbed() {
        return fail("a press on the slider takes no grab");
    }
    w.frames();
    w.says(&[Reply::Proposal {
        id: 8,
        proposal: free_proposal(),
    }]);
    w.key(NamedKey::Escape);
    w.press(say::NEXT);
    w.frames();
    w.says(&[Reply::System(machine(Some(opened(Ok(()), None))))]);
    w.frames();
    w.says(&[Reply::Proposal {
        id: 9,
        proposal: free_proposal(),
    }]);
    (w.page() == Page::How && w.slider().is_some() && !w.grabbed())
        || fail("the grab outlived the page")
}

fn how_reaches_every_control_by_tab() -> bool {
    let Some((w, _)) = at_free() else {
        return fail("no How page");
    };
    let roles = w.tab_stops();
    let expected = [
        Role::ScrollArea,
        Role::List,
        Role::List,
        Role::Slider,
        Role::TextField,
        Role::TextField,
        Role::Checkbox,
        Role::Button,
        Role::Button,
    ];
    roles == expected || fail(format!("tab stops {roles:?}"))
}

/// Back keeps every answer, and opening the disk again proposes them.
fn back_keeps_every_answer() -> bool {
    let Some((mut w, _)) = at_free() else {
        return fail("no How page");
    };
    w.app.update(Msg::First);
    w.frames();
    w.says(&[Reply::Proposal {
        id: 8,
        proposal: free_proposal(),
    }]);
    w.app.update(Msg::Size(SliderInput::Change(4 * GIB)));
    w.frames();
    w.says(&[Reply::Proposal {
        id: 9,
        proposal: free_proposal(),
    }]);
    w.press(say::REVIEW);
    if w.page() != Page::Review {
        return fail("Review");
    }
    w.key(NamedKey::Escape);
    if w.page() != Page::How {
        return fail("Escape is Back");
    }
    w.press(say::BACK);
    if w.page() != Page::Disk {
        return fail("Back from How");
    }
    w.press(say::NEXT);
    w.frames();
    w.says(&[Reply::System(machine(Some(opened(Ok(()), None))))]);
    let again = w.frames();
    let kept = matches!(&again[..], [Request::Propose(c)]
        if c.first && c.root_size == Some(4 * GIB) && c.mode == Mode::Free { region: None });
    kept || fail(format!("opened again, proposed {again:?}"))
}

/// The Review page's box and button say what the install does to the disk,
/// the button waits for the box, and Enter is not the button there.
fn the_review_is_worded_by_the_damage() -> bool {
    let reinstall = |keep| {
        let mut case = Case::on(
            installed(),
            PlanMode::Reuse {
                root: 3,
                region: None,
            },
        );
        case.keep_root = keep;
        let open = opened(
            Ok(()),
            Some(Reinstall {
                root: 3,
                keep,
                fit: Ok(()),
            }),
        );
        (machine(Some(open)), case.proposal(), None)
    };
    let reuse = Case::on(
        roomy(),
        PlanMode::Reuse {
            root: 2,
            region: None,
        },
    );
    let cases = [
        (
            (
                machine(Some(opened(Ok(()), None))),
                Case::erasing(Some(foreign())).proposal(),
                Some(0),
            ),
            Kind::Erase.title(),
        ),
        (
            (machine(Some(opened(Ok(()), None))), free_proposal(), None),
            "Install beside Other OS",
        ),
        (
            (
                machine_holding(roomy(), opened(Ok(()), None)),
                reuse.proposal(),
                Some(2),
            ),
            "Erase partition and install",
        ),
        (reinstall(true), Kind::Reinstall.title()),
        (reinstall(false), "Erase and reinstall SlopOS"),
    ];
    let mut confirmations = Vec::new();
    for ((system, proposal, tile), button) in cases {
        let Some((mut w, _)) = at_how(system, proposal.clone()) else {
            return fail(format!("no How page for {button}"));
        };
        if let Some(tile) = tile {
            w.app.update(Msg::Tile(tile));
            w.frames();
            w.says(&[Reply::Proposal {
                id: 8,
                proposal: proposal.clone(),
            }]);
        }
        w.press(say::REVIEW);
        if w.page() != Page::Review {
            return fail(format!("{button}: no Review page"));
        }
        match w.checkbox("I understand") {
            Some((said, false)) => confirmations.push(said),
            other => return fail(format!("{button}: box {other:?}")),
        }
        if w.button(button).map(|(enabled, _)| enabled) != Some(false) {
            return fail(format!("{button}: not there, or acts before the box"));
        }
        w.app.update(Msg::Next);
        if w.page() != Page::Review || !w.asked().is_empty() {
            return fail(format!("{button}: installed before the box"));
        }
        w.app.update(Msg::Confirm);
        w.key(NamedKey::Enter);
        if w.page() != Page::Review || !w.asked().is_empty() || !w.enabled(button) {
            return fail(format!(
                "{button}: Enter installed, or the box held it back"
            ));
        }
        w.press(button);
        let id = if tile.is_some() { 8 } else { 7 };
        let sent = w.frames();
        if !matches!(&sent[..], [Request::Install { proposal, .. }] if *proposal == id)
            || w.page() != Page::Install
        {
            return fail(format!("{button}: installed with {sent:?}"));
        }
    }
    confirmations.sort();
    confirmations.dedup();
    confirmations.len() >= 4 || fail(format!("the box says too few things: {confirmations:?}"))
}

fn at_install() -> Option<Window> {
    let (mut w, _) = at_free()?;
    w.press(say::REVIEW);
    w.app.update(Msg::Confirm);
    w.press("Install beside Other OS");
    w.frames();
    (w.page() == Page::Install).then_some(w)
}

/// An install shows each step, its copy and, under its details, its log;
/// nothing closes the window meanwhile, and Done offers to restart.
fn an_install_shows_its_steps_and_will_not_close() -> bool {
    let Some(mut w) = at_install() else {
        return fail("no Install page");
    };
    if w.button(say::BACK).is_some() || w.app.on_close_request().0 {
        return fail("a way out of an install");
    }
    w.key(NamedKey::Escape);
    w.says(&[
        Reply::Event(Event::Step(Step::Partition)),
        Reply::Event(Event::Step(Step::CopySystem)),
        Reply::Event(Event::Log {
            line: "copying the toolchain".into(),
        }),
        Reply::Event(Event::Progress {
            done: GIB,
            total: 4 * GIB,
        }),
    ]);
    let running = w.app.wizard().steps().get(3).map(|(_, state)| *state);
    if w.page() != Page::Install
        || !w.shows(&capitalised(Step::CopySystem.doing()))
        || running != Some(StepState::Running)
    {
        return fail(format!("the install page shows {:?}", w.texts()));
    }
    let mut copied = None;
    walk(&w.view(), &mut |node| {
        if let Node::ProgressBar {
            value: Some(v),
            label,
            ..
        } = node
        {
            copied = Some((*v, label.clone()));
        }
    });
    let label = format!("{} of {} copied", disk_size(GIB), disk_size(4 * GIB));
    if copied != Some((25, label)) {
        return fail(format!("the copy shows {copied:?}"));
    }
    let logged = |w: &Window| {
        w.texts()
            .iter()
            .filter(|t| *t == "copying the toolchain")
            .count()
    };
    if logged(&w) != 0 {
        return fail("the log shows with the details closed");
    }
    if w.disclose() != Some(true) || !w.shows(say::HIDE_DETAILS) {
        return fail("the details do not open");
    }
    if logged(&w) != 1 {
        return fail("the log is not under the details");
    }
    w.says(&[Reply::Done(Summary {
        disk: NODE.into(),
        entry: 3,
    })]);
    if w.page() != Page::Done || !w.shows(&w.app.wizard().heading(true).title) {
        return fail("Done");
    }
    w.key(NamedKey::Enter);
    if !w.asked().is_empty() {
        return fail("Enter restarts the computer before the USB stick may be out");
    }
    w.press(say::RESTART);
    if w.asked() != [Asked::Restart] {
        return fail("Restart does not ask privd");
    }
    w.app.update(Msg::NotRestarted { failed: false });
    if w.shows(say::NOT_RESTARTED) {
        return fail("a restart the person refused is said not to have happened");
    }
    w.press(say::RESTART);
    w.app.update(Msg::NotRestarted { failed: true });
    if !w.shows(say::NOT_RESTARTED) {
        return fail("a restart that failed is not shown");
    }
    w.press(say::KEEP_TRYING) == Some(Action::Exit)
}

/// A failed install says where it stopped, and Try again opens the disk
/// again and lands on Review with the same choices.
fn a_failed_install_goes_back_to_review() -> bool {
    let Some(mut w) = at_install() else {
        return fail("no Install page");
    };
    w.says(&[
        Reply::Event(Event::Step(Step::Format)),
        Reply::Failed(Failure::Stopped {
            step: Some(Step::Format),
            why: "mke2fs exited 1".into(),
        }),
    ]);
    let said = w.app.wizard().heading(false).subtitle;
    if w.page() != Page::Error || !said.contains(Step::Format.doing()) || !w.shows(&said) {
        return fail(format!("the error page shows {:?}", w.texts()));
    }
    if w.shows("mke2fs exited 1") || w.disclose() != Some(true) || !w.shows("mke2fs exited 1") {
        return fail("the failure is not under the details");
    }
    if !w.enabled(say::RESTART) || !w.app.on_close_request().0 {
        return fail("Restart, or closing, is refused after a failure");
    }
    w.press(say::RESTART);
    w.key(NamedKey::Escape);
    if w.asked() != [Asked::Restart] || w.page() != Page::Error || w.enabled(say::TRY_AGAIN) {
        return fail("the page can be left while a restart is asked for");
    }
    w.app.update(Msg::NotRestarted { failed: true });
    w.press(say::TRY_AGAIN);
    if w.frames() != [Request::Probe] {
        return fail("Try again probes the engine still running");
    }
    w.says(&[Reply::System(machine(None))]);
    if w.frames() != [Request::Open { disk: NODE.into() }] {
        return fail("the disk is not opened again");
    }
    w.says(&[Reply::System(machine(Some(opened(Ok(()), None))))]);
    let again = w.frames();
    if !matches!(&again[..], [Request::Propose(c)] if c.mode == Mode::Free { region: None }) {
        return fail(format!("proposed again {again:?}"));
    }
    w.says(&[Reply::Proposal {
        id: 12,
        proposal: free_proposal(),
    }]);
    w.page() == Page::Review
        && w.checkbox("I understand")
            .is_some_and(|(_, ticked)| !ticked)
}

fn a_refused_install_says_why_in_the_window_s_words() -> bool {
    let Some(mut w) = at_install() else {
        return fail("no Install page");
    };
    let issue = Issue::Plan {
        error: PlanError::NoSlot,
        node: NODE.into(),
        root: None,
    };
    w.says(&[Reply::Failed(Failure::Refused(issue.clone()))]);
    w.disclose();
    let texts = w.texts();
    if w.page() != Page::Error || !w.shows(&issue.words()) || texts.iter().any(|t| t.contains("--"))
    {
        return fail(format!("a refused install shows {texts:?}"));
    }
    true
}

/// An engine whose output ends mid-install is a failure, and one that ends
/// before it leaves the person at Welcome to start it again.
fn an_engine_that_ends_is_said() -> bool {
    let Some(mut w) = at_install() else {
        return fail("no Install page");
    };
    w.says(&[Reply::Event(Event::Step(Step::Menu))]);
    if !w.ends() || w.page() != Page::Error {
        return fail("an install whose engine ended");
    }
    let Some((mut w, _)) = at_free() else {
        return fail("no How page");
    };
    if !w.ends() || w.page() != Page::Welcome || !w.enabled(say::INSTALL) {
        return fail("an engine that ended before the install");
    }
    w.press(say::INSTALL);
    w.asked() == [Asked::Start]
}

fn every_page_passes_the_layout_audit() -> bool {
    let mut findings = Vec::new();
    let mut audit = |w: &Window, page: Page| {
        if w.page() != page {
            findings.push(format!("{page:?} is not in front but {:?}", w.page()));
            return;
        }
        let spec = w.app.window();
        let widths = [spec.min_size.width, spec.size.width];
        for height in [spec.min_size.height, spec.size.height] {
            let view = w.view();
            for finding in slopos_appkit::audit::audit_node(&view, &widths, height) {
                findings.push(format!("{page:?} at {height} px: {finding}"));
            }
        }
    };
    audit(&Window::new(), Page::Welcome);
    let Some(w) = at_disk() else {
        return fail("no Disk page");
    };
    audit(&w, Page::Disk);
    let Some((mut w, _)) = at_free() else {
        return fail("no How page");
    };
    audit(&w, Page::How);
    w.press(say::REVIEW);
    audit(&w, Page::Review);
    let Some(mut w) = at_install() else {
        return fail("no Install page");
    };
    w.says(&[
        Reply::Event(Event::Step(Step::CopySystem)),
        Reply::Event(Event::Log {
            line: "copying the toolchain".into(),
        }),
    ]);
    audit(&w, Page::Install);
    w.disclose();
    audit(&w, Page::Install);
    w.says(&[Reply::Failed(Failure::Stopped {
        step: Some(Step::CopySystem),
        why: "mke2fs exited 1".into(),
    })]);
    audit(&w, Page::Error);
    let Some(mut w) = at_install() else {
        return fail("no Install page");
    };
    w.says(&[Reply::Done(Summary {
        disk: NODE.into(),
        entry: 3,
    })]);
    audit(&w, Page::Done);
    findings.is_empty() || fail(findings.join("\n"))
}

fn main() {
    let cases: &[(&'static str, fn() -> bool)] = &[
        (
            "welcome_asks_privd_once_and_probes",
            welcome_asks_privd_once_and_probes,
        ),
        (
            "a_refusal_leaves_the_person_at_welcome",
            a_refusal_leaves_the_person_at_welcome,
        ),
        (
            "the_dock_s_entry_asks_privd_at_once",
            the_dock_s_entry_asks_privd_at_once,
        ),
        (
            "the_disk_page_opens_the_disk_chosen",
            the_disk_page_opens_the_disk_chosen,
        ),
        (
            "how_proposes_each_change_and_review_waits",
            how_proposes_each_change_and_review_waits,
        ),
        ("a_bare_size_is_gb", a_bare_size_is_gb),
        ("a_drag_ends_with_its_page", a_drag_ends_with_its_page),
        (
            "the_slider_s_end_is_the_whole_region",
            the_slider_s_end_is_the_whole_region,
        ),
        (
            "how_reaches_every_control_by_tab",
            how_reaches_every_control_by_tab,
        ),
        ("back_keeps_every_answer", back_keeps_every_answer),
        (
            "the_review_is_worded_by_the_damage",
            the_review_is_worded_by_the_damage,
        ),
        (
            "an_install_shows_its_steps_and_will_not_close",
            an_install_shows_its_steps_and_will_not_close,
        ),
        (
            "a_failed_install_goes_back_to_review",
            a_failed_install_goes_back_to_review,
        ),
        (
            "a_refused_install_says_why_in_the_window_s_words",
            a_refused_install_says_why_in_the_window_s_words,
        ),
        ("an_engine_that_ends_is_said", an_engine_that_ends_is_said),
        (
            "every_page_passes_the_layout_audit",
            every_page_passes_the_layout_audit,
        ),
    ];
    slopos_slibc::test_harness::run(cases);
}
