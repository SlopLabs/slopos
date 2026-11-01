//! About: what this computer is and what it runs.

use slopos_appkit::{Node, Notice, PropertyList, Section, Tone};
use slopos_theme_core::words::{Bytes, Decimal};

use crate::{Effect as AppEffect, Msg as AppMsg, Panel};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub version: String,
    pub processor: String,
    pub processors: u32,
    pub memory: u64,
    /// The root's size and what is free of it, in bytes.
    pub storage: Option<(u64, u64)>,
    pub name: String,
}

#[derive(Clone, Debug)]
pub enum Msg {
    Read(Facts),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Read the facts; answer [`Msg::Read`].
    Read,
}

#[derive(Default)]
pub struct About {
    facts: Option<Facts>,
}

impl About {
    pub fn update(&mut self, msg: Msg) -> Vec<AppEffect> {
        match msg {
            Msg::Read(facts) => self.facts = Some(facts),
        }
        Vec::new()
    }

    pub fn view(&self) -> Node<AppMsg> {
        let about = "What this computer is and what it runs.";
        let Some(facts) = &self.facts else {
            let reading = Notice::new(Tone::Info, "Reading about this computer");
            return crate::page(Panel::About, about, vec![reading.into()], None);
        };
        let processors = match facts.processors {
            1 => String::from("1 processor"),
            n => format!("{n} processors"),
        };
        let mut computer = PropertyList::new()
            .row("Name", facts.name.clone())
            .row("Processor", format!("{} · {processors}", facts.processor))
            .row("Memory", Bytes(facts.memory).to_string());
        if let Some((size, free)) = facts.storage {
            computer = computer.row(
                "Storage",
                format!("{} · {} free", Decimal(size), Decimal(free)),
            );
        }
        let parts = vec![
            Section::new("Computer", computer).into(),
            Section::new(
                "System",
                PropertyList::new().row("Version", facts.version.clone()),
            )
            .into(),
        ];
        crate::page(Panel::About, about, parts, None)
    }
}
