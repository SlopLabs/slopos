//! The clients' buffers, mapped once rather than every frame, by surface
//! incarnation and buffer slot; the mapping outlives the fd it borrows.

use crate::syscall::{CachedShmMapping, UserWindowInfo};

use super::MAX_WINDOWS;

const BUFFERS_PER_TASK: usize = 2;
const CACHE_SLOTS: usize = MAX_WINDOWS * BUFFERS_PER_TASK;

/// `token` is the fd mapped, which a slot registered again changes.
struct ClientSurfaceEntry {
    task_id: u32,
    generation: u32,
    buffer_id: u8,
    token: u32,
    mapping: Option<CachedShmMapping>,
}

impl ClientSurfaceEntry {
    const fn empty() -> Self {
        Self {
            task_id: 0,
            generation: 0,
            buffer_id: 0,
            token: 0,
            mapping: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.mapping.is_none()
    }

    fn keyed(&self, task_id: u32, generation: u32, buffer_id: u8) -> bool {
        self.mapping.is_some()
            && self.task_id == task_id
            && self.generation == generation
            && self.buffer_id == buffer_id
    }
}

pub struct ClientSurfaceCache {
    entries: [ClientSurfaceEntry; CACHE_SLOTS],
}

impl ClientSurfaceCache {
    pub fn new() -> Self {
        Self {
            entries: core::array::from_fn(|_| ClientSurfaceEntry::empty()),
        }
    }

    /// A slot whose fd changed is mapped again in place.
    pub fn get_or_create_index(
        &mut self,
        task_id: u32,
        generation: u32,
        buffer_id: u8,
        token: u32,
        buffer_size: usize,
    ) -> Option<usize> {
        if token == 0 {
            return None;
        }

        if let Some(i) = self
            .entries
            .iter()
            .position(|e| e.keyed(task_id, generation, buffer_id))
        {
            if self.entries[i].token == token {
                return Some(i);
            }
            let mapping = CachedShmMapping::map_readonly_fd_borrowed(token as i32, buffer_size)?;
            self.entries[i].token = token;
            self.entries[i].mapping = Some(mapping);
            return Some(i);
        }

        let slot = self.entries.iter().position(|e| e.is_empty())?;
        let mapping = CachedShmMapping::map_readonly_fd_borrowed(token as i32, buffer_size)?;
        self.entries[slot] = ClientSurfaceEntry {
            task_id,
            generation,
            buffer_id,
            token,
            mapping: Some(mapping),
        };
        Some(slot)
    }

    pub fn get_slice(&self, index: usize) -> Option<&[u8]> {
        self.entries
            .get(index)?
            .mapping
            .as_ref()
            .map(|m| m.as_slice())
    }

    /// Drop the mappings of surfaces no longer shown, both of a surface's
    /// buffers kept, though it shows only one.
    pub fn cleanup_stale(&mut self, windows: &[UserWindowInfo; MAX_WINDOWS], window_count: u32) {
        for entry in &mut self.entries {
            if entry.is_empty() {
                continue;
            }
            let still_present = (0..window_count as usize).any(|i| {
                windows[i].task_id == entry.task_id && windows[i].generation == entry.generation
            });
            if !still_present {
                *entry = ClientSurfaceEntry::empty();
            }
        }
    }
}
