//! Token colours.
//!
//! The palette is One Dark — the values an editor's "one dark" has had since
//! Atom shipped it, which is what makes the surface read as an editor to anyone
//! who has used one. Colours are data, so this is a table rather than a
//! derivation.

use slopos_abi::draw::Color32;
use slopos_editor_core::syntax::TokenKind;

pub const KEYWORD: Color32 = Color32::rgb(0xb4, 0x77, 0xcf);
pub const TYPE: Color32 = Color32::rgb(0x6e, 0xb4, 0xbf);
pub const FUNCTION: Color32 = Color32::rgb(0x73, 0xad, 0xe9);
pub const MACRO: Color32 = Color32::rgb(0xbf, 0x95, 0x6a);
pub const CONSTANT: Color32 = Color32::rgb(0xdf, 0xc1, 0x84);
pub const NUMBER: Color32 = Color32::rgb(0xbf, 0x95, 0x6a);
pub const STRING: Color32 = Color32::rgb(0xa1, 0xc1, 0x81);
pub const COMMENT: Color32 = Color32::rgb(0x5d, 0x63, 0x6f);
pub const ATTRIBUTE: Color32 = Color32::rgb(0x74, 0xad, 0xe8);
pub const OPERATOR: Color32 = Color32::rgb(0x6e, 0xb4, 0xbf);
pub const PUNCTUATION: Color32 = Color32::rgb(0xb2, 0xb9, 0xc6);
pub const PROPERTY: Color32 = Color32::rgb(0xd0, 0x72, 0x77);
pub const EMPHASIS: Color32 = Color32::rgb(0xbf, 0x95, 0x6a);
pub const LINK: Color32 = Color32::rgb(0x73, 0xad, 0xe9);
pub const TEXT: Color32 = Color32::rgb(0xac, 0xb2, 0xbe);

pub fn token_color(kind: TokenKind) -> Color32 {
    match kind {
        TokenKind::Keyword => KEYWORD,
        TokenKind::Type => TYPE,
        TokenKind::Function => FUNCTION,
        TokenKind::Macro => MACRO,
        TokenKind::Constant => CONSTANT,
        TokenKind::Number => NUMBER,
        TokenKind::Str => STRING,
        TokenKind::Comment => COMMENT,
        TokenKind::Attribute => ATTRIBUTE,
        TokenKind::Operator => OPERATOR,
        TokenKind::Punctuation => PUNCTUATION,
        TokenKind::Property => PROPERTY,
        TokenKind::Emphasis => EMPHASIS,
        TokenKind::Link => LINK,
        TokenKind::Text => TEXT,
    }
}
