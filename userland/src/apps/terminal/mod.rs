//! Terminal: a compositor client holding a pty's master, with `/bin/shell` on
//! its slave. One thread, as `ProtocolHandle` is `!Send`.

pub mod grid;
pub mod input;
mod render;
mod surface;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use slopos_abi::task::TaskPriority;

use crate::ring::{Ring, slopfut};
use crate::syscall::{ShmBuffer, fs, process, tty};

use grid::TerminalGrid;
use input::{CompositorEvent, KeyAction, KeyBytes, MouseEventKind, PointerState, Selection};
use slopos_terminal_core::damage::{CellDamage, DamageHistory};

const OPEN_COLS: i32 = 80;
const OPEN_ROWS: i32 = 30;
/// As many as `SoftSurface` cycles, so a recycled slot's age always resolves
/// against a recorded frame.
const SURFACE_BUFFERS: usize = 2;
const BLINK_INTERVAL: Duration = Duration::from_millis(500);

const MASTER_READ_CHUNK: usize = 1024;
/// Per loop turn, so output cannot starve input.
const MASTER_READ_BUDGET: usize = 4 * 1024;
const MASTER_WRITE_QUEUE_CAP: usize = 64 * 1024;
const MASTER_WRITE_BUDGET: usize = 4 * 1024;

pub fn terminal_user_main() {
    use slopos_windowing::connection;

    // Its stderr goes nowhere visible.
    std::panic::set_hook(Box::new(|info| {
        let _ = tty::write(b"terminal: PANIC: ");
        let msg = std::format!("{info}\n");
        let _ = tty::write(msg.as_bytes());
    }));

    let handle = connection::connect().unwrap_or_else(|e| panic!("{e}"));
    surface::init_handle(handle.clone());

    let (cw, ch) = render::cell_size();
    let width = OPEN_COLS * cw + 2 * render::MARGIN;
    let height = OPEN_ROWS * ch + 2 * render::MARGIN;
    if !surface::init(width, height) {
        panic!("terminal: surface init failed");
    }
    surface::set_title("Terminal");
    surface::set_app_id("org.slopos.terminal");

    let (master_owned, _slave_num) = match process::openpty() {
        Ok(pair) => pair,
        Err(_) => {
            let _ = tty::write(b"terminal: openpty failed\n");
            return;
        }
    };
    let master_fd = master_owned.into_raw();

    let slave_fd = match fs::ioctl_tiocgptpeer(master_fd) {
        Ok(fd) => fd.into_raw(),
        Err(_) => {
            let _ = tty::write(b"terminal: tiocgptpeer failed\n");
            let _ = fs::close_fd_raw(master_fd);
            return;
        }
    };

    // The shell needs a size before the first configure arrives.
    let cols = render::cells_in(width, cw, grid::MAX_COLS);
    let rows = render::cells_in(height, ch, grid::MAX_ROWS);
    push_winsize(master_fd, rows, cols);

    spawn_shell_on_slave(slave_fd);

    // Only the shell's copy left, so its exit reaches the master as end of file.
    let _ = fs::close_fd_raw(slave_fd);

    let _ = fs::set_fd_nonblocking(master_fd);

    let compositor_fd = with_client_fd(&handle);

    let mut grid = TerminalGrid::new(rows, cols);

    let result = match Ring::setup(32) {
        Ok(ring) => slopfut::block_on(
            ring,
            event_loop(master_fd, compositor_fd, &handle, &mut grid),
        ),
        Err(_) => {
            let _ = tty::write(b"terminal: ring setup failed\n");
            ExitReason::Error
        }
    };

    let _ = fs::close_fd_raw(master_fd);
    match result {
        ExitReason::ShellGone => {
            let _ = tty::write(b"terminal: shell session ended\n");
        }
        ExitReason::Closed => {
            let _ = tty::write(b"terminal: window closed\n");
        }
        ExitReason::Error => {
            let _ = tty::write(b"terminal: fatal loop error\n");
        }
    }
    std::process::exit(0);
}

enum ExitReason {
    ShellGone,
    Closed,
    Error,
}

/// The kernel tells the slave's foreground group with `SIGWINCH`.
fn push_winsize(master_fd: i32, rows: u16, cols: u16) {
    let ws = slopos_abi::syscall::UserWinsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let _ = fs::tiocswinsz(master_fd, &ws);
}

fn with_client_fd(handle: &slopos_windowing::ProtocolHandle) -> i32 {
    handle.borrow_client().fd()
}

