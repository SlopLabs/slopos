//! Spawn file-action ABI shared between kernel and userland.
//!
//! Follows the `posix_spawn` file-actions model: the child begins with an empty
//! descriptor table and each action installs exactly what it should inherit, so
//! a spawner never mutates its own fd table around the call.

pub const EXEC_MAX_ARG_STRLEN: usize = 4096;

/// Pages of `argv` + `envp` an `execve` accepts, after Linux's
/// `MAX_ARG_PAGES`. Here rather than in the kernel because the kernel
/// enforces it and `sysconf(_SC_ARG_MAX)` reports it.
pub const EXEC_MAX_ARG_PAGES: usize = 32;

pub const EXEC_MAX_ARG_BYTES: usize = EXEC_MAX_ARG_PAGES * crate::PAGE_SIZE as usize;

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnFdActionKind {
    /// Share the parent's `src_fd` description into the child's `target_fd`.
    CloneFd = 1,
    /// Move the parent's `src_fd` into the child's `target_fd` (parent slot emptied).
    TransferFd = 2,
    /// Close the child's `target_fd`.
    Close = 3,
}

impl SpawnFdActionKind {
    #[inline]
    pub const fn from_u32(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::CloneFd),
            2 => Some(Self::TransferFd),
            3 => Some(Self::Close),
            _ => None,
        }
    }
}

/// A single spawn file action. Every kind moves or drops a descriptor the
/// spawner already holds; none mints one. Unused fields for a given `kind`
/// are ignored.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SpawnFdAction {
    /// A [`SpawnFdActionKind`] discriminant.
    pub kind: u32,
    /// Source fd in the parent — `CloneFd` / `TransferFd`.
    pub src_fd: i32,
    /// Destination fd in the child — every kind.
    pub target_fd: i32,
    pub _pad: u32,
}

/// Spawn attributes, passed by pointer in the spawn syscall's `attrs` register.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SpawnAttrs {
    /// Task priority (`TaskPriority::as_u8`).
    pub priority: u8,
    pub _pad: [u8; 3],
    /// Task flags (`TASK_FLAG_*`).
    pub flags: u16,
    pub _pad2: u16,
    /// User pointer to the [`SpawnFdAction`] array.
    pub actions_ptr: u64,
    pub actions_len: u64,
    /// Signals forced to `SIG_DFL` in the child (POSIX_SPAWN_SETSIGDEF).
    pub sigdefault_mask: u64,
    /// User pointer to a NULL-terminated `envp` array, or 0 for no environment.
    pub envp_ptr: u64,
    /// Number of entries in `envp`, excluding the terminating NULL.
    pub envp_len: u64,
    /// User pointer to the child's initial working directory, or 0 to inherit
    /// the spawner's.
    pub cwd_ptr: u64,
    pub cwd_len: u64,
    /// Gated capabilities the child holds beside the universal ones it
    /// inherits. Each must be the spawner's, or the spawn is `EPERM`.
    pub caps: u64,
    /// The child's real, effective and saved uid under [`SPAWN_SET_UID`]. One
    /// the spawner does not hold takes `SetId`.
    pub uid: u32,
    /// The same for the gid, under [`SPAWN_SET_GID`].
    pub gid: u32,
    /// User pointer to the child's supplementary groups, under
    /// [`SPAWN_SET_GROUPS`], which takes `SetId`.
    pub groups_ptr: u64,
    pub ngroups: u32,
    /// Which ids the child takes from the fields above; it keeps the
    /// spawner's others, so a zeroed field asks for nothing.
    pub set: u32,
    /// The login tag the child's process carries, under [`SPAWN_SET_LOGIN`].
    /// Fork, spawn and exec pass a process's tag on, and nothing changes it
    /// after.
    pub login: u32,
    pub _pad3: u32,
}

pub const SPAWN_SET_UID: u32 = 1 << 0;
pub const SPAWN_SET_GID: u32 = 1 << 1;
pub const SPAWN_SET_GROUPS: u32 = 1 << 2;
/// The child takes [`SpawnAttrs::login`], which takes `SetId`.
pub const SPAWN_SET_LOGIN: u32 = 1 << 3;
pub const SPAWN_SET_ALL: u32 = SPAWN_SET_UID | SPAWN_SET_GID | SPAWN_SET_GROUPS | SPAWN_SET_LOGIN;

impl SpawnAttrs {
    /// A spawn at `priority` with `flags` and nothing else: no fd actions, no
    /// environment, and the spawner's directory, ids and universal
    /// capabilities.
    pub const fn new(priority: u8, flags: u16) -> Self {
        Self {
            priority,
            _pad: [0; 3],
            flags,
            _pad2: 0,
            actions_ptr: 0,
            actions_len: 0,
            sigdefault_mask: 0,
            envp_ptr: 0,
            envp_len: 0,
            cwd_ptr: 0,
            cwd_len: 0,
            caps: 0,
            uid: 0,
            gid: 0,
            groups_ptr: 0,
            ngroups: 0,
            set: 0,
            login: 0,
            _pad3: 0,
        }
    }
}

/// Upper bound on the action-array length the kernel will read.
pub const SPAWN_MAX_FD_ACTIONS: usize = 64;

const _: () = assert!(core::mem::size_of::<SpawnFdAction>() == 16);
const _: () = assert!(core::mem::align_of::<SpawnFdAction>() == 4);
const _: () = assert!(core::mem::offset_of!(SpawnFdAction, kind) == 0);
const _: () = assert!(core::mem::offset_of!(SpawnFdAction, src_fd) == 4);
const _: () = assert!(core::mem::offset_of!(SpawnFdAction, target_fd) == 8);

const _: () = assert!(core::mem::size_of::<SpawnAttrs>() == 104);
const _: () = assert!(core::mem::align_of::<SpawnAttrs>() == 8);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, priority) == 0);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, flags) == 4);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, actions_ptr) == 8);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, actions_len) == 16);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, sigdefault_mask) == 24);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, envp_ptr) == 32);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, envp_len) == 40);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, cwd_ptr) == 48);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, cwd_len) == 56);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, caps) == 64);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, uid) == 72);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, gid) == 76);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, groups_ptr) == 80);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, ngroups) == 88);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, set) == 92);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, login) == 96);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_three_actions_decode() {
        for (raw, want) in [
            (1u32, SpawnFdActionKind::CloneFd),
            (2, SpawnFdActionKind::TransferFd),
            (3, SpawnFdActionKind::Close),
        ] {
            assert_eq!(SpawnFdActionKind::from_u32(raw), Some(want));
        }
        for raw in [0u32, 4, 5, u32::MAX] {
            assert_eq!(SpawnFdActionKind::from_u32(raw), None);
        }
    }
}
