# Pending for the docs

- `docs/architecture/desktop`: the apps outside the base are the editor, the
  file manager, the image viewer, Breach and the widget gallery; all but the
  gallery ship an app entry and icon. Breach is a first-person shooter whose
  levels, simulation and software raycaster are `shooter-core`, host-tested
  under `just test-host`. `xtask lint app-deps` lets an app depend on
  `slopos-shooter-core` beside `slopos-appkit`, `slopos-editor-core` and
  `slopos-image`, and `shooter-core` is a userland-side crate, outside the
  kernel's unsafe discipline.
- `docs/architecture/desktop`, the System Monitor: the *Processes* tab is a
  table (name led by an icon for a person's, the system's or the kernel's
  own; user, CPU, memory, id) beside a pane on the chosen process: a minute
  of its CPU and memory, which `Monitor` carries per `TaskKey`, what started
  it, its threads, CPU time and the most memory it held. The pane
  says what ending it would come to before anyone asks (`Ending`, read once
  when the row is chosen by the questions `choose_to_end` asks) and offers
  *End task…* only where that can end it; asking still decides afresh. A
  process the search hides is selected no longer. `endtask::of_a_person` is
  the one test of whose a task is, for `endable` and the row's icon alike.
  `just ui-shots apps` also saves `system-monitor-process`, a row chosen.
- `docs/architecture/desktop`, components: `TableColumn::align` places a
  heading as its cells' text is; at the end its sort arrow leads, so sorting
  never moves the label off the digits' edge.
