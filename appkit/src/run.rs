use slopos_abi::Canvas;
use slopos_gfx::RenderSurface;
use slopos_protocol::types::Event as ProtocolEvent;

use slopos_windowing::Event;
use slopos_windowing::connection;
use slopos_windowing::{EVENT_BUF_LEN, Window};

use super::event::{self, EventResponse, HitTestResult, MessageSink, PointerButton, WidgetEvent};
use super::focus::FocusManager;
use super::input::translate_event;
use super::node::{Action, App};
use super::overlay::OverlayManager;
use super::paint::PaintContext;
use super::sender::AppSender;
use super::style::StyleSheet;
use super::traits::{FocusPolicy, Widget};
use super::tree;

/// Run a widget-framework-driven application in a window as
/// [`App::window`] describes it.
pub fn run_app<A: App>(mut app: A) -> ! {
    let spec = app.window();
    let (width, height) = (
        spec.size.width.max(1) as u32,
        spec.size.height.max(1) as u32,
    );
    let handle = connection::connect().expect("compositor not running");
    let mut win = Window::new(handle.clone(), width, height).expect("failed to create window");
    let mut title = app.title();
    win.set_title(&title);
    let (min, max) = spec.limits();
    let pair = |s: super::constraints::Size| (s.width.max(0) as u32, s.height.max(0) as u32);
    win.set_size_limits(pair(min), max.map_or((0, 0), pair));
    let id = app.app_id();
    if !id.is_empty() {
        win.set_app_id(id);
    }
    super::clipboard::install(handle.clone());
    let style = StyleSheet::with_theme(slopos_theme_core::current());
    let mut focus = FocusManager::new();
    let mut overlays = OverlayManager::new();

    let mut needs_rebuild = false;
    let mut needs_repaint = true;
    let mut gesture = Gesture::default();
    let action = app.on_start(AppSender::new(handle.ui_sender()));
    process_action(action, &mut needs_rebuild, &mut needs_repaint);

    let node = app.view();
    let mut root = tree::build_widget_tree(&node);
    let mut page_key = app.page_key();

    let mut window_size = super::constraints::Size::new(width as i32, height as i32);
    tree::layout_tree(root.as_mut(), window_size, &style);
    let _ = focus.rebuild_tab_chain(root.as_ref());

    let mut proto_events: [ProtocolEvent; EVENT_BUF_LEN] =
        core::array::from_fn(|_| ProtocolEvent::FrameDone {
            surface: slopos_protocol::types::SurfaceId::NONE,
            timestamp_ms: 0,
        });
    let mut last_tick_ms: u64 = slopos_windowing::get_time_ms();
    // The compositor reports modifiers only with key events; a pointer press
    // takes the most recent snapshot, which is what shift-click is.
    let mut modifiers = super::event::Modifiers::default();
    let mut tip = Tip::default();

    loop {
        let mut key_taken = false;
        handle.flush_pending_destroys();
        handle.drain_ui_queue();

        let count = win.poll_protocol_events(&mut proto_events);
        // A poll returns a batch, so this is a vector and not an `Option`.
        let mut unhandled_keys: Vec<(super::event::Key, super::event::Modifiers)> = Vec::new();
        let mut sink = MessageSink::new();

        for i in 0..count {
            let ev = match Event::from_protocol(&proto_events[i]) {
                Some(e) => e,
                None => continue,
            };
            win.track_pointer(&ev);

            match &ev {
                Event::CloseRequest => {
                    let (accept, action) = app.on_close_request();
                    if accept {
                        std::process::exit(0);
                    }
                    process_action(action, &mut needs_rebuild, &mut needs_repaint);
                    continue;
                }
                Event::ClipboardOffer { len } => {
                    if !super::clipboard::accept_offer(*len) {
                        // An empty offer is still an answer; without it the
                        // application waits for data that never comes.
                        let action = app.on_paste(String::new());
                        process_action(action, &mut needs_rebuild, &mut needs_repaint);
                    }
                    continue;
                }
                Event::ClipboardData { len } => {
                    if let Some(text) = super::clipboard::take(*len) {
                        let action = app.on_paste(text);
                        process_action(action, &mut needs_rebuild, &mut needs_repaint);
                    }
                    continue;
                }
                Event::Configure {
                    width: w,
                    height: h,
                } => {
                    let _ = win.resize(*w, *h);
                    window_size = super::constraints::Size::new(*w as i32, *h as i32);
                    let action = app.on_resize(*w, *h);
                    process_action(action, &mut needs_rebuild, &mut needs_repaint);
                    needs_rebuild = true;
                    continue;
                }
                _ => {}
            }

            let widget_event = match translate_event(&ev) {
                Some(e) => e,
                None => continue,
            };

            match &widget_event {
                WidgetEvent::KeyDown { modifiers: m, .. }
                | WidgetEvent::KeyUp { modifiers: m, .. } => {
                    modifiers = *m;
                }
                _ => {}
            }

            let (px, py) = win.pointer();
            let widget_event = fill_pointer_state(widget_event, px, py, modifiers);
            if tip.note(&widget_event, (px, py), slopos_windowing::get_time_ms()) {
                needs_repaint = true;
            }

            gesture.note(&widget_event, slopos_windowing::get_time_ms());
            match &widget_event {
                WidgetEvent::PointerDown { .. } => focus.note_pointer_input(),
                WidgetEvent::KeyDown { .. } | WidgetEvent::TextInput { .. } => {
                    focus.note_keyboard_input()
                }
                _ => {}
            }

            let (px, py) = win.pointer();
            let resp = if let Some(hit) = event::hit_test(root.as_ref(), px, py) {
                if matches!(widget_event, WidgetEvent::PointerDown { .. }) {
                    let target_policy = find_focus_policy(root.as_ref(), hit.target);
                    if target_policy.is_focusable() {
                        let previous = focus.focused();
                        focus.set_focused(Some(hit.target));
                        move_focus_events(root.as_mut(), previous, Some(hit.target), &mut sink);
                    }
                    if overlays.hit_test(px, py).is_none() && !overlays.is_empty() {
                        overlays.dismiss_light(&mut focus);
                    }
                }
                event::dispatch_event(root.as_mut(), &hit, &widget_event, &mut sink)
            } else {
                let dummy_hit = HitTestResult {
                    target: focus.focused().unwrap_or(super::traits::WidgetId::NONE),
                    chain: Vec::new(),
                };
                event::dispatch_event(root.as_mut(), &dummy_hit, &widget_event, &mut sink)
            };

            let blocked = resp == EventResponse::Blocked;
            if resp.is_consumed() {
                needs_repaint = true;
            }
            key_taken |= resp.is_consumed()
                && !blocked
                && matches!(widget_event, WidgetEvent::KeyDown { .. });
            if let (
                true,
                WidgetEvent::KeyDown {
                    key, modifiers: m, ..
                },
            ) = (!resp.is_consumed() || blocked, &widget_event)
            {
                // An editor indents with Tab, so focus only gets what nothing
                // claimed.
                if matches!(key, super::event::Key::Named(super::event::NamedKey::Tab)) {
                    let previous = focus.focused();
                    if m.shift {
                        focus.move_focus_prev();
                    } else {
                        focus.move_focus_next();
                    }
                    move_focus_events(root.as_mut(), previous, focus.focused(), &mut sink);
                    if let Some(id) = focus.focused() {
                        super::focus::reveal(root.as_mut(), id, &mut sink);
                    }
                    needs_repaint = true;
                } else if tree::offer_unhandled(root.as_mut(), *key, *m, &mut sink) {
                    needs_repaint = true;
                } else if let Some(next) = arrow_along_toolbar(root.as_ref(), &focus, *key, *m) {
                    let previous = focus.focused();
                    focus.set_focused(Some(next));
                    move_focus_events(root.as_mut(), previous, Some(next), &mut sink);
                    needs_repaint = true;
                } else if !blocked {
                    unhandled_keys.push((*key, *m));
                }
            }
        }

        for msg in sink.drain_typed::<A::Message>() {
            let action = app.update(msg);
            process_action(action, &mut needs_rebuild, &mut needs_repaint);
        }

        for msg in handle.take_messages() {
            if let Ok(msg) = msg.downcast::<A::Message>() {
                let action = gesture.hold(app.update(*msg), slopos_windowing::get_time_ms());
                process_action(action, &mut needs_rebuild, &mut needs_repaint);
            }
        }

        for (key, mods) in unhandled_keys {
            let action = app.on_key(key, mods);
            process_action(action, &mut needs_rebuild, &mut needs_repaint);
        }

        if let Some(interval) = app.tick_interval_ms() {
            let now_ms = slopos_windowing::get_time_ms();
            if now_ms.wrapping_sub(last_tick_ms) >= interval {
                last_tick_ms = now_ms;
                let action = gesture.hold(app.tick(), now_ms);
                process_action(action, &mut needs_rebuild, &mut needs_repaint);
            }
        }
        needs_rebuild |= gesture.released(slopos_windowing::get_time_ms());
        if tip.ripe(slopos_windowing::get_time_ms()) {
            tip.shown = super::tooltip::find(root.as_ref(), tip.at.0, tip.at.1);
            needs_repaint |= tip.shown.is_some();
        }

        if needs_rebuild {
            let node = app.view();
            let now = app.title();
            if now != title {
                win.set_title(&now);
                title = now;
            }
            let mut scrolled = Vec::new();
            if app.page_key() != page_key {
                page_key = app.page_key();
                focus.leave_page();
            } else {
                tree::kept_scrolls(root.as_ref(), &mut scrolled);
            }
            root = tree::build_widget_tree(&node);
            tree::restore_scrolls(root.as_mut(), &scrolled);
            tree::layout_tree(root.as_mut(), window_size, &style);
            // A rebuild replaces every widget, so whoever holds focus has to
            // be told again. The application's answer comes first: a widget it
            // built focused *is* the focused one, and only when it names none
            // does the remembered chain position apply.
            let declared = find_declared_focus(super::focus::modal_layer(root.as_ref()));
            let restored = focus.rebuild_tab_chain(root.as_ref());
            let target = match declared {
                Some(id) => {
                    focus.set_focused(Some(id));
                    Some(id)
                }
                None => restored,
            };
            if let Some(id) = target {
                // Its own sink: anything emitted here would be applied after
                // the rebuild that caused it.
                let mut focus_sink = MessageSink::new();
                tree::send_to_id(
                    root.as_mut(),
                    id,
                    &WidgetEvent::FocusGained,
                    &mut focus_sink,
                );
            }
            needs_rebuild = false;
            needs_repaint = true;
        }

        if let Some(id) = focus.focused().filter(|_| key_taken) {
            let mut sink = MessageSink::new();
            super::focus::reveal(root.as_mut(), id, &mut sink);
            for msg in sink.drain_typed::<A::Message>() {
                let action = app.update(msg);
                process_action(action, &mut needs_rebuild, &mut needs_repaint);
            }
            needs_repaint = true;
        }

        if needs_repaint {
            // Read before the renderer is borrowed mutably for the frame.
            let pointer = win.pointer();
            if let Some(mut fb) = win.renderer_mut().frame() {
                let fmt = fb.pixel_format();
                fb.clear_canvas(fmt.encode(style.theme.surface));
                let mut ctx = PaintContext::new(&mut fb, &style);
                ctx.focus_visible = focus.is_focus_visible();
                ctx.pointer = pointer;
                tree::paint_tree(root.as_ref(), &mut ctx);
                overlays.paint(&mut ctx);
                if let Some(text) = &tip.shown {
                    super::tooltip::paint(&mut ctx, text, tip.at, window_size);
                }
            }
            win.renderer_mut().present();
            needs_repaint = false;
        }

        let timeout_ms: i64 = if needs_repaint || needs_rebuild {
            0
        } else if count == proto_events.len() {
            // The batch filled the buffer, so the connection still holds
            // decoded events; blocking now would sleep with them in hand.
            0
        } else if let Some(interval) = app.tick_interval_ms() {
            let now = slopos_windowing::get_time_ms();
            let elapsed = now.wrapping_sub(last_tick_ms);
            if elapsed >= interval {
                0
            } else {
                (interval - elapsed) as i64
            }
        } else {
            -1
        };
        let now = slopos_windowing::get_time_ms();
        let timeout_ms = [gesture.wait_ms(now), tip.wait_ms(now)]
            .into_iter()
            .flatten()
            .fold(timeout_ms, |t, wait| {
                if t < 0 {
                    wait as i64
                } else {
                    t.min(wait as i64)
                }
            });
        handle.wait_events(timeout_ms);
    }
}

