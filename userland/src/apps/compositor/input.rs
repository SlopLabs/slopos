use std::time::Instant;

use slopos_abi::InputEvent;
use slopos_abi::input::MODIFIER_SUPER;
use slopos_abi::surface::{WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL};
use slopos_abi::task::TaskPriority;
use slopos_abi::window::{
    CURSOR_SHAPE_DEFAULT, CURSOR_SHAPE_E_RESIZE, CURSOR_SHAPE_GRAB, CURSOR_SHAPE_GRABBING,
    CURSOR_SHAPE_N_RESIZE, CURSOR_SHAPE_NE_RESIZE, CURSOR_SHAPE_NW_RESIZE, CURSOR_SHAPE_S_RESIZE,
    CURSOR_SHAPE_SE_RESIZE, CURSOR_SHAPE_SW_RESIZE, CURSOR_SHAPE_W_RESIZE,
};
use slopos_chrome_core::positioner::Rect as ChromeRect;
use slopos_protocol::toplevel_state::{ACTIVATED, MAXIMIZED, RESIZING};

use super::MAX_WINDOWS;
use super::decorations::{self, WindowButton};
use super::dock::Dock;
use super::frame::TITLE_BAR_HEIGHT;
use super::menu_bar::MenuBar;
use super::popover::Panel;
use super::protocol::ProtocolBridge;
use crate::syscall::{UserWindowInfo, input, process};

/// What a press asks of the window manager.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pressed {
    /// The client's under the pointer: a press on its content, or of any
    /// button but the primary.
    PassOn,
    /// Taken by a frame, the dock or the desktop.
    Taken,
    TogglePanel(Panel),
    DismissPanel,
    /// Within the open popover, at the pointer.
    InPanel,
}

const MAX_CURSOR_TRAIL: usize = 16;

/// Wayland's edge bits: top 1, bottom 2, left 4, right 8.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ResizeEdge(u8);

impl ResizeEdge {
    pub const NONE: Self = Self(0);
    pub const TOP: Self = Self(1);
    pub const BOTTOM: Self = Self(2);
    pub const LEFT: Self = Self(4);
    pub const RIGHT: Self = Self(8);
    pub const TOP_LEFT: Self = Self(1 | 4);
    pub const TOP_RIGHT: Self = Self(1 | 8);
    pub const BOTTOM_LEFT: Self = Self(2 | 4);
    pub const BOTTOM_RIGHT: Self = Self(2 | 8);

    #[inline]
    pub fn is_none(self) -> bool {
        self.0 == 0
    }
    #[inline]
    pub fn has_top(self) -> bool {
        self.0 & 1 != 0
    }
    #[inline]
    pub fn has_bottom(self) -> bool {
        self.0 & 2 != 0
    }
    #[inline]
    pub fn has_left(self) -> bool {
        self.0 & 4 != 0
    }
    #[inline]
    pub fn has_right(self) -> bool {
        self.0 & 8 != 0
    }

    pub fn cursor_shape(self) -> u8 {
        match self.0 {
            1 => CURSOR_SHAPE_N_RESIZE,
            2 => CURSOR_SHAPE_S_RESIZE,
            4 => CURSOR_SHAPE_W_RESIZE,
            8 => CURSOR_SHAPE_E_RESIZE,
            5 => CURSOR_SHAPE_NW_RESIZE,
            9 => CURSOR_SHAPE_NE_RESIZE,
            6 => CURSOR_SHAPE_SW_RESIZE,
            10 => CURSOR_SHAPE_SE_RESIZE,
            _ => CURSOR_SHAPE_DEFAULT,
        }
    }
}

/// What the pointer is over, from the top down. Its shape, pointer focus,
/// hover and where a press goes all follow from this one answer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CursorPart {
    /// The open popover holds the pointer, so it outranks every window.
    Popover,
    /// A press here closes the popover and reaches nothing else.
    PopoverOutside,
    MenuBar(Option<Panel>),
    Dock(usize),
    ResizeEdge(ResizeEdge),
    Button(WindowButton),
    /// The title bar less its buttons.
    TitleBar,
    Content,
    Desktop,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CursorHit {
    pub part: CursorPart,
    /// `usize::MAX` for a part of no window.
    pub window_idx: usize,
    /// 0 for a part of no window.
    pub task_id: u32,
}

