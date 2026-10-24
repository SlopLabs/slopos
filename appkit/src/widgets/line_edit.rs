//! A single-line input whose text and caret live in the application.
//!
//! [`super::text_field::TextFieldWidget`] keeps its own text and its own idea of
//! focus, which works for a form and not for an editor: the widget tree is
//! rebuilt on every message, so anything the widget remembers is lost. This one
//! holds nothing — text, caret and focus are all given to it, every keystroke is
//! reported, and a tab stop reports focus arriving and leaving — which makes a
//! find bar, a path prompt, a command palette and a form's field all the same
//! widget with different state behind them.

use std::any::Any;

use crate::constraints::{BoxConstraints, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, Modifiers, NamedKey, PointerButton, WidgetEvent,
};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

use super::icon::{IconKind, draw_icon};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineEditInput {
    Text {
        character: char,
    },
    Key {
        key: Key,
        modifiers: Modifiers,
    },
    /// Caret placed by a click, in characters.
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
    /// Caret position in characters.
    caret: usize,
    focused: bool,
    icon: Option<IconKind>,
    /// Drawn on the right: a match counter, a hint, an error.
    suffix: String,
    /// Red when the content is not usable — a search with no match.
    invalid: bool,
    tab_stop: bool,
    on_input: Option<InputCallback>,
    font_size: i32,
    padding_h: i32,
}

impl LineEditWidget {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        text: String,
        placeholder: String,
        caret: usize,
        focused: bool,
        icon: Option<IconKind>,
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
            font_size: 14,
            padding_h: 8,
        }
    }

    pub fn with_tab_stop(mut self, tab_stop: bool) -> Self {
        self.tab_stop = tab_stop;
        self
    }

    fn text_x(&self) -> i32 {
        let rect = self.layout_rect();
        let icon = if self.icon.is_some() {
            self.icon_size() + self.padding_h / 2
        } else {
            0
        };
        rect.x + self.padding_h + icon
    }

    fn icon_size(&self) -> i32 {
        (self.font_size + 2).clamp(10, 20)
    }

    fn prefix_width(&self, chars: usize) -> i32 {
        let prefix: String = self.text.chars().take(chars).collect();
        crate::text::ui::width(&prefix, self.font_size.max(1) as u16)
    }

    /// Width of the content area, inside the padding, the icon and the suffix.
    fn content_width(&self) -> i32 {
        let rect = self.layout_rect();
        let suffix = if self.suffix.is_empty() {
            0
        } else {
            crate::text::ui::width(&self.suffix, self.font_size.max(1) as u16) + self.padding_h
        };
        (rect.x + rect.width - self.text_x() - self.padding_h - suffix).max(0)
    }

    /// How far the text is scrolled left so the caret stays in view.
    ///
    /// Derived rather than remembered: the widget is rebuilt on every message,
    /// so a stored offset would be gone by the next keystroke — and the caret
    /// and the text it belongs to are all this needs.
    fn scroll_offset(&self) -> i32 {
        let caret_x = self.prefix_width(self.caret);
        let width = self.content_width();
        if width <= 0 {
            return 0;
        }
        // The 2 px keeps the caret bar itself on screen at the right edge.
        (caret_x - width + 2).max(0)
    }

    fn col_at(&self, x: i32) -> usize {
        let local = x - self.text_x() + self.scroll_offset();
        if local <= 0 {
            return 0;
        }
        // Measured as prefixes, never as a sum of per-character advances: the
        // renderer accumulates fractional advances and rounds once at the end,
        // so a per-character sum over-counts by up to a pixel a glyph and the
        // caret would drift further right of the click the longer the text got.
        let size = self.font_size.max(1) as u16;
        let byte = crate::text::ui::prefix_fitting(
            &self.text,
            size,
            local,
            crate::text::ui::Weight::Regular,
        );
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
        self.font_size = ctx.style.font_size;
        self.padding_h = ctx.style.field_padding_h;
        let height = ctx.text_height() + ctx.style.field_padding_v * 2;
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            ctx.text_width(&self.text).max(ctx.style.field_min_width)
        };
        constraints.constrain(Size::new(width, height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let style = ctx.style;
        let radius = style.corner_radius;

        ctx.fill_rounded_rect(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            radius,
            style.bg_primary,
        );
        let border = if self.invalid {
            style.bg_destructive
        } else if self.focused {
            style.border_focused
        } else {
            style.border_default
        };
        ctx.draw_rounded_rect(rect.x, rect.y, rect.width, rect.height, radius, border);

        if let Some(icon) = self.icon {
            let size = self.icon_size();
            draw_icon(
                ctx,
                icon,
                rect.x + self.padding_h,
                rect.y + (rect.height - size) / 2,
                size,
                style.text_secondary,
            );
        }

        let text_h = ctx.text_height();
        let text_y = rect.y + (rect.height - text_h) / 2;
        let text_x = self.text_x() - self.scroll_offset();

        let content =
            crate::constraints::Rect::new(self.text_x(), rect.y, self.content_width(), rect.height);

        ctx.with_clip(content, |ctx| {
            if self.text.is_empty() {
                ctx.draw_text_transparent(text_x, text_y, &self.placeholder, style.text_disabled);
            } else {
                ctx.draw_text_transparent(text_x, text_y, &self.text, style.text_primary);
            }
            if self.focused {
                let caret_x = text_x + self.prefix_width(self.caret);
                ctx.fill_rect(caret_x, text_y, 2, text_h, style.cursor_color);
            }
        });

        if !self.suffix.is_empty() {
            let w = ctx.text_width(&self.suffix);
            ctx.draw_text_transparent(
                rect.x + rect.width - self.padding_h - w,
                text_y,
                &self.suffix,
                style.text_secondary,
            );
        }
    }

    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        if phase != EventPhase::Target && phase != EventPhase::Bubble {
            return EventResponse::Ignored;
        }

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
}

/// Text and caret for one input line. The widget draws it and reports keys;
/// this is where they take effect.
#[derive(Clone, Debug, Default)]
pub struct LineInput {
    pub text: String,
    /// Caret position in characters.
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

    /// Deletes back to the previous word boundary, as Ctrl+Backspace does
    /// everywhere else.
    ///
    /// A separator counts as a boundary, not just whitespace: what this line
    /// most often holds is a path, and a whitespace-only rule makes one
    /// Ctrl+Backspace erase the whole of it rather than the last segment.
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

    /// Applies a key that edits or moves within the line. Returns false for a
    /// key the caller should interpret itself (Enter, Escape, Up, Down, Tab).
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
