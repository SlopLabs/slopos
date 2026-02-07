//! SlopOS Compositor - Wayland-like userland compositor
//!
//! This compositor runs entirely in userland (Ring 3) and uses shared memory
//! buffers for all graphics operations. No kernel drawing calls - all rendering
//! is done with 100% safe Rust via the gfx library.
//!
//! Architecture:
//! - Compositor allocates an output buffer via shared memory
//! - Clients allocate surface buffers via shared memory (Phase 4)
//! - Compositor composites all windows to output buffer
//! - Compositor draws chrome (title bars, taskbar, cursor)
//! - Compositor presents output buffer via sys_fb_flip()

use core::ffi::c_void;

use crate::gfx::{self, DamageRect, DamageTracker, DrawBuffer, DrawTarget, PixelFormat, rgb};
use crate::program_registry;
use crate::syscall::{
    CachedShmMapping, DisplayInfo, ShmBuffer, UserWindowInfo, core as sys_core, input, memory,
    process, tty, window,
};
use crate::ui_utils;

use crate::theme::*;

// Window placeholder colors (until clients migrate to shared memory)
const COLOR_WINDOW_PLACEHOLDER: u32 = rgb(0x20, 0x20, 0x30);

const MAX_WINDOWS: usize = 32;

/// Cache entry for a mapped client surface
struct ClientSurfaceEntry {
    task_id: u32,
    token: u32,
    mapping: Option<CachedShmMapping>,
}

impl ClientSurfaceEntry {
    const fn empty() -> Self {
        Self {
            task_id: 0,
            token: 0,
            mapping: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.task_id == 0 && self.mapping.is_none()
    }

    fn matches(&self, task_id: u32, token: u32) -> bool {
        self.task_id == task_id && self.token == token && self.mapping.is_some()
    }
}

/// Cache of mapped client surfaces (100% safe - no raw pointers)
struct ClientSurfaceCache {
    entries: [ClientSurfaceEntry; MAX_WINDOWS],
}

impl ClientSurfaceCache {
    fn new() -> Self {
        // Can't use const fn with Option initialization, so use Default-style init
        Self {
            entries: core::array::from_fn(|_| ClientSurfaceEntry::empty()),
        }
    }

    /// Get the index of a cached mapping for the given task/token, or create one.
    /// Returns the index into entries array, or None if mapping failed.
    fn get_or_create_index(
        &mut self,
        task_id: u32,
        token: u32,
        buffer_size: usize,
    ) -> Option<usize> {
        if token == 0 {
            return None;
        }

        // Check if we already have this mapping
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.matches(task_id, token) {
                return Some(i);
            }
        }

        // Need to create a new mapping
        let mapping = CachedShmMapping::map_readonly(token, buffer_size)?;

        // Find a slot to store the mapping
        for (i, entry) in self.entries.iter_mut().enumerate() {
            if entry.is_empty() {
                *entry = ClientSurfaceEntry {
                    task_id,
                    token,
                    mapping: Some(mapping),
                };
                return Some(i);
            }
        }

        // No slot available
        None
    }

    /// Get a slice view of the cached buffer at the given index.
    fn get_slice(&self, index: usize) -> Option<&[u8]> {
        self.entries
            .get(index)?
            .mapping
            .as_ref()
            .map(|m| m.as_slice())
    }

    /// Unmaps and clears cached client surface mappings for entries whose windows no longer exist.
    ///
    /// Iterates the cache and for each entry with a nonzero task id checks whether that task id
    /// is present in the provided window list (first `window_count` entries of `windows`).
    /// If the task id is not found, the entry's shared-memory mapping (if any) is unmapped and
    /// the entry is reset to an empty state.
    ///
    /// # Parameters
    ///
    /// - `windows`: slice containing the current set of windows (length `MAX_WINDOWS`).
    /// - `window_count`: number of active windows to consider from `windows`.
    ///
    /// # Examples
    ///
    /// ```
    /// // Call cleanup_stale to ensure mappings for removed windows are released.
    /// let mut cache = ClientSurfaceCache::new();
    /// // Create an all-zero windows array (no active windows).
    /// let windows: [UserWindowInfo; MAX_WINDOWS] = unsafe { std::mem::zeroed() };
    /// cache.cleanup_stale(&windows, 0);
    /// ```
    fn cleanup_stale(&mut self, windows: &[UserWindowInfo; MAX_WINDOWS], window_count: u32) {
        for entry in &mut self.entries {
            if entry.task_id == 0 {
                continue;
            }

            let still_exists =
                (0..window_count as usize).any(|i| windows[i].task_id == entry.task_id);

            if !still_exists {
                // Window no longer exists - unmap the shared memory and clear the entry
                if let Some(ref mapping) = entry.mapping {
                    unsafe {
                        memory::shm_unmap(mapping.vaddr());
                    }
                }
                *entry = ClientSurfaceEntry::empty();
            }
        }
    }
}

const WINDOW_STATE_NORMAL: u8 = 0;
const WINDOW_STATE_MINIMIZED: u8 = 1;

// Cursor constants
const CURSOR_SIZE: i32 = 9;

// Grace period before force-closing unresponsive apps after close request
const CLOSE_REQUEST_GRACE_MS: u64 = 1500;

struct StartMenuItem {
    label: &'static str,
    window_title: Option<&'static [u8]>,
    program_name: &'static [u8],
}

const START_MENU_ITEMS: [StartMenuItem; 3] = [
    StartMenuItem {
        label: "Files",
        window_title: Some(b"Files"),
        program_name: b"file_manager",
    },
    StartMenuItem {
        label: "Info",
        window_title: Some(b"Sysinfo"),
        program_name: b"sysinfo",
    },
    StartMenuItem {
        label: "Shell",
        window_title: Some(b"SlopOS Shell"),
        program_name: b"shell",
    },
];

/// Tracks state for conditional taskbar redraws
#[derive(Clone, Copy, PartialEq, Eq)]
struct TaskbarState {
    window_count: u32,
    focused_task: u32,
    window_states: u32,
    start_menu_open: bool,
}

impl TaskbarState {
    const fn empty() -> Self {
        Self {
            window_count: 0,
            focused_task: 0,
            window_states: 0,
            start_menu_open: false,
        }
    }

    fn from_windows(
        windows: &[UserWindowInfo; MAX_WINDOWS],
        count: u32,
        focused: u32,
        start_menu_open: bool,
    ) -> Self {
        let mut states = 0u32;
        for i in 0..count.min(32) as usize {
            if windows[i].state == WINDOW_STATE_MINIMIZED {
                states |= 1 << i;
            }
        }
        Self {
            window_count: count,
            focused_task: focused,
            window_states: states,
            start_menu_open,
        }
    }
}

/// Compositor output buffer backed by shared memory (100% safe - uses ShmBuffer)
struct CompositorOutput {
    buffer: ShmBuffer,
    width: u32,
    height: u32,
    pitch: usize,
    bytes_pp: u8,
}

