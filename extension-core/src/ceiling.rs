//! What any extension can ever be granted: the base's ceiling, sealed in the
//! base at [`CEILING`](crate::CEILING), one `ceiling` line of capability
//! names. Whatever it says, no extension is granted a capability in
//! [`NEVER`] or one every program holds.

use slopos_abi::caps;

use crate::words::{LineError, caps_words, lines, once};

/// What would let an extension change the base, the boot chain, the
/// extensions themselves, who a process is, or the seats.
pub const NEVER: u64 = caps::BASE_UPDATE
    | caps::EXTENSION_UPDATE
    | caps::SEAL
    | caps::BOOT_ENTRY
    | caps::SET_ID
    | caps::PROC_SIGNAL
    | caps::TEST_HARNESS
    | caps::DISPLAY_SEAT
    | caps::INPUT_SEAT
    | caps::SEAT_SWITCH;

pub fn parse(text: &str) -> Result<u64, LineError> {
    let mut ceiling = None;
    for (line, indented, keyword, rest) in lines(text) {
        let fail = |msg| LineError { line, msg };
        if indented || keyword != "ceiling" {
            return Err(fail("the file is one ceiling line"));
        }
        let mask = caps_words(rest).map_err(fail)?;
        if mask & (NEVER | caps::UNIVERSAL) != 0 {
            return Err(fail("the ceiling names what no extension is granted"));
        }
        once(&mut ceiling, mask).map_err(fail)?;
    }
    ceiling.ok_or(LineError {
        line: 0,
        msg: "the file names no ceiling",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED: &str = include_str!("../../assets/privd/extensions");

    #[test]
    fn the_shipped_ceiling_names_what_the_plan_does() {
        assert_eq!(
            parse(SHIPPED),
            Ok(caps::POWER
                | caps::MOUNT
                | caps::RAW_BLOCK
                | caps::NET_ADMIN
                | caps::CONSOLE_CONFIG
                | caps::SYS_INSPECT
                | caps::CLOCK
                | caps::DAC_OVERRIDE
                | caps::SCHED_HIGH)
        );
    }

    #[test]
    fn a_ceiling_naming_what_is_never_granted_is_refused() {
        for name in [
            "Seal",
            "BaseUpdate",
            "ExtensionUpdate",
            "SeatSwitch",
            "SetId",
            "ConsoleIo",
            "Fate",
        ] {
            assert_eq!(
                parse(&alloc::format!("ceiling Power {name}\n")).map_err(|e| e.line),
                Err(1),
                "{name}"
            );
        }
        assert_eq!(parse("# none\n").map_err(|e| e.line), Err(0));
        assert_eq!(
            parse("ceiling Power\nceiling Mount\n").map_err(|e| e.line),
            Err(2)
        );
        assert_eq!(parse("ceiling\n"), Ok(0));
    }
}
