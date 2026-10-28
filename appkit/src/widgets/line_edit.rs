//! A line of text whose text, caret and focus the app keeps: every key is
//! reported, and a tab stop reports focus arriving and leaving.

use std::any::Any;
use std::cell::Cell;

use slopos_theme_core::{Icon, TextRole, space};

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{
    EventResponse, Key, MessageSink, Modifiers, NamedKey, PointerButton, WidgetEvent,
};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

const ROLE: TextRole = TextRole::Body;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineEditInput {
    Text {
        character: char,
    },
    Key {
        key: Key,
        modifiers: Modifiers,
    },
    /// Placed by a click, in characters.
    Caret {
        col: usize,
    },
    Focus,
    Blur,
}

type InputCallback = Box<dyn Fn(LineEditInput) -> Box<dyn Any>>;

pub struct LineEditWidget {
    core: WidgetCore,
    text: String,
    placeholder: String,
    /// In characters.
    caret: usize,
    focused: bool,
    icon: Option<Icon>,
    suffix: String,
    invalid: bool,
    tab_stop: bool,
    on_input: Option<InputCallback>,
    padding_h: i32,
    placeholder_cut: Cell<bool>,
}

impl LineEditWidget {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        text: String,
        placeholder: String,
        caret: usize,
        focused: bool,
        icon: Option<Icon>,
        suffix: String,
        invalid: bool,
        on_input: Option<InputCallback>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            text,
            placeholder,
            caret,
            focused,
            icon,
            suffix,
            invalid,
            tab_stop: false,
            on_input,
            padding_h: 0,
            placeholder_cut: Cell::new(false),
        }
    }

    pub fn with_tab_stop(mut self, tab_stop: bool) -> Self {
        self.tab_stop = tab_stop;
        self
    }

    fn text_x(&self) -> i32 {
        let icon = match self.icon {
            Some(_) => ROLE.style().icon + space::XS,
            None => 0,
        };
        self.layout_rect().x + self.padding_h + icon
    }

    fn prefix_width(&self, chars: usize) -> i32 {
        let prefix: String = self.text.chars().take(chars).collect();
        ui::width(&prefix, ROLE)
    }

    /// Inside the padding, the icon and the suffix.
    fn content_width(&self) -> i32 {
        let rect = self.layout_rect();
        let suffix = if self.suffix.is_empty() {
            0
        } else {
            ui::width(&self.suffix, ROLE) + self.padding_h
        };
        (rect.x + rect.width - self.text_x() - self.padding_h - suffix).max(0)
    }

    /// How far the text is scrolled left so the caret shows.
    fn scroll_offset(&self) -> i32 {
        let caret_x = self.prefix_width(self.caret);
        let width = self.content_width();
        if width <= 0 {
            return 0;
        }
        (caret_x - width + super::CARET).max(0)
    }

    fn col_at(&self, x: i32) -> usize {
        let local = x - self.text_x() + self.scroll_offset();
        if local <= 0 {
            return 0;
        }
        // As prefixes: a sum of rounded advances drifts a pixel a glyph.
        let byte = ui::prefix_fitting(&self.text, ROLE, local);
        let col = self.text[..byte].chars().count();
        let before = self.prefix_width(col);
        let after = self.prefix_width(col + 1);
        if after > before && local >= (before + after) / 2 {
            col + 1
        } else {
            col
        }
    }

    fn emit(&self, input: LineEditInput, sink: &mut MessageSink) -> EventResponse {
        match &self.on_input {
            Some(cb) => {
                sink.emit_raw(cb(input));
                EventResponse::Consumed
            }
            None => EventResponse::Ignored,
        }
    }
}

