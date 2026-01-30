# Syscall Architecture Refactoring Plan

> **Status**: Planning (Refined)  
> **Created**: 2026-01-29  
> **Last Updated**: 2026-01-30  
> **Priority**: High (DRY violation, API confusion)

---

### ⚠️ SlopOS Migration Philosophy

**SlopOS does not allow:**
- Dead code (unused code must be deleted immediately)
- Deprecation warnings (`#[deprecated]` is forbidden)
- Gradual migrations (no "both APIs available" periods)

**SlopOS does allow:**
- Breaking changes (we're not deployed anywhere)
- Hard migrations (atomic cutover in a single commit)
- Build failures during development (iterate until fixed)

This plan uses **two-phase atomic migration**:
1. **Phase 1**: Build new infrastructure alongside old (additive only)
2. **Phase 2**: Single commit that switches everything and deletes old code

---

## 1. Problem Statement

### 1.1 Current Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           CURRENT SLOPOS ARCHITECTURE                        │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  abi/src/syscall.rs          ← Syscall numbers (SINGLE SOURCE OF TRUTH ✓)  │
│       │                                                                     │
│       ▼                                                                     │
│  userland/src/syscall_raw.rs ← Raw inline asm (SHARED ✓)                   │
│       │                                                                     │
│       ├────────────────────┬────────────────────────────┐                  │
│       ▼                    ▼                            ▼                  │
│  syscall.rs           libslop/syscall.rs          libslop/ffi.rs           │
│  (Rust-native API)    (C-ABI syscalls)            (extern "C" wrappers)    │
│       │                    │                            │                  │
│       │                    └────────────────────────────┘                  │
│       │                              │                                     │
│       ▼                              ▼                                     │
│  Native Apps               C Runtime (malloc, crt0)                        │
│  (shell, compositor,       (POSIX compatibility)                           │
│   roulette, sysinfo)                                                       │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 1.2 The Core Problem: Semantic Name Collision

| Module | Function | Syscall Number | Actual Operation |
|--------|----------|----------------|------------------|
| `syscall.rs` | `sys_write(buf: &[u8])` | `SYSCALL_WRITE` (2) | **TTY/Console output** |
| `syscall.rs` | `sys_read(buf: &mut [u8])` | `SYSCALL_READ` (3) | **TTY/Console input** |
| `syscall.rs` | `sys_fs_write(fd, buf, len)` | `SYSCALL_FS_WRITE` (17) | File descriptor write |
| `syscall.rs` | `sys_fs_read(fd, buf, len)` | `SYSCALL_FS_READ` (16) | File descriptor read |
| `libslop/syscall.rs` | `sys_write(fd, buf, count)` | `SYSCALL_FS_WRITE` (17) | File descriptor write |
| `libslop/syscall.rs` | `sys_read(fd, buf, count)` | `SYSCALL_FS_READ` (16) | File descriptor read |
| `syscall.rs` | `sys_exit()` | `SYSCALL_EXIT` (1) | Exit with code 0 |
| `libslop/syscall.rs` | `sys_exit(status)` | `SYSCALL_EXIT` (1) | Exit with status code |

**Critical Issue**: `sys_write` in `syscall.rs` writes to console, but `sys_write` in `libslop/syscall.rs` writes to a file descriptor. **Same name, completely different semantics.**

### 1.3 Duplication Inventory

| Concern | `syscall.rs` | `libslop/syscall.rs` | Notes |
|---------|-------------|---------------------|-------|
| File read | `sys_fs_read` (unsafe) | `sys_read` | Different signatures |
| File write | `sys_fs_write` (unsafe) | `sys_write` | Different signatures |
| File open | `sys_fs_open` (unsafe) | `sys_open` | Similar |
| File close | `sys_fs_close` | `sys_close` | Similar |
| Exit | `sys_exit()` → code 0 | `sys_exit(status)` | Different semantics! |
| Brk | Not present | `sys_brk`, `sys_sbrk` | libslop only |

### 1.4 What's Working Well

1. **Syscall numbers** are properly centralized in `abi/src/syscall.rs`
2. **Raw primitives** are shared via `syscall_raw.rs`
3. **Link sections** are correctly applied (`#[unsafe(link_section = ".user_text")]`)
4. **ShmBuffer RAII wrapper** in `syscall.rs` is excellent design
5. **libslop/ffi.rs** properly delegates to syscall wrappers

---

## 2. Research Findings: Industry Best Practices

### 2.1 Redox OS: Four-Layer Architecture

```
┌───────────────────────────────────────────────────────────────┐
│  Layer 4: RAII Wrappers (FdGuard, FileHandle)                │
│  - Automatic cleanup via Drop                                 │
│  - High-level, ergonomic API                                  │
├───────────────────────────────────────────────────────────────┤
│  Layer 3: Typed Safe Wrappers (read, write, open)            │
│  - Slice parameters instead of ptr+len                        │
│  - Documented error conditions                                │
├───────────────────────────────────────────────────────────────┤
│  Layer 2: Error Demultiplexing (Error::demux)                │
│  - Converts raw usize to Result<usize, Error>                │
│  - Single point of error interpretation                       │
├───────────────────────────────────────────────────────────────┤
│  Layer 1: Raw Syscall Primitives (syscall0..syscall6)        │
│  - Inline assembly                                            │
│  - Architecture-specific                                      │
└───────────────────────────────────────────────────────────────┘
```

**Key Pattern**: Redox's `Error::demux()` function converts raw return values to `Result<usize, Error>` at a single point, ensuring consistent error handling.

### 2.2 Hermit: Proc-Macro Generation

```rust
#[hermit_macro::system(errno)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sys_write(fd: RawFd, buf: *const u8, len: usize) -> isize {
    unsafe { write(fd, buf, len) }
}
```

**Key Pattern**: Procedural macros generate boilerplate, ensuring consistency between C ABI and internal implementation.

### 2.3 Common Principles

| Principle | Redox | Hermit | SlopOS Current |
|-----------|-------|--------|----------------|
| Single error conversion point | ✓ `demux()` | ✓ macro-generated | ✗ ad-hoc |
| Typed wrappers over raw | ✓ | ✓ | Partial |
| Clear naming convention | ✓ | ✓ | ✗ Collision |
| RAII for resources | ✓ `FdGuard` | ✓ `ObjectInterface` | ✓ `ShmBuffer` only |
| Separation of concerns | ✓ 4 layers | ✓ 2 layers | ✗ Mixed |

---

## 3. Proposed Architecture

### 3.1 New Module Structure

```
userland/
├── src/
│   ├── lib.rs
│   │
│   ├── syscall/                    # NEW: Unified syscall module
│   │   ├── mod.rs                  # Public API exports
│   │   ├── raw.rs                  # Layer 1: Raw asm (moved from syscall_raw.rs)
│   │   ├── error.rs                # Layer 2: Error demux/mux
│   │   ├── numbers.rs              # Syscall number constants (re-export from abi)
│   │   │
│   │   ├── core.rs                 # Layer 3: Core syscalls (yield, exit, sleep, time)
│   │   ├── tty.rs                  # Layer 3: TTY/Console I/O (renamed from bare read/write)
│   │   ├── fs.rs                   # Layer 3: File descriptor operations
│   │   ├── memory.rs               # Layer 3: brk, shm_*
│   │   ├── process.rs              # Layer 3: fork, exec, spawn
│   │   ├── window.rs               # Layer 3: Window/surface management
│   │   ├── input.rs                # Layer 3: Input events
│   │   │
│   │   └── wrappers/               # Layer 4: RAII wrappers
│   │       ├── mod.rs
│   │       ├── shm.rs              # ShmBuffer, ShmBufferRef, CachedShmMapping
│   │       └── fd.rs               # NEW: FdGuard for file descriptors
│   │
│   ├── libc/                       # NEW: POSIX-compatible C ABI (renamed from libslop)
│   │   ├── mod.rs
│   │   ├── crt0.rs                 # C runtime startup
│   │   ├── syscall.rs              # C-ABI syscall wrappers (thin layer over syscall/)
│   │   ├── ffi.rs                  # extern "C" exports
│   │   └── malloc.rs               # Heap allocator
│   │
│   └── ... (apps, compositor, shell, etc.)
```

### 3.2 Naming Convention

**Rule**: Function names must unambiguously describe their operation.

| Old Name | New Name | Rationale |
|----------|----------|-----------|
| `sys_write(buf)` | `tty_write(buf)` | Writes to TTY, not generic write |
| `sys_read(buf)` | `tty_read(buf)` | Reads from TTY, not generic read |
| `sys_fs_write(fd, buf, len)` | `fd_write(fd, buf)` | Generic FD write with Rust slice |
| `sys_fs_read(fd, buf, len)` | `fd_read(fd, buf)` | Generic FD read with Rust slice |
| `sys_exit()` | `exit()` or `exit_success()` | Clear intent |
| `sys_exit(status)` | `exit_with_code(code)` | Clear intent |

### 3.3 Error Handling Strategy

**New `SyscallError` type** (Layer 2):

```rust
// userland/src/syscall/error.rs

/// Syscall error with errno-compatible representation
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct SyscallError(i32);

impl SyscallError {
    pub const EPERM: Self = Self(1);
    pub const ENOENT: Self = Self(2);
    pub const EIO: Self = Self(5);
    pub const EBADF: Self = Self(9);
    pub const ENOMEM: Self = Self(12);
    pub const EACCES: Self = Self(13);
    pub const EFAULT: Self = Self(14);
    pub const EINVAL: Self = Self(22);
    pub const ENOSYS: Self = Self(38);
    // ... more as needed
    
    /// Get the raw errno value
    #[inline]
    pub const fn errno(self) -> i32 {
        self.0
    }
}

pub type SyscallResult<T> = Result<T, SyscallError>;

/// Convert raw syscall return to Result (SINGLE CONVERSION POINT)
#[inline]
pub fn demux(value: u64) -> SyscallResult<u64> {
    let signed = value as i64;
    if signed >= -4095 && signed < 0 {
        Err(SyscallError((-signed) as i32))
    } else {
        Ok(value)
    }
}

/// Convert Result to raw syscall return (for kernel use)
#[inline]
pub fn mux(result: SyscallResult<u64>) -> u64 {
    match result {
        Ok(v) => v,
        Err(e) => (-e.0 as i64) as u64,
    }
}
```

### 3.4 Layer 3: Typed Wrappers Example

```rust
// userland/src/syscall/fs.rs

use super::error::{SyscallError, SyscallResult, demux};
use super::raw::{syscall1, syscall3};
use slopos_abi::syscall::*;

/// Open a file, returning a file descriptor
/// 
/// # Arguments
/// * `path` - Null-terminated path string
/// * `flags` - Open flags (O_RDONLY, O_WRONLY, O_RDWR, O_CREAT, etc.)
/// 
/// # Errors
/// * `ENOENT` - File not found
/// * `EACCES` - Permission denied
/// * `EINVAL` - Invalid flags
#[inline]
#[unsafe(link_section = ".user_text")]
pub fn open(path: &core::ffi::CStr, flags: u32) -> SyscallResult<RawFd> {
    let result = unsafe { syscall2(SYSCALL_FS_OPEN, path.as_ptr() as u64, flags as u64) };
    demux(result).map(|v| v as RawFd)
}

/// Read from a file descriptor into a buffer
/// 
/// # Arguments
/// * `fd` - File descriptor
/// * `buf` - Buffer to read into
/// 
/// # Returns
/// Number of bytes read, or 0 on EOF
/// 
/// # Errors
/// * `EBADF` - Invalid file descriptor
/// * `EFAULT` - Invalid buffer pointer
/// * `EIO` - I/O error
#[inline]
#[unsafe(link_section = ".user_text")]
pub fn read(fd: RawFd, buf: &mut [u8]) -> SyscallResult<usize> {
    let result = unsafe {
        syscall3(SYSCALL_FS_READ, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64)
    };
    demux(result).map(|v| v as usize)
}

/// Write to a file descriptor from a buffer
/// 
/// # Arguments
/// * `fd` - File descriptor
/// * `buf` - Buffer to write from
/// 
/// # Returns
/// Number of bytes written
/// 
/// # Errors
/// * `EBADF` - Invalid file descriptor
/// * `EFAULT` - Invalid buffer pointer
/// * `EIO` - I/O error
/// * `ENOSPC` - No space left on device
#[inline]
#[unsafe(link_section = ".user_text")]
pub fn write(fd: RawFd, buf: &[u8]) -> SyscallResult<usize> {
    let result = unsafe {
        syscall3(SYSCALL_FS_WRITE, fd as u64, buf.as_ptr() as u64, buf.len() as u64)
    };
    demux(result).map(|v| v as usize)
}

/// Close a file descriptor
#[inline]
#[unsafe(link_section = ".user_text")]
pub fn close(fd: RawFd) -> SyscallResult<()> {
    let result = unsafe { syscall1(SYSCALL_FS_CLOSE, fd as u64) };
    demux(result).map(|_| ())
}
```

### 3.5 Layer 4: RAII Wrapper Example

```rust
// userland/src/syscall/wrappers/fd.rs

use super::super::fs;
use super::super::error::SyscallResult;

pub type RawFd = i32;

/// RAII wrapper for a file descriptor
/// 
/// Automatically closes the file descriptor when dropped.
/// 
/// # Example
/// ```
/// let file = FdGuard::open(c"/etc/passwd", O_RDONLY)?;
/// let mut buf = [0u8; 1024];
/// let n = file.read(&mut buf)?;
/// // file automatically closed when `file` goes out of scope
/// ```
pub struct FdGuard {
    fd: RawFd,
}

impl FdGuard {
    /// Open a file and wrap the descriptor
    #[inline]
    pub fn open(path: &core::ffi::CStr, flags: u32) -> SyscallResult<Self> {
        fs::open(path, flags).map(|fd| Self { fd })
    }
    
    /// Wrap an existing file descriptor
    /// 
    /// # Safety
    /// The caller must ensure the fd is valid and not owned elsewhere.
    #[inline]
    pub const unsafe fn from_raw(fd: RawFd) -> Self {
        Self { fd }
    }
    
    /// Get the raw file descriptor (for syscalls that need it)
    #[inline]
    pub const fn as_raw(&self) -> RawFd {
        self.fd
    }
    
    /// Read from the file
    #[inline]
    pub fn read(&self, buf: &mut [u8]) -> SyscallResult<usize> {
        fs::read(self.fd, buf)
    }
    
    /// Write to the file
    #[inline]
    pub fn write(&self, buf: &[u8]) -> SyscallResult<usize> {
        fs::write(self.fd, buf)
    }
    
    /// Consume the guard and return the raw fd without closing
    #[inline]
    pub fn into_raw(self) -> RawFd {
        let fd = self.fd;
        core::mem::forget(self);
        fd
    }
}

impl Drop for FdGuard {
    #[inline]
    fn drop(&mut self) {
        let _ = fs::close(self.fd);
    }
}
```

### 3.6 C-ABI Layer (libc compatibility)

```rust
// userland/src/libc/syscall.rs
//
// Thin wrappers providing C-compatible interface over the typed syscall module.
// These are used by ffi.rs for extern "C" exports.

use core::ffi::{c_char, c_int, c_void};
use crate::syscall::{fs, process, memory};

/// POSIX-style read (for C compatibility)
#[inline]
pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize {
    if buf.is_null() {
        return -14; // EFAULT
    }
    let slice = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, count) };
    match fs::read(fd, slice) {
        Ok(n) => n as isize,
        Err(e) => -(e.errno() as isize),
    }
}

