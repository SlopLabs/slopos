# Pending for the docs

- `docs/architecture/desktop`: the apps outside the base are the editor, the
  file manager, the image viewer, Breach and the widget gallery; all but the
  gallery ship an app entry and icon. Breach is a first-person shooter whose
  levels, simulation and software raycaster are `shooter-core`, host-tested
  under `just test-host`. `xtask lint app-deps` lets an app depend on
  `slopos-shooter-core` beside `slopos-appkit`, `slopos-editor-core` and
  `slopos-image`, and `shooter-core` is a userland-side crate, outside the
  kernel's unsafe discipline.
