use super::driver::TtyDriverKind;
use super::table::{TTY_INPUT_WAITERS, TTY_SLOTS, find_free_slot, find_free_slot_excluding};
use super::{MAX_TTYS, Tty, TtyError, TtyIndex};

pub fn pty_alloc() -> Result<TtyIndex, TtyError> {
    let master_slot = find_free_slot().ok_or(TtyError::NotAllocated)?;
    let slave_slot = find_free_slot_excluding(master_slot).ok_or(TtyError::NotAllocated)?;

    let master_idx = TtyIndex(master_slot as u8);
    let slave_idx = TtyIndex(slave_slot as u8);

    {
        let mut guard = TTY_SLOTS[master_slot].lock();
        *guard = Some(Tty::new_pty_master(master_idx, slave_idx));
    }
    {
        let mut guard = TTY_SLOTS[slave_slot].lock();
        *guard = Some(Tty::new_pty_slave(slave_idx, master_idx));
    }

    Ok(master_idx)
}

pub fn master_write(slave_idx: TtyIndex, data: &[u8]) {
    for &byte in data {
        super::push_input(slave_idx, byte);
    }
}

pub fn slave_write(master_idx: TtyIndex, data: &[u8]) {
    let slot = master_idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let should_wake = {
        let mut guard = TTY_SLOTS[slot].lock();
        let Some(master) = guard.as_mut() else {
            return;
        };

        if master.peer_closed || master.hung_up {
            return;
        }

        for &byte in data {
            let _ = master.ldisc.input_char(byte);
        }

        master.ldisc.has_data()
    };

    if should_wake {
        TTY_INPUT_WAITERS[slot].wake_all();
    }
}

pub fn is_pty_slave(idx: TtyIndex) -> bool {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }

    let guard = TTY_SLOTS[slot].lock();
    matches!(
        guard.as_ref().map(|tty| &tty.driver),
        Some(TtyDriverKind::PtySlave { .. })
    )
}

pub fn get_pty_number(idx: TtyIndex) -> Result<u32, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    let guard = TTY_SLOTS[slot].lock();
    let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
    match tty.driver {
        TtyDriverKind::PtyMaster { slave_idx } => Ok(slave_idx.0 as u32),
        _ => Err(TtyError::NotAllocated),
    }
}

pub fn mark_peer_closed(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.peer_closed = true;
    }
    drop(guard);
    TTY_INPUT_WAITERS[slot].wake_all();
}

pub fn clear_peer_closed(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.peer_closed = false;
    }
}

pub fn free_pair_if_unused(idx: TtyIndex, peer_idx: TtyIndex) {
    let idx_slot = idx.0 as usize;
    let peer_slot = peer_idx.0 as usize;
    if idx_slot >= MAX_TTYS || peer_slot >= MAX_TTYS {
        return;
    }

    let idx_unused = {
        let guard = TTY_SLOTS[idx_slot].lock();
        matches!(guard.as_ref(), Some(tty) if tty.open_count == 0)
    };
    let peer_unused = {
        let guard = TTY_SLOTS[peer_slot].lock();
        matches!(guard.as_ref(), Some(tty) if tty.open_count == 0)
    };

    if !(idx_unused && peer_unused) {
        return;
    }

    {
        let mut guard = TTY_SLOTS[idx_slot].lock();
        *guard = None;
    }
    {
        let mut guard = TTY_SLOTS[peer_slot].lock();
        *guard = None;
    }
}
