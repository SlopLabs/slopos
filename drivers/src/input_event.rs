//! Input Event Protocol — per-task queues with SeqLock focus tracking.
//!
//! Focus state (which task receives input) is protected by a [`SeqLock`] —
//! ISR handlers read it lock-free, the compositor writes it rarely. Per-task
//! event queues are individually locked with [`SpinLock`], so event delivery
//! to one task never blocks delivery to another.

use core::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, Ordering};
use slopos_hid_core::pointer::{Axis, Motion};
use slopos_ostd::RingBuffer;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SeqLock, SpinLock};

/// Monotonic millisecond timestamp for input events.
pub fn get_timestamp_ms() -> u64 {
    crate::hpet::nanoseconds(crate::hpet::read_counter()) / 1_000_000
}

pub use slopos_abi::{
    InputEvent, InputEventData, InputEventType, MAX_EVENTS_PER_TASK, MAX_INPUT_TASKS,
};

struct TaskEventQueue {
    /// Task that owns this slot, or 0 when the slot is free. The authoritative
    /// copy of the [`SLOT_TASK_IDS`] mirror entry.
    task_id: u32,
    events: RingBuffer<InputEvent, MAX_EVENTS_PER_TASK>,
}

impl TaskEventQueue {
    const fn new() -> Self {
        Self {
            task_id: 0,
            events: RingBuffer::new_with(InputEvent {
                event_type: InputEventType::KeyPress,
                _padding: [0; 3],
                timestamp_ms: 0,
                data: InputEventData { data0: 0, data1: 0 },
            }),
        }
    }
}

static TASK_QUEUES: [SpinLock<TaskEventQueue>; MAX_INPUT_TASKS] = [const {
    SpinLock::new(
        TaskEventQueue::new(),
        lock_class!("TASK_QUEUES", LOCK_LEVEL_RESOURCE),
    )
}; MAX_INPUT_TASKS];

#[derive(Clone, Copy)]
struct InputFocusState {
    keyboard_focus: u32,
    pointer_focus: u32,
    window_offset_x: i32,
    window_offset_y: i32,
    compositor_task_id: u32,
}

impl InputFocusState {
    const fn new() -> Self {
        Self {
            keyboard_focus: 0,
            pointer_focus: 0,
            window_offset_x: 0,
            window_offset_y: 0,
            compositor_task_id: 0,
        }
    }
}

static FOCUS: SeqLock<InputFocusState> = SeqLock::new(InputFocusState::new());

/// Mirrors of [`POINTER`]'s position and merged buttons, read without its
/// lock.
static POINTER_X: AtomicI32 = AtomicI32::new(0);
static POINTER_Y: AtomicI32 = AtomicI32::new(0);
static POINTER_BUTTONS: AtomicU8 = AtomicU8::new(0);

/// Pointing devices that move the cursor at once.
pub const MAX_POINTERS: usize = 8;

/// A pointing device's hold on the one cursor: its buttons are its own, the
/// position everyone's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerSource(u8);

impl PointerSource {
    pub const PS2: Self = Self(0);
    pub const TOUCHPAD: Self = Self(1);
    const FIXED: u8 = 2;
}

struct PointerState {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    buttons: [u8; MAX_POINTERS],
    /// Each source's last absolute X and Y, as it reported them.
    placed: [[Option<i32>; 2]; MAX_POINTERS],
    claimed: u8,
}

impl PointerState {
    fn merged(&self) -> u8 {
        self.buttons.iter().fold(0, |all, b| all | b)
    }

    /// Whether `axis` moves the cursor: an absolute one only when it changed,
    /// since a tablet repeats its position in every report.
    fn moves(&mut self, source: PointerSource, index: usize, axis: Axis) -> bool {
        let Axis::Absolute { value, .. } = axis else {
            return true;
        };
        let Some(placed) = self
            .placed
            .get_mut(usize::from(source.0))
            .map(|axes| &mut axes[index])
        else {
            return false;
        };
        placed.replace(value) != Some(value)
    }
}

