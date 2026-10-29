//! HEIF grid composition (M3.5 work, started at M1 on Daniel's
//! approval — decision 52, 2026-10-28: grid is IN scope).
//!
//! Composition geometry only: this module places already-decoded
//! tile pictures onto the grid canvas. Decoding the tile bitstreams
//! is M2/M3; the bit-exact junction where the two meet is M3.5.
//!
//! Rules implemented (ISO/IEC 23008-12 grid semantics + the gate-5
//! hash convention's grid clause from MILESTONE-M1.md):
//!
//! - tiles are placed row-major in `dimg` reference order — the
//!   order of the slice passed in IS the contract; nothing is
//!   re-sorted here;
//! - each tile contributes its full decoded picture (the decoder
//!   has already applied the tile's own conformance crop);
//! - the canvas is `columns * tile_width` × `rows * tile_height`
//!   luma samples; the composed picture is then cropped to the grid
//!   item's output size, which is where the right/bottom overhang
//!   of edge tiles is cut;
//! - chroma (4:2:0) follows the same placement at half resolution:
//!   tile chroma is `ceil(tile/2)` per axis, canvas chroma is the
//!   sum over the grid, cropped to `ceil(output/2)` per axis.
//!
//! Sample storage is u16 with values as decoded (8-bit samples in
//! the low bits), matching the hash convention in MILESTONE-M0.md.

use sha2::{Digest, Sha256};

use super::container::{GridHeader, HeicError};

type Res<T> = Result<T, HeicError>;

fn refused<T>(msg: impl Into<String>) -> Res<T> {
    Err(HeicError::Refused(msg.into()))
}

/// One decoded picture's planes (4:2:0). `width`/`height` are luma
/// dimensions; chroma planes are `ceil(w/2)` × `ceil(h/2)`.
#[derive(Debug, Clone, PartialEq)]
pub struct PicturePlanes {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u16>,
    pub cb: Vec<u16>,
    pub cr: Vec<u16>,
}

impl PicturePlanes {
    pub fn chroma_dims(&self) -> (usize, usize) {
        (self.width.div_ceil(2), self.height.div_ceil(2))
    }
    pub fn new_filled(width: usize, height: usize, value: u16) -> Self {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        PicturePlanes {
            width,
            height,
            y: vec![value; width * height],
            cb: vec![value; cw * ch],
            cr: vec![value; cw * ch],
        }
    }
    fn validate(&self) -> Res<()> {
        let (cw, ch) = self.chroma_dims();
        if self.y.len() != self.width * self.height
            || self.cb.len() != cw * ch
            || self.cr.len() != cw * ch
        {
            return refused(format!(
                "plane lengths inconsistent with {}x{} (y {}, cb {}, cr {}; expected {} / {} / {})",
                self.width,
                self.height,
                self.y.len(),
                self.cb.len(),
                self.cr.len(),
                self.width * self.height,
                cw * ch,
                cw * ch
            ));
        }
        Ok(())
    }
}

