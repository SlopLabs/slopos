//! Pure shell frontend: input framing, token recognition, the syntax tree and
//! its parser, plus the pattern matcher, field splitter and arithmetic
//! evaluator the expander drives.
//!
//! Nothing here touches a syscall, a variable table or a file, which is what
//! makes the shell's grammar host-testable; the userland shell keeps only the
//! parts that must talk to the kernel.
//!
//! A shell shares its input descriptor with the commands it runs, so
//! [`ScriptReader`] frames lines one byte per call: it consumes the line it
//! returns and not one byte more.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod arith;
pub mod ast;
pub mod fields;
pub mod lexer;
pub mod param;
pub mod pattern;
pub mod qbuf;
pub mod script;
pub mod syntax;

pub use script::{ByteSource, Line, ScriptReader, SourceError};