static POINTER: SpinLock<PointerState> = SpinLock::new(
    PointerState {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        buttons: [0; MAX_POINTERS],
        placed: [[None; 2]; MAX_POINTERS],
        claimed: (1 << PointerSource::FIXED) - 1,
    },
    lock_class!("POINTER", LOCK_LEVEL_RESOURCE),
);

/// A slot for a device that comes and goes; `None` when every one is taken.
pub fn claim_pointer() -> Option<PointerSource> {
    let mut p = POINTER.lock();
    let free = (!p.claimed).trailing_zeros();
    if free as usize >= MAX_POINTERS {
        return None;
    }
    p.claimed |= 1 << free;
    p.buttons[free as usize] = 0;
    p.placed[free as usize] = [None; 2];
    Some(PointerSource(free as u8))
}

/// Lifts every button only `source` held, then frees its slot.
pub fn release_pointer(source: PointerSource, timestamp_ms: u64) {
    pointer_buttons(source, 0, timestamp_ms);
    if source.0 >= PointerSource::FIXED {
        POINTER.lock().claimed &= !(1 << source.0);
    }
}

/// The screen the cursor moves on, from the video layer; the first one
/// centres it.
pub fn set_pointer_bounds(width: i32, height: i32) {
    if width <= 0 || height <= 0 {
        return;
    }
    let mut p = POINTER.lock();
    if p.width == 0 {
        p.x = width / 2;
        p.y = height / 2;
    }
    p.width = width;
    p.height = height;
    p.x = p.x.clamp(0, width - 1);
    p.y = p.y.clamp(0, height - 1);
    POINTER_X.store(p.x, Ordering::Relaxed);
    POINTER_Y.store(p.y, Ordering::Relaxed);
}

/// `(0, 0)` until the video layer has published a screen.
pub fn pointer_bounds() -> (i32, i32) {
    let p = POINTER.lock();
    (p.width, p.height)
}

fn moved(axis: Axis, at: i32, extent: i32) -> i32 {
    let next = match axis {
        Axis::Relative(delta) => at.saturating_add(delta),
        absolute => absolute.onto(extent).unwrap_or(at),
    };
    next.clamp(0, (extent - 1).max(0))
}

/// One report from `source`: relative axes move the cursor, absolute ones
/// place it on the screen when they change, and a button it reports goes
/// down when the first device presses it and up when the last releases it.
pub fn pointer_report(source: PointerSource, motion: &Motion, timestamp_ms: u64) {
    {
        let mut p = POINTER.lock();
        let x = motion
            .x
            .filter(|&axis| p.width > 0 && p.moves(source, 0, axis));
        let y = motion
            .y
            .filter(|&axis| p.height > 0 && p.moves(source, 1, axis));
        let (x, y) = (
            x.map_or(p.x, |axis| moved(axis, p.x, p.width)),
            y.map_or(p.y, |axis| moved(axis, p.y, p.height)),
        );
        if (x, y) != (p.x, p.y) {
            p.x = x;
            p.y = y;
            POINTER_X.store(x, Ordering::Relaxed);
            POINTER_Y.store(y, Ordering::Relaxed);
            input_route_pointer_motion(x, y, timestamp_ms);
        }
        let reported = motion.reported as u8;
        let held = p.buttons.get(usize::from(source.0)).copied().unwrap_or(0);
        let buttons = held & !reported | motion.buttons as u8 & reported;
        set_buttons(&mut p, source, buttons, timestamp_ms);
    }
    if motion.wheel != 0 {
        input_route_pointer_axis(
            slopos_abi::POINTER_AXIS_VERTICAL,
            motion.wheel.saturating_mul(-120),
            timestamp_ms,
        );
    }
    if motion.pan != 0 {
        input_route_pointer_axis(
            slopos_abi::POINTER_AXIS_HORIZONTAL,
            motion.pan.saturating_mul(120),
            timestamp_ms,
        );
    }
}

/// How many pointing devices hold `button` down.
#[cfg(feature = "test-hooks")]
pub fn button_holders(button: u8) -> usize {
    POINTER
        .lock()
        .buttons
        .iter()
        .filter(|held| *held & button != 0)
        .count()
}