/// No `TASK_FLAG_NEW_PGRP`: the shell makes its own session and takes the slave
/// as its terminal.
fn spawn_shell_on_slave(slave_fd: i32) {
    let actions = [
        process::clone_fd(slave_fd, 0),
        process::clone_fd(slave_fd, 1),
        process::clone_fd(slave_fd, 2),
    ];
    let env = process::environment();
    let envp: Vec<*const u8> = env.iter().map(|var| var.as_ptr().cast()).collect();
    let tid = process::spawn_path_with_env(
        b"/bin/shell",
        &[],
        &envp,
        TaskPriority::Normal,
        slopos_abi::task::TASK_FLAG_USER_MODE,
        &actions,
        0,
    );
    if tid <= 0 {
        let _ = tty::write(b"terminal: failed to spawn shell\n");
    }
}

fn enqueue_mouse(queue: &mut MasterWriteQueue, report: Option<KeyBytes>) {
    if let Some(bytes) = report {
        queue.enqueue_action_back(bytes.as_bytes().to_vec());
    }
}

async fn event_loop(
    master_fd: i32,
    compositor_fd: i32,
    handle: &slopos_windowing::ProtocolHandle,
    grid: &mut TerminalGrid,
) -> ExitReason {
    use slopos_abi::syscall::POLLIN;

    let mut selection = Selection::NONE;
    let mut ptr = PointerState::new();
    let mut mods: u8 = 0;
    let mut cursor_on = true;
    let mut last_blink = Instant::now();
    let mut history = DamageHistory::new(SURFACE_BUFFERS);
    // Only a present clears it, so a skipped frame carries its damage forward.
    let mut pending = CellDamage::new();
    pending.set_rows(grid.rows as usize);
    let mut pending_writes = MasterWriteQueue::new();
    // Between a paste's ready and its result: the receiver provides the buffer.
    let mut pending_paste: Option<ShmBuffer> = None;
    // One motion report per cell crossed rather than one per pixel.
    let mut last_mouse_cell: Option<(usize, usize)> = None;

    let _ = grid.take_damage();
    render_full(grid, &selection, cursor_on, &mut history, &mut pending);

    loop {
        let mut repaint_all = false;
        loop {
            let polled = handle.borrow_client().poll_event();
            let evt = match polled {
                Ok(Some(evt)) => evt,
                _ => break,
            };
            // Polling the socket directly, it routes releases itself, or both
            // buffers stay in flight.
            if let slopos_protocol::types::Event::BufferRelease { buffer_id, .. } = &evt {
                surface::release_buffer(*buffer_id);
                continue;
            }
            match input::classify(&evt) {
                CompositorEvent::Key(key) => {
                    match input::encode_key(key, grid.application_cursor_keys()) {
                        KeyAction::ToMaster(bytes) => {
                            let action = bytes.as_bytes().to_vec();
                            if is_priority_keyboard_control(bytes.as_bytes()) {
                                pending_writes.write_priority_action(master_fd, action);
                            } else {
                                pending_writes.enqueue_action_back(action);
                            }
                            if grid.viewing_history() {
                                grid.scroll_view_down(usize::MAX);
                            }
                        }
                        KeyAction::ScrollUp(n) => {
                            grid.scroll_view_up(n);
                        }
                        KeyAction::ScrollDown(n) => {
                            grid.scroll_view_down(n);
                        }
                        KeyAction::CopySelection => {
                            if selection.is_active() {
                                copy_selection(handle, grid, &selection);
                            }
                        }
                        KeyAction::RequestPaste => {
                            let _ = handle.borrow_client().clipboard_paste();
                        }
                        KeyAction::None => {}
                    }
                }
                CompositorEvent::Modifiers(m) => {
                    mods = m;
                }
                CompositorEvent::Resize(w, h) => {
                    if surface::resize(w as u32, h as u32) {
                        let (cw, ch) = render::cell_size();
                        let cols = render::cells_in(w, cw, grid::MAX_COLS);
                        let rows = render::cells_in(h, ch, grid::MAX_ROWS);
                        // A new width re-wraps scrollback, the selection with it.
                        let mut pts = selection.endpoints();
                        let outcome = grid.resize(rows, cols, &mut pts);
                        if outcome.reflowed && selection.is_active() {
                            match (pts[0], pts[1]) {
                                (Some(a), Some(h)) => selection.set_endpoints(a, h),
                                // An end left the history.
                                _ => selection.clear(),
                            }
                        }
                        push_winsize(master_fd, rows, cols);
                        last_mouse_cell = None;
                        history.clear();
                        pending.set_rows(grid.rows as usize);
                        let _ = grid.take_damage();
                        render_full(grid, &selection, cursor_on, &mut history, &mut pending);
                    }
                }
                CompositorEvent::Close => {
                    return ExitReason::Closed;
                }
                CompositorEvent::PointerMotion(x, y) | CompositorEvent::PointerEnter(x, y) => {
                    let (x, y) = (x - render::MARGIN, y - render::MARGIN);
                    // Only an enter carries the serial a cursor request needs.
                    if matches!(evt, slopos_protocol::types::Event::PointerEnter { .. }) {
                        surface::set_cursor_shape(slopos_abi::CURSOR_SHAPE_TEXT);
                    }
                    ptr.last_x = x;
                    ptr.last_y = y;
                    ptr.has_focus = true;
                    let (cw, ch) = render::cell_size();
                    if input::mouse_reporting(grid, mods) {
                        let cell = input::pixel_to_cell(x, y, cw, ch, grid);
                        if last_mouse_cell != Some(cell) {
                            last_mouse_cell = Some(cell);
                            enqueue_mouse(
                                &mut pending_writes,
                                input::mouse_report_at(
                                    grid,
                                    MouseEventKind::Motion,
                                    0,
                                    ptr.button_state,
                                    x,
                                    y,
                                    cw,
                                    ch,
                                    mods,
                                ),
                            );
                        }
                    } else if input::update_selection(&mut ptr, &mut selection, grid, cw, ch) {
                        repaint_all = true;
                    }
                }
                CompositorEvent::PointerLeave => {
                    ptr.has_focus = false;
                    last_mouse_cell = None;
                }
                CompositorEvent::Scroll(value_v120) => {
                    if input::mouse_reporting(grid, mods) {
                        let notches = value_v120 / 120;
                        let kind = if notches < 0 {
                            MouseEventKind::WheelUp
                        } else {
                            MouseEventKind::WheelDown
                        };
                        let (cw, ch) = render::cell_size();
                        for _ in 0..notches.unsigned_abs() {
                            enqueue_mouse(
                                &mut pending_writes,
                                input::mouse_report_at(
                                    grid,
                                    kind,
                                    0,
                                    ptr.button_state,
                                    ptr.last_x,
                                    ptr.last_y,
                                    cw,
                                    ch,
                                    mods,
                                ),
                            );
                        }
                    } else {
                        let lines = input::wheel_scroll_lines(value_v120);
                        if lines < 0 {
                            grid.scroll_view_up((-lines) as usize);
                        } else if lines > 0 {
                            grid.scroll_view_down(lines as usize);
                        }
                    }
                }
                CompositorEvent::PointerButton { pressed, code } => {
                    if pressed {
                        ptr.button_state |= code;
                    } else {
                        ptr.button_state &= !code;
                    }
                    let (cw, ch) = render::cell_size();
                    if input::mouse_reporting(grid, mods) {
                        last_mouse_cell =
                            Some(input::pixel_to_cell(ptr.last_x, ptr.last_y, cw, ch, grid));
                        enqueue_mouse(
                            &mut pending_writes,
                            input::mouse_report_at(
                                grid,
                                if pressed {
                                    MouseEventKind::Press
                                } else {
                                    MouseEventKind::Release
                                },
                                code,
                                ptr.button_state,
                                ptr.last_x,
                                ptr.last_y,
                                cw,
                                ch,
                                mods,
                            ),
                        );
                    } else {
                        if input::update_selection(&mut ptr, &mut selection, grid, cw, ch) {
                            repaint_all = true;
                        }
                        // A bare click leaves the selection inactive.
                        if !pressed && selection.is_active() {
                            copy_selection(handle, grid, &selection);
                        }
                    }
                }
                CompositorEvent::PasteReady(len) => {
                    pending_paste = None;
                    if len > 0 {
                        if let Ok(dst) = ShmBuffer::create(len as usize) {
                            if handle.borrow_client().clipboard_read(dst.fd(), len).is_ok() {
                                pending_paste = Some(dst);
                            }
                        }
                    }
                }
                CompositorEvent::PasteResult(len) => {
                    if let Some(dst) = pending_paste.take() {
                        let n = (len as usize).min(dst.size());
                        write_paste(
                            master_fd,
                            &mut pending_writes,
                            &dst.as_slice()[..n],
                            grid.bracketed_paste(),
                        );
                    }
                }
                CompositorEvent::Ignored => {}
            }
        }

        let drained = drain_master(master_fd, grid);

        // On this turn: a program blocked reading a reply would otherwise wait
        // for the next wake.
        pending_writes.enqueue_action_back(grid.take_replies());

        pending_writes.drain(master_fd, MASTER_WRITE_BUDGET);

        match drained {
            MasterDrain::Eof => return ExitReason::ShellGone,
            MasterDrain::Data | MasterDrain::Idle => {}
        }

        if repaint_all {
            // The grid records no damage for the selection's shading.
            pending.add_all(grid.cols);
        }
        pending.union(&grid.take_damage());
        present_pending(grid, &selection, cursor_on, &mut history, &mut pending);

        let elapsed = last_blink.elapsed();
        if elapsed >= BLINK_INTERVAL {
            cursor_on = !cursor_on;
            last_blink = Instant::now();
            grid.damage_cursor();
            pending.union(&grid.take_damage());
            present_pending(grid, &selection, cursor_on, &mut history, &mut pending);
            continue;
        }
        let blink_ms = (BLINK_INTERVAL - elapsed).as_millis().max(1) as u64;
        let comp = slopfut::poll_add(compositor_fd, POLLIN);
        let master_mask = if pending_writes.is_empty() {
            POLLIN
        } else {
            POLLIN | slopos_abi::syscall::POLLOUT
        };
        let mst = slopfut::poll_add(master_fd, master_mask);
        let blink = Box::pin(slopfut::time::sleep_ms(blink_ms));
        let _ = slopfut::select3(comp, mst, blink).await;
    }
}