/// POSIX-style write (for C compatibility)
#[inline]
pub fn write(fd: c_int, buf: *const c_void, count: usize) -> isize {
    if buf.is_null() {
        return -14; // EFAULT
    }
    let slice = unsafe { core::slice::from_raw_parts(buf as *const u8, count) };
    match fs::write(fd, slice) {
        Ok(n) => n as isize,
        Err(e) => -(e.errno() as isize),
    }
}

/// POSIX-style exit
#[inline]
pub fn exit(status: c_int) -> ! {
    process::exit_with_code(status);
}
```

---

## 4. Migration Strategy

> **SlopOS Philosophy**: No dead code. No deprecation. Break and fix immediately.
> Breaking changes are trivial—SlopOS isn't deployed anywhere. Hard migrations only.

### 4.1 Phase 1: Build New Infrastructure (Additive Only)

**Goal**: Create the complete new syscall layer alongside old code. Nothing removed yet, nothing broken.

**Why separate phase**: Allows testing new APIs in isolation before the cutover. New code can compile and be verified without affecting existing functionality.

1. Create `userland/src/syscall/` directory structure:
   ```
   syscall/
   ├── mod.rs          # Public exports only
   ├── raw.rs          # Copy from syscall_raw.rs (not move yet)
   ├── error.rs        # SyscallError, demux(), SyscallResult
   ├── numbers.rs      # Re-exports from abi::syscall
   ├── core.rs         # yield, exit, exit_with_code, sleep, time, random
   ├── tty.rs          # tty_write, tty_read, tty_read_char (console I/O)
   ├── fs.rs           # open, close, read, write, stat, list, mkdir, unlink
   ├── memory.rs       # brk, sbrk, shm_* functions
   ├── process.rs      # spawn, exec, fork, halt
   ├── window.rs       # fb_*, surface_*, enumerate_windows, set_window_*
   ├── input.rs        # input_poll, input_get_*, drain_queue
   ├── roulette.rs     # Wheel of Fate syscalls
   └── wrappers/
       ├── mod.rs
       ├── shm.rs      # ShmBuffer, ShmBufferRef (copy from syscall.rs)
       └── fd.rs       # FdGuard (new RAII wrapper)
   ```

2. Implement all new modules with proper `demux()` error handling
3. Create `userland/src/libc/` directory structure (parallel to libslop):
   ```
   libc/
   ├── mod.rs
   ├── syscall.rs      # C-ABI wrappers using new syscall/
   ├── ffi.rs          # extern "C" exports
   ├── malloc.rs       # Heap allocator
   └── crt0.rs         # C runtime startup
   ```

4. Verify new code compiles (can use `#[cfg(feature = "new_syscall")]` temporarily if needed)

