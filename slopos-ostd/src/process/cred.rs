//! A process's Unix identity: the ids its permission bits and signals are
//! judged by.
//!
//! Identity carries no power. Changing to an id the process does not already
//! hold takes the `SetId` capability, and bypassing a mode bit takes
//! `DacOverride`; uid 0 is only the owner of the system's files.

use slopos_abi::Errno;

/// Supplementary groups a process may carry.
pub const NGROUPS_MAX: usize = 32;

/// `(uid_t)-1`: the set*id calls leave an id given as this alone, and no
/// process may hold it.
pub const ID_UNCHANGED: u32 = u32::MAX;

pub const ROOT_UID: u32 = 0;

/// Real, effective and saved user and group ids, and the supplementary
/// groups. Permission checks use the effective ids; Linux's separate
/// filesystem ids always equal them here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cred {
    pub ruid: u32,
    pub euid: u32,
    pub suid: u32,
    pub rgid: u32,
    pub egid: u32,
    pub sgid: u32,
    ngroups: u8,
    groups: [u32; NGROUPS_MAX],
}

impl Cred {
    pub const ROOT: Self = Self::of(ROOT_UID, ROOT_UID);

    /// The identity of a user task whose process is gone: one no file or task
    /// holds.
    pub const NOBODY: Self = Self::of(ID_UNCHANGED, ID_UNCHANGED);

    /// Every id `uid` or `gid`, and no supplementary group.
    pub const fn of(uid: u32, gid: u32) -> Self {
        Self {
            ruid: uid,
            euid: uid,
            suid: uid,
            rgid: gid,
            egid: gid,
            sgid: gid,
            ngroups: 0,
            groups: [0; NGROUPS_MAX],
        }
    }

    pub fn groups(&self) -> &[u32] {
        &self.groups[..usize::from(self.ngroups)]
    }

    /// The effective group or a supplementary one.
    pub fn in_group(&self, gid: u32) -> bool {
        self.egid == gid || self.groups().contains(&gid)
    }

    /// This identity with its real ids acting as the effective ones, which is
    /// what `access(2)` checks by default.
    pub fn as_real(&self) -> Self {
        Self {
            euid: self.ruid,
            egid: self.rgid,
            ..*self
        }
    }

    /// What an exec leaves: the effective ids saved, as POSIX has it, so the
    /// new image cannot return to an id the old one set aside.
    pub fn saved_at_exec(&self) -> Self {
        Self {
            suid: self.euid,
            sgid: self.egid,
            ..*self
        }
    }

    /// `setuid(2)`: a holder of `SetId` sets all three ids, anyone else only
    /// the effective one, to its real or saved id.
    pub fn set_uid(&self, uid: u32, set_id: bool) -> Result<Self, Errno> {
        let ids = set_one([self.ruid, self.euid, self.suid], uid, set_id)?;
        Ok(self.with_uids(ids))
    }

    pub fn set_gid(&self, gid: u32, set_id: bool) -> Result<Self, Errno> {
        let ids = set_one([self.rgid, self.egid, self.sgid], gid, set_id)?;
        Ok(self.with_gids(ids))
    }

    /// `setreuid(2)`.
    pub fn set_reuid(&self, ruid: u32, euid: u32, set_id: bool) -> Result<Self, Errno> {
        let ids = set_re([self.ruid, self.euid, self.suid], ruid, euid, set_id)?;
        Ok(self.with_uids(ids))
    }

    pub fn set_regid(&self, rgid: u32, egid: u32, set_id: bool) -> Result<Self, Errno> {
        let ids = set_re([self.rgid, self.egid, self.sgid], rgid, egid, set_id)?;
        Ok(self.with_gids(ids))
    }

    /// `setresuid(2)`.
    pub fn set_resuid(&self, want: [u32; 3], set_id: bool) -> Result<Self, Errno> {
        let ids = set_res([self.ruid, self.euid, self.suid], want, set_id)?;
        Ok(self.with_uids(ids))
    }

    pub fn set_resgid(&self, want: [u32; 3], set_id: bool) -> Result<Self, Errno> {
        let ids = set_res([self.rgid, self.egid, self.sgid], want, set_id)?;
        Ok(self.with_gids(ids))
    }