/// Where the buffers hold nothing a partial repaint could build on.
fn render_full(
    grid: &TerminalGrid,
    selection: &Selection,
    cursor_on: bool,
    history: &mut DamageHistory,
    pending: &mut CellDamage,
) {
    render::render_full(grid, selection, cursor_on);
    pending.clear();
    // No recorded frame describes what the other slots hold.
    history.clear();
    let mut all = CellDamage::new();
    all.set_rows(grid.rows as usize);
    all.add_all(grid.cols);
    history.push(all);
}

fn present_pending(
    grid: &TerminalGrid,
    selection: &Selection,
    cursor_on: bool,
    history: &mut DamageHistory,
    pending: &mut CellDamage,
) {
    if pending.is_empty() {
        return;
    }
    match history.resolve(surface::buffer_age(), pending) {
        Some(repaint) => {
            render::render_damage(grid, selection, cursor_on, &repaint);
            history.push(core::mem::replace(pending, {
                let mut d = CellDamage::new();
                d.set_rows(grid.rows as usize);
                d
            }));
        }
        None => render_full(grid, selection, cursor_on, history, pending),
    }
}

struct QueuedWrite {
    bytes: Vec<u8>,
    written: usize,
}

struct MasterWriteQueue {
    actions: VecDeque<QueuedWrite>,
    queued_bytes: usize,
}

