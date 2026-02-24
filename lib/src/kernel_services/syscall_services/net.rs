crate::define_service! {
    net => NetServices {
        scan_members(out: *mut slopos_abi::net::UserNetMember, max: usize, active: u32) -> usize;
        is_ready() -> u32;
    }
}
