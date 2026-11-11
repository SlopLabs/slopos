---
name: slopos-desktop
description: >
  The look and behaviour of the SlopOS desktop: the theme, appkit's components
  and audits, the compositor's chrome and the System Monitor. Use when a task
  mentions theme-core, appkit, chrome-core, apps/, tools/icons, the
  compositor's frames, dock, menu bar, lock or login screen, system menu or
  wallpaper, sysmon, widget_gallery, ui-shots, or a colour, tone, text role,
  icon, focus, dialog, switch or the words on a screen.
---

## Read first

- `docs/architecture/desktop`: theme, components, audits, chrome, icons, System Monitor, contributor rules
- `docs/development/writing-an-app`: adding an app, its dock entry, icon and tests
- `docs/architecture/userland-windowing`: frame pacing, buffers and input under the toolkit
- `docs/architecture/settings`: preferences that choose the background and clock

## Rules a change must keep

- An app names a `Tone`, `Surface`, `TextRole`, `space` step or `inset`,
  never a colour or a number. A new role goes in `theme-core` with a contrast
  test for every surface it sits on.
- A crate under `apps/` depends on `slopos-appkit`, `slopos-editor-core` and
  `slopos-image` only.
- A control waiting on the system is never disabled for being busy: keep what
  was asked in `appkit::Pending` and build the control from its `shown` value.
- A new component owns its states, focus ring, `…` overflow and
  `style::MIN_TARGET` hit size, and joins `appkit::catalogue` and `GUIDE.md`.
- Enter and Escape no control took go to the modal layer's `ButtonRow` before
  `App::on_key`; Enter never reaches a destructive button that way.
- A dialog is modal for Tab, pointer and tooltips, opens with the keyboard in
  its first field and gives focus back when it closes.
- Focus is kept by place across rebuilds (`FocusManager`). A widget never
  tracks held keys: `run_app` tells repeats from presses.
- Screen words follow `theme_core::words`; a surface whose words are data
  holds them to it in a host test.
- One name per app: entry `name` and window title agree (`apps_test`). An
  app icon is an `Art` in `tools/icons`, rendered by `just icons`.
- The lock, login, menu and question surfaces send every key to their
  `chrome-core` model; put behaviour there, where host tests reach it.
- Say why a file was refused from `appkit::Refusal`, never from its path.

## Checks

- `just test-host`: theme contrast and words, chrome-core models, icon art
- `just test '*utest_appkit'`: every component and gallery page against the layout audit
- `just test '*utest_apps,*utest_dock,*utest_sysmon'`: titles, dock entries, System Monitor
- `cargo run -p slopos-xtask -- lint ui-literals`: no colour or spacing literal in an app
- `cargo run -p slopos-xtask -- lint app-deps`: apps depend on the toolkit alone
- `just ui-shots`: PNGs of every surface to look at; compares nothing

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
