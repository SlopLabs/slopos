//! Address text for `inet_pton`/`inet_ntop`: RFC 4291 section 2.2 to read
//! IPv6, RFC 5952 section 4 to write it.

/// Exactly four decimal parts of at most three digits, each 0..=255: unlike
/// `inet_addr`, no shorter forms and no octal or hex.
pub fn parse_ipv4(text: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = text.split(|&b| b == b'.');
    for slot in &mut out {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 3 || !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let value = part
            .iter()
            .fold(0u32, |acc, &d| acc * 10 + u32::from(d - b'0'));
        *slot = u8::try_from(value).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

/// Eight groups of one to four hex digits, one `::` for one or more zero
/// groups, optionally ending in a dotted-decimal IPv4 address.
pub fn parse_ipv6(text: &[u8]) -> Option<[u8; 16]> {
    let mut groups = [0u16; 8];
    let mut count = 0usize;
    let mut gap: Option<usize> = None;
    let mut i = 0usize;

    if text.starts_with(b"::") {
        gap = Some(0);
        i = 2;
        if text.len() == 2 {
            return Some([0; 16]);
        }
    } else if text.first() == Some(&b':') {
        return None;
    }

    loop {
        let start = i;
        while i < text.len() && text[i].is_ascii_hexdigit() {
            i += 1;
        }
        let digits = &text[start..i];
        if i < text.len() && text[i] == b'.' {
            if count > 6 {
                return None;
            }
            let v4 = parse_ipv4(&text[start..])?;
            groups[count] = u16::from_be_bytes([v4[0], v4[1]]);
            groups[count + 1] = u16::from_be_bytes([v4[2], v4[3]]);
            count += 2;
            break;
        }
        if digits.is_empty() || digits.len() > 4 || count == 8 {
            return None;
        }
        groups[count] = digits.iter().fold(0u16, |acc, &d| {
            let v = match d {
                b'0'..=b'9' => d - b'0',
                b'a'..=b'f' => d - b'a' + 10,
                _ => d - b'A' + 10,
            };
            (acc << 4) | u16::from(v)
        });
        count += 1;
        if i == text.len() {
            break;
        }
        if text[i] != b':' {
            return None;
        }
        i += 1;
        if i < text.len() && text[i] == b':' {
            if gap.is_some() {
                return None;
            }
            gap = Some(count);
            i += 1;
            if i == text.len() {
                break;
            }
        } else if i == text.len() {
            return None;
        }
    }

    match gap {
        Some(at) => {
            if count >= 8 {
                return None;
            }
            let tail = count - at;
            groups.copy_within(at..count, 8 - tail);
            groups[at..8 - tail].fill(0);
        }
        None if count != 8 => return None,
        None => {}
    }
    let mut out = [0u8; 16];
    for (pair, group) in out.chunks_exact_mut(2).zip(groups) {
        pair.copy_from_slice(&group.to_be_bytes());
    }
    Some(out)
}

/// Longest text either writer produces, excluding the terminator:
/// `255.255.255.255` and `ffff:ffff:ffff:ffff:ffff:ffff:255.255.255.255`.
pub const IPV4_TEXT_MAX: usize = 15;
pub const IPV6_TEXT_MAX: usize = 45;

struct Cursor<'a> {
    out: &'a mut [u8],
    len: usize,
}

impl Cursor<'_> {
    fn push(&mut self, byte: u8) -> Option<()> {
        *self.out.get_mut(self.len)? = byte;
        self.len += 1;
        Some(())
    }

    fn decimal(&mut self, value: u8) -> Option<()> {
        if value >= 100 {
            self.push(b'0' + value / 100)?;
        }
        if value >= 10 {
            self.push(b'0' + value / 10 % 10)?;
        }
        self.push(b'0' + value % 10)
    }

    fn hex(&mut self, value: u16) -> Option<()> {
        let mut started = false;
        for shift in [12u16, 8, 4, 0] {
            let nibble = ((value >> shift) & 0xf) as u8;
            if nibble != 0 || started || shift == 0 {
                started = true;
                self.push(if nibble < 10 {
                    b'0' + nibble
                } else {
                    b'a' + nibble - 10
                })?;
            }
        }
        Some(())
    }

    fn ipv4(&mut self, addr: [u8; 4]) -> Option<()> {
        for (i, octet) in addr.into_iter().enumerate() {
            if i > 0 {
                self.push(b'.')?;
            }
            self.decimal(octet)?;
        }
        Some(())
    }
}

/// Dotted decimal into `out`; the length, or `None` if it does not fit.
pub fn format_ipv4(addr: [u8; 4], out: &mut [u8]) -> Option<usize> {
    let mut cursor = Cursor { out, len: 0 };
    cursor.ipv4(addr)?;
    Some(cursor.len)
}

