//! The names `/dev/disk/by-id` gives a disk, as systemd's rules and `usb_id`
//! build them, but with `_` for the `/` and `\` udev keeps.

pub const DISK_ID_MAX: usize = 192;
const STEM_MAX: usize = DISK_ID_MAX - b"-part255".len();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bus {
    Nvme,
    Virtio,
    Usb,
}

impl Bus {
    const ALL: [Bus; 3] = [Bus::Nvme, Bus::Virtio, Bus::Usb];

    fn prefix(self) -> &'static str {
        match self {
            Bus::Nvme => "nvme-",
            Bus::Virtio => "virtio-",
            Bus::Usb => "usb-",
        }
    }
}

/// Whether udev writes `c` into a name as it is: an ASCII letter or digit,
/// one of `#+-.:=@_`, or beyond ASCII what systemd's `unichar_is_valid` takes.
pub fn kept(c: char) -> bool {
    match u32::from(c) {
        0..=0x7f => c.is_ascii_alphanumeric() || "#+-.:=@_".contains(c),
        0xfdd0..=0xfdef => false,
        scalar => scalar & 0xfffe != 0xfffe,
    }
}

fn blank(raw: &[u8]) -> bool {
    trimmed(raw).is_empty()
}

fn space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t'..=b'\r')
}

fn before_nul(raw: &[u8]) -> &[u8] {
    raw.split(|&b| b == 0).next().unwrap_or_default()
}

fn trimmed(raw: &[u8]) -> &[u8] {
    let raw = before_nul(raw);
    let start = raw.iter().position(|&b| !space(b)).unwrap_or(raw.len());
    let end = raw
        .iter()
        .rposition(|&b| !space(b))
        .map_or(start, |at| at + 1);
    &raw[start..end]
}

#[derive(Clone, Copy)]
enum Spaces {
    /// `$attr`'s: trailing space, tab, CR and LF dropped, any other
    /// whitespace byte `_`.
    Each,
    /// `udev_replace_whitespace`'s: none at either end, each run one `_`.
    Collapse,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DiskId {
    stem: [u8; STEM_MAX],
    len: u8,
    /// The prefix of the stem udev also names the disk by; 0 for none.
    alias: u8,
}

impl DiskId {
    fn begin(bus: Bus) -> Option<Self> {
        let mut id = Self {
            stem: [0; STEM_MAX],
            len: 0,
            alias: 0,
        };
        id.push(bus.prefix().as_bytes())?;
        Some(id)
    }

    /// `nvme-<model>_<serial>_<nsid>`, and namespace 1's alias
    /// `nvme-<model>_<serial>`.
    pub fn nvme(model: &[u8], serial: &[u8], nsid: u32) -> Option<Self> {
        let mut id = Self::begin(Bus::Nvme)?;
        id.filled(model, Spaces::Each)?;
        id.push(b"_")?;
        id.filled(serial, Spaces::Each)?;
        let alias = id.len;
        id.push(b"_")?;
        id.decimal(nsid)?;
        if nsid == 1 {
            id.alias = alias;
        }
        Some(id)
    }

    /// `virtio-<serial>`, from what `VIRTIO_BLK_T_GET_ID` answers.
    pub fn virtio(serial: &[u8]) -> Option<Self> {
        let mut id = Self::begin(Bus::Virtio)?;
        id.filled(serial, Spaces::Collapse)?;
        Some(id)
    }

    /// `usb-<vendor>_<product>_<serial>-0:<lun>`, without `_<serial>` for a
    /// stick with none. A blank vendor or product is an empty part, as
    /// `usb_id`'s is once it has fallen back.
    pub fn usb(vendor: &[u8], product: &[u8], serial: Option<&[u8]>, lun: u8) -> Option<Self> {
        let mut id = Self::begin(Bus::Usb)?;
        id.part(vendor, Spaces::Collapse)?;
        id.push(b"_")?;
        id.part(product, Spaces::Collapse)?;
        if let Some(serial) = serial.filter(|s| !blank(s)) {
            id.push(b"_")?;
            id.part(serial, Spaces::Collapse)?;
        }
        id.push(b"-0:")?;
        id.decimal(u32::from(lun))?;
        Some(id)
    }