/// The longest a press holds rebuilds back, since a release the compositor
/// drops would otherwise hold them until the next click.
const HOLD_MS: u64 = 3000;

/// Holds back, while a pointer button is down, a rebuild that the gesture's
/// own messages did not ask for: a press and the drag it starts live in the
/// widgets they began on, which a thread's message or a tick would replace.
#[derive(Default)]
pub(crate) struct Gesture {
    buttons: u8,
    since_ms: u64,
    waiting: bool,
}

impl Gesture {
    pub(crate) fn note(&mut self, event: &WidgetEvent, now_ms: u64) {
        let bit = |button: &PointerButton| match button {
            PointerButton::Left => 1,
            PointerButton::Right => 2,
            PointerButton::Middle => 4,
        };
        match event {
            WidgetEvent::PointerDown { button, .. } => {
                if self.buttons == 0 {
                    self.since_ms = now_ms;
                }
                self.buttons |= bit(button);
            }
            WidgetEvent::PointerUp { button, .. } => self.buttons &= !bit(button),
            _ => {}
        }
    }

    fn holding(&mut self, now_ms: u64) -> bool {
        if now_ms.saturating_sub(self.since_ms) >= HOLD_MS {
            self.buttons = 0;
        }
        self.buttons != 0
    }

    /// `action`, or nothing while a button is down and it would rebuild.
    pub(crate) fn hold(&mut self, action: Action, now_ms: u64) -> Action {
        if action == Action::Rebuild && self.holding(now_ms) {
            self.waiting = true;
            return Action::None;
        }
        action
    }

