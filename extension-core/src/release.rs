//! `/.extension/release`: what an image says it is.
//!
//! ```text
//! name     hello
//! version  3
//! display  1.2-beta
//! summary  Says hello to whoever connects
//! ```
//!
//! `version` is what updates are ordered by; `display` is free and defaults
//! to it.

use alloc::string::{String, ToString};

use crate::words::{LineError, lines, once, parse_number, single, valid_text};

const DISPLAY_MAX: usize = 32;
const SUMMARY_MAX: usize = 120;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Release {
    pub name: String,
    pub version: u64,
    pub display: String,
    pub summary: Option<String>,
}

impl Release {
    pub fn parse(text: &str) -> Result<Release, LineError> {
        let (mut name, mut version, mut display, mut summary) = (None, None, None, None);
        for (line, indented, keyword, rest) in lines(text) {
            let fail = |msg| LineError { line, msg };
            if indented {
                return Err(fail("a release line is not indented"));
            }
            match keyword {
                "name" => {
                    let word = single(rest).filter(|w| crate::valid_name(w)).ok_or(fail(
                        "name is lowercase letters, digits and -, not reserved",
                    ))?;
                    once(&mut name, word.to_string()).map_err(fail)?;
                }
                "version" => {
                    let n = single(rest)
                        .and_then(parse_number)
                        .ok_or(fail("version is a number"))?;
                    once(&mut version, n).map_err(fail)?;
                }
                "display" => {
                    let word = single(rest)
                        .filter(|w| valid_text(w, DISPLAY_MAX))
                        .ok_or(fail("display is one printable word"))?;
                    once(&mut display, word.to_string()).map_err(fail)?;
                }
                "summary" => {
                    if !valid_text(rest, SUMMARY_MAX) {
                        return Err(fail("summary is a line of printable ASCII"));
                    }
                    once(&mut summary, rest.to_string()).map_err(fail)?;
                }
                _ => return Err(fail("unknown setting")),
            }
        }
        let whole = |msg| LineError { line: 0, msg };
        let version = version.ok_or(whole("the release names no version"))?;
        Ok(Release {
            name: name.ok_or(whole("the release names no name"))?,
            version,
            display: display.unwrap_or_else(|| version.to_string()),
            summary,
        })
    }
}

#[cfg(test)]
mod tests;