    /// `setgroups(2)`, which only a holder of `SetId` may call.
    pub fn set_groups(&self, groups: &[u32], set_id: bool) -> Result<Self, Errno> {
        if !set_id {
            return Err(Errno::EPERM);
        }
        if groups.len() > NGROUPS_MAX || groups.contains(&ID_UNCHANGED) {
            return Err(Errno::EINVAL);
        }
        let mut next = *self;
        next.groups = [0; NGROUPS_MAX];
        next.groups[..groups.len()].copy_from_slice(groups);
        next.ngroups = groups.len() as u8;
        Ok(next)
    }

    fn with_uids(&self, [ruid, euid, suid]: [u32; 3]) -> Self {
        Self {
            ruid,
            euid,
            suid,
            ..*self
        }
    }

    fn with_gids(&self, [rgid, egid, sgid]: [u32; 3]) -> Self {
        Self {
            rgid,
            egid,
            sgid,
            ..*self
        }
    }
}

fn set_one(ids: [u32; 3], id: u32, set_id: bool) -> Result<[u32; 3], Errno> {
    if id == ID_UNCHANGED {
        return Err(Errno::EINVAL);
    }
    if set_id {
        return Ok([id; 3]);
    }
    if id != ids[0] && id != ids[2] {
        return Err(Errno::EPERM);
    }
    Ok([ids[0], id, ids[2]])
}

/// The saved id follows the effective one whenever the real id is set, or the
/// effective id moves off the old real one, so a process cannot keep an id it
/// meant to give up.
fn set_re(ids: [u32; 3], real: u32, effective: u32, set_id: bool) -> Result<[u32; 3], Errno> {
    let [old_r, old_e, old_s] = ids;
    let r = if real == ID_UNCHANGED { old_r } else { real };
    let e = if effective == ID_UNCHANGED {
        old_e
    } else {
        effective
    };
    if !set_id {
        let real_ok = real == ID_UNCHANGED || real == old_r || real == old_e;
        let effective_ok = effective == ID_UNCHANGED || [old_r, old_e, old_s].contains(&effective);
        if !real_ok || !effective_ok {
            return Err(Errno::EPERM);
        }
    }
    let s = if real != ID_UNCHANGED || (effective != ID_UNCHANGED && effective != old_r) {
        e
    } else {
        old_s
    };
    Ok([r, e, s])
}

fn set_res(ids: [u32; 3], want: [u32; 3], set_id: bool) -> Result<[u32; 3], Errno> {
    let mut next = ids;
    for (slot, id) in next.iter_mut().zip(want) {
        if id == ID_UNCHANGED {
            continue;
        }
        if !set_id && !ids.contains(&id) {
            return Err(Errno::EPERM);
        }
        *slot = id;
    }
    Ok(next)
}

/// Who a permission check is for.
#[derive(Clone, Copy, Debug)]
pub struct Subject {
    pub cred: Cred,
    pub dac_override: bool,
}

impl Subject {
    /// The kernel acting for itself, which no mode bit refuses.
    pub const KERNEL: Self = Self {
        cred: Cred::ROOT,
        dac_override: true,
    };

    pub fn permits(&self, mode: u16, uid: u32, gid: u32, is_dir: bool, want: Perm) -> bool {
        permits(&self.cred, self.dac_override, mode, uid, gid, is_dir, want)
    }

    /// Owns the inode for `chmod`, `utimensat` and the sticky bit.
    pub fn owns(&self, uid: u32) -> bool {
        self.dac_override || self.cred.euid == uid
    }

    pub fn sticky_permits(&self, dir_mode: u16, dir_uid: u32, owner: u32) -> bool {
        sticky_permits(&self.cred, self.dac_override, dir_mode, dir_uid, owner)
    }

    /// `chown`'s rule: a holder of `DacOverride` gives anything to anyone; the
    /// owner keeps the file and may move it to a group it is in. `None` keeps
    /// that id.
    pub fn may_chown(&self, owner: u32, group: u32, uid: Option<u32>, gid: Option<u32>) -> bool {
        if self.dac_override {
            return true;
        }
        let is_owner = self.cred.euid == owner;
        uid.is_none_or(|uid| is_owner && uid == owner)
            && gid.is_none_or(|gid| is_owner && (gid == group || self.cred.in_group(gid)))
    }

    /// Who an inode created in a directory of `dir_mode` and `dir_gid` belongs
    /// to: its creator, in the directory's group when that is set-group-ID.
    pub fn child_owner(&self, dir_mode: u16, dir_gid: u32) -> (u32, u32) {
        let gid = if dir_mode & S_ISGID != 0 {
            dir_gid
        } else {
            self.cred.egid
        };
        (self.cred.euid, gid)
    }

