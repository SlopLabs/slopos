mod confirm;
pub mod consent;
pub mod decorations;
pub mod dock;
mod frame;
mod gate;
mod hover;
mod input;
pub mod menu_bar;
mod output;
mod popover;
pub mod protocol;
mod region;
mod renderer;
mod status_item;
mod surface_cache;
pub mod views;

use crate::gfx::DamageRect;
use crate::init_control::Command;
use crate::net_query;
use crate::ring::{Ring, slopfut};
use crate::syscall::{DisplayInfo, UserWindowInfo, core as sys_core, process, tty, window};
use crate::theme::*;
use region::Region;
use slopos_abi::input::POINTER_AXIS_VERTICAL;
use slopos_abi::net::{
    NET_EVENT_LEN, NET_IFINDEX_NONE, NET_MON_DEFAULT, NET_Q_ADDRS, NET_Q_GLOBAL, NET_Q_IFACES,
    UserAddr, UserIface, UserNetGlobal,
};
use slopos_abi::syscall::POLLIN;
use slopos_chrome_core::clock::WallTime;
use slopos_chrome_core::menu::{Choice, Ending};
use slopos_chrome_core::status::MAX_STATUS_ITEMS;
use slopos_chrome_core::{
    IfaceKind, IfaceRow, NetIndicatorState, NetPanelModel, indicator_state_for,
};
use slopos_protocol::server::MAX_CLIENTS;
use std::time::Instant;
use std::vec::Vec;

use frame::WindowBounds;
use hover::HoverRegistry;
use input::InputHandler;
use output::{CompositorOutput, FrameMetrics, RenderMode, estimate_present_bytes};
use protocol::ProtocolBridge;
use renderer::Renderer;
use slopos_abi::surface::WINDOW_STATE_MINIMIZED;
use surface_cache::ClientSurfaceCache;

const MAX_WINDOWS: usize = 32;

struct WindowManager {
    windows: [UserWindowInfo; MAX_WINDOWS],
    window_count: u32,
    prev_windows: [UserWindowInfo; MAX_WINDOWS],
    prev_window_count: u32,

    input: InputHandler,
    renderer: Renderer,
    hover_registry: HoverRegistry,
    surface_cache: ClientSurfaceCache,

    system_bar: menu_bar::SystemBar,
    shelf: dock::LauncherShelf,
    /// Compositor-owned chrome; holds a pointer grab while open.
    popover: popover::Popover,
    /// What a choice that ends something asks first, while it is up.
    asking: confirm::Asking,
    /// Endings the person went ahead with that did not happen, for
    /// [`confirm::Asking::failed`] to say so.
    undone: std::sync::mpsc::Receiver<Ending>,
    undone_tx: std::sync::mpsc::Sender<Ending>,
    /// privd's question, while one is up: above everything, and the only
    /// thing input reaches.
    consent: consent::Consent,
    /// The login screen, or this desktop's lock screen, above even that.
    gate: gate::Gate,
    /// Another session is in front: nothing is drawn, and init says when
    /// this one is again.
    behind: bool,
    init_says: Option<InitSays>,
    /// A new base changed the session's programs: open the system panel,
    /// which offers the restart, on the next frame.
    restart_offer_due: bool,

    /// `None` if the AF_UNIX bind failed.
    protocol: Option<Box<ProtocolBridge>>,

    protocol_serial: u32,
    protocol_pointer_focus: u32,
    /// The surface holding the implicit pointer grab, or 0.
    ///
    /// A press on a client's content pins pointer delivery to that client
    /// until every button is up again — the `wl_pointer` rule. Without it a
    /// drag that leaves the window takes the pointer's focus with it, the
    /// release is delivered to whatever is underneath instead, and the client
    /// is left holding a drag it was never told had ended.
    protocol_pointer_grab: u32,
    /// The grabbed surface's generation, because the task id alone is a slot
    /// index and slots are recycled.
    protocol_pointer_grab_gen: u32,

    first_frame: bool,
    /// This frame's accumulated, disjoint damage region.
    output_damage: Region,
    /// Tracked separately from `output_damage` so that region stays a precise
    /// rect set.
    force_full_redraw: bool,
    /// Damage from a present that did not reach the screen, carried forward
    /// until actually shown.
    pending_damage: Region,
    /// A suppressed/failed *full* present to retry next frame.
    pending_full: bool,
    /// Surfaces (by `task_id`) whose committed damage was folded into this
    /// frame. Only these are cleared after a present that reached the screen —
    /// a commit landing after the snapshot stays dirty and exports next frame.
    frame_dirty_surfaces: Vec<u32>,
    prev_window_bounds: [WindowBounds; MAX_WINDOWS],
    prev_cursor_shape: u8,
    net_cache: NetPanelModel,
    net_indicator: NetIndicatorState,
    /// Only consulted on the fallback path — see [`WindowManager::net_event_driven`].
    net_last_poll: Option<Instant>,
    /// Whether a `net_monitor` fd is driving refreshes. When it is, the poll
    /// timer is switched off entirely rather than kept as a backstop, so a
    /// monitor that stopped firing is noticed instead of masked.
    net_event_driven: bool,
    /// `None` = not yet probed, `Some(true)` = the virtio-gpu overlay owns the
    /// pointer (software cursor suppressed), `Some(false)` = unavailable.
    hw_cursor: Option<bool>,
    /// Cursor shape last uploaded to the hardware overlay (`-1` = none).
    hw_cursor_shape: i32,
    /// Last position sent to the hardware cursor (`i32::MIN` = never), so the
    /// blocking move is only issued when the pointer actually moved.
    hw_cursor_last_x: i32,
    hw_cursor_last_y: i32,
}

impl WindowManager {
    fn new(consent: consent::Consent, gate: gate::Gate) -> Self {
        let mut shelf = dock::LauncherShelf::new();
        let home = std::env::var("HOME").ok();
        if !gate.greeter() {
            shelf.pin(crate::app_entry::pinned(home.as_deref()));
        }
        let protocol = match gate.greeter() {
            true => None,
            false => ProtocolBridge::new().map(Box::new),
        };
        let (undone_tx, undone) = std::sync::mpsc::channel();
        Self {
            windows: [UserWindowInfo::default(); MAX_WINDOWS],
            window_count: 0,
            prev_windows: [UserWindowInfo::default(); MAX_WINDOWS],
            prev_window_count: 0,
            input: InputHandler::new(),
            renderer: Renderer::new(),
            hover_registry: HoverRegistry::new(),
            surface_cache: ClientSurfaceCache::new(),
            system_bar: menu_bar::SystemBar::new(),
            shelf,
            popover: popover::Popover::new(
                net_toggle_unasked(),
                gate.person().unwrap_or_default().to_owned(),
            ),
            asking: confirm::Asking::new(),
            undone,
            undone_tx,
            consent,
            gate,
            behind: false,
            init_says: None,
            restart_offer_due: false,
            protocol,
            protocol_serial: 0,
            protocol_pointer_focus: 0,
            protocol_pointer_grab: 0,
            protocol_pointer_grab_gen: 0,
            first_frame: true,
            output_damage: Region::new(),
            force_full_redraw: false,
            pending_damage: Region::new(),
            pending_full: false,
            frame_dirty_surfaces: Vec::new(),
            prev_window_bounds: [WindowBounds::default(); MAX_WINDOWS],
            prev_cursor_shape: 0,
            net_cache: NetPanelModel::EMPTY,
            net_indicator: NetIndicatorState::Disconnected,
            net_last_poll: None,
            net_event_driven: false,
            hw_cursor: None,
            hw_cursor_shape: -1,
            hw_cursor_last_x: i32::MIN,
            hw_cursor_last_y: i32::MIN,
        }
    }