    /// Whether a held rebuild may run now.
    pub(crate) fn released(&mut self, now_ms: u64) -> bool {
        !self.holding(now_ms) && core::mem::take(&mut self.waiting)
    }

    /// How long until a held rebuild runs whether or not the button is up.
    pub(crate) fn wait_ms(&self, now_ms: u64) -> Option<u64> {
        self.waiting
            .then(|| (self.since_ms + HOLD_MS).saturating_sub(now_ms))
    }
}

fn process_action(action: Action, needs_rebuild: &mut bool, needs_repaint: &mut bool) {
    match action {
        Action::None => {}
        Action::Repaint => *needs_repaint = true,
        Action::Rebuild => {
            *needs_rebuild = true;
        }
        Action::Exit => std::process::exit(0),
    }
}

fn fill_pointer_state(
    mut event: WidgetEvent,
    px: i32,
    py: i32,
    mods: super::event::Modifiers,
) -> WidgetEvent {
    match &mut event {
        WidgetEvent::PointerDown {
            x, y, modifiers, ..
        } => {
            *x = px;
            *y = py;
            *modifiers = mods;
        }
        WidgetEvent::PointerUp { x, y, .. } | WidgetEvent::Scroll { x, y, .. } => {
            *x = px;
            *y = py;
        }
        _ => {}
    }
    event
}