    /// The mode a child asked for as `mode` takes there. A directory takes no
    /// set-ID bit but its parent's set-group-ID; anything else loses a
    /// set-group-ID that means group execution in a group its creator is not
    /// in.
    pub fn child_mode(&self, dir_mode: u16, dir_gid: u32, mode: u16, is_dir: bool) -> u16 {
        if is_dir {
            return mode & !(S_ISUID | S_ISGID) | dir_mode & S_ISGID;
        }
        let (_, gid) = self.child_owner(dir_mode, dir_gid);
        if mode & S_IXGRP != 0 && !self.in_group_or_override(gid) {
            mode & !S_ISGID
        } else {
            mode
        }
    }

    /// The mode `chmod` stores: set-group-ID survives only for a caller in the
    /// file's group.
    pub fn chmod_mode(&self, mode: u16, gid: u32) -> u16 {
        if self.in_group_or_override(gid) {
            mode
        } else {
            mode & !S_ISGID
        }
    }

    /// A non-directory's mode once its owner changes: no set-user-ID, and no
    /// set-group-ID where it means group execution or the caller is outside
    /// the file's group `gid`.
    pub fn mode_after_chown(&self, mode: u16, gid: u32, is_dir: bool) -> u16 {
        if is_dir {
            return mode;
        }
        let mode = mode & !S_ISUID;
        if mode & S_IXGRP != 0 || !self.in_group_or_override(gid) {
            mode & !S_ISGID
        } else {
            mode
        }
    }

    fn in_group_or_override(&self, gid: u32) -> bool {
        self.dac_override || self.cred.in_group(gid)
    }
}

const S_ISUID: u16 = slopos_abi::fs::S_ISUID as u16;
const S_ISGID: u16 = slopos_abi::fs::S_ISGID as u16;
const S_IXGRP: u16 = 0o010;

/// A mode bit a caller asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u16)]
pub enum Perm {
    Read = 4,
    Write = 2,
    Exec = 1,
}

/// Whether `cred` may `want` on an inode of `mode`, owned `uid:gid`.
///
/// One class answers: the owner's bits for the owner, the group's for a
/// member, the others' for the rest, even where a later class would grant
/// more. `dac_override` grants reading and writing, and executing anything
/// some class may execute or that is a directory.
pub fn permits(
    cred: &Cred,
    dac_override: bool,
    mode: u16,
    uid: u32,
    gid: u32,
    is_dir: bool,
    want: Perm,
) -> bool {
    let bit = want as u16;
    let class = if cred.euid == uid {
        mode >> 6
    } else if cred.in_group(gid) {
        mode >> 3
    } else {
        mode
    };
    if class & bit != 0 {
        return true;
    }
    dac_override && (want != Perm::Exec || is_dir || mode & 0o111 != 0)
}

