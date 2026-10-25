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
- **A trustworthy permission dialog.** It names the app asking, with its icon
  and the name from its app entry. It says in one plain sentence what will
  happen. It keeps the command line visible as a detail, and its button
  colours mean what they say.
- **Polished chrome.** Windows open centred or cascaded, frames and shadows
  are rounded and anti-aliased, the clock shows the time of day, and the
  system menu can restart and power off the computer. The lock screen and
  login screen look like part of the same system.
- **Plain words.** One voice and one glossary across the installer, privd,
  the chrome and the apps. Host tests hold the rules wherever text is data.
- **A way to look.** `just ui-shots` boots the live system and saves a PNG of
  every surface, and appkit's layout audit catches clipped text, misaligned
  baselines, overlaps and low contrast without anyone looking.

## Where it stands

The evidence comes from `ROULETTE=0 just iso`, booted headless with a QMP
socket at 1920 × 1080. `screendump` captured the welcome, the consent
dialog, every installer page, the dock, the terminal, Files, the editor,
System Monitor, the system menu and the lock screen.

### Text

**Every glyph has its own baseline.** `FontRenderer::rasterize_glyph`
(`font/src/lib.rs:297`) rasterises each glyph so that the outline's top,
`y_max · scale`, falls exactly on bitmap row 0, which is a fractional
position. It then places the bitmap at `y + ascender − round(y_max · scale)`
(`:224`, `:330`). The baseline therefore lands `y_max·s − round(y_max·s)`
pixels off the line, a different amount per glyph. Measured from
`Inter-Regular.ttf` (2048 units per em):

| Size | `h d k l T W` | `e s o a n c` | `t` | `S O` | Spread |
|---|---|---|---|---|---|
| 14 px (body) | +0.19 | −0.26 | +0.46 | +0.32 | 0.82 px |
| 17 px (heading) | +0.37 | +0.40 | +0.49 | −0.47 | 0.95 px |

At 14 px, an `h` sits 0.45 px below the `e` beside it, and `t` sits 0.72 px
below it. Anti-aliasing turns each offset into a differently blurred bottom
row. That is the up-and-down wobble in the installer.

**Horizontal placement rounds twice.** The pen is rounded (`:217`), then the
bitmap, whose column 0 is the exact `x_min`, is offset by
`round(lsb · scale)` (`:329`). Each glyph's ink is up to 1 px from where it
belongs. In "started" at 14 px, `s` is drawn +0.26 px off and the `t` after
it −0.53 px off, so the `st` gap shrinks by 0.79 px. `t` and `a` move 0.95 px
apart. The screenshot reads "st arted", "th e", "volu me". The parser reads
no kerning table at all, and Inter keeps its kerning in `GPOS`.

**Two line boxes.** `Label` draws at the top of its fixed 20 px row
(`appkit/src/widgets/label.rs:160`). `StyledLabel` centres the font's 17 px
line in the same row (`styled_label.rs:52`). On the installer's review page
the grey labels sit 1 px below their values.

**The compositor sets chrome in the console font.** The consent dialog, the
lock and login screens, the system menu, the network panel, the clock and
the dock's tooltips draw through `gfx::draw_str_clipped`, the fixed-cell
10 × 22 atlas the kernel console uses. Window titles and the menu bar's app
name use Inter. One screen shows two families, and the atlas has no `…`,
which is why truncation is spelled `..` or `...` in three places.

### Theme and toolkit

- `appkit/src/style.rs` defines one `StyleSheet::dark()`. `run.rs:30` builds
  it, and no `App` can read it. Every app keeps its own palette: Files has 12
  colour constants that are not the theme's, the image viewer 6, the editor
  about 14 literals plus 15 syntax colours, the installer 9, and System
  Monitor 4, with a different green.
- appkit hard-codes colours inside its own widgets: the text field selection
  is macOS blue `(0,122,255,100)`, the progress bar uses traffic-light
  colours, the checkbox tick is `WHITE`, and the scrollbar is `#c8ccd4`.
- `font_size_small` (12) is defined and never used. `font_size_heading` (17)
  is used only by `Dialog`. `Node` has no heading, caption or body role, so
  page titles are body text: "Welcome to SlopOS" and "Which disk should
  SlopOS be installed on?" are the same size as the paragraph under them.
- `VStack`, `HStack` and `EdgeInsets` take raw numbers. Pages pass 2, 4, 6, 8,
  10, 12, 16 and 24. The spacing tokens in the style sheet go unused.
- `LineEdit` and `TextField` share a role but differ in fill, focus ring,
  caret colour, caret blink and vertical alignment. Focus is shown four
  different ways: a ring on a button, a ring around the checkbox square, a
  ring around the radio dot only, and a 1 px line on a scroll view.