    /// Drive the virtio-gpu hardware cursor: probe lazily, re-upload on shape
    /// change, and report whether the overlay owns the pointer (so the renderer
    /// skips compositing a software cursor into the frame).
    fn update_hw_cursor(&mut self, shape: u8) -> bool {
        match self.hw_cursor {
            Some(false) => return false,
            Some(true) if self.hw_cursor_shape == shape as i32 => return true,
            _ => {}
        }
        const N: usize = (renderer::HW_CURSOR_DIM * renderer::HW_CURSOR_DIM * 4) as usize;
        let mut pixels = [0u8; N];
        self.renderer.render_cursor_image(shape, &mut pixels);
        let ok = window::cursor_set_image(
            &pixels,
            renderer::HW_CURSOR_HOTSPOT,
            renderer::HW_CURSOR_HOTSPOT,
        ) == 0;
        if ok {
            self.hw_cursor = Some(true);
            self.hw_cursor_shape = shape as i32;
            return true;
        }
        // A failed re-upload after activation stays on hardware, keeping the
        // last good overlay shape, so overlay and software cursor never coexist.
        if self.hw_cursor.is_none() {
            self.hw_cursor = Some(false);
            false
        } else {
            true
        }
    }

    /// Rebuild the network model from the kernel and republish the indicator.
    ///
    /// A failed query leaves the previous model in place rather than falling
    /// back to `EMPTY`: a transient error is not evidence the network went away.
    fn refresh_from_kernel(&mut self) {
        let Some(model) = read_net_model() else {
            return;
        };
        self.net_indicator = indicator_state_for(&model);
        self.net_cache = model;
        self.system_bar.set_network(true, self.net_indicator);
    }

    fn refresh_windows(&mut self) {
        self.prev_windows = self.windows;
        self.prev_window_count = self.window_count;
        let saved_bounds = self.prev_window_bounds;

        let raw_count = if let Some(ref proto) = self.protocol {
            proto.get_windows(&mut self.windows) as i64
        } else {
            0
        };
        self.window_count = if raw_count > 0 {
            (raw_count as usize).min(MAX_WINDOWS) as u32
        } else {
            0
        };

        self.surface_cache
            .cleanup_stale(&self.windows, self.window_count);

        self.shelf
            .sync_running_apps(&self.windows, self.window_count);

        self.output_damage.clear();
        self.frame_dirty_surfaces.clear();

        if self.pending_full {
            self.force_full_redraw = true;
            self.pending_full = false;
        }
        if !self.pending_damage.is_empty() {
            let pending = core::mem::take(&mut self.pending_damage);
            self.output_damage.push_region(&pending);
        }

        for i in 0..self.window_count as usize {
            let window = self.windows[i];
            let curr_bounds = WindowBounds::from_window(&window);

            let prev = self.find_prev_index(window.task_id);

            if let Some(prev_idx) = prev {
                let old = saved_bounds[prev_idx];
                let geometry_changed = old != curr_bounds;
                // A pure restack changes no geometry but changes occlusion, so
                // the covered/revealed area must be repainted.
                let restacked = prev_idx != i;
                let retitled = self.prev_windows[prev_idx].title != window.title;
                if geometry_changed || restacked || retitled {
                    self.add_bounds_damage(&old);
                    self.add_bounds_damage(&curr_bounds);
                }
            } else if curr_bounds.rect().is_valid() {
                self.force_full_redraw = true;
            }

            self.prev_window_bounds[i] = curr_bounds;

            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            if window.is_dirty() {
                self.add_window_damage(&window);
                self.frame_dirty_surfaces.push(window.task_id);
            }
        }

        for i in 0..self.prev_window_count as usize {
            let prev = &self.prev_windows[i];
            if !self.window_exists(prev.task_id) {
                self.add_bounds_damage(&saved_bounds[i]);
            }
        }
    }

    /// Cursor-trail / shelf / hover damage. Must run after the frame's input
    /// events are dispatched: `process_input_events` populates the cursor trail.
    fn add_pointer_damage(&mut self) {
        let cursor_moved = self.input.cursor_trail_count > 0;
        let shelf_content_changed = self.shelf.take_content_dirty();
        if cursor_moved || shelf_content_changed {
            let shelf_bounds = self.shelf.bounds();
            if shelf_bounds.is_valid() {
                self.output_damage.add_rect(
                    shelf_bounds.x0,
                    shelf_bounds.y0,
                    shelf_bounds.x1,
                    shelf_bounds.y1,
                );
            }
        }

        if self.input.cursor_trail_count > 0 {
            for i in 0..self.input.cursor_trail_count {
                let (x, y) = self.input.cursor_trail[i];
                self.add_cursor_damage_at(x, y);
            }
            self.add_cursor_damage_at(self.input.mouse_x, self.input.mouse_y);
        }

        self.register_hover_regions();
    }

    fn register_hover_regions(&mut self) {
        self.hover_registry.begin_frame();

        if self.renderer.output_height == 0 {
            return;
        }

        // The renderer shows/hides the button glyphs from this hover state.
        for i in (0..self.window_count as usize).rev() {
            let w = self.windows[i];
            if w.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            let frame_y = w.y - TITLE_BAR_HEIGHT;
            let group_hovered = decorations::hit_test_signal_group(
                w.x,
                frame_y,
                self.input.mouse_x,
                self.input.mouse_y,
            );

            let gx = w.x + SIGNAL_GROUP_X;
            let gy = frame_y + SIGNAL_GROUP_Y;
            self.hover_registry.register(
                hover::HOVER_SIGNAL_GROUP_BASE | w.task_id,
                DamageRect {
                    x0: gx,
                    y0: gy,
                    x1: gx + SIGNAL_GROUP_W - 1,
                    y1: gy + SIGNAL_GROUP_H - 1,
                },
                group_hovered,
            );
        }

        let mut status_regions = [(0u32, DamageRect::invalid(), false); MAX_STATUS_ITEMS];
        let status_count = self.system_bar.hover_regions(
            self.renderer.output_width,
            self.input.mouse_x,
            self.input.mouse_y,
            &mut status_regions,
        );
        for &(id, rect, hovered) in &status_regions[..status_count] {
            self.hover_registry.register(id, rect, hovered);
        }

        let mut hover_damage = [DamageRect::invalid(); 32];
        let hover_damage_count = self.hover_registry.changed_regions(&mut hover_damage);
        for i in 0..hover_damage_count {
            self.output_damage.add_rect(
                hover_damage[i].x0,
                hover_damage[i].y0,
                hover_damage[i].x1,
                hover_damage[i].y1,
            );
        }
    }

    /// The index this `task_id` held in the previous frame's window list. The
    /// index doubles as its stacking rank (bottom = 0), so callers detect
    /// restacks by comparing it against the current index.
    fn find_prev_index(&self, task_id: u32) -> Option<usize> {
        (0..self.prev_window_count as usize).find(|&i| self.prev_windows[i].task_id == task_id)
    }

    fn window_exists(&self, task_id: u32) -> bool {
        (0..self.window_count as usize).any(|i| self.windows[i].task_id == task_id)
    }

    fn add_bounds_damage(&mut self, bounds: &WindowBounds) {
        let rect = bounds.rect();
        if rect.is_valid() {
            self.output_damage
                .add_rect(rect.x0, rect.y0, rect.x1, rect.y1);
        }
    }

