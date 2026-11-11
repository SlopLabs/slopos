//! Keyboard: your layout, a preference init loads while your desktop is in
//! front, and the login screen's, the computer's own in `/etc/keymap`, which
//! everyone who chose none follows. Outcomes take a fixed line, so none moves
//! the page.

use slopos_appkit::{
    Button, ButtonRow, Dialog, KeyCap, KeyboardMap, LineEditInput, List, ListRow, Node, Notice,
    PropertyList, Section, Text, Tone, column, row,
};
use slopos_keymap_core::board::{self, Board, Fixed, Key, Place, Shape};
use slopos_theme_core::{Icon, Surface, TextRole, inset, space};

use crate::field::{Field, caption_text};
use crate::{Ask, Asked, Effect as AppEffect, Msg as AppMsg, Panel, Purpose};

pub const ACTION: &str = "org.slopos.console.keymap";

const YOURS: &str = "You type in it whenever your desktop is on the screen.";
const FOLLOWS: &str = "It follows the login screen's, as you haven't chosen your own.";
const LATER: &str = "It's kept, and takes effect when you next log in.";
const SAME: &str = "Same as the login screen";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub name: String,
    pub title: String,
    pub board: Option<Board>,
}

/// What the window read of the layouts.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Read {
    pub layouts: Vec<Layout>,
    pub yours: Option<String>,
    pub computer: String,
}

#[derive(Clone, Debug)]
pub enum Msg {
    Read(Read),
    /// Row 0 follows the login screen's; then the layouts in order.
    Yours(usize),
    Try(LineEditInput),
    UseYours,
    Change,
    Computer(usize),
    Cancel,
    Confirm,
    /// What came of keeping yours: why it did not take, if it did not.
    Kept(Result<(), Kept>),
}

/// Why a layout chosen is not in use now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kept {
    /// It is kept, for the next login.
    Later,
    /// It is not kept, as this sentence says.
    Not(String),
}

#[derive(Clone, Copy, Default)]
struct ComputerDialog {
    chosen: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Read the layouts; answer [`Msg::Read`].
    Read,
    /// Keep the person's layout, `None` for the computer's, and have it
    /// loaded; answer [`Msg::Kept`].
    Keep(Option<String>),
}

#[derive(Default)]
pub struct Keyboard {
    read: Option<Read>,
    try_it: Field,
    computer_dialog: Option<ComputerDialog>,
    /// One request at a time, since each asks the person.
    asking: bool,
    /// What came of keeping yours, until the next choice.
    yours_said: Option<(Tone, String)>,
    computer_trouble: Option<String>,
}

