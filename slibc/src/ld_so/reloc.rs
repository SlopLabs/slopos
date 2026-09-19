//! Applying one object's relocations.
//!
//! Binding is eager: `.rela.plt` is processed exactly like `.rela.dyn`, so
//! there is no PLT trampoline, no `GOT[1]`/`GOT[2]` handshake, and no
//! resolver running with live argument registers. Full RELRO then costs
//! nothing, because nothing writes a GOT slot after this pass.

use core::ptr;

use super::dso::{DSO_BOOTSTRAPPED, DSO_SYMBOLIC, Dso};
use super::elf::*;
use super::sym::{self, Def};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelocError {
    UndefinedSymbol,
    UnsupportedType,
    /// An initial-exec reference reached a module with no place in the static
    /// TLS block, which only a `dlopen`ed object can be.
    DynamicStaticTls,
    /// A `COPY` relocation whose definition is smaller than the reference.
    CopySizeMismatch,
}

/// Apply `DT_RELR`, `.rela.dyn` and `.rela.plt` for `table[index]`.
///
/// `scope` is the search order for undefined symbols, nearest first.
///
/// # Safety
/// Every object in `scope` must be parsed, and `table[index]`'s writable
/// segments must still be writable — RELRO is applied after this.
pub unsafe fn relocate(table: &[Dso], index: usize, scope: &[u16]) -> Result<(), RelocError> {
    let dso = table[index];
    if dso.flags & DSO_BOOTSTRAPPED == 0 {
        apply_relr(&dso);
    }
    for i in 0..dso.rela_count {
        apply_one(
            table,
            index,
            &dso,
            scope,
            ptr::read_unaligned(dso.rela.add(i)),
        )?;
    }
    for i in 0..dso.jmprel_count {
        apply_one(
            table,
            index,
            &dso,
            scope,
            ptr::read_unaligned(dso.jmprel.add(i)),
        )?;
    }
    Ok(())
}

/// `DT_RELR`: relative relocations packed as an address word followed by
/// bitmaps, each covering the next 63 slots.
unsafe fn apply_relr(dso: &Dso) {
    let mut cursor = 0usize;
    for i in 0..dso.relr_count {
        let entry = ptr::read_unaligned(dso.relr.add(i));
        if entry & 1 == 0 {
            cursor = dso.base.wrapping_add(entry);
            *(cursor as *mut usize) += dso.base;
            cursor += size_of::<usize>();
            continue;
        }
        let mut bits = entry >> 1;
        let mut slot = cursor;
        while bits != 0 {
            if bits & 1 != 0 {
                *(slot as *mut usize) += dso.base;
            }
            bits >>= 1;
            slot += size_of::<usize>();
        }
        cursor += (usize::BITS as usize - 1) * size_of::<usize>();
    }
}

unsafe fn apply_one(
    table: &[Dso],
    index: usize,
    dso: &Dso,
    scope: &[u16],
    rela: Rela,
) -> Result<(), RelocError> {
    let kind = rela.reloc_type();
    if kind == R_X86_64_NONE {
        return Ok(());
    }
    let place = dso.base.wrapping_add(rela.r_offset as usize);
    let addend = rela.r_addend as usize;

    if kind == R_X86_64_RELATIVE {
        *(place as *mut usize) = dso.base.wrapping_add(addend);
        return Ok(());
    }
    if kind == R_X86_64_IRELATIVE {
        let resolver: extern "C" fn() -> usize =
            core::mem::transmute(dso.base.wrapping_add(addend));
        *(place as *mut usize) = resolver();
        return Ok(());
    }

    let sym_index = rela.sym();
    let need_def = kind == R_X86_64_JUMP_SLOT;
    let def = if sym_index == 0 {
        None
    } else {
        resolve_for(table, index, dso, scope, sym_index, need_def, kind)?
    };

    match kind {
        R_X86_64_64 => *(place as *mut usize) = value_of(table, &def).wrapping_add(addend),
        R_X86_64_GLOB_DAT | R_X86_64_JUMP_SLOT => *(place as *mut usize) = value_of(table, &def),
        R_X86_64_32 => *(place as *mut u32) = value_of(table, &def).wrapping_add(addend) as u32,
        R_X86_64_32S => *(place as *mut i32) = value_of(table, &def).wrapping_add(addend) as i32,
        R_X86_64_PC32 => {
            *(place as *mut i32) = value_of(table, &def)
                .wrapping_add(addend)
                .wrapping_sub(place) as i32
        }
        R_X86_64_COPY => {
            let Some(def) = def else {
                return Err(RelocError::UndefinedSymbol);
            };
            let src = table[def.dso].base.wrapping_add(def.sym.st_value as usize);
            let own = own_symbol(dso, sym_index);
            if def.sym.st_size < own.st_size {
                return Err(RelocError::CopySizeMismatch);
            }
            ptr::copy_nonoverlapping(src as *const u8, place as *mut u8, own.st_size as usize);
        }
        R_X86_64_DTPMOD64 => {
            // gABI: an undefined index means this module.
            *(place as *mut usize) = match def {
                Some(def) => table[def.dso].tls_modid,
                None => dso.tls_modid,
            };
        }
        R_X86_64_DTPOFF64 => {
            *(place as *mut usize) = match def {
                Some(def) => (def.sym.st_value as usize).wrapping_add(addend),
                None => addend,
            };
        }
        R_X86_64_TPOFF64 => {
            let (modid, value) = match def {
                Some(def) => (table[def.dso].tls_modid, def.sym.st_value as usize),
                None => (dso.tls_modid, 0),
            };
            let offset = crate::thread::tls::static_offset(modid);
            if offset == 0 {
                return Err(RelocError::DynamicStaticTls);
            }
            *(place as *mut usize) = value.wrapping_add(addend).wrapping_sub(offset);
        }
        _ => return Err(RelocError::UnsupportedType),
    }
    Ok(())
}

/// A definition's absolute address: the defining object's base plus the
/// symbol's value. An unresolved reference contributes zero, which is the
/// gABI's answer and what makes a weak undefined symbol testable against
/// null.
#[inline]
fn value_of(table: &[Dso], def: &Option<Def>) -> usize {
    match def {
        Some(def) => table[def.dso].base.wrapping_add(def.sym.st_value as usize),
        None => 0,
    }
}

unsafe fn own_symbol(dso: &Dso, index: u32) -> Sym {
    ptr::read_unaligned(dso.symtab.add(index as usize))
}

unsafe fn resolve_for(
    table: &[Dso],
    index: usize,
    dso: &Dso,
    scope: &[u16],
    sym_index: u32,
    need_def: bool,
    kind: u32,
) -> Result<Option<Def>, RelocError> {
    let own = own_symbol(dso, sym_index);
    let name = dso.str_at(own.st_name);
    let found = if kind == R_X86_64_COPY {
        // This image holds the copy's destination, so the definition has to
        // come from somewhere else.
        sym::resolve(table, scope, name, need_def, Some(index))
    } else if dso.flags & DSO_SYMBOLIC != 0 {
        sym::lookup_in(dso, name, need_def)
            .map(|sym| Def { sym, dso: index })
            .or_else(|| sym::resolve(table, scope, name, need_def, None))
    } else {
        sym::resolve(table, scope, name, need_def, None)
    };
    if found.is_none() && own.bind() != STB_WEAK {
        return Err(RelocError::UndefinedSymbol);
    }
    Ok(found)
}
