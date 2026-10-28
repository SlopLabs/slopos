//! The compositor's events a window acts on.

use slopos_protocol::types::Event as ProtocolEvent;

#[derive(Clone, Copy, Debug)]
pub enum Event {
    PointerMotion {
        x: i32,
        y: i32,
    },
    /// The pointer left the window, or something now covers it.
    PointerLeave,
    PointerPress {
        button: u8,
    },
    PointerRelease {
        button: u8,
    },
    /// `axis` a `POINTER_AXIS_*`; `value_v120` in 120ths of a notch.
    PointerAxis {
        axis: u32,
        value_v120: i32,
    },
    KeyPress {
        /// The USB HID usage, whatever the layout.
        keycode: u16,
        /// The text the layout and modifiers make; 0 for none.
        codepoint: u32,
        modifiers: u8,
    },
    KeyRelease {
        keycode: u16,
        codepoint: u32,
        modifiers: u8,
    },
    CloseRequest,
    Configure {
        width: u32,
        height: u32,
    },
    /// The selection holds `len` bytes; answer with
    /// [`crate::Clipboard::accept_offer`].
    ClipboardOffer {
        len: u32,
    },
    /// The destination buffer now holds `len` bytes; take them with
    /// [`crate::Clipboard::take`].
    ClipboardData {
        len: u32,
    },
}

impl Event {
    pub fn from_protocol(evt: &ProtocolEvent) -> Option<Self> {
        match evt {
            ProtocolEvent::PointerEnter { x, y, .. }
            | ProtocolEvent::PointerMotion { x, y, .. } => {
                Some(Event::PointerMotion { x: *x, y: *y })
            }
            ProtocolEvent::PointerLeave { .. } => Some(Event::PointerLeave),
            ProtocolEvent::PointerButton {
                button, pressed, ..
            } => {
                if *pressed {
                    Some(Event::PointerPress {
                        button: *button as u8,
                    })
                } else {
                    Some(Event::PointerRelease {
                        button: *button as u8,
                    })
                }
            }
            ProtocolEvent::PointerAxis { axis, value, .. } => Some(Event::PointerAxis {
                axis: *axis,
                value_v120: *value,
            }),
            ProtocolEvent::Key {
                keycode,
                codepoint,
                modifiers,
                pressed,
                ..
            } => {
                let keycode = u16::try_from(*keycode).unwrap_or(0);
                let codepoint = *codepoint;
                let modifiers = u8::try_from(*modifiers).unwrap_or(0);
                Some(match pressed {
                    true => Event::KeyPress {
                        keycode,
                        codepoint,
                        modifiers,
                    },
                    false => Event::KeyRelease {
                        keycode,
                        codepoint,
                        modifiers,
                    },
                })
            }
            ProtocolEvent::PasteReady { len, .. } => Some(Event::ClipboardOffer { len: *len }),
            ProtocolEvent::PasteResult { len, .. } => Some(Event::ClipboardData { len: *len }),
            ProtocolEvent::Close { .. } => Some(Event::CloseRequest),
            ProtocolEvent::Configure { width, height, .. } => Some(Event::Configure {
                width: *width,
                height: *height,
            }),
            _ => None,
        }
    }
}
