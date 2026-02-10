/// Generate `align_down_$suffix` and `align_up_$suffix` for a given integer type.
///
/// Both functions treat `alignment == 0` as a no-op (returns `value` unchanged).
/// `align_up` uses saturating arithmetic to prevent overflow.
macro_rules! impl_align_fns {
    ($ty:ty, $suffix:ident) => {
        paste::paste! {
            /// Align `value` down to the nearest multiple of `alignment`.
            /// If `alignment` is zero, the input is returned unchanged.
            #[inline(always)]
            pub const fn [<align_down_ $suffix>](value: $ty, alignment: $ty) -> $ty {
                if alignment == 0 {
                    return value;
                }
                value & !(alignment - 1)
            }

            /// Align `value` up to the nearest multiple of `alignment`.
            /// If `alignment` is zero, the input is returned unchanged.
            /// Uses saturating arithmetic to prevent overflow.
            #[inline(always)]
            pub const fn [<align_up_ $suffix>](value: $ty, alignment: $ty) -> $ty {
                if alignment == 0 {
                    return value;
                }
                let adjusted = value.saturating_add(alignment - 1);
                adjusted & !(alignment - 1)
            }
        }
    };
}

impl_align_fns!(u64, u64);
impl_align_fns!(usize, usize);