impl Widget for LineEditWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.padding_h = ctx.style.field_padding_h;
        let height = ctx.style.control_height;
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            ui::width(&self.text, ROLE).max(ctx.style.field_min_width)
        };
        constraints.constrain(Size::new(width, height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let theme = ctx.theme();

        let edge = if self.invalid {
            theme.danger
        } else if self.focused {
            theme.focus
        } else {
            theme.border_strong
        };
        let lit = ctx.lit(self.id()).is_some();
        super::field_frame(ctx, rect, self.focused, lit, edge);

        if let Some(icon) = self.icon {
            let size = ROLE.style().icon;
            ctx.draw_icon(
                icon,
                rect.x + self.padding_h,
                rect.y + (rect.height - size) / 2,
                size,
                theme.text_secondary,
            );
        }

        let text_y = ui::centred_top(rect.y, rect.height, ROLE);
        let text_x = self.text_x() - self.scroll_offset();

        let content = Rect::new(self.text_x(), rect.y, self.content_width(), rect.height);

        self.placeholder_cut.set(false);
        ctx.with_viewport(content, |ctx| {
            if self.text.is_empty() {
                let (room, ink) = (self.content_width(), theme.text_secondary);
                let cut = ctx.draw_text_fitted(text_x, text_y, &self.placeholder, ROLE, room, ink);
                self.placeholder_cut.set(cut);
            } else {
                ctx.draw_text(text_x, text_y, &self.text, ROLE, theme.text);
            }
            if self.focused {
                let caret_x = text_x + self.prefix_width(self.caret);
                let h = ui::font_height(ROLE);
                ctx.fill_rect(caret_x, text_y, super::CARET, h, theme.accent);
            }
        });

        if !self.suffix.is_empty() {
            let x = rect.x + rect.width - self.padding_h - ui::width(&self.suffix, ROLE);
            ctx.draw_text(x, text_y, &self.suffix, ROLE, theme.text_secondary);
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            WidgetEvent::FocusGained if self.tab_stop => {
                self.focused = true;
                self.emit(LineEditInput::Focus, sink)
            }
            WidgetEvent::FocusLost if self.tab_stop => {
                self.focused = false;
                self.emit(LineEditInput::Blur, sink)
            }
            WidgetEvent::KeyDown {
                key: Key::Named(NamedKey::Tab),
                ..
            } if self.tab_stop => EventResponse::Ignored,

            WidgetEvent::PointerDown { x, y, button, .. } => {
                if !self.layout_rect().contains(*x, *y) || *button != PointerButton::Left {
                    return EventResponse::Ignored;
                }
                if !self.focused {
                    self.emit(LineEditInput::Focus, sink);
                }
                let col = self.col_at(*x);
                self.emit(LineEditInput::Caret { col }, sink)
            }

            WidgetEvent::TextInput { character } => {
                if !self.focused {
                    return EventResponse::Ignored;
                }
                self.emit(
                    LineEditInput::Text {
                        character: *character,
                    },
                    sink,
                )
            }

            WidgetEvent::KeyDown { key, modifiers, .. } => {
                if !self.focused {
                    return EventResponse::Ignored;
                }
                self.emit(
                    LineEditInput::Key {
                        key: *key,
                        modifiers: *modifiers,
                    },
                    sink,
                )
            }

            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::TextField
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.placeholder)
    }

    fn focus_policy(&self) -> FocusPolicy {
        if self.tab_stop {
            FocusPolicy::StrongFocus
        } else {
            FocusPolicy::ClickFocus
        }
    }

    fn declares_focus(&self) -> bool {
        self.focused
    }

    fn hover_part(&self, x: i32, y: i32) -> Option<usize> {
        super::button::Press::hover_part(true, self.layout_rect(), x, y)
    }

    /// The placeholder, while it shows cut short: the text typed scrolls to
    /// the caret instead.
    fn tooltip(&self, _x: i32, _y: i32) -> Option<&str> {
        self.placeholder_cut
            .get()
            .then_some(self.placeholder.as_str())
    }
}

/// Text and caret for one input line, where the keys a line edit reports take
/// effect.
#[derive(Clone, Debug, Default)]
pub struct LineInput {
    pub text: String,
    /// In characters.
    pub caret: usize,
}

impl LineInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_text(text: String) -> Self {
        let caret = text.chars().count();
        Self { text, caret }
    }

    fn byte_of(&self, col: usize) -> usize {
        self.text
            .char_indices()
            .nth(col)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn insert(&mut self, c: char) {
        let at = self.byte_of(self.caret);
        self.text.insert(at, c);
        self.caret += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' || c == '\r' {
                continue;
            }
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        let end = self.byte_of(self.caret);
        let start = self.byte_of(self.caret - 1);
        self.text.replace_range(start..end, "");
        self.caret -= 1;
    }

    pub fn delete(&mut self) {
        if self.caret >= self.len() {
            return;
        }
        let start = self.byte_of(self.caret);
        let end = self.byte_of(self.caret + 1);
        self.text.replace_range(start..end, "");
    }

    pub fn set_caret(&mut self, col: usize) {
        self.caret = col.min(self.len());
    }

    pub fn left(&mut self) {
        self.caret = self.caret.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.caret = (self.caret + 1).min(self.len());
    }

    pub fn home(&mut self) {
        self.caret = 0;
    }

    pub fn end(&mut self) {
        self.caret = self.len();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
    }

    /// Deletes back to the start of the word, as Ctrl+Backspace does; a `/`
    /// ends a word too, since a line most often holds a path.
    pub fn delete_word_left(&mut self) {
        let boundary = |c: char| c.is_whitespace() || c == '/';
        let chars: Vec<char> = self.text.chars().collect();
        let mut start = self.caret;
        while start > 0 && boundary(chars[start - 1]) {
            start -= 1;
        }
        while start > 0 && !boundary(chars[start - 1]) {
            start -= 1;
        }
        let end_byte = self.byte_of(self.caret);
        let start_byte = self.byte_of(start);
        self.text.replace_range(start_byte..end_byte, "");
        self.caret = start;
    }

    /// Applies a key that edits or moves within the line; false for one the
    /// caller interprets, as Enter, Escape, Up, Down and Tab.
    pub fn handle_key(&mut self, key: Key, mods: Modifiers) -> bool {
        match key {
            Key::Named(NamedKey::Space) if !mods.ctrl && !mods.plain_alt() => {
                self.insert(' ');
                true
            }
            Key::Named(NamedKey::Backspace) => {
                if mods.ctrl {
                    self.delete_word_left();
                } else {
                    self.backspace();
                }
                true
            }
            Key::Named(NamedKey::Delete) => {
                self.delete();
                true
            }
            Key::Named(NamedKey::Left) => {
                self.left();
                true
            }
            Key::Named(NamedKey::Right) => {
                self.right();
                true
            }
            Key::Named(NamedKey::Home) => {
                self.home();
                true
            }
            Key::Named(NamedKey::End) => {
                self.end();
                true
            }
            Key::Char('a') if mods.ctrl => {
                self.home();
                true
            }
            Key::Char('e') if mods.ctrl => {
                self.end();
                true
            }
            Key::Char('u') if mods.ctrl => {
                self.clear();
                true
            }
            _ => false,
        }
    }
}
