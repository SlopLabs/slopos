//! The machine's one keyboard state, which every keyboard feeds with
//! `(source, usage, pressed)` steps: the modifiers, merged by counting, the
//! locks and the layout with its dead key, the diagnostic console's trigger,
//! the boot log's Esc, the scroll keys, and the TTY a key reaches when no task
//! has keyboard focus.

use core::sync::atomic::{AtomicU8, Ordering};

use slopos_abi::Errno;
use slopos_abi::input::{KEY_FLAG_FROM_KEYPAD, KEY_FLAG_HAS_CANONICAL, KEY_FLAG_IS_REPEAT};
use slopos_hid_core::keyboard::KeySet;
use slopos_kernel_services::driver_runtime::request_reschedule_from_interrupt;
use slopos_keymap_core::keycode::{self, NamedKey};
use slopos_keymap_core::keymap::KeyOutcome;
use slopos_keymap_core::scancode_set1::make_code;
use slopos_keymap_core::sysrq::{SysrqFsm, Verdict};
use slopos_keymap_core::{
    DeadKeyState, LayoutTable, ModSnapshot, ModTracker, Resolved, SERIALIZED_LEN, US_QWERTY,
    deserialize, resolve,
};
use slopos_mm::user_io_buf::memdup_user;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KBox, lock_class};

use crate::input_event::{has_keyboard_focus, input_route_key_full};
use crate::tty::vconsole;
use crate::tty::{active_tty, push_input};

/// Keyboards that feed the state at once, the i8042 among them.
pub const MAX_KEYBOARDS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyboardSource(u8);

impl KeyboardSource {
    pub const I8042: Self = Self(0);

    pub fn is_i8042(self) -> bool {
        self == Self::I8042
    }
}

struct State {
    held: [KeySet; MAX_KEYBOARDS],
    claimed: u8,
    /// How many keyboards hold each of the eight modifiers.
    holders: [u8; 8],
    mods: ModTracker,
    dead: DeadKeyState,
    sysrq: SysrqFsm,
    /// The boot log took the Esc held now, so its repeats and release are
    /// its too.
    esc_eaten: bool,
    /// `None` is the built-in [`US_QWERTY`].
    layout: Option<KBox<LayoutTable>>,
    #[cfg(feature = "test-hooks")]
    repeats: [u32; MAX_KEYBOARDS],
}

impl State {
    const fn new() -> Self {
        Self {
            held: [KeySet::EMPTY; MAX_KEYBOARDS],
            claimed: 1 << KeyboardSource::I8042.0,
            holders: [0; 8],
            mods: ModTracker::new(),
            dead: DeadKeyState::new(),
            sysrq: SysrqFsm::new(),
            esc_eaten: false,
            layout: None,
            #[cfg(feature = "test-hooks")]
            repeats: [0; MAX_KEYBOARDS],
        }
    }

    /// Whether the press or release changes what the machine holds.
    fn hold(&mut self, source: KeyboardSource, usage: u16, pressed: bool) -> bool {
        let held = &mut self.held[usize::from(source.0)];
        if held.contains(usage) == pressed {
            return false;
        }
        if pressed {
            held.insert(usage);
        } else {
            held.remove(usage);
        }
        let Some(modifier) = modifier_index(usage) else {
            return true;
        };
        let holders = &mut self.holders[modifier];
        let before = *holders;
        *holders = if pressed {
            before.saturating_add(1)
        } else {
            before.saturating_sub(1)
        };
        if (before == 0) != (*holders == 0) {
            self.mods.update(usage, pressed);
        }
        true
    }
}

fn modifier_index(usage: u16) -> Option<usize> {
    keycode::is_modifier(usage).then(|| usize::from(usage - keycode::KEY_LEFTCTRL))
}

fn is_lock(usage: u16) -> bool {
    matches!(
        usage,
        keycode::KEY_CAPSLOCK | keycode::KEY_NUMLOCK | keycode::KEY_SCROLLLOCK
    )
}

static STATE: SpinLock<State> = SpinLock::new(
    State::new(),
    lock_class!("keyboard.STATE", LOCK_LEVEL_RESOURCE),
);

/// The `LOCK_*` bits, read without the state's lock by whoever sets LEDs.
static LOCKS: AtomicU8 = AtomicU8::new(slopos_keymap_core::LOCK_NUM);