/// The sticky bit's rule for removing or replacing a name: in a sticky
/// directory only the entry's owner or the directory's may.
pub fn sticky_permits(
    cred: &Cred,
    dac_override: bool,
    dir_mode: u16,
    dir_uid: u32,
    owner: u32,
) -> bool {
    dir_mode & slopos_abi::fs::S_ISVTX as u16 == 0
        || dac_override
        || cred.euid == owner
        || cred.euid == dir_uid
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: u32 = 1000;
    const OTHER: u32 = 2000;

    fn cred(r: u32, e: u32, s: u32) -> Cred {
        Cred {
            ruid: r,
            euid: e,
            suid: s,
            ..Cred::of(r, r)
        }
    }

    fn uids(c: &Cred) -> [u32; 3] {
        [c.ruid, c.euid, c.suid]
    }

    #[test]
    fn setuid_without_set_id_moves_only_the_effective_id_among_its_own() {
        let c = cred(U, OTHER, OTHER);
        assert_eq!(uids(&c.set_uid(U, false).unwrap()), [U, U, OTHER]);
        assert_eq!(uids(&c.set_uid(OTHER, false).unwrap()), [U, OTHER, OTHER]);
        assert_eq!(c.set_uid(0, false), Err(Errno::EPERM));
        assert_eq!(uids(&c.set_uid(0, true).unwrap()), [0, 0, 0]);
    }

    #[test]
    fn uid_zero_without_set_id_cannot_become_anyone() {
        assert_eq!(Cred::ROOT.set_uid(U, false), Err(Errno::EPERM));
        assert_eq!(Cred::ROOT.set_resuid([U; 3], false), Err(Errno::EPERM));
        assert_eq!(Cred::ROOT.set_groups(&[U], false), Err(Errno::EPERM));
        assert!(Cred::ROOT.set_groups(&[U], true).is_ok());
    }

    #[test]
    fn the_unchanged_id_is_never_held() {
        assert_eq!(Cred::ROOT.set_uid(ID_UNCHANGED, true), Err(Errno::EINVAL));
        assert_eq!(
            Cred::ROOT.set_groups(&[ID_UNCHANGED], true),
            Err(Errno::EINVAL)
        );
        let kept = Cred::ROOT.set_resuid([ID_UNCHANGED; 3], false).unwrap();
        assert_eq!(kept, Cred::ROOT);
    }

    #[test]
    fn setreuid_moves_the_saved_id_with_a_real_or_new_effective_id() {
        let c = cred(U, OTHER, OTHER);
        let swapped = c.set_reuid(OTHER, U, false).unwrap();
        assert_eq!(uids(&swapped), [OTHER, U, U]);
        let back = c.set_reuid(ID_UNCHANGED, U, false).unwrap();
        assert_eq!(
            uids(&back),
            [U, U, OTHER],
            "effective back to real keeps saved"
        );
        assert_eq!(c.set_reuid(0, ID_UNCHANGED, false), Err(Errno::EPERM));
        let saved = cred(U, U, OTHER);
        assert_eq!(
            saved.set_reuid(OTHER, ID_UNCHANGED, false),
            Err(Errno::EPERM)
        );
        assert_eq!(
            uids(&saved.set_reuid(ID_UNCHANGED, OTHER, false).unwrap()),
            [U, OTHER, OTHER]
        );
    }

    #[test]
    fn setresuid_takes_any_of_the_three_old_ids() {
        let c = cred(U, OTHER, 0);
        let rotated = c.set_resuid([0, U, OTHER], false).unwrap();
        assert_eq!(uids(&rotated), [0, U, OTHER]);
        assert_eq!(c.set_resuid([3, U, U], false), Err(Errno::EPERM));
        assert_eq!(uids(&c.set_resuid([3, 4, 5], true).unwrap()), [3, 4, 5]);
    }

    #[test]
    fn gid_calls_leave_the_uids_alone() {
        let c = Cred::of(U, U);
        let moved = c.set_resgid([OTHER; 3], true).unwrap();
        assert_eq!(uids(&moved), [U; 3]);
        assert_eq!([moved.rgid, moved.egid, moved.sgid], [OTHER; 3]);
        assert!(!moved.in_group(U));
        assert!(moved.in_group(OTHER));
    }

    #[test]
    fn too_many_groups_is_einval() {
        let many = [7u32; NGROUPS_MAX + 1];
        assert_eq!(Cred::ROOT.set_groups(&many, true), Err(Errno::EINVAL));
        let max = Cred::ROOT.set_groups(&many[..NGROUPS_MAX], true).unwrap();
        assert_eq!(max.groups().len(), NGROUPS_MAX);
        assert!(max.in_group(7));
    }

    #[test]
    fn as_real_checks_with_the_real_ids() {
        let c = cred(U, 0, 0);
        assert_eq!(c.as_real().euid, U);
        assert!(!permits(
            &c.as_real(),
            false,
            0o600,
            0,
            0,
            false,
            Perm::Read
        ));
        assert!(permits(&c, false, 0o600, 0, 0, false, Perm::Read));
    }

    #[test]
    fn one_class_answers_even_when_a_later_one_grants_more() {
        let c = Cred::of(U, U);
        assert!(!permits(&c, false, 0o077, U, U, false, Perm::Read));
        assert!(!permits(&c, false, 0o707, OTHER, U, false, Perm::Read));
        assert!(permits(&c, false, 0o007, OTHER, OTHER, false, Perm::Read));
        assert!(permits(&c, false, 0o040, OTHER, U, false, Perm::Read));
        assert!(permits(&c, false, 0o400, U, OTHER, false, Perm::Read));
    }

    #[test]
    fn a_supplementary_group_selects_the_group_class() {
        let c = Cred::of(U, U).set_groups(&[OTHER], true).unwrap();
        assert!(permits(&c, false, 0o020, 0, OTHER, false, Perm::Write));
        assert!(!permits(&c, false, 0o020, 0, 3, false, Perm::Write));
    }

    #[test]
    fn uid_zero_is_an_owner_like_any_other() {
        assert!(!permits(
            &Cred::ROOT,
            false,
            0o644,
            U,
            U,
            false,
            Perm::Write
        ));
        assert!(permits(&Cred::ROOT, true, 0o644, U, U, false, Perm::Write));
    }

    #[test]
    fn override_executes_only_what_some_class_may() {
        let c = Cred::of(U, U);
        assert!(permits(&c, true, 0o000, 0, 0, false, Perm::Read));
        assert!(permits(&c, true, 0o000, 0, 0, false, Perm::Write));
        assert!(!permits(&c, true, 0o666, 0, 0, false, Perm::Exec));
        assert!(permits(&c, true, 0o001, 0, 0, false, Perm::Exec));
        assert!(permits(&c, true, 0o000, 0, 0, true, Perm::Exec));
    }

    fn user() -> Subject {
        Subject {
            cred: Cred::of(U, U).set_groups(&[7], true).unwrap(),
            dac_override: false,
        }
    }

    #[test]
    fn only_the_owner_moves_a_file_and_only_to_its_own_groups() {
        let who = user();
        assert!(who.may_chown(U, U, None, Some(7)));
        assert!(who.may_chown(U, U, Some(U), Some(U)));
        assert!(!who.may_chown(U, U, None, Some(OTHER)));
        assert!(!who.may_chown(U, U, Some(OTHER), None));
        assert!(!who.may_chown(OTHER, OTHER, None, Some(7)));
        assert!(!who.may_chown(OTHER, OTHER, Some(OTHER), None));
        assert!(who.may_chown(OTHER, OTHER, None, None));
        let all = Subject {
            dac_override: true,
            ..who
        };
        assert!(all.may_chown(OTHER, OTHER, Some(3), Some(4)));
    }

    #[test]
    fn a_setgid_directory_hands_its_group_down() {
        let who = user();
        assert_eq!(who.child_owner(0o755, OTHER), (U, U));
        assert_eq!(who.child_owner(0o2775, OTHER), (U, OTHER));
        assert_eq!(who.child_mode(0o2775, OTHER, 0o755, true), 0o2755);
        assert_eq!(who.child_mode(0o755, OTHER, 0o6755, true), 0o755);
        assert_eq!(who.child_mode(0o2775, OTHER, 0o2654, false), 0o654);
        assert_eq!(who.child_mode(0o2775, OTHER, 0o2644, false), 0o2644);
        assert_eq!(who.child_mode(0o2775, 7, 0o2654, false), 0o2654);
        assert_eq!(who.child_mode(0o755, OTHER, 0o2654, false), 0o2654);
    }

    #[test]
    fn chmod_drops_setgid_outside_the_callers_groups() {
        let who = user();
        assert_eq!(who.chmod_mode(0o2755, OTHER), 0o755);
        assert_eq!(who.chmod_mode(0o2755, 7), 0o2755);
        let all = Subject {
            dac_override: true,
            ..who
        };
        assert_eq!(all.chmod_mode(0o2755, OTHER), 0o2755);
    }

    #[test]
    fn a_new_owner_takes_the_set_id_bits_off_a_file() {
        let who = user();
        assert_eq!(who.mode_after_chown(0o6755, U, false), 0o755);
        assert_eq!(who.mode_after_chown(0o6745, U, false), 0o2745);
        assert_eq!(who.mode_after_chown(0o6745, OTHER, false), 0o745);
        assert_eq!(who.mode_after_chown(0o6755, U, true), 0o6755);
    }

    #[test]
    fn exec_saves_the_effective_ids() {
        let c = cred(U, U, 0);
        assert_eq!(c.set_uid(0, false).map(|c| uids(&c)), Ok([U, 0, 0]));
        let after = c.saved_at_exec();
        assert_eq!(uids(&after), [U, U, U]);
        assert_eq!(after.set_uid(0, false), Err(Errno::EPERM));
    }

    #[test]
    fn a_sticky_directory_keeps_names_to_their_owners() {
        let c = Cred::of(U, U);
        assert!(sticky_permits(&c, false, 0o777, 0, OTHER));
        assert!(!sticky_permits(&c, false, 0o1777, 0, OTHER));
        assert!(sticky_permits(&c, false, 0o1777, 0, U));
        assert!(sticky_permits(&c, false, 0o1777, U, OTHER));
        assert!(sticky_permits(&c, true, 0o1777, 0, OTHER));
    }
}
