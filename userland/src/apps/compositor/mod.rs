macro_rules! log {
    ($($arg:tt)*) => {{
        let line = std::format!("COMPOSITOR: {}\n", format_args!($($arg)*));
        let _ = $crate::syscall::tty::write(line.as_bytes());
    }};
}

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
mod overlay;
mod popover;
pub mod protocol;
mod region;
mod renderer;
mod status_item;
mod surface_cache;
pub mod views;
mod wallpaper;
pub mod window_info;

use crate::gfx::DamageRect;
use crate::init_control::Command;
use crate::net_query;
use crate::syscall::{DisplayInfo, core as sys_core, process, window};
use region::Region;
use slopos_abi::input::POINTER_AXIS_VERTICAL;
use slopos_abi::net::{
    NET_EVENT_LEN, NET_IFINDEX_NONE, NET_MON_DEFAULT, NET_Q_ADDRS, NET_Q_GLOBAL, NET_Q_IFACES,
    UserAddr, UserIface, UserNetGlobal,
};
use slopos_abi::syscall::POLLIN;
use slopos_chrome_core::clock::WallTime;
use slopos_chrome_core::menu::{Choice, Ending};
use slopos_chrome_core::netstate::{
    IfaceKind, IfaceRow, NetIndicatorState, NetPanelModel, indicator_state_for,
};
use slopos_chrome_core::positioner;
use slopos_chrome_core::status::{BAR_HEIGHT, MAX_STATUS_ITEMS};
use slopos_protocol::server::MAX_CLIENTS;
use slopos_rt::{Ring, slopfut};
use std::time::Instant;
use std::vec::Vec;

use dock::{DOCK_HEIGHT, Dock};
use frame::{TITLE_BAR_HEIGHT, WindowBounds};
use hover::HoverRegistry;
use input::{InputHandler, Pressed};
use menu_bar::MenuBar;
use output::{CompositorOutput, FrameMetrics, RenderMode, estimate_present_bytes};
use protocol::ProtocolBridge;
use renderer::Renderer;
use surface_cache::ClientSurfaceCache;
use window_info::{WindowInfo, WindowState};

const MAX_WINDOWS: usize = 32;

struct WindowManager {
    windows: [WindowInfo; MAX_WINDOWS],
    window_count: u32,
    prev_windows: [WindowInfo; MAX_WINDOWS],
    prev_window_count: u32,

    input: InputHandler,
    renderer: Renderer,
    hover_registry: HoverRegistry,
    surface_cache: ClientSurfaceCache,

    menu_bar: MenuBar,
    dock: Dock,
    popover: popover::Popover,
    asking: confirm::Asking,
    /// Endings the person went ahead with that did not happen.
    undone: std::sync::mpsc::Receiver<Ending>,
    undone_tx: std::sync::mpsc::Sender<Ending>,
    consent: consent::Consent,
    gate: gate::Gate,
    /// Another session is in front: nothing is drawn until init says this
    /// one is again.
    behind: bool,
    init_says: Option<InitSays>,
    /// A new base changed the session's programs: the system menu offers the
    /// restart on the next frame.
    restart_offer_due: bool,

    /// `None` on the login screen, or when its socket could not be bound.
    protocol: Option<Box<ProtocolBridge>>,

    protocol_serial: u32,
    protocol_pointer_focus: u32,
    /// The surface a press on its content pinned the pointer to until every
    /// button is up, as `wl_pointer` has it, or 0: a drag that leaves the
    /// window still ends there.
    protocol_pointer_grab: u32,
    /// The grab's window is a slot, which is recycled.
    protocol_pointer_grab_gen: u32,