/// A slot for a keyboard that comes and goes; `None` when every one is taken.
pub fn claim() -> Option<KeyboardSource> {
    let mut state = STATE.lock();
    let free = (!state.claimed).trailing_zeros() as usize;
    if free >= MAX_KEYBOARDS {
        return None;
    }
    state.claimed |= 1 << free;
    state.held[free] = KeySet::EMPTY;
    Some(KeyboardSource(free as u8))
}

/// Releases every key `source` holds, then frees its slot.
pub fn release(source: KeyboardSource, timestamp_ms: u64) {
    let held = STATE.lock().held[usize::from(source.0)];
    for usage in held.keys() {
        key(source, usage, false, timestamp_ms);
    }
    if !source.is_i8042() {
        STATE.lock().claimed &= !(1 << source.0);
    }
}

/// The lock keys' state as `LOCK_*` bits.
pub fn locks() -> u8 {
    LOCKS.load(Ordering::Acquire)
}

/// One key transition from `source`. A press of a key the source already
/// holds is a repeat: it is delivered marked so and toggles no lock.
pub fn key(source: KeyboardSource, usage: u16, pressed: bool, timestamp_ms: u64) {
    let mut state = STATE.lock();
    let Some(slot) = state.held.get(usize::from(source.0)) else {
        return;
    };
    let repeat = pressed && slot.contains(usage);
    #[cfg(feature = "test-hooks")]
    if repeat {
        state.repeats[usize::from(source.0)] += 1;
    }
    state.hold(source, usage, pressed);
    if usage == keycode::KEY_ESC {
        if pressed && !repeat && slopos_ostd::fblog::handle_esc_press() {
            state.esc_eaten = true;
        }
        if state.esc_eaten {
            state.esc_eaten = pressed;
            return;
        }
    }
    let lock_toggled = pressed && !repeat && is_lock(usage);
    if lock_toggled {
        state.mods.update(usage, true);
    }
    let mods = state.mods.mods();
    let snap = state.mods.snapshot();
    LOCKS.store(snap.locks, Ordering::Release);

    if slopos_ostd::kconsole::enabled() {
        let arm_ms = slopos_ostd::kconsole::policy().arm_ms;
        match state.sysrq.feed(usage, pressed, mods, timestamp_ms, arm_ms) {
            Verdict::Pass => {}
            Verdict::Eat => return,
            Verdict::Run(command) => {
                drop(state);
                slopos_ostd::kconsole::request(command);
                return;
            }
        }
    }

    let resolved = if pressed && !keycode::is_modifier(usage) && !is_lock(usage) {
        let st = &mut *state;
        let table = st.layout.as_deref().unwrap_or(&US_QWERTY);
        resolve(table, usage, mods, st.mods.locks(), &mut st.dead)
    } else {
        Resolved::none()
    };
    drop(state);

    if pressed
        && mods.shift
        && !mods.ctrl
        && matches!(usage, keycode::KEY_PAGEUP | keycode::KEY_PAGEDOWN)
    {
        if usage == keycode::KEY_PAGEUP {
            vconsole::scroll_view_up(12);
        } else {
            vconsole::scroll_view_down(12);
        }
        return;
    }

    if lock_toggled {
        leds_changed();
    }

    deliver(usage, pressed, repeat, resolved, snap, timestamp_ms);
}

/// The i8042's LED exchange starts here; the USB thread brings every USB
/// keyboard's LEDs up.
fn leds_changed() {
    crate::ps2::keyboard::sync_leds();
    crate::usb::wake();
}

fn deliver(
    usage: u16,
    pressed: bool,
    repeat: bool,
    resolved: Resolved,
    snap: ModSnapshot,
    timestamp_ms: u64,
) {
    if resolved.flush != 0 {
        route_text(resolved.flush, snap.mods, timestamp_ms);
    }
    let (ascii, codepoint) = legacy_and_canonical(resolved.outcome, pressed);
    let mut flags = KEY_FLAG_HAS_CANONICAL;
    if keycode::is_keypad(usage) {
        flags |= KEY_FLAG_FROM_KEYPAD;
    }
    if repeat {
        flags |= KEY_FLAG_IS_REPEAT;
    }
    input_route_key_full(
        make_code(usage).unwrap_or(0),
        ascii,
        usage,
        codepoint,
        snap.mods,
        flags,
        pressed,
        timestamp_ms,
    );

    if pressed && !has_keyboard_focus() {
        let flush_byte = if resolved.flush != 0 && resolved.flush <= 0x7F {
            resolved.flush as u8
        } else {
            0
        };
        if flush_byte != 0 {
            push_input(active_tty(), flush_byte);
        }
        if ascii != 0 {
            push_input(active_tty(), ascii);
        }
        if flush_byte != 0 || ascii != 0 {
            request_reschedule_from_interrupt();
        }
    }
}