/// Writes `addr` in RFC 5952's canonical form: lowercase, no leading zeros,
/// the longest run of two or more zero groups (the first on a tie) as `::`,
/// and `::ffff:a.b.c.d` for an IPv4-mapped address.
pub fn format_ipv6(addr: [u8; 16], out: &mut [u8]) -> Option<usize> {
    let mut groups = [0u16; 8];
    for (group, pair) in groups.iter_mut().zip(addr.chunks_exact(2)) {
        *group = u16::from_be_bytes([pair[0], pair[1]]);
    }
    let mut cursor = Cursor { out, len: 0 };
    if groups[..5] == [0; 5] && groups[5] == 0xffff {
        for &b in b"::ffff:" {
            cursor.push(b)?;
        }
        cursor.ipv4([addr[12], addr[13], addr[14], addr[15]])?;
        return Some(cursor.len);
    }

    let (mut best_at, mut best_len) = (8usize, 0usize);
    let mut i = 0;
    while i < 8 {
        if groups[i] == 0 {
            let start = i;
            while i < 8 && groups[i] == 0 {
                i += 1;
            }
            if i - start > best_len {
                best_at = start;
                best_len = i - start;
            }
        } else {
            i += 1;
        }
    }
    if best_len < 2 {
        best_at = 8;
    }

    let mut i = 0;
    while i < 8 {
        if i == best_at {
            cursor.push(b':')?;
            cursor.push(b':')?;
            i += best_len;
            continue;
        }
        if i > 0 && i != best_at + best_len {
            cursor.push(b':')?;
        }
        cursor.hex(groups[i])?;
        i += 1;
    }
    Some(cursor.len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v6(text: &str) -> Option<[u8; 16]> {
        parse_ipv6(text.as_bytes())
    }

    fn assert_shows(addr: [u8; 16], want: &str) {
        let mut buf = [0u8; IPV6_TEXT_MAX];
        let len = format_ipv6(addr, &mut buf).unwrap();
        assert_eq!(&buf[..len], want.as_bytes());
    }

    #[test]
    fn ipv4_takes_only_four_decimal_parts() {
        assert_eq!(parse_ipv4(b"192.168.0.1"), Some([192, 168, 0, 1]));
        assert_eq!(parse_ipv4(b"0.0.0.0"), Some([0; 4]));
        assert_eq!(parse_ipv4(b"255.255.255.255"), Some([255; 4]));
        for bad in [
            &b"256.0.0.1"[..],
            b"1.2.3",
            b"1.2.3.4.5",
            b"1.2.3.",
            b"0x1.2.3.4",
            b"1..3.4",
            b"1234.1.1.1",
            b"",
            b" 1.2.3.4",
        ] {
            assert_eq!(parse_ipv4(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn ipv6_expands_the_gap_and_the_embedded_ipv4() {
        let mut loopback = [0u8; 16];
        loopback[15] = 1;
        assert_eq!(v6("::1"), Some(loopback));
        assert_eq!(v6("::"), Some([0; 16]));
        assert_eq!(
            v6("2001:DB8::8:800:200C:417A"),
            Some([
                0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0x08, 0x08, 0, 0x20, 0x0c, 0x41, 0x7a
            ])
        );
        assert_eq!(
            v6("::ffff:10.0.2.2"),
            Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 10, 0, 2, 2])
        );
        assert_eq!(
            v6("1:2:3:4:5:6:7:8"),
            Some([0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0, 8])
        );
        assert_eq!(
            v6("fe80::"),
            Some([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        );
    }

    #[test]
    fn ipv6_rejects_malformed_text() {
        for bad in [
            "1:2:3:4:5:6:7",
            "1:2:3:4:5:6:7:8:9",
            "1::2::3",
            ":1:2:3:4:5:6:7",
            "1:2:3:4:5:6:7:",
            "12345::",
            "1:2:3:4:5:6:7::8",
            "::g",
            "1:2:3:4:5:6:7:1.2.3.4",
            "",
            ":",
        ] {
            assert_eq!(v6(bad), None, "{bad}");
        }
    }

    #[test]
    fn ipv6_writes_the_canonical_form() {
        assert_shows(v6("2001:db8:0:0:0:0:2:1").unwrap(), "2001:db8::2:1");
        assert_shows(v6("2001:db8:0:1:1:1:1:1").unwrap(), "2001:db8:0:1:1:1:1:1");
        assert_shows(v6("2001:0:0:1:0:0:0:1").unwrap(), "2001:0:0:1::1");
        assert_shows(v6("2001:db8:0:0:1:0:0:1").unwrap(), "2001:db8::1:0:0:1");
        assert_shows(v6("::1").unwrap(), "::1");
        assert_shows([0; 16], "::");
        assert_shows(v6("fe80::").unwrap(), "fe80::");
        assert_shows(v6("::ffff:10.0.2.2").unwrap(), "::ffff:10.0.2.2");
        assert_shows(
            v6("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff").unwrap(),
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        );
    }

    #[test]
    fn writers_refuse_a_short_buffer() {
        let mut small = [0u8; 6];
        assert_eq!(format_ipv4([10, 0, 2, 2], &mut small), None);
        assert_eq!(format_ipv4([1, 2, 3, 4], &mut small[..]), None);
        let mut exact = [0u8; 7];
        assert_eq!(format_ipv4([1, 2, 3, 4], &mut exact), Some(7));
        let mut tiny = [0u8; 2];
        assert_eq!(format_ipv6([0; 16], &mut tiny), Some(2));
        assert_eq!(format_ipv6(v6("::1").unwrap(), &mut tiny), None);
    }
}
