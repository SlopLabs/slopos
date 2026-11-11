---
name: slopos-settings
description: >
  The Settings app and each person's preferences file in SlopOS. Use when a
  task touches `settings-app/`, `settings-core/`, `userland/src/apps/settings.rs`,
  `userland/src/apps/prefs.rs`, `userland/src/prefs.rs`, the compositor's
  `desktop_prefs.rs`, or mentions `/bin/settings`, `/bin/prefs`,
  `~/.config/slopos/prefs`, a Settings page (Network, Users, Keyboard,
  Desktop, About), backgrounds, wallpapers or the clock format.
---

## Read first

- `docs/architecture/settings`: how Settings is built, what each page drives, preferences
- `docs/architecture/networking`: `netctl` and `/etc/network`, which the Network page drives
- `docs/architecture/users`: `/bin/users` and the accounts record
- `docs/architecture/privd`: how a request for an action is decided
- `docs/abi/privd-actions`: the actions and who may ask
- `docs/architecture/desktop`: appkit components and the wording rules

## Rules a change must keep

- Settings changes nothing a terminal cannot: a control runs a base tool
  through a privd action or writes the person's own preferences. Missing
  ability goes into the tool, never into `/bin/settings`.
- `settings-app` stays data: no file, privd or syscall there. What touches
  the system is an `Effect` the window carries out and answers as a message,
  so every state lays out on the host.
- Passwords go on the mechanism's standard input only (`Input`, wiped on
  drop, silent in `Debug`), never in args.
- One change is one request: a switch is one `netctl` switch call, a
  dialog's Save is one `netctl load` with the whole file on stdin.
- A switch is pending until the read after its answer
  (`network::Msg::Settled`); the answer itself shows nothing, no control is
  disabled meanwhile, and later presses are asked in turn.
- Nothing said moves the control it is about: refusals go under what was
  asked, outcome lines keep a fixed place.
- Validate fields with `settings-core`, shared with the tools, so app and
  terminal refuse the same values in the same words. Visible strings pass
  `theme-core::words`.
- Preferences live at `~/.config/slopos/prefs` under the home, never
  `XDG_CONFIG_HOME`: init reads the layout there with no person's
  environment.
- A reader that is not the person uses `prefs::of_person`: a regular file
  they own, no link followed.
- Writers replace the file through `write_durable`. A default set is no
  line; an unparsed line is skipped.
- Readers see changes by `stat` polling (`prefs::Watch`); the layout alone
  also needs init asked (`Command::Keymap`).

## Checks

- `just test-host`: `settings-app` (layout at three widths, wording, switch frames) and `settings-core`
- `just test '*utest_settings*,*utest_ip_netctl'`: `netctl`, `prefs`, `keymap`, `users list` inside SlopOS
- `SETTINGS_SHOTS=<dir> cargo test -p slopos-settings-app shots`: a PNG of every state to look at

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
