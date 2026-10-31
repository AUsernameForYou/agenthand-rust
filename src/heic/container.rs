//! HEIF container parsing and item extraction (M1).
//!
//! Written from ISO/IEC 23008-12 (HEIF / ISOBMFF) only. Scope per
//! SPEC-SUBSET.md §1 as amended at M1:
//!
//! - box walk: 32-bit size, `size==1` largesize, `size==0` to EOF;
//! - `meta` children: `hdlr` (must be `pict`), `pitm`,
//!   `iinf`/`infe` (v2/v3; v0/v1 recorded best-effort), `iloc`,
//!   `idat`, `iprp` (`ipco` + `ipma`), `iref`;
//! - `iloc` construction methods 0 (file offset) and 1 (`idat`);
//!   method 2 is refused loudly at extraction;
//! - `hvcC` fully parsed (parameter-set arrays + length size);
//! - the primary item may be `hvc1` or `grid`; the grid header is
//!   parsed and recorded, but **composition is not implemented**
//!   (pending Daniel's written widening — see MILESTONE-M1.md);
//! - aux items are recorded, never decoded; thumbnails are recorded
//!   as decode targets for M2/M3 diagnostics, never selected as the
//!   primary.

use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug)]
pub enum HeicError {
    /// The file uses a feature outside the subset. Loud by design.
    Refused(String),
    /// The file is structurally broken.
    Malformed(String),
    Io(std::io::Error),
}

impl fmt::Display for HeicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeicError::Refused(m) => write!(f, "refused (out of subset): {m}"),
            HeicError::Malformed(m) => write!(f, "malformed HEIF: {m}"),
            HeicError::Io(e) => write!(f, "io: {e}"),
        }
    }
}
impl std::error::Error for HeicError {}
impl From<std::io::Error> for HeicError {
    fn from(e: std::io::Error) -> Self {
        HeicError::Io(e)
    }
}

type Res<T> = Result<T, HeicError>;

fn malformed<T>(msg: impl Into<String>) -> Res<T> {
    Err(HeicError::Malformed(msg.into()))
}

// ---------------------------------------------------------------------------
// byte cursor
//
// Reader discipline (N24): every accessor on this cursor goes
// through `take`, the one bounds-checked read. There is no
// unchecked or "caller already guaranteed it" fast path, and
// there must never be one — a tired edit must not be able to
// reach for a raw slice here. Callers that hold a BoxRef slice
// the payload through split_at_checked (N25) or re-enter
// through a Cur bounded to the box.

struct Cur<'a> {
    d: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Cur<'a> {
    fn new(d: &'a [u8], start: usize, end: usize) -> Self {
        Cur { d, pos: start, end }
    }
    fn remaining(&self) -> usize {
        self.end.saturating_sub(self.pos)
    }
    fn take(&mut self, n: usize) -> Res<&'a [u8]> {
        if self.remaining() < n {
            return malformed(format!(
                "read of {n} bytes at {} exceeds range end {}",
                self.pos, self.end
            ));
        }
        let s = &self.d[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Res<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Res<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Res<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Res<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    /// Big-endian unsigned integer of `n` bytes (0..=8); n==0 reads 0.
    fn uint(&mut self, n: usize) -> Res<u64> {
        if n == 0 {
            return Ok(0);
        }
        if n > 8 {
            return malformed(format!("integer size {n} > 8"));
        }
        let b = self.take(n)?;
        let mut v: u64 = 0;
        for &x in b {
            v = (v << 8) | x as u64;
        }
        Ok(v)
    }
    fn fourcc(&mut self) -> Res<String> {
        let b = self.take(4)?;
        Ok(b.iter().map(|&c| c as char).collect())
    }
    fn fullbox_version(&mut self) -> Res<u8> {
        let v = self.u8()?;
        self.take(3)?; // flags
        Ok(v)
    }
}

// ---------------------------------------------------------------------------
// box walk

#[derive(Debug, Clone)]
struct BoxRef {
    typ: String,
    start: usize,       // offset of the box header
    size: usize,        // total box size incl. header
    content: usize,     // offset of the box content
    content_len: usize, // content length
}

