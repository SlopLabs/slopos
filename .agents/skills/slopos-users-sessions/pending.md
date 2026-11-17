# Pending for the docs

- `docs/architecture/users`, `docs/architecture/init`: `/usr/local` left
  `ROOT_LAYOUT` and `PERSON_TREES`; the owners record names the home and
  `/src`, and a line naming any other tree is ignored. Nothing is handed over:
  `root_layout::{share, settled}`, `/run/slopos-local` and
  `owners.sharing` are gone, and init's boot lays out the root alone. The
  first person's home, where something makes it, is `0755` while the
  accounts record holds at most one person (`root_layout::alone`; a root
  with none counts as one) and `0700` otherwise; `build_fs_image.sh` makes a
  kept image's missing home `0700`. The `users add` that makes the second
  person closes the first home (`root_layout::close_home`) before it records
  anyone, or adds nobody, and asks for no restart. `dac_test` holds it with
  `the_first_home_closes_for_a_second_person`; `check_fs_image.sh` no longer
  walks a system-owned prefix (16 self-test checks).