- Only `TreeView` elides with `…`. `Label` and `Button` clip silently, and
  Files truncates a path by byte index (`file_manager.rs:297`), which panics
  on a multi-byte path.
- Icons are eight shapes drawn from rectangles (`widgets/icon.rs`). Files
  shows `[D] bin` for a folder, uses the letters `/ ~ T D B S` for its places,
  and labels its navigation buttons `<`, `>` and `^`. The dock shows letter
  tiles, 64 × 64 PNGs of `E F I M T`.

### Compositor chrome

- Title bars are `#2D2D30` on windows whose content is `#282c33`, so chrome
  and content are two different greys. The top corners are rounded without
  anti-aliasing (`decorations.rs`), the bottom corners are square, and the
  shadow is a square halo. The three window buttons are aliased circles.
- Every window opens at (100, 100) (`compositor/protocol.rs:136`). The
  terminal, Files and System Monitor stacked on the installer, and the
  installer covered the wallpaper's logo.
- The clock is uptime, not the time of day (`status_item.rs:74`), even though
  `CLOCK_REALTIME` answers the wall clock the loader anchored.
- The system menu offers *Lock*, *Switch person*, *Log out* and *Restart
  session*, in monospace, with no hover and no icons. There is no way to
  restart or power off the computer from the desktop.
- The lock screen reads "user's desktop is locked" above a green-bordered
  field and the hint "Enter: unlock  F2: switch person". It has no clock, no
  wallpaper and no button. The login screen reads "Log in: a, b, c" above
  "Up, Down: choose  Tab: next field  Enter: log in".
- A reachable placeholder draws "Window content pending migration" and writes
  `DEBUG: draw_window_placeholder called!` to stderr (`renderer.rs:931,945`).
- `userland/src/theme.rs` carries a `TODO(tech-debt)` block of legacy aliases,
  and `consent.rs` and `gate.rs` each declare their own card constants.

### The permission dialog

The screenshot when the installer asks for its engine:

```
Allow: Install SlopOS onto whichever disk the reques
ter names, erasing what it says to?
Asked by /bin/installer-ui, as user, pid 18
Runs /bin/installer --serve
In /
                                     [ Deny ] [ Allow ]
```

- The headline is `format!("Allow: {}?", describe)` (`consent.rs:115`) over a
  policy sentence written for the policy's reader. `wrap` (`:97`) breaks
  every line by character, so prose breaks mid-word ("reques/ter").
- The requester is a path, a uid's name and a pid (`priv-core`
  `Dialog::requester`). Nothing names the app the person sees on screen.
- The focused button is filled with the window-zoom green `#28C840`, so with
  the default focus *Deny* is the green button. Its label is `#e0e0e0` on
  that green: a contrast of 1.69:1.
- The whole dialog is set in the console font, with no icon, title or shadow.
- 33 `describe` lines in `assets/privd/actions` leak the policy's vocabulary:
  capability names ("RawBlock, Mount or DacOverride gives it the machine"),
  "requester", "slot", "base", "ceilings", "every file not sealed". "Restart
  the machine" appears twice, for two actions.
- privd's terminal prompt is `"{action}: {whose}'s password: "`
  (`privd/mod.rs:1072`), so a person reads
  `org.slopos.accounts.password: alice's password:`. Refusals repeat "the
  person at the machine" five times (`priv-core/src/consent.rs`).

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
  (`sysmon/mod.rs:274`), shows "19 total 23 active", and writes its section
  headings in capitals.

An audit of the string sources counts roughly 95 problem strings in the
installer, 40 in privd and the dialog, 25 in the chrome, 70 in the editor and
25 in the smaller apps.

## What the prior art settles

