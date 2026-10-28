//! Raw input events, queued for the task that holds the input sink.

pub mod keycode;
pub mod layout;

pub const MAX_INPUT_TASKS: usize = 32;

pub const MAX_EVENTS_PER_TASK: usize = 64;
pub const CLIPBOARD_MAX_SIZE: usize = 4096;

/// Keyboard modifier bitfield, snapshotted into every key event.
/// Follows the Wayland `wlr_keyboard_modifier` convention.
pub const MODIFIER_SHIFT: u8 = 1 << 0;
pub const MODIFIER_CTRL: u8 = 1 << 1;
pub const MODIFIER_ALT: u8 = 1 << 2;
pub const MODIFIER_SUPER: u8 = 1 << 3;
pub const MODIFIER_CAPS_LOCK: u8 = 1 << 4;
/// Num Lock active. Carried in the key-event modifier snapshot so a consumer can
/// resolve keypad keys (digit vs navigation) without tracking lock state itself.
pub const MODIFIER_NUM_LOCK: u8 = 1 << 5;
pub const MODIFIER_SCROLL_LOCK: u8 = 1 << 6;
/// AltGr (right Alt) held; `MODIFIER_ALT` is set too. Distinguishes
/// AltGr-as-text (`AltGr+2 = @`) from a left-Alt shortcut chord.
pub const MODIFIER_ALTGR: u8 = 1 << 7;

/// This press is an auto-repeat, not a fresh physical key-down.
pub const KEY_FLAG_IS_REPEAT: u8 = 1 << 0;
pub const KEY_FLAG_FROM_KEYPAD: u8 = 1 << 1;

/// Axis identifiers for PointerAxis events (Wayland wl_pointer.axis convention)
pub const POINTER_AXIS_VERTICAL: u32 = 0;
pub const POINTER_AXIS_HORIZONTAL: u32 = 1;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputEventType {
    #[default]
    KeyPress = 0,
    KeyRelease = 1,
    PointerMotion = 2,
    PointerButtonPress = 3,
    PointerButtonRelease = 4,
    /// Pointer axis (scroll wheel) event.
    PointerAxis = 5,
}

