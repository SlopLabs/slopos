//! The user and group databases as passwd(5) and group(5) spell them: one
//! entry a line, fields separated by `:`, a group's members by `,`.

/// What stands for each database while its file is absent or not sealed: the
/// system, the login screen, the one person at the machine, who administers
/// it, and `nobody`.
pub const BASE_PASSWD: &str = "root:x:0:0:root:/root:/bin/shell\n\
                               greeter:x:50:50:login screen:/:/bin/false\n\
                               user:x:1000:1000:user:/home/user:/bin/shell\n\
                               nobody:x:65534:65534:nobody:/nonexistent:/bin/false\n";
pub const BASE_GROUP: &str =
    "root:x:0:\nadmin:x:10:user\ngreeter:x:50:\nuser:x:1000:\nnobody:x:65534:\n";

/// The longest line either database takes. A longer one is no entry, so
/// every entry fits a buffer of this size with its fields' NULs.
pub const LINE_MAX: usize = 1024;

/// `name:password:uid:gid:gecos:home:shell`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Passwd<'a> {
    pub name: &'a [u8],
    pub passwd: &'a [u8],
    pub uid: u32,
    pub gid: u32,
    pub gecos: &'a [u8],
    pub dir: &'a [u8],
    pub shell: &'a [u8],
}

/// `name:password:gid:member,member`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Group<'a> {
    pub name: &'a [u8],
    pub passwd: &'a [u8],
    pub gid: u32,
    members: &'a [u8],
}

/// Exactly `N` fields, none holding a NUL, the first not empty.
fn fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    if line.len() > LINE_MAX || line.contains(&0) {
        return None;
    }
    let mut out = [&line[..0]; N];
    let mut parts = line.split(|&b| b == b':');
    for slot in &mut out {
        *slot = parts.next()?;
    }
    (parts.next().is_none() && !out[0].is_empty()).then_some(out)
}

/// A decimal id; `u32::MAX` is `(uid_t)-1`, which names no one.
fn id(text: &[u8]) -> Option<u32> {
    if text.is_empty() || text.len() > 10 || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value = text
        .iter()
        .fold(0u64, |acc, &d| acc * 10 + u64::from(d - b'0'));
    u32::try_from(value).ok().filter(|&id| id != u32::MAX)
}

impl<'a> Passwd<'a> {
    pub fn parse(line: &'a [u8]) -> Option<Self> {
        let [name, passwd, uid, gid, gecos, dir, shell] = fields(line)?;
        Some(Self {
            name,
            passwd,
            uid: id(uid)?,
            gid: id(gid)?,
            gecos,
            dir,
            shell,
        })
    }

    /// The bytes its five strings take with their NULs.
    pub fn strings_len(&self) -> usize {
        [self.name, self.passwd, self.gecos, self.dir, self.shell]
            .iter()
            .map(|field| field.len() + 1)
            .sum()
    }
}

impl<'a> Group<'a> {
    pub fn parse(line: &'a [u8]) -> Option<Self> {
        let [name, passwd, gid, members] = fields(line)?;
        Some(Self {
            name,
            passwd,
            gid: id(gid)?,
            members,
        })
    }

    pub fn members(&self) -> impl Iterator<Item = &'a [u8]> + 'a {
        self.members
            .split(|&b| b == b',')
            .filter(|member| !member.is_empty())
    }

    pub fn lists(&self, user: &[u8]) -> bool {
        self.members().any(|member| member == user)
    }
}

/// Each line of `text`, without its newline; the last needs none.
pub fn lines(text: &[u8]) -> impl Iterator<Item = &[u8]> {
    text.split(|&b| b == b'\n').filter(|line| !line.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_rows_read_back() {
        let users: [Passwd<'_>; 4] = {
            let mut rows = lines(BASE_PASSWD.as_bytes()).map(|l| Passwd::parse(l).unwrap());
            core::array::from_fn(|_| rows.next().unwrap())
        };
        assert_eq!(users[0].name, b"root");
        assert_eq!((users[0].uid, users[0].gid), (0, 0));
        assert_eq!((users[1].name, users[1].uid), (&b"greeter"[..], 50));
        assert_eq!(users[2].name, b"user");
        assert_eq!((users[2].uid, users[2].dir), (1000, &b"/home/user"[..]));
        assert_eq!(users[2].strings_len(), 5 + 2 + 5 + 11 + 11);
        assert_eq!((users[3].name, users[3].uid), (&b"nobody"[..], 65534));
        let groups: [Group<'_>; 5] = {
            let mut rows = lines(BASE_GROUP.as_bytes()).map(|l| Group::parse(l).unwrap());
            core::array::from_fn(|_| rows.next().unwrap())
        };
        assert_eq!((groups[1].name, groups[1].gid), (&b"admin"[..], 10));
        assert!(groups[1].lists(b"user"));
        assert_eq!(
            groups
                .iter()
                .filter(|g| g.members().next().is_none())
                .count(),
            4
        );
    }

    #[test]
    fn a_line_out_of_shape_is_no_entry() {
        for bad in [
            &b""[..],
            b"a:x:1:1:g:/h",
            b"a:x:1:1:g:/h:/s:extra",
            b":x:1:1:g:/h:/s",
            b"a:x:-1:1:g:/h:/s",
            b"a:x:4294967295:1:g:/h:/s",
            b"a:x:4294967296:1:g:/h:/s",
            b"a:x:1:+1:g:/h:/s",
            b"a:x:1: 1:g:/h:/s",
            b"a:x:1:1:g\0:/h:/s",
        ] {
            assert_eq!(Passwd::parse(bad), None, "{:?}", core::str::from_utf8(bad));
        }
        let long = [b'a'; LINE_MAX + 1];
        assert_eq!(Group::parse(&long), None);
        assert_eq!(Group::parse(b"g:x:7"), None);
    }

    #[test]
    fn a_group_lists_its_members() {
        let group = Group::parse(b"wheel:x:10:alice,,bob").unwrap();
        assert_eq!(group.gid, 10);
        assert!(group.lists(b"alice") && group.lists(b"bob"));
        assert!(!group.lists(b"") && !group.lists(b"ali"));
        assert_eq!(group.members().count(), 2);
    }
}
