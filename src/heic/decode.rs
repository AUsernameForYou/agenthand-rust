//! Full-file HEIC decode for the pipeline (rust-100 M5): container
//! primary item (grid or single `hvc1` picture) → decoded native
//! planes in DISPLAY orientation → 8-bit RGB.
//!
//! This is the integration surface the MCP server consumes. It is a
//! thin composition of the milestone parts: `container` (item
//! graph), `syntax` (retained slice walk), `recon` (reconstruction
//! with deblocking + SAO), and `grid` (tile composition + rotation).
//! Nothing here falls back to an external decoder: a file outside
//! the SPEC-SUBSET refuses with the subset's own message.
//!
//! Orientation: the stored planes are rotated by the primary
//! item's `irot` (the container/display transform), so the output
//! is the picture as a viewer shows it. The parity gates stay on
//! native pre-rotation planes; this module is the product surface,
//! not the measurement surface.
//!
//! Colour: planes convert to RGB with the matrix + range the
//! stream itself signals in its SPS VUI (BT.601/709/2020, limited
//! or full). Chroma is bilinearly upsampled from 4:2:0. The
//! embedded ICC profile is carried alongside, uninterpreted, so
//! receipts can pin its identity exactly as the PNG/JPEG paths do.

use std::collections::BTreeMap;

use super::container::{self, HeicError, HeifFile, Item};
use super::grid::{self, PicturePlanes};
use super::params::{self, Pps, Sps, VuiInfo};
use super::{bitreader, recon, syntax};

type Res<T> = Result<T, HeicError>;

fn refused<T>(msg: impl Into<String>) -> Res<T> {
    Err(HeicError::Refused(msg.into()))
}

/// Decoder safety ceiling on any single picture's coded area
/// (M5 untrusted-input gate). A header claiming more refuses
/// before a plane is allocated. 2^28 samples sits above the
/// largest phone picture in the corpus (the 200 MP Samsung grid
/// composes to 199,724,160) and far below allocation-bomb
/// territory; the product's own ingest cap (40 MP) is separate
/// and stricter.
pub const MAX_CANVAS_PIXELS: u64 = 268_435_456;

fn check_area(area: u64, kind: &str) -> Res<()> {
    check_area_within(area, kind, MAX_CANVAS_PIXELS)
}

fn check_area_within(area: u64, kind: &str, limit: u64) -> Res<()> {
    if area > limit {
        // Name the binding limit honestly: the decoder's own
        // ceiling when that is what binds, the caller's pixel
        // budget when a tighter budget was passed in.
        let what = if limit < MAX_CANVAS_PIXELS {
            "pixel budget"
        } else {
            "decoder safety ceiling"
        };
        return refused(format!(
            "{kind} canvas {area} samples exceeds the {what} {limit}"
        ));
    }
    Ok(())
}

fn check_canvas(sps: &Sps) -> Res<()> {
    check_area(sps.width as u64 * sps.height as u64, "picture")
}

/// Cumulative decode-WORK budget for one file, separate from the
/// per-picture pixel budget (Gauntlet GPT lane, 2026-10-31, after
/// libheif GHSA-48p4-gj45-w9v6). The pixel budget caps each
/// picture's conformance window and the grid's output, but nothing
/// capped the TOTAL samples decoded across a grid's tiles: a small
/// file can alias one tiny all-skip tile bitstream into thousands
/// of `dimg` references — or declare many tiles whose SPS canvases
/// each sit under the per-picture ceiling — and force decode work
/// orders of magnitude beyond the output size. Every decoded
/// picture charges its full SPS canvas here (the recon walk covers
/// the canvas, not the crop window), via `checked_sub`, so the
/// failure point is this refusal, never a wrap.
struct WorkBudget {
    limit: u64,
    charged: u64,
}

impl WorkBudget {
    fn new(pixel_limit: u64) -> Self {
        // Headroom over the output budget for honest files: a
        // grid's tile canvases sum to roughly its canvas, and one
        // picture's canvas can exceed its crop window (conformance
        // offsets, edge CTBs). 4x the pixel budget is generous for
        // real photos and still bounds a hostile file to a
        // seconds-scale decode.
        WorkBudget {
            limit: pixel_limit.saturating_mul(4),
            charged: 0,
        }
    }

