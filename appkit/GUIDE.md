# Building an app with appkit

Read this before writing or changing an app. appkit gives an app its
components, and the components carry the spacing, type, colour, focus and
keyboard behaviour. An app that is built only from them looks and behaves like
the rest of the desktop without trying.

## The shape of an app

An app implements `App`. `view` turns its state into a `Node` tree, `update`
applies a message, and `run_app(app)` opens the window that `window` asks for:

```rust
use slopos_appkit::{
    Action, App, Button, ButtonRow, Node, Page, PropertyList, Section, Text, WindowSpec,
};

struct About;

#[derive(Clone)]
enum Msg {
    Close,
}

impl App for About {
    type Message = Msg;

    fn view(&self) -> Node<Msg> {
        Page::new("About this computer")
            .subtitle("What SlopOS is running on.")
            .content(Section::new(
                "Hardware",
                PropertyList::new()
                    .row("Processor", "Intel Core i7-1260P")
                    .row("Memory", "16 GB"),
            ))
            .footer(ButtonRow::new().confirm(Button::new("Close").on_press(Msg::Close)))
            .into()
    }

    fn update(&mut self, msg: Msg) -> Action {
        match msg {
            Msg::Close => Action::Exit,
        }
    }

    fn title(&self) -> &str {
        "About"
    }

    fn app_id(&self) -> &str {
        "org.slopos.about"
    }

    fn window(&self) -> WindowSpec {
        WindowSpec::new(560, 420)
    }
}

fn main() {
    slopos_appkit::run_app(About);
}
```

The tree is rebuilt after every message that returns `Action::Rebuild`. The
app keeps all of its state. A widget keeps nothing between rebuilds except
what the app gives it back, such as a caret or a slider grab. A scroll view
whose offset the app does not keep holds its place while it shows the same
thing, and starts at the top when it shows something else. A table cannot
tell that by itself, so give `Node::Table` a `shows` that changes when its
rows are of something else, such as the directory a file manager lists.

`WindowSpec` gives the size the window opens at and the smallest size it can
be laid out at. `.fixed()` makes a window that cannot be resized. The
compositor enforces both limits, the same way xdg-shell's `set_min_size` and
`set_max_size` work.

## Components

Choose the component that says what a thing is. A stack with padding is a
last resort.

| Component | Use it for |
|---|---|
| `Page` | A whole screen: a title, a sentence about it, content that scrolls, and a `ButtonRow` under a separator. |
| `Section` | A heading and the content it heads. Put sections in a column with `space::XL` between them. |
| `PropertyList` | Names and their values, such as a disk's model or a review of choices. The name column is as wide as the widest name, and each row sits on one baseline. |
| `Notice` | Something the person must know before going on. Its tone picks its icon: info, success, warning, danger. It can carry one action. |
| `ButtonRow` | The buttons that end a page or a dialog. `.back()` and `.aside()` go on the left, and `.cancel()`, `.confirm()` and `.destructive()` on the right, in the order you add them; the one that finishes goes last. In a `Page`'s or a dialog's footer, Enter presses the confirming button and Escape presses back or cancel. Enter never reaches a destructive button through the footer: only one that has focus takes it. Left and Right move focus along the row. |
| `Button` | A single button outside a row. A button is secondary unless you call `.primary()` or `.destructive()`. |
| `List`, `ListRow` | Rows the person chooses from, such as disks, places or results. The whole list is one tab stop and the arrows move the choice. Enter opens the chosen row when the list has something to open, and otherwise reaches the page's `ButtonRow`. A row that cannot be chosen is built with `.disabled(reason)`, and the reason is shown in place of its detail. |
| `Sidebar` | The places an app moves between, down its left side: a `List`, optionally in sections. `Sidebar::empty(reason)` says why there are none, as when a filter matches nothing. |
| `Toolbar`, `IconButton` | Tools across the top of a view. An icon button's label is its tooltip, so always give it a label. |
| `SearchField` | A field that filters what the view shows. |
| `Tag` | One word that marks what happens to a thing: *Erased*, *New*, *Kept*. |
| `EmptyState` | What a view shows when it has nothing to show: why it is empty, and what to do about it. |
| `Disclosure` | More detail the person may open, such as a log or a command line. |
| `Node::Dialog` | A question over the page. It covers the page for both the pointer and the keyboard until it is answered, and its footer is a `ButtonRow`. |
| `Text` | Any text. `.role()` sets its size: `Title`, `Heading`, `Body`, `Label`, `Caption`, or `Mono` for code and command lines. `.tone()` sets its colour. `.wrap()` breaks prose at spaces, and `.wrap_anywhere()` breaks a path or command line anywhere so that none of it is lost. |
| `Node::Icon` | A glyph of the icon font, at the size of a text role and in a tone. |