| Source | What it shows | Taken here |
|---|---|---|
| FreeType ([glyph metrics](https://freetype.org/freetype2/docs/glyphs/glyphs-3.html)), stb_truetype ([`stbtt_GetCodepointBitmapBoxSubpixel`](https://github.com/nothings/stb/blob/master/stb_truetype.h)) | A bitmap's box is integer: `bitmap_top` and the box's `iy0` are whole pixels, and the outline is rasterised with its fractional offset inside that box. Horizontal subpixel position is a shift passed to the rasteriser, not a rounding of the bitmap. | baseline on a pixel boundary; quarter-pixel horizontal phases |
| Windows UAC ([how it works](https://learn.microsoft.com/en-us/windows/security/application-security/application-control/user-account-control/how-it-works)) | The prompt names the program and its verified publisher, asks one question, and puts the path and command line under *Show more details*. | app name and icon from a trusted source; the command line as a detail |
| polkit ([polkit(8)](https://www.freedesktop.org/software/polkit/docs/latest/polkit.8.html)) | Each action carries a `message` written for the person ("Authentication is required to power off the system"), separate from its id. | the policy's sentence is written for the person; the id is never shown |
| GNOME HIG ([writing style](https://developer.gnome.org/hig/guidelines/writing-style.html)), Apple HIG ([writing](https://developer.apple.com/design/human-interface-guidelines/writing)) | Short, direct, second person. Buttons are verbs. An ellipsis marks a command that asks something further. Errors say what happened and what to do next. | the voice guide and the copy rules |
| WCAG 2.2 ([contrast minimum](https://www.w3.org/TR/WCAG22/#contrast-minimum)) | 4.5:1 for body text and 3:1 for large text and control boundaries. | a host test over every text and surface token pair |
| Lucide ([licence](https://lucide.dev/license)), Phosphor ([repository](https://github.com/phosphor-icons/homepage)) | Complete icon sets on one grid and stroke weight, shipped as TrueType fonts under ISC and MIT. | an icon font loaded at runtime like Inter, drawn by the same renderer |

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
4.5:1, and `text_disabled`, `border_strong` and `focus` to 3:1. The current
`text_disabled` on `#282c33` is 2.86:1 and fails.

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

**Icons are a font.** A permissive icon font (Lucide, ISC, or Phosphor,
MIT) ships at `/usr/share/fonts/` with its licence beside it and a
`NOTICE.md` entry, as Inter does. `Icon::Folder`, `Icon::Warning` and so on
name glyphs in it, drawn at the role's size and coloured by tone. App icons
for the dock are drawn on one template, a tinted rounded square with a glyph,
and shipped at 64 and 128 px.

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
spacing and insets. It fails unless the file is allowlisted for one of those
escape hatches. It carries a `--self-test` with planted violations and
accepted forms, and runs from `check-framekernel-gates`.

**The layout audit.** `appkit::audit` walks a laid-out tree and reports:

- text clipped without an ellipsis;
- siblings that overlap, or a child outside its parent;
- a row whose texts do not share a baseline;
- a focusable control with no visible focus;
- a text and surface pair under the contrast floor.

`appkit_test` builds each component, and each page of the widget gallery, at
several widths and fails on any finding.

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
  top reads "Restart the desktop to finish updating" with a *Restart desktop*
  button, in place of a permanent *Restart session* entry.
- **Lock and login screens.** These use the wallpaper, dimmed, with the time
  and date large, a circle with the person's initial, their name, a password
  field with the placeholder "Password", and an *Unlock* or *Log in* button.
  The login screen lists people as rows. Keyboard hints are gone; Tab, the
  arrows and Enter work as they do in any form. Messages are sentences:
  "Wrong password. Try again." and "Can't check the password right now. Try
  again in a moment."
- **Dock.** Icons come from the shared template. A dot marks a running app,
  and tooltips use `label`.
- **Gone.** The pending-migration placeholder, its debug write, the
  `TODO(tech-debt)` alias block in `userland/src/theme.rs`, and the card
  constants duplicated across `consent.rs` and `gate.rs`.

### 5. The permission dialog

**Who is asking.** privd resolves the requester's attested program to an
app entry. Only sources the person cannot write are used:

| Attested as | Name and icon from |
|---|---|
| a base program | an entry in the base's `/usr/share/slopos/apps` whose `exec` is that path |
| an extension's program | an entry in that activation's `share/slopos/apps` |
| anything else | "An unidentified program", with its path shown in the details |

Entries under `/usr/local` and `~/.local` are never used here: a person can
write them, so any program could take another's name. The name and the icon's
path go to the compositor in the question, which is a new version of the
consent frame. Both sides keep speaking the older version, as `READY`'s
version already allows across a live base switch.

**What it says.** Each action gains two settings and changes one:

- `describe` becomes a verb phrase in lower case that completes "{App} wants
  to …", for example "erase a disk and install SlopOS on it". It is at most
  72 characters.
- `detail` is optional: one sentence about the consequence, for example
  "Everything on the disk you choose will be deleted."
- `verb` names the allow button, for example *Install*, *Restart* or
  *Power off*. It defaults to *Allow*.

`priv-core`'s `describe` parser, which extension declarations share, refuses
capability names and the words "requester", "mechanism", "principal",
"privd", "slot", "base", "sealed" and "uid". A host test runs every action in
`assets/privd/actions` through it, and `extctl` refuses an extension whose
`describe` breaks the same rules. The extension's name stops being appended
to `describe`, because the app line already says whose program it is.

**How it looks.**

```
┌──────────────────────────────────────────────────────────┐
│ [icon]  Install SlopOS wants to erase a disk and install │
│         SlopOS on it.                                    │
│         Everything on the disk you choose will be        │
│         deleted.                                         │
│                                                          │
│         Password for user  [••••••••            ]        │
│                                                          │
│ ▾ Details                                                │
│   /bin/installer-ui as user                              │
│   /bin/installer --serve   in /                          │
│                                        [Cancel] [Install]│
└──────────────────────────────────────────────────────────┘
```

- The headline is `title`, the detail `body`, and the details `mono` in
  `caption`. Prose wraps at word boundaries; the command line wraps by
  character and is never cut.
- *Details* is shown expanded, so the whole command line and directory stay
  in view, as `AGENTS.md`'s privd paragraph requires. *Show less* collapses
  them.
- *Cancel* is focused first. Focus is shown by the focus ring, never by fill.
  The allow button is filled with `accent`, or `danger` for an action marked
  `destructive`, whichever has focus.
- The dialog centres over the requesting app's window when the compositor
  can tie the requester's pid to a surface, and over the screen otherwise.
  The scrim dims everything else.
- These do not change: one question at a time, the 750 ms arming, the 1.5 s
  quiet period for keys, keys never reaching a client, and the 320-byte cap.

**What privd says back.** Refusals are short sentences addressed to the
person running the command: "You didn't allow this.", "Nobody answered in
time.", "Wrong password.", "Another request is waiting. Try again in a
moment.", "Restart the desktop, then try again." The terminal prompt is
`Password for alice: `, after a line that gives the action's sentence.

### 6. Words

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

### Phase 1: Text on its line

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

### Phase 2: One theme

- Add `theme-core` with the roles, the dark table and the contrast tests.
- appkit reads it through `ctx`. The compositor's `userland/src/theme.rs`
  becomes a view over it, without the alias block.
- Move every app palette onto roles. Remove appkit's own hard-coded colours.
- Add `check_ui_literals.sh`, with its self-test, to
  `check-framekernel-gates`.

Grading: `theme-core`'s host tests, the gate's self-test, and the gate run
over the tree.

### Phase 3: Components, icons, audit and screenshots

- Add the text roles, the components, `WindowSpec` and the icon font, and
  rebuild `Dialog` on them.
- Add `appkit::audit` and run it in `appkit_test` over every component and
  gallery page.
- Make the widget gallery the catalogue: every component in every state.
- Add `just ui-shots` (`scripts/ui_shots.py`). It boots the live ISO with a
  QMP socket and a USB tablet for absolute pointer moves. It follows a scene
  script of keys and clicks, waits for serial markers, and writes PNGs to
  `builddir/ui-shots/`: each gallery page, each installer page driven against
  a blank scratch disk, the consent dialog, the lock and
  login screens, the system menu, and each app's main window.
- Write `appkit/GUIDE.md`: the components, when to use each, the voice and
  glossary, and the screenshot loop. This is what an agent or a person reads
  before writing an app.

Grading: `appkit_test` with the audit, and `just ui-shots` producing every
scene. The screenshots are for review and are not compared to goldens.

### Phase 4: The permission dialog

- Add app identity resolution in privd, the new consent frame version, and
  the `describe`, `detail`, `verb` and `destructive` settings with their
  validator.
- Rewrite all 33 actions. Extension declarations share the validator.
- Build the dialog from components through the embedded runner.
- Rewrite the refusals and the terminal prompt.

Grading:

- `priv-core` host tests: the validator, and every action in the base policy.
- `privd_test` and `extension_action_test` keep answering through `test_key`.
  The frames they check gain the app name.
- A new case asks from a program with no base entry and from one whose
  `/usr/local` entry claims another app's name. The dialog must say "An
  unidentified program" for both.

### Phase 5: Chrome

- Rework window frames, shadows and placement.
- Make the clock show wall time, and add power entries and the update notice
  to the system menu.
- Rebuild the lock and login screens and the popovers through the embedded
  runner. Redraw the dock's icons.
- Remove the placeholder and the alias block.

Grading: the compositor's existing tests, `apps_test` and `dock_test`, plus a
`chrome-core` host test of the placement rule (centred, cascaded, never under
the dock, shrunk to fit). The `ui-shots` scenes show the result.

### Phase 6: Words and apps

- Publish `/dev/disk/by-id` from the block layer.
- Rewrite `installer-core`'s words, issues and proposal lines to the voice and
  glossary, and build the installer's pages from `Page`, `PropertyList`,
  `ListRow`, `Notice` and `Disclosure`. The log moves under *Show details*.
- Rebuild Files with a `Sidebar`, `Toolbar`, icons and `EmptyState`. Give the
  editor its one name and its errors a next step. Give the image viewer the
  same. Fix System Monitor's network row and task counts, and use sentence
  case for its headings. Give the terminal an inner margin.
- Give every app entry, title and About box one name.

Grading:

- `installer-core`'s copy rules over every fixture.
- `installer_ui_test` and `just test-installer`, whose grep of the CLI's
  output lines moves with the CLI's wording in the same commit.
- `apps_test` and `editor_test`.
- A `devfs` test for the `by-id` names on QEMU's NVMe, virtio and USB disks.

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