    fn charge(&mut self, samples: u64) -> Res<()> {
        match self.charged.checked_add(samples) {
            Some(total) if total <= self.limit => {
                self.charged = total;
                Ok(())
            }
            _ => refused(format!(
                "cumulative decode work budget exhausted: {} samples already decoded, {samples} more exceeds the work limit {} (pixel budget x4)",
                self.charged, self.limit
            )),
        }
    }

    /// N27 (Gauntlet fine-tooth, 2026-10-31): refuse at header time
    /// when the work a file DECLARES already exceeds the whole
    /// budget, before any of it is spent. The incremental `charge`
    /// calls during decode remain the enforcer for anything the
    /// headers understate; this check only makes a grid whose own
    /// declared tile canvases bust the budget fail fast and cheap.
    fn check_declared(&self, declared: u64) -> Res<()> {
        if declared > self.limit {
            return refused(format!(
                "declared decode work {declared} samples exceeds the cumulative work budget {} (pixel budget x4), refused from header dimensions before any tile is decoded",
                self.limit
            ));
        }
        Ok(())
    }
}

/// True when the bytes carry an `ftyp` box whose major or
/// compatible brands name a HEIF-family file. Content sniffing,
/// never the file name.
pub fn is_heic(data: &[u8]) -> bool {
    if data.len() < 16 || &data[4..8] != b"ftyp" {
        return false;
    }
    const BRANDS: &[&[u8; 4]] = &[
        b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"mif2",
        b"msf1", b"avif", b"avis",
    ];
    let brand = |b: &[u8]| BRANDS.iter().any(|x| *x == b);
    if brand(&data[8..12]) {
        return true;
    }
    // Compatible brands follow the major/minor pair.
    let box_end = {
        let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if size >= 16 && size <= data.len() {
            size
        } else {
            data.len()
        }
    };
    data[16..box_end].chunks_exact(4).any(brand)
}

/// One fully decoded HEIC picture, display orientation.
#[derive(Debug, Clone)]
pub struct HeicImage {
    pub planes: PicturePlanes,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    /// Colour description signalled by the decoded stream's SPS.
    pub vui: VuiInfo,
    /// Embedded ICC profile bytes (the `colr`/`prof` payload), if
    /// the primary item or any of its tiles carries one.
    pub icc_profile: Option<Vec<u8>>,
    /// Tiles composed for a grid primary; 0 for a single picture.
    pub grid_tiles: usize,
}