    /// The name, or the alias, of the disk or its partition `partition`, into
    /// `out`; its length.
    pub fn write(&self, alias: bool, partition: Option<u8>, out: &mut [u8]) -> Option<usize> {
        let stem = match alias {
            true if self.alias == 0 => return None,
            true => usize::from(self.alias),
            false => usize::from(self.len),
        };
        let mut name = Name { out, len: 0 };
        name.push(&self.stem[..stem])?;
        if let Some(number) = partition {
            name.push(b"-part")?;
            name.push(digits(u32::from(number), &mut [0; 10]))?;
        }
        Some(name.len)
    }

    fn push(&mut self, bytes: &[u8]) -> Option<()> {
        let at = usize::from(self.len);
        self.stem
            .get_mut(at..at + bytes.len())?
            .copy_from_slice(bytes);
        self.len += bytes.len() as u8;
        Some(())
    }

    fn decimal(&mut self, value: u32) -> Option<()> {
        self.push(digits(value, &mut [0; 10]))
    }

    fn filled(&mut self, raw: &[u8], spaces: Spaces) -> Option<()> {
        (self.part(raw, spaces)? > 0).then_some(())
    }

    /// `raw` as `string_escape=replace` writes it: kept characters as they
    /// are, every other byte `_`. Its length, `None` for no room.
    fn part(&mut self, raw: &[u8], spaces: Spaces) -> Option<usize> {
        let raw = match spaces {
            Spaces::Each => {
                let raw = before_nul(raw);
                let end = raw
                    .iter()
                    .rposition(|b| !b" \t\n\r".contains(b))
                    .map_or(0, |at| at + 1);
                &raw[..end]
            }
            Spaces::Collapse => trimmed(raw),
        };
        let start = self.len;
        let mut gap = false;
        for chunk in raw.utf8_chunks() {
            for c in chunk.valid().chars() {
                if matches!(spaces, Spaces::Collapse) && c.is_ascii() && space(c as u8) {
                    gap = true;
                } else if kept(c) {
                    self.put(&mut gap, c.encode_utf8(&mut [0; 4]).as_bytes())?;
                } else {
                    for _ in 0..c.len_utf8() {
                        self.put(&mut gap, b"_")?;
                    }
                }
            }
            for _ in chunk.invalid() {
                self.put(&mut gap, b"_")?;
            }
        }
        Some(usize::from(self.len - start))
    }

    fn put(&mut self, gap: &mut bool, bytes: &[u8]) -> Option<()> {
        if core::mem::take(gap) {
            self.push(b"_")?;
        }
        self.push(bytes)
    }
}

/// A disk as its by-id names describe it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Described<'a> {
    pub bus: Bus,
    model: &'a str,
}

impl<'a> Described<'a> {
    /// Its maker's model a word at a time, `_` read as whitespace: what an
    /// NVMe or USB name carries before its last `_` part, read as the serial.
    /// A serial with whitespace inside, which a Bulk-Only one never holds,
    /// lends the model its first words, and a USB disk without a serial loses
    /// its product's last word; a virtio name carries none.
    pub fn model(&self) -> impl Iterator<Item = &'a str> {
        self.model.split('_').filter(|word| !word.is_empty())
    }
}

/// What the by-id `names` of one disk say of it. Namespace 1's shorter name
/// is a prefix of its longer one, so the longest is read.
pub fn describe<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<Described<'a>> {
    fn digits(text: &str) -> bool {
        !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
    }
    let (bus, rest) = names
        .into_iter()
        .filter_map(|name| {
            Bus::ALL
                .into_iter()
                .find_map(|bus| Some((bus, name.strip_prefix(bus.prefix())?)))
        })
        .max_by_key(|(_, rest)| rest.len())?;
    let id = match bus {
        Bus::Nvme => rest.rsplit_once('_').filter(|(_, nsid)| digits(nsid)),
        Bus::Usb => rest.rsplit_once("-0:").filter(|(_, lun)| digits(lun)),
        Bus::Virtio => None,
    };
    let model = id
        .and_then(|(id, _)| id.rsplit_once('_'))
        .map_or("", |(model, _serial)| model);
    Some(Described { bus, model })
}

struct Name<'a> {
    out: &'a mut [u8],
    len: usize,
}

impl Name<'_> {
    fn push(&mut self, bytes: &[u8]) -> Option<()> {
        self.out
            .get_mut(self.len..self.len + bytes.len())?
            .copy_from_slice(bytes);
        self.len += bytes.len();
        Some(())
    }
}

fn digits(mut value: u32, buf: &mut [u8; 10]) -> &[u8] {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &buf[at..];
        }
    }
}

#[cfg(test)]
mod tests;