The controls (`Node::TextField`, `Node::LineEdit`, `Node::Checkbox`,
`Node::RadioGroup`, `Node::Slider`, `Node::ProgressBar`,
`Node::PartitionBar`) and the containers (`Node::VStack`, `Node::HStack`,
`Node::ScrollView`, `Node::Expand`) are still available. Use them inside the
components.

The widget gallery (`widget_gallery`, or `widget_gallery --page <name>`)
shows every component in each of its states. Its pages are
`appkit::catalogue`.

## What comes with every component

- **Text that does not fit.** One line ends in `…`, and the whole text shows
  in a tooltip when the pointer rests on it. Wrapped text never loses a word.
- **States.** Every control has hover, pressed, focused and disabled states.
  You never draw one yourself.
- **Focus.** The focused control gets one 2 px ring around the whole control.
  For a list, the ring goes around the focused row. Tab and Shift+Tab move
  focus. While a dialog is open, Tab moves only among the dialog's controls.
- **Keys.** Enter and Escape reach the page's `ButtonRow` when no control took
  them. A focused button takes Enter for itself.
- **Hit targets.** Every control is at least 28 px tall.

## Rules

- **Name roles, never values.** An app never makes a colour (`Color32`), sets
  a font size, or uses a number for spacing or an inset. It names a `Tone`, a
  `Surface`, a `TextRole`, a `theme::space` step or a `theme::inset`.
  `scripts/check_ui_literals.sh` enforces this. A file whose colours are its
  content, such as a syntax palette, needs a line in
  `scripts/gates/ui-literals.txt` that says why.
- **One name per app.** The app entry's `name`, the window title and the About
  box all use the same name.
- **Icons come from the font.** Name a glyph with `Icon::Folder`, `Icon::Trash`
  and so on. The font is Lucide, at `/usr/share/fonts/Lucide.ttf`. To add an
  icon, add a variant that names the codepoint from Lucide's
  `codepoints.json`. theme-core's tests check it against the shipped font.

## Words

Write plainly, in short sentences, in the second person. Keep implementation
terms off any screen where a person must act. Device names, sizes in bytes and
paths go in captions and in details. Use sentence case everywhere except for
proper nouns. Buttons and menu items are verbs, with no full stop. An ellipsis
(`…`, never `...`) marks an item that asks something further. An error says
what happened and what to do next. Give disk sizes in decimal units (GB, TB),
as the drive's label does. Give memory and file sizes in binary units.

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
| app | program, client |
| startup order | boot order, firmware entry, `BootOrder` |

## Checking your work

- **The layout audit.** `appkit::audit` lays a tree out, paints it off screen,
  and reports these problems:
  - text cut by an edge, on any side, rather than ending in an ellipsis; a
    field or a scroll view showing part of what it holds is not cut;
  - siblings that overlap, a child outside its parent, or text drawn over
    other text;
  - a centred or baseline row whose texts do not share a baseline;
  - a focusable control whose focus does not show all the way round it;
  - a control under 28 px across;
  - text under the contrast floor against what is behind it.

  `appkit::audit::audit_node(&node, &widths, height)` returns every finding.
  `appkit_test` runs the audit on every component and on every gallery page
  at 360, 640 and 960 px. Add your app's pages to a test the same way.
- **Look at it.** `just ui-shots` boots the live ISO and saves a PNG of every
  surface in `builddir/ui-shots/`: each installer page, the consent dialog,
  the lock and login screens, the system menu, each app, and each gallery
  page. `just ui-shots apps` takes only the apps. `--no-install` stops the
  installer at its review page. Look at the shots before and after a change.
  They are not compared with anything; the audit and the tests are the gate.
