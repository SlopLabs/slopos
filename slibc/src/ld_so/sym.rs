//! Symbol lookup: the two hash tables, and the rule for what counts as a
//! definition.

use core::ptr;

use super::dso::Dso;
use super::elf::*;

/// A resolved definition: the symbol and the object that defines it.
#[derive(Clone, Copy)]
pub struct Def {
    pub sym: Sym,
    pub dso: usize,
}

fn streq(a: *const u8, b: *const u8) -> bool {
    unsafe {
        let mut i = 0usize;
        loop {
            let ca = *a.add(i);
            if ca != *b.add(i) {
                return false;
            }
            if ca == 0 {
                return true;
            }
            i += 1;
        }
    }
}

fn gnu_hash(name: *const u8) -> u32 {
    unsafe {
        let mut h: u32 = 5381;
        let mut i = 0usize;
        loop {
            let c = *name.add(i);
            if c == 0 {
                return h;
            }
            h = h.wrapping_mul(33).wrapping_add(c as u32);
            i += 1;
        }
    }
}

fn sysv_hash(name: *const u8) -> u32 {
    unsafe {
        let mut h: u32 = 0;
        let mut i = 0usize;
        loop {
            let c = *name.add(i);
            if c == 0 {
                return h & 0x0fff_ffff;
            }
            h = h.wrapping_mul(16).wrapping_add(c as u32);
            h ^= (h >> 24) & 0xf0;
            i += 1;
        }
    }
}

/// Whether `sym` is a definition a reference may bind to.
///
/// `need_def` is set for a PLT slot, which must reach a definition rather than
/// another undefined reference.
fn usable(sym: &Sym, need_def: bool) -> bool {
    if sym.st_shndx == SHN_UNDEF && (need_def || sym.sym_type() == STT_TLS) {
        return false;
    }
    if sym.st_value == 0 && sym.sym_type() != STT_TLS {
        return false;
    }
    // `STT_GNU_IFUNC` is deliberately absent: binding to one means calling
    // its resolver and using the answer, and a reference that silently took
    // the resolver's own address instead would be a wrong call rather than a
    // failed link. Nothing in this tree exports one.
    matches!(
        sym.sym_type(),
        STT_NOTYPE | STT_OBJECT | STT_FUNC | STT_COMMON | STT_TLS
    ) && matches!(sym.bind(), STB_GLOBAL | STB_WEAK | STB_GNU_UNIQUE)
}

/// Look `name` up in one object's own tables.
///
/// # Safety
/// `dso` must be fully parsed.
pub unsafe fn lookup_in(dso: &Dso, name: *const u8, need_def: bool) -> Option<Sym> {
    if dso.symtab.is_null() || dso.strtab.is_null() {
        return None;
    }
    let index = if !dso.gnu_hash.is_null() {
        gnu_index(dso, name)?
    } else {
        // No `DT_HASH` either means no `nchain`, which is the only length
        // `.dynsym` ever states: an object with neither table has no
        // searchable symbol set, not an empty one.
        sysv_index(dso, name)?
    };
    let sym = ptr::read_unaligned(dso.symtab.add(index));
    if usable(&sym, need_def) {
        Some(sym)
    } else {
        None
    }
}

unsafe fn gnu_index(dso: &Dso, name: *const u8) -> Option<usize> {
    const BITS: u32 = usize::BITS;
    let h = dso.gnu_hash;
    let nbuckets = ptr::read_unaligned(h) as usize;
    if nbuckets == 0 {
        return None;
    }
    let symoffset = ptr::read_unaligned(h.add(1)) as usize;
    let bloom_size = ptr::read_unaligned(h.add(2)) as usize;
    if bloom_size == 0 {
        return None;
    }
    let bloom_shift = ptr::read_unaligned(h.add(3));
    let bloom = h.add(4).cast::<u64>();
    let buckets = bloom.add(bloom_size).cast::<u32>();
    let chain = buckets.add(nbuckets);

    let h1 = gnu_hash(name);
    let word = ptr::read_unaligned(bloom.add(((h1 / BITS) as usize) & (bloom_size - 1)));
    if word & (1u64 << (h1 % BITS)) == 0 {
        return None;
    }
    if word & (1u64 << ((h1 >> bloom_shift) % BITS)) == 0 {
        return None;
    }

    let mut i = ptr::read_unaligned(buckets.add((h1 as usize) % nbuckets)) as usize;
    // The chain array is indexed `i - symoffset`, so a bucket below the bias
    // — which only a malformed table holds — would read far out of bounds.
    if i == 0 || i < symoffset {
        return None;
    }
    let want = h1 | 1;
    loop {
        if i >= dso.nsyms {
            return None;
        }
        let h2 = ptr::read_unaligned(chain.add(i - symoffset));
        if want == (h2 | 1) {
            let sym = ptr::read_unaligned(dso.symtab.add(i));
            if streq(name, dso.str_at(sym.st_name)) {
                return Some(i);
            }
        }
        if h2 & 1 != 0 {
            return None;
        }
        i += 1;
    }
}

unsafe fn sysv_index(dso: &Dso, name: *const u8) -> Option<usize> {
    let h = dso.sysv_hash;
    let nbucket = ptr::read_unaligned(h) as usize;
    if nbucket == 0 {
        return None;
    }
    let nchain = ptr::read_unaligned(h.add(1)) as usize;
    let bucket = h.add(2);
    let chain = bucket.add(nbucket);

    let mut i = ptr::read_unaligned(bucket.add((sysv_hash(name) as usize) % nbucket)) as usize;
    while i != 0 && i < nchain {
        let sym = ptr::read_unaligned(dso.symtab.add(i));
        if streq(name, dso.str_at(sym.st_name)) {
            return Some(i);
        }
        i = ptr::read_unaligned(chain.add(i)) as usize;
    }
    None
}

/// Search `order` and answer the first strong definition, falling back to the
/// first weak one.
///
/// `skip` leaves one object out, which is what a `COPY` relocation needs: the
/// image carrying the relocation holds the destination, never the definition.
///
/// # Safety
/// Every index in `order` must name a parsed object in `table`.
pub unsafe fn resolve(
    table: &[Dso],
    order: &[u16],
    name: *const u8,
    need_def: bool,
    skip: Option<usize>,
) -> Option<Def> {
    let mut weak: Option<Def> = None;
    for slot in order.iter() {
        let index = *slot as usize;
        if Some(index) == skip {
            continue;
        }
        let Some(sym) = lookup_in(&table[index], name, need_def) else {
            continue;
        };
        let def = Def { sym, dso: index };
        if sym.bind() == STB_WEAK {
            if weak.is_none() {
                weak = Some(def);
            }
            continue;
        }
        return Some(def);
    }
    weak
}