/// `source`'s whole button state, bit `n - 1` for button `n`.
pub fn pointer_buttons(source: PointerSource, buttons: u8, timestamp_ms: u64) {
    set_buttons(&mut POINTER.lock(), source, buttons, timestamp_ms);
}

fn set_buttons(p: &mut PointerState, source: PointerSource, buttons: u8, timestamp_ms: u64) {
    let Some(held) = p.buttons.get(usize::from(source.0)).copied() else {
        return;
    };
    if held == buttons {
        return;
    }
    let before = p.merged();
    p.buttons[usize::from(source.0)] = buttons;
    let after = p.merged();
    POINTER_BUTTONS.store(after, Ordering::Relaxed);
    for bit in 0..8 {
        let button = 1u8 << bit;
        if (before ^ after) & button != 0 {
            input_route_pointer_button(button, after & button != 0, timestamp_ms);
        }
    }
}

/// Avoids a SeqLock read in `has_keyboard_focus`.
static KEYBOARD_FOCUS_FAST: AtomicU32 = AtomicU32::new(0);

/// Task id occupying each queue slot, or 0 when the slot is free. Mirrors
/// [`TaskEventQueue::task_id`] so that resolving a task to its slot is a
/// lock-free scan, which the event-routing path does once per event with
/// pointer devices reporting at up to 1000 Hz.
///
/// Indexed by *slot*, not by task id: task ids are monotonic and never
/// recycled, so a task-id-indexed structure would carry a ceiling that a
/// long-lived boot session eventually walks past.
///
/// # Ordering
///
/// A slot's mirror entry is always written **last**, after the queue behind it
/// has been put into the state the entry advertises: on claim, after the queue
/// is bound and drained; on release, after it is unbound and drained. The
/// stores are `Release` and the scans `Acquire`.
///
/// The mirror is a hint, never a proof: a scan is not atomic with the lock
/// that follows it, so every operation re-reads `TaskEventQueue::task_id`
/// under the queue lock and abandons the operation if the slot has since been
/// rebound.
static SLOT_TASK_IDS: [AtomicU32; MAX_INPUT_TASKS] = [const { AtomicU32::new(0) }; MAX_INPUT_TASKS];

/// Serialises claiming and releasing a slot, so a task owns at most one. Held
/// only on task creation and destruction, never on the event-routing path.
///
/// A queue lock is taken while this is held, and never the reverse, so the
/// dependency graph over these two classes has a single edge and no cycle.
static SLOT_REGISTRY: SpinLock<()> =
    SpinLock::new((), lock_class!("SLOT_REGISTRY", LOCK_LEVEL_REGISTRY));

/// Find the slot a task's queue lives in, without taking any lock.
fn find_queue(task_id: u32) -> Option<usize> {
    if task_id == 0 {
        return None;
    }
    SLOT_TASK_IDS
        .iter()
        .position(|slot| slot.load(Ordering::Acquire) == task_id)
}

/// Find the slot a task's queue lives in, claiming a free one if it has none.
///
/// Returns `None` only when the task id is invalid or all
/// [`MAX_INPUT_TASKS`] slots are already spoken for.
///
/// # Never from the routing path
///
/// Every caller must be a syscall, a focus change or a registration — a point
/// where a task is *asking* for a queue. `input_route_*` runs in interrupt
/// handlers and the xHCI drain, where there is no principal to charge and no
/// errno to return, so
/// those paths call [`find_queue`] and drop the event when a task has no
/// queue: a claim there would acquire a slot on behalf of a task that never
/// asked, at a point that cannot refuse.
///
/// The queue itself is a fixed `.bss` array, so a claim costs no memory — what
/// it takes is one of [`MAX_INPUT_TASKS`] slots, pre-reserved at its full
/// [`MAX_EVENTS_PER_TASK`] capacity, which makes a full queue a bound the
/// owner already paid for.
fn resolve_queue(task_id: u32) -> Option<usize> {
    if task_id == 0 {
        return None;
    }

    if let Some(slot) = find_queue(task_id) {
        return Some(slot);
    }

    // The registry lock makes "look, then claim" atomic against a concurrent
    // registration of the same task.
    let _registry = SLOT_REGISTRY.lock();

    // Another CPU may have claimed a slot for this task on our way to the lock.
    if let Some(slot) = find_queue(task_id) {
        return Some(slot);
    }

    let slot = SLOT_TASK_IDS
        .iter()
        .position(|slot| slot.load(Ordering::Acquire) == 0)?;

    {
        let mut queue = TASK_QUEUES[slot].lock();
        queue.task_id = task_id;
        queue.events.reset();
    }
    SLOT_TASK_IDS[slot].store(task_id, Ordering::Release);

    Some(slot)
}