impl CompositorOutput {
    fn new(fb: &DisplayInfo) -> Option<Self> {
        let pitch = fb.pitch as usize;
        let size = pitch * fb.height as usize;
        let bytes_pp = fb.bytes_per_pixel();

        if size == 0 || bytes_pp < 3 {
            return None;
        }

        let buffer = ShmBuffer::create(size).ok()?;

        Some(Self {
            buffer,
            width: fb.width,
            height: fb.height,
            pitch,
            bytes_pp,
        })
    }

    /// Get a DrawBuffer for this output (100% safe - no raw pointers)
    fn draw_buffer(&mut self) -> Option<DrawBuffer<'_>> {
        // ShmBuffer::as_mut_slice() is safe - bounds checked at creation
        let slice = self.buffer.as_mut_slice();
        DrawBuffer::new(slice, self.width, self.height, self.pitch, self.bytes_pp)
    }

    /// Present the output buffer to the framebuffer.
    ///
    /// When `damage` is empty this falls back to full-buffer present.
    fn present(&self, damage: &[DamageRect]) -> bool {
        window::fb_flip_damage(self.buffer.token(), damage) == 0
    }
}

/// Bounds of a window (for damage tracking)
#[derive(Copy, Clone, Default)]
struct WindowBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    visible: bool,
}

impl WindowBounds {
    fn from_window(w: &UserWindowInfo) -> Self {
        Self {
            x: w.x,
            y: w.y,
            width: w.width,
            height: w.height,
            visible: w.state != WINDOW_STATE_MINIMIZED,
        }
    }

    /// Get the full window rect including title bar
    fn to_damage_rect(&self) -> DamageRect {
        if !self.visible {
            return DamageRect::invalid();
        }
        DamageRect {
            x0: self.x,
            y0: self.y - TITLE_BAR_HEIGHT,
            x1: self.x + self.width as i32 - 1,
            y1: self.y + self.height as i32 - 1,
        }
    }
}

/// Maximum cursor positions to track per frame (for damage)
const MAX_CURSOR_TRAIL: usize = 16;
const FRAME_METRICS_WINDOW: usize = 128;

#[derive(Copy, Clone, Eq, PartialEq)]
enum RenderMode {
    Full,
    Partial,
}

struct FrameMetrics {
    full_redraw_frames: u64,
    partial_redraw_frames: u64,
    total_bytes_copied: u64,
    late_frames: u64,
    dropped_presents: u64,
    frame_times: [u64; FRAME_METRICS_WINDOW],
    frame_times_count: usize,
    frame_times_cursor: usize,
}

impl FrameMetrics {
    fn new() -> Self {
        Self {
            full_redraw_frames: 0,
            partial_redraw_frames: 0,
            total_bytes_copied: 0,
            late_frames: 0,
            dropped_presents: 0,
            frame_times: [0; FRAME_METRICS_WINDOW],
            frame_times_count: 0,
            frame_times_cursor: 0,
        }
    }

    fn record(
        &mut self,
        mode: RenderMode,
        bytes_copied: usize,
        frame_time_ms: u64,
        target_frame_ms: u64,
        present_ok: bool,
    ) {
        match mode {
            RenderMode::Full => self.full_redraw_frames = self.full_redraw_frames.saturating_add(1),
            RenderMode::Partial => {
                self.partial_redraw_frames = self.partial_redraw_frames.saturating_add(1)
            }
        }
        self.total_bytes_copied = self.total_bytes_copied.saturating_add(bytes_copied as u64);
        if frame_time_ms > target_frame_ms {
            self.late_frames = self.late_frames.saturating_add(1);
        }
        if !present_ok {
            self.dropped_presents = self.dropped_presents.saturating_add(1);
        }

        self.frame_times[self.frame_times_cursor] = frame_time_ms;
        self.frame_times_cursor = (self.frame_times_cursor + 1) % FRAME_METRICS_WINDOW;
        if self.frame_times_count < FRAME_METRICS_WINDOW {
            self.frame_times_count += 1;
        }
    }
}

struct WindowManager {
    windows: [UserWindowInfo; MAX_WINDOWS],
    window_count: u32,
    prev_windows: [UserWindowInfo; MAX_WINDOWS],
    prev_window_count: u32,
    focused_task: u32,
    dragging: bool,
    drag_task: u32,
    drag_offset_x: i32,
    drag_offset_y: i32,
    mouse_x: i32,
    mouse_y: i32,
    mouse_buttons: u8,
    mouse_buttons_prev: u8,
    start_menu_open: bool,
    first_frame: bool,
    prev_taskbar_state: TaskbarState,
    taskbar_needs_redraw: bool,
    // Force full redraw flag
    needs_full_redraw: bool,
    // Client surface cache for shared memory mappings
    surface_cache: ClientSurfaceCache,
    // Output buffer info for compositing
    output_width: u32,
    output_height: u32,
    output_bytes_pp: u8,
    output_pitch: usize,
    // Output damage accumulator for partial redraw
    output_damage: DamageTracker,
    // Previous frame's window bounds (for expose damage calculation)
    prev_window_bounds: [WindowBounds; MAX_WINDOWS],
    // Cursor positions visited this frame (for trail-free damage)
    cursor_trail: [(i32, i32); MAX_CURSOR_TRAIL],
    cursor_trail_count: usize,
    // Pending graceful close requests (task + deadline)
    pending_close_tasks: [u32; MAX_WINDOWS],
    pending_close_deadlines: [u64; MAX_WINDOWS],
    pending_close_count: usize,
}

impl WindowManager {
    /// Constructs a new WindowManager initialized to its default, empty runtime state.
    ///
    /// The returned manager has empty window lists and previous-window buffers, default input
    /// and dragging state, an initialized ClientSurfaceCache, default output damage tracker,
    /// and flags set to require an initial full redraw.
    ///
    /// # Examples
    ///
    /// ```
    /// let wm = WindowManager::new();
    /// assert_eq!(wm.window_count, 0);
    /// assert!(wm.first_frame);
    /// assert_eq!(wm.surface_cache.get_slice(0), None);
    /// ```
    fn new() -> Self {
        Self {
            windows: [UserWindowInfo::default(); MAX_WINDOWS],
            window_count: 0,
            prev_windows: [UserWindowInfo::default(); MAX_WINDOWS],
            prev_window_count: 0,
            focused_task: 0,
            dragging: false,
            drag_task: 0,
            drag_offset_x: 0,
            drag_offset_y: 0,
            mouse_x: 0,
            mouse_y: 0,
            mouse_buttons: 0,
            mouse_buttons_prev: 0,
            start_menu_open: false,
            first_frame: true,
            prev_taskbar_state: TaskbarState::empty(),
            taskbar_needs_redraw: true,
            needs_full_redraw: true,
            surface_cache: ClientSurfaceCache::new(),
            output_bytes_pp: 4,
            output_pitch: 0,
            output_damage: DamageTracker::new(),
            prev_window_bounds: [WindowBounds::default(); MAX_WINDOWS],
            cursor_trail: [(0, 0); MAX_CURSOR_TRAIL],
            cursor_trail_count: 0,
            pending_close_tasks: [0; MAX_WINDOWS],
            pending_close_deadlines: [0; MAX_WINDOWS],
            pending_close_count: 0,
            output_width: 0,
            output_height: 0,
        }
    }