    /// Dispatch the frame's raw input events in stream order: each event is
    /// processed against the input state accumulated up to that event, so a
    /// press is hit-tested at its own pointer position rather than at the
    /// end-of-batch one.
    fn process_input_events(&mut self, fb_width: i32, fb_height: i32) {
        use slopos_abi::InputEventType;

        // Taken out so it can be passed mutably into `InputHandler` methods
        // without borrowing all of `self`.
        let mut proto_box = self.protocol.take();

        for i in 0..self.input.raw_event_count {
            let event = self.input.raw_event_buf[i];
            let time = event.timestamp_ms as u32;
            let primary = event.data.data0 & 0x01 != 0;
            let gated = match event.event_type {
                InputEventType::KeyPress => self.gate.takes_press(&event),
                InputEventType::KeyRelease => self.gate.takes_release(event.key_keycode()),
                _ => false,
            };
            if gated {
                self.input.set_modifier_state(event.key_modifiers());
                continue;
            }
            if self.gate.shown() {
                self.popover.dismiss();
                self.asking.withdraw();
                match event.event_type {
                    InputEventType::PointerMotion => {
                        self.input.apply_motion(&event);
                        self.gate.hover(self.input.mouse_x, self.input.mouse_y);
                    }
                    InputEventType::PointerButtonPress if primary => {
                        self.gate
                            .click(self.input.mouse_x, self.input.mouse_y, true);
                    }
                    InputEventType::PointerButtonRelease if primary => {
                        self.gate
                            .click(self.input.mouse_x, self.input.mouse_y, false);
                    }
                    InputEventType::PointerAxis if event.axis_id() == POINTER_AXIS_VERTICAL => {
                        self.gate.wheel(event.axis_value_v120());
                    }
                    _ => {}
                }
                if !matches!(
                    event.event_type,
                    InputEventType::KeyRelease | InputEventType::PointerButtonRelease
                ) {
                    continue;
                }
            }
            let taken = match event.event_type {
                InputEventType::KeyPress => {
                    self.consent.takes_press(&event)
                        || self.asking.takes_press(&event)
                        || self.popover.takes_press(&event)
                }
                InputEventType::KeyRelease => {
                    let keycode = event.key_keycode();
                    self.consent.takes_release(keycode)
                        | self.asking.takes_release(keycode)
                        | self.popover.takes_release(keycode)
                }
                _ => false,
            };
            if taken {
                let mods = event.key_modifiers();
                self.input.set_modifier_state(mods);
                if event.event_type == InputEventType::KeyRelease
                    && !self.consent.capturing()
                    && let Some(p) = proto_box.as_deref_mut()
                {
                    p.send_modifiers_for_task(self.input.focused_task(), mods);
                }
                continue;
            }
            if self.consent.active() {
                match event.event_type {
                    InputEventType::PointerMotion => {
                        self.input.apply_motion(&event);
                        self.consent.hover(self.input.mouse_x, self.input.mouse_y);
                        continue;
                    }
                    InputEventType::PointerButtonPress => {
                        if primary {
                            self.consent
                                .click(self.input.mouse_x, self.input.mouse_y, true);
                        }
                        continue;
                    }
                    InputEventType::PointerButtonRelease if primary && self.gate.shown() => {
                        self.consent.release_unseen();
                    }
                    InputEventType::PointerButtonRelease if primary => {
                        self.consent
                            .click(self.input.mouse_x, self.input.mouse_y, false);
                    }
                    InputEventType::PointerAxis => {
                        if event.axis_id() == POINTER_AXIS_VERTICAL {
                            self.consent.wheel(
                                self.input.mouse_x,
                                self.input.mouse_y,
                                event.axis_value_v120(),
                            );
                        }
                        continue;
                    }
                    _ => {}
                }
            } else if self.asking.active() {
                match event.event_type {
                    InputEventType::PointerMotion => {
                        self.input.apply_motion(&event);
                        self.asking.hover(self.input.mouse_x, self.input.mouse_y);
                        continue;
                    }
                    InputEventType::PointerButtonPress => {
                        if primary {
                            self.asking
                                .click(self.input.mouse_x, self.input.mouse_y, true);
                        }
                        continue;
                    }
                    InputEventType::PointerButtonRelease if primary => {
                        self.asking
                            .click(self.input.mouse_x, self.input.mouse_y, false);
                    }
                    InputEventType::PointerAxis => continue,
                    _ => {}
                }
            }
            match event.event_type {
                InputEventType::PointerMotion => {
                    self.input.apply_motion(&event);
                    self.popover.hover(self.input.mouse_x, self.input.mouse_y);
                    self.input.apply_grab_motion(proto_box.as_deref_mut());
                    self.sync_pointer_focus(proto_box.as_deref_mut());
                    let target = self.pointer_target(proto_box.as_deref());
                    if target != 0 {
                        if let Some(p) = proto_box.as_deref_mut() {
                            p.send_pointer_motion_for_task(
                                target,
                                time,
                                self.input.mouse_x,
                                self.input.mouse_y,
                            );
                        }
                    }
                }
                InputEventType::PointerButtonPress => {
                    let button = event.data.data0 as u8;
                    let popover_rect = self.popover.rect();
                    let screen_w = self.renderer.output_width;
                    let should_forward = self.input.on_button_press(
                        button,
                        fb_width,
                        fb_height,
                        &self.windows,
                        self.window_count,
                        &self.shelf,
                        popover_rect,
                        screen_w,
                        &self.system_bar,
                        proto_box.as_deref_mut(),
                    );
                    if let Some(request) = self.input.take_chrome_request() {
                        self.apply_chrome_request(request);
                    }
                    // Self-heals a grab left over from a release the kernel
                    // queue dropped under a motion flood, as the input layer's
                    // own drag and resize grabs do.
                    if self.input.mouse_buttons & !button == 0 {
                        self.release_pointer_grab();
                        self.sync_pointer_focus(proto_box.as_deref_mut());
                    }
                    if should_forward && self.protocol_pointer_focus != 0 {
                        if self.live_pointer_grab(proto_box.as_deref()) == 0 {
                            self.protocol_pointer_grab = self.protocol_pointer_focus;
                            self.protocol_pointer_grab_gen = proto_box
                                .as_deref()
                                .and_then(|p| {
                                    p.surface_generation_for_task(self.protocol_pointer_focus)
                                })
                                .unwrap_or(0);
                        }
                        if let Some(p) = proto_box.as_deref_mut() {
                            self.protocol_serial = self.protocol_serial.wrapping_add(1);
                            p.send_pointer_button_for_task(
                                self.protocol_pointer_grab,
                                self.protocol_serial,
                                time,
                                event.data.data0,
                                1,
                            );
                        }
                    }
                }
                InputEventType::PointerButtonRelease => {
                    let button = event.data.data0 as u8;
                    if primary && self.popover.rect().is_some() {
                        self.popover
                            .click(self.input.mouse_x, self.input.mouse_y, false);
                    }
                    self.input
                        .on_button_release(button, proto_box.as_deref_mut());
                    let pressed_on = self.live_pointer_grab(proto_box.as_deref());
                    if pressed_on != 0 {
                        if let Some(p) = proto_box.as_deref_mut() {
                            self.protocol_serial = self.protocol_serial.wrapping_add(1);
                            p.send_pointer_button_for_task(
                                pressed_on,
                                self.protocol_serial,
                                time,
                                event.data.data0,
                                0,
                            );
                        }
                    }
                    // The one authoritative mask, already updated by
                    // `on_button_release`.
                    if self.input.mouse_buttons == 0 && self.protocol_pointer_grab != 0 {
                        self.release_pointer_grab();
                        self.sync_pointer_focus(proto_box.as_deref_mut());
                    }
                }
                InputEventType::PointerAxis => {
                    let target = self.pointer_target(proto_box.as_deref());
                    if target != 0 {
                        if let Some(p) = proto_box.as_deref_mut() {
                            p.send_pointer_axis_for_task(
                                target,
                                time,
                                event.axis_id(),
                                event.axis_value_v120(),
                            );
                        }
                    }
                }
                InputEventType::KeyPress | InputEventType::KeyRelease => {
                    let pressed = event.event_type == InputEventType::KeyPress;
                    let mods = event.key_modifiers();
                    self.input.set_modifier_state(mods);
                    let kbd_focus = self.input.focused_task();
                    if let Some(p) = proto_box.as_deref_mut() {
                        p.forward_key_event(
                            kbd_focus,
                            &event,
                            pressed,
                            mods,
                            &mut self.protocol_serial,
                        );
                    }
                }
                _ => {}
            }
        }

        // Pointer focus can change without a motion event — a window moved,
        // appeared or vanished under a static cursor.
        self.sync_pointer_focus(proto_box.as_deref_mut());

        self.protocol = proto_box;
    }

    fn apply_chrome_request(&mut self, request: input::ChromeRequest) {
        match request {
            input::ChromeRequest::PopoverToggle(panel) => {
                if let Some(item) = self.system_bar.item_rect(self.renderer.output_width, panel) {
                    self.popover.toggle(panel, item);
                }
            }
            input::ChromeRequest::PopoverDismiss => self.popover.dismiss(),
            input::ChromeRequest::PopoverPress { x, y } => self.popover.click(x, y, true),
        }
    }

    /// Recompute pointer focus and emit protocol enter/leave events on
    /// transitions. Decorations and the desktop belong to the compositor, so a
    /// non-`Content` hit drops focus to none.
    /// Where a pointer event goes: the grab holder while one is held, the
    /// surface under the pointer otherwise.
    fn pointer_target(&mut self, proto: Option<&ProtocolBridge>) -> u32 {
        match self.live_pointer_grab(proto) {
            0 => self.protocol_pointer_focus,
            grab => grab,
        }
    }

