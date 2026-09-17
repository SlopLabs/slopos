use slopos_abi::Canvas;
use slopos_abi::syscall::posix::POLLIN;
use slopos_gfx::RenderSurface;
use slopos_protocol::types::Event as ProtocolEvent;
use slopos_rt::Ring;
use slopos_rt::slopfut;

use slopos_windowing::Event;
use slopos_windowing::connection;
use slopos_windowing::{EVENT_BUF_LEN, Window};

use super::event::{self, HitTestResult, MessageSink, WidgetEvent};
use super::focus::FocusManager;
use super::input::translate_event;
use super::node::{Action, App};
use super::overlay::OverlayManager;
use super::paint::PaintContext;
use super::style::StyleSheet;
use super::traits::{FocusPolicy, Widget};
use super::tree;

/// Run a widget-framework-driven application.
pub fn run_app<A: App>(app: A, width: u32, height: u32) -> ! {
    let ring = Ring::setup(16).expect("appkit: ring setup failed");
    slopfut::block_on(ring, run_app_async(app, width, height))
}

async fn run_app_async<A: App>(mut app: A, width: u32, height: u32) -> ! {
    let handle = connection::connect().expect("compositor not running");
    let mut win = Window::new(handle.clone(), width, height).expect("failed to create window");
    win.set_title(app.title());
    let id = app.app_id();
    if !id.is_empty() {
        win.set_app_id(id);
    }
    super::clipboard::install(handle.clone());
    let style = StyleSheet::dark();
    let mut focus = FocusManager::new();
    let mut overlays = OverlayManager::new();

    let node = app.view();
    let mut root = tree::build_widget_tree(&node);

    let mut window_size = super::constraints::Size::new(width as i32, height as i32);
    tree::layout_tree(root.as_mut(), window_size, &style);
    let _ = focus.rebuild_tab_chain(root.as_ref());

    let mut needs_rebuild = false;
    let mut needs_repaint = true;
    let mut proto_events: [ProtocolEvent; EVENT_BUF_LEN] =
        core::array::from_fn(|_| ProtocolEvent::FrameDone {
            surface: slopos_protocol::types::SurfaceId::NONE,
            timestamp_ms: 0,
        });
    let mut last_tick_ms: u64 = slopos_windowing::get_time_ms();
    // The compositor reports modifiers only with key events; a pointer press
    // takes the most recent snapshot, which is what shift-click is.
    let mut modifiers = super::event::Modifiers::default();

    loop {
        handle.flush_pending_destroys();
        handle.drain_ui_queue();

        let count = win.poll_protocol_events(&mut proto_events);
        // Every unconsumed key, not just the last: a poll can return a batch,
        // and an `Option` here silently dropped all but one of them.
        let mut unhandled_keys: Vec<(super::event::Key, super::event::Modifiers)> = Vec::new();
        let mut sink = MessageSink::new();

        for i in 0..count {
            let ev = match Event::from_protocol(&proto_events[i]) {
                Some(e) => e,
                None => continue,
            };
            win.track_pointer(&ev);

            match &ev {
                Event::CloseRequest => std::process::exit(0),
                Event::ClipboardOffer { len } => {
                    if !super::clipboard::accept_offer(*len) {
                        // An empty offer, or one we could not take delivery of,
                        // is still an answer: without it a paste with nothing
                        // on the selection would leave the application waiting
                        // for a `ClipboardData` that never comes, and its own
                        // fallback unreachable.
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

            if resp.is_consumed() {
                needs_repaint = true;
            } else if let WidgetEvent::KeyDown {
                key, modifiers: m, ..
            } = &widget_event
            {
                // Tab moves focus only where nothing claimed it: an editor
                // indents with Tab, and stealing it before dispatch would make
                // that impossible to express.
                if matches!(key, super::event::Key::Named(super::event::NamedKey::Tab)) {
                    let previous = focus.focused();
                    if m.shift {
                        focus.move_focus_prev();
                    } else {
                        focus.move_focus_next();
                    }
                    move_focus_events(root.as_mut(), previous, focus.focused(), &mut sink);
                    needs_repaint = true;
                } else {
                    unhandled_keys.push((*key, *m));
                }
            }
        }

        for msg in sink.drain_typed::<A::Message>() {
            let action = app.update(msg);
            process_action(action, &mut needs_rebuild, &mut needs_repaint);
        }

        for (key, mods) in unhandled_keys {
            let action = app.on_key(key, mods);
            process_action(action, &mut needs_rebuild, &mut needs_repaint);
        }

        if let Some(interval) = app.tick_interval_ms() {
            let now_ms = slopos_windowing::get_time_ms();
            if now_ms.wrapping_sub(last_tick_ms) >= interval {
                last_tick_ms = now_ms;
                let action = app.tick();
                process_action(action, &mut needs_rebuild, &mut needs_repaint);
            }
        }

        if needs_rebuild {
            let node = app.view();
            root = tree::build_widget_tree(&node);
            tree::layout_tree(root.as_mut(), window_size, &style);
            // A rebuild replaces every widget, so the one that held the focus
            // has to be told again — otherwise the focus gate every widget now
            // consults is false for the rest of the session and Enter, Space
            // and the arrows reach nothing.
            //
            // The application's own answer comes first. A widget it built as
            // focused *is* the focused widget; only when it names none does the
            // framework's remembered chain position apply. Without that
            // precedence the two disagree, and since keys are offered to every
            // widget until one consumes, the framework's stale answer wins: a
            // button clicked once keeps eating the Enter and Space meant for
            // the field the application focused.
            let declared = find_declared_focus(root.as_ref());
            let restored = focus.rebuild_tab_chain(root.as_ref());
            let target = match declared {
                Some(id) => {
                    focus.set_focused(Some(id));
                    Some(id)
                }
                None => restored,
            };
            if let Some(id) = target {
                // Its own sink: gaining focus is a notification, not a message
                // source, and anything emitted here would be applied after the
                // rebuild it caused.
                let mut focus_sink = MessageSink::new();
                send_to_id(
                    root.as_mut(),
                    id,
                    &WidgetEvent::FocusGained,
                    &mut focus_sink,
                );
            }
            needs_rebuild = false;
            needs_repaint = true;
        }

        if needs_repaint {
            // Read before the renderer is borrowed mutably for the frame.
            let pointer = win.pointer();
            if let Some(mut fb) = win.renderer_mut().frame() {
                let fmt = fb.pixel_format();
                fb.clear_canvas(fmt.encode(style.bg_primary));
                let mut ctx = PaintContext::new(&mut fb, &style);
                ctx.focus_visible = focus.is_focus_visible();
                ctx.pointer = pointer;
                tree::paint_tree(root.as_ref(), &mut ctx);
                overlays.paint(&mut ctx);
            }
            win.renderer_mut().present();
            needs_repaint = false;
        }

        let timeout_ms: i64 = if needs_repaint || needs_rebuild {
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
        // A zero timeout deliberately awaits nothing.
        if timeout_ms < 0 {
            match slopfut::select2(
                slopfut::poll_add(handle.compositor_fd(), POLLIN),
                slopfut::poll_add(handle.wakeup_read_fd(), POLLIN),
            )
            .await
            {
                slopfut::Either2::A(_) => {}
                slopfut::Either2::B(_) => handle.drain_wakeup(),
            }
        } else if timeout_ms > 0 {
            // `sleep_ms` is an `async fn` and so not `Unpin`; the by-reference
            // `select3` needs it pinned.
            let timer: core::pin::Pin<Box<dyn core::future::Future<Output = ()>>> =
                Box::pin(slopfut::time::sleep_ms(timeout_ms as u64));
            match slopfut::select3(
                slopfut::poll_add(handle.compositor_fd(), POLLIN),
                slopfut::poll_add(handle.wakeup_read_fd(), POLLIN),
                timer,
            )
            .await
            {
                slopfut::Either3::A(_) => {}
                slopfut::Either3::B(_) => handle.drain_wakeup(),
                slopfut::Either3::C(_) => {}
            }
        }
    }
}

fn process_action(action: Action, needs_rebuild: &mut bool, _needs_repaint: &mut bool) {
    match action {
        Action::None => {}
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
/// Nothing sent these before, so every widget's `focused` flag was permanently
/// false — which is why `ButtonWidget` and `ListViewWidget` acted on Enter,
/// Space and the arrow keys without ever asking whether the key was theirs.
/// Keyboard events are offered to every child until one consumes, so a widget
/// that answers a key it was not given takes it from whoever was.
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
        send_to_id(root, id, &WidgetEvent::FocusLost, sink);
    }
    if let Some(id) = next {
        send_to_id(root, id, &WidgetEvent::FocusGained, sink);
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

fn send_to_id(
    widget: &mut dyn Widget,
    id: super::traits::WidgetId,
    event: &WidgetEvent,
    sink: &mut MessageSink,
) -> bool {
    if widget.id() == id {
        widget.event(event, super::event::EventPhase::Target, sink);
        return true;
    }
    for child in widget.children_mut() {
        if send_to_id(child.as_mut(), id, event, sink) {
            return true;
        }
    }
    false
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