    fn set_output_info(&mut self, width: u32, height: u32, bytes_pp: u8, pitch: usize) {
        self.output_width = width;
        self.output_height = height;
        self.output_bytes_pp = bytes_pp;
        self.output_pitch = pitch;
    }

    fn add_cursor_damage_at(&mut self, x: i32, y: i32) {
        self.output_damage.add_rect(x - 4, y - 4, x + 4, y + 4);
    }

    fn add_taskbar_damage(&mut self) {
        if self.output_width == 0 || self.output_height == 0 {
            return;
        }

        let fb_height = self.output_height as i32;
        self.output_damage.add_rect(
            0,
            fb_height - TASKBAR_HEIGHT,
            self.output_width as i32 - 1,
            fb_height - 1,
        );

        if self.start_menu_open {
            let menu_h = self.start_menu_height();
            self.output_damage.add_rect(
                self.start_menu_x(),
                self.start_menu_y(fb_height),
                self.start_menu_x() + START_MENU_WIDTH - 1,
                self.start_menu_y(fb_height) + menu_h - 1,
            );
        }
    }

    /// Update mouse state from kernel.
    /// Queries global position and button state directly (works even when focus is on another task).
    fn update_mouse(&mut self) {
        // Reset cursor trail for this frame
        self.cursor_trail_count = 0;

        // Record previous position for damage tracking
        let old_x = self.mouse_x;
        let old_y = self.mouse_y;

        // Always query global pointer position (works even when focus is on another task)
        let (new_x, new_y) = input::get_pointer_pos();
        if new_x != self.mouse_x || new_y != self.mouse_y {
            // Record trail for damage tracking
            if self.cursor_trail_count < MAX_CURSOR_TRAIL {
                self.cursor_trail[self.cursor_trail_count] = (old_x, old_y);
                self.cursor_trail_count += 1;
            }
            self.mouse_x = new_x;
            self.mouse_y = new_y;
        }

        // Always query global button state (works even when focus is on another task)
        self.mouse_buttons_prev = self.mouse_buttons;
        self.mouse_buttons = input::get_button_state();
    }

    /// Check if mouse was just clicked (press event)
    fn mouse_clicked(&self) -> bool {
        (self.mouse_buttons & 0x01) != 0 && (self.mouse_buttons_prev & 0x01) == 0
    }

    /// Check if mouse is currently pressed
    fn mouse_pressed(&self) -> bool {
        (self.mouse_buttons & 0x01) != 0
    }

    /// Refresh window list from kernel and accumulate damage
    fn refresh_windows(&mut self) {
        self.prev_windows = self.windows;
        self.prev_window_count = self.window_count;

        self.window_count = window::enumerate_windows(&mut self.windows) as u32;

        // Clean up stale surface mappings
        self.surface_cache
            .cleanup_stale(&self.windows, self.window_count);

        // Check if taskbar state changed
        let new_state = TaskbarState::from_windows(
            &self.windows,
            self.window_count,
            self.focused_task,
            self.start_menu_open,
        );
        if new_state != self.prev_taskbar_state {
            self.taskbar_needs_redraw = true;
            self.prev_taskbar_state = new_state;
        }

        // Clear output damage for this frame (accumulate fresh)
        self.output_damage.clear();

        // Accumulate damage from all sources
        for i in 0..self.window_count as usize {
            // Copy window data to avoid borrow conflicts
            let window = self.windows[i];
            let curr_bounds = WindowBounds::from_window(&window);

            // Find previous bounds for this window
            let prev_bounds = self.find_prev_bounds(window.task_id);

            // Check for window movement or visibility change - add both old and new positions as damage
            if let Some(old) = prev_bounds {
                if old.x != curr_bounds.x
                    || old.y != curr_bounds.y
                    || old.width != curr_bounds.width
                    || old.height != curr_bounds.height
                    || old.visible != curr_bounds.visible
                {
                    // Old position needs redraw (expose damage)
                    // Note: add_bounds_damage handles invisible bounds by returning early
                    self.add_bounds_damage(&old);
                    // New position needs redraw (if visible)
                    self.add_bounds_damage(&curr_bounds);
                }
            }

            // Store current bounds for next frame (even for minimized windows)
            self.prev_window_bounds[i] = curr_bounds;

            // Skip content damage for minimized windows (they're not drawn)
            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            // Add window's content damage (from client's sys_surface_damage calls)
            if window.is_dirty() {
                self.add_window_damage(&window);
            }
        }

        // Handle removed windows (expose damage)
        for i in 0..self.prev_window_count as usize {
            let prev = &self.prev_windows[i];
            if !self.window_exists(prev.task_id) {
                let old_bounds = self.prev_window_bounds[i];
                self.add_bounds_damage(&old_bounds);
            }
        }

        if self.taskbar_needs_redraw {
            self.add_taskbar_damage();
        }

        if self.cursor_trail_count > 0 {
            for i in 0..self.cursor_trail_count {
                let (x, y) = self.cursor_trail[i];
                self.add_cursor_damage_at(x, y);
            }
            self.add_cursor_damage_at(self.mouse_x, self.mouse_y);
        }
    }

    /// Find previous bounds for a window by task_id
    fn find_prev_bounds(&self, task_id: u32) -> Option<WindowBounds> {
        for i in 0..self.prev_window_count as usize {
            if self.prev_windows[i].task_id == task_id {
                return Some(self.prev_window_bounds[i]);
            }
        }
        None
    }

    /// Check if a window with given task_id exists in current frame
    fn window_exists(&self, task_id: u32) -> bool {
        (0..self.window_count as usize).any(|i| self.windows[i].task_id == task_id)
    }

    fn pending_close_index(&self, task_id: u32) -> Option<usize> {
        (0..self.pending_close_count).find(|&i| self.pending_close_tasks[i] == task_id)
    }

    fn remove_pending_close_at(&mut self, idx: usize) {
        if idx >= self.pending_close_count {
            return;
        }

        let last = self.pending_close_count - 1;
        self.pending_close_tasks[idx] = self.pending_close_tasks[last];
        self.pending_close_deadlines[idx] = self.pending_close_deadlines[last];
        self.pending_close_tasks[last] = 0;
        self.pending_close_deadlines[last] = 0;
        self.pending_close_count -= 1;
    }