/// Run `f` against a task's queue, having confirmed under the queue lock that
/// `slot` still belongs to `task_id`. Returns `None` if it does not.
#[inline]
fn with_queue<R>(slot: usize, task_id: u32, f: impl FnOnce(&mut TaskEventQueue) -> R) -> Option<R> {
    let queue = TASK_QUEUES.get(slot)?;
    let mut queue = queue.lock();
    if queue.task_id != task_id {
        return None;
    }
    Some(f(&mut queue))
}

/// Find a task's queue and run `f` against it. Does not create a queue.
#[inline]
fn with_task_queue<R>(task_id: u32, f: impl FnOnce(&mut TaskEventQueue) -> R) -> Option<R> {
    with_queue(find_queue(task_id)?, task_id, f)
}

#[inline]
fn push_event(slot: usize, task_id: u32, event: InputEvent) {
    with_queue(slot, task_id, |queue| queue.events.push_overwrite(event));
}

#[inline]
pub fn has_keyboard_focus() -> bool {
    KEYBOARD_FOCUS_FAST.load(Ordering::Acquire) != 0
}

pub fn input_set_keyboard_focus(task_id: u32) {
    // Giving focus to a task is the moment it asks for a queue; see
    // `resolve_queue` for why a claim never happens on the routing path.
    if task_id != 0 {
        let _ = resolve_queue(task_id);
    }
    KEYBOARD_FOCUS_FAST.store(task_id, Ordering::Release);
    let mut guard = FOCUS.write_lock();
    guard.get_mut().keyboard_focus = task_id;
}

pub fn input_set_pointer_focus(task_id: u32, timestamp_ms: u64) {
    input_set_pointer_focus_with_offset(task_id, 0, 0, timestamp_ms);
}

pub fn input_set_pointer_focus_with_offset(
    task_id: u32,
    offset_x: i32,
    offset_y: i32,
    timestamp_ms: u64,
) {
    let old_state = FOCUS.read();
    let old_focus = old_state.pointer_focus;
    let x = POINTER_X.load(Ordering::Relaxed);
    let y = POINTER_Y.load(Ordering::Relaxed);

    {
        let mut guard = FOCUS.write_lock();
        let s = guard.get_mut();
        s.pointer_focus = task_id;
        s.window_offset_x = offset_x;
        s.window_offset_y = offset_y;
    }

    if old_focus == task_id {
        return;
    }

    if old_focus != 0 {
        if let Some(slot) = find_queue(old_focus) {
            push_event(
                slot,
                old_focus,
                InputEvent::pointer_enter_leave(false, x, y, timestamp_ms),
            );
        }
    }

    if task_id != 0 {
        if let Some(slot) = resolve_queue(task_id) {
            let local_x = x - offset_x;
            let local_y = y - offset_y;
            push_event(
                slot,
                task_id,
                InputEvent::pointer_enter_leave(true, local_x, local_y, timestamp_ms),
            );
        }
    }
}

pub fn input_request_close(task_id: u32, timestamp_ms: u64) -> bool {
    if task_id == 0 {
        return false;
    }
    if let Some(slot) = resolve_queue(task_id) {
        push_event(slot, task_id, InputEvent::close_request(timestamp_ms));
        true
    } else {
        false
    }
}

pub fn input_send_configure(task_id: u32, width: u32, height: u32, timestamp_ms: u64) -> bool {
    if task_id == 0 {
        return false;
    }
    if let Some(slot) = resolve_queue(task_id) {
        push_event(
            slot,
            task_id,
            InputEvent::configure(width, height, timestamp_ms),
        );
        true
    } else {
        false
    }
}

pub fn input_get_keyboard_focus() -> u32 {
    FOCUS.read().keyboard_focus
}