    /// The grab holder, or 0 once the surface that took it is gone.
    ///
    /// A task id is a slot index and slots are recycled, so a client that died
    /// mid-drag — with no release ever arriving to clear the grab — would hand
    /// its pointer stream to whatever opened in its place. The generation the
    /// grab was taken with is what tells the successor apart.
    fn live_pointer_grab(&mut self, proto: Option<&ProtocolBridge>) -> u32 {
        if self.protocol_pointer_grab == 0 {
            return 0;
        }
        let live = proto
            .and_then(|p| p.surface_generation_for_task(self.protocol_pointer_grab))
            .is_some_and(|generation| generation == self.protocol_pointer_grab_gen);
        if !live {
            self.release_pointer_grab();
        }
        self.protocol_pointer_grab
    }

    fn release_pointer_grab(&mut self) {
        self.protocol_pointer_grab = 0;
        self.protocol_pointer_grab_gen = 0;
    }

    /// Whether the lock or login screen or a question is over every window,
    /// so the pointer reaches none.
    fn covered(&self) -> bool {
        self.gate.shown() || self.consent.active() || self.asking.active()
    }

    fn sync_pointer_focus(&mut self, proto: Option<&mut ProtocolBridge>) {
        // Leaving the window mid-drag is the ordinary way to drag, not a
        // reason to hand the pointer to the window underneath.
        if self.live_pointer_grab(proto.as_deref()) != 0 {
            return;
        }
        let hit = self.input.resolve_cursor_hit(
            &self.windows,
            self.window_count,
            &self.shelf,
            self.popover.rect(),
            self.renderer.output_width,
            &self.system_bar,
        );
        let new_ptr_focus = match hit.part {
            input::CursorPart::Content if !self.covered() => hit.task_id,
            _ => 0,
        };
        if new_ptr_focus == self.protocol_pointer_focus {
            return;
        }
        if let Some(p) = proto {
            if self.protocol_pointer_focus != 0 {
                self.protocol_serial = self.protocol_serial.wrapping_add(1);
                p.send_pointer_leave_for_task(self.protocol_pointer_focus, self.protocol_serial);
            }
            if new_ptr_focus != 0 {
                // Serial 0 is the "never entered" sentinel the cursor gate rejects.
                self.protocol_serial = self.protocol_serial.wrapping_add(1).max(1);
                p.send_pointer_enter_for_task(
                    new_ptr_focus,
                    self.protocol_serial,
                    self.input.mouse_x,
                    self.input.mouse_y,
                );
            }
        }
        self.protocol_pointer_focus = new_ptr_focus;
    }

    fn resync_windows_post_input(&mut self) {
        let pre_count = self.window_count as usize;
        let mut pre_windows = [UserWindowInfo::default(); MAX_WINDOWS];
        let mut pre_bounds = [WindowBounds::default(); MAX_WINDOWS];
        for i in 0..pre_count {
            pre_windows[i] = self.windows[i];
            pre_bounds[i] = WindowBounds::from_window(&self.windows[i]);
        }

        let raw_count = if let Some(ref proto) = self.protocol {
            proto.get_windows(&mut self.windows) as i64
        } else {
            return;
        };
        self.window_count = if raw_count > 0 {
            (raw_count as usize).min(MAX_WINDOWS) as u32
        } else {
            0
        };

        for i in 0..self.window_count as usize {
            let curr_bounds = WindowBounds::from_window(&self.windows[i]);
            let task_id = self.windows[i].task_id;
            let prev_idx = pre_windows[..pre_count]
                .iter()
                .position(|w| w.task_id == task_id);
            if let Some(j) = prev_idx {
                let p = pre_bounds[j];
                let geometry_changed = p != curr_bounds;
                // A raise/lower happens during input processing, so a restack
                // is only visible here, in the post-input re-fetch.
                let restacked = j != i;
                if geometry_changed || restacked {
                    self.add_bounds_damage(&p);
                    self.add_bounds_damage(&curr_bounds);
                }
            }
            self.prev_window_bounds[i] = curr_bounds;
        }
    }

    fn add_window_damage(&mut self, window: &UserWindowInfo) {
        if window.damage_count == u8::MAX {
            let bounds = WindowBounds::from_window(window);
            self.add_bounds_damage(&bounds);
            return;
        }

        for i in 0..window.damage_count as usize {
            let region = &window.damage_regions[i];
            if region.is_valid() {
                self.output_damage.add_rect(
                    window.x + region.x0,
                    window.y + region.y0,
                    window.x + region.x1,
                    window.y + region.y1,
                );
            }
        }
    }

    fn add_cursor_damage_at(&mut self, x: i32, y: i32) {
        // Conservative rect covering ALL cursor shapes:
        // default arrow (12×17 at x,y) and resize arrows (up to 17×17 centered).
        self.output_damage.add_rect(x - 9, y - 9, x + 12, y + 17);
    }

    /// What a change of focus repaints of the window `task_id`: its title
    /// bar and its shadow, which darkens with focus, and the corners of its
    /// content, which blend over the shadow.
    fn add_focus_damage_for_task(&mut self, task_id: u32) {
        let Some(w) = self.windows[..self.window_count as usize]
            .iter()
            .find(|w| task_id != 0 && w.task_id == task_id)
        else {
            return;
        };
        let interior = DamageRect {
            x0: w.x,
            y0: w.y,
            x1: w.x + w.effective_width() as i32 - 1,
            y1: w.y + w.effective_height() as i32 - 1 - WINDOW_CORNER_RADIUS,
        };
        let mut changed = Region::from_rect(frame::bounds(w));
        changed.subtract(&interior);
        for rect in changed.rects() {
            self.output_damage.add(*rect);
        }
    }

    fn needs_redraw(&self) -> bool {
        self.first_frame
            || self.input.needs_full_redraw
            || self.force_full_redraw
            || !self.output_damage.is_empty()
    }

    /// Whether the question changed since the last frame, laid out on the
    /// screen over the window of the app that asks, when it has one shown.
    fn consent_dirty(&mut self, fb_info: &DisplayInfo) -> bool {
        let screen = screen_rect(fb_info);
        let windows = &self.windows[..self.window_count as usize];
        let anchor = self.consent.asker().and_then(|pid| {
            let w = windows
                .iter()
                .find(|w| w.task_id == pid && w.state != WINDOW_STATE_MINIMIZED)?;
            let frame = frame::frame_box(w);
            Some(slopos_appkit::Rect::new(
                frame.x0,
                frame.y0,
                frame.x1 - frame.x0 + 1,
                frame.y1 - frame.y0 + 1,
            ))
        });
        self.consent.take_dirty(screen, anchor)
    }