/// A bare text codepoint as a key press with no canonical keycode: the
/// accent a dead key leaves when nothing composes with it.
fn route_text(codepoint: u32, modifiers: u8, ts: u64) {
    let ascii = if codepoint <= 0x7F {
        codepoint as u8
    } else {
        0
    };
    input_route_key_full(
        0,
        ascii,
        0,
        codepoint,
        modifiers,
        KEY_FLAG_HAS_CANONICAL,
        true,
        ts,
    );
}

/// The legacy byte is ASCII only: a Latin-1 byte on the TTY stream would be
/// mojibake to UTF-8 readers, and 0x80..=0x88 are the navigation codes.
fn legacy_and_canonical(outcome: KeyOutcome, pressed: bool) -> (u8, u32) {
    if !pressed {
        return (0, 0);
    }
    match outcome {
        KeyOutcome::Text(cp) => {
            let ascii = if cp <= 0x7F { cp as u8 } else { 0 };
            (ascii, cp)
        }
        KeyOutcome::Named(nk) => (named_to_legacy_ascii(nk), 0),
        KeyOutcome::None => (0, 0),
    }
}

/// The codes the terminal and the shell turn into ANSI escape sequences.
fn named_to_legacy_ascii(nk: NamedKey) -> u8 {
    match nk {
        NamedKey::PageUp => 0x80,
        NamedKey::PageDown => 0x81,
        NamedKey::Up => 0x82,
        NamedKey::Down => 0x83,
        NamedKey::Left => 0x84,
        NamedKey::Right => 0x85,
        NamedKey::Home => 0x86,
        NamedKey::End => 0x87,
        NamedKey::Delete => 0x88,
        _ => 0,
    }
}

/// The old layout is freed outside the lock: a free may drain TLBs across
/// CPUs, which must not wait under a spinlock.
pub fn set_layout(layout: KBox<LayoutTable>) {
    let old = {
        let mut state = STATE.lock();
        state.dead.reset();
        state.layout.replace(layout)
    };
    drop(old);
}

/// Only the binary form, validated by `keymap-core` before it is installed.
pub fn load_layout_from_user(data_ptr: u64, len: usize) -> Result<(), Errno> {
    if data_ptr == 0 || len != SERIALIZED_LEN {
        return Err(Errno::EINVAL);
    }
    let bytes = memdup_user(data_ptr, len, SERIALIZED_LEN)?;
    let mut layout = KBox::<LayoutTable>::zeroed().map_err(|_| Errno::ENOMEM)?;
    deserialize(bytes.as_slice(), &mut layout).map_err(|_| Errno::EINVAL)?;
    set_layout(layout);
    Ok(())
}

/// Into a kernel buffer, which the syscall copies out.
pub fn layout_name(out: &mut [u8]) -> usize {
    let state = STATE.lock();
    let table = state.layout.as_deref().unwrap_or(&US_QWERTY);
    let bytes = table.name_str().as_bytes();
    let n = bytes.len().min(out.len());
    out[..n].copy_from_slice(&bytes[..n]);
    n
}

/// The `MODIFIER_*` bits.
pub fn get_modifier_state() -> u8 {
    STATE.lock().mods.snapshot().mods
}

/// Whether `source` holds `usage`.
#[cfg(feature = "test-hooks")]
pub fn holds(source: KeyboardSource, usage: u16) -> bool {
    STATE
        .lock()
        .held
        .get(usize::from(source.0))
        .is_some_and(|held| held.contains(usage))
}

/// Presses `source` has repeated since boot.
#[cfg(feature = "test-hooks")]
pub fn repeats(source: KeyboardSource) -> u32 {
    STATE
        .lock()
        .repeats
        .get(usize::from(source.0))
        .copied()
        .unwrap_or(0)
}

/// The state as boot left it, keeping the claimed slots; the old layout is
/// freed outside the lock.
#[cfg(feature = "test-hooks")]
pub fn reset_for_test() {
    let old = {
        let mut state = STATE.lock();
        state.held = [KeySet::EMPTY; MAX_KEYBOARDS];
        state.holders = [0; 8];
        state.mods = ModTracker::new();
        state.dead = DeadKeyState::new();
        state.sysrq = SysrqFsm::new();
        state.esc_eaten = false;
        LOCKS.store(state.mods.snapshot().locks, Ordering::Release);
        state.layout.take()
    };
    drop(old);
}