impl CursorHit {
    fn ui(part: CursorPart) -> Self {
        Self {
            part,
            window_idx: usize::MAX,
            task_id: 0,
        }
    }
    fn window(part: CursorPart, idx: usize, task_id: u32) -> Self {
        Self {
            part,
            window_idx: idx,
            task_id,
        }
    }
    pub fn is_window(self) -> bool {
        self.window_idx != usize::MAX
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Geometry {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl Geometry {
    fn of(w: &UserWindowInfo) -> Geometry {
        Geometry {
            x: w.x,
            y: w.y,
            w: w.effective_width(),
            h: w.effective_height(),
        }
    }
}

/// A window zoom resized: where it was before and where zoom put it.
#[derive(Clone, Copy)]
struct Zoomed {
    task_id: u32,
    generation: u32,
    before: Geometry,
    after: Geometry,
}

pub struct InputHandler {
    pub mouse_x: i32,
    pub mouse_y: i32,
    pub mouse_buttons: u8,

    pub dragging: bool,
    drag_task: u32,
    drag_offset_x: i32,
    drag_offset_y: i32,

    pub resizing: bool,
    resize_task: u32,
    resize_edges: ResizeEdge,
    resize_grab_x: i32,
    resize_grab_y: i32,
    resize_grab_w: u32,
    resize_grab_h: u32,
    resize_grab_mouse_x: i32,
    resize_grab_mouse_y: i32,
    resize_last_configure_ms: u64,

    zoomed: Vec<Zoomed>,

    focused_task: u32,
    pub needs_full_redraw: bool,

    pub cursor_trail: [(i32, i32); MAX_CURSOR_TRAIL],
    pub cursor_trail_count: usize,

    clock_origin: Instant,

    local_modifier_state: u8,
    pub raw_event_buf: [InputEvent; 64],
    pub raw_event_count: usize,
}

impl InputHandler {
    pub fn new() -> Self {
        Self {
            mouse_x: 0,
            mouse_y: 0,
            mouse_buttons: 0,
            dragging: false,
            drag_task: 0,
            drag_offset_x: 0,
            drag_offset_y: 0,
            resizing: false,
            resize_task: 0,
            resize_edges: ResizeEdge::NONE,
            resize_grab_x: 0,
            resize_grab_y: 0,
            resize_grab_w: 0,
            resize_grab_h: 0,
            resize_grab_mouse_x: 0,
            resize_grab_mouse_y: 0,
            resize_last_configure_ms: 0,
            zoomed: Vec::new(),
            focused_task: 0,
            needs_full_redraw: false,
            cursor_trail: [(0, 0); MAX_CURSOR_TRAIL],
            cursor_trail_count: 0,
            clock_origin: Instant::now(),
            local_modifier_state: 0,
            raw_event_buf: [InputEvent::default(); 64],
            raw_event_count: 0,
        }
    }

    pub fn focused_task(&self) -> u32 {
        self.focused_task
    }

    #[inline]
    fn now_ms(&self) -> u64 {
        self.clock_origin.elapsed().as_millis() as u64
    }

    /// The frame's events, which the frame dispatches in order, so each
    /// press is tested where the pointer was then.
    pub fn drain_events(&mut self) {
        self.cursor_trail_count = 0;
        self.raw_event_count = input::poll_batch(&mut self.raw_event_buf);
    }

    pub fn apply_motion(&mut self, event: &InputEvent) {
        let new_x = event.pointer_x();
        let new_y = event.pointer_y();
        if new_x != self.mouse_x || new_y != self.mouse_y {
            if self.cursor_trail_count < MAX_CURSOR_TRAIL {
                self.cursor_trail[self.cursor_trail_count] = (self.mouse_x, self.mouse_y);
                self.cursor_trail_count += 1;
            }
            self.mouse_x = new_x;
            self.mouse_y = new_y;
        }
    }

    pub fn apply_grab_motion(&mut self, proto: Option<&mut ProtocolBridge>) {
        if self.dragging {
            self.update_drag(proto);
        } else if self.resizing {
            self.update_resize(proto);
        }
    }

    pub(super) fn set_modifier_state(&mut self, mods: u8) {
        self.local_modifier_state = mods;
    }

    pub fn sync_keyboard_focus(
        &mut self,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
        prev_windows: &[UserWindowInfo; MAX_WINDOWS],
        prev_window_count: u32,
    ) {
        for i in 0..window_count as usize {
            let task_id = windows[i].task_id;
            if windows[i].state == WINDOW_STATE_MINIMIZED {
                continue;
            }
            let existed_before =
                (0..prev_window_count as usize).any(|j| prev_windows[j].task_id == task_id);
            if !existed_before {
                self.set_focused(task_id);
                break;
            }
        }

        if self.focused_task != 0 {
            let still_visible = (0..window_count as usize).any(|i| {
                windows[i].task_id == self.focused_task
                    && windows[i].state != WINDOW_STATE_MINIMIZED
            });
            if !still_visible {
                let mut new_focus = 0u32;
                for i in (0..window_count as usize).rev() {
                    if windows[i].state != WINDOW_STATE_MINIMIZED {
                        new_focus = windows[i].task_id;
                        break;
                    }
                }
                self.set_focused(new_focus);
            }
        }

        // A key must never fall into nothing while a window shows.
        if self.focused_task == 0 {
            for i in (0..window_count as usize).rev() {
                if windows[i].state != WINDOW_STATE_MINIMIZED {
                    self.set_focused(windows[i].task_id);
                    break;
                }
            }
        }
    }

    pub fn on_button_release(&mut self, button: u8, proto: Option<&mut ProtocolBridge>) {
        self.mouse_buttons &= !button;
        if button & 0x01 == 0 {
            return;
        }
        if self.dragging {
            self.stop_drag();
        } else if self.resizing {
            self.stop_resize(proto);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_button_press(
        &mut self,
        button: u8,
        area: ChromeRect,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
        dock: &Dock,
        popover: Option<slopos_appkit::Rect>,
        screen_width: u32,
        bar: &MenuBar,
        mut proto: Option<&mut ProtocolBridge>,
    ) -> Pressed {
        self.mouse_buttons |= button;
        if button & 0x01 == 0 {
            return Pressed::PassOn;
        }

        // A press during a grab means its release was lost under a flood of
        // motion.
        if self.dragging {
            self.stop_drag();
        }
        if self.resizing {
            self.stop_resize(proto.as_deref_mut());
        }

        let hit = self.resolve_cursor_hit(windows, window_count, dock, popover, screen_width, bar);
        match hit.part {
            CursorPart::Popover => Pressed::InPanel,
            CursorPart::PopoverOutside => Pressed::DismissPanel,
            CursorPart::MenuBar(Some(panel)) => Pressed::TogglePanel(panel),
            CursorPart::MenuBar(None) => Pressed::Taken,
            CursorPart::Dock(idx) => {
                self.dock_click(idx, dock, windows, window_count, &mut proto);
                Pressed::Taken
            }
            CursorPart::ResizeEdge(edge) => {
                let window = windows[hit.window_idx];
                self.start_resize(&window, edge);
                if let Some(ref mut p) = proto {
                    p.raise_window(window.task_id);
                }
                self.set_focused(window.task_id);
                Pressed::Taken
            }
            CursorPart::Button(button) => {
                let window = windows[hit.window_idx];
                let shown = &windows[..window_count as usize];
                self.window_button(button, &window, shown, area, &mut proto);
                Pressed::Taken
            }
            CursorPart::TitleBar => {
                let window = windows[hit.window_idx];
                self.start_drag(&window);
                if let Some(ref mut p) = proto {
                    p.raise_window(window.task_id);
                }
                self.set_focused(window.task_id);
                Pressed::Taken
            }
            CursorPart::Content => {
                let window = windows[hit.window_idx];
                if let Some(ref mut p) = proto {
                    p.raise_window(window.task_id);
                }
                self.set_focused(window.task_id);
                // Super and a press move a window that is not maximized.
                if self.local_modifier_state & MODIFIER_SUPER != 0
                    && window.state != WINDOW_STATE_MAXIMIZED
                {
                    self.start_drag(&window);
                    return Pressed::Taken;
                }
                Pressed::PassOn
            }
            // Focus stays where it was, so keys typed next still land.
            CursorPart::Desktop => Pressed::Taken,
        }
    }

    fn window_button(
        &mut self,
        button: WindowButton,
        window: &UserWindowInfo,
        windows: &[UserWindowInfo],
        area: ChromeRect,
        proto: &mut Option<&mut ProtocolBridge>,
    ) {
        match button {
            WindowButton::Close => {
                if let Some(p) = proto.as_deref_mut() {
                    p.send_close_for_task(window.task_id);
                }
            }
            WindowButton::Minimize => {
                if let Some(p) = proto.as_deref_mut() {
                    p.set_window_state(window.task_id, WINDOW_STATE_MINIMIZED);
                }
            }
            WindowButton::Zoom if window.fixed_size => {}
            WindowButton::Zoom => {
                if let Some(p) = proto.as_deref_mut() {
                    self.zoom(window, windows, area, p);
                }
            }
        }
    }

    /// Fill `area` with the window, or as much of it as the window's limits
    /// allow, centred and still a window; or put back where zoom found it a
    /// window zoom resized that is still maximized or where zoom put it.
    fn zoom(
        &mut self,
        window: &UserWindowInfo,
        windows: &[UserWindowInfo],
        area: ChromeRect,
        p: &mut ProtocolBridge,
    ) {
        let task = window.task_id;
        let incarnation =
            |z: &Zoomed, w: &UserWindowInfo| z.task_id == w.task_id && z.generation == w.generation;
        self.zoomed
            .retain(|z| windows.iter().any(|w| incarnation(z, w)));
        self.needs_full_redraw = true;
        let now = Geometry::of(window);
        if let Some(at) = self.zoomed.iter().position(|z| incarnation(z, window)) {
            let zoomed = self.zoomed.swap_remove(at);
            if window.state == WINDOW_STATE_MAXIMIZED || now == zoomed.after {
                let to = zoomed.before;
                p.set_window_position(task, to.x, to.y);
                p.set_window_size(task, to.w, to.h);
                p.set_window_state(task, WINDOW_STATE_NORMAL);
                p.send_configure_for_task(task, to.w, to.h, ACTIVATED);
                return;
            }
        }
        let room = (area.w, area.h - TITLE_BAR_HEIGHT);
        let (w, h) = p.clamp_window_size(task, room.0, room.1);
        let (x, y, state, states) = match (w, h) == room {
            true => (
                area.x,
                area.y + TITLE_BAR_HEIGHT,
                WINDOW_STATE_MAXIMIZED,
                ACTIVATED | MAXIMIZED,
            ),
            false => (
                area.x + (room.0 - w).max(0) / 2,
                area.y + TITLE_BAR_HEIGHT + (room.1 - h).max(0) / 2,
                WINDOW_STATE_NORMAL,
                ACTIVATED,
            ),
        };
        let (w, h) = (w as u32, h as u32);
        p.set_window_position(task, x, y);
        p.set_window_size(task, w, h);
        p.set_window_state(task, state);
        p.send_configure_for_task(task, w, h, states);
        self.zoomed.push(Zoomed {
            task_id: task,
            generation: window.generation,
            before: now,
            after: Geometry { x, y, w, h },
        });
    }

    pub fn hit_test_content_area(&self, window: &UserWindowInfo) -> bool {
        self.mouse_x >= window.x
            && self.mouse_x < window.x + window.effective_width() as i32
            && self.mouse_y >= window.y
            && self.mouse_y < window.y + window.effective_height() as i32
    }

    pub fn hit_test_frame_area(&self, window: &UserWindowInfo) -> bool {
        let frame_y = window.y - TITLE_BAR_HEIGHT;
        self.mouse_x >= window.x
            && self.mouse_x < window.x + window.effective_width() as i32
            && self.mouse_y >= frame_y
            && self.mouse_y < window.y + window.effective_height() as i32
    }

    fn set_focused(&mut self, task_id: u32) {
        self.focused_task = task_id;
    }

    fn start_drag(&mut self, window: &UserWindowInfo) {
        self.dragging = true;
        self.drag_task = window.task_id;
        self.drag_offset_x = self.mouse_x - window.x;
        self.drag_offset_y = self.mouse_y - window.y;
    }

    fn stop_drag(&mut self) {
        self.dragging = false;
        self.drag_task = 0;
    }

    fn update_drag(&mut self, proto: Option<&mut ProtocolBridge>) {
        let new_x = self.mouse_x - self.drag_offset_x;
        let new_y = self.mouse_y - self.drag_offset_y;
        if let Some(p) = proto {
            p.set_window_position(self.drag_task, new_x, new_y);
        }
    }

    fn start_resize(&mut self, window: &UserWindowInfo, edges: ResizeEdge) {
        self.resizing = true;
        self.resize_task = window.task_id;
        self.resize_edges = edges;
        self.resize_grab_x = window.x;
        self.resize_grab_y = window.y;
        self.resize_grab_w = window.width;
        self.resize_grab_h = window.height;
        self.resize_grab_mouse_x = self.mouse_x;
        self.resize_grab_mouse_y = self.mouse_y;
    }

    fn update_resize(&mut self, proto: Option<&mut ProtocolBridge>) {
        let dx = self.mouse_x - self.resize_grab_mouse_x;
        let dy = self.mouse_y - self.resize_grab_mouse_y;

        let mut new_x = self.resize_grab_x;
        let mut new_y = self.resize_grab_y;
        let mut new_w = self.resize_grab_w as i32;
        let mut new_h = self.resize_grab_h as i32;

        if self.resize_edges.has_right() {
            new_w += dx;
        }
        if self.resize_edges.has_left() {
            new_w -= dx;
            new_x += dx;
        }
        if self.resize_edges.has_bottom() {
            new_h += dy;
        }
        if self.resize_edges.has_top() {
            new_h -= dy;
            new_y += dy;
        }

        // The corner held keeps still.
        let (w, h) = self.limit_size(proto.as_deref(), new_w, new_h);
        if self.resize_edges.has_left() {
            new_x -= w - new_w;
        }
        if self.resize_edges.has_top() {
            new_y -= h - new_h;
        }
        let (new_w, new_h) = (w, h);

        if let Some(p) = proto {
            p.set_window_position(self.resize_task, new_x, new_y);
            p.set_window_size(self.resize_task, new_w as u32, new_h as u32);

            // Every 100 ms, so the client draws during the drag and not only
            // at its end.
            let now = self.now_ms();
            if now.saturating_sub(self.resize_last_configure_ms) >= 100 {
                self.resize_last_configure_ms = now;
                p.send_configure_for_task(
                    self.resize_task,
                    new_w as u32,
                    new_h as u32,
                    ACTIVATED | RESIZING,
                );
            }
        }
    }

    fn limit_size(&self, proto: Option<&ProtocolBridge>, w: i32, h: i32) -> (i32, i32) {
        proto.map_or((w, h), |p| p.clamp_window_size(self.resize_task, w, h))
    }

    fn stop_resize(&mut self, proto: Option<&mut ProtocolBridge>) {
        if self.resize_task != 0 {
            let dx = self.mouse_x - self.resize_grab_mouse_x;
            let dy = self.mouse_y - self.resize_grab_mouse_y;
            let mut final_w = self.resize_grab_w as i32;
            let mut final_h = self.resize_grab_h as i32;
            if self.resize_edges.has_right() {
                final_w += dx;
            }
            if self.resize_edges.has_left() {
                final_w -= dx;
            }
            if self.resize_edges.has_bottom() {
                final_h += dy;
            }
            if self.resize_edges.has_top() {
                final_h -= dy;
            }
            let (final_w, final_h) = self.limit_size(proto.as_deref(), final_w, final_h);
            if let Some(p) = proto {
                p.send_configure_for_task(
                    self.resize_task,
                    final_w as u32,
                    final_h as u32,
                    ACTIVATED,
                );
            }
        }
        self.resizing = false;
        self.resize_task = 0;
        self.resize_edges = ResizeEdge::NONE;
    }

    /// What the pointer is over: a window's whole frame hides everything
    /// beneath it.
    pub fn resolve_cursor_hit(
        &self,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
        dock: &Dock,
        popover: Option<slopos_appkit::Rect>,
        screen_width: u32,
        bar: &MenuBar,
    ) -> CursorHit {
        let (mx, my) = (self.mouse_x, self.mouse_y);

        if let Some(rect) = popover {
            return match rect.contains(mx, my) {
                true => CursorHit::ui(CursorPart::Popover),
                false => CursorHit::ui(CursorPart::PopoverOutside),
            };
        }
        if MenuBar::covers(my) {
            return CursorHit::ui(CursorPart::MenuBar(bar.hit_test(screen_width, mx, my)));
        }
        if let Some(idx) = dock.hit_test(mx, my) {
            return CursorHit::ui(CursorPart::Dock(idx));
        }

        for i in (0..window_count as usize).rev() {
            let w = windows[i];
            if w.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            // Its shadow takes a resize, and a maximized window has none.
            if w.state != WINDOW_STATE_MAXIMIZED && !w.fixed_size {
                let edge = decorations::hit_test_resize_edge(
                    w.x,
                    w.y,
                    w.effective_width(),
                    w.effective_height(),
                    mx,
                    my,
                );
                if !edge.is_none() {
                    return CursorHit::window(CursorPart::ResizeEdge(edge), i, w.task_id);
                }
            }

            let frame_y = w.y - TITLE_BAR_HEIGHT;
            if let Some(button) = decorations::button_at(w.x, frame_y, mx, my) {
                return CursorHit::window(CursorPart::Button(button), i, w.task_id);
            }
            if self.hit_test_content_area(&w) {
                return CursorHit::window(CursorPart::Content, i, w.task_id);
            }
            if self.hit_test_frame_area(&w) {
                return CursorHit::window(CursorPart::TitleBar, i, w.task_id);
            }
        }

        CursorHit::ui(CursorPart::Desktop)
    }

    /// A grab's shape before what the pointer is over.
    pub fn cursor_shape_for(&self, hit: &CursorHit, windows: &[UserWindowInfo; MAX_WINDOWS]) -> u8 {
        if self.dragging {
            return CURSOR_SHAPE_GRABBING;
        }
        if self.resizing {
            return self.resize_edges.cursor_shape();
        }
        match hit.part {
            CursorPart::ResizeEdge(edge) => edge.cursor_shape(),
            CursorPart::TitleBar => CURSOR_SHAPE_GRAB,
            // A client shapes the pointer over its own content alone.
            CursorPart::Content => windows[hit.window_idx].cursor_shape,
            CursorPart::Button(_)
            | CursorPart::MenuBar(_)
            | CursorPart::Dock(_)
            | CursorPart::Popover
            | CursorPart::PopoverOutside
            | CursorPart::Desktop => CURSOR_SHAPE_DEFAULT,
        }
    }

    /// The window whose buttons show their glyphs: the one on top under the
    /// pointer, never one hidden beneath.
    pub fn buttons_hovered_task(
        &self,
        hit: &CursorHit,
        windows: &[UserWindowInfo; MAX_WINDOWS],
    ) -> u32 {
        if !hit.is_window() {
            return 0;
        }
        let w = &windows[hit.window_idx];
        let group = decorations::buttons_group(w.x, w.y - TITLE_BAR_HEIGHT);
        match group.contains(self.mouse_x, self.mouse_y) {
            true => hit.task_id,
            false => 0,
        }
    }

    fn dock_click(
        &mut self,
        idx: usize,
        dock: &Dock,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
        proto: &mut Option<&mut ProtocolBridge>,
    ) {
        let Some(entry) = dock.entry(idx) else {
            return;
        };

        if entry.running && entry.task_id != 0 {
            for i in 0..window_count as usize {
                if windows[i].task_id == entry.task_id {
                    if let Some(p) = proto.as_deref_mut() {
                        if windows[i].state == WINDOW_STATE_MINIMIZED {
                            p.set_window_state(entry.task_id, WINDOW_STATE_NORMAL);
                        }
                        p.raise_window(entry.task_id);
                    }
                    self.set_focused(entry.task_id);
                    self.needs_full_redraw = true;
                    return;
                }
            }
        }

        if entry.pinned {
            self.spawn_program(&entry.exec);
        }
    }

    /// With the session's environment, `HOME` among it for its loader. Its
    /// window takes focus once it maps.
    fn spawn_program(&mut self, path: &str) {
        let stdio = [
            process::clone_fd(0, 0),
            process::clone_fd(1, 1),
            process::clone_fd(2, 2),
        ];
        let env = process::environment();
        let envp: Vec<*const u8> = env.iter().map(|var| var.as_ptr().cast()).collect();
        let tid = process::spawn_path_with_env(
            path.as_bytes(),
            &[],
            &envp,
            TaskPriority::Normal,
            0,
            &stdio,
            0,
        );
        if tid <= 0 {
            log!("spawn failed for program");
        }
    }
}
