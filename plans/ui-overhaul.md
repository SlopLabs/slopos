# The Desktop's Look, Words and Toolkit

## Goal

Make every SlopOS surface read and look like one finished product, and make
appkit good enough by default that a new app built from it cannot look bad.
When this plan is done:

- **Text sits on its line.** Every glyph in a run shares one baseline, glyphs
  are spaced evenly and kerned, and all UI text is Inter. A monospace face
  appears only for code, the terminal and command lines.
- **One design system.** Colours, type, spacing, radii and shadows are named
  roles defined once. appkit and the compositor both use them, and no app
  picks a colour or a font size of its own.
- **Components, not primitives.** An app builds pages from `Page`, `Section`,
  `PropertyList`, `Notice`, `ButtonRow`, `ListRow`, `Sidebar`, `Toolbar` and
  `EmptyState`. Text overflow, focus, hover, disabled states and spacing are
  handled by the toolkit. A gate fails the build when an app draws with raw
  colours or sizes.
- **Plain words.** One voice and one glossary across the installer, the
  chrome and the apps, as privd and its dialog already speak. Host tests
  hold the rules wherever text is data.
- **A way to look.** `just ui-shots` boots the live system and saves a PNG of
  every surface, and appkit's layout audit catches clipped text, misaligned
  baselines, overlaps and low contrast without anyone looking.

## Where it stands

The evidence comes from `ROULETTE=0 just iso`, booted headless with a QMP
socket at 1920 × 1080. `screendump` captured the welcome, every installer
page, the terminal, Files, the editor and System Monitor.

### Toolkit

- The installer, Files, the editor and System Monitor build their pages from
  stacks and body `Text` rather than appkit's components, so page titles are
  body text: "Welcome to SlopOS" and "Which disk should SlopOS be installed
  on?" are the same size as the paragraph under them.
- Files truncates a path by byte index (`build_navbar`), which panics on a
  multi-byte path.
- Files shows `[D] bin` for a folder, uses the letters `/ ~ T D B S` for its
  places, and labels its navigation buttons `<`, `>` and `^`.

### Words

The installer's pages, from the screenshots and `installer-core`:

- The warning "The medium carries no toolchain, so the installed system has
  none" appears first on the disk page, in orange, above the question.
- Disks are named `nvme1n1 — 32.0 GiB`, with "an ext4 volume across the whole
  disk" under them. The model the NVMe identify page reports
  (`nvme-core/src/identify.rs:38`) never leaves the driver.
- Reasons are lower-case fragments: "the free region holds 31.0 MiB; SlopOS
  needs 2.3 GiB", "nvme0n1 holds an ext4 volume across the whole disk; only
  erasing it installs onto it". A disabled option dims its title but not its
  reason, so the reason reads as the option.
- The CLI's flags reach the window: "install over it with --mode reuse",
  "only --mode erase installs onto it" (`installer-core/src/issue.rs`).
- Review reads "Boot order: SlopOS where it stands, or after the systems
  there" and "People: user, the first administrator". Proposal lines shout
  "EVERYTHING" and "FORMATTED".
- The size box says `31478M` beside a range given as "1.0 GiB – 30.7 GiB".
- A failure reads "It stopped at step 5 of 9, writing both kernel slots and
  the base: the installer stopped before it said how the install ended",
  above raw log lines, with *Restart* and *Back to review*.
- One app has three names: the dock says *Editor*, the title bar *Sloped*,
  the About box "SlopOS editor". The dock says *Files*, the title bar *File
  Manager*.
- System Monitor prints `RX: 0.0 MiB (0 pkts)` from a constant
  (`sysmon/mod.rs`, `view_overview`), shows "19 total 23 active", and writes its section
  headings in capitals.

An audit of the string sources counts roughly 95 problem strings in the
installer, 25 in the chrome, 70 in the editor and 25 in the smaller apps.

## What the prior art settles