fn walk_boxes(d: &[u8], start: usize, end: usize) -> Res<Vec<BoxRef>> {
    let mut out = Vec::new();
    let mut off = start;
    while off + 8 <= end {
        let mut c = Cur::new(d, off, end);
        let size32 = c.u32()?;
        let typ = c.fourcc()?;
        let (size, hdr) = match size32 {
            1 => (c.u64()? as usize, 16),
            0 => (end - off, 8),
            s => (s as usize, 8),
        };
        if size < hdr || off + size > end {
            return malformed(format!(
                "box {typ:?} at {off} has size {size} outside range {start}..{end}"
            ));
        }
        out.push(BoxRef {
            typ,
            start: off,
            size,
            content: off + hdr,
            content_len: size - hdr,
        });
        off += size;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// parsed structures

#[derive(Debug, Clone, Serialize)]
pub struct HvcC {
    pub configuration_version: u8,
    pub general_profile_space: u8,
    pub general_tier_flag: bool,
    pub general_profile_idc: u8,
    pub general_level_idc: u8,
    pub chroma_format_idc: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    /// NAL unit length field size in bytes (lengthSizeMinusOne + 1).
    pub length_size: u8,
    pub num_temporal_layers: u8,
    /// (nal_unit_type, unit count, total bytes) per array.
    pub arrays: Vec<(u8, usize, usize)>,
    /// The parameter-set NAL units themselves, keyed by NAL type.
    #[serde(skip)]
    pub parameter_sets: BTreeMap<u8, Vec<Vec<u8>>>,
}

#[derive(Debug, Clone, Serialize)]
pub enum Property {
    HvcC(HvcC),
    Ispe {
        width: u32,
        height: u32,
    },
    Pixi {
        bits_per_channel: Vec<u8>,
    },
    Irot {
        angle: u8,
    },
    ColrNclx {
        primaries: u16,
        transfer: u16,
        matrix: u16,
        full_range: bool,
    },
    ColrOther {
        colour_type: String,
        /// Remaining box payload after the colour type (for `prof`,
        /// the ICC profile bytes themselves; retained from M5 so the
        /// pipeline can carry the profile identity into receipts).
        data: Vec<u8>,
    },
    AuxC {
        aux_type: String,
    },
    Other {
        typ: String,
        size: usize,
    },
}

impl Property {
    pub fn type_name(&self) -> &'static str {
        match self {
            Property::HvcC(_) => "hvcC",
            Property::Ispe { .. } => "ispe",
            Property::Pixi { .. } => "pixi",
            Property::Irot { .. } => "irot",
            Property::ColrNclx { .. } | Property::ColrOther { .. } => "colr",
            Property::AuxC { .. } => "auxC",
            Property::Other { .. } => "other",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemPropertyAssoc {
    pub index: u16, // 1-based into the ipco list; 0 = none
    pub essential: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Extent {
    /// Absolute file offset of the extent's first byte (for
    /// construction method 1, already resolved into the file via the
    /// `idat` box content start).
    pub offset: u64,
    pub length: u64,
    pub index: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reference {
    pub ref_type: String,
    pub to_item_ids: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GridHeader {
    pub rows: u32,
    pub columns: u32,
    pub output_width: u32,
    pub output_height: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub item_id: u32,
    pub item_type: String,
    pub item_name: String,
    pub hidden: bool,
    pub construction_method: u16,
    pub data_reference_index: u16,
    pub extents: Vec<Extent>,
    pub properties: Vec<ItemPropertyAssoc>,
    /// References FROM this item (iref), e.g. a grid's `dimg` list.
    pub references: Vec<Reference>,
}

impl Item {
    pub fn total_extent_bytes(&self) -> u64 {
        self.extents.iter().map(|e| e.length).sum()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HeifFile {
    pub major_brand: String,
    pub minor_version: u32,
    pub compatible_brands: Vec<String>,
    pub primary_item_id: u32,
    pub items: Vec<Item>,
    /// The ipco property list (index 0 = property #1).
    #[serde(skip)]
    pub properties: Vec<Property>,
    /// Content range of the `idat` box, if present.
    pub idat_range: Option<(usize, usize)>,
}

impl HeifFile {
    pub fn item(&self, id: u32) -> Option<&Item> {
        self.items.iter().find(|i| i.item_id == id)
    }
    pub fn primary_item(&self) -> Option<&Item> {
        self.item(self.primary_item_id)
    }
    pub fn census(&self) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for i in &self.items {
            *m.entry(i.item_type.clone()).or_insert(0) += 1;
        }
        m
    }
    pub fn item_ispe(&self, item: &Item) -> Option<(u32, u32)> {
        for a in &item.properties {
            if a.index == 0 {
                continue;
            }
            if let Some(Property::Ispe { width, height }) =
                self.properties.get((a.index - 1) as usize)
            {
                return Some((*width, *height));
            }
        }
        None
    }
    pub fn item_hvcc(&self, item: &Item) -> Option<&HvcC> {
        for a in &item.properties {
            if a.index == 0 {
                continue;
            }
            if let Some(Property::HvcC(h)) = self.properties.get((a.index - 1) as usize) {
                return Some(h);
            }
        }
        None
    }
    pub fn item_aux_type(&self, item: &Item) -> Option<String> {
        for a in &item.properties {
            if a.index == 0 {
                continue;
            }
            if let Some(Property::AuxC { aux_type }) = self.properties.get((a.index - 1) as usize) {
                return Some(aux_type.clone());
            }
        }
        None
    }
    /// The item's `irot` property, if any: the raw 2-bit angle
    /// field. Per ISO/IEC 23008-12 the displayed image is the
    /// stored image rotated `angle * 90` degrees counter-clockwise.
    /// Rotation is a container/display transform — the decoder's
    /// parity metric is on native planes, pre-rotation.
    pub fn item_irot(&self, item: &Item) -> Option<u8> {
        for a in &item.properties {
            if a.index == 0 {
                continue;
            }
            if let Some(Property::Irot { angle }) = self.properties.get((a.index - 1) as usize) {
                return Some(*angle);
            }
        }
        None
    }
    /// Whether the item carries an embedded ICC colour profile
    /// (a `colr` property of type `prof`). Presence is recorded;
    /// no colour conversion is applied or claimed.
    pub fn item_has_prof(&self, item: &Item) -> bool {
        item.properties.iter().any(|a| {
            a.index != 0
                && matches!(
                    self.properties.get((a.index - 1) as usize),
                    Some(Property::ColrOther { colour_type, .. }) if colour_type == "prof"
                )
        })
    }
    /// The item's embedded ICC profile bytes (the payload of its
    /// `colr`/`prof` property), if it carries one.
    pub fn item_icc_profile(&self, item: &Item) -> Option<Vec<u8>> {
        for a in &item.properties {
            if a.index == 0 {
                continue;
            }
            if let Some(Property::ColrOther { colour_type, data }) =
                self.properties.get((a.index - 1) as usize)
            {
                if colour_type == "prof" && !data.is_empty() {
                    return Some(data.clone());
                }
            }
        }
        None
    }
    /// Items the primary references via `dimg` (grid tiles, in order).
    pub fn primary_dimg(&self) -> Vec<u32> {
        match self.primary_item() {
            Some(p) => p
                .references
                .iter()
                .filter(|r| r.ref_type == "dimg")
                .flat_map(|r| r.to_item_ids.clone())
                .collect(),
            None => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// hvcC

fn parse_hvcc(d: &[u8], b: &BoxRef) -> Res<HvcC> {
    let mut c = Cur::new(d, b.content, b.content + b.content_len);
    let configuration_version = c.u8()?;
    if configuration_version != 1 {
        return malformed(format!(
            "hvcC configuration_version {configuration_version} != 1"
        ));
    }
    let b1 = c.u8()?;
    let general_profile_space = b1 >> 6;
    let general_tier_flag = (b1 >> 5) & 1 == 1;
    let general_profile_idc = b1 & 0x1f;
    c.take(4)?; // general_profile_compatibility_flags
    c.take(6)?; // general_constraint_indicator_flags
    let general_level_idc = c.u8()?;
    let mssi = c.u16()?;
    if mssi >> 12 != 0xf {
        return malformed("hvcC min_spatial_segmentation_idc reserved bits not 1111");
    }
    let pt = c.u8()?;
    if pt >> 2 != 0x3f {
        return malformed("hvcC parallelism_type reserved bits not 111111");
    }
    let cf = c.u8()?;
    if cf >> 2 != 0x3f {
        return malformed("hvcC chroma_format_idc reserved bits not 111111");
    }
    let chroma_format_idc = cf & 0x3;
    let bdl = c.u8()?;
    if bdl >> 3 != 0x1f {
        return malformed("hvcC bit_depth_luma_minus8 reserved bits not 11111");
    }
    let bit_depth_luma_minus8 = bdl & 0x7;
    let bdc = c.u8()?;
    if bdc >> 3 != 0x1f {
        return malformed("hvcC bit_depth_chroma_minus8 reserved bits not 11111");
    }
    let bit_depth_chroma_minus8 = bdc & 0x7;
    c.take(2)?; // avg_frame_rate
    let last = c.u8()?;
    let num_temporal_layers = (last >> 3) & 0x7;
    let length_size = (last & 0x3) + 1;
    let num_arrays = c.u8()?;
    let mut arrays = Vec::new();
    let mut parameter_sets: BTreeMap<u8, Vec<Vec<u8>>> = BTreeMap::new();
    for _ in 0..num_arrays {
        let ab = c.u8()?;
        let nal_type = ab & 0x3f;
        let n = c.u16()? as usize;
        let mut total = 0usize;
        let mut units = Vec::new();
        for _ in 0..n {
            let len = c.u16()? as usize;
            let unit = c.take(len)?.to_vec();
            total += len;
            units.push(unit);
        }
        arrays.push((nal_type, n, total));
        parameter_sets.entry(nal_type).or_default().extend(units);
    }
    Ok(HvcC {
        configuration_version,
        general_profile_space,
        general_tier_flag,
        general_profile_idc,
        general_level_idc,
        chroma_format_idc,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        length_size,
        num_temporal_layers,
        arrays,
        parameter_sets,
    })
}

// ---------------------------------------------------------------------------
// ipco property

fn parse_property(d: &[u8], b: &BoxRef) -> Res<Property> {
    let mut c = Cur::new(d, b.content, b.content + b.content_len);
    Ok(match b.typ.as_str() {
        "hvcC" => Property::HvcC(parse_hvcc(d, b)?),
        "ispe" => {
            c.fullbox_version()?;
            let width = c.u32()?;
            let height = c.u32()?;
            Property::Ispe { width, height }
        }
        "pixi" => {
            c.fullbox_version()?;
            let n = c.u8()? as usize;
            let mut bits = Vec::new();
            for _ in 0..n {
                bits.push(c.u8()?);
            }
            Property::Pixi {
                bits_per_channel: bits,
            }
        }
        "irot" => {
            let v = c.u8()?;
            Property::Irot { angle: v & 0x3 }
        }
        "colr" => {
            let colour_type = c.fourcc()?;
            if colour_type == "nclx" {
                let primaries = c.u16()?;
                let transfer = c.u16()?;
                let matrix = c.u16()?;
                let fr = c.u8()?;
                Property::ColrNclx {
                    primaries,
                    transfer,
                    matrix,
                    full_range: fr >> 7 == 1,
                }
            } else {
                let mut rest = Vec::new();
                while c.remaining() > 0 {
                    rest.push(c.u8()?);
                }
                Property::ColrOther {
                    colour_type,
                    data: rest,
                }
            }
        }
        "auxC" => {
            c.fullbox_version()?;
            let mut s = Vec::new();
            while c.remaining() > 0 {
                let ch = c.u8()?;
                if ch == 0 {
                    break;
                }
                s.push(ch);
            }
            Property::AuxC {
                aux_type: String::from_utf8_lossy(&s).into_owned(),
            }
        }
        other => Property::Other {
            typ: other.to_string(),
            size: b.size,
        },
    })
}

// ---------------------------------------------------------------------------
// top-level parse

struct IlocEntry {
    construction_method: u16,
    data_reference_index: u16,
    base_offset: u64,
    extents: Vec<(Option<u64>, u64, u64)>, // (index, offset, length), unresolved
}

pub fn parse(data: &[u8]) -> Res<HeifFile> {
    let top = walk_boxes(data, 0, data.len())?;
    let ftyp = top
        .iter()
        .find(|b| b.typ == "ftyp")
        .ok_or_else(|| HeicError::Malformed("no ftyp box".into()))?;
    let mut fc = Cur::new(data, ftyp.content, ftyp.content + ftyp.content_len);
    let major_brand = fc.fourcc()?;
    let minor_version = fc.u32()?;
    let mut compatible_brands = Vec::new();
    while fc.remaining() >= 4 {
        compatible_brands.push(fc.fourcc()?);
    }

    let meta = top
        .iter()
        .find(|b| b.typ == "meta")
        .ok_or_else(|| HeicError::Malformed("no meta box".into()))?;
    let mut mc = Cur::new(data, meta.content, meta.content + meta.content_len);
    mc.fullbox_version()?; // meta is a FullBox
    let children = walk_boxes(data, mc.pos, meta.content + meta.content_len)?;

    let mut primary_item_id: Option<u32> = None;
    let mut item_types: BTreeMap<u32, (String, String, bool)> = BTreeMap::new();
    let mut iloc: BTreeMap<u32, IlocEntry> = BTreeMap::new();
    let mut properties: Vec<Property> = Vec::new();
    let mut associations: BTreeMap<u32, Vec<ItemPropertyAssoc>> = BTreeMap::new();
    let mut references: BTreeMap<u32, Vec<Reference>> = BTreeMap::new();
    let mut idat_range: Option<(usize, usize)> = None;

    for b in &children {
        let mut c = Cur::new(data, b.content, b.content + b.content_len);
        match b.typ.as_str() {
            "hdlr" => {
                c.fullbox_version()?;
                c.take(4)?; // pre_defined
                let handler = c.fourcc()?;
                if handler != "pict" {
                    return Err(HeicError::Refused(format!(
                        "meta handler {handler:?} is not 'pict'"
                    )));
                }
            }
            "pitm" => {
                let v = c.fullbox_version()?;
                primary_item_id = Some(if v == 0 { c.u16()? as u32 } else { c.u32()? });
            }
            "iinf" => {
                let v = c.fullbox_version()?;
                let _count = if v == 0 { c.u16()? as u32 } else { c.u32()? };
                let infes = walk_boxes(data, c.pos, b.content + b.content_len)?;
                for ib in &infes {
                    if ib.typ != "infe" {
                        continue;
                    }
                    let mut ic = Cur::new(data, ib.content, ib.content + ib.content_len);
                    let iv = ic.fullbox_version()?;
                    // infe flags bit 0 = item_hidden; the flags' low
                    // byte is raw byte 3 of the box content. Read
                    // through .get() (N24): fullbox_version above
                    // already proved 4 content bytes exist, but the
                    // reader rule is that no site indexes raw.
                    let hidden = data.get(ib.content + 3).map(|&b| b & 1 == 1).unwrap_or(false);
                    match iv {
                        2 | 3 => {
                            let item_id = if iv == 2 { ic.u16()? as u32 } else { ic.u32()? };
                            let _protection = ic.u16()?;
                            let item_type = ic.fourcc()?;
                            let mut name = Vec::new();
                            while ic.remaining() > 0 {
                                let ch = ic.u8()?;
                                if ch == 0 {
                                    break;
                                }
                                name.push(ch);
                            }
                            item_types.insert(
                                item_id,
                                (
                                    item_type,
                                    String::from_utf8_lossy(&name).into_owned(),
                                    hidden,
                                ),
                            );
                        }
                        0 | 1 => {
                            // Legacy infe: no item_type field; record id
                            // with an empty type rather than guessing.
                            let item_id = ic.u16()? as u32;
                            item_types
                                .entry(item_id)
                                .or_insert_with(|| (String::new(), String::new(), false));
                        }
                        other => {
                            return malformed(format!("infe version {other} unsupported"));
                        }
                    }
                }
            }
            "iloc" => {
                let v = c.fullbox_version()?;
                let sizes = c.u8()?;
                let offset_size = (sizes >> 4) as usize;
                let length_size = (sizes & 0xf) as usize;
                let sizes2 = c.u8()?;
                let base_offset_size = (sizes2 >> 4) as usize;
                let index_size = if v == 1 || v == 2 {
                    (sizes2 & 0xf) as usize
                } else {
                    0
                };
                let item_count = if v < 2 { c.u16()? as u32 } else { c.u32()? };
                for _ in 0..item_count {
                    let item_id = if v < 2 { c.u16()? as u32 } else { c.u32()? };
                    let construction_method = if v == 1 || v == 2 {
                        c.u16()? & 0xfff
                    } else {
                        0
                    };
                    let data_reference_index = c.u16()?;
                    let base_offset = c.uint(base_offset_size)?;
                    let extent_count = c.u16()? as usize;
                    let mut extents = Vec::new();
                    for _ in 0..extent_count {
                        let index = if index_size > 0 {
                            Some(c.uint(index_size)?)
                        } else {
                            None
                        };
                        let offset = c.uint(offset_size)?;
                        let length = c.uint(length_size)?;
                        extents.push((index, offset, length));
                    }
                    iloc.insert(
                        item_id,
                        IlocEntry {
                            construction_method,
                            data_reference_index,
                            base_offset,
                            extents,
                        },
                    );
                }
            }
            "idat" => {
                idat_range = Some((b.content, b.content_len));
            }
            "iprp" => {
                let subs = walk_boxes(data, b.content, b.content + b.content_len)?;
                for sb in &subs {
                    match sb.typ.as_str() {
                        "ipco" => {
                            let props = walk_boxes(data, sb.content, sb.content + sb.content_len)?;
                            for pb in &props {
                                properties.push(parse_property(data, pb)?);
                            }
                        }
                        "ipma" => {
                            let mut ac = Cur::new(data, sb.content, sb.content + sb.content_len);
                            let v = ac.u8()?;
                            let flags = {
                                let f1 = ac.u8()?;
                                let f2 = ac.u8()?;
                                let f3 = ac.u8()?;
                                ((f1 as u32) << 16) | ((f2 as u32) << 8) | f3 as u32
                            };
                            // entry_count is 32-bit in every ipma
                            // version (verified against libheif
                            // output: version 0 still writes u32);
                            // only the item_ID width follows version.
                            let entry_count = ac.u32()?;
                            for _ in 0..entry_count {
                                let item_id = if v < 1 { ac.u16()? as u32 } else { ac.u32()? };
                                let assoc_count = ac.u8()? as usize;
                                let mut list = Vec::new();
                                for _ in 0..assoc_count {
                                    let (index, essential) = if flags & 1 == 1 {
                                        let raw = ac.u16()?;
                                        (raw & 0x7fff, raw >> 15 == 1)
                                    } else {
                                        let raw = ac.u8()?;
                                        ((raw & 0x7f) as u16, raw >> 7 == 1)
                                    };
                                    list.push(ItemPropertyAssoc { index, essential });
                                }
                                associations.insert(item_id, list);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "iref" => {
                let mut rc = Cur::new(data, b.content, b.content + b.content_len);
                let v = rc.fullbox_version()?;
                let refs = walk_boxes(data, rc.pos, b.content + b.content_len)?;
                for rb in &refs {
                    let mut rcur = Cur::new(data, rb.content, rb.content + rb.content_len);
                    let (from, count) = if v == 0 {
                        (rcur.u16()? as u32, rcur.u16()? as usize)
                    } else {
                        (rcur.u32()?, rcur.u16()? as usize)
                    };
                    let mut tos = Vec::new();
                    for _ in 0..count {
                        tos.push(if v == 0 {
                            rcur.u16()? as u32
                        } else {
                            rcur.u32()?
                        });
                    }
                    references.entry(from).or_default().push(Reference {
                        ref_type: rb.typ.clone(),
                        to_item_ids: tos,
                    });
                }
            }
            _ => {}
        }
    }

    let primary_item_id = primary_item_id
        .ok_or_else(|| HeicError::Malformed("no pitm box: no primary item".into()))?;

    // Resolve extents to absolute file offsets.
    let mut items = Vec::new();
    let mut all_ids: Vec<u32> = item_types.keys().cloned().collect();
    for id in iloc.keys() {
        if !all_ids.contains(id) {
            all_ids.push(*id);
        }
    }
    all_ids.sort_unstable();
    for id in all_ids {
        let (item_type, item_name, hidden) = item_types
            .get(&id)
            .cloned()
            .unwrap_or_else(|| (String::new(), String::new(), false));
        let (construction_method, data_reference_index, extents) = match iloc.get(&id) {
            Some(e) => {
                let mut resolved = Vec::new();
                for (index, offset, length) in &e.extents {
                    let abs = match e.construction_method {
                        0 => e.base_offset + offset,
                        1 => {
                            let (istart, _) = idat_range.ok_or_else(|| {
                                HeicError::Malformed(format!(
                                    "item {id} uses iloc method 1 but the file has no idat box"
                                ))
                            })?;
                            istart as u64 + e.base_offset + offset
                        }
                        _ => {
                            // Method 2 (and anything else): keep the
                            // raw offset un-resolved; extraction
                            // refuses loudly.
                            *offset
                        }
                    };
                    resolved.push(Extent {
                        offset: abs,
                        length: *length,
                        index: *index,
                    });
                }
                (e.construction_method, e.data_reference_index, resolved)
            }
            None => (0, 0, Vec::new()),
        };
        items.push(Item {
            item_id: id,
            item_type,
            item_name,
            hidden,
            construction_method,
            data_reference_index,
            extents,
            properties: associations.get(&id).cloned().unwrap_or_default(),
            references: references.get(&id).cloned().unwrap_or_default(),
        });
    }

    Ok(HeifFile {
        major_brand,
        minor_version,
        compatible_brands,
        primary_item_id,
        items,
        properties,
        idat_range,
    })
}

// ---------------------------------------------------------------------------
// extraction

/// Concatenate an item's extents, in order. Construction method 2
/// (item data in another file) and nonzero data_reference_index are
/// refused loudly — never silently misread.
pub fn extract_item_data(data: &[u8], f: &HeifFile, item_id: u32) -> Res<Vec<u8>> {
    let item = f
        .item(item_id)
        .ok_or_else(|| HeicError::Malformed(format!("no item with id {item_id}")))?;
    extract_item_data_for(data, item)
}

pub fn extract_item_data_for(data: &[u8], item: &Item) -> Res<Vec<u8>> {
    if item.construction_method == 2 {
        return Err(HeicError::Refused(format!(
            "item {} uses iloc construction_method 2 (item offset); out of subset",
            item.item_id
        )));
    }
    if item.construction_method > 1 {
        return Err(HeicError::Refused(format!(
            "item {} uses unknown iloc construction_method {}",
            item.item_id, item.construction_method
        )));
    }
    if item.data_reference_index != 0 {
        return Err(HeicError::Refused(format!(
            "item {} has data_reference_index {} (external data reference); out of subset",
            item.item_id, item.data_reference_index
        )));
    }
    // Capacity hint only — never size it from the file's declared
    // extent lengths unclamped: a crafted iloc can declare a huge
    // total and abort the process on this allocation before the
    // per-extent bounds checks below ever run (allocation failure
    // is an abort, not a catchable panic). Every extent's bytes
    // come out of `data`, so the input length bounds a useful
    // pre-size; overlapping extents still grow the Vec normally.
    let hint = item
        .extents
        .iter()
        .fold(0u64, |acc, e| acc.saturating_add(e.length));
    let mut out = Vec::with_capacity((hint as usize).min(data.len()));
    for e in &item.extents {
        let start = e.offset as usize;
        let end = start
            .checked_add(e.length as usize)
            .filter(|&en| en <= data.len())
            .ok_or_else(|| {
                HeicError::Malformed(format!(
                    "item {} extent at {start}+{} exceeds file size {}",
                    item.item_id,
                    e.length,
                    data.len()
                ))
            })?;
        out.extend_from_slice(&data[start..end]);
    }
    Ok(out)
}

/// Parse the ImageGrid header at the start of a grid item's data
/// (ISO/IEC 23008-12 §6.6.2). Parsing only — composition of the
/// canvas from the tile items is NOT implemented (pending scope
/// widening; see MILESTONE-M1.md).
pub fn parse_grid_header(item_data: &[u8]) -> Res<GridHeader> {
    let mut c = Cur::new(item_data, 0, item_data.len());
    let version = c.u8()?;
    if version != 0 {
        return malformed(format!("grid version {version} != 0"));
    }
    let flags = c.u8()?;
    let rows = c.u8()? as u32 + 1;
    let columns = c.u8()? as u32 + 1;
    // flags bit 0 selects the output-size field width: 32-bit when
    // set, 16-bit when clear (Apple writes 16-bit: an 8-byte item).
    let (output_width, output_height) = if flags & 1 == 1 {
        (c.u32()?, c.u32()?)
    } else {
        (c.u16()? as u32, c.u16()? as u32)
    };
    Ok(GridHeader {
        rows,
        columns,
        output_width,
        output_height,
    })
}

/// Convert an item bitstream to Annex B form: the hvcC
/// parameter-set arrays first (in stored order), then the item's
/// own NAL units, each prefixed with a 4-byte start code. This is
/// the form external oracles/traces (ffmpeg raw decode, HM) consume;
/// it is also the strongest cheap check that extraction selected
/// the right bytes — an Annex B stream built from OUR extraction
/// must decode to the same planes as the oracle demuxing the HEIF
/// itself.
pub fn to_annexb(hvcc: &HvcC, item_data: &[u8]) -> Res<Vec<u8>> {
    let mut out = Vec::new();
    let mut types_in_order: Vec<u8> = hvcc.arrays.iter().map(|(t, _, _)| *t).collect();
    types_in_order.dedup();
    for t in &types_in_order {
        if let Some(units) = hvcc.parameter_sets.get(t) {
            for u in units {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(u);
            }
        }
    }
    let mut pos = 0usize;
    while pos < item_data.len() {
        if pos + hvcc.length_size as usize > item_data.len() {
            return malformed("truncated NAL length prefix (annexb)");
        }
        let mut len: usize = 0;
        for i in 0..hvcc.length_size as usize {
            len = (len << 8) | item_data[pos + i] as usize;
        }
        pos += hvcc.length_size as usize;
        if len == 0 {
            return malformed("NAL length out of range (annexb)");
        }
        // The payload split is itself the checked operation
        // (N25): split_at_checked returns None on a lying length
        // instead of slicing on arithmetic done lines away.
        let Some(unit) = item_data
            .split_at_checked(pos)
            .and_then(|(_, tail)| tail.split_at_checked(len).map(|(unit, _)| unit))
        else {
            return malformed("NAL length out of range (annexb)");
        };
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(unit);
        pos += len;
    }
    Ok(out)
}

/// Split a length-prefixed HEVC item bitstream (hvcC length size)
/// into its NAL units: (nal_unit_type, unit bytes incl. header).
pub fn walk_nals(item_data: &[u8], length_size: u8) -> Res<Vec<(u8, usize)>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < item_data.len() {
        if pos + length_size as usize > item_data.len() {
            return malformed("truncated NAL length prefix");
        }
        let mut len: usize = 0;
        for i in 0..length_size as usize {
            len = (len << 8) | item_data[pos + i] as usize;
        }
        pos += length_size as usize;
        if pos + len > item_data.len() || len < 2 {
            return malformed(format!(
                "NAL unit at {pos} claims {len} bytes, {} available",
                item_data.len() - pos
            ));
        }
        let nal_type = (item_data[pos] >> 1) & 0x3f;
        out.push((nal_type, len));
        pos += len;
    }
    Ok(out)
}

/// Split a length-prefixed HEVC item bitstream into borrowed NAL
/// units: (nal_unit_type, unit bytes including the 2-byte header).
pub fn split_nals<'a>(item_data: &'a [u8], length_size: u8) -> Res<Vec<(u8, &'a [u8])>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < item_data.len() {
        if pos + length_size as usize > item_data.len() {
            return malformed("truncated NAL length prefix");
        }
        let mut len: usize = 0;
        for i in 0..length_size as usize {
            len = (len << 8) | item_data[pos + i] as usize;
        }
        pos += length_size as usize;
        if len < 2 {
            return malformed(format!(
                "NAL unit at {pos} claims {len} bytes, {} available",
                item_data.len() - pos
            ));
        }
        // The payload split is itself the checked operation
        // (N25), as in to_annexb: None on a lying length.
        let Some(unit) = item_data
            .split_at_checked(pos)
            .and_then(|(_, tail)| tail.split_at_checked(len).map(|(unit, _)| unit))
        else {
            return malformed(format!(
                "NAL unit at {pos} claims {len} bytes, {} available",
                item_data.len() - pos
            ));
        };
        let nal_type = (unit[0] >> 1) & 0x3f;
        out.push((nal_type, unit));
        pos += len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_(typ: &str, content: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&((content.len() + 8) as u32).to_be_bytes());
        v.extend_from_slice(typ.as_bytes());
        v.extend_from_slice(content);
        v
    }
    fn fullbox(typ: &str, version: u8, content: &[u8]) -> Vec<u8> {
        let mut c = vec![version, 0, 0, 0];
        c.extend_from_slice(content);
        box_(typ, &c)
    }

    /// Minimal synthetic HEIF: one hvc1 primary, item data in idat
    /// (construction method 1), plus a thumbnail item (method 0) to
    /// make sure the primary is not confused with it. Assembled in
    /// two passes because item 2's extent offset depends on the
    /// total header size (which is value-independent: the offset
    /// field is a fixed 4 bytes).
    fn synthetic_heif() -> (Vec<u8>, Vec<u8>) {
        let (file0, _) = assemble_synthetic(0);
        let item2_off = (file0.len() - 4) as u32;
        assemble_synthetic(item2_off)
    }

    fn assemble_synthetic(item2_offset: u32) -> (Vec<u8>, Vec<u8>) {
        // fake item payload: one length-prefixed NAL (type 32, 3 bytes)
        let payload: Vec<u8> = vec![0, 0, 0, 3, 0x40, 0x01, 0x0c];
        let mut hvcc = vec![1u8, 0x60];
        hvcc.extend_from_slice(&[0xff; 4]); // compatibility flags
        hvcc.extend_from_slice(&[0xff; 6]); // constraint indicator flags
        hvcc.push(150); // level_idc
        hvcc.extend_from_slice(&[0xff, 0xfc]); // min_spatial_segmentation
        hvcc.push(0xfc); // parallelism
        hvcc.push(0xfd); // chroma_format 1
        hvcc.push(0xf8); // bit_depth_luma_minus8 0
        hvcc.push(0xf8); // bit_depth_chroma_minus8 0
        hvcc.extend_from_slice(&[0, 0]); // avg_frame_rate
        hvcc.push(0b0000_1111); // 1 temporal layer, lengthSizeMinusOne=3 -> len 4
        hvcc.push(1); // one array
        hvcc.push(0x20); // type 32
        hvcc.extend_from_slice(&1u16.to_be_bytes());
        hvcc.extend_from_slice(&3u16.to_be_bytes());
        hvcc.extend_from_slice(&[0x40, 0x01, 0x0c]);

        let mut ispe = vec![0, 0, 0, 0];
        ispe.extend_from_slice(&64u32.to_be_bytes());
        ispe.extend_from_slice(&48u32.to_be_bytes());

        let ipco = box_(
            "ipco",
            &[box_("hvcC", &hvcc), fullbox("ispe", 0, &ispe[4..])].concat(),
        );
        // ipma v1, flags 0 (7-bit indices): item 1 -> props 1 (ess), 2
        let mut ipma_c = Vec::new();
        ipma_c.extend_from_slice(&1u32.to_be_bytes());
        ipma_c.extend_from_slice(&1u32.to_be_bytes());
        ipma_c.push(2);
        ipma_c.push(0x81); // index 1, essential
        ipma_c.push(2); // index 2
        let ipma = fullbox("ipma", 1, &ipma_c);
        let iprp = box_("iprp", &[ipco, ipma].concat());

        let hdlr = fullbox(
            "hdlr",
            0,
            &[&[0u8; 4] as &[u8], b"pict", &[0u8; 12]].concat(),
        );
        let pitm = fullbox("pitm", 1, &1u32.to_be_bytes());
        let infe = {
            let mut c = Vec::new();
            c.extend_from_slice(&1u16.to_be_bytes());
            c.extend_from_slice(&0u16.to_be_bytes());
            c.extend_from_slice(b"hvc1");
            c.push(0);
            fullbox("infe", 2, &c)
        };
        let infe2 = {
            let mut c = Vec::new();
            c.extend_from_slice(&2u16.to_be_bytes());
            c.extend_from_slice(&0u16.to_be_bytes());
            c.extend_from_slice(b"hvc1");
            c.push(0);
            fullbox("infe", 2, &c)
        };
        let mut iinf_c = Vec::new();
        iinf_c.extend_from_slice(&2u16.to_be_bytes());
        iinf_c.extend_from_slice(&infe);
        iinf_c.extend_from_slice(&infe2);
        let iinf = fullbox("iinf", 0, &iinf_c);

        // iloc v1: item1 method 1 offset 0 len = payload; item2 (thumbnail)
        // method 0 pointing at the mdat payload below.
        let mut iloc_c = Vec::new();
        iloc_c.push(0x44); // offset_size 4, length_size 4
        iloc_c.push(0x00); // base_offset_size 0, index_size 0
        iloc_c.extend_from_slice(&2u16.to_be_bytes());
        iloc_c.extend_from_slice(&1u16.to_be_bytes());
        iloc_c.extend_from_slice(&1u16.to_be_bytes()); // method 1
        iloc_c.extend_from_slice(&0u16.to_be_bytes()); // dri
        iloc_c.extend_from_slice(&1u16.to_be_bytes()); // 1 extent
        iloc_c.extend_from_slice(&0u32.to_be_bytes());
        iloc_c.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        iloc_c.extend_from_slice(&2u16.to_be_bytes());
        iloc_c.extend_from_slice(&0u16.to_be_bytes()); // method 0
        iloc_c.extend_from_slice(&0u16.to_be_bytes());
        iloc_c.extend_from_slice(&1u16.to_be_bytes());
        iloc_c.extend_from_slice(&item2_offset.to_be_bytes());
        iloc_c.extend_from_slice(&4u32.to_be_bytes());
        let iloc = fullbox("iloc", 1, &iloc_c);

        let idat = box_("idat", &payload);
        let meta = fullbox("meta", 0, &[hdlr, pitm, iinf, iprp, idat, iloc].concat());
        let ftyp = box_("ftyp", &[b"heic" as &[u8], &[0u8; 4], b"mif1heic"].concat());
        let mut file = [ftyp, meta].concat();
        // item 2's method-0 payload: 4 raw bytes at the very end of
        // the file, at the offset the caller computed.
        file.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        (file, payload)
    }

    #[test]
    fn parses_synthetic_and_extracts_both_methods() {
        let (file, payload) = synthetic_heif();
        let f = parse(&file).expect("parse");
        assert_eq!(f.major_brand, "heic");
        assert_eq!(f.primary_item_id, 1);
        let p = f.primary_item().unwrap();
        assert_eq!(p.item_type, "hvc1");
        assert_eq!(f.item_ispe(p), Some((64, 48)));
        let h = f.item_hvcc(p).expect("hvcc");
        assert_eq!(h.length_size, 4);
        assert_eq!(h.chroma_format_idc, 1);
        let data = extract_item_data(&file, &f, 1).expect("extract method 1");
        assert_eq!(data, payload);
        let nals = walk_nals(&data, h.length_size).expect("nals");
        assert_eq!(nals, vec![(32, 3)]);
        let d2 = extract_item_data(&file, &f, 2).expect("extract method 0");
        assert_eq!(d2, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn method2_is_refused_loudly() {
        let mut item = Item {
            item_id: 9,
            item_type: "hvc1".into(),
            item_name: String::new(),
            hidden: false,
            construction_method: 2,
            data_reference_index: 0,
            extents: vec![Extent {
                offset: 0,
                length: 4,
                index: None,
            }],
            properties: Vec::new(),
            references: Vec::new(),
        };
        match extract_item_data_for(b"abcdefgh", &item) {
            Err(HeicError::Refused(m)) => assert!(m.contains("construction_method 2")),
            other => panic!("expected refusal, got {other:?}"),
        }
        item.construction_method = 0;
        item.data_reference_index = 3;
        match extract_item_data_for(b"abcdefgh", &item) {
            Err(HeicError::Refused(m)) => assert!(m.contains("data_reference_index")),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn grid_header_parses() {
        // 16-bit output fields (flags bit 0 clear) — Apple's layout,
        // byte-for-byte the shape of IMG_1340's primary grid item.
        let mut g = vec![0u8, 0, 4, 8]; // v0, flags 0, rows-1=4, cols-1=8
        g.extend_from_slice(&5712u16.to_be_bytes());
        g.extend_from_slice(&4284u16.to_be_bytes());
        let h = parse_grid_header(&g).expect("grid header");
        assert_eq!((h.rows, h.columns), (5, 9));
        assert_eq!((h.output_width, h.output_height), (5712, 4284));
        // 32-bit output fields (flags bit 0 set).
        let mut g2 = vec![0u8, 1, 0, 0];
        g2.extend_from_slice(&70000u32.to_be_bytes());
        g2.extend_from_slice(&50000u32.to_be_bytes());
        let h2 = parse_grid_header(&g2).expect("grid header 32-bit");
        assert_eq!((h2.rows, h2.columns), (1, 1));
        assert_eq!((h2.output_width, h2.output_height), (70000, 50000));
    }
}
