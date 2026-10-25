//! Minimal TrueType font parser.
//!
//! Parses the subset of TTF tables needed for basic Latin text rendering:
//! `head`, `maxp`, `cmap`, `hhea`, `hmtx`, `loca`, `glyf`, and the pair
//! adjustments of `GPOS`'s `kern` feature.

use core::cell::{Cell, OnceCell};

use slopos_ostd::KVec;

pub struct TtfFont<'a> {
    data: &'a [u8],
    head: HeadTable,
    maxp: MaxpTable,
    cmap_offset: usize,
    hhea: HheaTable,
    hmtx_offset: usize,
    loca_offset: usize,
    glyf_offset: usize,
    pairs: KVec<PairSubtable>,
    /// Recently asked pairs and their kerning, direct-mapped by the pair:
    /// reading the class tables costs several binary searches, and text
    /// repeats its pairs. Made by the first pair asked, so a font that is only
    /// rasterised into an atlas never allocates it; empty when it could not be.
    kern_memo: OnceCell<KVec<Cell<u64>>>,
}

/// log2 of the memo's slot count.
const KERN_MEMO_BITS: u32 = 10;

/// No pair packs to an all-ones key: glyph ids stop short of 0xFFFF.
const KERN_MEMO_EMPTY: u64 = u64::MAX;

/// A pair-adjustment subtable of the `kern` feature, in lookup order. Within
/// one lookup the first subtable that applies to a pair decides it; separate
/// lookups add up.
#[derive(Clone, Copy)]
struct PairSubtable {
    offset: usize,
    lookup: u16,
}

#[derive(Clone, Copy)]
struct HeadTable {
    units_per_em: u16,
    index_to_loc_format: i16, // 0 = short, 1 = long
}

#[derive(Clone, Copy)]
struct MaxpTable {
    num_glyphs: u16,
}