    /// The synchronous per-frame work the timer arm invokes: gather input,
    /// refresh window state, render+present if dirty, emit frame_done, reap
    /// disconnected clients and flush. Accept and pacing live in the driver.
    fn render_frame(
        &mut self,
        output: &mut CompositorOutput,
        fb_info: &DisplayInfo,
        pixel_format: slopos_abi::PixelFormat,
        frame_count: &mut u32,
        metrics: &mut FrameMetrics,
        time_origin: Instant,
        frame_start: Instant,
        target_frame_ms: u64,
    ) {
        let wm = self;
        wm.input.drain_events();
        wm.refresh_windows();
        wm.gate.poll();
        if wm.consent_dirty(fb_info) | wm.gate.take_dirty(screen_rect(fb_info)) {
            wm.force_full_redraw = true;
        }
        // Snapshot before any input processing so a change from any source —
        // sync, click, shelf, spawn — is one comparison afterwards.
        let focus_before = wm.input.focused_task();

        wm.input.sync_keyboard_focus(
            &wm.windows,
            wm.window_count,
            &wm.prev_windows,
            wm.prev_window_count,
        );
        wm.input
            .process_pending_close_requests(&wm.windows, wm.window_count);
        wm.process_input_events(fb_info.width as i32, fb_info.height as i32);
        match wm.popover.take_choice() {
            Some(Choice::Lock) => wm.gate.lock(),
            Some(Choice::SwitchUser) => ask_init(Command::Greeter),
            Some(choice) => {
                if let Some(ending) = choice.ending() {
                    wm.asking.ask(ending);
                }
            }
            None => {}
        }
        if let Some(ending) = wm.asking.take_answer() {
            carry_out(ending, wm.undone_tx.clone());
        }
        if !wm.asking.active()
            && !wm.gate.shown()
            && let Ok(ending) = wm.undone.try_recv()
        {
            wm.asking.failed(ending);
        }
        if wm.gate.shown() {
            wm.popover.dismiss();
            wm.asking.withdraw();
        }
        if wm.consent_dirty(fb_info) | wm.gate.take_dirty(screen_rect(fb_info)) {
            wm.force_full_redraw = true;
        }

        wm.add_pointer_damage();

        // Resolved once, against this window snapshot, before the post-input
        // resync can reorder the array.
        let cursor_hit = wm.input.resolve_cursor_hit(
            &wm.windows,
            wm.window_count,
            &wm.shelf,
            wm.popover.rect(),
            wm.renderer.output_width,
            &wm.system_bar,
        );
        let (cursor_shape, signal_hovered_task) = match wm.covered() {
            true => (slopos_abi::window::CURSOR_SHAPE_DEFAULT, 0),
            false => (
                wm.input.cursor_shape_for(&cursor_hit, &wm.windows),
                wm.input.signal_hovered_task(&cursor_hit, &wm.windows),
            ),
        };

        if cursor_shape != wm.prev_cursor_shape {
            wm.add_cursor_damage_at(wm.input.mouse_x, wm.input.mouse_y);
            wm.prev_cursor_shape = cursor_shape;
        }

        let hw_cursor = wm.update_hw_cursor(cursor_shape);
        if hw_cursor
            && (wm.input.mouse_x != wm.hw_cursor_last_x || wm.input.mouse_y != wm.hw_cursor_last_y)
        {
            let _ = window::cursor_move(
                wm.input.mouse_x.max(0) as u32,
                wm.input.mouse_y.max(0) as u32,
            );
            wm.hw_cursor_last_x = wm.input.mouse_x;
            wm.hw_cursor_last_y = wm.input.mouse_y;
        }

        let focus_after = wm.input.focused_task();
        if focus_after != focus_before {
            wm.add_focus_damage_for_task(focus_before);
            wm.add_focus_damage_for_task(focus_after);
            // The bar names the focused application and nothing else in the
            // frame touches the bar strip, so its name would otherwise stay
            // stale until an unrelated change repainted it.
            let name_rect = wm.system_bar.app_name_damage(output.width);
            wm.output_damage
                .add_rect(name_rect.x0, name_rect.y0, name_rect.x1, name_rect.y1);
        }

        wm.resync_windows_post_input();

        if !wm.net_event_driven
            && wm
                .net_last_poll
                .is_none_or(|last| last.elapsed().as_millis() >= NET_POLL_INTERVAL_MS)
        {
            wm.net_last_poll = Some(Instant::now());
            wm.refresh_from_kernel();
        }

        let undisturbed =
            !wm.gate.shown() && !wm.behind && !wm.consent.active() && !wm.asking.active();
        if undisturbed
            && std::mem::take(&mut wm.restart_offer_due)
            && let Some(item) = wm
                .system_bar
                .item_rect(output.width, popover::Panel::System)
        {
            wm.popover.offer_restart(item);
        }
        // Before the panel is laid out, so a switch request that timed out
        // this frame reverts in the same frame rather than one later.
        wm.popover.settle(&wm.net_cache);
        wm.popover
            .refresh(&wm.net_cache, output.width, output.height);
        if wm.asking.take_dirty(screen_rect(fb_info)) {
            wm.force_full_redraw = true;
        }

        let mut popover_damage = [DamageRect::invalid(); 2];
        let popover_damage_count = wm.popover.take_damage(&mut popover_damage);
        for rect in &popover_damage[..popover_damage_count] {
            wm.output_damage
                .add_rect(rect.x0, rect.y0, rect.x1, rect.y1);
        }

        let now = sys_core::realtime_secs().and_then(WallTime::from_unix);
        let mut bar_damage = [DamageRect::invalid(); menu_bar::MAX_BAR_DAMAGE];
        let bar_damage_count =
            wm.system_bar
                .take_damage(output.width, now.as_ref(), &mut bar_damage);
        for rect in &bar_damage[..bar_damage_count] {
            wm.output_damage
                .add_rect(rect.x0, rect.y0, rect.x1, rect.y1);
        }

        if wm.needs_redraw() {
            let force_full = wm.first_frame || wm.input.needs_full_redraw || wm.force_full_redraw;

            let frame_damage = if force_full {
                Region::full(output.width, output.height)
            } else {
                wm.output_damage.clone()
            };
            let mode = if force_full {
                RenderMode::Full
            } else {
                RenderMode::Partial
            };

            // A superset of the painted region: never fewer pixels than were
            // repainted, so the scanout copy covers everything that changed.
            let (damage_arr, damage_n) = frame_damage.to_bounded::<MAX_OUTPUT_DAMAGE_REGIONS>();

            if let Some(mut buf) = output.draw_buffer() {
                buf.set_pixel_format(pixel_format);

                let active_app_name = active_app_name(
                    &wm.windows,
                    wm.window_count,
                    wm.input.focused_task(),
                    &wm.shelf,
                );

                wm.renderer.render(
                    &mut buf,
                    &wm.windows,
                    wm.window_count as usize,
                    wm.input.focused_task(),
                    signal_hovered_task,
                    wm.input.mouse_x,
                    wm.input.mouse_y,
                    cursor_shape,
                    &mut wm.surface_cache,
                    &wm.system_bar,
                    &mut wm.shelf,
                    &wm.popover,
                    &wm.asking,
                    &wm.consent,
                    &wm.gate,
                    &active_app_name,
                    &frame_damage,
                    hw_cursor,
                );
            }

            // Empty damage means a full flip.
            let damage_slice: &[DamageRect] = if force_full {
                &[]
            } else {
                &damage_arr[..damage_n]
            };

            let flip_result = output.present(damage_slice);
            if *frame_count < 3 {
                if flip_result {
                    tty::write(b"COMPOSITOR: fb_flip ok\n");
                } else {
                    tty::write(b"COMPOSITOR: fb_flip FAILED\n");
                }
            }
            *frame_count = frame_count.saturating_add(1);
            if flip_result {
                let present_time = time_origin.elapsed().as_millis() as u64;

                let presented = core::mem::take(&mut wm.frame_dirty_surfaces);
                if let Some(ref mut proto) = wm.protocol {
                    proto.mark_frames_done(present_time);
                    proto.clear_presented(&presented);
                }
            } else {
                if force_full {
                    wm.pending_full = true;
                } else {
                    wm.pending_damage.push_region(&frame_damage);
                }
            }

            let frame_time = frame_start.elapsed().as_millis() as u64;
            let copied = estimate_present_bytes(
                output.width,
                output.height,
                output.bytes_pp,
                output.pitch,
                mode,
                damage_slice,
            );
            metrics.record(mode, copied, frame_time, target_frame_ms, flip_result);

            wm.input.needs_full_redraw = false;
            wm.force_full_redraw = false;
            wm.first_frame = false;
        }

        if let Some((frames, bytes)) =
            metrics.take_window(time_origin.elapsed().as_millis() as u64, METRICS_REPORT_MS)
        {
            let mut line = std::string::String::new();
            use core::fmt::Write;
            let _ = write!(line, "COMPOSITOR: frames={frames} bytes={bytes}\n");
            tty::write(line.as_bytes());
        }

        // `process_requests()` is load-bearing, not an optimisation:
        // `cleanup_disconnected`'s close-probe recv empties the kernel-side
        // socket, so the per-client readiness stream never fires again for
        // bytes it pulled into the read buffer. Without an every-frame parse
        // those bytes are never handled.
        if let Some(ref mut proto) = wm.protocol {
            proto.process_requests();
            proto.cleanup_disconnected();
            proto.flush_all();
        }

        // Dock and shelf launches are fire-and-forget — `spawn_program` keeps
        // no tid — and the compositor outlives them, so it reaps them all.
        process::reap_exited_children();
    }
}