/// Input event data (union-like structure)
///
/// For key events the two 32-bit words are bit-packed (low→high):
/// ```text
///   data0[ 0: 8]  KEY_FLAG_* flags             -- key_flags()
///   data0[ 8:16]  MODIFIER_* snapshot          -- key_modifiers()
///   data0[16:32]  zero
///   data1[ 0:16]  HID usage keycode            -- key_keycode()
///   data1[16:32]  text codepoint (BMP; 0=none) -- key_codepoint()
/// ```
/// For pointer motion: data0 is x coordinate, data1 is y coordinate
/// For pointer button: data0 contains button code
/// For pointer axis: data0 is a `POINTER_AXIS_*`, data1 the value120 delta
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct InputEventData {
    pub data0: u32,
    pub data1: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InputEvent {
    pub event_type: InputEventType,
    pub _padding: [u8; 3],
    /// Timestamp in milliseconds since boot.
    pub timestamp_ms: u64,
    pub data: InputEventData,
}

impl Default for InputEvent {
    fn default() -> Self {
        Self {
            event_type: InputEventType::KeyPress,
            _padding: [0; 3],
            timestamp_ms: 0,
            data: InputEventData::default(),
        }
    }
}

impl InputEvent {
    /// `keycode` is a HID Keyboard/Keypad usage (see [`keycode`](self::keycode));
    /// `codepoint` is the produced Unicode scalar (BMP only — the high 16 bits
    /// are truncated).
    pub fn key(
        pressed: bool,
        keycode: u16,
        codepoint: u32,
        modifiers: u8,
        flags: u8,
        timestamp_ms: u64,
    ) -> Self {
        Self {
            event_type: if pressed {
                InputEventType::KeyPress
            } else {
                InputEventType::KeyRelease
            },
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: (flags as u32) | ((modifiers as u32) << 8),
                data1: (keycode as u32) | ((codepoint & 0xFFFF) << 16),
            },
        }
    }

    pub fn pointer_motion(x: i32, y: i32, timestamp_ms: u64) -> Self {
        Self {
            event_type: InputEventType::PointerMotion,
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: x as u32,
                data1: y as u32,
            },
        }
    }

    pub fn pointer_button(pressed: bool, button: u8, timestamp_ms: u64) -> Self {
        Self {
            event_type: if pressed {
                InputEventType::PointerButtonPress
            } else {
                InputEventType::PointerButtonRelease
            },
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: button as u32,
                data1: 0,
            },
        }
    }

    /// `axis` is `POINTER_AXIS_VERTICAL` or `POINTER_AXIS_HORIZONTAL`;
    /// `value_v120` is in value120 units (±120 = one wheel click, + is down/right).
    pub fn pointer_axis(axis: u32, value_v120: i32, timestamp_ms: u64) -> Self {
        Self {
            event_type: InputEventType::PointerAxis,
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: axis,
                data1: value_v120 as u32,
            },
        }
    }

    #[inline]
    pub fn axis_id(&self) -> u32 {
        self.data.data0
    }

    #[inline]
    pub fn axis_value_v120(&self) -> i32 {
        self.data.data1 as i32
    }

    #[inline]
    pub fn key_flags(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }

    /// The `MODIFIER_*` snapshot captured when the key event was produced.
    #[inline]
    pub fn key_modifiers(&self) -> u8 {
        ((self.data.data0 >> 8) & 0xFF) as u8
    }

    /// HID usage keycode; 0 for a bare codepoint, such as the accent a dead key
    /// leaves.
    #[inline]
    pub fn key_keycode(&self) -> u16 {
        (self.data.data1 & 0xFFFF) as u16
    }

    /// The produced text codepoint; 0 for a key that makes no text.
    #[inline]
    pub fn key_codepoint(&self) -> u32 {
        self.data.data1 >> 16
    }

    #[inline]
    pub fn key_is_repeat(&self) -> bool {
        self.key_flags() & KEY_FLAG_IS_REPEAT != 0
    }

    #[inline]
    pub fn key_from_keypad(&self) -> bool {
        self.key_flags() & KEY_FLAG_FROM_KEYPAD != 0
    }

    #[inline]
    pub fn pointer_x(&self) -> i32 {
        self.data.data0 as i32
    }

    #[inline]
    pub fn pointer_y(&self) -> i32 {
        self.data.data1 as i32
    }

    #[inline]
    pub fn pointer_button_code(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::keycode::*;
    use super::*;

    #[test]
    fn key_round_trips_every_field() {
        let flags = KEY_FLAG_FROM_KEYPAD;
        let mods = MODIFIER_SHIFT | MODIFIER_CTRL | MODIFIER_ALTGR;
        let e = InputEvent::key(true, KEY_KP_1, '1' as u32, mods, flags, 1000);
        assert_eq!(e.event_type, InputEventType::KeyPress);
        assert_eq!(e.timestamp_ms, 1000);
        assert_eq!(e.key_flags(), flags);
        assert_eq!(e.key_modifiers(), mods);
        assert_eq!(e.key_keycode(), KEY_KP_1);
        assert_eq!(e.key_codepoint(), '1' as u32);
        assert!(e.key_from_keypad());
        assert!(!e.key_is_repeat());
    }

    #[test]
    fn key_packs_flags_low_and_modifiers_above_them() {
        let e = InputEvent::key(false, KEY_Q, 0x00E9, MODIFIER_ALTGR, KEY_FLAG_IS_REPEAT, 0);
        assert_eq!(e.event_type, InputEventType::KeyRelease);
        assert_eq!(
            e.data.data0,
            KEY_FLAG_IS_REPEAT as u32 | (MODIFIER_ALTGR as u32) << 8
        );
        assert_eq!(e.data.data1, KEY_Q as u32 | 0x00E9 << 16);
        assert!(e.key_is_repeat());
    }

    #[test]
    fn a_key_that_makes_no_text_has_no_codepoint() {
        let e = InputEvent::key(true, KEY_PAGEUP, 0, 0, 0, 0);
        assert_eq!(e.key_keycode(), KEY_PAGEUP);
        assert_eq!(e.key_codepoint(), 0);
    }

    #[test]
    fn a_codepoint_past_the_bmp_keeps_its_low_half() {
        let e = InputEvent::key(true, KEY_A, 0x1F600, 0, 0, 0);
        assert_eq!(e.key_codepoint(), 0xF600);
        assert_eq!(e.key_keycode(), KEY_A);
    }

    #[test]
    fn keycode_classifiers() {
        assert!(is_modifier(KEY_LEFTCTRL));
        assert!(is_modifier(KEY_RIGHTMETA));
        assert!(!is_modifier(KEY_A));
        assert!(is_keypad(KEY_KP_0));
        assert!(is_keypad(KEY_KP_SLASH));
        assert!(!is_keypad(KEY_NUMLOCK)); // NumLock is not a keypad text key
        assert!(!is_keypad(KEY_HOME));
    }
}