/// Decode one `hvc1` item's picture (filters on) plus its SPS.
/// Ported from the M3.5 CLI driver into the library so the server
/// and the CLI share one decode path.
fn decode_picture_item(
    data: &[u8],
    f: &HeifFile,
    item: &Item,
    limit: u64,
    work: &mut WorkBudget,
) -> Result<(Sps, recon::Picture), HeicError> {
    let ep = bitreader::remove_emulation_prevention;
    let Some(hvcc) = f.item_hvcc(item) else {
        return refused("item has no hvcC (not a coded HEVC picture)");
    };
    let mut sps_map: BTreeMap<u32, Sps> = BTreeMap::new();
    let mut pps_map: BTreeMap<u32, Pps> = BTreeMap::new();
    let mut sps_refusal: Option<String> = None;
    fn seed(
        nal_type: u8,
        nal: &[u8],
        ep: fn(&[u8]) -> Vec<u8>,
        sps_map: &mut BTreeMap<u32, Sps>,
        pps_map: &mut BTreeMap<u32, Pps>,
        sps_refusal: &mut Option<String>,
    ) {
        let rbsp = ep(nal);
        match nal_type {
            params::NAL_SPS => match params::parse_sps(&rbsp) {
                Ok(s) => {
                    sps_map.insert(s.id, s);
                }
                Err(e) => {
                    if sps_refusal.is_none() {
                        *sps_refusal = Some(format!("{e:?}"));
                    }
                }
            },
            params::NAL_PPS => {
                if let Ok(p) = params::parse_pps(&rbsp) {
                    pps_map.insert(p.id, p);
                }
            }
            _ => {}
        }
    }
    for (t, units) in &hvcc.parameter_sets {
        for u in units {
            seed(*t, u, ep, &mut sps_map, &mut pps_map, &mut sps_refusal);
        }
    }
    let bytes = container::extract_item_data_for(data, item)
        .map_err(|e| HeicError::Malformed(format!("extract: {e}")))?;
    let nals = container::split_nals(&bytes, hvcc.length_size)
        .map_err(|e| HeicError::Malformed(format!("split: {e}")))?;
    for (t, nal) in &nals {
        if *t == params::NAL_SPS || *t == params::NAL_PPS || *t == params::NAL_VPS {
            seed(*t, nal, ep, &mut sps_map, &mut pps_map, &mut sps_refusal);
        }
    }
    if sps_map.is_empty() {
        if let Some(r) = sps_refusal {
            return refused(format!("SPS refused: {r}"));
        }
    }
    for sps in sps_map.values() {
        check_canvas(sps)?;
        // Declared-size budget (M5 fix lane): the SPS conformance
        // window is the picture the caller receives; refuse a
        // window over the caller's pixel budget here, at header
        // time, before the slice walk or any plane allocation.
        let declared = sps.crop_width() as u64 * sps.crop_height() as u64;
        if declared > limit {
            return refused(format!(
                "picture conformance window {declared} samples exceeds the pixel budget {limit} (refused from header dimensions, before decode)"
            ));
        }
    }
    for (t, nal) in &nals {
        if *t > 21 {
            continue;
        }
        let (rbsp, raw_of_rbsp, nal_drops) = bitreader::map_rbsp(nal);
        let mut found = None;
        for sps in sps_map.values() {
            let pps_lookup = |id: u32| pps_map.get(&id).cloned();
            if let Ok((sh, pps)) = params::parse_slice_header(&rbsp, *t, sps, &pps_lookup) {
                if pps.sps_id == sps.id {
                    found = Some((sps.clone(), sh, pps));
                    break;
                }
            }
        }
        let Some((sps, sh, pps)) = found else {
            return refused("slice header parse failed (no in-subset parameter set matched)");
        };
        // Charge the decode work before the walk: the recon walk
        // covers the full SPS canvas, so the canvas is the honest
        // work measure (not the smaller crop window).
        work.charge(sps.width as u64 * sps.height as u64)?;
        // Tile-grid sanity (M5 untrusted-input hardening): a PPS
        // claiming more tile columns/rows than the picture has CTB
        // columns/rows is invalid, and its starts vectors are
        // reserved from those counts — refuse before they size
        // anything.
        if pps.tiles_enabled
            && ((pps.num_tile_columns_minus1 as u64 + 1) > sps.width_in_ctbs() as u64
                || (pps.num_tile_rows_minus1 as u64 + 1) > sps.height_in_ctbs() as u64)
        {
            return refused("tile grid exceeds the picture CTB grid");
        }
        let start_byte = sh.cabac_start_bit / 8;
        if start_byte >= rbsp.len() {
            return refused("cabac start beyond NAL");
        }
        let slice_data = &rbsp[start_byte..];
        let raw_start = raw_of_rbsp[start_byte];
        let slice_drops: Vec<usize> = nal_drops
            .iter()
            .filter(|&&d| d >= raw_start)
            .map(|&d| d - raw_start)
            .collect();
        let outcome = syntax::walk_slice_retained(&sps, &pps, &sh, slice_data, &slice_drops);
        let retained = outcome
            .retained
            .ok_or_else(|| HeicError::Refused("slice walk produced no retained slice".into()))?;
        let pic = recon::decode_picture(&sps, &pps, &sh, &retained, true);
        return Ok((sps, pic));
    }
    refused("no VCL NAL in item")
}

/// Conformance-crop a decoded picture to its PicturePlanes (the
/// M3.5 CLI's crop, shared verbatim).
fn crop_planes(sps: &Sps, pic: &recon::Picture) -> PicturePlanes {
    let grab = |plane: &[u16], fw: usize, cw: usize, ch: usize| -> Vec<u16> {
        let mut out = Vec::with_capacity(cw * ch);
        for row in 0..ch {
            let start = row * fw;
            out.extend_from_slice(&plane[start..start + cw]);
        }
        out
    };
    let (lw, lh) = (pic.width as usize, pic.height as usize);
    PicturePlanes {
        width: lw,
        height: lh,
        y: grab(&pic.y, sps.width as usize, lw, lh),
        cb: grab(
            &pic.cb,
            (sps.width / 2) as usize,
            (sps.crop_width() / 2) as usize,
            (sps.crop_height() / 2) as usize,
        ),
        cr: grab(
            &pic.cr,
            (sps.width / 2) as usize,
            (sps.crop_width() / 2) as usize,
            (sps.crop_height() / 2) as usize,
        ),
    }
}

