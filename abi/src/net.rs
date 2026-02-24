#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserNetMember {
    pub ipv4: [u8; 4],
    pub mac: [u8; 6],
    pub flags: u16,
}

pub const USER_NET_MEMBER_FLAG_ARP: u16 = 1 << 0;
pub const USER_NET_MEMBER_FLAG_IPV4: u16 = 1 << 1;

pub const USER_NET_MAX_MEMBERS: usize = 32;