    fn request_window_close(&mut self, task_id: u32) {
        if let Some(idx) = self.pending_close_index(task_id) {
            // Second click on an already-pending close: force-close immediately.
            let _ = process::terminate_task(task_id);
            self.remove_pending_close_at(idx);
            self.needs_full_redraw = true;
            return;
        }

        let now = sys_core::get_time_ms();
        let requested = input::request_close(task_id) == 0;

        if !requested || self.pending_close_count >= MAX_WINDOWS {
            // Fallback when graceful close cannot be queued.
            let _ = process::terminate_task(task_id);
            self.needs_full_redraw = true;
            return;
        }

        let idx = self.pending_close_count;
        self.pending_close_tasks[idx] = task_id;
        self.pending_close_deadlines[idx] = now.saturating_add(CLOSE_REQUEST_GRACE_MS);
        self.pending_close_count += 1;
        self.needs_full_redraw = true;
    }

    fn process_pending_close_requests(&mut self) {
        if self.pending_close_count == 0 {
            return;
        }

        let now = sys_core::get_time_ms();
        let mut i = 0usize;
        while i < self.pending_close_count {
            let task_id = self.pending_close_tasks[i];

            if !self.window_exists(task_id) {
                self.remove_pending_close_at(i);
                continue;
            }

            if now >= self.pending_close_deadlines[i] {
                let _ = process::terminate_task(task_id);
                self.remove_pending_close_at(i);
                self.needs_full_redraw = true;
                continue;
            }

            i += 1;
        }
    }

