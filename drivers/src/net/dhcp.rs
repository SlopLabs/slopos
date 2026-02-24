pub const UDP_PORT_SERVER: u16 = 67;
pub const UDP_PORT_CLIENT: u16 = 68;

pub const BOOTREQUEST: u8 = 1;
pub const BOOTREPLY: u8 = 2;
pub const FLAGS_BROADCAST: u16 = 0x8000;
pub const MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

pub const OPTION_PAD: u8 = 0;
pub const OPTION_SUBNET_MASK: u8 = 1;
pub const OPTION_ROUTER: u8 = 3;
pub const OPTION_DNS: u8 = 6;
pub const OPTION_REQUESTED_IP: u8 = 50;
pub const OPTION_MSG_TYPE: u8 = 53;
pub const OPTION_SERVER_ID: u8 = 54;
pub const OPTION_PARAM_REQ_LIST: u8 = 55;
pub const OPTION_END: u8 = 255;

pub const MSG_DISCOVER: u8 = 1;
pub const MSG_OFFER: u8 = 2;
pub const MSG_REQUEST: u8 = 3;
pub const MSG_ACK: u8 = 5;

pub const BOOTP_MIN_LEN: usize = 240;

#[derive(Clone, Copy)]
pub struct DhcpLease {
    pub ipv4: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub router: [u8; 4],
    pub dns: [u8; 4],
}

impl DhcpLease {
    pub const fn empty() -> Self {
        Self {
            ipv4: [0; 4],
            subnet_mask: [0; 4],
            router: [0; 4],
            dns: [0; 4],
        }
    }

    pub fn is_valid(&self) -> bool {
        self.ipv4 != [0; 4]
    }
}

#[derive(Clone, Copy)]
pub struct DhcpOffer {
    pub yiaddr: [u8; 4],
    pub server_id: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub router: [u8; 4],
    pub dns: [u8; 4],
}

pub fn build_discover(mac: [u8; 6], xid: u32, out: &mut [u8; 320]) -> usize {
    out.fill(0);
    out[0] = BOOTREQUEST;
    out[1] = 1;
    out[2] = 6;
    out[4..8].copy_from_slice(&xid.to_be_bytes());
    out[10..12].copy_from_slice(&FLAGS_BROADCAST.to_be_bytes());
    out[28..34].copy_from_slice(&mac);
    out[236..240].copy_from_slice(&MAGIC_COOKIE);

    let mut i = BOOTP_MIN_LEN;
    out[i] = OPTION_MSG_TYPE;
    out[i + 1] = 1;
    out[i + 2] = MSG_DISCOVER;
    i += 3;

    out[i] = OPTION_PARAM_REQ_LIST;
    out[i + 1] = 3;
    out[i + 2] = OPTION_SUBNET_MASK;
    out[i + 3] = OPTION_ROUTER;
    out[i + 4] = OPTION_DNS;
    i += 5;

    out[i] = OPTION_END;
    i + 1
}

pub fn build_request(mac: [u8; 6], xid: u32, offer: DhcpOffer, out: &mut [u8; 320]) -> usize {
    out.fill(0);
    out[0] = BOOTREQUEST;
    out[1] = 1;
    out[2] = 6;
    out[4..8].copy_from_slice(&xid.to_be_bytes());
    out[10..12].copy_from_slice(&FLAGS_BROADCAST.to_be_bytes());
    out[28..34].copy_from_slice(&mac);
    out[236..240].copy_from_slice(&MAGIC_COOKIE);

    let mut i = BOOTP_MIN_LEN;
    out[i] = OPTION_MSG_TYPE;
    out[i + 1] = 1;
    out[i + 2] = MSG_REQUEST;
    i += 3;

    out[i] = OPTION_REQUESTED_IP;
    out[i + 1] = 4;
    out[i + 2..i + 6].copy_from_slice(&offer.yiaddr);
    i += 6;

    out[i] = OPTION_SERVER_ID;
    out[i + 1] = 4;
    out[i + 2..i + 6].copy_from_slice(&offer.server_id);
    i += 6;

    out[i] = OPTION_PARAM_REQ_LIST;
    out[i + 1] = 3;
    out[i + 2] = OPTION_SUBNET_MASK;
    out[i + 3] = OPTION_ROUTER;
    out[i + 4] = OPTION_DNS;
    i += 5;

    out[i] = OPTION_END;
    i + 1
}

fn parse_options(
    options: &[u8],
    message_type: &mut u8,
    server_id: &mut [u8; 4],
    subnet_mask: &mut [u8; 4],
    router: &mut [u8; 4],
    dns: &mut [u8; 4],
) {
    let mut i = 0usize;
    while i < options.len() {
        let code = options[i];
        if code == OPTION_END {
            break;
        }
        if code == OPTION_PAD {
            i += 1;
            continue;
        }
        if i + 1 >= options.len() {
            break;
        }
        let len = options[i + 1] as usize;
        if i + 2 + len > options.len() {
            break;
        }

        let data = &options[i + 2..i + 2 + len];
        match code {
            OPTION_MSG_TYPE if len >= 1 => *message_type = data[0],
            OPTION_SERVER_ID if len >= 4 => server_id.copy_from_slice(&data[..4]),
            OPTION_SUBNET_MASK if len >= 4 => subnet_mask.copy_from_slice(&data[..4]),
            OPTION_ROUTER if len >= 4 => router.copy_from_slice(&data[..4]),
            OPTION_DNS if len >= 4 => dns.copy_from_slice(&data[..4]),
            _ => {}
        }

        i += 2 + len;
    }
}

pub fn parse_bootp_reply(payload: &[u8], xid: u32, expected_type: u8) -> Option<DhcpOffer> {
    if payload.len() < BOOTP_MIN_LEN {
        return None;
    }
    if payload[0] != BOOTREPLY {
        return None;
    }
    if u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]) != xid {
        return None;
    }
    if payload[236..240] != MAGIC_COOKIE {
        return None;
    }

    let mut message_type = 0u8;
    let mut server_id = [0u8; 4];
    let mut subnet_mask = [0u8; 4];
    let mut router = [0u8; 4];
    let mut dns = [0u8; 4];

    parse_options(
        &payload[240..],
        &mut message_type,
        &mut server_id,
        &mut subnet_mask,
        &mut router,
        &mut dns,
    );

    if message_type != expected_type {
        return None;
    }
    if expected_type == MSG_OFFER && server_id == [0; 4] {
        return None;
    }

    Some(DhcpOffer {
        yiaddr: [payload[16], payload[17], payload[18], payload[19]],
        server_id,
        subnet_mask,
        router,
        dns,
    })
}