    first_frame: bool,
    /// Disjoint.
    output_damage: Region,
    force_full_redraw: bool,
    /// What a present that did not reach the screen leaves for the next.
    pending_damage: Region,
    pending_full: bool,
    /// The surfaces whose damage this frame shows, which alone are clean
    /// once it reaches the screen.
    frame_dirty_surfaces: Vec<u32>,
    prev_window_bounds: [WindowBounds; MAX_WINDOWS],
    prev_cursor_shape: u8,
    net_cache: NetPanelModel,
    net_indicator: NetIndicatorState,
    net_last_poll: Option<Instant>,
    /// A `net_monitor` drives refreshes, and no timer backs it up, so one
    /// that stopped is noticed rather than masked.
    net_event_driven: bool,
    /// Whether the virtio-gpu overlay draws the pointer, once probed.
    hw_cursor: Option<bool>,
    /// -1 before any.
    hw_cursor_shape: i32,
    /// Where the overlay was last moved to, so a move that blocks is made
    /// only when the pointer moved.
    hw_cursor_last_x: i32,
    hw_cursor_last_y: i32,
}

impl WindowManager {
    fn new(consent: consent::Consent, gate: gate::Gate) -> Self {
        let mut dock = Dock::new();
        let home = std::env::var("HOME").ok();
        if !gate.greeter() {
            dock.pin(crate::app_entry::pinned(home.as_deref()));
        }
        let protocol = match gate.greeter() {
            true => None,
            false => ProtocolBridge::new().map(Box::new),
        };
        let (undone_tx, undone) = std::sync::mpsc::channel();
        Self {
            windows: [WindowInfo::default(); MAX_WINDOWS],
            window_count: 0,
            prev_windows: [WindowInfo::default(); MAX_WINDOWS],
            prev_window_count: 0,
            input: InputHandler::new(),
            renderer: Renderer::new(),
            hover_registry: HoverRegistry::new(),
            surface_cache: ClientSurfaceCache::new(),
            menu_bar: MenuBar::new(),
            dock,
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

    /// Whether the virtio-gpu overlay draws the pointer, `shape` uploaded to
    /// it when it changed.
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
        // A failed upload once the overlay is in use keeps its last shape, so
        // two pointers never show.
        if self.hw_cursor.is_none() {
            self.hw_cursor = Some(false);
            false
        } else {
            true
        }
    }

    /// A failed query keeps the model it had: a passing error is no sign the
    /// network went away.
    fn refresh_from_kernel(&mut self) {
        let Some(model) = read_net_model() else {
            return;
        };
        self.net_indicator = indicator_state_for(&model);
        self.net_cache = model;
        self.menu_bar.set_network(true, self.net_indicator);
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

        self.dock
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

            let prev = self.find_prev_index(window.id);

            if let Some(prev_idx) = prev {
                let old = saved_bounds[prev_idx];
                let geometry_changed = old != curr_bounds;
                // A restack moves nothing but changes what covers what.
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

            if window.state == WindowState::Minimized {
                continue;
            }

            if window.is_dirty() {
                self.add_window_damage(&window);
                self.frame_dirty_surfaces.push(window.id);
            }
        }

        for i in 0..self.prev_window_count as usize {
            let prev = &self.prev_windows[i];
            if !self.window_exists(prev.id) {
                self.add_bounds_damage(&saved_bounds[i]);
            }
        }
    }

    /// What the pointer's path, the dock and hover repaint, once the frame's
    /// input has laid the path.
    fn add_pointer_damage(&mut self) {
        let cursor_moved = self.input.cursor_trail_count > 0;
        if self.dock.take_content_dirty() || cursor_moved {
            self.output_damage.add(self.dock.bounds());
            self.output_damage
                .add(self.dock.reach(self.input.mouse_x, self.input.mouse_y));
        }
        if cursor_moved {
            for i in 0..self.input.cursor_trail_count {
                let (x, y) = self.input.cursor_trail[i];
                self.output_damage.add(renderer::cursor_reach(x, y));
            }
            self.output_damage.add(renderer::cursor_reach(
                self.input.mouse_x,
                self.input.mouse_y,
            ));
        }
    }

    fn register_hover_regions(&mut self, buttons_hovered_window: u32) {
        self.hover_registry.begin_frame();

        if self.renderer.output_height == 0 {
            return;
        }

        for w in self.windows[..self.window_count as usize].iter().rev() {
            if w.state == WindowState::Minimized {
                continue;
            }
            let group = decorations::buttons_group(w.x, w.y - TITLE_BAR_HEIGHT);
            let hovered = w.id == buttons_hovered_window;
            self.hover_registry
                .register(hover::HOVER_BUTTONS_BASE | w.id, group, hovered);
        }

        let mut status_regions = [(0u32, DamageRect::invalid(), false); MAX_STATUS_ITEMS];
        let status_count = self.menu_bar.hover_regions(
            self.renderer.output_width,
            self.input.mouse_x,
            self.input.mouse_y,
            &mut status_regions,
        );
        for &(id, rect, hovered) in &status_regions[..status_count] {
            self.hover_registry.register(id, rect, hovered);
        }

        let damage = &mut self.output_damage;
        self.hover_registry.changed_regions(|rect| damage.add(rect));
    }

    /// Where `window` was in the last frame's list, which is its rank from
    /// the bottom.
    fn find_prev_index(&self, window: u32) -> Option<usize> {
        (0..self.prev_window_count as usize).find(|&i| self.prev_windows[i].id == window)
    }

    fn window_exists(&self, window: u32) -> bool {
        (0..self.window_count as usize).any(|i| self.windows[i].id == window)
    }

    fn add_bounds_damage(&mut self, bounds: &WindowBounds) {
        self.output_damage.add(bounds.rect());
    }

    /// The frame's events in order, each against the state the ones before
    /// it left, so a press is tested where the pointer was then.
    fn process_input_events(&mut self, area: positioner::Rect) {
        use slopos_abi::InputEventType;

        // Out of `self`, so `InputHandler` can borrow it mutably beside the
        // rest.
        let mut proto_box = self.protocol.take();

        for i in 0..self.input.raw_event_count {
            let event = self.input.raw_event_buf[i];
            let time = event.timestamp_ms as u32;
            let primary = event.data.data0 & 0x01 != 0;
            if event.event_type == InputEventType::PointerMotion {
                self.input.apply_motion(&event);
            }
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
                    p.send_modifiers_for_window(self.input.focused_window(), mods);
                }
                continue;
            }
            if self.consent.active() {
                match event.event_type {
                    InputEventType::PointerMotion => {
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
                    self.popover.hover(self.input.mouse_x, self.input.mouse_y);
                    self.input.apply_grab_motion(proto_box.as_deref_mut());
                    self.sync_pointer_focus(proto_box.as_deref_mut());
                    let target = self.pointer_target(proto_box.as_deref());
                    if target != 0 {
                        if let Some(p) = proto_box.as_deref_mut() {
                            p.send_pointer_motion_for_window(
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
                    let screen_w = self.renderer.output_width;
                    let pressed = self.input.on_button_press(
                        button,
                        area,
                        &self.windows,
                        self.window_count,
                        &self.dock,
                        self.popover.rect(),
                        screen_w,
                        &self.menu_bar,
                        proto_box.as_deref_mut(),
                    );
                    let (x, y) = (self.input.mouse_x, self.input.mouse_y);
                    match pressed {
                        Pressed::TogglePanel(panel) => {
                            if let Some(item) = self.menu_bar.item_rect(screen_w, panel) {
                                self.popover.toggle(panel, item);
                            }
                        }
                        Pressed::DismissPanel => self.popover.dismiss(),
                        Pressed::InPanel => self.popover.click(x, y, true),
                        Pressed::PassOn | Pressed::Taken => {}
                    }
                    // A grab whose release a flood of motion lost ends at the
                    // next press, as the input layer's own drags do.
                    if self.input.mouse_buttons & !button == 0 {
                        self.release_pointer_grab();
                        self.sync_pointer_focus(proto_box.as_deref_mut());
                    }
                    if pressed == Pressed::PassOn && self.protocol_pointer_focus != 0 {
                        if self.live_pointer_grab(proto_box.as_deref()) == 0 {
                            self.protocol_pointer_grab = self.protocol_pointer_focus;
                            self.protocol_pointer_grab_gen = proto_box
                                .as_deref()
                                .and_then(|p| {
                                    p.surface_generation_for_window(self.protocol_pointer_focus)
                                })
                                .unwrap_or(0);
                        }
                        if let Some(p) = proto_box.as_deref_mut() {
                            self.protocol_serial = self.protocol_serial.wrapping_add(1);
                            p.send_pointer_button_for_window(
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
                            p.send_pointer_button_for_window(
                                pressed_on,
                                self.protocol_serial,
                                time,
                                event.data.data0,
                                0,
                            );
                        }
                    }
                    if self.input.mouse_buttons == 0 && self.protocol_pointer_grab != 0 {
                        self.release_pointer_grab();
                        self.sync_pointer_focus(proto_box.as_deref_mut());
                    }
                }
                InputEventType::PointerAxis => {
                    let target = self.pointer_target(proto_box.as_deref());
                    if target != 0 {
                        if let Some(p) = proto_box.as_deref_mut() {
                            p.send_pointer_axis_for_window(
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
                    let kbd_focus = self.input.focused_window();
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
            }
        }

        // A window can move, come or go under a pointer that stays still.
        self.sync_pointer_focus(proto_box.as_deref_mut());

        self.protocol = proto_box;
    }

    /// The grab's holder while one is held, else the surface under the
    /// pointer.
    fn pointer_target(&mut self, proto: Option<&ProtocolBridge>) -> u32 {
        match self.live_pointer_grab(proto) {
            0 => self.protocol_pointer_focus,
            grab => grab,
        }
    }

    /// The grab's holder, or 0 once the surface that took it is gone, which
    /// its generation tells from whatever took its slot.
    fn live_pointer_grab(&mut self, proto: Option<&ProtocolBridge>) -> u32 {
        if self.protocol_pointer_grab == 0 {
            return 0;
        }
        let live = proto
            .and_then(|p| p.surface_generation_for_window(self.protocol_pointer_grab))
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

    /// The lock or login screen or a question is over every window, so the
    /// pointer reaches none.
    fn covered(&self) -> bool {
        self.gate.shown() || self.consent.active() || self.asking.active()
    }

    /// Enter and leave the surfaces the pointer moved onto and off; it
    /// enters content alone.
    fn sync_pointer_focus(&mut self, proto: Option<&mut ProtocolBridge>) {
        if self.live_pointer_grab(proto.as_deref()) != 0 {
            return;
        }
        let hit = self.input.resolve_cursor_hit(
            &self.windows,
            self.window_count,
            &self.dock,
            self.popover.rect(),
            self.renderer.output_width,
            &self.menu_bar,
        );
        let new_ptr_focus = match hit.part {
            input::CursorPart::Content if !self.covered() => hit.window,
            _ => 0,
        };
        if new_ptr_focus == self.protocol_pointer_focus {
            return;
        }
        if let Some(p) = proto {
            if self.protocol_pointer_focus != 0 {
                p.send_pointer_leave_for_window(self.protocol_pointer_focus);
            }
            if new_ptr_focus != 0 {
                // 0 is the serial of no enter, which a cursor request may not
                // carry.
                self.protocol_serial = self.protocol_serial.wrapping_add(1).max(1);
                p.send_pointer_enter_for_window(
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
        let mut pre_windows = [WindowInfo::default(); MAX_WINDOWS];
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
            let id = self.windows[i].id;
            let prev_idx = pre_windows[..pre_count].iter().position(|w| w.id == id);
            if let Some(j) = prev_idx {
                let p = pre_bounds[j];
                let geometry_changed = p != curr_bounds;
                // A press raises a window, so a restack shows only here.
                let restacked = j != i;
                if geometry_changed || restacked {
                    self.add_bounds_damage(&p);
                    self.add_bounds_damage(&curr_bounds);
                }
            }
            self.prev_window_bounds[i] = curr_bounds;
        }
    }

    fn add_window_damage(&mut self, window: &WindowInfo) {
        if window.damage_count == u8::MAX {
            let bounds = WindowBounds::from_window(window);
            self.add_bounds_damage(&bounds);
            return;
        }

        for region in &window.damage_regions[..window.damage_count as usize] {
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

    /// What a change of focus repaints of `window`: its title bar, its
    /// shadow, which darkens with focus, and its content's corners, which
    /// blend over the shadow.
    fn add_focus_damage_for_window(&mut self, window: u32) {
        let Some(w) = self.windows[..self.window_count as usize]
            .iter()
            .find(|w| window != 0 && w.id == window)
        else {
            return;
        };
        let interior = DamageRect {
            x0: w.x,
            y0: w.y,
            x1: w.x + w.effective_width() as i32 - 1,
            y1: w.y + w.effective_height() as i32 - 1 - slopos_theme_core::radius::WINDOW,
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

    /// What the lock or login screen and the questions repaint, each laid out
    /// again when it changed: privd's over the topmost window of the process
    /// that asks, when it has one shown.
    fn add_overlay_damage(&mut self, screen: slopos_appkit::Rect) {
        let pointer = (self.input.mouse_x, self.input.mouse_y);
        let windows = &self.windows[..self.window_count as usize];
        let anchor = self.consent.asker().and_then(|pid| {
            let w = windows
                .iter()
                .rev()
                .find(|w| w.pid == pid && w.state != WindowState::Minimized)?;
            let frame = frame::frame_box(w);
            Some(slopos_appkit::Rect::new(
                frame.x0,
                frame.y0,
                frame.x1 - frame.x0 + 1,
                frame.y1 - frame.y0 + 1,
            ))
        });
        for rect in self.consent.take_damage(screen, anchor, pointer) {
            self.output_damage.add(rect);
        }
        for rect in self.gate.take_damage(screen, pointer) {
            self.output_damage.add(rect);
        }
        for rect in self.asking.take_damage(screen, pointer) {
            self.output_damage.add(rect);
        }
    }

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
        let screen = screen_rect(fb_info);
        wm.input.drain_events();
        wm.refresh_windows();
        wm.gate.poll();
        wm.add_overlay_damage(screen);
        let focus_before = wm.input.focused_window();

        wm.input.sync_keyboard_focus(
            &wm.windows,
            wm.window_count,
            &wm.prev_windows,
            wm.prev_window_count,
        );
        wm.process_input_events(work_area(fb_info.width, fb_info.height));
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

        wm.add_pointer_damage();

        // Against this snapshot of the windows, before the resync below can
        // reorder them.
        let cursor_hit = wm.input.resolve_cursor_hit(
            &wm.windows,
            wm.window_count,
            &wm.dock,
            wm.popover.rect(),
            wm.renderer.output_width,
            &wm.menu_bar,
        );
        let (cursor_shape, buttons_hovered_window) = match wm.covered() {
            true => (slopos_protocol::CURSOR_SHAPE_DEFAULT, 0),
            false => (
                wm.input.cursor_shape_for(&cursor_hit, &wm.windows),
                wm.input.buttons_hovered_window(&cursor_hit, &wm.windows),
            ),
        };
        wm.register_hover_regions(buttons_hovered_window);

        if cursor_shape != wm.prev_cursor_shape {
            wm.output_damage
                .add(renderer::cursor_reach(wm.input.mouse_x, wm.input.mouse_y));
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

        let focus_after = wm.input.focused_window();
        if focus_after != focus_before {
            wm.add_focus_damage_for_window(focus_before);
            wm.add_focus_damage_for_window(focus_after);
            wm.output_damage
                .add(wm.menu_bar.app_name_damage(output.width));
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
            && let Some(item) = wm.menu_bar.item_rect(output.width, popover::Panel::System)
        {
            wm.popover.offer_restart(item);
        }
        // Before the panel is laid out, so a switch request that timed out
        // this frame reverts in this frame.
        wm.popover.settle(&wm.net_cache);
        let pointer = (wm.input.mouse_x, wm.input.mouse_y);
        wm.popover
            .refresh(&wm.net_cache, output.width, output.height, pointer);
        for rect in wm.popover.take_damage() {
            wm.output_damage.add(rect);
        }
        wm.add_overlay_damage(screen);

        let now = sys_core::realtime_secs().and_then(WallTime::from_unix);
        let mut bar_damage = [DamageRect::invalid(); menu_bar::MAX_BAR_DAMAGE];
        let bar_damage_count = wm
            .menu_bar
            .take_damage(output.width, now.as_ref(), &mut bar_damage);
        for rect in &bar_damage[..bar_damage_count] {
            wm.output_damage.add(*rect);
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

            // Never fewer pixels than were painted.
            let (damage_arr, damage_n) = frame_damage.to_bounded::<MAX_OUTPUT_DAMAGE_REGIONS>();

            if let Some(mut buf) = output.draw_buffer() {
                buf.set_pixel_format(pixel_format);

                let active_app_name = active_app_name(
                    &wm.windows,
                    wm.window_count,
                    wm.input.focused_window(),
                    &wm.dock,
                );

                wm.renderer.render(
                    &mut buf,
                    &wm.windows[..wm.window_count as usize],
                    wm.input.focused_window(),
                    buttons_hovered_window,
                    wm.input.mouse_x,
                    wm.input.mouse_y,
                    cursor_shape,
                    &mut wm.surface_cache,
                    &wm.menu_bar,
                    &mut wm.dock,
                    &wm.popover,
                    &wm.asking,
                    &wm.consent,
                    &wm.gate,
                    &active_app_name,
                    &frame_damage,
                    hw_cursor,
                );
            }

            // No damage is a whole flip.
            let damage_slice: &[DamageRect] = if force_full {
                &[]
            } else {
                &damage_arr[..damage_n]
            };

            let flip_result = output.present(damage_slice);
            if *frame_count < 3 {
                if flip_result {
                    log!("fb_flip ok");
                } else {
                    log!("fb_flip FAILED");
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
            log!("frames={frames} bytes={bytes}");
        }

        // Every frame: the probe in `cleanup_disconnected` reads bytes the
        // client's readiness stream will not fire for again.
        if let Some(ref mut proto) = wm.protocol {
            proto.process_requests();
            proto.cleanup_disconnected();
            proto.flush_all();
        }

        // What the dock launched, whose ids nothing keeps.
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

    /// Before the pass takes any input, so a lock init sent before handing
    /// the seats back holds for every key typed after.
    fn hear_init(&mut self) {
        let Some(says) = self.init_says.as_mut() else {
            return;
        };
        let (lines, closed) = says.read();
        for line in lines {
            self.told(&line);
        }
        if closed {
            log!("init's session pipe closed");
            self.init_says = None;
        }
    }

    /// Behind, its clients are still served, and their frames wait.
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
    let kind = match std::env::args().nth(1).as_deref() {
        Some(GREETER_ARG) => gate::Kind::Greeter,
        _ => gate::Kind::Desktop {
            name: std::env::var("USER").unwrap_or_default(),
        },
    };
    log!("starting");

    // The seats are held for the process's life, released only by its exit;
    // a session of init's takes them as init hands them over.
    if session.is_none() {
        if !acquire_seat(window::screen_acquire) {
            log!("screen seat unavailable");
        }
        if !acquire_seat(window::input_sink_acquire) {
            log!("input seat unavailable");
        }
    }

    let mut wm = WindowManager::new(consent, gate::Gate::new(kind));
    let mut fb_info = DisplayInfo::default();

    if window::fb_info(&mut fb_info) < 0 {
        log!("fb_info failed");
        loop {
            sys_core::yield_now();
        }
    }
    log!("fb_info ok");

    let output = match CompositorOutput::new(&fb_info) {
        Some(out) => out,
        None => {
            log!("output alloc failed");
            loop {
                sys_core::yield_now();
            }
        }
    };
    log!("output allocated");

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

    // The accept stream, one per client and the frame timer.
    let ring = Ring::setup(64).expect("COMPOSITOR: ring setup failed");
    slopfut::block_on(
        ring,
        compositor_async(wm, output, fb_info, pixel_format, readiness, session),
    );
}

/// The login screen's argument; a desktop takes none.
pub const GREETER_ARG: &str = "--greeter";
const BEHIND_FRAME_MS: u64 = 100;

fn session_pipe() -> Option<crate::syscall::OwnedFd> {
    use crate::init_control::fds::COMPOSITOR_SESSION;
    use slopos_abi::syscall::F_GETFD;
    use slopos_slibc::pal::{Pal, Sys};
    Sys::fcntl(COMPOSITOR_SESSION, F_GETFD as i32, 0).ok()?;
    let _ = crate::syscall::fs::set_fd_cloexec(COMPOSITOR_SESSION);
    // SAFETY: init handed this descriptor over and nothing else here owns it.
    let fd = unsafe { crate::syscall::OwnedFd::from_raw(COMPOSITOR_SESSION) };
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

/// Where windows open and what a maximized one fills: the screen between the
/// menu bar, its border row included, and the dock.
pub fn work_area(screen_w: u32, screen_h: u32) -> positioner::Rect {
    let top = BAR_HEIGHT + 1;
    positioner::Rect::new(0, top, screen_w as i32, screen_h as i32 - top - DOCK_HEIGHT)
}

enum Undone {
    /// The person answered privd's question so.
    Refused,
    Failed(String),
}

/// Carry `ending` out on a thread of its own, through init or privd; `undone`
/// hears of one that failed other than by the person's no, and the log why.
fn carry_out(ending: Ending, undone: std::sync::mpsc::Sender<Ending>) {
    let report = undone.clone();
    let spawned = std::thread::Builder::new()
        .name("compositor-ending".into())
        .spawn(move || {
            let done = match ending {
                Ending::RestartDesktop => init(Command::RestartSession),
                Ending::LogOut => init(Command::EndSession),
                Ending::Restart => power("org.slopos.power.reboot", "reboot"),
                Ending::PowerOff => power("org.slopos.power.poweroff", "poweroff"),
            };
            if let Err(Undone::Failed(why)) = done {
                log!("{}: {why}", ending.verb());
                let _ = undone.send(ending);
            }
        });
    if let Err(why) = spawned {
        log!("{}: {why}", ending.verb());
        let _ = report.send(ending);
    }
}

fn init(command: Command) -> Result<(), Undone> {
    crate::init_control::ask(command).map_err(Undone::Failed)
}

fn power(action: &str, arg: &str) -> Result<(), Undone> {
    match crate::privd::run_quietly(action, &[arg], b"") {
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
                log!("init: {why}");
            }
        });
}

const TARGET_FRAME_MS: u64 = 16;

/// Polled only without a `net_monitor`.
const NET_POLL_INTERVAL_MS: u128 = 2000;

const METRICS_REPORT_MS: u64 = 5000;

/// Off unless `SLOPOS_COMPOSITOR_METRICS` asks, since the report goes to the
/// console over whatever the person is doing.
fn metrics_reporting_enabled() -> bool {
    std::env::var("SLOPOS_COMPOSITOR_METRICS").is_ok_and(|v| v != "0")
}

/// The kernel's per flip, past which it drops rects.
const MAX_OUTPUT_DAMAGE_REGIONS: usize = slopos_abi::damage::MAX_DAMAGE_REGIONS;

const ACCEPT_BATCH: usize = MAX_CLIENTS;

/// One thread runs the accept task, each client's and the frame timer, so no
/// `RefCell` borrow may cross an `.await`.
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

    // A connection queued while starting is greeted before init hears the
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

    // Readiness rather than `accept_multishot`, so `Server::accept()` alone
    // gives out slots; a stream that ends on a passing error is armed again.
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
                log!("accept stream ended; re-arming");
                // The frame's own accept covers the gap.
                slopfut::time::sleep_ms(50).await;
            }
        });
    }

    let mut frame_count: u32 = 0;
    let mut metrics = FrameMetrics::new(metrics_reporting_enabled());
    let time_origin = Instant::now();
    loop {
        let frame_start = Instant::now();

        // Covers the accept stream while it is armed again.
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

/// Drive the network indicator from a `net_monitor`: each wake reads the
/// whole model again, so a lost event costs nothing, and the monitor opens
/// before the first read, so nothing falls between. Whether it opened.
fn spawn_net_monitor_task(wm: std::rc::Rc<std::cell::RefCell<WindowManager>>) -> bool {
    let fd = match crate::syscall::net::net_monitor(NET_MON_DEFAULT, 0) {
        Ok(fd) => fd,
        Err(_) => {
            log!("net_monitor unavailable; polling instead");
            return false;
        }
    };
    let raw = fd.into_raw();

    wm.borrow_mut().refresh_from_kernel();

    slopfut::spawn(async move {
        let mut buf = [0u8; NET_EVENT_LEN * 16];
        loop {
            let mut stream = slopfut::poll_add_multishot(raw, POLLIN);
            while stream.next().await.is_some() {
                // To empty, or the stream spins; an empty read is `EAGAIN`.
                while crate::syscall::fs::read_slice(raw, &mut buf).unwrap_or(0) > 0 {}
                wm.borrow_mut().refresh_from_kernel();
            }
            log!("net_monitor stream ended; re-arming");
            slopfut::time::sleep_ms(500).await;
        }
    });
    true
}

/// A oneshot poll each time: a multishot fires only when readiness changes,
/// so a question landing between a read and the ring's next pass would wake
/// nothing, and privd answers a `READY` at once.
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
        log!("privd's consent channel closed");
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

    /// The lines written since, and whether init closed its end.
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

/// Offer a restart of the desktop while the base holds another compositor or
/// terminal than the session started from.
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

/// Serve `idx`'s requests as `fd` is readable, until it disconnects.
fn spawn_client_task(
    wm: std::rc::Rc<std::cell::RefCell<WindowManager>>,
    idx: usize,
    fd: i32,
    generation: u64,
) {
    if fd < 0 {
        return;
    }
    // A successor can hold this slot and fd; only the generation tells.
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
                    // The last requests may come with the hangup, which ends
                    // the stream without a readiness of their own.
                    let mut w = wm.borrow_mut();
                    if let Some(ref mut proto) = w.protocol {
                        if owns(proto) {
                            if proto.process_client(idx) {
                                proto.disconnect_client(idx);
                            }
                        }
                    }
                    break;
                }
            }
        }
    });
}

/// Interfaces, addresses and the global record, joined on `ifindex`; `None`
/// when a query failed.
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
    windows: &[WindowInfo; MAX_WINDOWS],
    count: u32,
    focused: u32,
    dock: &Dock,
) -> String {
    let Some(window) = windows[..count as usize]
        .iter()
        .find(|w| focused != 0 && w.id == focused)
    else {
        return String::new();
    };
    dock.app_name(window.app_id.as_str())
        .unwrap_or_else(|| window.title_str())
        .to_owned()
}
