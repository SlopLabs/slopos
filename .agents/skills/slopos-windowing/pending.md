# Pending for the docs

- `docs/architecture/userland-windowing`, the terminal: a frame repaints the
  cells whose look changed, not the cells written. `terminal_core::shown`
  keeps the glyph and colours every cell was last drawn with, the cursor and
  the selection applied; the grid's damage only says where to compare, and a
  frame in which nothing looks different is not presented. A change to how a
  cell looks that the grid does not record (the blink phase, the selection)
  must still be added to the pending damage.
- `docs/architecture/userland-windowing`, the terminal: it honours the
  synchronized update, DECSET 2026. Between `ESC[?2026h` and `ESC[?2026l`
  no frame is presented, for at most 150 ms; a program that never ends its
  update loses the hold. A pty delivers a write in pieces a terminal can
  paint between, so a program that erases what it is about to write again
  does so inside one. The shell's line editor writes each redraw as one
  `write` wrapped in it.
