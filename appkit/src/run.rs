use slopos_abi::Canvas;
use slopos_gfx::RenderSurface;

use slopos_windowing::Event;
use slopos_windowing::Window;
use slopos_windowing::connection;

use super::constraints::Size;
use super::event::{
    EventResponse, Key, MessageSink, Modifiers, NamedKey, PointerButton, WidgetEvent, control_at,
    lit_at,
};
use super::focus::FocusManager;
use super::input::translate_event;
use super::node::{Action, App};
use super::paint::PaintContext;
use super::sender::AppSender;
use super::style::StyleSheet;
use super::traits::{Widget, WidgetId};
use super::tree;

/// The events one pass takes before it draws.
const EVENTS_PER_PASS: usize = 16;

/// Runs `app` in a window as [`App::window`] describes it.
pub fn run_app<A: App>(mut app: A) -> ! {
    let spec = app.window();
    let (width, height) = (
        spec.size.width.max(1) as u32,
        spec.size.height.max(1) as u32,
    );
    let handle = connection::connect().unwrap_or_else(|e| panic!("{e}"));
    let mut win = Window::new(handle.clone(), width, height).unwrap_or_else(|e| panic!("{e}"));
    let mut title = app.title();
    win.set_title(&title);
    let (min, max) = spec.limits();
    let pair = |s: Size| (s.width.max(0) as u32, s.height.max(0) as u32);
    win.set_size_limits(pair(min), max.map_or((0, 0), pair));
    win.set_app_id(app.app_id());
    super::clipboard::install(handle.clone());
    let style = StyleSheet::current();
    let mut focus = FocusManager::new();

    let mut needs_rebuild = false;
    let mut needs_repaint = true;
    let mut gesture = Gesture::default();
    let action = app.on_start(AppSender::new(handle.ui_sender()));
    process_action(action, &mut needs_rebuild, &mut needs_repaint);

    let node = app.view();
    let mut root = tree::build_widget_tree(&node);
    let mut page_key = app.page_key();

    let mut window_size = Size::new(width as i32, height as i32);
    tree::layout_tree(root.as_mut(), window_size, &style);
    let _ = focus.rebuild_tab_chain(root.as_ref());

    let mut last_tick_ms: u64 = slopos_windowing::get_time_ms();
    let mut modifiers = Modifiers::default();
    let mut tip = Tip::default();
    let mut inside = false;
    let mut hovered: Option<(WidgetId, usize)> = None;

    loop {
        let mut key_taken = false;
        handle.flush_pending_destroys();
        handle.drain_ui_queue();

        let mut unhandled_keys: Vec<(Key, Modifiers)> = Vec::new();
        let mut sink = MessageSink::new();

        let mut taken = 0;
        while taken < EVENTS_PER_PASS {
            let Some(ev) = win.next_event() else {
                break;
            };
            taken += 1;

            match &ev {
                Event::PointerLeave => {
                    inside = false;
                    needs_repaint |= tip.clear() | hovered.take().is_some();
                    continue;
                }
                Event::PointerEnter { .. } | Event::PointerMotion { .. } => inside = true,
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
                    window_size = Size::new(*w as i32, *h as i32);
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
                WidgetEvent::PointerDown { .. } => {
                    focus.note_pointer_input();
                    let control = control_at(root.as_ref(), px, py);
                    let clicked = control.filter(|w| w.focus_policy().is_click_focusable());
                    if let Some(clicked) = clicked.map(|w| w.id()) {
                        let previous = focus.focused();
                        focus.set_focused(Some(clicked));
                        move_focus_events(root.as_mut(), previous, Some(clicked), &mut sink);
                    }
                }
                WidgetEvent::KeyDown { .. } | WidgetEvent::TextInput { .. } => {
                    focus.note_keyboard_input()
                }
                _ => {}
            }

            let resp = root.event(&widget_event, &mut sink);
            if let WidgetEvent::PointerMove { .. } = widget_event {
                let lit = lit_at(root.as_ref(), px, py);
                needs_repaint |= lit != hovered;
                hovered = lit;
            }

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
                // Only a Tab nothing took moves focus: an editor indents with it.
                if matches!(key, Key::Named(NamedKey::Tab)) {
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
                tree::kept(root.as_ref(), &mut scrolled);
            }
            root = tree::build_widget_tree(&node);
            tree::restore_kept(root.as_mut(), &scrolled);
            tree::layout_tree(root.as_mut(), window_size, &style);
            // Every widget is new, so whoever holds focus is told again; one
            // the app built focused wins over the remembered place.
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
            // Again, since a rebuild or a scroll may have moved what is there.
            let (px, py) = win.pointer();
            hovered = lit_at(root.as_ref(), px, py).filter(|_| inside);
            if let Some(mut fb) = win.renderer_mut().frame() {
                let fmt = fb.pixel_format();
                fb.clear_canvas(fmt.encode(style.theme.surface));
                let mut ctx = PaintContext::new(&mut fb, &style);
                ctx.focus_visible = focus.is_focus_visible();
                ctx.hovered = hovered;
                root.paint(&mut ctx);
                if let Some(text) = &tip.shown {
                    super::tooltip::paint(&mut ctx, text, tip.at, window_size);
                }
            }
            win.renderer_mut().present();
            needs_repaint = false;
        }

        let timeout_ms: i64 = if needs_repaint || needs_rebuild {
            0
        } else if taken == EVENTS_PER_PASS {
            // The connection may hold decoded events already, which a wait
            // on the socket would sleep over.
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

fn fill_pointer_state(mut event: WidgetEvent, px: i32, py: i32, mods: Modifiers) -> WidgetEvent {
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

fn move_focus_events(
    root: &mut dyn Widget,
    previous: Option<WidgetId>,
    next: Option<WidgetId>,
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

/// The last widget in depth-first order the app built focused: a popup comes
/// after the layer it covers, so an open menu's claim outranks the field's.
fn find_declared_focus(widget: &dyn Widget) -> Option<WidgetId> {
    let mut found = widget.declares_focus().then(|| widget.id());
    for child in widget.children() {
        if let Some(id) = find_declared_focus(child.as_ref()) {
            found = Some(id);
        }
    }
    found
}

fn arrow_along_toolbar(
    root: &dyn Widget,
    focus: &FocusManager,
    key: Key,
    modifiers: Modifiers,
) -> Option<WidgetId> {
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
pub(crate) struct Tip {
    at: (i32, i32),
    since_ms: u64,
    /// Waiting for the pointer to rest, until a press or a key.
    armed: bool,
    shown: Option<String>,
}

impl Tip {
    /// Answers whether a shown tooltip went away.
    pub(crate) fn note(&mut self, event: &WidgetEvent, at: (i32, i32), now_ms: u64) -> bool {
        let rest = match event {
            WidgetEvent::PointerMove { .. } if at != self.at => true,
            WidgetEvent::PointerDown { .. }
            | WidgetEvent::KeyDown { .. }
            | WidgetEvent::Scroll { .. } => false,
            _ => return false,
        };
        self.at = at;
        self.since_ms = now_ms;
        self.armed = rest;
        self.shown.take().is_some()
    }

    /// The pointer left: answers whether a shown tooltip went away.
    pub(crate) fn clear(&mut self) -> bool {
        self.armed = false;
        self.shown.take().is_some()
    }

    pub(crate) fn ripe(&mut self, now_ms: u64) -> bool {
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
