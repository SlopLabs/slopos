use slopos_abi::damage::{DamageRect, MAX_DAMAGE_REGIONS, MAX_INTERNAL_DAMAGE_REGIONS};

#[derive(Clone)]
pub struct DamageTracker<const N: usize = MAX_DAMAGE_REGIONS> {
    regions: [DamageRect; N],
    count: u8,
    full_damage: bool,
}

impl<const N: usize> Default for DamageTracker<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> DamageTracker<N> {
    pub const fn new() -> Self {
        Self {
            regions: [DamageRect::invalid(); N],
            count: 0,
            full_damage: false,
        }
    }

    pub fn add(&mut self, rect: DamageRect) {
        if !rect.is_valid() {
            return;
        }

        if self.full_damage {
            return;
        }

        if (self.count as usize) >= N {
            self.merge_smallest_pair();
        }

        if (self.count as usize) < N {
            self.regions[self.count as usize] = rect;
            self.count += 1;
        } else {
            self.full_damage = true;
        }
    }

    pub fn add_merge_overlapping(&mut self, rect: DamageRect) {
        if !rect.is_valid() {
            return;
        }

        if self.full_damage {
            return;
        }

        for i in 0..(self.count as usize) {
            if self.regions[i].intersects(&rect) {
                self.regions[i] = self.regions[i].union(&rect);
                self.merge_all_overlapping();
                return;
            }
        }

        if (self.count as usize) < N {
            self.regions[self.count as usize] = rect;
            self.count += 1;
        } else {
            self.full_damage = true;
        }
    }

    #[inline]
    pub fn add_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.add(DamageRect { x0, y0, x1, y1 });
    }

    fn merge_smallest_pair(&mut self) {
        if self.count < 2 {
            return;
        }

        let count = self.count as usize;
        let mut best_i = 0;
        let mut best_j = 1;
        let mut best_area = i32::MAX;

        for i in 0..count {
            for j in (i + 1)..count {
                let combined = self.regions[i].combined_area(&self.regions[j]);
                if combined < best_area {
                    best_area = combined;
                    best_i = i;
                    best_j = j;
                }
            }
        }

        self.regions[best_i] = self.regions[best_i].union(&self.regions[best_j]);
        if best_j < count - 1 {
            self.regions[best_j] = self.regions[count - 1];
        }
        self.count -= 1;
    }

    fn merge_all_overlapping(&mut self) {
        if self.count <= 1 {
            return;
        }

        let mut i = 0;
        while i < self.count as usize {
            let mut j = i + 1;
            while j < self.count as usize {
                if self.regions[i].intersects(&self.regions[j]) {
                    self.regions[i] = self.regions[i].union(&self.regions[j]);
                    self.count -= 1;
                    self.regions[j] = self.regions[self.count as usize];
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }

    #[inline]
    pub fn clear(&mut self) {
        self.count = 0;
        self.full_damage = false;
    }

    #[inline]
    pub fn count(&self) -> u8 {
        self.count
    }

    #[inline]
    pub fn regions(&self) -> &[DamageRect] {
        &self.regions[..self.count as usize]
    }

    pub fn bounding_box(&self) -> DamageRect {
        if self.count == 0 {
            return DamageRect::invalid();
        }
        let mut result = self.regions[0];
        for i in 1..self.count as usize {
            result = result.union(&self.regions[i]);
        }
        result
    }

    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.count > 0 || self.full_damage
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0 && !self.full_damage
    }

    #[inline]
    pub fn is_full_damage(&self) -> bool {
        self.full_damage
    }

    #[inline]
    pub fn set_full_damage(&mut self) {
        self.full_damage = true;
    }

    pub fn export_to_array<const M: usize>(&self) -> ([DamageRect; M], u8) {
        let mut out = [DamageRect::invalid(); M];

        if self.full_damage {
            return (out, u8::MAX);
        }

        let export_count = (self.count as usize).min(M);
        for i in 0..export_count {
            out[i] = self.regions[i];
        }

        if (self.count as usize) > M {
            return (out, u8::MAX);
        }

        (out, export_count as u8)
    }
}

pub type InternalDamageTracker = DamageTracker<MAX_INTERNAL_DAMAGE_REGIONS>;
