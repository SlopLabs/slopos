//! The interpreter's entry: self-relocation, then the startup link.
//!
//! [`__dls_bootstrap`] runs before this object has been relocated, so it must
//! touch no pointer that lives in memory — every such pointer is exactly what
//! it is about to fix. Reaching a `static` is fine (a non-preemptible symbol
//! in a `-Bsymbolic` object is RIP-relative and needs no relocation); reading
//! a pointer *out of* one is not.

use core::ptr;

use crate::pal::{Pal, Sys};

use super::dso::DL_MAX_OBJECTS;
use super::elf::*;
use super::{DlError, loader, lock};

// `_dlstart` hands `__dls_bootstrap` the untouched entry stack and this
// object's load base, then jumps to whatever it answers with the stack
// exactly as the kernel left it. `rdx` is zeroed because the System V entry
// contract makes it an atexit function the program may register, and this is
// not one.
//
// `__ehdr_start` is the linker's name for the ELF header, which in a shared
// object sits at vaddr 0 — so its RIP-relative address *is* the load base,
// and computing it needs no relocation.
core::arch::global_asm!(
    ".section .text._dlstart,\"ax\",@progbits",
    ".p2align 4",
    ".globl _dlstart",
    ".type _dlstart, @function",
    "_dlstart:",
    "xor rbp, rbp",
    "mov rbx, rsp",
    "mov rdi, rsp",
    "lea rsi, [rip + __ehdr_start]",
    "and rsp, -16",
    "call __dls_bootstrap",
    "mov rsp, rbx",
    "xor rdx, rdx",
    "jmp rax",
    ".size _dlstart, . - _dlstart",
    ".section .note.GNU-stack,\"\",@progbits",
);

/// Apply this object's own relative relocations, then link the program.
///
/// Answers the address to enter the program at.
///
/// # Safety
/// Called only from `_dlstart`, with the kernel's entry stack and this
/// object's load base.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __dls_bootstrap(stack: *const usize, base: usize) -> usize {
    self_relocate(base);
    match link_program(stack, base) {
        Ok(entry) => entry,
        Err(err) => fail(err),
    }
}

/// The first pass: `R_X86_64_RELATIVE` and `DT_RELR` out of our own
/// `PT_DYNAMIC`, and nothing else. Every other relocation kind needs a string
/// table pointer, and reading one is what this pass exists to make possible.
unsafe fn self_relocate(base: usize) {
    let ehdr = base as *const Ehdr;
    let phdr = (base + (*ehdr).e_phoff as usize) as *const Phdr;
    let phnum = (*ehdr).e_phnum as usize;

    let mut dynamic = ptr::null::<Dyn>();
    for i in 0..phnum {
        let ph = ptr::read_unaligned(phdr.add(i));
        if ph.p_type == PT_DYNAMIC {
            dynamic = (base + ph.p_vaddr as usize) as *const Dyn;
        }
    }
    if dynamic.is_null() {
        return;
    }

    let (mut rela, mut relasz) = (0usize, 0usize);
    let (mut relr, mut relrsz) = (0usize, 0usize);
    let mut p = dynamic;
    loop {
        let d = ptr::read_unaligned(p);
        match d.d_tag {
            DT_NULL => break,
            DT_RELA => rela = base + d.d_val as usize,
            DT_RELASZ => relasz = d.d_val as usize,
            DT_RELR => relr = base + d.d_val as usize,
            DT_RELRSZ => relrsz = d.d_val as usize,
            _ => {}
        }
        p = p.add(1);
    }

    for i in 0..relasz / size_of::<Rela>() {
        let r = ptr::read_unaligned((rela as *const Rela).add(i));
        if r.reloc_type() == R_X86_64_RELATIVE {
            *((base + r.r_offset as usize) as *mut usize) = base.wrapping_add(r.r_addend as usize);
        }
    }

    let mut cursor = 0usize;
    for i in 0..relrsz / size_of::<usize>() {
        let entry = ptr::read_unaligned((relr as *const usize).add(i));
        if entry & 1 == 0 {
            cursor = base.wrapping_add(entry);
            *(cursor as *mut usize) += base;
            cursor += size_of::<usize>();
            continue;
        }
        let mut bits = entry >> 1;
        let mut slot = cursor;
        while bits != 0 {
            if bits & 1 != 0 {
                *(slot as *mut usize) += base;
            }
            bits >>= 1;
            slot += size_of::<usize>();
        }
        cursor += (usize::BITS as usize - 1) * size_of::<usize>();
    }
}

struct EntryStack {
    phdr: usize,
    phnum: usize,
    phent: usize,
    entry: usize,
    envp: *const *const core::ffi::c_char,
}

