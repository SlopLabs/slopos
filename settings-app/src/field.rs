//! A line a person types into, and whether it has the keyboard.

use slopos_appkit::{
    Key, LineEditInput, LineInput, NamedKey, Node, PasswordField, Text, Tone, row,
};
use slopos_theme_core::{Icon, TextRole, space};

#[derive(Clone, Default)]
pub struct Field {
    pub input: LineInput,
    pub focused: bool,
    /// Focus left it holding something, so what is wrong with it shows.
    pub left: bool,
}

/// Its length alone, since what a person types may be a password.
impl std::fmt::Debug for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Field(<{} bytes>)", self.input.text.len())
    }
}

/// What a field did not take: Enter, which ends what it is part of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Left {
    Nothing,
    Enter,
}

impl Field {
    pub fn with(text: impl Into<String>) -> Field {
        Field {
            input: LineInput::with_text(text.into()),
            ..Field::default()
        }
    }

    pub fn text(&self) -> &str {
        &self.input.text
    }

    pub fn edit(&mut self, input: LineEditInput) -> Left {
        match input {
            LineEditInput::Text { character } => self.input.insert(character),
            LineEditInput::Caret { col } => self.input.set_caret(col),
            LineEditInput::Focus => self.focused = true,
            LineEditInput::Blur => {
                self.focused = false;
                self.left |= !self.input.text.trim().is_empty();
            }
            LineEditInput::Key {
                key: Key::Named(NamedKey::Enter),
                ..
            } => return Left::Enter,
            LineEditInput::Key { key, modifiers } => {
                self.input.handle_key(key, modifiers);
            }
        }
        Left::Nothing
    }

    pub fn line<M>(&self, placeholder: &str, on_input: fn(LineEditInput) -> M) -> Node<M> {
        self.checked(placeholder, false, on_input)
    }

    /// Edged in the danger tone while what it holds is `wrong`.
    pub fn checked<M>(
        &self,
        placeholder: &str,
        wrong: bool,
        on_input: fn(LineEditInput) -> M,
    ) -> Node<M> {
        Node::LineEdit {
            text: self.input.text.clone(),
            placeholder: placeholder.into(),
            caret: self.input.caret,
            focused: self.focused,
            icon: None,
            suffix: String::new(),
            invalid: wrong,
            tab_stop: true,
            on_input: Some(on_input),
        }
    }

    pub fn secret<M>(
        &self,
        placeholder: &str,
        wrong: bool,
        on_input: fn(LineEditInput) -> M,
    ) -> Node<M> {
        PasswordField::new(&self.input)
            .focused(self.focused)
            .placeholder(placeholder)
            .invalid(wrong)
            .on_input(on_input)
            .into()
    }

    /// A field for a secret, with room enough that typing never moves what
    /// is typed and leaves a copy behind.
    pub fn for_secret() -> Field {
        Field {
            input: LineInput {
                text: String::with_capacity(256),
                caret: 0,
            },
            ..Field::default()
        }
    }

    /// Empty, every byte it held zeroed, what was deleted included.
    pub fn wipe(&mut self) {
        let mut bytes = std::mem::take(&mut self.input.text).into_bytes();
        let room = bytes.capacity();
        bytes.clear();
        bytes.resize(room, 0);
        std::hint::black_box(&bytes);
        self.input.caret = 0;
        self.left = false;
    }
}

/// `field` over what is wrong with it, when anything is.
pub fn with_error<M>(field: Node<M>, error: Option<&str>) -> Node<M> {
    with_caption(field, error.map(|why| (why, Tone::Danger)))
}

/// `field` over a line about it in `tone`, when there is one.
pub fn with_caption<M>(field: Node<M>, caption: Option<(&str, Tone)>) -> Node<M> {
    match caption {
        None => field,
        Some((said, tone)) => {
            slopos_appkit::column(space::XS, vec![field, caption_text(said, tone)])
        }
    }
}

pub fn caption_text<M>(said: &str, tone: Tone) -> Node<M> {
    Text::new(said)
        .role(TextRole::Caption)
        .tone(tone)
        .wrap()
        .into()
}

pub fn locked_text<M>(said: &str) -> Node<M> {
    row(
        space::XS,
        vec![
            Node::Icon {
                icon: Icon::Lock,
                role: TextRole::Caption,
                tone: Tone::Secondary,
            },
            Node::Expand {
                weight: 1,
                child: Box::new(caption_text(said, Tone::Secondary)),
            },
        ],
    )
}
