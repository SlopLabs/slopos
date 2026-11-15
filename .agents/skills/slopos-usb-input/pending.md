# Pending for the docs

- `docs/architecture/drivers` (layouts): the kernel no longer answers the
  active layout's name. Init writes the name of each layout it loads to
  `/run/keymap` (uid 0's, by rename); `keymap::current_name` reads it, and
  its absence means the kernel's built-in `us`. A program that loads a
  layout itself with `keymap_load` leaves the file stale.