**Exit Criteria**:
- All new modules compile
- New APIs have doc comments
- No changes to existing functionality
- Old `syscall.rs`, `syscall_raw.rs`, `libslop/` untouched

### 4.2 Phase 2: Atomic Cutover

**Goal**: Single commit that switches everything. All or nothing.

**The Big Bang** (one commit, coordinated changes):

1. **Update all consumers** (10 files total):
   
   **Apps** (6 files):
   - `shell.rs` → new `syscall::{tty, fs, core, window, input, process}` APIs
   - `compositor.rs` → new `syscall::{tty, window, input, core, memory}` APIs
   - `roulette.rs` → new `syscall::{tty, core, roulette, window}` APIs
   - `apps/file_manager.rs` → new `syscall::{window, fs, input, core}` APIs
   - `apps/sysinfo.rs` → new `syscall::{window, core, tty}` APIs
   
   **C Runtime** (4 files):
   - `libc/malloc.rs` → `syscall::memory::brk`
   - `libc/ffi.rs` → `libc::syscall::*`
   - `libc/crt0.rs` → `syscall::core::exit_with_code`
   - `libc/mod.rs` → re-export from new `libc::syscall`

2. **Delete old code**:
   - Delete `userland/src/syscall.rs` (old monolithic module)
   - Delete `userland/src/syscall_raw.rs` (replaced by `syscall/raw.rs`)
   - Delete entire `userland/src/libslop/` directory