/// Compose the grid canvas from its tile pictures and crop to the
/// grid output size. `tiles` must be in `dimg` order and number
/// exactly `rows * columns`; all tiles must share one size (true of
/// every iPhone grid in the corpus; anything else is refused loudly
/// rather than silently misplaced).
pub fn compose_grid(tiles: &[PicturePlanes], grid: &GridHeader) -> Res<PicturePlanes> {
    let expect = (grid.rows * grid.columns) as usize;
    if tiles.len() != expect {
        return refused(format!(
            "grid {}x{} needs {expect} tile pictures, got {}",
            grid.rows,
            grid.columns,
            tiles.len()
        ));
    }
    if tiles.is_empty() {
        return refused("grid with zero tiles");
    }
    let tw = tiles[0].width;
    let th = tiles[0].height;
    for (i, t) in tiles.iter().enumerate() {
        t.validate()?;
        if t.width != tw || t.height != th {
            return refused(format!(
                "tile {i} is {}x{}, expected uniform {tw}x{th} (non-uniform grids are out of subset)",
                t.width, t.height
            ));
        }
    }
    let cols = grid.columns as usize;
    let canvas_w = cols * tw;
    let canvas_h = grid.rows as usize * th;
    let out_w = grid.output_width as usize;
    let out_h = grid.output_height as usize;
    if out_w == 0 || out_h == 0 || out_w > canvas_w || out_h > canvas_h {
        return refused(format!(
            "grid output {out_w}x{out_h} does not fit canvas {canvas_w}x{canvas_h}"
        ));
    }
    // Luma placement.
    let mut canvas_y = vec![0u16; canvas_w * canvas_h];
    let (tcw, tch) = tiles[0].chroma_dims();
    let canvas_cw = cols * tcw;
    let canvas_ch = grid.rows as usize * tch;
    let mut canvas_cb = vec![0u16; canvas_cw * canvas_ch];
    let mut canvas_cr = vec![0u16; canvas_cw * canvas_ch];
    for (i, t) in tiles.iter().enumerate() {
        let gx = (i % cols) * tw;
        let gy = (i / cols) * th;
        for row in 0..th {
            let dst = (gy + row) * canvas_w + gx;
            let src = row * tw;
            canvas_y[dst..dst + tw].copy_from_slice(&t.y[src..src + tw]);
        }
        let cx = (i % cols) * tcw;
        let cy = (i / cols) * tch;
        for row in 0..tch {
            let dst = (cy + row) * canvas_cw + cx;
            let src = row * tcw;
            canvas_cb[dst..dst + tcw].copy_from_slice(&t.cb[src..src + tcw]);
            canvas_cr[dst..dst + tcw].copy_from_slice(&t.cr[src..src + tcw]);
        }
    }
    // Crop canvas to the grid output size (cuts the edge overhang).
    let crop = |plane: &[u16], pw: usize, ow: usize, oh: usize| -> Vec<u16> {
        let mut out = Vec::with_capacity(ow * oh);
        for row in 0..oh {
            let src = row * pw;
            out.extend_from_slice(&plane[src..src + ow]);
        }
        out
    };
    let (ocw, och) = (out_w.div_ceil(2), out_h.div_ceil(2));
    Ok(PicturePlanes {
        width: out_w,
        height: out_h,
        y: crop(&canvas_y, canvas_w, out_w, out_h),
        cb: crop(&canvas_cb, canvas_cw, ocw, och),
        cr: crop(&canvas_cr, canvas_cw, ocw, och),
    })
}

