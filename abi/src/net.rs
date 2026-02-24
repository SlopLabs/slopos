#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserNetMember {
    pub ipv4: [u8; 4],
    pub mac: [u8; 6],
    pub flags: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserNetInfo {
    pub ipv4: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub gateway: [u8; 4],
    pub dns: [u8; 4],
    pub mac: [u8; 6],
    pub mtu: u16,
    pub link_up: u8,
    pub nic_ready: u8,
    pub _pad: [u8; 2],
}

pub const USER_NET_MEMBER_FLAG_ARP: u16 = 1 << 0;
pub const USER_NET_MEMBER_FLAG_IPV4: u16 = 1 << 1;

pub const USER_NET_MAX_MEMBERS: usize = 32;
