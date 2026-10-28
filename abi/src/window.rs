/// The seats `screen_acquire` and `input_sink_acquire` take, by rank: the
/// higher wins, so the kernel log and `/bin/roulette` can always take the
/// display back from the compositor.
pub const SEAT_COMPOSITOR_PRIMARY: u32 = 0;
pub const SEAT_VIRTCON: u32 = 1;