3. **Update lib.rs**:
   - Replace `mod syscall;` with `pub mod syscall;` (new module)
   - Replace `mod libslop;` with `pub mod libc;`
   - Remove `mod syscall_raw;`

4. **Verify build passes**: `make build && make test`

**Exit Criteria**:
- `make build` succeeds
- `make test` passes (kernel boots, harness runs)
- No references to old `syscall.rs`, `syscall_raw.rs`, or `libslop/`
- Zero dead code

### 4.3 Verification & Polish

**Goal**: Ensure nothing regressed after the atomic cutover.

1. **Manual smoke test** (`make boot`):
   - Shell launches and accepts commands
   - `cat`, `echo`, `ls`, `mkdir`, `rm` work
   - Compositor renders windows
   - Roulette spins the Wheel of Fate
   - File manager and sysinfo apps display correctly

2. **Verify link sections**:
   ```bash
   objdump -h builddir/kernel.elf | grep user_text
   ```
   Ensure all userland syscall functions are in `.user_text`

3. **Code review**: Ensure no subtle semantic changes
   - TTY I/O uses correct syscall numbers (SYSCALL_WRITE vs SYSCALL_FS_WRITE)
   - Error handling is consistent via `demux()`
   - RAII wrappers don't leak resources

### 4.4 Risk Mitigation