impl WindowManager {
    fn told(&mut self, line: &[u8]) {
        match line {
            b"front" | b"front unlock" => {
                self.gate.in_front();
                if line == b"front unlock" {
                    self.gate.unlocked();
                }
                self.behind = false;
                self.force_full_redraw = true;
                self.first_frame = true;
            }
            b"behind" => {
                self.gate.lock();
                self.popover.dismiss();
                self.asking.withdraw();
                self.behind = true;
            }
            b"lock" => self.gate.lock(),
            _ => {}
        }
    }

    /// Act on what init said since the last pass, before the pass takes any
    /// input: a lock init sent before handing the seats back is then in
    /// force for every key typed after.
    fn hear_init(&mut self) {
        let Some(says) = self.init_says.as_mut() else {
            return;
        };
        let (lines, closed) = says.read();
        for line in lines {
            self.told(&line);
        }
        if closed {
            tty::write(b"COMPOSITOR: init's session pipe closed\n");
            self.init_says = None;
        }
    }

    /// What a compositor behind still does each pass: serve and reap its
    /// clients, whose frames wait until it is in front again.
    fn look_after_clients(&mut self) {
        self.gate.poll();
        if let Some(ref mut proto) = self.protocol {
            proto.process_requests();
            proto.cleanup_disconnected();
            proto.flush_all();
        }
        process::reap_exited_children();
    }
}

/// How long a seat may stay taken: a compositor that just exited holds its
/// seats until its exit's cleanup hooks run, which is after init is woken to
/// start the next.
const SEAT_WAIT_MS: u64 = 5000;

fn acquire_seat(acquire: fn(u32) -> i64) -> bool {
    let deadline = sys_core::get_time_ms() + SEAT_WAIT_MS;
    loop {
        let fd = acquire(window::SEAT_COMPOSITOR_PRIMARY);
        if fd >= 0 {
            return true;
        }
        if fd != i64::from(slopos_abi::Errno::EBUSY.raw()) || sys_core::get_time_ms() >= deadline {
            return false;
        }
        sys_core::sleep_ms(10);
    }
}

pub fn compositor_user_main() {
    // Before any descriptor is opened: one could land at the number of an
    // inherited one the compositor was started without.
    let readiness = crate::readiness::ReadinessNotifier::acquire();
    let consent = consent::Consent::acquire();
    let session = session_pipe();
    let role = match std::env::args().nth(1).as_deref() {
        Some(GREETER_ARG) => gate::Role::Greeter,
        _ => gate::Role::Desktop {
            name: std::env::var("USER").unwrap_or_default(),
        },
    };
    tty::write(b"COMPOSITOR: starting\n");

    // Ownership of the screen and the input stream is announced here, once,
    // rather than re-asserted by every frame. Both fds are deliberately leaked
    // for the process lifetime: closing one would not release the seat anyway
    // (release is arbiter revocation at task exit), and holding them keeps the
    // descriptor's existence honest about what this process owns.
    // A session of init's takes the seats only as init hands them over,
    // once it is the session in front.
    if session.is_none() {
        if !acquire_seat(window::screen_acquire) {
            tty::write(b"COMPOSITOR: screen seat unavailable\n");
        }
        if !acquire_seat(window::input_sink_acquire) {
            tty::write(b"COMPOSITOR: input seat unavailable\n");
        }
    }

    let mut wm = WindowManager::new(consent, gate::Gate::new(role));
    let mut fb_info = DisplayInfo::default();

    if window::fb_info(&mut fb_info) < 0 {
        tty::write(b"COMPOSITOR: fb_info failed\n");
        loop {
            sys_core::yield_now();
        }
    }
    tty::write(b"COMPOSITOR: fb_info ok\n");

    let output = match CompositorOutput::new(&fb_info) {
        Some(out) => out,
        None => {
            tty::write(b"COMPOSITOR: output alloc failed\n");
            loop {
                sys_core::yield_now();
            }
        }
    };
    tty::write(b"COMPOSITOR: output allocated\n");

    wm.renderer
        .set_output_info(output.width, output.height, output.bytes_pp, output.pitch);

    if let Some(ref mut proto) = wm.protocol {
        proto.set_display_info(
            output.width,
            output.height,
            fb_info.format as u32,
            output.pitch as u32,
        );
        proto.set_work_area(work_area(output.width, output.height));
    }

    let pixel_format = fb_info.format;

    // Ring sized for the peak armed-row count: one accept-readiness stream,
    // one per connected client (MAX_CLIENTS), and the frame timer.
    let ring = Ring::setup(64).expect("COMPOSITOR: ring setup failed");
    slopfut::block_on(
        ring,
        compositor_async(wm, output, fb_info, pixel_format, readiness, session),
    );
}

const GREETER_ARG: &str = "--greeter";
/// Where init tells a compositor whether its session is in front.
const SESSION_FD: i32 = 5;
/// How often a compositor behind looks after its clients.
const BEHIND_FRAME_MS: u64 = 100;

/// The pipe init hands over at [`SESSION_FD`], when it is one.
fn session_pipe() -> Option<crate::syscall::OwnedFd> {
    use slopos_abi::syscall::F_GETFD;
    use slopos_slibc::pal::{Pal, Sys};
    Sys::fcntl(SESSION_FD, F_GETFD as i32, 0).ok()?;
    let _ = crate::syscall::fs::set_fd_cloexec(SESSION_FD);
    // SAFETY: init handed this descriptor over and nothing else here owns it.
    let fd = unsafe { crate::syscall::OwnedFd::from_raw(SESSION_FD) };
    let _ = crate::syscall::net::set_nonblocking(fd.raw());
    Some(fd)
}

/// Whether the person this desktop runs as may turn networking on and off
/// unasked, so its switch is worth offering.
fn net_toggle_unasked() -> bool {
    use slopos_priv_core::{People, Person, Reach, Requester, Verdict};
    let Ok(policy) = crate::privd::policy() else {
        return false;
    };
    let Some(action) = policy.action("org.slopos.net.toggle") else {
        return false;
    };
    let uid = process::getuid();
    let mut groups = [0u32; 32];
    // SAFETY: `groups` holds as many as the call is told it may write.
    let n = unsafe { slopos_slibc::process::ids::getgroups(32, groups.as_mut_ptr()) };
    let admin = groups[..n.max(0) as usize].contains(&slopos_abi::fs::ADMIN_GID);
    let people = People {
        people: vec![Person {
            uid,
            name: std::env::var("USER").unwrap_or_default(),
            admin,
            password: true,
        }],
        logged_in: vec![uid],
    };
    let requester = Requester::new(uid, Some(crate::init_control::COMPOSITOR), false)
        .reached(Reach::Screen { theirs: true });
    action.admits(&requester, &people) == Verdict::Run
}

fn screen_rect(fb_info: &DisplayInfo) -> slopos_appkit::Rect {
    slopos_appkit::Rect::new(0, 0, fb_info.width as i32, fb_info.height as i32)
}

/// How an ending the person went ahead with did not happen.
enum Undone {
    /// The person answered privd's question so.
    Refused,
    Failed(String),
}

/// Carry `ending` out on a thread of its own: init logs out or restarts the
/// desktop, privd restarts or powers off the computer. `undone` hears of
/// one that did not happen but for the person's own no; why goes to the
/// log.
fn carry_out(ending: Ending, undone: std::sync::mpsc::Sender<Ending>) {
    let report = undone.clone();
    let spawned = std::thread::Builder::new()
        .name("compositor-ending".into())
        .spawn(move || {
            let done = match ending {
                Ending::RestartDesktop => init(Command::RestartSession),
                Ending::LogOut => init(Command::EndSession),
                Ending::Restart => privd("org.slopos.power.reboot", "reboot"),
                Ending::PowerOff => privd("org.slopos.power.poweroff", "poweroff"),
            };
            if let Err(Undone::Failed(why)) = done {
                eprintln!("compositor: {}: {why}", ending.verb());
                let _ = undone.send(ending);
            }
        });
    if let Err(why) = spawned {
        eprintln!("compositor: {}: {why}", ending.verb());
        let _ = report.send(ending);
    }
}

fn init(command: Command) -> Result<(), Undone> {
    crate::init_control::ask(command).map_err(Undone::Failed)
}

