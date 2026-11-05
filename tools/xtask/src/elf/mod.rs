//! Gates over a linked kernel: what codegen, inlining and the linker actually
//! produced, which no source scan can see. The ELF is read with `object` and
//! its code decoded with `iced-x86`, never by scraping a disassembler's text.

use std::path::Path;

use iced_x86::{Decoder, DecoderOptions};
use object::read::elf::ElfFile64;
use object::{Object, ObjectSection, ObjectSymbol, SectionKind, SymbolKind};

use crate::Result;
use crate::repo::Repo;
use crate::report::{self, Report};

mod authority;
#[cfg(test)]
mod fixture;
pub mod loose_programs;
mod registry_sections;
mod softfloat;
mod stack_rewind;
mod stack_sizes;

/// What a kernel gate is handed: the image and the variant whose allowlists
/// under `scripts/gates/` it is held to.
pub struct Ctx<'a> {
    pub repo: &'a Repo,
    pub path: &'a Path,
    pub variant: &'a str,
    pub image: &'a Image<'a>,
}

pub struct Gate {
    pub name: &'static str,
    /// Run after every kernel build when no gate is named.
    pub per_build: bool,
    pub run: fn(&Ctx) -> Result<Report>,
    /// Writes the variant's allowlist as measured from the image, for a gate
    /// whose allowlist records measurements.
    pub emit: Option<fn(&Ctx) -> Result<String>>,
}

pub const GATES: &[Gate] = &[
    Gate {
        name: "stack-sizes",
        per_build: true,
        run: stack_sizes::run,
        emit: Some(stack_sizes::emit_allowlist),
    },
    Gate {
        name: "softfloat",
        per_build: true,
        run: softfloat::run,
        emit: Some(softfloat::emit_allowlist),
    },
    Gate {
        name: "registry-sections",
        per_build: true,
        run: registry_sections::run,
        emit: None,
    },
    Gate {
        name: "stack-rewind",
        per_build: true,
        run: stack_rewind::run,
        emit: None,
    },
    Gate {
        name: "authority",
        per_build: false,
        run: authority::run,
        emit: Some(authority::emit_allowlist),
    },
];

const ELF_USAGE: &str =
    "usage: xtask elf --variant VARIANT KERNEL [GATE...] | --emit-allowlist GATE";

/// `xtask elf --variant VARIANT KERNEL [GATE...]`, or with `--emit-allowlist
/// GATE` the gate's allowlist measured from KERNEL, on standard output.
pub fn main(repo: &Repo, args: &[String]) -> Result<bool> {
    let mut variant = None;
    let mut emit = None;
    let mut positional = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--variant" => variant = Some(it.next().ok_or("--variant needs a value")?.clone()),
            s if s.starts_with("--variant=") => variant = Some(s["--variant=".len()..].to_string()),
            "--emit-allowlist" => {
                emit = Some(it.next().ok_or("--emit-allowlist needs a gate")?.clone())
            }
            s if s.starts_with('-') => {
                return Err(format!("unknown flag `{s}`\n{ELF_USAGE}").into());
            }
            _ => positional.push(arg.clone()),
        }
    }
    let variant = variant.ok_or("--variant is required (dev, tests, release, release-tests)")?;
    let (path, names) = positional.split_first().ok_or(ELF_USAGE)?;
    let find = |name: &str| {
        GATES
            .iter()
            .find(|g| g.name == name)
            .ok_or_else(|| format!("no elf gate named `{name}` (see `xtask list`)"))
    };
    let gates: Vec<&Gate> = match (&emit, names.is_empty()) {
        (Some(_), false) => return Err(ELF_USAGE.into()),
        (Some(name), true) => vec![find(name)?],
        (None, true) => GATES.iter().filter(|g| g.per_build).collect(),
        (None, false) => names.iter().map(|n| find(n)).collect::<Result<_, _>>()?,
    };

    let path = Path::new(path);
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let image = Image::parse(&data).map_err(|e| format!("{}: {e}", path.display()))?;
    let ctx = Ctx {
        repo,
        path,
        variant: &variant,
        image: &image,
    };
    if emit.is_some() {
        let gate = gates[0];
        let write = gate
            .emit
            .ok_or_else(|| format!("{} keeps no measured allowlist", gate.name))?;
        print!("{}", write(&ctx)?);
        return Ok(true);
    }
    let mut results = Vec::new();
    let mut broken = Vec::new();
    for gate in gates {
        match (gate.run)(&ctx) {
            Ok(r) => results.push((gate.name, r)),
            Err(e) => broken.push(format!("{}: {e}", gate.name)),
        }
    }
    let passed = report::print_all(&results);
    if !broken.is_empty() {
        return Err(broken.join("\n").into());
    }
    Ok(passed)
}

