---
name: slopos-usb-input
description: >
  USB (the xHCI driver, the device tree, usb-storage, usb-net, usb-hid) and the
  machine's one keyboard state, one cursor and keyboard layouts. Use when a
  task touches `drivers/src/usb`, `usb-core`, `hid-core`, `keymap-core`,
  `drivers/src/keyboard.rs`, `drivers/src/input_event.rs`, `drivers/src/ps2`,
  `chrome-core/src/pointer.rs`, `assets/keymaps`, the `keymap` program or
  `scripts/test_usb.py`, or mentions xHCI, hubs, sticks, `usb_driver!`, HID,
  key repeat, lock lights, the pointer or `just test-usb`.
---

## Read first

- `docs/architecture/usb`: how controllers, enumeration, the three drivers and removal work
- `docs/architecture/drivers`: binding, the keyboard state, the cursor, layouts, the pointer overlay
- `docs/development/adding-a-driver`: the handles and steps for a new USB driver

## Rules a change must keep

- Formats and state machines go in `usb-core` and `hid-core`, which allocate
  nothing and own no hardware; a parser of device input bounds every read and
  joins the mutation loops.
- The event ring is the only completion path. A `ReportSink` or
  `TransferSink` runs where it is drained: it may not block, allocate or log.
- Nothing slow runs under a spinlock, and the `usb` thread never waits for a
  device. Waiting belongs to probes and removals on `usb-bind`.
- Removal is ordered: gone, endpoints stopped, transfers failed, claims
  released and removals run, slot disabled, pages freed. Code that outlives
  the binding touches no ring or page of a device that is gone.
- Clear a halt on both sides (controller toggle and device toggle). QEMU's
  sticks keep no toggles, so prove recovery against the simulated controller.
- A keyboard or pointer driver reports `(source, usage, pressed)` or motion
  and keeps no modifier, lock, layout or cursor state. It releases its slot
  when the device leaves.
- Nothing masks the i8042's interrupt line or polls its data port after
  boot; the light exchange is carried by the interrupt.
- `EPERM` from `cursor_set_image` says the caller lacks the screen, not that
  the display has no overlay. The compositor paints a pointer only after the
  display refused an image.
- The kernel takes layouts only as the checked binary table. Init loads the
  wanted layout; `keymap` asks init and never loads one itself.
- No code or prose from Linux or QEMU; comments cite spec sections.
- A new `just test-usb` check adds its `USB-TEST:` line to both
  `drivers/src/tests/usb_tests.rs` and `scripts/test_usb.py`.

## Checks

- `just test-host`: `usb-core`, `hid-core` and `keymap-core` on the host, with the simulated controller
- `just test-usb`: both QEMU controllers, plug and pull, keys, cursor, stick, NIC, shutdown
- `just test 'slopos_drivers::tests::keyboard_tests::*'`: the keyboard state (likewise `input_event_tests`, `i8042_tests`)

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