/// Decode a whole HEIC/HEIF file's primary picture with our
/// decoder. Grid primaries compose their tiles; single `hvc1`
/// primaries decode directly. Anything outside the subset refuses
/// with the subset's message.
pub fn decode_heic(data: &[u8]) -> Res<HeicImage> {
    decode_heic_with_budget(data, MAX_CANVAS_PIXELS)
}

/// `decode_heic` with a caller pixel budget (M5 fix lane). The
/// budget is enforced from DECLARED dimensions, before the full
/// picture is allocated or decoded: a grid primary is measured by
/// its grid-header canvas (and its `ispe`, when present), a single
/// picture by its SPS conformance window (checked inside
/// `decode_picture_item`). On top of that, a cumulative work
/// budget (`WorkBudget`) charges every decoded picture's SPS
/// canvas, so a grid cannot multiply decode work past 4x the
/// pixel budget by repeating or aliasing tiles. The decoder's own
/// safety ceiling (`MAX_CANVAS_PIXELS`) always applies on top:
/// the effective limit is the smaller of the two. The shipped
/// server path passes the product ingest cap here, so an over-cap
/// upload refuses in header time instead of after a full decode.
///
/// N26 (Gauntlet fine-tooth, 2026-10-31): the budget ledger runs
/// through the receipt step by construction. Every buffer that
/// outlives decode is output-sized: the composed/rotated planes
/// (the grid canvas and the single-picture window were both
/// checked against `limit` above, before allocation), the RGBA
/// expansion in `image.rs` (same checked canvas, 4 bytes/px), and
/// nothing else — receipt hashing streams plane rows and holds no
/// second full-size copy. So the composite a receipt is issued
/// over was charged when its canvas passed the header checks;
/// there is no uncharged post-decode allocation to thread a
/// charge through.
pub fn decode_heic_with_budget(data: &[u8], max_pixels: u64) -> Res<HeicImage> {
    let limit = max_pixels.min(MAX_CANVAS_PIXELS);
    let mut work = WorkBudget::new(limit);
    let f = container::parse(data)?;
    let Some(primary) = f.primary_item() else {
        return refused("file has no primary item");
    };
    let primary = primary.clone();
    let (mut planes, bd_y, bd_c, vui, mut icc, grid_tiles) = match primary.item_type.as_str() {
        "grid" => {
            let gdata = container::extract_item_data_for(data, &primary)?;
            let header = container::parse_grid_header(&gdata)?;
            check_area_within(
                header.output_width as u64 * header.output_height as u64,
                "grid",
                limit,
            )?;
            if let Some((ow, oh)) = f.item_ispe(&primary) {
                check_area_within(ow as u64 * oh as u64, "grid", limit)?;
            }
            let tile_ids: Vec<u32> = primary
                .references
                .iter()
                .filter(|r| r.ref_type == "dimg")
                .flat_map(|r| r.to_item_ids.clone())
                .collect();
            if tile_ids.is_empty() {
                return refused("grid primary references no tiles");
            }
            // Refuse a tile-count mismatch BEFORE decoding any
            // tile: compose_grid would refuse it anyway, but only
            // after every referenced tile was decoded — wasted
            // work a hostile dimg list could inflate at will.
            let expect = (header.rows as usize) * (header.columns as usize);
            if tile_ids.len() != expect {
                return refused(format!(
                    "grid {}x{} needs {expect} tile pictures, got {}",
                    header.rows,
                    header.columns,
                    tile_ids.len()
                ));
            }
            // N27: reserve the summed tile canvases at header time.
            // Each tile's ispe is its declared canvas; if the sum
            // alone busts the cumulative work budget, refuse here,
            // before decoding tile one. Tiles without ispe are
            // skipped: their SPS canvas is charged incrementally
            // during decode, which stays the binding enforcement.
            let declared_work: u64 = tile_ids
                .iter()
                .filter_map(|tid| f.item(*tid))
                .filter_map(|t| f.item_ispe(t))
                .map(|(w, h)| (w as u64).saturating_mul(h as u64))
                .fold(0u64, u64::saturating_add);
            work.check_declared(declared_work)?;
            let mut tiles = Vec::with_capacity(tile_ids.len());
            let mut first: Option<(u32, u32, VuiInfo)> = None;
            let mut icc = f.item_icc_profile(&primary);
            for tid in &tile_ids {
                let Some(titem) = f.item(*tid) else {
                    return refused(format!("dimg tile {tid} not in file"));
                };
                let titem = titem.clone();
                if icc.is_none() {
                    icc = f.item_icc_profile(&titem);
                }
                let (sps, pic) = decode_picture_item(data, &f, &titem, limit, &mut work)?;
                if first.is_none() {
                    first = Some((pic.bit_depth_luma, pic.bit_depth_chroma, sps.vui));
                }
                tiles.push(crop_planes(&sps, &pic));
            }
            let (bd_y, bd_c, vui) = first.unwrap_or((8, 8, VuiInfo::default()));
            let composed = grid::compose_grid(&tiles, &header)?;
            (composed, bd_y, bd_c, vui, icc, tiles.len())
        }
        "hvc1" => {
            let (sps, pic) = decode_picture_item(data, &f, &primary, limit, &mut work)?;
            let planes = crop_planes(&sps, &pic);
            (
                planes,
                pic.bit_depth_luma,
                pic.bit_depth_chroma,
                sps.vui,
                f.item_icc_profile(&primary),
                0,
            )
        }
        other => {
            return refused(format!(
                "primary item is {other}, not a decodable picture (grid/hvc1 subset)"
            ));
        }
    };
    if let Some(angle) = f.item_irot(&primary).filter(|&a| a != 0) {
        planes = grid::rotate_ccw(&planes, angle);
    }
    Ok(HeicImage {
        planes,
        bit_depth_luma: bd_y,
        bit_depth_chroma: bd_c,
        vui,
        icc_profile: icc.take(),
        grid_tiles,
    })
}