/// Tells the widget losing focus and the one gaining it.
///
/// Without them every widget's `focused` flag stays false, and the gate each
/// one puts on its keys never opens.
fn move_focus_events(
    root: &mut dyn Widget,
    previous: Option<super::traits::WidgetId>,
    next: Option<super::traits::WidgetId>,
    sink: &mut MessageSink,
) {
    if previous == next {
        return;
    }
    if let Some(id) = previous {
        tree::send_to_id(root, id, &WidgetEvent::FocusLost, sink);
    }
    if let Some(id) = next {
        tree::send_to_id(root, id, &WidgetEvent::FocusGained, sink);
    }
}

/// The last widget in depth-first order that the application built as focused.
///
/// Last rather than first: a popup is appended over the layer beneath it, so
/// an open menu's own claim outranks the field it is covering.
fn find_declared_focus(widget: &dyn Widget) -> Option<super::traits::WidgetId> {
    let mut found = widget.declares_focus().then(|| widget.id());
    for child in widget.children() {
        if let Some(id) = find_declared_focus(child.as_ref()) {
            found = Some(id);
        }
    }
    found
}

fn find_focus_policy(widget: &dyn Widget, id: super::traits::WidgetId) -> FocusPolicy {
    if widget.id() == id {
        return widget.focus_policy();
    }
    for child in widget.children() {
        let result = find_focus_policy(child.as_ref(), id);
        if result.is_focusable() {
            return result;
        }
    }
    FocusPolicy::None
}

fn arrow_along_toolbar(
    root: &dyn Widget,
    focus: &FocusManager,
    key: super::event::Key,
    modifiers: super::event::Modifiers,
) -> Option<super::traits::WidgetId> {
    use super::event::{Key, NamedKey};
    if modifiers.ctrl || modifiers.alt || modifiers.shift {
        return None;
    }
    let forward = match key {
        Key::Named(NamedKey::Right) => true,
        Key::Named(NamedKey::Left) => false,
        _ => return None,
    };
    super::focus::beside_in_toolbar(root, focus.focused()?, forward)
}

/// Where the pointer rests, since when, and the tooltip shown there.
#[derive(Default)]
struct Tip {
    at: (i32, i32),
    since_ms: u64,
    /// Waiting for the pointer to rest; a press or a key ends the wait until
    /// it moves again.
    armed: bool,
    shown: Option<String>,
}

impl Tip {
    /// Answers whether a shown tooltip went away.
    fn note(&mut self, event: &WidgetEvent, at: (i32, i32), now_ms: u64) -> bool {
        let rest = match event {
            WidgetEvent::PointerMove { .. } if at != self.at => true,
            WidgetEvent::PointerDown { .. }
            | WidgetEvent::KeyDown { .. }
            | WidgetEvent::Scroll { .. }
            | WidgetEvent::PointerLeave => false,
            _ => return false,
        };
        self.at = at;
        self.since_ms = now_ms;
        self.armed = rest;
        self.shown.take().is_some()
    }

    fn ripe(&mut self, now_ms: u64) -> bool {
        if self.armed && now_ms.saturating_sub(self.since_ms) >= super::tooltip::DELAY_MS {
            self.armed = false;
            return true;
        }
        false
    }

    fn wait_ms(&self, now_ms: u64) -> Option<u64> {
        self.armed
            .then(|| (self.since_ms + super::tooltip::DELAY_MS).saturating_sub(now_ms))
    }
}