| Source | What it shows | Taken here |
|---|---|---|
| FreeType ([glyph metrics](https://freetype.org/freetype2/docs/glyphs/glyphs-3.html)), stb_truetype ([`stbtt_GetCodepointBitmapBoxSubpixel`](https://github.com/nothings/stb/blob/master/stb_truetype.h)) | A bitmap's box is integer: `bitmap_top` and the box's `iy0` are whole pixels, and the outline is rasterised with its fractional offset inside that box. Horizontal subpixel position is a shift passed to the rasteriser, not a rounding of the bitmap. | baseline on a pixel boundary; quarter-pixel horizontal phases |
| Windows UAC ([how it works](https://learn.microsoft.com/en-us/windows/security/application-security/application-control/user-account-control/how-it-works)) | The prompt names the program and its verified publisher, asks one question, and puts the path and command line under *Show more details*. A program with no verified publisher is "an unknown publisher" in yellow. | app name and icon from a trusted source; the command line as a detail |
| polkit ([polkit(8)](https://www.freedesktop.org/software/polkit/docs/latest/polkit.8.html)), GNOME Shell's agent ([`polkitAgent.js`](https://gitlab.gnome.org/GNOME/gnome-shell/-/blob/main/js/ui/components/polkitAgent.js)) | Each action carries a `message` written for the person ("Authentication is required to power off the system"), separate from its id. GNOME's agent shows that message, *Cancel* and *Authenticate*, and hides the action id and command line. | the policy's sentence is written for the person; the id is never shown, the command line is |
| sudo ([sudoers(5)](https://man.archlinux.org/man/sudoers.5.en)), Redox ([`sudo.rs`](https://gitlab.redox-os.org/redox-os/userutils/-/blob/master/src/bin/sudo.rs)) | A terminal asks `[sudo] password for alice:` and refuses with "Sorry, try again."; Redox prints the same prompt and has no graphical elevation prompt. | `Password for alice: ` after a line saying what it allows |
| GNOME HIG ([writing style](https://developer.gnome.org/hig/guidelines/writing-style.html)), Apple HIG ([writing](https://developer.apple.com/design/human-interface-guidelines/writing)) | Short, direct, second person. Buttons are verbs. An ellipsis marks a command that asks something further. Errors say what happened and what to do next. | the voice guide and the copy rules |
| WCAG 2.2 ([contrast minimum](https://www.w3.org/TR/WCAG22/#contrast-minimum)) | 4.5:1 for body text and 3:1 for large text and control boundaries. | a host test over every text and surface token pair |
| Lucide ([licence](https://lucide.dev/license)), Phosphor ([repository](https://github.com/phosphor-icons/homepage)) | Complete icon sets on one grid and stroke weight, shipped as TrueType fonts under ISC and MIT. | an icon font loaded at runtime like Inter, drawn by the same renderer |
| libadwaita ([CSS variables](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/css-variables.html), [`_colors.scss`](https://gitlab.gnome.org/GNOME/libadwaita/-/blob/main/src/stylesheet/_colors.scss)) | Colours named by surface (window, view, card, dialog, popover, headerbar) and by meaning (accent, destructive, success, warning, error). Hover and active lay `currentColor` over a widget at 7% and 16%. | the roles; hover and pressed derived at those shares |
| Redox orbtk ([`dark.ron`](https://docs.rs/crate/orbtk-theme/0.3.1-alpha3/source/assets/dark/dark.ron)), Orbital ([`config.rs`](https://gitlab.redox-os.org/redox-os/orbital/-/blob/master/src/config.rs)) | A named palette that styles reference, with every state of every style listed by hand; Orbital's chrome is five colours, focused and unfocused listed in pairs. | roles over one palette; states derived rather than listed per style |
| KDE ([KColorScheme](https://invent.kde.org/frameworks/kcolorscheme/-/blob/master/src/kcolorscheme.h)) | Colour sets per surface, each with normal, inactive, negative, neutral and positive foregrounds. | text tones that hold on every surface |
| Android lint's third-party `RawColor` and `RawDimen` ([checks](https://googlesamples.github.io/android-custom-lint-rules/checks/RawColor.md.html)), stylelint's [`declaration-strict-value`](https://github.com/AndyOGo/stylelint-declaration-strict-value), Atlassian's [`ensure-design-token-usage`](https://atlassian.design/components/eslint-plugin-design-system/ensure-design-token-usage/usage) | Lints that fail a raw colour or dimension where a token belongs. | `check_ui_literals.sh` |

Asterinas has no toolkit or theme of its own: its desktop is Xfce over its
Linux ABI.

## Design

### 1. Text

- **Baseline snapping.** `rasterize_glyph` puts the bitmap's top at the
  integer `top = ceil(y_max · s)` and offsets the edges by `top` instead of
  `y_max · s`. The baseline then falls on a row boundary inside every glyph's
  bitmap, `bearing_y = top`, and placement stays `y + ascender − bearing_y`.
  The fixed-cell atlas (`font/src/atlas.rs:177`) takes the same rule.
- **Horizontal phases.** The pen keeps its fraction. A glyph is rasterised at
  one of four quarter-pixel phases, with column 0 at
  `floor(pen + x_min · s)`. The cache key gains the phase. Pen and bitmap
  round once, together.
- **Kerning.** The TTF parser reads `GPOS` lookup type 2, pair adjustment in
  formats 1 and 2, for the default script and language. `text_width`,
  `prefix_fitting` and drawing all apply the same kerning, so measurement and
  painting agree.
- **One line box.** A text role has a size, a weight and a line height,
  rounded once. Text widgets place text by baseline. A row of widgets aligns
  its children on their first baseline, which `HStack` exposes as
  `CrossAxisAlignment::Baseline`.
- **Inter everywhere in chrome.** The compositor's consent dialog, lock and
  login screens, popovers, clock and tooltips use the same renderer and roles
  as appkit. The console atlas stays the kernel console's and the terminal's.
- **Masking and ellipsis.** Password fields show `•` and truncation uses `…`,
  both of which Inter carries.

### 2. One theme, as roles

A new host-tested crate, `theme-core` (`no_std`, no dependencies outside the
tree), holds the tokens appkit and the compositor both read:

- **Colours by role:** `surface` (window), `surface_raised` (cards, popovers,
  dialogs), `surface_sunken` (fields, lists), `chrome` (title and menu bars,
  equal to `surface` so content and frame read as one), `scrim`, `border`,
  `border_strong`, `text`, `text_secondary`, `text_disabled`,
  `text_on_accent`, `accent`, `danger`, `warning`, `success`, `info`, `focus`
  and `selection`, each with hover and pressed derivations.
- **Type roles:** `display` 24/600, `title` 18/600, `heading` 15/600,
  `body` 14/400, `label` 13/400, `caption` 12/400 and `mono` 13/400, each
  with a fixed line height.
- **Spacing:** a 4 px scale (4, 8, 12, 16, 24, 32), plus named insets for
  page, section, card and control.
- **Shape and depth:** a control radius of 6, a card and window radius of 10,
  and two shadow levels for raised surfaces and focused windows.
- **One dark theme.** It is built from the roles, so a light theme is a
  second table and not a code change.

`theme-core`'s tests hold every text role on every surface it may sit on to
4.5:1, and `text_disabled`, `border_strong` and `focus` to 3:1.

### 3. appkit by default

**Apps get roles, not paint.** `App::view` builds from these components, and
the theme is reachable only through them:

- `Text::new(s)` with `.role(Title | Heading | Body | Label | Caption | Mono)`
  and `.tone(Normal | Secondary | Danger | Warning | Success)`. This replaces
  `StyledLabel { color }`.
- `Page { title, subtitle, content, footer: ButtonRow }`. The page owns its
  insets, the title's role and the footer's separator.
- `Section { heading, content }`.
- `PropertyList` of `(label, value)` rows. The label column is as wide as the
  widest label, and each row aligns on its baseline.
- `Notice { tone, title, body, action }`, with an icon for its tone.
- `ButtonRow`: secondary buttons on the left, the default button on the
  right, and a destructive button styled as one even while disabled.
- `ListRow { icon, title, detail, trailing, disabled_reason }`. This also
  serves as a selectable row for choices: a disabled row dims its title and
  its detail together and shows the reason in place of the detail.
- `Sidebar` of sections and `ListRow`s, `Toolbar` of `IconButton`s with
  tooltips, `SearchField`, `Tag` (Erased, New, Kept), `EmptyState { icon,
  title, body, action }` and `Disclosure` ("Show details").
- `Dialog`, rebuilt on `Page`'s parts.

**Behaviour comes with the component.** Every text that can overflow
ellipsises and shows the whole text in a tooltip. Every interactive control
has hover, pressed, focused and disabled states. Focus is one 2 px `focus`
ring around the whole control, a row included. Hit targets are at least
28 px. Enter activates the default button and Escape cancels or goes back,
as `ButtonRow` declares. `LineEdit` and `TextField` share one chrome.

**Icons are a font.** Lucide (ISC) ships at `/usr/share/fonts/` with its
licence beside it and a `NOTICE.md` entry, as Inter does. `Icon::Folder`,
`Icon::TriangleAlert` and so on name glyphs in it, drawn at the role's size
and coloured by tone. App icons for the dock are drawn on one template, a
tinted rounded square with a glyph, and shipped at 64 and 128 px.

**Windows describe themselves.** `App::window()` returns a `WindowSpec`
with a default and minimum size and whether the window resizes. The dock and
the menu bar show the app entry's `name`. A window's title names its
document when it has one ("notes.txt — Editor") and repeats that name
otherwise, so an app has one name everywhere. The compositor places the
window.

**Escape hatches stay narrow.** `Canvas`, the code view's syntax colours, the
terminal's palette and an image's own pixels may use raw colours.
`scripts/check_ui_literals.sh` scans `apps/` and the appkit apps under
`userland/src/apps/` for `Color32` constructors, font sizes, and numeric
spacing and insets, and appkit for colours. It fails unless the file is
allowlisted for one of those escape hatches. It carries a `--self-test` with planted violations and
accepted forms, and runs from `check-framekernel-gates`.

**The layout audit.** `appkit::audit` walks a laid-out tree and reports:

- text clipped without an ellipsis;
- siblings that overlap, or a child outside its parent;
- a row whose texts do not share a baseline;
- a focusable control with no visible focus, or one under 28 px across;
- a text and surface pair under the contrast floor.

`appkit_test` builds each component, and each page of the widget gallery, at
several widths and fails on any finding. `appkit/GUIDE.md` says which
component to use when.

### 4. The compositor

- **Its own surfaces are appkit trees.** The consent dialog, the lock and
  login screens and the popovers are built from appkit components and laid
  out and painted into the compositor's buffer by an embedded runner. That
  runner takes the tree, a rect and input events, and needs no connection.
  The trusted path is unchanged: these surfaces are still drawn only by the
  compositor process, and the quiet and arming rules stay compositor logic.
  Window frames and the dock stay hand-drawn, because they paint every frame,
  but they read `theme-core`'s tokens.
- **Frames.** A window's four corners are rounded at the window radius with
  anti-aliased edges. The shadow follows the rounded rect, and it is stronger
  for the focused window. The title bar takes `chrome`. The close, minimise
  and zoom buttons are anti-aliased, show their glyphs while the group is
  hovered, and turn grey on an unfocused window.
- **Placement.** The first window of an app opens centred in the area between
  the menu bar and the dock. Each later window cascades by the title bar's
  height and wraps before it would cross the dock. A window larger than the
  area is shrunk to its minimum size or the area, whichever is larger.
- **Menu bar.** The app name in `heading`. The clock shows `HH:MM` from
  `CLOCK_REALTIME`, in UTC until SlopOS has time zones, and shows the date
  when clicked. Status items use the icon font.
- **System menu.** A header with the person's name, then *Lock screen*,
  *Switch user…* and *Log out…*, then *Restart…* and *Power off…*. The last
  two ask privd for `org.slopos.power.reboot` and `.poweroff`, which already
  allow `user alone`. When a new base changed the desktop, a notice at the
  top reads "Restart the desktop to finish updating" with a *Restart
  desktop…* button, in place of a permanent *Restart session* entry.
- **Lock and login screens.** These use the wallpaper, dimmed, with the time
  and date large, a circle with the person's initial, their name, a password
  field with the placeholder "Password", and an *Unlock* or *Log in* button.
  The login screen lists people as rows. Keyboard hints are gone; Tab, the
  arrows and Enter work as they do in any form. Messages are sentences:
  "Wrong password. Try again." and "Can't check the password right now. Try
  again in a moment."
- **Dock.** Icons come from the shared template. A dot marks a running app,
  and tooltips use `label`.
- **Gone.** The pending-migration placeholder and its debug write.

### 5. Words

**Voice.** Plain, short sentences in the second person. No implementation
terms on a screen a person must act on; device names, sizes in bytes and
paths go in captions and *Details*. Sentence case everywhere except proper
nouns. Buttons and menu items are verbs. An ellipsis marks an item that asks
something further. An error says what happened and what to do next. Sizes of
disks are decimal (GB, TB), as on the label of the drive.

**Glossary.**

| Use | Not |
|---|---|
| computer | machine |
| disk | drive, medium (for the target) |
| installer, USB stick, DVD | medium |
| partition | volume, region |
| restart | reboot |
| power off | halt, shut down |
| log out, switch user | end session, switch person |
| administrator | admin |
| app | program, client (on screen) |
| startup order | boot order, firmware entry, `BootOrder` |

**Disk names.** The block layer publishes `/dev/disk/by-id`, in udev's
naming: `nvme-<model>_<serial>` from NVMe identify, `usb-<vendor>_<product>`
from INQUIRY, and `virtio-<serial>`. The installer names a disk by its model
and size, for example "KINGSTON SNV3S1000G · 1 TB", with `nvme0n1` in its
caption.

**Rules held by tests.** Where text is data, a host test holds it:
`installer-core`'s words over every fixture, `priv-core`'s `describe`,
`chrome-core`'s strings and the app entries' names. The test finds no
`--` flags, no `...` (only `…`), no word from the banned list, a capital
first letter on headings and buttons, and no full stop on a button. These
tests replace any test that pins an exact wording.

**Examples of the target.**

| Surface | Now | Target |
|---|---|---|
| Welcome | This system runs from the medium it started from, and keeps nothing you do. | You're running SlopOS from the installer. Nothing you do here is saved. |
| Disk page | Which disk should SlopOS be installed on? | Where do you want to install SlopOS? |
| No toolchain | The medium carries no toolchain, so the installed system has none | (info notice) Developer tools aren't on this installer, so they won't be installed. |
| Disk too small | the free region holds 31.0 MiB; SlopOS needs 2.3 GiB | Too small. SlopOS needs at least 2.5 GB. |
| Beside, unavailable | nvme1n1 holds nothing; only erasing it installs onto it | This disk is empty. Choose Erase disk instead. |
| Review title | Review the install | Ready to install |
| Startup order | SlopOS where it stands, or after the systems there | Starts after the systems already on this computer |
| Failure | It stopped at step 5 of 9 … before it said how the install ended | SlopOS couldn't be installed. The installer stopped while copying the system. Try again, or restart and start over. |
| Done | (restart button) | SlopOS is installed. Remove the USB stick, then restart. |
| Lock screen | user's desktop is locked | (the person's name, the time, a password field and *Unlock*) |

## Phases

Each phase ends with something that runs, and lands as commits of its own.

### Phase 1: Text on its line (done)

- Snap the baseline in `FontRenderer` and in the atlas.
- Add quarter-pixel phases and `GPOS` pair kerning.
- Give `Label` and `StyledLabel` one line box and add baseline alignment to
  `HStack`.
- Move the compositor's chrome text to Inter through a shared text module.

Grading:

- New host tests in `font`: for every size from 11 to 24 px, every glyph whose
  outline has `y_min == 0` has ink in the row directly above the baseline and
  none below it. A run's ink positions stay within ¼ px
  of the ideal pen. "To" and "AV" measure narrower than the sum of their
  glyphs.
- `appkit_test`: a label beside a styled label shares a baseline.
- Screenshots of the installer and the consent dialog, before and after.

### Phase 2: One theme (done)

`theme-core` holds the dark table's colour roles, with hover and pressed
derived, the categorical `series`, the text roles, the 4 px scale and its
page, section, card and control insets, edge widths, radii and two shadows;
its host tests
hold the contrast floors. appkit reads the theme through `ctx`, and `Node`
names a `Tone`, a `Surface` or a series where it took a colour. Every app
palette and appkit's own colours are roles, and `userland/src/theme.rs` is the
chrome's geometry over theme-core's tokens, its alias block, the dead
`ui_utils` and the card constants `consent.rs` and `gate.rs` both declared
gone. `scripts/check_ui_literals.sh`, with its self-test and
`scripts/gates/ui-literals.txt`, runs from `check-framekernel-gates`.

### Phase 3: Components, icons, audit and screenshots (done)

appkit's `Text` takes a role and a tone and replaced `Label` and
`StyledLabel`. `Page`, `Section`, `PropertyList`, `Notice`, `ButtonRow`, a
`List` of `ListRow`s, `Sidebar`, a `Toolbar` of `IconButton`s, `SearchField`,
`Tag`, `EmptyState` and `Disclosure` are its components, and `Dialog` is a
modal layer of a title, content and a `ButtonRow`. Lucide's glyphs are
`theme-core::Icon`, shipped at `/usr/share/fonts/Lucide.ttf`. `App::window`
is a `WindowSpec`, which protocol version 4 hands the compositor as size
limits. `appkit::audit` holds every component and each page of
`appkit::catalogue`, which `widget_gallery --page <name>` shows, to its rules
at three widths in `appkit_test`. `just ui-shots` saves every surface this
plan names, and `appkit/GUIDE.md` is what to read before writing an app.

### Phase 4: The permission dialog (done)

Every action's `describe` completes "{App} wants to …", beside an optional
`detail`, `verb` and `destructive`, all held by `priv-core::words`; the 33
base actions are rewritten, and an extension's words are held as it goes
in, a verb being one of `words::VERBS`. privd names the requester
from the base's app entries, else by the attested program's file name, an
extension's program as its extension, else *An unidentified program*, and
sends the name and icon in consent version 3 and the name in `ASKING`. The
compositor builds the dialog from appkit's components and paints it through
`appkit::embed`, centred over the asking app's window; Cancel has focus
first, focus is a ring, the details stay shown until *Show less*. Refusals
and the terminal prompt are sentences to the person. `privd_test` holds the
dialog to the audit at three widths and names a base program, a copy in
`/usr/local` and one whose `/usr/local` entry claims System Monitor;
`extension_action_test` names an extension's own program.

### Phase 5: Chrome (done)

Frames are rounded at all four corners, their edges' coverage taken from
`gfx::rounded`'s signed distance; the shadow follows the rounded rect, half
as dark without focus, and the window buttons are anti-aliased discs, grey
without focus, that show their colours and glyphs while the group is
hovered. `chrome-core::placement` centres an app's first window between the
menu bar and the dock and cascades its later ones, which the compositor
applies when a toplevel first shows. The clock is `HH:MM` of
`CLOCK_REALTIME` in UTC (`chrome-core::clock`), its date a click away, and
the status items are icon-font glyphs. The lock and login screens, the
system menu, the network and clock panels and the question each ending asks
are appkit trees (`compositor::views`) over chrome-core's `gate` and `menu`
models, drawn with appkit's new `Avatar` and theme-core's `Hero` role;
*Restart…* and *Power off…* ask privd for `org.slopos.power.reboot` and
`.poweroff`, *Restart desktop…* sits in the update notice, and an ending
that did not happen says so where its question was. Every shipped app icon
is `appkit::app_icon`'s template at 64 and 128 px, rendered by `just icons`
and held to it by `tools/icons`' host test; the dock draws the 64 px one,
and the template, tinted by app id, for an app that ships none. The
placeholder and its debug write are gone. `appkit_test` holds the chrome's
surfaces to the audit and `dock_test` the icons' sizes; `ui-shots` adds the
clock and the power-off question.

### Phase 6: Words and apps (done)

devfs publishes `/dev/disk/by-id` in udev's naming (`boot_core::disk_id`) —
`nvme-<model>_<serial>_<nsid>` with namespace 1's shorter alias,
`virtio-<serial>` and `usb-<vendor>_<product>_<serial>-0:<lun>`, each
partition with `-part<N>` — and the installer names a disk by the model it
reads there, sized in decimal units. `theme-core::words` holds the glossary
and the shapes of a label and a sentence, which `priv-core::words` and
installer-core's copy tests share; those hold every page of every fixture,
and the CLI's issues, to them. An issue gives a window a fact and a next step
and the CLI the flag that changes it, and the protocol, version 2, carries a
disk's model and why it is not offered. The installer's pages are `Page`s of
`ListRow`s, `PropertyList`s, `Notice`s and `Tag`s, the engine's log under
*Show details*. Files has a `Sidebar` of places, a `Toolbar`, icons and
`EmptyState`s; the editor is *Editor* and the image viewer *Image Viewer*
everywhere, each error saying what to do next, and a window showing a
document is titled `<document> — <App>`. System Monitor counts real traffic
and tasks by state under sentence-case headings, and the terminal keeps an
inner margin. `apps_test` holds every app's entry and title to one name,
`editor_test` its errors to a next step, `installer_ui_test` each page to the
audit, and a devfs test the `by-id` names on QEMU's NVMe, virtio and USB
disks.

## Grading

- **Host:** `font`, `theme-core`, `chrome-core`, `priv-core`, `installer-core`
  and `extension-core` under `just test-host`.
- **Gates:** `check_ui_literals.sh` and its self-test in
  `check-framekernel-gates`.
- **In the guest:** `appkit_test` with the audit, `installer_ui_test`,
  `privd_test`, `extension_action_test`, `dock_test`, `apps_test` and
  `editor_test`.
- **End to end:** `just test-installer` keeps every lane.
- **By eye:** `just ui-shots`, before and after each phase. No pixel goldens:
  a change to the font renderer would rewrite all of them, and the audit
  catches the failures goldens would.

## Out of scope

- Scaling for high-density screens. Tokens are in logical pixels so a scale
  factor can come later without touching apps.
- A light theme, translations (the font set has no CJK), and screen-reader
  output.
- Animation beyond what exists: the dock's magnification and the progress
  bar's sweep.
- Time zones. The clock shows UTC until SlopOS has them.
- Changing who may ask for what. Every privd principal, level and capability
  stays as `AGENTS.md` states it. Only the words and the dialog change.

## Decided

- **Fix the renderer before anything else.** Every other phase is judged by
  screenshots, and every screenshot shows the wobble.
- **Roles, not colours, in the app API.** An app that cannot name a colour
  cannot pick a bad one. The raw-literal gate holds that, and its allowlist
  names the few places where colour is the content.
- **Apps name roles; appkit resolves them.** `Node` takes a `Tone`, a
  `Surface` or a `SegmentFill::Series`, never a `Color32`, and a widget reads
  the theme from `ctx` when it paints, so a second theme changes no app. A
  code view's syntax spans are the one colour an app hands appkit.
- **States are derived, not listed.** Hover and pressed lay the theme's
  `state_ink` over a colour at libadwaita's 7% and 16%, and a row over a
  surface it does not know lays `Theme::overlay` on it. A theme that listed
  them, as orbtk's did, would repeat every state for every role.
- **Colours that tell things apart name a series, not a meaning.** A disk's
  partitions and a chart's series take the theme's `series` by index, never
  a status tone: a crash partition is not a danger. The series may share hues
  with the tones; a second theme may not.
- **The contrast relation is stated, not total.** Body and secondary text
  hold 4.5:1 on every surface, hovered and selected rows included; status
  tones hold it on plain surfaces only, so a widget does not set them in a
  selectable row; `text_on_accent` on every fill in every state and on every
  series colour; edges, focus and disabled text 3:1. WCAG 1.4.3 exempts
  disabled text, and SlopOS holds it to 3:1 anyway. Body text also holds on a
  pressed surface, where a menu's open title sits, and secondary text on
  `border`. The code view's syntax colours and the wash it lays under a found
  match are its own.
- **A state shows.** Hover sits at least 1.1:1 from idle and pressed further,
  and a fill mixes the ink drawn on it, so a light accent button darkens as
  libadwaita's does. A selected row also carries an edge, accent while its
  list has focus, since its wash alone sits too close to a hovered row's. A
  focused field's edge is `stroke::FOCUS`, twice an idle one's, since `focus`
  and `border_strong` differ in hue far more than in contrast. A disabled
  button is edged in `text_disabled`, as a disabled checkbox is.
- **Apps reach text only by role.** `text::ui` takes pixel sizes for appkit
  and the compositor; an app that calls it, or names a `font_size`, fails the
  gate. appkit's fixed-cell atlas is drawn at the `mono` role.
- **The gate finds apps by what they are.** A module of `userland/src/apps/`
  is an app when a file of it implements `App` or calls `run_app`, so a new
  app is scanned without a list. appkit is held to colours alone, since its
  geometry is the toolkit's own. An exemption names one check for one file
  and fails once it matches nothing.
- **The pointer is a sprite.** It stays white edged in black on any theme,
  the one raw colour the chrome keeps.
- **The compositor embeds appkit for its dialogs and screens, not its
  frames.** Dialogs, lock and login screens and popovers gain the same
  components, wrapping and focus behaviour as apps. Frames and the dock paint
  every frame and keep their own code over the same tokens.
- **The app's name comes from a source the person cannot write.** A name
  from `/usr/local` or `~/.local` would let any program ask in another app's
  name, so those entries are never used.
- **The command line stays visible.** Windows hides it behind *Show more
  details*. SlopOS keeps it expanded by default, because `AGENTS.md` requires
  the whole command line in the question, and a collapsed detail is one that
  people skip.
- **Cancel is focused first, and focus is a ring.** A filled button means
  "default", not "focused", so the safe answer never looks like the
  dangerous one.
- **Sentence case everywhere.** One rule is easier to follow and to test than
  GNOME's split between header and sentence capitalisation.
- **Decimal sizes for disks.** A 1 TB drive reads "1 TB", as its label does.
  Memory and file sizes stay binary.
- **Copy rules are tests; exact wording is not.** Wording tests churn with
  every edit, and rules catch the regressions that matter: a flag, jargon or
  a broken ellipsis reaching a screen.
- **Screenshots are for people.** `just ui-shots` gives a person or an agent
  something to look at. The audit and the tests are the gate.
- **The audit judges what was painted.** It records each line of text as
  `PaintContext` draws it, with the colour that was under it, and compares
  painted pixels with focus on and off, so a widget is held to what it put
  on screen rather than to what it says of itself. Each rule is first shown
  to fire on a planted violation, as every gate's self-test is.
- **A control is at least 28 px across**, and the audit holds every tab stop
  to it.
- **A modal layer owns the keyboard and the pointer.** A dialog marks itself
  modal: Tab moves only among its controls, a key none of them took is
  `Blocked` from the page beneath while Enter and Escape still reach its
  `ButtonRow`, and nothing beneath shows a tooltip. Escape presses its cancel
  button, and dismisses it only when it has none. The compositor's own
  dialogs, built from the same components, inherit this.
- **Enter never destroys.** Enter presses the confirming button and Escape
  the back or cancel one; a destructive button is pressed only by a click
  or by Enter once it has focus, as macOS and GNOME have it.
- **Only a footer answers Enter and Escape.** The keys reach the `ButtonRow`
  that ends a `Page` or a dialog, never one inside the content, and a list
  with nothing to open leaves Enter to it. Left and Right move along a
  `ButtonRow`, as along an ARIA toolbar.
- **A scroll view is a tab stop only while it scrolls.** A page or a dialog
  that fits adds no stop between its controls.
- **A scroll view keeps its place while it shows the same.** A rebuild
  hands each view the offset of the one before it that showed the same, a
  page or a list by its first name, a table by its columns and what the
  app says its rows are of (`shows`, as React's `key`), so a message, a
  tick or a dialog over it moves nothing and another page or directory
  starts at the top. The wheel goes to the innermost view that can still
  move, and a key that moves a list's choice scrolls it into view, as
  GTK's focus adjustments do.
- **Focus comes back when a dialog closes.** One that opens starts with
  nothing focused; the page's focus waits beneath it.
- **The catalogue is the toolkit's.** Every component's states live in
  `appkit::catalogue`, so the gallery a person opens and the pages the audit
  grades are one source. The gallery is a loose app, so the live ISO carries
  it.
- **The icon font ships whole and unmodified.** All of Lucide costs under a
  megabyte and needs no subsetting tool; an icon is a variant naming its
  codepoint, and a host test holds every variant to the shipped font.
- **A window's limits are the compositor's to hold.** A client states its
  minimum and maximum size as xdg-shell does, and the compositor clamps
  resizes, zoom and, later, placement to them.
- **A row aligns its children; it stretches them only when asked.** A stack
  passes its cross-axis minimum to its children only under `Stretch`, as a
  flex layout does, so a centred or baseline row never makes a child taller
  than it asked to be.

- **A program the system attests is named even without an entry.** A base
  program that no app entry runs is named by its file name: the base is as
  hard to write as an entry in it. Only a program the kernel attests as none
  of the system's is *An unidentified program*, and its path is not shown,
  since the kernel attests none.
- **An extension's program is named as its extension.** *The <name>
  extension*, with no icon: an extension's entries are its author's words
  and art, so one that named itself Terminal could ask for a base action as
  Terminal, while its name is the activation's, the kernel's.
- **A command keeps its own spelling.** A name is a proper noun, so a
  headline that starts with `bootctl` starts in lower case rather than
  misnaming it.
- **A refusal is a sentence; why goes to the log.** What privd tells a
  requester is a sentence the person can act on, the same for every cause
  they cannot tell apart; the detail a developer needs is in privd's log
  beside it.
- **What an installed extension says stays.** The copy rules hold what an
  extension says when it goes in, by a person or a key, and never as it
  parses, so an image installed before the rules tighten still activates,
  saying what it said; a verb they no longer take shows as *Allow*.
- **The extension is named in the details.** An extension's action names it
  in the dialog's details, and to a dialog of an older consent version by
  `describe`, so a person always sees whose mechanism runs.
- **The model holds the keys; the tree is the view.** The compositor's own
  surfaces are appkit trees laid out and painted by `appkit::embed`, which
  takes the pointer and answers with messages; keys go to the host-tested
  model behind the surface, which says what has focus, so the trusted path's
  rules stay in one place a test reads.
- **Focus is found by what a control says.** The model names the focused
  control by role and label, so no label repeats: no verb says *Cancel*,
  *Show details* or *Show less*.
- **A verb is one the dialog knows.** `words::VERBS` lists what an allowing
  button may say, since a denylist cannot foresee *Disallow* or *Show more*
  beside *Show less*: an extension's button says one of them or *Allow*.
- **A requester hears the name the dialog shows.** `ASKING` carries it, so a
  program, and a test, know what the person was told.
- **A question stays where it first appeared.** Its card's place is taken
  when it is first laid out, so the window of the app that asks cannot move
  it under the person's pointer; a screen that changes size moves it, and
  input waits again as it does after a move.
- **A click is judged at its press.** A button answers when it is let go,
  so a press made while input waits, or before the dialog moved, answers
  nothing: the person began it on what was there before. A release over the
  lock or login screen ends the press unanswered.
- **Enter in a destructive question's field moves to its button.** The
  danger button is pressed only by a click or by Enter once it has focus.
- **A requester reads a policy as it is written.** privd holds its own
  base's policy to every setting and to the words; a requester, which may
  read another base's after a live switch, passes over a setting it does not
  know and holds no words. A requester of a base from before this one
  refuses a policy carrying the new settings, so it asks nothing across a
  live switch until it starts again, and a rollback to such a base stops
  each extension whose declaration uses them until `extctl enable` brings it
  back.
- **A reply can say more without a version.** A requester passes over a
  reply's field it does not know. Requesters of earlier bases read none, so
  one still running across a live switch to this base fails a request that
  asks the person, once.
- **Words that are data are held where they are stated.** A refusal is a
  sentence a host test holds, and an in-guest test compares with that
  sentence rather than a copy of its words.
- **Windows are placed per app.** An app's later window cascades from its
  own latest, known by app id or else by client, as mutter places them, so
  another app's windows never push it along; one too large for the area is
  shrunk only as far as its minimum.
- **A shadow keeps its reach.** Focus changes how dark it is, never how far
  it goes, as libadwaita's do, so a change of focus repaints the same pixels;
  where two shadows overlap both darken, as they would.
- **A choice that ends something asks once and never counts down.**
  *Restart desktop…*, *Log out…*, *Restart…* and *Power off…* ask with
  Cancel focused. GNOME's sixty-second countdown is not taken: a question
  that answers itself ends someone's work while they are away.
- **An ending that did not happen says so.** Its question's place says what
  did not happen and to try again, unless the person refused privd's
  question themselves; why goes to the log, as a refusal's does.
- **A panel the person opened takes the keyboard.** A key pressed while a
  panel or the system menu is open is the panel's, as a GNOME Shell menu's
  is, so nothing typed reaches the window beneath it unseen. The menu that
  opens on its own to offer a restart takes none until the pointer reaches
  it: whoever was typing goes on typing.
- **What a person can type at a list chooses from it.** Typing on the login
  screen's list chooses the first person whose name starts with what was
  typed, as a GTK list does, so the keyboard's way in is still a name, Tab,
  a password and Enter.
- **Shipped icons are rendered, never drawn by hand.** One template renders
  every icon the system ships and the dock's fallback; a host test holds the
  PNGs to it, so a change to the template or the theme fails until `just
  icons` renders them again. Each ships at 64 px and twice that. The dock
  draws the 64 px one, which it never draws past, since its filter samples
  two pixels a side and a larger source would skip pixels; the larger waits
  for a scale factor.
- **A colour that names something comes from its name.** An avatar's
  series colour is a hash of the person's name and a fallback icon's of the
  app's id, or its title when it gives none (`theme-core::series_of`), as
  libadwaita's avatars are, so each stays the same in every place and boot.
- **One glossary, stated once.** `theme-core::words` holds it with the
  shapes of a label and a sentence; a surface adds only the trade words it
  keeps to its details, so the policy and the installer cannot drift apart.
- **The window and the CLI speak from one source.** An issue's `words` are
  the window's fact and next step, its `Display` the CLI's with the flag
  that changes it, and one test holds both to the rules with the flags
  stripped.
- **A disk's model is read from its by-id names.** installer-core takes it
  from the names devfs already publishes, so no call is added to the base
  interface. A serial is the last `_` part of such a name, as every NVMe and
  Bulk-Only serial is.
- **by-id follows systemd's current rules, per bus.** An NVMe namespace's
  name ends in its nsid, namespace 1 keeping the older name as an alias, and
  keeps each whitespace byte as an `_`, as `string_escape=replace` does; a
  USB or virtio part is trimmed and each run of whitespace one `_`, as
  `udev_replace_whitespace` does. The rules live in `boot_core::disk_id`,
  where a host test holds them; a `/` udev keeps is an `_` here, since a
  name is one directory entry.
- **A title names the document, then the app.** `<document> — <App>`, read
  again after every rebuild, as GNOME and Windows title their windows.
- **An error says what happened, then what to do.** None ends at the fact.
- **A test interface names variants, not words.** The installer's markers
  print `Refusal`'s variant, so the wording moves without moving a grep.
- **The engine's log is a developer's.** Its lines keep the engine's own
  terms and sit under *Show details*; the voice holds what a page says.
- **A bare size is GB.** The size box shows decimal units, as a disk is
  sized, and reads a number without one as gigabytes. The CLI takes `kB` to
  `TB` as its listing gives them, `K` to `T` as binary, and a bare number
  as bytes, which scripts pass.
- **An Alt chord is the app's.** A list or a table moves its choice with
  the arrows alone, so Alt+Up reaches the app, as a file manager's way up.
- **A row opens on Enter or a second click.** A table's `on_activate` is
  separate from its choice, so a key that leaves the choice where it is
  opens nothing.

## Constraints

- The trusted path holds. Only the compositor draws a consent question,
  keys go to no client while one is up, and the quiet and arming periods
  stay.
- The consent frame and the `describe` grammar change by version. A desktop
  older than a live base switch still confirms, as it does today.
- Fonts and the icon font load at runtime from `/usr/share/fonts/` with their
  licences beside them. Neither is compiled into a binary (`AGENTS.md`,
  *Licensing discipline*).
- `installer-core`, `theme-core` and `chrome-core` add no dependency outside
  the tree. The icon font needs a `NOTICE.md` entry.
- The CLI's output lines are a test interface. A change to one changes
  `just test-installer` in the same commit.
- `AGENTS.md` and `plans/README.md` move with each phase.
