# Building an app with appkit

Read this before writing or changing an app. appkit gives an app its
components, and the components carry the spacing, type, colour, focus and
keyboard behaviour. An app that is built only from them looks and behaves like
the rest of the desktop without trying.

## The shape of an app

An app implements `App`. `view` turns its state into a `Node` tree, `update`
applies a message, and `run_app(app)` opens the window that `window` asks for:

```rust
use slopos_appkit::theme::words::Bytes;
use slopos_appkit::{Action, App, Button, ButtonRow, Node, Page, PropertyList, Section, WindowSpec};

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
                    .row("Memory", Bytes(16 << 30).to_string()),
            ))
            .footer(ButtonRow::new().confirm(Button::new("Close").on_press(Msg::Close)))
            .into()
    }

    fn update(&mut self, msg: Msg) -> Action {
        match msg {
            Msg::Close => Action::Exit,
        }
    }

    fn title(&self) -> String {
        "About".into()
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
what the app gives it back, such as a caret or a slider grab, and three
things it hands the widget built in its place while that shows the same: a
scroll view whose offset the app does not keep, its offset; a table, how far
it is scrolled; and a menu, the row the arrows reached. Shown something else,
each starts afresh. A table cannot tell by itself that its rows are of
something else, so give `Node::Table` a `shows` that changes when they are,
such as the directory a file manager lists.

`title` is read again after every rebuild. A window that shows a document
is titled `document_title(document, app)`, as `notes.txt — Editor`; any other
is titled with the app's name alone.

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
| `List`, `ListRow` | Rows the person chooses from, such as disks, places or results. The whole list is one tab stop and the arrows move the choice. Enter opens the chosen row when the list has something to open, and otherwise reaches the page's `ButtonRow`. A row that cannot be chosen is built with `.disabled(reason)`, and the reason is shown in place of its detail. A row for a person leads with `.avatar()`. |
| `Sidebar` | The places an app moves between, down its left side: a `List`, optionally in sections. `Sidebar::empty(reason)` says why there are none, as when a filter matches nothing. |
| `Toolbar`, `IconButton` | Tools across the top of a view. An icon button's label is its tooltip, so always give it a label. |
| `SearchField` | A field that filters what the view shows. `.placeholder()` says what it finds, *Search* unless told, `.count()` how many matched, and `.invalid(true)` edges it in the danger tone when nothing did. `.tab_stop(false)` keeps a palette's field out of the tab order, so Tab reaches the app as a key. |
| `StatusBar` | The line along a view's foot that says how things stand. `.tone()` colours what it says, as `Danger` for a failure, and `.end()` adds what it always shows at its right, such as a caret's line and column. It is `StatusBar::HEIGHT` tall whatever it says. |
| `Tag` | One word that marks what happens to a thing: *Erased*, *New*, *Kept*. |
| `EmptyState` | What a view shows when it has nothing to show: why it is empty, and what to do about it. |
| `Disclosure` | More detail the person may open, such as a log or a command line. |
| `Dialog` | A question over the page: a title, its content and a `ButtonRow` footer. It covers the page for both the pointer and the keyboard until it is answered. |
| `Text` | Any text. `.role()` sets its size: `Display` for a page that is one number, `Title`, `Heading`, `Body`, `Label`, `Caption`, or `Mono` for code and command lines, set in the cells a code view's columns are; `Hero` is the lock screen's clock and nothing else. `.tone()` sets its colour. `.wrap()` breaks prose at spaces, and `.wrap_anywhere()` breaks a path or command line anywhere so that none of it is lost. |
| `Node::Icon` | A glyph of the icon font, at the size of a text role and in a tone. |
| `Node::Avatar` | A person: the initial of their name in a circle of the colour their name picks. |

The controls (`Node::LineEdit`, `Node::Checkbox`, `Node::Slider`,
`Node::ProgressBar`, `Node::PartitionBar`) and the containers (`Node::VStack`,
`Node::HStack`, `Node::ZStack`, `Node::ScrollView`, `Node::Expand`) go inside
the components. `column(spacing, children)` and `row(spacing, children)` set
components one under another or side by side. One of several choices is a
`List`.

A view of many things, or a document, takes a widget the app steers:

| Node | Use it for |
|---|---|
| `Node::Table` | Rows of cells under column headings, as files or processes. One tab stop; the arrows move the choice, Enter or a second click opens it (`on_activate`), a heading click sorts (`on_header_click`), and a secondary click or the Menu key asks for a context menu (`on_context_menu`, at a `ContextMenuAt`). Give it a `shows` that changes when its rows are of something else. |
| `Node::TreeView` | A tree whose rows, expansion and scroll the app keeps, handing over the rows in view, as a file sidebar does. Every gesture comes back as a `TreeInput`. |
| `Node::CodeView` | Lines of code with a caret and a selection, every gesture back in document coordinates. Its text is `Mono`. |
| `Node::TabBar` | Tabs that switch which of its panels shows. |
| `Node::EditorTabs` | The documents an app has open, each with a dot for unsaved changes and a close button. |
| `Node::MenuBar` | Menu titles across a window's top; the app opens the menu it reports as a `Node::Popup` holding a `Node::Menu`. |
| `Node::DragHandle` | A divider between panes, which reports where the pointer drags it; the app sizes the panes. |

A menu over the view is a `Node::ZStack` whose last child is a `Node::Popup`
at the point the menu opens from, holding a `Node::Menu`; that is how a menu
bar's menu and a table's context menu both open. The popup takes the keyboard
while it is open, the arrows move along the menu and Enter chooses, a click
outside it or Escape sends its `on_dismiss`, and the app closes it by building
the view without it. A table's and a tree's rows are `StyleSheet::row_height`
tall. An app that counts what fits, as the editor counts rows, reads the sizes
from appkit: `ListRow::height`, `StatusBar::HEIGHT`, `ICON_BUTTON_SIDE`, and
`StyleSheet`'s `control_height`, `menu_bar_height`, `tab_height`,
`row_height` and `divider_width`. A window whose content is a table takes its
least width from `TableColumn::least_width`, adding `SIDEBAR_WIDTH` for a
`Sidebar` beside it, and a fixed column's width from `TableColumnWidth::fitting`
over what it shows, which counts every digit as the widest: a number's longest
shape, such as `1023.9 MiB`, stands for every number of that shape.

The widget gallery (`widget_gallery`, or `widget_gallery --page <name>`)
shows every component in each of its states. Its pages are
`appkit::catalogue`.

## What comes with every component

- **Text that does not fit.** A line too long is cut with `…`: at its end,
  or at its start where the end tells it apart, as a path or a file name does
  in a tree's rows, an editor's tabs and a `ListRow::detail_tail`. The whole
  text shows in a tooltip when the pointer rests on it. Wrapped text never
  loses a word, and a field's own text is never cut: it scrolls to the caret.
- **States.** Every control lights under the pointer and shows when it has
  focus. A button, an icon button, a checkbox, a disclosure's header and a
  slider's knob also show while pressed; a field shows focus instead, since a
  press focuses it. A button, an icon button, a checkbox, a slider and a row
  of a list or a menu can be disabled and show it; a field cannot be. You
  never draw a state yourself.
- **Focus.** The focused control gets one 2 px ring around it, a disclosure
  around its header and a tab bar just inside its tabs; a field's own edge
  thickens in the focus colour instead. For a list, a table, a tree or a
  menu, the ring goes around the row the keyboard is on while all of that row
  shows, and around the whole view otherwise. Tab and Shift+Tab move focus. A
  click gives focus to a field, a list or a table, and leaves it where it was
  for a button, so a toolbar button never takes the keyboard from a search
  field. While a dialog is open, Tab moves only among the dialog's controls.
- **Keys.** Enter and Escape reach the page's `ButtonRow` when no control took
  them. A focused button takes Enter for itself; a checkbox takes only Space.
- **Hit targets.** Every control, and every row of a list, a table, a tree or
  a menu, is at least `style::MIN_TARGET` across. A `DragHandle` is a divider
  rather than a control: it is as long as the panes it parts and as wide as
  `StyleSheet::divider_width`.
- **Scrolling.** Every view that scrolls draws the same bar down its right
  edge, and lays what it shows beside it rather than under it. Its thumb
  lights under the pointer, and a press anywhere on the bar moves the thumb
  there and drags it, in a window and in a tree the compositor embeds alike.

## Rules

- **Name roles, never values.** An app never makes a colour (`Color32`), sets
  a font size, or uses a number for spacing or an inset. It names a `Tone`, a
  `Surface`, a `TextRole`, a `theme::space` step or a `theme::inset`.
  `scripts/check_ui_literals.sh` enforces this. A file whose colours are its
  content, such as a syntax palette, needs a line in
  `scripts/gates/ui-literals.txt` that says why.
- **One name per app.** The app entry's `name`, the window title and the About
  box all use the same name, and `apps_test` holds each title to its entry.
- **Errors say what to do next.** A message about a failure is what happened,
  then what the person can do about it, each a sentence.
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
as the drive's label does. Give memory and file sizes in binary units, as
`theme_core::words::Bytes` writes them: `512 bytes`, `1.5 KiB`, `16 GiB`.

The words a screen never says, and what it says in their place, are
`theme_core::words::GLOSSARY`; `words::label` and `words::sentence` hold a
label and a sentence to their shapes. Hold your app's words to them in a test.

## Checking your work

- **The layout audit.** `appkit::audit` lays a tree out, paints it off screen,
  and reports these problems:
  - text cut by an edge, on any side, rather than ending in an ellipsis; a
    field or a scroll view showing part of what it holds is not cut;
  - siblings that overlap, a child outside its parent, or text drawn over
    other text;
  - a centred or baseline row whose texts do not share a baseline;
  - a focusable control whose focus does not show all the way round it;
  - a control under `style::MIN_TARGET` across;
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