unsafe fn read_auxv(stack: *const usize) -> EntryStack {
    let argc = *stack;
    let envp = stack.add(1 + argc + 1);
    let mut p = envp;
    while *p != 0 {
        p = p.add(1);
    }
    p = p.add(1);

    // A constructor runs before the program's own `_start` does, so what
    // `__slibc_start` would publish has to be published here or the first
    // `getenv` or `getauxval` in an `init_array` answers nothing.
    crate::auxv::capture(p);

    let mut out = EntryStack {
        phdr: 0,
        phnum: 0,
        phent: 0,
        entry: 0,
        envp: envp as *const *const core::ffi::c_char,
    };
    loop {
        let tag = *p as u64;
        let val = *p.add(1);
        if tag == slopos_abi::auxv::AT_NULL {
            break;
        }
        match tag {
            slopos_abi::auxv::AT_PHDR => out.phdr = val,
            slopos_abi::auxv::AT_PHNUM => out.phnum = val,
            slopos_abi::auxv::AT_PHENT => out.phent = val,
            slopos_abi::auxv::AT_ENTRY => out.entry = val,
            _ => {}
        }
        p = p.add(2);
    }
    out
}

/// The soname a `DT_NEEDED` on the C library resolves to. The interpreter is
/// already mapped, so the dependency is satisfied by this object rather than
/// by a second copy of it.
const LIBC_SONAME: &[u8] = b"libc.so\0";

unsafe fn link_program(stack: *const usize, base: usize) -> Result<usize, DlError> {
    let aux = read_auxv(stack);
    if aux.phdr == 0 || aux.phnum == 0 || aux.phent != size_of::<Phdr>() {
        return Err(DlError::Malformed);
    }

    let guard = lock();
    let dl = loader();

    // The executable's bias is `AT_PHDR - PT_PHDR.p_vaddr`, which is 0 for a
    // non-relocated image and the mapping address for a PIE.
    let exe_phdr = aux.phdr as *const Phdr;
    let mut exe_base = 0usize;
    for i in 0..aux.phnum {
        let ph = ptr::read_unaligned(exe_phdr.add(i));
        if ph.p_type == PT_PHDR {
            exe_base = aux.phdr.wrapping_sub(ph.p_vaddr as usize);
        }
    }

    let exe = dl
        .adopt(exe_base, exe_phdr, aux.phnum, ptr::null(), false)
        .ok_or(DlError::TooManyObjects)?;

    let self_ehdr = base as *const Ehdr;
    dl.adopt(
        base,
        (base + (*self_ehdr).e_phoff as usize) as *const Phdr,
        (*self_ehdr).e_phnum as usize,
        LIBC_SONAME.as_ptr(),
        true,
    )
    .ok_or(DlError::TooManyObjects)?;

    // Breadth-first from the executable, which puts every startup object in
    // the global scope in the order a symbol lookup must see them.
    let mut group = [0u16; DL_MAX_OBJECTS];
    group[0] = exe;
    let mut count = 1usize;
    let mut cursor = 0usize;
    while cursor < count {
        let index = group[cursor] as usize;
        cursor += 1;
        for n in 0..dl.get(index).needed_count as usize {
            let name = dl.get(index).needed_name(n);
            if name.is_null() {
                continue;
            }
            let dep = dl.load_needed(index, n, name)?;
            if !group[..count].contains(&dep) {
                group[count] = dep;
                count += 1;
            }
        }
    }
    for slot in group[..count].iter() {
        dl.add_global(*slot);
    }

    // The TLS layout must exist before relocation: a `TPOFF64` is a distance
    // below the thread pointer, and that distance is what registration
    // assigns.
    dl.assign_tls(&group[..count], true)?;
    dl.relocate_group(&group[..count])?;

    let (tls_base, tcb) = crate::thread::tls::alloc_thread_tls();
    if tcb.is_null() {
        return Err(DlError::NoMemory);
    }
    let _ = tls_base;
    (*tcb).self_ptr = tcb;
    (*tcb).tid = Sys::getpid();
    if Sys::arch_prctl_set_fs(tcb as u64).is_err() {
        return Err(DlError::NoMemory);
    }
    crate::thread::tls::adopt_thread_tls(tcb);

    crate::env::environ = aux.envp as *mut *mut u8;
    crate::stdio::streams::stdio_init();
    // Before the first constructor: one of them may throw.
    crate::unwind::init();

    drop(guard);
    // Constructors run with the lock released: one of them may `dlopen`.
    super::run_init(&group[..count]);

    Ok(aux.entry)
}

/// Report and die. The program has no interpreter, so there is nothing to
/// return to and nothing but `write(2)` to report with.
fn fail(err: DlError) -> ! {
    const PREFIX: &[u8] = b"ld-slopos: ";
    let msg = err.message();
    let _ = Sys::write(2, PREFIX.as_ptr(), PREFIX.len());
    let _ = Sys::write(2, msg.as_ptr(), msg.len() - 1);
    let _ = Sys::write(2, b"\n".as_ptr(), 1);
    Sys::exit(127)
}