    /// Find a window by its title and return its task_id
    fn find_window_by_title(&self, title: &[u8]) -> Option<u32> {
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            // Compare title bytes (null-terminated)
            let title_len = title.iter().position(|&b| b == 0).unwrap_or(title.len());
            let win_title_len = window.title.iter().position(|&b| b == 0).unwrap_or(32);
            if title_len == win_title_len && &window.title[..win_title_len] == &title[..title_len] {
                return Some(window.task_id);
            }
        }
        None
    }

    /// Add window bounds (including title bar) to output damage
    fn add_bounds_damage(&mut self, bounds: &WindowBounds) {
        let rect = bounds.to_damage_rect();
        if rect.is_valid() {
            self.output_damage
                .add_rect(rect.x0, rect.y0, rect.x1, rect.y1);
        }
    }

    /// Add window's content damage (transformed to screen coords) to output damage
    fn add_window_damage(&mut self, window: &UserWindowInfo) {
        // If full damage (damage_count == u8::MAX), add entire window bounds
        if window.damage_count == u8::MAX {
            let bounds = WindowBounds::from_window(window);
            self.add_bounds_damage(&bounds);
            return;
        }

        // Transform each damage region from surface-local to screen coordinates
        for i in 0..window.damage_count as usize {
            let region = &window.damage_regions[i];
            if region.is_valid() {
                // Transform to screen coordinates (add window position)
                self.output_damage.add_rect(
                    window.x + region.x0,
                    window.y + region.y0,
                    window.x + region.x1,
                    window.y + region.y1,
                );
            }
        }
    }

    /// Process queued mouse input and perform high-level interactions (window dragging,
    /// title-bar actions, and taskbar clicks).
    ///
    /// This consumes click/drag events in priority order:
    /// - Ongoing window drags are continued or stopped based on current button state.
    /// - New clicks are tested against the taskbar, then window title bars (front-to-back),
    ///   handling close, minimize, raise/focus, and initiating drags.
    ///
    /// # Parameters
    ///
    /// - `fb_height`: Height of the framebuffer in pixels; used to detect clicks on the taskbar
    ///   area at the bottom of the screen.
    ///
    /// # Examples
    ///
    /// ```
    /// // Basic example: update internal mouse state first, then dispatch events.
    /// let mut wm = WindowManager::new();
    /// // ... populate wm.mouse_x/mouse_y/mouse_buttons as needed ...
    /// wm.handle_mouse_events(480);
    /// ```
    fn handle_mouse_events(&mut self, fb_height: i32) {
        let clicked = self.mouse_clicked();

        // Handle ongoing drag
        if self.dragging {
            if !self.mouse_pressed() {
                self.stop_drag();
            } else {
                self.update_drag();
            }
            return;
        }

        // Handle new clicks
        if clicked {
            if self.start_menu_open && self.hit_test_start_menu(fb_height) {
                if let Some(item_idx) = self.hit_test_start_menu_item(fb_height) {
                    self.activate_start_menu_item(item_idx);
                }
                return;
            }

            if self.start_menu_open
                && !self.hit_test_start_button(fb_height)
                && !self.hit_test_start_menu(fb_height)
            {
                self.start_menu_open = false;
                self.needs_full_redraw = true;
            }

            // Check taskbar clicks
            if self.mouse_y >= fb_height - TASKBAR_HEIGHT {
                self.handle_taskbar_click(fb_height);
                return;
            }

            // Check window title bar and content area clicks (front to back)
            for i in (0..self.window_count as usize).rev() {
                let window = self.windows[i];
                if window.state == WINDOW_STATE_MINIMIZED {
                    continue;
                }

                if self.hit_test_title_bar(&window) {
                    if self.hit_test_close_button(&window) {
                        self.close_window(window.task_id);
                        return;
                    }

                    if self.hit_test_minimize_button(&window) {
                        window::set_window_state(window.task_id, WINDOW_STATE_MINIMIZED);
                        return;
                    }

                    self.start_drag(&window);
                    window::raise_window(window.task_id);
                    tty::set_focus(window.task_id);
                    self.focused_task = window.task_id;
                    return;
                }

                // Check content area click - set pointer focus so client receives events
                if self.hit_test_content_area(&window) {
                    window::raise_window(window.task_id);
                    tty::set_focus(window.task_id);
                    // Set pointer focus with window offset for coordinate translation
                    input::set_pointer_focus_with_offset(window.task_id, window.x, window.y);
                    self.focused_task = window.task_id;
                    return;
                }
            }
        }
    }

    fn hit_test_content_area(&self, window: &UserWindowInfo) -> bool {
        self.mouse_x >= window.x
            && self.mouse_x < window.x + window.width as i32
            && self.mouse_y >= window.y
            && self.mouse_y < window.y + window.height as i32
    }

    fn hit_test_title_bar(&self, window: &UserWindowInfo) -> bool {
        let title_y = window.y - TITLE_BAR_HEIGHT;
        self.mouse_x >= window.x
            && self.mouse_x < window.x + window.width as i32
            && self.mouse_y >= title_y
            && self.mouse_y < window.y
    }

    fn hit_test_close_button(&self, window: &UserWindowInfo) -> bool {
        let button_x = window.x + window.width as i32 - BUTTON_SIZE - BUTTON_PADDING;
        let button_y = window.y - TITLE_BAR_HEIGHT + BUTTON_PADDING;
        self.mouse_x >= button_x
            && self.mouse_x < button_x + BUTTON_SIZE
            && self.mouse_y >= button_y
            && self.mouse_y < button_y + BUTTON_SIZE
    }

    fn hit_test_minimize_button(&self, window: &UserWindowInfo) -> bool {
        let button_x = window.x + window.width as i32 - (BUTTON_SIZE * 2) - (BUTTON_PADDING * 2);
        let button_y = window.y - TITLE_BAR_HEIGHT + BUTTON_PADDING;
        self.mouse_x >= button_x
            && self.mouse_x < button_x + BUTTON_SIZE
            && self.mouse_y >= button_y
            && self.mouse_y < button_y + BUTTON_SIZE
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

    fn update_drag(&mut self) {
        let new_x = self.mouse_x - self.drag_offset_x;
        let new_y = self.mouse_y - self.drag_offset_y;
        window::set_window_position(self.drag_task, new_x, new_y);
        self.needs_full_redraw = true;
    }

    /// Requests graceful close for the app task identified by `task_id`.
    ///
    /// This sends a close-request input event first (Redox/Orbital-style). If the app does not
    /// exit within a grace period, the compositor force-terminates it.
    ///
    /// # Examples
    ///
    /// ```
    /// // Given a mutable WindowManager instance `wm`
    /// wm.close_window(42);
    /// ```
    fn close_window(&mut self, task_id: u32) {
        self.request_window_close(task_id);
    }

    fn start_button_x(&self) -> i32 {
        TASKBAR_BUTTON_PADDING
    }

    fn app_buttons_start_x(&self) -> i32 {
        self.start_button_x() + START_BUTTON_WIDTH + START_APPS_GAP
    }

    fn start_button_y(&self, fb_height: i32) -> i32 {
        fb_height - TASKBAR_HEIGHT + TASKBAR_BUTTON_PADDING
    }

    fn start_button_height(&self) -> i32 {
        TASKBAR_HEIGHT - (TASKBAR_BUTTON_PADDING * 2)
    }

    fn start_menu_height(&self) -> i32 {
        (START_MENU_ITEMS.len() as i32 * START_MENU_ITEM_HEIGHT) + (START_MENU_PADDING * 2)
    }

    fn start_menu_x(&self) -> i32 {
        self.start_button_x()
    }

    fn start_menu_y(&self, fb_height: i32) -> i32 {
        self.start_button_y(fb_height) - self.start_menu_height() - TASKBAR_BUTTON_PADDING
    }

    fn hit_test_start_button(&self, fb_height: i32) -> bool {
        let btn_x = self.start_button_x();
        let btn_y = self.start_button_y(fb_height);
        let btn_h = self.start_button_height();
        self.mouse_x >= btn_x
            && self.mouse_x < btn_x + START_BUTTON_WIDTH
            && self.mouse_y >= btn_y
            && self.mouse_y < btn_y + btn_h
    }

    fn hit_test_start_menu(&self, fb_height: i32) -> bool {
        let menu_x = self.start_menu_x();
        let menu_y = self.start_menu_y(fb_height);
        let menu_h = self.start_menu_height();
        self.mouse_x >= menu_x
            && self.mouse_x < menu_x + START_MENU_WIDTH
            && self.mouse_y >= menu_y
            && self.mouse_y < menu_y + menu_h
    }

    fn hit_test_start_menu_item(&self, fb_height: i32) -> Option<usize> {
        if !self.start_menu_open || !self.hit_test_start_menu(fb_height) {
            return None;
        }

        let menu_y = self.start_menu_y(fb_height) + START_MENU_PADDING;
        let rel_y = self.mouse_y - menu_y;
        if rel_y < 0 {
            return None;
        }
        let idx = (rel_y / START_MENU_ITEM_HEIGHT) as usize;
        if idx < START_MENU_ITEMS.len() {
            Some(idx)
        } else {
            None
        }
    }

    fn launch_or_raise_program(&mut self, window_title: Option<&[u8]>, program_name: &[u8]) {
        if let Some(title) = window_title {
            if let Some(task_id) = self.find_window_by_title(title) {
                window::raise_window(task_id);
                tty::set_focus(task_id);
                self.focused_task = task_id;
                return;
            }
        }

        if let Some(spec) = program_registry::resolve_program(program_name) {
            process::spawn_path_with_attrs(spec.path, spec.priority, spec.flags);
        }
    }

    fn activate_start_menu_item(&mut self, item_idx: usize) {
        if let Some(item) = START_MENU_ITEMS.get(item_idx) {
            self.launch_or_raise_program(item.window_title, item.program_name);
            self.start_menu_open = false;
            self.needs_full_redraw = true;
        }
    }

    /// Handle a mouse click on the taskbar, toggling start menu and minimizing/restoring windows.
    ///
    /// # Examples
    ///
    /// ```
    /// // Assume `wm` is a prepared WindowManager with valid taskbar layout and mouse_x/mouse_y set.
    /// let mut wm = WindowManager::new();
    /// // Position mouse over the Start button and simulate click handling.
    /// wm.mouse_x = 4; // within TASKBAR_BUTTON_PADDING
    /// wm.handle_taskbar_click(480);
    /// assert!(wm.needs_full_redraw);
    /// ```
    fn handle_taskbar_click(&mut self, fb_height: i32) {
        if self.hit_test_start_button(fb_height) {
            self.start_menu_open = !self.start_menu_open;
            self.needs_full_redraw = true;
            return;
        }

        if let Some(item_idx) = self.hit_test_start_menu_item(fb_height) {
            self.activate_start_menu_item(item_idx);
            return;
        }

        let mut x = self.app_buttons_start_x();
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            let button_width = TASKBAR_BUTTON_WIDTH;

            if self.mouse_x >= x && self.mouse_x < x + button_width {
                let new_state = if window.state == WINDOW_STATE_MINIMIZED {
                    WINDOW_STATE_NORMAL
                } else {
                    WINDOW_STATE_MINIMIZED
                };
                window::set_window_state(window.task_id, new_state);
                if new_state == WINDOW_STATE_NORMAL {
                    window::raise_window(window.task_id);
                    tty::set_focus(window.task_id);
                    self.focused_task = window.task_id;
                }
                self.needs_full_redraw = true;
                return;
            }

            x += button_width + TASKBAR_BUTTON_PADDING;
        }

        if self.start_menu_open {
            self.start_menu_open = false;
            self.needs_full_redraw = true;
        }
    }

    /// Draw window title bar to the output buffer
    fn draw_title_bar(&self, buf: &mut DrawBuffer, window: &UserWindowInfo) {
        let focused = window.task_id == self.focused_task;
        let color = if focused {
            COLOR_TITLE_BAR_FOCUSED
        } else {
            COLOR_TITLE_BAR
        };

        let title_y = window.y - TITLE_BAR_HEIGHT;

        // Title bar background
        gfx::fill_rect(
            buf,
            window.x,
            title_y,
            window.width as i32,
            TITLE_BAR_HEIGHT,
            color,
        );

        // Window title text
        let title = title_to_str(&window.title);
        gfx::font::draw_string(buf, window.x + 8, title_y + 4, title, COLOR_TEXT, color);

        // Close button (X)
        ui_utils::draw_button(
            buf,
            window.x + window.width as i32 - BUTTON_SIZE - BUTTON_PADDING,
            title_y + BUTTON_PADDING,
            BUTTON_SIZE,
            "X",
            self.hit_test_close_button(window),
            true,
        );

        // Minimize button (_)
        ui_utils::draw_button(
            buf,
            window.x + window.width as i32 - (BUTTON_SIZE * 2) - (BUTTON_PADDING * 2),
            title_y + BUTTON_PADDING,
            BUTTON_SIZE,
            "_",
            self.hit_test_minimize_button(window),
            false,
        );
    }

    /// Renders the taskbar into the provided draw buffer, including Start and window buttons.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// let wm = WindowManager::new();
    /// // Obtain a DrawBuffer from a CompositorOutput in real usage:
    /// // let mut buf = output.draw_buffer();
    /// // For this example we assume `buf` is a valid DrawBuffer:
    /// let mut buf: DrawBuffer = unimplemented!();
    /// wm.draw_taskbar(&mut buf);
    /// ```
    fn draw_taskbar(&self, buf: &mut DrawBuffer) {
        let taskbar_y = buf.height() as i32 - TASKBAR_HEIGHT;

        // Taskbar background
        gfx::fill_rect(
            buf,
            0,
            taskbar_y,
            buf.width() as i32,
            TASKBAR_HEIGHT,
            COLOR_TASKBAR,
        );

        // Draw Start button
        let start_btn_x = self.start_button_x();
        let btn_y = taskbar_y + TASKBAR_BUTTON_PADDING;
        let btn_height = TASKBAR_HEIGHT - (TASKBAR_BUTTON_PADDING * 2);

        let start_hover = self.mouse_x >= start_btn_x
            && self.mouse_x < start_btn_x + START_BUTTON_WIDTH
            && self.mouse_y >= btn_y
            && self.mouse_y < btn_y + btn_height;

        let start_color = if self.start_menu_open || start_hover {
            COLOR_BUTTON_HOVER
        } else {
            COLOR_BUTTON
        };

        gfx::fill_rect(
            buf,
            start_btn_x,
            btn_y,
            START_BUTTON_WIDTH,
            btn_height,
            start_color,
        );
        gfx::font::draw_string(
            buf,
            start_btn_x + 4,
            btn_y + 4,
            "Start",
            COLOR_TEXT,
            start_color,
        );

        // Draw a subtle separator/gap between Start section and app buttons
        let separator_x = self.app_buttons_start_x() - (START_APPS_GAP / 2);
        gfx::fill_rect(
            buf,
            separator_x,
            taskbar_y + 2,
            1,
            TASKBAR_HEIGHT - 4,
            COLOR_BUTTON_HOVER,
        );

        // Draw app buttons
        let mut x = self.app_buttons_start_x();
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            let focused = window.task_id == self.focused_task;
            let btn_color = if focused {
                COLOR_BUTTON_HOVER
            } else {
                COLOR_BUTTON
            };

            let btn_y = taskbar_y + TASKBAR_BUTTON_PADDING;
            let btn_height = TASKBAR_HEIGHT - (TASKBAR_BUTTON_PADDING * 2);

            gfx::fill_rect(buf, x, btn_y, TASKBAR_BUTTON_WIDTH, btn_height, btn_color);

            // Button text (truncated to fit)
            let title = title_to_str(&window.title);
            let max_chars = (TASKBAR_BUTTON_WIDTH / 8 - 1) as usize;
            let truncated: &str = if title.len() > max_chars {
                &title[..max_chars]
            } else {
                title
            };
            gfx::font::draw_string(buf, x + 4, btn_y + 4, truncated, COLOR_TEXT, btn_color);

            x += TASKBAR_BUTTON_WIDTH + TASKBAR_BUTTON_PADDING;
        }
    }

    fn draw_start_menu(&self, buf: &mut DrawBuffer) {
        if !self.start_menu_open {
            return;
        }

        let fb_height = buf.height() as i32;
        let menu_x = self.start_menu_x();
        let menu_y = self.start_menu_y(fb_height);
        let menu_h = self.start_menu_height();

        gfx::fill_rect(
            buf,
            menu_x,
            menu_y,
            START_MENU_WIDTH,
            menu_h,
            COLOR_START_MENU_BG,
        );

        for (idx, item) in START_MENU_ITEMS.iter().enumerate() {
            let item_y = menu_y + START_MENU_PADDING + (idx as i32 * START_MENU_ITEM_HEIGHT);
            let item_hover = self.hit_test_start_menu_item(fb_height) == Some(idx);
            let item_color = if item_hover {
                COLOR_BUTTON_HOVER
            } else {
                COLOR_START_MENU_BG
            };

            gfx::fill_rect(
                buf,
                menu_x + START_MENU_PADDING,
                item_y,
                START_MENU_WIDTH - (START_MENU_PADDING * 2),
                START_MENU_ITEM_HEIGHT,
                item_color,
            );
            gfx::font::draw_string(
                buf,
                menu_x + START_MENU_PADDING + 4,
                item_y + 6,
                item.label,
                COLOR_TEXT,
                item_color,
            );
        }
    }

    /// Draw mouse cursor to the output buffer
    fn draw_cursor(&self, buf: &mut DrawBuffer) {
        // Simple crosshair cursor
        let mx = self.mouse_x;
        let my = self.mouse_y;

        // Horizontal line
        gfx::fill_rect(buf, mx - 4, my, CURSOR_SIZE, 1, COLOR_CURSOR);

        // Vertical line
        gfx::fill_rect(buf, mx, my - 4, 1, CURSOR_SIZE, COLOR_CURSOR);
    }

    /// Draw window content from client's shared memory surface (100% safe)
    fn draw_window_content(&mut self, buf: &mut DrawBuffer, window: &UserWindowInfo) {
        let full_clip = DamageRect {
            x0: window.x,
            y0: window.y,
            x1: window.x + window.width as i32 - 1,
            y1: window.y + window.height as i32 - 1,
        };
        self.draw_window_content_clipped(buf, window, &full_clip);
    }

    fn draw_window_content_clipped(
        &mut self,
        buf: &mut DrawBuffer,
        window: &UserWindowInfo,
        clip: &DamageRect,
    ) {
        // Calculate buffer size for this surface
        let bytes_pp = self.output_bytes_pp as usize;
        let src_pitch = (window.width as usize) * bytes_pp;
        let buffer_size = src_pitch * (window.height as usize);

        // Try to get or create a cached mapping for the client's surface
        let cache_index = match self.surface_cache.get_or_create_index(
            window.task_id,
            window.shm_token,
            buffer_size,
        ) {
            Some(idx) => idx,
            None => {
                // No shared memory surface - draw placeholder
                self.draw_window_placeholder(buf, window);
                return;
            }
        };

        // Get the cached buffer slice (100% safe - bounds checked)
        let src_data = match self.surface_cache.get_slice(cache_index) {
            Some(slice) => slice,
            None => {
                self.draw_window_placeholder(buf, window);
                return;
            }
        };

        let dst_pitch = self.output_pitch;
        let buf_width = buf.width() as i32;
        let buf_height = buf.height() as i32;

        let window_rect = DamageRect {
            x0: window.x,
            y0: window.y,
            x1: window.x + window.width as i32 - 1,
            y1: window.y + window.height as i32 - 1,
        };
        let Some(draw_rect) = intersect_rect(clip, &window_rect) else {
            return;
        };

        // Clip to buffer bounds
        let x0 = draw_rect.x0.max(0);
        let y0 = draw_rect.y0.max(0);
        let x1 = (draw_rect.x1 + 1).min(buf_width);
        let y1 = (draw_rect.y1 + 1).min(buf_height);

        if x0 >= x1 || y0 >= y1 {
            return;
        }

        // Calculate offsets into source buffer
        let src_start_x = (x0 - window.x) as usize;
        let src_start_y = (y0 - window.y) as usize;

        // Get destination buffer data
        let dst_data = buf.data_mut();

        // Copy each row from client surface to output buffer (100% safe - slice ops only)
        for row in 0..(y1 - y0) as usize {
            let src_row = src_start_y + row;
            let dst_row = (y0 as usize) + row;

            let src_off = src_row * src_pitch + src_start_x * bytes_pp;
            let dst_off = dst_row * dst_pitch + (x0 as usize) * bytes_pp;
            let copy_width = ((x1 - x0) as usize) * bytes_pp;

            // Safe slice operations with bounds checking
            let src_end = src_off + copy_width;
            let dst_end = dst_off + copy_width;

            if src_end <= src_data.len() && dst_end <= dst_data.len() {
                dst_data[dst_off..dst_end].copy_from_slice(&src_data[src_off..src_end]);
            }
        }
    }

    fn draw_partial_region(&mut self, buf: &mut DrawBuffer, damage: &DamageRect) {
        if !damage.is_valid() {
            return;
        }

        gfx::fill_rect(
            buf,
            damage.x0,
            damage.y0,
            damage.x1 - damage.x0 + 1,
            damage.y1 - damage.y0 + 1,
            COLOR_BACKGROUND,
        );

        let window_count = self.window_count as usize;
        for i in 0..window_count {
            let window = self.windows[i];
            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            let content_rect = DamageRect {
                x0: window.x,
                y0: window.y,
                x1: window.x + window.width as i32 - 1,
                y1: window.y + window.height as i32 - 1,
            };
            if intersect_rect(damage, &content_rect).is_some() {
                self.draw_window_content_clipped(buf, &window, damage);
            }

            let title_rect = DamageRect {
                x0: window.x,
                y0: window.y - TITLE_BAR_HEIGHT,
                x1: window.x + window.width as i32 - 1,
                y1: window.y - 1,
            };
            if intersect_rect(damage, &title_rect).is_some() {
                self.draw_title_bar(buf, &window);
            }
        }

        let taskbar_y = buf.height() as i32 - TASKBAR_HEIGHT;
        let taskbar_rect = DamageRect {
            x0: 0,
            y0: taskbar_y,
            x1: buf.width() as i32 - 1,
            y1: buf.height() as i32 - 1,
        };
        if intersect_rect(damage, &taskbar_rect).is_some() {
            self.draw_taskbar(buf);
        }

        if self.start_menu_open {
            let menu_h = self.start_menu_height();
            let menu_rect = DamageRect {
                x0: self.start_menu_x(),
                y0: self.start_menu_y(buf.height() as i32),
                x1: self.start_menu_x() + START_MENU_WIDTH - 1,
                y1: self.start_menu_y(buf.height() as i32) + menu_h - 1,
            };
            if intersect_rect(damage, &menu_rect).is_some() {
                self.draw_start_menu(buf);
            }
        }

        let cursor_rect = DamageRect {
            x0: self.mouse_x - 4,
            y0: self.mouse_y - 4,
            x1: self.mouse_x + 4,
            y1: self.mouse_y + 4,
        };
        if intersect_rect(damage, &cursor_rect).is_some() {
            self.draw_cursor(buf);
        }
    }

    /// Render a simple placeholder for a window's content area when the client's surface is not available.
    ///
    /// This draws a filled rectangle using COLOR_WINDOW_PLACEHOLDER, an outline using COLOR_TITLE_BAR,
    /// and a short informational text centered vertically inside the window bounds.
    ///
    /// # Parameters
    ///
    /// - `buf`: the draw target where the placeholder will be rendered.
    /// - `window`: the window geometry and position; `x`, `y`, `width`, and `height` determine the placeholder area.
    ///
    /// # Examples
    ///
    /// ```
    /// // Construct minimal buffer and window info (types shown here are from the compositor crate).
    /// let mut buf = DrawBuffer::default();
    /// let window = UserWindowInfo { x: 20, y: 16, width: 200, height: 120, ..Default::default() };
    /// let wm = WindowManager::new();
    /// // Render a placeholder for the window into the buffer.
    /// wm.draw_window_placeholder(&mut buf, &window);
    /// ```
    fn draw_window_placeholder(&self, buf: &mut DrawBuffer, window: &UserWindowInfo) {
        // Draw a colored rectangle as placeholder for window content
        gfx::fill_rect(
            buf,
            window.x,
            window.y,
            window.width as i32,
            window.height as i32,
            COLOR_WINDOW_PLACEHOLDER,
        );

        // Draw a border to show window bounds
        gfx::draw_rect(
            buf,
            window.x,
            window.y,
            window.width as i32,
            window.height as i32,
            COLOR_TITLE_BAR,
        );

        // Draw placeholder text
        let text = "Window content pending migration";
        let text_x = window.x + 10;
        let text_y = window.y + window.height as i32 / 2 - 8;
        gfx::font::draw_string(
            buf,
            text_x,
            text_y,
            text,
            COLOR_TEXT,
            COLOR_WINDOW_PLACEHOLDER,
        );
    }

    /// Perform a full compositor render pass into the given draw buffer.
    ///
    /// Renders either a full frame or only damaged regions depending on tracked output damage.
    /// Full redraw is used as a correctness fallback for first frame, explicit full redraw,
    /// and full/unknown damage states.
    ///
    /// # Parameters
    ///
    /// - `buf`: The output draw buffer to render into.
    ///
    /// # Examples
    ///
    /// ```
    /// // Prepare compositor state and an output buffer (types and construction depend on context).
    /// let mut wm = WindowManager::new();
    /// let mut draw_buf = /* obtain a DrawBuffer for the output framebuffer */ unimplemented!();
    /// // Render the current scene into the output buffer.
    /// wm.render(&mut draw_buf);
    /// ```
    fn render(&mut self, buf: &mut DrawBuffer) -> RenderMode {
        let force_full =
            self.first_frame || self.needs_full_redraw || self.output_damage.is_full_damage();

        let mode = if force_full {
            // Full redraw fallback path
            buf.clear(COLOR_BACKGROUND);

            let window_count = self.window_count as usize;
            for i in 0..window_count {
                let window = self.windows[i];
                if window.state == WINDOW_STATE_MINIMIZED {
                    continue;
                }
                self.draw_window_content(buf, &window);
                self.draw_title_bar(buf, &window);
            }

            self.draw_taskbar(buf);
            self.draw_start_menu(buf);
            self.draw_cursor(buf);
            RenderMode::Full
        } else {
            let mut damage_regions = [DamageRect::invalid(); 8];
            let mut damage_count = 0usize;
            for rect in self.output_damage.regions() {
                if damage_count >= damage_regions.len() {
                    break;
                }
                damage_regions[damage_count] = *rect;
                damage_count += 1;
            }

            if damage_count == 0 {
                RenderMode::Partial
            } else {
                for rect in &damage_regions[..damage_count] {
                    self.draw_partial_region(buf, rect);
                }
                RenderMode::Partial
            }
        };

        // Reset redraw flags
        self.needs_full_redraw = false;
        self.first_frame = false;
        self.taskbar_needs_redraw = false;
        mode
    }

    /// Check if any redraw is needed
    fn needs_redraw(&self) -> bool {
        self.first_frame
            || self.needs_full_redraw
            || self.taskbar_needs_redraw
            || self.output_damage.is_dirty()
    }
}