pub fn input_get_pointer_focus() -> u32 {
    FOCUS.read().pointer_focus
}

pub fn input_get_pointer_position() -> (i32, i32) {
    (
        POINTER_X.load(Ordering::Relaxed),
        POINTER_Y.load(Ordering::Relaxed),
    )
}

pub fn input_get_button_state() -> u8 {
    POINTER_BUTTONS.load(Ordering::Relaxed)
}

pub fn input_get_modifier_state() -> u8 {
    crate::keyboard::get_modifier_state()
}

/// Route a fully-populated key event to the focused task / compositor.
#[allow(clippy::too_many_arguments)]
pub fn input_route_key_full(
    scancode: u8,
    ascii: u8,
    keycode: u16,
    codepoint: u32,
    modifiers: u8,
    flags: u8,
    pressed: bool,
    timestamp_ms: u64,
) {
    let state = FOCUS.read();

    let target = if state.compositor_task_id != 0 {
        state.compositor_task_id
    } else {
        if KEYBOARD_FOCUS_FAST.load(Ordering::Acquire) == 0 {
            return;
        }
        if state.keyboard_focus == 0 {
            return;
        }
        state.keyboard_focus
    };

    if let Some(slot) = find_queue(target) {
        let event_type = if pressed {
            InputEventType::KeyPress
        } else {
            InputEventType::KeyRelease
        };
        push_event(
            slot,
            target,
            InputEvent::key_full(
                event_type,
                scancode,
                ascii,
                keycode,
                codepoint,
                modifiers,
                flags,
                timestamp_ms,
            ),
        );
    }
}

fn input_route_pointer_motion(x: i32, y: i32, timestamp_ms: u64) {
    let state = FOCUS.read();
    let comp_id = state.compositor_task_id;
    if comp_id != 0 {
        if let Some(slot) = find_queue(comp_id) {
            push_event(
                slot,
                comp_id,
                InputEvent::pointer_motion(x, y, timestamp_ms),
            );
        }
        return;
    }

    let focus = state.pointer_focus;
    if focus == 0 {
        return;
    }

    let local_x = x - state.window_offset_x;
    let local_y = y - state.window_offset_y;
    if let Some(slot) = find_queue(focus) {
        push_event(
            slot,
            focus,
            InputEvent::pointer_motion(local_x, local_y, timestamp_ms),
        );
    }
}

fn input_route_pointer_button(button: u8, pressed: bool, timestamp_ms: u64) {
    let state = FOCUS.read();
    let comp_id = state.compositor_task_id;
    if comp_id != 0 {
        if let Some(slot) = find_queue(comp_id) {
            push_event(
                slot,
                comp_id,
                InputEvent::pointer_button(pressed, button, timestamp_ms),
            );
        }
        return;
    }

    let focus = state.pointer_focus;
    if focus == 0 {
        return;
    }
    if let Some(slot) = find_queue(focus) {
        push_event(
            slot,
            focus,
            InputEvent::pointer_button(pressed, button, timestamp_ms),
        );
    }
}

pub fn input_route_pointer_axis(axis: u32, value_v120: i32, timestamp_ms: u64) {
    let state = FOCUS.read();
    let comp_id = state.compositor_task_id;
    if comp_id != 0 {
        if let Some(slot) = find_queue(comp_id) {
            push_event(
                slot,
                comp_id,
                InputEvent::pointer_axis(axis, value_v120, timestamp_ms),
            );
        }
        return;
    }

    let focus = state.pointer_focus;
    if focus == 0 {
        return;
    }
    if let Some(slot) = find_queue(focus) {
        push_event(
            slot,
            focus,
            InputEvent::pointer_axis(axis, value_v120, timestamp_ms),
        );
    }
}

pub fn input_register_compositor(task_id: u32) {
    // Pre-create the queue here; see `resolve_queue` for why a claim never
    // happens on the routing path.
    let _ = resolve_queue(task_id);
    let mut guard = FOCUS.write_lock();
    guard.get_mut().compositor_task_id = task_id;
}

/// The task every raw input event is routed to, or 0 when the sink is free.
pub fn input_compositor_task_id() -> u32 {
    FOCUS.read().compositor_task_id
}