impl MasterWriteQueue {
    fn new() -> Self {
        Self {
            actions: VecDeque::new(),
            queued_bytes: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.queued_bytes == 0
    }

    fn remaining_capacity(&self) -> usize {
        MASTER_WRITE_QUEUE_CAP.saturating_sub(self.queued_bytes)
    }

    fn enqueue_action_back(&mut self, data: Vec<u8>) {
        self.enqueue_action(self.actions.len(), data, 0)
    }

    /// Drops what does not fit whole: a truncated sequence would garble what
    /// follows it.
    fn enqueue_action(&mut self, at: usize, data: Vec<u8>, written: usize) {
        let remaining = data.len().saturating_sub(written);
        if remaining == 0 || remaining > self.remaining_capacity() {
            return;
        }
        let action = QueuedWrite {
            bytes: data,
            written,
        };
        self.actions.insert(at, action);
        self.queued_bytes += remaining;
    }

    fn write_priority_action(&mut self, fd: i32, data: Vec<u8>) {
        if data.len() > self.remaining_capacity() {
            return;
        }
        if !self.is_empty() {
            // Behind a write already begun, which it would cut in two.
            let begun = self
                .actions
                .front()
                .is_some_and(|action| action.written > 0);
            return self.enqueue_action(usize::from(begun), data, 0);
        }
        match fs::write_slice(fd, &data) {
            Ok(n) if n == data.len() => {}
            Ok(n) => self.enqueue_action(0, data, n),
            Err(e) if e == crate::syscall::SyscallError::EAGAIN => self.enqueue_action(0, data, 0),
            Err(e) => {
                let msg = std::format!("terminal: priority write error {e:?}\n");
                let _ = tty::write(msg.as_bytes());
                self.enqueue_action(0, data, 0)
            }
        }
    }

    fn drain(&mut self, fd: i32, budget: usize) {
        let mut written_total = 0usize;
        loop {
            let Some(action) = self.actions.front_mut() else {
                break;
            };
            let limit = budget.saturating_sub(written_total);
            if limit == 0 {
                break;
            }
            let remaining = &action.bytes[action.written..];
            let chunk_len = remaining.len().min(limit);
            match fs::write_slice(fd, &remaining[..chunk_len]) {
                Ok(0) => break,
                Ok(n) => {
                    written_total += n;
                    action.written += n;
                    self.queued_bytes = self.queued_bytes.saturating_sub(n);
                    if action.written == action.bytes.len() {
                        let _ = self.actions.pop_front();
                    }
                }
                Err(e) if e == crate::syscall::SyscallError::EAGAIN => break,
                Err(e) => {
                    let msg = std::format!("terminal: queued write error {e:?}\n");
                    let _ = tty::write(msg.as_bytes());
                    self.actions.clear();
                    self.queued_bytes = 0;
                    break;
                }
            }
        }
    }
}

fn is_priority_keyboard_control(bytes: &[u8]) -> bool {
    matches!(bytes, [0x03] | [0x1C] | [0x1A] | [0x11] | [0x13])
}

enum MasterDrain {
    Data,
    Idle,
    Eof,
}

/// Mirrors every byte to the kernel console, so `just boot-log` shows the
/// shell's output.
fn drain_master(master_fd: i32, grid: &mut TerminalGrid) -> MasterDrain {
    let mut buf = [0u8; MASTER_READ_CHUNK];
    let mut got_any = false;
    let mut total = 0usize;
    loop {
        match fs::read_slice(master_fd, &mut buf) {
            Ok(0) => {
                return MasterDrain::Eof;
            }
            Ok(n) => {
                got_any = true;
                total += n;
                let _ = tty::write(&buf[..n]);
                for &b in &buf[..n] {
                    grid.process_byte(b);
                }
                if total >= MASTER_READ_BUDGET {
                    return MasterDrain::Data;
                }
            }
            Err(e) if e == crate::syscall::SyscallError::EAGAIN => {
                return if got_any {
                    MasterDrain::Data
                } else {
                    MasterDrain::Idle
                };
            }
            Err(e) => {
                let msg = std::format!("terminal: master read error {e:?}\n");
                let _ = tty::write(msg.as_bytes());
                return MasterDrain::Eof;
            }
        }
    }
}

/// The compositor's ceiling on one copy.
const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

fn copy_selection(
    handle: &slopos_windowing::ProtocolHandle,
    grid: &TerminalGrid,
    selection: &Selection,
) {
    let bound = input::selection_byte_bound(grid, selection).min(MAX_CLIPBOARD_BYTES);
    if bound == 0 {
        return;
    }
    let Ok(mut shm) = ShmBuffer::create(bound) else {
        return;
    };
    let n = input::collect_selection(grid, selection, shm.as_mut_slice());
    if n > 0 {
        // The compositor keeps its own copy of the descriptor.
        let _ = handle.borrow_client().clipboard_copy(shm.fd(), n as u32);
    }
}

/// Bracketed only when the program asked for it with DECSET 2004.
/// `sanitize_paste` strips every control byte first, so the bracket cannot be
/// escaped and a clipboard never types Ctrl+C.
fn write_paste(
    master_fd: i32,
    pending_writes: &mut MasterWriteQueue,
    data: &[u8],
    bracketed: bool,
) {
    if data.is_empty() {
        return;
    }
    let mut clean = std::vec![0u8; data.len()];
    let n = input::sanitize_paste(data, &mut clean);
    if n == 0 {
        return;
    }
    let markers_len = if bracketed {
        b"\x1b[200~".len() + b"\x1b[201~".len()
    } else {
        0
    };
    let action_len = n.saturating_add(markers_len);
    if action_len == 0 || action_len > pending_writes.remaining_capacity() {
        return;
    }
    let mut action = Vec::with_capacity(action_len);
    if bracketed {
        action.extend_from_slice(b"\x1b[200~");
    }
    action.extend_from_slice(&clean[..n]);
    if bracketed {
        action.extend_from_slice(b"\x1b[201~");
    }
    pending_writes.enqueue_action_back(action);
    pending_writes.drain(master_fd, MASTER_WRITE_BUDGET);
}