/// Convert UTF-8 title array to &str (now 100% safe - no unsafe needed)
///
/// With the ABI change to use `[u8; 32]` instead of `[c_char; 32]`,
/// this function is now completely safe Rust.
fn title_to_str(title: &[u8; 32]) -> &str {
    // Find the null terminator
    let len = title.iter().position(|&b| b == 0).unwrap_or(32);

    if len == 0 {
        return "";
    }

    // Direct UTF-8 validation - no unsafe needed!
    core::str::from_utf8(&title[..len]).unwrap_or("<invalid>")
}

fn intersect_rect(a: &DamageRect, b: &DamageRect) -> Option<DamageRect> {
    let x0 = a.x0.max(b.x0);
    let y0 = a.y0.max(b.y0);
    let x1 = a.x1.min(b.x1);
    let y1 = a.y1.min(b.y1);
    if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    }
}

fn estimate_present_bytes(
    width: u32,
    height: u32,
    bytes_pp: u8,
    pitch: usize,
    mode: RenderMode,
    damage: &[DamageRect],
) -> usize {
    if mode == RenderMode::Full || damage.is_empty() {
        return pitch.saturating_mul(height as usize);
    }

    let mut total = 0usize;
    for rect in damage {
        let clipped = rect.clip(width as i32, height as i32);
        if !clipped.is_valid() {
            continue;
        }
        let w = (clipped.x1 - clipped.x0 + 1) as usize;
        let h = (clipped.y1 - clipped.y0 + 1) as usize;
        total = total.saturating_add(w.saturating_mul(h).saturating_mul(bytes_pp as usize));
    }
    total
}