impl Keyboard {
    fn layout<'a>(&'a self, name: &str) -> Option<&'a Layout> {
        self.read.as_ref()?.layouts.iter().find(|l| l.name == name)
    }

    fn title_of<'a>(&'a self, name: &'a str) -> &'a str {
        self.layout(name).map_or(name, |l| l.title.as_str())
    }

    fn typed(&self) -> Option<char> {
        let input = &self.try_it.input;
        let before = input.caret.checked_sub(1)?;
        self.try_it
            .focused
            .then(|| input.text.chars().nth(before))
            .flatten()
    }

    fn yours_apart(&self) -> Option<&Layout> {
        let read = self.read.as_ref()?;
        let yours = self.layout(read.yours.as_deref()?)?;
        (yours.name != read.computer).then_some(yours)
    }

    fn set_computer(&mut self, name: String) -> Vec<AppEffect> {
        if self.asking {
            return Vec::new();
        }
        self.computer_dialog = None;
        self.asking = true;
        self.computer_trouble = None;
        vec![AppEffect::Ask(Ask {
            purpose: Purpose::Keyboard,
            action: ACTION,
            args: vec!["set".into(), "--computer".into(), name],
            input: crate::Input::default(),
        })]
    }

    pub fn update(&mut self, msg: Msg) -> Vec<AppEffect> {
        match msg {
            Msg::Read(read) => self.read = Some(read),
            Msg::Yours(row) => {
                let Some(read) = &mut self.read else {
                    return Vec::new();
                };
                let yours = row
                    .checked_sub(1)
                    .and_then(|at| read.layouts.get(at))
                    .map(|l| l.name.clone());
                if yours == read.yours {
                    return Vec::new();
                }
                read.yours = yours.clone();
                self.yours_said = None;
                return vec![AppEffect::Keyboard(Effect::Keep(yours))];
            }
            Msg::Kept(Ok(())) => self.yours_said = None,
            Msg::Kept(Err(Kept::Later)) => {
                self.yours_said = Some((Tone::Info, String::from(LATER)));
            }
            Msg::Kept(Err(Kept::Not(why))) => {
                self.yours_said = Some((Tone::Danger, why));
                return vec![AppEffect::Keyboard(Effect::Read)];
            }
            Msg::Try(input) => {
                self.try_it.edit(input);
            }
            Msg::UseYours => {
                let Some(name) = self.yours_apart().map(|l| l.name.clone()) else {
                    return Vec::new();
                };
                return self.set_computer(name);
            }
            Msg::Change => {
                let read = self.read.as_ref();
                let at = read.and_then(|r| r.layouts.iter().position(|l| l.name == r.computer));
                self.computer_dialog = Some(ComputerDialog { chosen: at });
            }
            Msg::Computer(row) => {
                if let Some(dialog) = &mut self.computer_dialog {
                    dialog.chosen = Some(row);
                }
            }
            Msg::Cancel => self.computer_dialog = None,
            Msg::Confirm => {
                let name = self
                    .computer_dialog
                    .and_then(|dialog| dialog.chosen)
                    .and_then(|row| self.read.as_ref()?.layouts.get(row))
                    .map(|l| l.name.clone());
                let Some(name) = name else {
                    return Vec::new();
                };
                return self.set_computer(name);
            }
        }
        Vec::new()
    }

    pub fn answered(&mut self, asked: Asked) -> Vec<AppEffect> {
        self.asking = false;
        self.computer_trouble = asked.trouble();
        vec![AppEffect::Keyboard(Effect::Read)]
    }

    pub fn view(&self) -> Node<AppMsg> {
        let about = "Which letters and symbols the keys type.";
        let Some(read) = &self.read else {
            let reading = Notice::new(Tone::Info, "Reading the layouts");
            return crate::page(Panel::Keyboard, about, vec![reading.into()], None);
        };
        let parts = vec![
            self.in_use(read),
            self.choices(read),
            self.login_screen(read),
        ];
        crate::page(Panel::Keyboard, about, parts, None)
    }

    /// Yours, no longer installed, isn't loaded, so the login screen's shows.
    fn in_use(&self, read: &Read) -> Node<AppMsg> {
        let yours = read.yours.as_deref().and_then(|name| self.layout(name));
        let (title, board) = match yours.or_else(|| self.layout(&read.computer)) {
            Some(layout) => (layout.title.as_str(), layout.board.as_ref()),
            None => (read.computer.as_str(), None),
        };
        let (tone, said, icon) = match (&self.yours_said, yours) {
            (Some((tone, said)), _) => (*tone, said.as_str(), Icon::for_tone(*tone)),
            (None, Some(_)) => (Tone::Secondary, YOURS, Icon::Keyboard),
            (None, None) => (Tone::Secondary, FOLLOWS, Icon::Keyboard),
        };
        let status = row(
            space::XS,
            vec![
                Node::Icon {
                    icon,
                    role: TextRole::Caption,
                    tone,
                },
                Node::Expand {
                    weight: 1,
                    child: Box::new(Text::new(said).role(TextRole::Caption).tone(tone).into()),
                },
            ],
        );
        let heading = column(
            space::XS,
            vec![Text::new(title).role(TextRole::Heading).into(), status],
        );
        let try_it = self
            .try_it
            .line("Type here to try it", |i| AppMsg::Keyboard(Msg::Try(i)));
        let keys = KeyboardMap::new(title, caps(board, self.typed()));
        Node::Card {
            surface: Surface::Raised,
            shadow: false,
            child: Box::new(Node::Padding {
                padding: inset::CARD,
                child: Box::new(column(space::MD, vec![heading, try_it, keys.into()])),
            }),
        }
    }

    fn choices(&self, read: &Read) -> Node<AppMsg> {
        let login_screen = self.title_of(&read.computer).to_string();
        let mut rows = vec![ListRow::new(SAME).detail(login_screen)];
        rows.extend(read.layouts.iter().map(|l| ListRow::new(l.title.clone())));
        let yours = match &read.yours {
            None => 0,
            Some(name) => read
                .layouts
                .iter()
                .position(|l| &l.name == name)
                .map_or(0, |at| at + 1),
        };
        Section::new(
            "Your layout",
            List::new(rows)
                .selected(Some(yours))
                .on_select(|row| AppMsg::Keyboard(Msg::Yours(row))),
        )
        .into()
    }

    /// Disables rather than hides, so a choice never changes the page's height.
    fn login_screen(&self, read: &Read) -> Node<AppMsg> {
        let about = "The login screen uses this layout, and so does everyone who hasn't chosen their own. Give it yours, so your password types the same there.";
        let buttons = vec![
            Button::new("Use your layout")
                .on_press(AppMsg::Keyboard(Msg::UseYours))
                .enabled(self.yours_apart().is_some())
                .into(),
            Button::new("Change…")
                .on_press(AppMsg::Keyboard(Msg::Change))
                .into(),
        ];
        let mut parts = vec![
            Text::new(about).tone(Tone::Secondary).wrap().into(),
            PropertyList::new()
                .row("Layout", self.title_of(&read.computer))
                .into(),
            row(space::SM, buttons),
        ];
        if let Some(why) = &self.computer_trouble {
            parts.push(caption_text(why, Tone::Danger));
        }
        Section::new("Login screen", column(space::SM, parts)).into()
    }

    pub fn dialog(&self) -> Option<Node<AppMsg>> {
        let chosen = self.computer_dialog?.chosen;
        let read = self.read.as_ref()?;
        let rows = read
            .layouts
            .iter()
            .map(|l| ListRow::new(l.title.clone()))
            .collect();
        let changes = chosen
            .and_then(|at| read.layouts.get(at))
            .is_some_and(|l| l.name != read.computer);
        Some(
            Dialog::new("Layout for the login screen")
                .content(
                    List::new(rows)
                        .selected(chosen)
                        .on_select(|row| AppMsg::Keyboard(Msg::Computer(row))),
                )
                .footer(
                    ButtonRow::new()
                        .cancel(Button::new("Cancel").on_press(AppMsg::Keyboard(Msg::Cancel)))
                        .confirm(
                            Button::new("Change")
                                .on_press(AppMsg::Keyboard(Msg::Confirm))
                                .enabled(changes && !self.asking),
                        ),
                )
                .into(),
        )
    }
}