| Risk | Mitigation |
|------|------------|
| Phase 2 commit doesn't compile | Phase 1 ensures new code works; Phase 2 is purely mechanical replacement |
| Missed a usage | Compiler will catch it—old modules deleted, imports will fail |
| Semantic regression | Phase 3 smoke tests catch runtime issues |
| Link section mistakes | objdump verification in Phase 3 |
| Performance issues | Inline attributes preserved; no new abstraction overhead |

**Recovery Plan**: If Phase 2 breaks badly, `git reset --hard HEAD~1` and iterate on Phase 1.

### 4.5 Migration Mapping (Quick Reference)

| Old Location | Old Function | New Location | New Function |
|--------------|--------------|--------------|--------------|
| `syscall.rs` | `sys_write(buf)` | `syscall::tty` | `write(buf)` |
| `syscall.rs` | `sys_read(buf)` | `syscall::tty` | `read(buf)` |
| `syscall.rs` | `sys_read_char()` | `syscall::tty` | `read_char()` |
| `syscall.rs` | `sys_exit()` | `syscall::core` | `exit()` |
| `syscall.rs` | `sys_fs_open(...)` | `syscall::fs` | `open(...)` |
| `syscall.rs` | `sys_fs_close(fd)` | `syscall::fs` | `close(fd)` |
| `syscall.rs` | `sys_fs_read(...)` | `syscall::fs` | `read(fd, buf)` |
| `syscall.rs` | `sys_fs_write(...)` | `syscall::fs` | `write(fd, buf)` |
| `syscall.rs` | `sys_fb_info()` | `syscall::window` | `fb_info()` |
| `syscall.rs` | `sys_surface_*` | `syscall::window` | `surface_*` |
| `syscall.rs` | `sys_input_*` | `syscall::input` | `poll_*`, `get_*` |
| `syscall.rs` | `sys_spawn_task(...)` | `syscall::process` | `spawn(...)` |
| `syscall.rs` | `sys_shm_*` | `syscall::memory` | `shm_*` |
| `syscall.rs` | `ShmBuffer` | `syscall::wrappers::shm` | `ShmBuffer` |
| `libslop/syscall.rs` | `sys_read(fd,...)` | `libc::syscall` | `read(fd,...)` |
| `libslop/syscall.rs` | `sys_write(fd,...)` | `libc::syscall` | `write(fd,...)` |
| `libslop/syscall.rs` | `sys_exit(status)` | `syscall::core` | `exit_with_code(status)` |
| `libslop/syscall.rs` | `sys_brk(addr)` | `syscall::memory` | `brk(addr)` |
| `libslop/syscall.rs` | `sys_sbrk(inc)` | `syscall::memory` | `sbrk(inc)` |