#[derive(Clone, Copy)]
pub struct HheaTable {
    pub ascender: i16,
    pub descender: i16,
    pub line_gap: i16,
    pub number_of_h_metrics: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct OutlinePoint {
    pub x: i16,
    pub y: i16,
    pub on_curve: bool,
}

/// A closed loop of outline points.
#[derive(Clone, Debug)]
pub struct Contour {
    pub points: KVec<OutlinePoint>,
}

#[derive(Clone, Debug)]
pub struct GlyphOutline {
    pub contours: KVec<Contour>,
    pub x_min: i16,
    pub y_min: i16,
    pub x_max: i16,
    pub y_max: i16,
}

#[derive(Clone, Copy, Debug)]
pub struct HMetrics {
    pub advance_width: u16,
    pub left_side_bearing: i16,
}

fn read_u8(data: &[u8], offset: usize) -> Option<u8> {
    data.get(offset).copied()
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    if offset + 2 > data.len() {
        return None;
    }
    Some(u16::from_be_bytes([data[offset], data[offset + 1]]))
}

fn read_i16(data: &[u8], offset: usize) -> Option<i16> {
    read_u16(data, offset).map(|v| v as i16)
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    if offset + 4 > data.len() {
        return None;
    }
    Some(u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

/// Find a table in the TrueType offset table; returns `(offset, length)`.
fn find_table(data: &[u8], tag: &[u8; 4]) -> Option<(usize, usize)> {
    let num_tables = read_u16(data, 4)? as usize;
    for i in 0..num_tables {
        let record_offset = 12 + i * 16;
        if record_offset + 16 > data.len() {
            break;
        }
        if &data[record_offset..record_offset + 4] == tag {
            let offset = read_u32(data, record_offset + 8)? as usize;
            let length = read_u32(data, record_offset + 12)? as usize;
            return Some((offset, length));
        }
    }
    None
}

impl<'a> TtfFont<'a> {
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        if data.len() < 12 {
            return None;
        }

        // sfVersion: 0x00010000 (TrueType) or 'true'.
        let sf_version = read_u32(data, 0)?;
        if sf_version != 0x00010000 && sf_version != 0x74727565 {
            return None;
        }

        let (head_off, _) = find_table(data, b"head")?;
        let head = HeadTable {
            units_per_em: read_u16(data, head_off + 18)?,
            index_to_loc_format: read_i16(data, head_off + 50)?,
        };
        if head.units_per_em == 0 {
            return None;
        }

        let (maxp_off, _) = find_table(data, b"maxp")?;
        let maxp = MaxpTable {
            num_glyphs: read_u16(data, maxp_off + 4)?,
        };

        let (cmap_offset, _) = find_table(data, b"cmap")?;

        let (hhea_off, _) = find_table(data, b"hhea")?;
        let hhea = HheaTable {
            ascender: read_i16(data, hhea_off + 4)?,
            descender: read_i16(data, hhea_off + 6)?,
            line_gap: read_i16(data, hhea_off + 8)?,
            number_of_h_metrics: read_u16(data, hhea_off + 34)?,
        };

        let (hmtx_offset, _) = find_table(data, b"hmtx")?;
        let (loca_offset, _) = find_table(data, b"loca")?;
        let (glyf_offset, _) = find_table(data, b"glyf")?;

        Some(Self {
            data,
            head,
            maxp,
            cmap_offset,
            hhea,
            hmtx_offset,
            loca_offset,
            glyf_offset,
            // A malformed GPOS kerns nothing rather than refusing the font.
            pairs: kern_pair_subtables(data).unwrap_or_else(KVec::new),
            kern_memo: OnceCell::new(),
        })
    }

    pub fn units_per_em(&self) -> u16 {
        self.head.units_per_em
    }

    pub fn hhea(&self) -> &HheaTable {
        &self.hhea
    }

    /// Only Format 4 (BMP) cmap subtables are supported.
    pub fn glyph_index(&self, codepoint: u32) -> Option<u16> {
        if codepoint > 0xFFFF {
            return None;
        }
        let cp = codepoint as u16;
        let data = self.data;
        let cmap_off = self.cmap_offset;

        let num_subtables = read_u16(data, cmap_off + 2)? as usize;

        // platform 0 = Unicode; platform 3 / encoding 1 or 10 = Windows Unicode.
        for i in 0..num_subtables {
            let record = cmap_off + 4 + i * 8;
            let platform_id = read_u16(data, record)?;
            let encoding_id = read_u16(data, record + 2)?;
            let subtable_offset = read_u32(data, record + 4)? as usize;

            let is_unicode =
                platform_id == 0 || (platform_id == 3 && (encoding_id == 1 || encoding_id == 10));

            if !is_unicode {
                continue;
            }

            let sub = cmap_off + subtable_offset;
            let format = read_u16(data, sub)?;

            if format == 4 {
                return self.cmap_format4_lookup(sub, cp);
            }
        }

        None
    }

    fn cmap_format4_lookup(&self, sub: usize, cp: u16) -> Option<u16> {
        let data = self.data;
        let seg_count = (read_u16(data, sub + 6)? / 2) as usize;

        let end_code_base = sub + 14;
        let start_code_base = end_code_base + seg_count * 2 + 2; // +2 for reservedPad
        let id_delta_base = start_code_base + seg_count * 2;
        let id_range_offset_base = id_delta_base + seg_count * 2;

        for seg in 0..seg_count {
            let end_code = read_u16(data, end_code_base + seg * 2)?;
            if cp > end_code {
                continue;
            }

            let start_code = read_u16(data, start_code_base + seg * 2)?;
            if cp < start_code {
                return Some(0); // .notdef
            }

            let id_delta = read_i16(data, id_delta_base + seg * 2)? as i32;
            let id_range_offset = read_u16(data, id_range_offset_base + seg * 2)?;

            let glyph_id = if id_range_offset == 0 {
                ((cp as i32 + id_delta) & 0xFFFF) as u16
            } else {
                let glyph_id_offset = id_range_offset_base
                    + seg * 2
                    + id_range_offset as usize
                    + (cp as usize - start_code as usize) * 2;
                let gid = read_u16(data, glyph_id_offset)?;
                if gid == 0 {
                    0
                } else {
                    ((gid as i32 + id_delta) & 0xFFFF) as u16
                }
            };

            return Some(glyph_id);
        }

        Some(0)
    }

    pub fn h_metrics(&self, glyph_id: u16) -> Option<HMetrics> {
        let data = self.data;
        let num_h_metrics = self.hhea.number_of_h_metrics as usize;
        let hmtx = self.hmtx_offset;

        if (glyph_id as usize) < num_h_metrics {
            let off = hmtx + (glyph_id as usize) * 4;
            Some(HMetrics {
                advance_width: read_u16(data, off)?,
                left_side_bearing: read_i16(data, off + 2)?,
            })
        } else {
            // Past numberOfHMetrics hmtx stores lsb only; the advance width is
            // the last recorded one.
            let last_aw_off = hmtx + (num_h_metrics - 1) * 4;
            let advance_width = read_u16(data, last_aw_off)?;
            let lsb_index = (glyph_id as usize) - num_h_metrics;
            let lsb_off = hmtx + num_h_metrics * 4 + lsb_index * 2;
            let left_side_bearing = read_i16(data, lsb_off)?;
            Some(HMetrics {
                advance_width,
                left_side_bearing,
            })
        }
    }

    /// How far `right` moves when it follows `left`, in font units: the first
    /// glyph's x-advance adjustment from each `kern` lookup, summed. The other
    /// adjustments a pair can carry (placements, the second glyph's advance)
    /// are not read; Inter's `kern` lookups carry none.
    pub fn kerning(&self, left: u16, right: u16) -> i32 {
        if self.pairs.is_empty() {
            return 0;
        }
        let key = (left as u32) << 16 | right as u32;
        let memo = self.kern_memo.get_or_init(|| {
            KVec::from_iter_fallible(
                (0..1usize << KERN_MEMO_BITS).map(|_| Cell::new(KERN_MEMO_EMPTY)),
            )
            .unwrap_or_else(|_| KVec::new())
        });
        let slot = memo.get(key.wrapping_mul(0x9E37_79B9) as usize >> (32 - KERN_MEMO_BITS));
        if let Some(slot) = slot {
            let cached = slot.get();
            if (cached >> 32) as u32 == key {
                return cached as u32 as i32;
            }
        }

        let mut total = 0i32;
        let mut decided: Option<u16> = None;
        for pair in self.pairs.iter() {
            if decided == Some(pair.lookup) {
                continue;
            }
            if let Some(adjust) = pair_x_advance(self.data, pair.offset, left, right) {
                total += adjust as i32;
                decided = Some(pair.lookup);
            }
        }
        if let Some(slot) = slot {
            slot.set((key as u64) << 32 | total as u32 as u64);
        }
        total
    }

    /// `(offset, length)` of a glyph within the glyf table.
    fn glyph_offset(&self, glyph_id: u16) -> Option<(usize, usize)> {
        let data = self.data;
        let loca = self.loca_offset;
        let gid = glyph_id as usize;

        if gid >= self.maxp.num_glyphs as usize {
            return None;
        }

        let (off0, off1) = if self.head.index_to_loc_format == 0 {
            // Short loca stores offsets halved.
            let o0 = read_u16(data, loca + gid * 2)? as usize * 2;
            let o1 = read_u16(data, loca + (gid + 1) * 2)? as usize * 2;
            (o0, o1)
        } else {
            let o0 = read_u32(data, loca + gid * 4)? as usize;
            let o1 = read_u32(data, loca + (gid + 1) * 4)? as usize;
            (o0, o1)
        };

        if off0 == off1 {
            return None; // Empty glyph (e.g., space)
        }

        Some((self.glyf_offset + off0, off1 - off0))
    }

    pub fn glyph_outline(&self, glyph_id: u16) -> Option<GlyphOutline> {
        let (glyph_off, _glyph_len) = self.glyph_offset(glyph_id)?;
        let data = self.data;

        let num_contours = read_i16(data, glyph_off)?;
        let x_min = read_i16(data, glyph_off + 2)?;
        let y_min = read_i16(data, glyph_off + 4)?;
        let x_max = read_i16(data, glyph_off + 6)?;
        let y_max = read_i16(data, glyph_off + 8)?;

        if num_contours < 0 {
            // Compound glyph
            return self.parse_compound_glyph(glyph_off, x_min, y_min, x_max, y_max);
        }

        let nc = num_contours as usize;
        if nc == 0 {
            return Some(GlyphOutline {
                contours: KVec::new(),
                x_min,
                y_min,
                x_max,
                y_max,
            });
        }

        let mut end_pts: KVec<u16> = KVec::with_capacity(nc).ok()?;
        for i in 0..nc {
            end_pts.push(read_u16(data, glyph_off + 10 + i * 2)?).ok()?;
        }

        let last_point = *end_pts.last()? as usize;
        let num_points = last_point + 1;

        let instr_len_off = glyph_off + 10 + nc * 2;
        let instr_len = read_u16(data, instr_len_off)? as usize;
        let flags_off = instr_len_off + 2 + instr_len;

        let mut flags: KVec<u8> = KVec::with_capacity(num_points).ok()?;
        let mut pos = flags_off;
        while flags.len() < num_points {
            let flag = read_u8(data, pos)?;
            pos += 1;
            flags.push(flag).ok()?;
            if flag & 0x08 != 0 {
                // REPEAT
                let repeat = read_u8(data, pos)? as usize;
                pos += 1;
                for _ in 0..repeat {
                    flags.push(flag).ok()?;
                    if flags.len() >= num_points {
                        break;
                    }
                }
            }
        }

        let mut x_coords: KVec<i16> = KVec::with_capacity(num_points).ok()?;
        let mut x: i16 = 0;
        for &flag in &flags[..num_points] {
            let x_short = flag & 0x02 != 0;
            let x_same_or_positive = flag & 0x10 != 0;

            if x_short {
                let dx = read_u8(data, pos)? as i16;
                pos += 1;
                x += if x_same_or_positive { dx } else { -dx };
            } else if !x_same_or_positive {
                let dx = read_i16(data, pos)?;
                pos += 2;
                x += dx;
            }
            // else: x_same_or_positive && !x_short => same as previous
            x_coords.push(x).ok()?;
        }

        let mut y_coords: KVec<i16> = KVec::with_capacity(num_points).ok()?;
        let mut y: i16 = 0;
        for &flag in &flags[..num_points] {
            let y_short = flag & 0x04 != 0;
            let y_same_or_positive = flag & 0x20 != 0;

            if y_short {
                let dy = read_u8(data, pos)? as i16;
                pos += 1;
                y += if y_same_or_positive { dy } else { -dy };
            } else if !y_same_or_positive {
                let dy = read_i16(data, pos)?;
                pos += 2;
                y += dy;
            }
            y_coords.push(y).ok()?;
        }

        let mut contours: KVec<Contour> = KVec::with_capacity(nc).ok()?;
        let mut start = 0usize;
        for &end in &end_pts {
            let end = end as usize;
            // `endPtsOfContours` comes from the file and is not checked to be
            // monotonic, so a malformed font can put `end` behind `start`, which
            // underflows the capacity below. The whole glyph goes rather than
            // the rest of it: the contours already read are half a shape, and
            // drawing half a shape is worse than drawing the fallback.
            if end >= num_points || end < start {
                return None;
            }
            let mut points: KVec<OutlinePoint> = KVec::with_capacity(end - start + 1).ok()?;
            for i in start..=end {
                points
                    .push(OutlinePoint {
                        x: x_coords[i],
                        y: y_coords[i],
                        on_curve: flags[i] & 0x01 != 0,
                    })
                    .ok()?;
            }
            contours.push(Contour { points }).ok()?;
            start = end + 1;
        }

        Some(GlyphOutline {
            contours,
            x_min,
            y_min,
            x_max,
            y_max,
        })
    }

    /// A compound glyph is built from references to other glyphs.
    fn parse_compound_glyph(
        &self,
        glyph_off: usize,
        x_min: i16,
        y_min: i16,
        x_max: i16,
        y_max: i16,
    ) -> Option<GlyphOutline> {
        let data = self.data;
        let mut pos = glyph_off + 10; // skip header
        let mut all_contours: KVec<Contour> = KVec::new();

        loop {
            let flags = read_u16(data, pos)?;
            let component_glyph_id = read_u16(data, pos + 2)?;
            pos += 4;

            let (dx, dy) = if flags & 0x0001 != 0 {
                // ARG_1_AND_2_ARE_WORDS
                let dx = read_i16(data, pos)?;
                let dy = read_i16(data, pos + 2)?;
                pos += 4;
                (dx as i32, dy as i32)
            } else {
                let dx = read_u8(data, pos)? as i8 as i32;
                let dy = read_u8(data, pos + 1)? as i8 as i32;
                pos += 2;
                (dx, dy)
            };

            if flags & 0x0008 != 0 {
                // WE_HAVE_A_SCALE
                pos += 2;
            } else if flags & 0x0040 != 0 {
                // WE_HAVE_AN_X_AND_Y_SCALE
                pos += 4;
            } else if flags & 0x0080 != 0 {
                // WE_HAVE_A_TWO_BY_TWO
                pos += 8;
            }

            if let Some(component) = self.glyph_outline(component_glyph_id) {
                for contour in component.contours {
                    let translated_points: KVec<OutlinePoint> =
                        KVec::from_iter_fallible(contour.points.iter().map(|p| OutlinePoint {
                            x: (p.x as i32 + dx) as i16,
                            y: (p.y as i32 + dy) as i16,
                            on_curve: p.on_curve,
                        }))
                        .ok()?;
                    all_contours
                        .push(Contour {
                            points: translated_points,
                        })
                        .ok()?;
                }
            }

            if flags & 0x0020 == 0 {
                // MORE_COMPONENTS
                break;
            }
        }

        Some(GlyphOutline {
            contours: all_contours,
            x_min,
            y_min,
            x_max,
            y_max,
        })
    }

    pub fn num_glyphs(&self) -> u16 {
        self.maxp.num_glyphs
    }
}

/// Every pair-adjustment subtable the `kern` feature of the default script's
/// default language system names, extension lookups unwrapped, in lookup
/// order.
fn kern_pair_subtables(data: &[u8]) -> Option<KVec<PairSubtable>> {
    let (gpos, _) = find_table(data, b"GPOS")?;
    if read_u16(data, gpos)? != 1 {
        return None;
    }
    let scripts = gpos + read_u16(data, gpos + 4)? as usize;
    let features = gpos + read_u16(data, gpos + 6)? as usize;
    let lookups = gpos + read_u16(data, gpos + 8)? as usize;
    let lang_sys = default_lang_sys(data, scripts)?;

    let feature_count = read_u16(data, features)?;
    let mut kern_lookups: KVec<u16> = KVec::new();
    for i in 0..read_u16(data, lang_sys + 4)? as usize {
        let feature = read_u16(data, lang_sys + 6 + i * 2)?;
        if feature >= feature_count {
            continue;
        }
        let record = features + 2 + feature as usize * 6;
        if data.get(record..record + 4)? != b"kern" {
            continue;
        }
        let table = features + read_u16(data, record + 4)? as usize;
        for j in 0..read_u16(data, table + 2)? as usize {
            let lookup = read_u16(data, table + 4 + j * 2)?;
            if let Err(at) = kern_lookups.as_slice().binary_search(&lookup) {
                kern_lookups.insert(at, lookup).ok()?;
            }
        }
    }

    let lookup_count = read_u16(data, lookups)?;
    let mut pairs = KVec::new();
    for &lookup in kern_lookups.iter() {
        if lookup >= lookup_count {
            continue;
        }
        let table = lookups + read_u16(data, lookups + 2 + lookup as usize * 2)? as usize;
        let kind = read_u16(data, table)?;
        for k in 0..read_u16(data, table + 4)? as usize {
            let mut offset = table + read_u16(data, table + 6 + k * 2)? as usize;
            let mut sub_kind = kind;
            if kind == 9 {
                // Extension: format 1, the wrapped type, then a 32-bit offset.
                if read_u16(data, offset)? != 1 {
                    continue;
                }
                sub_kind = read_u16(data, offset + 2)?;
                offset += read_u32(data, offset + 4)? as usize;
            }
            if sub_kind == 2 {
                pairs.push(PairSubtable { offset, lookup }).ok()?;
            }
        }
    }
    Some(pairs)
}

/// The default language system of the `DFLT` script, else of `latn`.
fn default_lang_sys(data: &[u8], scripts: usize) -> Option<usize> {
    let count = read_u16(data, scripts)? as usize;
    for tag in [b"DFLT", b"latn"] {
        for i in 0..count {
            let record = scripts + 2 + i * 6;
            if data.get(record..record + 4)? != tag {
                continue;
            }
            let script = scripts + read_u16(data, record + 4)? as usize;
            let lang_sys = read_u16(data, script)? as usize;
            return (lang_sys != 0).then_some(script + lang_sys);
        }
    }
    None
}

/// The first glyph's x-advance adjustment from one PairPos subtable, or
/// `None` when the subtable does not apply to the pair: format 1 applies only
/// to the pairs it lists, format 2 to every pair whose first glyph it covers.
fn pair_x_advance(data: &[u8], sub: usize, left: u16, right: u16) -> Option<i16> {
    let coverage = coverage_index(data, sub + read_u16(data, sub + 2)? as usize, left)?;
    let format1 = read_u16(data, sub + 4)?;
    let format2 = read_u16(data, sub + 6)?;
    let record = value_record_len(format1) + value_record_len(format2);
    match read_u16(data, sub)? {
        1 => {
            if coverage >= read_u16(data, sub + 8)? {
                return None;
            }
            let set = sub + read_u16(data, sub + 10 + coverage as usize * 2)? as usize;
            let stride = 2 + record;
            let (mut lo, mut hi) = (0usize, read_u16(data, set)? as usize);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let at = set + 2 + mid * stride;
                let second = read_u16(data, at)?;
                if second == right {
                    return x_advance(data, at + 2, format1);
                }
                if second < right {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            None
        }
        2 => {
            let class1 = class_of(data, sub + read_u16(data, sub + 8)? as usize, left)?;
            let class2 = class_of(data, sub + read_u16(data, sub + 10)? as usize, right)?;
            let class1_count = read_u16(data, sub + 12)?;
            let class2_count = read_u16(data, sub + 14)?;
            if class1 >= class1_count || class2 >= class2_count {
                return None;
            }
            let index = class1 as usize * class2_count as usize + class2 as usize;
            x_advance(data, sub + 16 + index * record, format1)
        }
        _ => None,
    }
}

/// Bytes in a ValueRecord of `format`: two for every field it carries.
fn value_record_len(format: u16) -> usize {
    (format & 0x00FF).count_ones() as usize * 2
}

/// The XAdvance field of the ValueRecord at `at`, which follows XPlacement and
/// YPlacement when those are present.
fn x_advance(data: &[u8], at: usize, format: u16) -> Option<i16> {
    if format & 0x0004 == 0 {
        return Some(0);
    }
    read_i16(data, at + value_record_len(format & 0x0003))
}

/// `glyph`'s index in a Coverage table, or `None` when it is not covered.
fn coverage_index(data: &[u8], table: usize, glyph: u16) -> Option<u16> {
    let count = read_u16(data, table + 2)? as usize;
    match read_u16(data, table)? {
        1 => {
            let (mut lo, mut hi) = (0usize, count);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let at = read_u16(data, table + 4 + mid * 2)?;
                if at == glyph {
                    return Some(mid as u16);
                }
                if at < glyph {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            None
        }
        2 => {
            let range = find_range(data, table + 4, count, glyph)?;
            let start = read_u16(data, range)?;
            let base = read_u16(data, range + 4)?;
            Some(base.wrapping_add(glyph - start))
        }
        _ => None,
    }
}

/// `glyph`'s class in a ClassDef table; a glyph it does not list is class 0.
fn class_of(data: &[u8], table: usize, glyph: u16) -> Option<u16> {
    match read_u16(data, table)? {
        1 => {
            let start = read_u16(data, table + 2)?;
            let count = read_u16(data, table + 4)?;
            if glyph < start || glyph - start >= count {
                return Some(0);
            }
            read_u16(data, table + 6 + (glyph - start) as usize * 2)
        }
        2 => {
            let count = read_u16(data, table + 2)? as usize;
            match find_range(data, table + 4, count, glyph) {
                Some(range) => read_u16(data, range + 4),
                None => Some(0),
            }
        }
        _ => None,
    }
}

/// The six-byte `(start, end, value)` record among `count` sorted ones at
/// `records` whose range holds `glyph`.
fn find_range(data: &[u8], records: usize, count: usize, glyph: u16) -> Option<usize> {
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let at = records + mid * 6;
        if read_u16(data, at + 2)? < glyph {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let at = records + lo * 6;
    (lo < count && read_u16(data, at)? <= glyph).then_some(at)
}