/// A bare keyboard when the layout can't be read, so the height holds.
fn caps(board: Option<&Board>, typed: Option<char>) -> Vec<Vec<KeyCap>> {
    let rows = board.map_or_else(|| board::rows(Shape::Ansi), Board::rows);
    rows.iter()
        .map(|row| row.iter().map(|place| cap(board, place, typed)).collect())
        .collect()
}

fn cap(board: Option<&Board>, place: &Place, typed: Option<char>) -> KeyCap {
    let quarters = u16::from(place.quarters);
    let cap = match place.key {
        Key::Typing(usage) => {
            let legend = board.map(|b| b.legend(usage)).unwrap_or_default();
            let text = |c: Option<char>| c.map(String::from).unwrap_or_default();
            let lit = typed
                .is_some_and(|c| [legend.base, legend.shift, legend.alt_gr].contains(&Some(c)));
            let cap = match legend.letter() {
                Some(capital) => {
                    KeyCap::typing(quarters).legends(capital, String::new(), text(legend.alt_gr))
                }
                None => KeyCap::typing(quarters).legends(
                    text(legend.shift),
                    text(legend.base),
                    text(legend.alt_gr),
                ),
            };
            cap.lit(lit)
        }
        Key::Fixed(fixed) => {
            KeyCap::fixed(quarters).lit(fixed == Fixed::Space && typed == Some(' '))
        }
    };
    match place.joins_above {
        true => cap.joins_above(),
        false => cap,
    }
}
