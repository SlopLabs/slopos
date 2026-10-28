//! `slopos-terminal-core`'s input model, and `classify`, which maps a
//! window's events onto it.

pub use slopos_terminal_core::input::*;

use slopos_abi::input::POINTER_AXIS_VERTICAL;
use slopos_windowing::Event;

pub fn classify(event: Event) -> CompositorEvent {
    match event {
        Event::KeyPress {
            keycode,
            codepoint,
            modifiers,
        } => CompositorEvent::Key(KeyPress {
            keycode,
            codepoint,
            mods: modifiers,
        }),
        Event::KeyRelease { .. } => CompositorEvent::Ignored,
        Event::Modifiers { modifiers } => CompositorEvent::Modifiers(modifiers),
        Event::PointerMotion { x, y } => CompositorEvent::PointerMotion(x, y),
        Event::PointerEnter { x, y } => CompositorEvent::PointerEnter(x, y),
        Event::PointerLeave => CompositorEvent::PointerLeave,
        Event::PointerPress { button } => CompositorEvent::PointerButton {
            pressed: true,
            code: button,
        },
        Event::PointerRelease { button } => CompositorEvent::PointerButton {
            pressed: false,
            code: button,
        },
        Event::Configure { width, height } => CompositorEvent::Resize(width as i32, height as i32),
        Event::PointerAxis { axis, value_v120 } if axis == POINTER_AXIS_VERTICAL => {
            CompositorEvent::Scroll(value_v120)
        }
        Event::PointerAxis { .. } => CompositorEvent::Ignored,
        Event::CloseRequest => CompositorEvent::Close,
        Event::ClipboardOffer { len } => CompositorEvent::PasteReady(len),
        Event::ClipboardData { len } => CompositorEvent::PasteResult(len),
    }
}
