//! A `static` array borrowed one row at a time.

use core::cell::UnsafeCell;
use core::ops::Index;

/// `[T; N]` for a `static` read one row at a time.
///
/// Indexing an array static borrows the whole array, which Miri prices by its
/// size: a lookup in a large arena paid for the arena. Here it pays for the row.
pub struct StaticTable<T, const N: usize>(UnsafeCell<[T; N]>);

// SAFETY: the table hands out `&T` and nothing else, so sharing it is sharing
// `[T; N]`.
unsafe impl<T: Sync, const N: usize> Sync for StaticTable<T, N> {}

impl<T, const N: usize> StaticTable<T, N> {
    pub const fn new(rows: [T; N]) -> Self {
        Self(UnsafeCell::new(rows))
    }

    #[inline]
    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= N {
            return None;
        }
        // SAFETY: `index < N`, and no `&mut` into the array is ever formed.
        Some(unsafe { &*self.0.get().cast::<T>().add(index) })
    }

    /// Borrows the table once for the whole walk, not once per row.
    #[inline]
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        let base = self.0.get().cast::<T>().cast_const();
        // SAFETY: `i < N`, and no `&mut` into the array is ever formed.
        (0..N).map(move |i| unsafe { &*base.add(i) })
    }
}

impl<T, const N: usize> Index<usize> for StaticTable<T, N> {
    type Output = T;

    #[inline]
    #[track_caller]
    fn index(&self, index: usize) -> &T {
        match self.get(index) {
            Some(row) => row,
            None => panic!("row {index} of a {N}-row table"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU32, Ordering};

    static TABLE: StaticTable<AtomicU32, 4> = StaticTable::new([const { AtomicU32::new(0) }; 4]);

    #[test]
    fn a_row_is_the_same_row_however_it_is_reached() {
        TABLE[2].store(7, Ordering::Relaxed);
        assert_eq!(TABLE.get(2).map(|r| r.load(Ordering::Relaxed)), Some(7));
        let walked: [u32; 4] =
            core::array::from_fn(|i| TABLE.iter().nth(i).expect("row").load(Ordering::Relaxed));
        assert_eq!(walked, [0, 0, 7, 0]);
        assert_eq!(
            TABLE.iter().rev().nth(1).map(|r| r.load(Ordering::Relaxed)),
            Some(7)
        );
        assert_eq!(TABLE.iter().len(), 4);
    }

    #[test]
    fn past_the_end_is_absent() {
        assert!(TABLE.get(4).is_none());
        assert!(TABLE.get(usize::MAX).is_none());
    }

    #[test]
    #[should_panic(expected = "row 4 of a 4-row table")]
    fn indexing_past_the_end_panics() {
        let _ = &TABLE[4];
    }
}