pub fn input_poll(task_id: u32) -> Option<InputEvent> {
    with_task_queue(task_id, |queue| queue.events.try_pop())?
}

pub fn input_drain_batch(task_id: u32, out_buffer: *mut InputEvent, max_count: usize) -> usize {
    if out_buffer.is_null() || max_count == 0 {
        return 0;
    }

    let slot = match resolve_queue(task_id) {
        Some(s) => s,
        None => return 0,
    };

    with_queue(slot, task_id, |queue| {
        let mut count = 0;
        while count < max_count {
            if let Some(event) = queue.events.try_pop() {
                slopos_ostd::util::ptr_buf::write_at_index(out_buffer, count, event);
                count += 1;
            } else {
                break;
            }
        }
        count
    })
    .unwrap_or(0)
}

pub fn input_peek(task_id: u32) -> Option<InputEvent> {
    with_task_queue(task_id, |queue| queue.events.peek().copied())?
}

pub fn input_has_events(task_id: u32) -> bool {
    with_task_queue(task_id, |queue| !queue.events.is_empty()).unwrap_or(false)
}

pub fn input_event_count(task_id: u32) -> u32 {
    with_task_queue(task_id, |queue| queue.events.len() as u32).unwrap_or(0)
}

struct ClipboardState {
    data: [u8; slopos_abi::CLIPBOARD_MAX_SIZE],
    len: usize,
}

impl ClipboardState {
    const fn new() -> Self {
        Self {
            data: [0u8; slopos_abi::CLIPBOARD_MAX_SIZE],
            len: 0,
        }
    }
}

static CLIPBOARD: SpinLock<ClipboardState> = SpinLock::new(
    ClipboardState::new(),
    lock_class!("CLIPBOARD", LOCK_LEVEL_RESOURCE),
);

pub fn clipboard_copy(src: &[u8]) -> usize {
    let mut clip = CLIPBOARD.lock();
    let copy_len = src.len().min(slopos_abi::CLIPBOARD_MAX_SIZE);
    clip.data[..copy_len].copy_from_slice(&src[..copy_len]);
    clip.len = copy_len;
    copy_len
}

pub fn clipboard_paste(dst: &mut [u8]) -> usize {
    let clip = CLIPBOARD.lock();
    if clip.len == 0 {
        return 0;
    }
    let copy_len = clip.len.min(dst.len());
    dst[..copy_len].copy_from_slice(&clip.data[..copy_len]);
    copy_len
}

pub fn input_cleanup_task(task_id: u32) {
    // A stale `compositor_task_id` would keep routing every key and pointer
    // event to a dead task, and `resolve_queue` would keep re-claiming a slot.
    {
        let current = FOCUS.read();
        if current.keyboard_focus == task_id
            || current.pointer_focus == task_id
            || current.compositor_task_id == task_id
        {
            let mut guard = FOCUS.write_lock();
            let s = guard.get_mut();
            if s.keyboard_focus == task_id {
                s.keyboard_focus = 0;
                KEYBOARD_FOCUS_FAST.store(0, Ordering::Release);
            }
            if s.pointer_focus == task_id {
                // The loss happens here, so the re-seed belongs here rather
                // than on `input_poll_batch`'s frame-rate path. 0 when no seat
                // is held, as the old re-arm left it.
                s.pointer_focus = slopos_ostd::seat::holder(slopos_ostd::seat::SeatKind::InputSink)
                    .filter(|holder| *holder != task_id)
                    .unwrap_or(0);
                s.window_offset_x = 0;
                s.window_offset_y = 0;
            }
            if s.compositor_task_id == task_id {
                s.compositor_task_id = 0;
            }
        }
    }

    // The registry lock keeps a concurrent claim from taking the slot between
    // the scan and the release.
    let _registry = SLOT_REGISTRY.lock();
    let Some(slot) = find_queue(task_id) else {
        return;
    };
    with_queue(slot, task_id, |queue| {
        queue.task_id = 0;
        queue.events.reset();
    });
    SLOT_TASK_IDS[slot].store(0, Ordering::Release);
}
