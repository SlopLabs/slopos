//! LRU glyph cache for avoiding re-rasterization of frequently used glyphs.

use slopos_ostd::KVec;

use crate::rasterizer::RasterizedGlyph;

/// Four quarter-pixel phases of the glyphs a few screens of text use.
const MAX_CACHE_SIZE: usize = 1024;

/// Codepoint, size in pixels and quarter-pixel phase.
pub type CacheKey = (u32, u16, u8);

struct CacheEntry {
    key: CacheKey,
    glyph: RasterizedGlyph,
    last_used: u64,
}

/// A fixed-capacity LRU glyph cache, kept sorted by key: every glyph of every
/// drawn run looks itself up, so a lookup is a binary search.
pub struct GlyphCache {
    entries: KVec<CacheEntry>,
    clock: u64,
}

impl GlyphCache {
    pub fn new() -> Self {
        Self {
            entries: KVec::new(),
            clock: 0,
        }
    }

    fn find(&self, key: CacheKey) -> Result<usize, usize> {
        self.entries
            .as_slice()
            .binary_search_by(|entry| entry.key.cmp(&key))
    }

    pub fn get(&mut self, key: CacheKey) -> Option<&RasterizedGlyph> {
        let at = self.find(key).ok()?;
        self.clock += 1;
        let entry = &mut self.entries.as_mut_slice()[at];
        entry.last_used = self.clock;
        Some(&entry.glyph)
    }

    /// Insert a rasterized glyph into the cache, evicting the LRU entry if full.
    pub fn insert(&mut self, key: CacheKey, glyph: RasterizedGlyph) {
        self.clock += 1;
        let mut at = match self.find(key) {
            Ok(at) => {
                let entry = &mut self.entries.as_mut_slice()[at];
                entry.glyph = glyph;
                entry.last_used = self.clock;
                return;
            }
            Err(at) => at,
        };

        if self.entries.len() >= MAX_CACHE_SIZE {
            let oldest = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
                .map_or(0, |(index, _)| index);
            self.entries.remove(oldest);
            if oldest < at {
                at -= 1;
            }
        }

        let _ = self.entries.insert(
            at,
            CacheEntry {
                key,
                glyph,
                last_used: self.clock,
            },
        );
    }
}