pub fn compositor_user_main(_arg: *mut c_void) {
    tty::write(b"COMPOSITOR: starting\n");
    let mut wm = WindowManager::new();
    let mut fb_info = DisplayInfo::default();

    if window::fb_info(&mut fb_info) < 0 {
        tty::write(b"COMPOSITOR: fb_info failed\n");
        loop {
            sys_core::yield_now();
        }
    }
    tty::write(b"COMPOSITOR: fb_info ok\n");

    let mut output = match CompositorOutput::new(&fb_info) {
        Some(out) => out,
        None => {
            tty::write(b"COMPOSITOR: output alloc failed\n");
            loop {
                sys_core::yield_now();
            }
        }
    };
    tty::write(b"COMPOSITOR: output allocated\n");

    wm.set_output_info(output.width, output.height, output.bytes_pp, output.pitch);

    let pixel_format = if fb_info.format.is_bgr_order() {
        PixelFormat::Bgra
    } else {
        PixelFormat::Rgba
    };

    const TARGET_FRAME_MS: u64 = 16;
    let mut frame_count: u32 = 0;
    let mut metrics = FrameMetrics::new();

    loop {
        let frame_start_ms = sys_core::get_time_ms();

        input::drain_queue();

        wm.update_mouse();
        wm.refresh_windows();
        wm.process_pending_close_requests();
        wm.handle_mouse_events(fb_info.height as i32);

        if wm.needs_redraw() {
            let mut mode = RenderMode::Full;
            if let Some(mut buf) = output.draw_buffer() {
                buf.set_pixel_format(pixel_format);
                mode = wm.render(&mut buf);
            }

            let mut present_damage = [DamageRect::invalid(); 8];
            let mut present_damage_count = 0usize;
            if mode == RenderMode::Partial {
                for rect in wm.output_damage.regions() {
                    if present_damage_count >= present_damage.len() {
                        break;
                    }
                    present_damage[present_damage_count] = *rect;
                    present_damage_count += 1;
                }
            }

            let damage_slice = if mode == RenderMode::Partial {
                &present_damage[..present_damage_count]
            } else {
                &[]
            };

            let flip_result = output.present(damage_slice);
            if frame_count < 3 {
                if flip_result {
                    tty::write(b"COMPOSITOR: fb_flip ok\n");
                } else {
                    tty::write(b"COMPOSITOR: fb_flip FAILED\n");
                }
            }
            frame_count = frame_count.saturating_add(1);
            if flip_result {
                let present_time = sys_core::get_time_ms();
                window::mark_frames_done(present_time);
            }

            let frame_end_ms = sys_core::get_time_ms();
            let frame_time = frame_end_ms.saturating_sub(frame_start_ms);
            let copied = estimate_present_bytes(
                output.width,
                output.height,
                output.bytes_pp,
                output.pitch,
                mode,
                damage_slice,
            );
            metrics.record(mode, copied, frame_time, TARGET_FRAME_MS, flip_result);
        }

        let frame_end_ms = sys_core::get_time_ms();
        let frame_time = frame_end_ms.saturating_sub(frame_start_ms);
        if frame_time < TARGET_FRAME_MS {
            sys_core::sleep_ms((TARGET_FRAME_MS - frame_time) as u32);
        }
    }
}