/// SHA-256 of one plane of a composed (already tightly cropped)
/// picture, rows packed: 1 byte per sample at 8-bit depth, 2 bytes
/// little-endian (value in the low bits) above 8 bits — the same
/// byte convention as `recon::Picture::plane_hash`.
pub fn plane_hash(plane: &[u16], width: usize, height: usize, bit_depth: u32) -> String {
    assert_eq!(plane.len(), width * height, "plane length vs dims");
    let mut hasher = Sha256::new();
    for row in 0..height {
        let line = &plane[row * width..(row + 1) * width];
        if bit_depth <= 8 {
            let bytes: Vec<u8> = line.iter().map(|&v| v as u8).collect();
            hasher.update(bytes);
        } else {
            let mut bytes = Vec::with_capacity(line.len() * 2);
            for &v in line {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            hasher.update(bytes);
        }
    }
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hash all three planes of a composed picture (luma at
/// `bit_depth_luma`, chroma at `bit_depth_chroma`).
pub fn picture_hashes(
    p: &PicturePlanes,
    bit_depth_luma: u32,
    bit_depth_chroma: u32,
) -> [String; 3] {
    let (cw, ch) = p.chroma_dims();
    [
        plane_hash(&p.y, p.width, p.height, bit_depth_luma),
        plane_hash(&p.cb, cw, ch, bit_depth_chroma),
        plane_hash(&p.cr, cw, ch, bit_depth_chroma),
    ]
}

/// Rotate a picture by `quarter_turns * 90` degrees
/// counter-clockwise (the HEIF `irot` display transform). This is
/// a container-stage operation: parity is measured on native
/// planes pre-rotation; rotation exists so the displayed
/// dimensions/orientation can be verified against the oracle's.
pub fn rotate_ccw(p: &PicturePlanes, quarter_turns: u8) -> PicturePlanes {
    let turns = quarter_turns & 0x3;
    if turns == 0 {
        return p.clone();
    }
    let rotate_plane = |plane: &[u16], w: usize, h: usize| -> (Vec<u16>, usize, usize) {
        // One CCW turn: out(x, y) = in(w - 1 - y, x); dims swap.
        let (mut cur, mut cw, mut ch) = (plane.to_vec(), w, h);
        for _ in 0..turns {
            let mut out = vec![0u16; cw * ch];
            for y in 0..cw {
                for x in 0..ch {
                    out[y * ch + x] = cur[x * cw + (cw - 1 - y)];
                }
            }
            cur = out;
            std::mem::swap(&mut cw, &mut ch);
        }
        (cur, cw, ch)
    };
    let (y, w, h) = rotate_plane(&p.y, p.width, p.height);
    let (cw0, ch0) = p.chroma_dims();
    let (cb, _, _) = rotate_plane(&p.cb, cw0, ch0);
    let (cr, _, _) = rotate_plane(&p.cr, cw0, ch0);
    PicturePlanes {
        width: w,
        height: h,
        y,
        cb,
        cr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: u32, cols: u32, ow: u32, oh: u32) -> GridHeader {
        GridHeader {
            rows,
            columns: cols,
            output_width: ow,
            output_height: oh,
        }
    }

    #[test]
    fn places_tiles_row_major_in_dimg_order() {
        // 2x2 grid of 4x4 tiles, no overhang. Tile i filled with i.
        let tiles: Vec<PicturePlanes> = (0..4)
            .map(|i| PicturePlanes::new_filled(4, 4, i as u16))
            .collect();
        let out = compose_grid(&tiles, &grid(2, 2, 8, 8)).expect("compose");
        assert_eq!((out.width, out.height), (8, 8));
        // corners: TL=tile0, TR=tile1, BL=tile2, BR=tile3
        assert_eq!(out.y[0], 0);
        assert_eq!(out.y[7], 1);
        assert_eq!(out.y[7 * 8], 2);
        assert_eq!(out.y[7 * 8 + 7], 3);
        // chroma canvas is 4x4, same placement at half scale
        assert_eq!((out.cb.len(), out.cr.len()), (16, 16));
        assert_eq!(out.cb[3], 1);
        assert_eq!(out.cb[3 * 4 + 3], 3);
    }

    #[test]
    fn overhang_is_cropped_to_output_size() {
        // Scaled-down mirror of the real pair-2 geometry: tiles
        // 6x4, grid 2 rows x 3 cols -> canvas 18x8, output 17x7
        // (odd output dims on purpose: exercises the chroma ceil).
        let tiles: Vec<PicturePlanes> = (0..6)
            .map(|i| PicturePlanes::new_filled(6, 4, (i + 1) as u16))
            .collect();
        let out = compose_grid(&tiles, &grid(2, 3, 17, 7)).expect("compose");
        assert_eq!((out.width, out.height), (17, 7));
        // last canvas column (tile 3's rightmost) is cut: the pixel
        // at (16, 0) is tile index 2 (value 3), not an 18-wide row.
        assert_eq!(out.y[16], 3);
        // bottom row of tiles starts at y=4; last kept row is y=6.
        assert_eq!(out.y[6 * 17], 4);
        // chroma: canvas 9x4 -> cropped to ceil(17/2)=9 x ceil(7/2)=4
        assert_eq!(out.cb.len(), 9 * 4);
    }

    #[test]
    fn rotate_ccw_swaps_dims_and_moves_samples() {
        // 4x2 luma (chroma 2x1), distinct values per sample.
        let mut p = PicturePlanes::new_filled(4, 2, 0);
        p.y = vec![0, 1, 2, 3, 4, 5, 6, 7];
        p.cb = vec![10, 11];
        p.cr = vec![12, 13];
        let r = rotate_ccw(&p, 1);
        assert_eq!((r.width, r.height), (2, 4));
        // CCW: the stored top-right sample (3) becomes top-left.
        assert_eq!(r.y, vec![3, 7, 2, 6, 1, 5, 0, 4]);
        assert_eq!(r.cb, vec![11, 10]);
        // Two turns = 180: dims restored, order reversed.
        let r2 = rotate_ccw(&p, 2);
        assert_eq!((r2.width, r2.height), (4, 2));
        assert_eq!(r2.y, vec![7, 6, 5, 4, 3, 2, 1, 0]);
        // Four turns = identity; three turns swaps back.
        assert_eq!(rotate_ccw(&p, 4), p);
        assert_eq!((rotate_ccw(&p, 3).width, rotate_ccw(&p, 3).height), (2, 4));
    }

    #[test]
    fn wrong_tile_count_and_mixed_sizes_refuse() {
        let tiles = vec![PicturePlanes::new_filled(4, 4, 0)];
        match compose_grid(&tiles, &grid(2, 2, 8, 8)) {
            Err(HeicError::Refused(m)) => assert!(m.contains("needs 4 tile pictures")),
            other => panic!("expected refusal, got {other:?}"),
        }
        let mixed = vec![
            PicturePlanes::new_filled(4, 4, 0),
            PicturePlanes::new_filled(2, 4, 0),
        ];
        match compose_grid(&mixed, &grid(1, 2, 6, 4)) {
            Err(HeicError::Refused(m)) => assert!(m.contains("non-uniform")),
            other => panic!("expected refusal, got {other:?}"),
        }
        let tiles: Vec<PicturePlanes> =
            (0..4).map(|_| PicturePlanes::new_filled(4, 4, 0)).collect();
        match compose_grid(&tiles, &grid(2, 2, 9, 8)) {
            Err(HeicError::Refused(m)) => assert!(m.contains("does not fit canvas")),
            other => panic!("expected refusal, got {other:?}"),
        }
    }
}