---

## 5. Summary: Key Decisions

### 5.1 Naming Resolution

| Collision | Resolution |
|-----------|------------|
| `sys_write` (console vs fd) | `tty::write()` vs `fs::write()` |
| `sys_read` (console vs fd) | `tty::read()` vs `fs::read()` |
| `sys_exit` (void vs status) | `process::exit()` vs `process::exit_with_code()` |

### 5.2 Architecture Principles

1. **Single error conversion point**: All syscalls use `demux()` in `error.rs`
2. **Typed wrappers**: Rust slices, not raw pointers
3. **RAII for resources**: `FdGuard`, `ShmBuffer`
4. **Clear module boundaries**: `tty`, `fs`, `process`, `memory`, `window`, `input`
5. **C-ABI as thin layer**: `libc/` wraps `syscall/`, not the other way around

### 5.3 What NOT to Do

1. **Don't add a proc-macro system** - complexity not worth it for SlopOS scale
2. **Don't encode metadata in syscall numbers** - current simple scheme is fine
3. **Don't remove C-ABI support** - needed for POSIX compatibility
4. **Don't use `#[deprecated]`** - SlopOS uses hard migrations, not gradual deprecation
5. **Don't leave dead code** - if old code has zero usages, delete it immediately
6. **Don't do gradual migration** - one atomic commit switches everything