/// privd's `action` with `arg`, once it has run or refused.
fn privd(action: &str, arg: &str) -> Result<(), Undone> {
    let null = crate::syscall::fs::open_cstr(c"/dev/null", slopos_abi::fs::O_RDWR)
        .map_err(|e| Undone::Failed(format!("/dev/null: {e}")))?;
    match crate::privd::run_feeding(action, &[arg], b"", null.raw(), 2) {
        Ok(0) => Ok(()),
        Ok(code) => Err(Undone::Failed(format!("{action} exited {code}"))),
        Err(not) if not.by_person => Err(Undone::Refused),
        Err(not) => Err(Undone::Failed(format!("{action}: {not}"))),
    }
}

/// Ask init on a thread of its own, which init answers once it has acted.
pub(super) fn ask_init(command: Command) {
    let _ = std::thread::Builder::new()
        .name("compositor-init".into())
        .spawn(move || {
            if let Err(why) = crate::init_control::ask(command) {
                eprintln!("compositor: init: {why}");
            }
        });
}

const TARGET_FRAME_MS: u64 = 16;

/// Network poll cadence for the fallback path only; with a monitor fd this
/// never runs. Free at 60 Hz, and fast enough to reflect a pulled cable.
const NET_POLL_INTERVAL_MS: u128 = 2000;

/// Frame-cost report cadence. Off unless `SLOPOS_COMPOSITOR_METRICS` is set,
/// because the report goes to the TTY over whatever the user is doing.
const METRICS_REPORT_MS: u64 = 5000;

/// Whether to print the periodic damage report. Read once by the caller — the
/// environment does not change under a running compositor.
fn metrics_reporting_enabled() -> bool {
    std::env::var("SLOPOS_COMPOSITOR_METRICS").is_ok_and(|v| v != "0")
}

/// Maximum rect count handed to the kernel flip per frame. Must match the
/// kernel's per-flip damage capacity: it drops rects beyond that, which would
/// leave painted pixels un-scanned-out.
const MAX_OUTPUT_DAMAGE_REGIONS: usize = slopos_abi::damage::MAX_DAMAGE_REGIONS;

/// Maximum new connections drained from the listen backlog in one accept wake.
const ACCEPT_BATCH: usize = MAX_CLIENTS;

/// Async compositor driver: an accept task, a task per client, and this root
/// future as the frame-timer arm, all on the single-threaded `!Send` executor.
/// Sharing state through `Rc<RefCell<…>>` is sound because no borrow is held
/// across an `.await`.
///
/// The accept task waits on listen-socket readiness rather than the ring's
/// `accept_multishot`, so `Server::accept()` stays the sole client-slot
/// allocator and fds are never accepted out from under it. It is woken
/// independently of the frame timer, so a new client is never gated on a frame.
async fn compositor_async(
    wm: WindowManager,
    mut output: CompositorOutput,
    fb_info: DisplayInfo,
    pixel_format: slopos_abi::PixelFormat,
    mut readiness: Option<crate::readiness::ReadinessNotifier>,
    session: Option<crate::syscall::OwnedFd>,
) {
    use std::cell::RefCell;
    use std::rc::Rc;

    let wm = Rc::new(RefCell::new(wm));

    // Accept pass and request drain before signalling readiness, so a
    // connection queued during init is greeted before init is told the
    // compositor is up.
    let listen_fd = {
        let mut w = wm.borrow_mut();
        let listen_fd = w.protocol.as_ref().map(|p| p.listen_fd());
        if let Some(ref mut proto) = w.protocol {
            let mut accepted = [(0usize, -1i32, 0u64); ACCEPT_BATCH];
            let n = proto.accept_and_collect(&mut accepted);
            proto.process_requests();
            drop(w);
            for &(idx, fd, generation) in &accepted[..n] {
                spawn_client_task(wm.clone(), idx, fd, generation);
            }
        } else {
            drop(w);
        }
        listen_fd
    };

    // Boot-critical: init blocks on this byte before launching the shell.
    if let Some(n) = readiness.take() {
        n.signal_ready();
    }

    {
        let event_driven = spawn_net_monitor_task(wm.clone());
        wm.borrow_mut().net_event_driven = event_driven;
    }
    spawn_consent_task(wm.clone());
    spawn_base_task(wm.clone());
    wm.borrow_mut().init_says = session.map(InitSays::new);

    // A multishot row can die on a transient error and the listen socket
    // outlives any such transient, so stream-end re-arms rather than ending
    // all future accepts.
    if let Some(listen_fd) = listen_fd {
        let wm_accept = wm.clone();
        slopfut::spawn(async move {
            loop {
                let mut stream = slopfut::poll_add_multishot(listen_fd, POLLIN);
                while stream.next().await.is_some() {
                    let mut accepted = [(0usize, -1i32, 0u64); ACCEPT_BATCH];
                    let n = {
                        let mut w = wm_accept.borrow_mut();
                        match w.protocol {
                            Some(ref mut proto) => proto.accept_and_collect(&mut accepted),
                            None => 0,
                        }
                    };
                    for &(idx, fd, generation) in &accepted[..n] {
                        spawn_client_task(wm_accept.clone(), idx, fd, generation);
                    }
                }
                tty::write(b"COMPOSITOR: accept stream ended; re-arming\n");
                // Paced so a persistently failing arm cannot become a hot loop;
                // the per-frame sweep below covers the gap meanwhile.
                slopfut::time::sleep_ms(50).await;
            }
        });
    }

    // The per-frame work runs synchronously under a `borrow_mut` dropped
    // before the next timer await, so no other task sees an overlapping borrow.
    let mut frame_count: u32 = 0;
    let mut metrics = FrameMetrics::new(metrics_reporting_enabled());
    let time_origin = Instant::now();
    loop {
        let frame_start = Instant::now();

        // Covers the accept task while its readiness stream is between
        // re-arms. `Server::accept()` stays the sole slot allocator, so the
        // two paths cannot double-accept a connection.
        {
            let mut accepted = [(0usize, -1i32, 0u64); ACCEPT_BATCH];
            let n = {
                let mut w = wm.borrow_mut();
                match w.protocol {
                    Some(ref mut proto) => proto.accept_and_collect(&mut accepted),
                    None => 0,
                }
            };
            for &(idx, fd, generation) in &accepted[..n] {
                spawn_client_task(wm.clone(), idx, fd, generation);
            }
        }

        wm.borrow_mut().hear_init();
        let behind = wm.borrow().behind;
        if behind {
            wm.borrow_mut().look_after_clients();
        } else {
            wm.borrow_mut().render_frame(
                &mut output,
                &fb_info,
                pixel_format,
                &mut frame_count,
                &mut metrics,
                time_origin,
                frame_start,
                TARGET_FRAME_MS,
            );
        }

        let frame_ms = if behind {
            BEHIND_FRAME_MS
        } else {
            TARGET_FRAME_MS
        };
        let elapsed = frame_start.elapsed().as_millis() as u64;
        let remaining = frame_ms.saturating_sub(elapsed);
        if remaining > 0 {
            slopfut::time::sleep_ms(remaining).await;
        } else {
            slopfut::yield_now().await;
        }
    }
}

/// Drive the network indicator from a `net_monitor` fd instead of a timer.
///
/// The fd is a change *notification* only: every wake re-reads the whole model
/// through `net_query` rather than applying the event payload as a delta, which
/// is what makes a dropped event harmless.
///
/// The monitor is opened before the first query, so an interface appearing
/// between the two shows up as an event rather than being missed by both.
///
/// Returns whether the fd was opened; a failure leaves the caller on the timer.
fn spawn_net_monitor_task(wm: std::rc::Rc<std::cell::RefCell<WindowManager>>) -> bool {
    let fd = match crate::syscall::net::net_monitor(NET_MON_DEFAULT, 0) {
        Ok(fd) => fd,
        Err(_) => {
            tty::write(b"COMPOSITOR: net_monitor unavailable; polling instead\n");
            return false;
        }
    };
    // Handed to the task rather than closed here; it lives until exit.
    let raw = fd.into_raw();

    wm.borrow_mut().refresh_from_kernel();

    slopfut::spawn(async move {
        let mut buf = [0u8; NET_EVENT_LEN * 16];
        loop {
            let mut stream = slopfut::poll_add_multishot(raw, POLLIN);
            while stream.next().await.is_some() {
                // Drain to empty, or POLLIN stays asserted and the stream
                // spins. An empty netmon read is `EAGAIN`, never a block.
                while crate::syscall::fs::read_slice(raw, &mut buf).unwrap_or(0) > 0 {}
                wm.borrow_mut().refresh_from_kernel();
            }
            // The fd outlives a multishot row that died on a transient error,
            // so re-arm. Logged because there is no polling backstop here.
            tty::write(b"COMPOSITOR: net_monitor stream ended; re-arming\n");
            slopfut::time::sleep_ms(500).await;
        }
    });
    true
}