/// Convert a decoded picture to 8-bit RGB (row-major, 3 bytes per
/// pixel) using the stream's signalled matrix + range. 10-bit
/// samples reduce by their high bits, the same convention every
/// consumer applies. Chroma is bilinearly upsampled.
pub fn to_rgb8(img: &HeicImage) -> Vec<u8> {
    let p = &img.planes;
    let (w, h) = (p.width, p.height);
    let (cw, ch) = p.chroma_dims();
    let sh_y = img.bit_depth_luma.saturating_sub(8);
    let sh_c = img.bit_depth_chroma.saturating_sub(8);
    let sample = |plane: &[u16], i: usize, sh: u32| -> f32 { (plane[i] >> sh) as f32 };
    // Bilinear chroma at (x, y): chroma sample centres sit at the
    // top-left of each 2x2 luma block (JPEG/MPEG-2 siting, what
    // ffmpeg's swscale assumes for 4:2:0 without explicit siting).
    let chroma_at = |plane: &[u16], x: usize, y: usize| -> f32 {
        let fx = (x as f32) / 2.0 - 0.25;
        let fy = (y as f32) / 2.0 - 0.25;
        let x0 = fx.floor();
        let y0 = fy.floor();
        let (mut acc, mut wsum) = (0.0f32, 0.0f32);
        for dy in 0..2i32 {
            for dx in 0..2i32 {
                let cx = x0 as i32 + dx;
                let cy = y0 as i32 + dy;
                let wx = if dx == 0 { 1.0 - (fx - x0) } else { fx - x0 };
                let wy = if dy == 0 { 1.0 - (fy - y0) } else { fy - y0 };
                let sx = cx.clamp(0, cw as i32 - 1) as usize;
                let sy = cy.clamp(0, ch as i32 - 1) as usize;
                acc += wx * wy * sample(plane, sy * cw + sx, sh_c);
                wsum += wx * wy;
            }
        }
        acc / wsum
    };
    let (kr, kb) = match img.vui.matrix_coeffs {
        Some(1) => (0.2126, 0.0722),         // BT.709
        Some(9) => (0.2627, 0.0593),         // BT.2020
        Some(5) | Some(6) => (0.299, 0.114), // BT.601 (625/525)
        Some(0) => (0.0, 0.0),               // GBR identity (handled below)
        _ => (0.299, 0.114),                 // unspecified: BT.601
    };
    let full = img.vui.video_full_range;
    let mut out = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let yv = sample(&p.y, i, sh_y);
            let cbv = chroma_at(&p.cb, x, y);
            let crv = chroma_at(&p.cr, x, y);
            let (r, g, b) = if img.vui.matrix_coeffs == Some(0) {
                // Identity (GBR stored in Y/Cb/Cr slots as G/B/R).
                (crv, yv, cbv)
            } else if full {
                let (cb, cr) = (cbv - 128.0, crv - 128.0);
                let r = yv + cr * (1.0 - kr) / 0.5;
                let b = yv + cb * (1.0 - kb) / 0.5;
                let g = (yv - kr * r - kb * b) / (1.0 - kr - kb);
                (r, g, b)
            } else {
                let yy = (yv - 16.0) * (255.0 / 219.0);
                let cb = (cbv - 128.0) * (255.0 / 224.0);
                let cr = (crv - 128.0) * (255.0 / 224.0);
                let r = yy + cr * (1.0 - kr) / 0.5;
                let b = yy + cb * (1.0 - kb) / 0.5;
                let g = (yy - kr * r - kb * b) / (1.0 - kr - kb);
                (r, g, b)
            };
            let o = i * 3;
            out[o] = r.round().clamp(0.0, 255.0) as u8;
            out[o + 1] = g.round().clamp(0.0, 255.0) as u8;
            out[o + 2] = b.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ftyp(major: &[u8; 4]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&24u32.to_be_bytes());
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(major);
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(b"heic");
        v.extend_from_slice(b"mif1");
        v
    }

    #[test]
    fn sniffs_heic_brands_by_content() {
        assert!(is_heic(&ftyp(b"heic")));
        assert!(is_heic(&ftyp(b"mif1")));
        // An unknown major brand still sniffs positive when a
        // compatible brand names the family.
        assert!(is_heic(&ftyp(b"xxxx")));
        let mut png = vec![0x89, b'P', b'N', b'G'];
        png.resize(24, 0);
        assert!(!is_heic(&png));
    }

    #[test]
    fn rgb_full_range_601_midgray_is_neutral() {
        let planes = PicturePlanes::new_filled(4, 4, 128);
        let img = HeicImage {
            planes,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            vui: VuiInfo {
                video_full_range: true,
                matrix_coeffs: Some(6),
                ..Default::default()
            },
            icc_profile: None,
            grid_tiles: 0,
        };
        let rgb = to_rgb8(&img);
        assert!(rgb.iter().all(|&v| v == 128), "neutral gray stays neutral");
    }

    #[test]
    fn non_heic_refuses_in_decode() {
        let junk = vec![0u8; 64];
        assert!(decode_heic(&junk).is_err());
    }

    /// The work budget charges cumulatively across pictures and
    /// refuses the charge that crosses 4x the pixel budget — the
    /// accounting that stops a grid of repeated/aliased tiles from
    /// multiplying decode work (libheif GHSA-48p4-gj45-w9v6 shape).
    #[test]
    fn work_budget_charges_cumulatively_and_refuses_over() {
        let mut wb = WorkBudget::new(1_000_000);
        assert_eq!(wb.limit, 4_000_000);
        wb.charge(1_500_000).expect("first tile");
        wb.charge(1_500_000).expect("second tile");
        wb.charge(1_000_000).expect("exactly at the limit");
        match wb.charge(1) {
            Err(HeicError::Refused(m)) => {
                assert!(m.contains("cumulative decode work budget"), "{m}")
            }
            other => panic!("expected work-budget refusal, got {other:?}"),
        }
        // A single charge larger than the whole budget refuses
        // from zero, and the accounting never wraps.
        let mut wb = WorkBudget::new(10);
        assert!(wb.charge(u64::MAX).is_err());
        assert_eq!(wb.charged, 0);
    }
}