/// A parsed 64-bit ELF. Gates read sections and symbols through `file`, and
/// code through [`regions`].
pub struct Image<'a> {
    pub file: ElfFile64<'a>,
}

impl<'a> Image<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        Ok(Self {
            file: ElfFile64::parse(data)?,
        })
    }
}

/// The name code before a section's first symbol answers to.
pub const NO_SYMBOL: &str = "<no-enclosing-symbol>";

/// A run of code from one symbol address up to the next, as `objdump -d`
/// lists it: decoding restarts at every symbol.
pub struct Region<'a> {
    /// Every symbol at `address`, sorted bytewise; empty for code before a
    /// section's first symbol.
    pub names: Vec<&'a str>,
    pub address: u64,
    pub bytes: &'a [u8],
}

impl<'a> Region<'a> {
    /// The alias `llvm-objdump` prints for the address: the bytewise-last.
    pub fn name(&self) -> &'a str {
        self.names.last().copied().unwrap_or(NO_SYMBOL)
    }

    pub fn decoder(&self) -> Decoder<'a> {
        Decoder::with_ip(64, self.bytes, self.address, DecoderOptions::NONE)
    }
}

/// The executable sections' code split at symbol addresses, sorted by
/// address. `only` restricts the walk to one section, which must exist.
pub fn regions<'a>(image: &Image<'a>, only: Option<&str>) -> Result<Vec<Region<'a>>> {
    let mut out = Vec::new();
    let mut found = false;
    for section in image.file.sections() {
        if section.kind() != SectionKind::Text {
            continue;
        }
        if let Some(want) = only
            && section.name()? != want
        {
            continue;
        }
        found = true;
        let data = section.data()?;
        let start = section.address();
        let end = start + data.len() as u64;
        let mut syms: Vec<(u64, &'a str)> = image
            .file
            .symbols()
            .filter(|s| {
                s.section_index() == Some(section.index())
                    && !matches!(s.kind(), SymbolKind::Section | SymbolKind::File)
                    && (start..end).contains(&s.address())
            })
            .filter_map(|s| Some((s.address(), s.name().ok().filter(|n| !n.is_empty())?)))
            .collect();
        syms.sort_unstable();
        syms.dedup();
        let mut cuts: Vec<(u64, Vec<&'a str>)> = Vec::new();
        if syms.first().is_none_or(|&(a, _)| a > start) {
            cuts.push((start, Vec::new()));
        }
        for (address, name) in syms {
            match cuts.last_mut() {
                Some((a, names)) if *a == address => names.push(name),
                _ => cuts.push((address, vec![name])),
            }
        }
        for (i, (address, names)) in cuts.iter().enumerate() {
            let next = cuts.get(i + 1).map_or(end, |c| c.0);
            if next == *address {
                continue;
            }
            let lo = usize::try_from(address - start)?;
            let hi = usize::try_from(next - start)?;
            out.push(Region {
                names: names.clone(),
                address: *address,
                bytes: &data[lo..hi],
            });
        }
    }
    if let Some(want) = only
        && !found
    {
        return Err(format!("the image has no executable {want} section").into());
    }
    out.sort_by_key(|r| r.address);
    Ok(out)
}

/// A symbol's demangled name, without the hash: for messages, not matching.
pub fn demangle(name: &str) -> String {
    format!("{:#}", rustc_demangle::demangle(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demangling_drops_the_hash() {
        assert_eq!(
            demangle("_ZN11slopos_ostd4task3fpu4save17h0123456789abcdefE"),
            "slopos_ostd::task::fpu::save"
        );
    }
}
