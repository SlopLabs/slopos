//! Keyboard: which layout the keys type in. Your own is a preference of
//! yours, which nothing needs permission to change and init loads while your
//! desktop is in front; the computer's, which the login screen and everyone
//! who chose none use, is kept in `/etc/keymap` by `keymap`, which privd runs
//! once allowed.

use slopos_appkit::{
    Button, ButtonRow, Dialog, LineEditInput, List, ListRow, Node, Notice, PropertyList, Section,
    Text, Tone, column, row,
};
use slopos_theme_core::space;

use crate::field::Field;
use crate::{Ask, Asked, Effect as AppEffect, Msg as AppMsg, Panel, Purpose};

pub const ACTION: &str = "org.slopos.console.keymap";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub name: String,
    pub title: String,
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
    /// Row 0 is the computer's; the rest are the layouts in order.
    Yours(usize),
    Try(LineEditInput),
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
    asking: bool,
    said: Option<(Tone, String)>,
}

impl Keyboard {
    fn title_of<'a>(&'a self, name: &'a str) -> &'a str {
        self.read
            .as_ref()
            .and_then(|r| r.layouts.iter().find(|l| l.name == name))
            .map_or(name, |l| l.title.as_str())
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
                self.said = None;
                return vec![AppEffect::Keyboard(Effect::Keep(yours))];
            }
            Msg::Kept(Ok(())) => {
                self.said = Some((Tone::Success, String::from("Your layout is in use.")));
            }
            Msg::Kept(Err(Kept::Later)) => {
                let later = "Your layout is kept, and takes effect when you next log in.";
                self.said = Some((Tone::Info, String::from(later)));
            }
            Msg::Kept(Err(Kept::Not(why))) => {
                self.said = Some((Tone::Danger, why));
                return vec![AppEffect::Keyboard(Effect::Read)];
            }
            Msg::Try(input) => {
                self.try_it.edit(input);
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
                let Some(name) = name.filter(|_| !self.asking) else {
                    return Vec::new();
                };
                self.computer_dialog = None;
                self.asking = true;
                self.said = None;
                return vec![AppEffect::Ask(Ask {
                    purpose: Purpose::Keyboard,
                    action: ACTION,
                    args: vec!["set".into(), "--computer".into(), name],
                    input: crate::Input::default(),
                })];
            }
        }
        Vec::new()
    }

    pub fn answered(&mut self, asked: Asked) -> Vec<AppEffect> {
        self.asking = false;
        self.said = Some(match asked.trouble() {
            None => (
                Tone::Success,
                String::from("The computer's layout is changed."),
            ),
            Some(why) => (Tone::Danger, why),
        });
        vec![AppEffect::Keyboard(Effect::Read)]
    }

    pub fn view(&self) -> Node<AppMsg> {
        let about = "Which letters and symbols the keys type.";
        let Some(read) = &self.read else {
            let reading = Notice::new(Tone::Info, "Reading the layouts");
            return crate::page(Panel::Keyboard, about, vec![reading.into()], None);
        };
        let computer = self.title_of(&read.computer).to_string();
        let mut rows = vec![ListRow::new("The computer's layout").detail(computer.clone())];
        rows.extend(
            read.layouts
                .iter()
                .map(|l| ListRow::new(l.title.clone()).detail(l.name.clone())),
        );
        let yours = match &read.yours {
            None => 0,
            Some(name) => read
                .layouts
                .iter()
                .position(|l| &l.name == name)
                .map_or(0, |at| at + 1),
        };
        let mut parts: Vec<Node<AppMsg>> = Vec::new();
        if let Some((tone, said)) = &self.said {
            parts.push(Notice::new(*tone, said.clone()).into());
        }
        parts.push(
            Section::new(
                "Your layout",
                column(
                    space::SM,
                    vec![
                        Text::new("Your desktop uses it whenever it is on the screen.")
                            .tone(Tone::Secondary)
                            .wrap()
                            .into(),
                        List::new(rows)
                            .selected(Some(yours))
                            .on_select(|row| AppMsg::Keyboard(Msg::Yours(row)))
                            .into(),
                    ],
                ),
            )
            .into(),
        );
        parts.push(
            Section::new(
                "Try it",
                self.try_it.line("Type here to try the layout", |i| {
                    AppMsg::Keyboard(Msg::Try(i))
                }),
            )
            .into(),
        );
        parts.push(
            Section::new(
                "The computer's layout",
                column(
                    space::SM,
                    vec![
                        Text::new(
                            "The login screen uses it, and so does everyone who hasn't chosen a layout of their own.",
                        )
                        .tone(Tone::Secondary)
                        .wrap()
                        .into(),
                        PropertyList::new().row("Layout", computer).into(),
                        row(
                            0,
                            vec![
                                Button::new("Change…")
                                    .on_press(AppMsg::Keyboard(Msg::Change))
                                    .enabled(!self.asking)
                                    .into(),
                            ],
                        ),
                    ],
                ),
            )
            .into(),
        );
        crate::page(Panel::Keyboard, about, parts, None)
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
            Dialog::new("The computer's layout")
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
                                .enabled(changes),
                        ),
                )
                .into(),
        )
    }
}
