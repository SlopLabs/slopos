//! What a pointer's report says: its buttons, its X and Y, relative or
//! absolute, and its wheel and horizontal pan.

use crate::boot;
use crate::descriptor::{Descriptor, Field, Kind};
use crate::usage::{self, id_of, page, page_of};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Relative(i32),
    /// A position within the logical range.
    Absolute {
        value: i32,
        min: i32,
        max: i32,
    },
}

impl Axis {
    /// The position onto `0..extent`, clamped; a relative axis has none.
    pub fn onto(self, extent: i32) -> Option<i32> {
        let Axis::Absolute { value, min, max } = self else {
            return None;
        };
        if max <= min || extent <= 0 {
            return None;
        }
        let span = i64::from(max) - i64::from(min);
        let at = (i64::from(value.clamp(min, max)) - i64::from(min)) * i64::from(extent - 1);
        Some((at / span) as i32)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Motion {
    /// Button `n` at bit `n - 1`, for buttons 1 to 32.
    pub buttons: u32,
    /// The buttons this report says anything of, held or not.
    pub reported: u32,
    pub x: Option<Axis>,
    pub y: Option<Axis>,
    /// Detents away from the user.
    pub wheel: i32,
    /// Detents to the right.
    pub pan: i32,
}

fn button_bit(usage: u32) -> u32 {
    match id_of(usage) {
        id @ 1..=32 if page_of(usage) == page::BUTTON => 1 << (id - 1),
        _ => 0,
    }
}

fn in_pointer(field: &Field) -> bool {
    matches!(field.application, usage::MOUSE | usage::POINTER)
}

/// Whether any input report carries an axis or button [`decode`] reads.
pub fn carries_pointer(desc: &Descriptor<'_>) -> bool {
    desc.every_element(Kind::Input).any(|e| {
        in_pointer(e.field) && (matches!(e.usage, usage::X | usage::Y) || button_bit(e.usage) != 0)
    }) || desc.fields().iter().any(|f| {
        f.kind == Kind::Input
            && !f.is_variable()
            && in_pointer(f)
            && desc.array_page(f) == page::BUTTON
    })
}

/// A report-protocol report of a mouse or pointer collection, which leaves
/// out game pads and joysticks. `None` when report `id` carries no pointer
/// usage or the payload is shorter than it.
pub fn decode(desc: &Descriptor<'_>, id: u8, payload: &[u8]) -> Option<Motion> {
    if payload.len() < desc.payload_bytes(Kind::Input, id) {
        return None;
    }
    let mut motion = Motion::default();
    let mut any = false;
    for element in desc
        .elements(Kind::Input, id)
        .filter(|e| in_pointer(e.field))
    {
        let bit = button_bit(element.usage);
        motion.reported |= bit;
        let wanted = bit != 0
            || matches!(
                element.usage,
                usage::X | usage::Y | usage::WHEEL | usage::AC_PAN
            );
        if !wanted {
            continue;
        }
        any = true;
        let Some(value) = element.reading(payload) else {
            continue;
        };
        let field = element.field;
        let axis = if field.is_relative() {
            Axis::Relative(value)
        } else {
            Axis::Absolute {
                value,
                min: field.logical_min,
                max: field.logical_max,
            }
        };
        match element.usage {
            usage::X if motion.x.is_none() => motion.x = Some(axis),
            usage::Y if motion.y.is_none() => motion.y = Some(axis),
            usage::WHEEL if field.is_relative() => motion.wheel = value,
            usage::AC_PAN if field.is_relative() => motion.pan = value,
            _ if value != 0 => motion.buttons |= bit,
            _ => {}
        }
    }
    for field in desc
        .arrays(Kind::Input, id)
        .filter(|f| in_pointer(f) && desc.array_page(f) == page::BUTTON)
    {
        any = true;
        let last = field.logical_max.min(field.logical_min.saturating_add(31));
        for value in field.logical_min..=last {
            motion.reported |= desc.array_usage(field, value).map_or(0, button_bit);
        }
        for index in 0..field.count {
            if let Some(usage) = field
                .value(payload, index)
                .and_then(|value| desc.array_usage(field, value))
            {
                motion.buttons |= button_bit(usage);
            }
        }
    }
    any.then_some(motion)
}

/// A boot-protocol report.
pub fn boot(report: &[u8]) -> Option<Motion> {
    let mouse = boot::Mouse::parse(report)?;
    Some(Motion {
        buttons: mouse.buttons.into(),
        reported: 0b111,
        x: Some(Axis::Relative(mouse.dx.into())),
        y: Some(Axis::Relative(mouse.dy.into())),
        ..Motion::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::{self, DATA_ARRAY, DATA_VAR, DATA_VAR_REL, Desc};
    use crate::descriptor::tests::{Storage, mutate};

    #[test]
    fn a_mouse_reports_relative_motion_buttons_and_wheel() {
        let mut s = Storage::new(8, 8);
        let parsed = s.parse(&build::mouse()).unwrap();
        let d = parsed.descriptor(&s.fields, &s.usages);
        let (id, payload) = d.split(&[2, 0x05, 0xfb, 0x03, 0xff]).unwrap();
        let motion = decode(&d, id, payload).unwrap();
        assert_eq!(
            motion,
            Motion {
                buttons: 0x05,
                reported: 0x07,
                x: Some(Axis::Relative(-5)),
                y: Some(Axis::Relative(3)),
                wheel: -1,
                pan: 0,
            }
        );
        assert_eq!(decode(&d, 1, payload), None, "report 1 carries nothing");
        assert_eq!(decode(&d, 2, &payload[..3]), None, "a short report");
    }

    #[test]
    fn a_tablet_maps_its_logical_range_onto_the_screen() {
        let mut s = Storage::new(8, 8);
        let parsed = s.parse(&build::tablet()).unwrap();
        let d = parsed.descriptor(&s.fields, &s.usages);
        let motion = decode(&d, 0, &[0x01, 0xff, 0x7f, 0x00, 0x40, 0x00]).unwrap();
        assert_eq!(motion.buttons, 1);
        let (x, y) = (motion.x.unwrap(), motion.y.unwrap());
        assert_eq!(x.onto(1024), Some(1023));
        assert_eq!(y.onto(768), Some(383));
        assert_eq!(Axis::Relative(5).onto(10), None);
        let degenerate = Axis::Absolute {
            value: 1,
            min: 3,
            max: 3,
        };
        assert_eq!(degenerate.onto(10), None);
        let below = Axis::Absolute {
            value: -9,
            min: 0,
            max: 10,
        };
        assert_eq!(below.onto(11), Some(0));
    }

    #[test]
    fn button_arrays_and_pan_are_read() {
        let desc = Desc::default()
            .page(0x01)
            .usage(0x02)
            .collection(1)
            .id(1)
            .page(0x09)
            .range(1, 8)
            .logical(1, 8)
            .size(4)
            .count(2)
            .input(DATA_ARRAY)
            .page(0x0c)
            .usage_extended(usage::AC_PAN)
            .logical(-127, 127)
            .size(8)
            .count(1)
            .input(DATA_VAR_REL)
            .id(2)
            .usage_extended(usage::AC_PAN)
            .input(DATA_VAR_REL)
            .end()
            .page(0x01)
            .usage(0x05)
            .collection(1)
            .id(3)
            .usage(0x30)
            .logical(0, 255)
            .input(DATA_VAR)
            .end()
            .0;
        let mut s = Storage::new(8, 8);
        let parsed = s.parse(&desc).unwrap();
        let d = parsed.descriptor(&s.fields, &s.usages);
        let motion = decode(&d, 1, &[0x52, 0x02]).unwrap();
        assert_eq!(motion.buttons, 1 << 1 | 1 << 4);
        assert_eq!(motion.reported, 0xff);
        assert_eq!(motion.pan, 2);
        assert_eq!(motion.x, None);
        let pan = decode(&d, 2, &[0xff]).unwrap();
        assert_eq!(
            (pan.pan, pan.reported),
            (-1, 0),
            "a pan alone reports no button"
        );
        assert_eq!(
            decode(&d, 3, &[0x80]),
            None,
            "a game pad's stick moves no cursor"
        );
        assert!(carries_pointer(&d));
    }

    #[test]
    fn a_boot_mouse_is_relative() {
        assert_eq!(
            boot(&[0x01, 0x02, 0xfe]),
            Some(Motion {
                buttons: 1,
                reported: 0b111,
                x: Some(Axis::Relative(2)),
                y: Some(Axis::Relative(-2)),
                ..Motion::default()
            })
        );
        assert_eq!(boot(&[0]), None);
    }

    #[test]
    fn mutated_reports_never_panic_or_read_past_their_bytes() {
        let mut s = Storage::new(8, 8);
        let parsed = s.parse(&build::tablet()).unwrap();
        let d = parsed.descriptor(&s.fields, &s.usages);
        mutate(&[0x07, 0x34, 0x12, 0x78, 0x56, 0x81], |report| {
            let _ = boot(report);
            if let Some(motion) = decode(&d, 0, report) {
                let _ = motion.x.and_then(|x| x.onto(i32::MAX));
                let _ = motion.y.and_then(|y| y.onto(1));
            }
        });
    }
}