---

## 6. Implementation Checklist

### Phase 1: Build New Infrastructure (Additive Only)

**New syscall/ module:**
- [ ] Create `userland/src/syscall/mod.rs` (public exports)
- [ ] Create `userland/src/syscall/raw.rs` (copy from syscall_raw.rs)
- [ ] Create `userland/src/syscall/error.rs` (SyscallError, demux, SyscallResult)
- [ ] Create `userland/src/syscall/numbers.rs` (re-export abi::syscall)
- [ ] Create `userland/src/syscall/core.rs` (yield, exit, exit_with_code, sleep, time)
- [ ] Create `userland/src/syscall/tty.rs` (write, read, read_char)
- [ ] Create `userland/src/syscall/fs.rs` (open, close, read, write, stat, list, mkdir, unlink)
- [ ] Create `userland/src/syscall/memory.rs` (brk, sbrk, shm_*)
- [ ] Create `userland/src/syscall/process.rs` (spawn, exec, fork, halt)
- [ ] Create `userland/src/syscall/window.rs` (fb_*, surface_*, enumerate_*, set_window_*)
- [ ] Create `userland/src/syscall/input.rs` (poll_*, get_*, drain_queue)
- [ ] Create `userland/src/syscall/roulette.rs` (Wheel of Fate syscalls)
- [ ] Create `userland/src/syscall/wrappers/mod.rs`
- [ ] Create `userland/src/syscall/wrappers/shm.rs` (ShmBuffer, ShmBufferRef, CachedShmMapping)
- [ ] Create `userland/src/syscall/wrappers/fd.rs` (FdGuard)

**New libc/ module:**
- [ ] Create `userland/src/libc/mod.rs`
- [ ] Create `userland/src/libc/syscall.rs` (C-ABI wrappers)
- [ ] Create `userland/src/libc/ffi.rs` (extern "C" exports)
- [ ] Create `userland/src/libc/malloc.rs` (heap allocator)
- [ ] Create `userland/src/libc/crt0.rs` (C runtime startup)

**Verification:**
- [ ] All new modules compile (may use temporary cfg flag)
- [ ] Doc comments on public APIs

### Phase 2: Atomic Cutover (Single Commit)

**Update consumers:**
- [ ] Update `shell.rs` to use new APIs
- [ ] Update `compositor.rs` to use new APIs
- [ ] Update `roulette.rs` to use new APIs
- [ ] Update `apps/file_manager.rs` to use new APIs
- [ ] Update `apps/sysinfo.rs` to use new APIs
- [ ] Update `lib.rs` module declarations

**Delete old code:**
- [ ] Delete `userland/src/syscall.rs`
- [ ] Delete `userland/src/syscall_raw.rs`
- [ ] Delete `userland/src/libslop/` directory

**Final verification:**
- [ ] `make build` succeeds
- [ ] `make test` passes

### Phase 3: Verification & Polish

- [ ] Manual smoke test with `make boot`
- [ ] Verify `.user_text` link sections with objdump
- [ ] Ensure no semantic regressions
- [ ] Update AGENTS.md if any new patterns established