/// Read privd's questions as they arrive, once privd has been told the
/// compositor can draw them. A oneshot poll each time: a multishot fires only
/// when readiness changes, so a frame landing between a read and the ring's
/// next pass would wake nothing, and privd answers a `READY` at once.
fn spawn_consent_task(wm: std::rc::Rc<std::cell::RefCell<WindowManager>>) {
    let fd = {
        let mut w = wm.borrow_mut();
        w.consent.announce();
        w.consent.fd()
    };
    let Some(fd) = fd else {
        return;
    };
    slopfut::spawn(async move {
        while wm.borrow().consent.fd().is_some() {
            if slopfut::poll_add(fd, POLLIN).await < 0 {
                slopfut::time::sleep_ms(500).await;
            } else if !wm.borrow_mut().consent.read() {
                break;
            }
        }
        tty::write(b"COMPOSITOR: privd's consent channel closed\n");
    });
}

/// What init says on the session pipe, whether this session is in front:
/// `front`, `front unlock` once its person proved themselves at the login
/// screen, `behind`, which locks a desktop, and `lock`.
struct InitSays {
    pipe: crate::syscall::OwnedFd,
    said: Vec<u8>,
}

impl InitSays {
    fn new(pipe: crate::syscall::OwnedFd) -> InitSays {
        InitSays {
            pipe,
            said: Vec::new(),
        }
    }

    /// The whole lines written since the last read, and whether init has
    /// closed its end.
    fn read(&mut self) -> (Vec<Vec<u8>>, bool) {
        let mut buf = [0u8; 256];
        let closed = loop {
            match crate::syscall::fs::read_slice(self.pipe.raw(), &mut buf) {
                Ok(0) => break true,
                Ok(n) => self.said.extend_from_slice(&buf[..n]),
                Err(_) => break false,
            }
        };
        let mut lines = Vec::new();
        while let Some(end) = self.said.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.said.drain(..=end).collect();
            lines.push(line[..end].to_vec());
        }
        (lines, closed)
    }
}

/// Hear a new base come in, and offer a session restart while it holds
/// another compositor or terminal than the session started from, which run
/// until then. A oneshot poll each time, as [`spawn_consent_task`]'s is.
fn spawn_base_task(wm: std::rc::Rc<std::cell::RefCell<WindowManager>>) {
    use crate::init_control::{COMPOSITOR, TERMINAL};
    let Some(mut watch) = crate::base_watch::BaseWatch::open() else {
        return;
    };
    let started = watch.manifest().to_owned();
    slopfut::spawn(async move {
        loop {
            if !watch.switched() && slopfut::poll_add(watch.fd(), POLLIN).await < 0 {
                slopfut::time::sleep_ms(500).await;
                continue;
            }
            let Some(switch) = watch.take() else {
                slopfut::time::sleep_ms(1000).await;
                continue;
            };
            let wanted =
                slopos_boot_core::base::changed(&started, &switch.new, &[COMPOSITOR, TERMINAL]);
            let mut w = wm.borrow_mut();
            if w.popover.want_restart(wanted) {
                w.restart_offer_due = true;
            }
        }
    });
}

/// Spawn a per-client task that drains `idx`'s requests on each readiness of
/// `fd` until it disconnects, then exits — dropping its stream, which
/// `OP_CANCEL`s the armed ring row.
fn spawn_client_task(
    wm: std::rc::Rc<std::cell::RefCell<WindowManager>>,
    idx: usize,
    fd: i32,
    generation: u64,
) {
    if fd < 0 {
        return;
    }
    // Both the slot index and the fd number are recycled across
    // disconnect→reconnect, so a successor client can inherit this exact
    // slot+fd; only the monotonic generation distinguishes it.
    let owns = move |proto: &ProtocolBridge| {
        proto.client_fd(idx) == Some(fd) && proto.client_gen(idx) == Some(generation)
    };
    let hangup = match wm.borrow().protocol {
        Some(ref proto) if owns(proto) => proto.hangup(idx),
        _ => None,
    };
    let Some(hangup) = hangup else {
        return;
    };
    slopfut::spawn(async move {
        let mut stream = slopfut::poll_add_multishot(fd, POLLIN);
        loop {
            let readiness = match slopfut::select2(stream.next(), hangup.notified()).await {
                slopfut::Either2::A(readiness) => readiness,
                slopfut::Either2::B(()) => break,
            };
            match readiness {
                Some(_revents) => {
                    let still_connected = {
                        let mut w = wm.borrow_mut();
                        match w.protocol {
                            Some(ref mut proto) if owns(proto) => proto.process_client(idx),
                            _ => false,
                        }
                    };
                    if !still_connected {
                        break;
                    }
                }
                None => {
                    // The kernel coalesces POLLHUP with a final POLLIN on peer
                    // close and the terminal CQE surfaces as `None` without
                    // yielding that data edge, so drain once before teardown
                    // to recover a trailing request burst.
                    let mut w = wm.borrow_mut();
                    if let Some(ref mut proto) = w.protocol {
                        if owns(proto) {
                            // A false return means the teardown funnel already
                            // ran, so only disconnect if it drained clean.
                            if proto.process_client(idx) {
                                proto.disconnect_client(idx);
                            }
                        }
                    }
                    break;
                }
            }
        }
        // `stream` drops here → its armed ring row is cancelled (`OP_CANCEL`).
    });
}

/// Read the whole network model out of `net_query`: interfaces, addresses and
/// the one global record, joined on `ifindex`. The resolver is not queried —
/// nothing the bar or panel draws reads the nameserver list, so it stays empty.
///
/// `None` if any query failed, which the caller reads as "keep what you had".
fn read_net_model() -> Option<NetPanelModel> {
    let ifaces = net_query::fetch::<UserIface>(NET_Q_IFACES, NET_IFINDEX_NONE).ok()?;
    let addrs = net_query::fetch::<UserAddr>(NET_Q_ADDRS, NET_IFINDEX_NONE).ok()?;
    let global = net_query::fetch::<UserNetGlobal>(NET_Q_GLOBAL, NET_IFINDEX_NONE).ok()?;
    let global = global.records.first()?;

    let mut model = NetPanelModel::EMPTY;
    model.enabled = global.enabled != 0;
    model.connectivity = global.connectivity;
    model.gateway = global.default_gateway;

    for iface in &ifaces.records {
        let mut row = IfaceRow::named(
            net_query::name_of(iface).as_bytes(),
            IfaceKind::from_abi(iface.kind),
        );
        row.admin_up = iface.admin_up != 0;
        row.carrier = iface.carrier != 0;
        row.oper = iface.oper_state;
        // Any address will do: the indicator asks only whether there is one,
        // and the panel reads the full list from `net_cache`, not this summary.
        if let Some(addr) = addrs
            .records
            .iter()
            .find(|addr| addr.ifindex == iface.ifindex)
        {
            row.ipv4 = addr.addr;
            row.prefix_len = addr.prefix_len;
        }
        model.push_iface(row);
    }
    Some(model)
}

/// What the menu bar names the focused window by: its app's name in the
/// dock when an entry there names the app, else its title.
fn active_app_name(
    windows: &[UserWindowInfo; MAX_WINDOWS],
    count: u32,
    focused_task: u32,
    shelf: &dock::LauncherShelf,
) -> String {
    let Some(window) = windows[..count as usize]
        .iter()
        .find(|w| focused_task != 0 && w.task_id == focused_task)
    else {
        return String::new();
    };
    shelf
        .app_name(window.app_id.as_str())
        .unwrap_or_else(|| window.title_str())
        .to_owned()
}
