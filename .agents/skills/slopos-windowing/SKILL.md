---
name: slopos-windowing
description: >
  How a SlopOS program's window buffers reach the screen: the compositor's
  client protocol, double buffering, damage and frame pacing. Use when a task
  touches `slop-protocol/`, `windowing/`, `gfx/src/swap.rs`,
  `gfx/src/damage.rs`, `appkit/src/run.rs`, the compositor's `protocol.rs`,
  `renderer.rs` or frame loop, or mentions `SurfaceAttach`, `BufferRelease`,
  `FrameDone`, buffer serials, damage rects, `SoftSurface`, tearing, flicker
  or half-drawn frames.
---

## Read first

- `docs/architecture/userland-windowing`: how pixels, frames and input move between program and compositor
- `docs/abi/window-protocol`: every message, field, tag, limit and serial rule
- `docs/architecture/desktop`: what appkit gives an app on top

## Rules a change must keep

- Never draw into a buffer the compositor may read: the one committed last,
  and the one before it until its `BufferRelease`. Begin frames only through
  `SoftSurface::frame` or `frame_over_shown`; never fall back to the shown
  buffer when none is free, wait for the release.
- A buffer is named by slot and serial. `replace_buffers` (resize) takes the
  next serial; a release with another serial frees nothing. Do not count
  releases or time one out as lost.
- A change to any message, field, tag or meaning is wire ABI: update
  `codec.rs`, both ends, `protocol_test`'s pinned tags and lengths, and
  `docs/abi/window-protocol` together, and take the next `PROTOCOL_VERSION`.
- The fd travels only with a buffer's first attach (`has_fd`); a re-select
  must not pop a later message's fd from the queue.
- Every commit asks for `FrameDone` and the next frame waits for it, with a
  bounded wait, since a hidden surface gets none. Do not remove the bound or
  the wait.
- The compositor serves each client on a oneshot poll taken again after each
  read (multishot fires only on a readiness change and misses a commit), and
  flushes that client's events right after.
- A commit's damage adds to damage not yet on screen; it is cleared only for
  surfaces whose frame was presented.
- Input is taken once a tick; an early frame between ticks shows a commit
  only after the last frame had a frame's time on screen.
- A partial repaint must equal the whole one pixel for pixel: a widget paints
  nothing outside its bounds when lit (`lit_damage`), and a key or wheel turn
  no widget took makes the next frame whole.
- Buffer mappings are keyed by window, surface generation and slot, never by
  fd number.

## Checks

- `just test-host`: `gfx` swap and damage, appkit, `settings-app`'s partial-repaint test, `chrome-core`
- `just test '*utest_protocol*'`: handshake, pinned wire, release by slot and serial
- `just test '*utest_apps,*utest_appkit'`: apps and appkit against a compositor

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
