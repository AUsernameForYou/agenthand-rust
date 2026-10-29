//! Sample adaptive offset (§8.7.3), applied after deblocking.
//! Reads classifications from a copy of the deblocked picture
//! (the oracle does the same), per CTB in raster order, with
//! merge-left / merge-up resolution of the retained parameters.

use crate::heic::params::{SliceHeader, Sps};
use crate::heic::recon::Plane;
use crate::heic::syntax::{RetainedSlice, SaoComp, SaoParams};

/// Resolved per-CTB, per-component SAO parameters. Band offsets
/// are signed; edge offsets are the retained magnitudes (the
/// sign comes from the edge category at application time).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Resolved {
    Off,
    Band { offsets: [i32; 4], position: u32 },
    Edge { offsets: [i32; 4], class: u32 },
}

fn resolve_comp(c: &SaoComp) -> Resolved {
    match c.ty {
        1 => Resolved::Band {
            offsets: c.offsets,
            position: c.aux as u32,
        },
        2 => Resolved::Edge {
            offsets: c.offsets,
            class: c.aux as u32,
        },
        _ => Resolved::Off,
    }
}

fn comp_from_resolved(r: &Resolved) -> SaoComp {
    match *r {
        Resolved::Off => SaoComp::default(),
        Resolved::Band { offsets, position } => SaoComp {
            ty: 1,
            offsets,
            aux: position as u8,
        },
        Resolved::Edge { offsets, class } => SaoComp {
            ty: 2,
            offsets,
            aux: class as u8,
        },
    }
}

pub fn apply_sao(sps: &Sps, hdr: &SliceHeader, retained: &RetainedSlice, planes: &mut [Plane; 3]) {
    if !sps.sao_enabled || (!hdr.sao_luma && !hdr.sao_chroma) {
        return;
    }
    // Source = the deblocked picture, frozen for classification.
    let src: Vec<Vec<i32>> = planes.iter().map(|p| p.data.clone()).collect();
    let ctb = sps.ctb_size() as usize;

    // Resolve merges in CTB raster order.
    let mut order: Vec<usize> = (0..retained.ctbs.len()).collect();
    order.sort_by_key(|&i| (retained.ctbs[i].ry, retained.ctbs[i].rx));
    let mut resolved: Vec<[Resolved; 3]> = vec![[Resolved::Off; 3]; retained.ctbs.len()];
    let pos_of = |rx: u32, ry: u32| -> Option<usize> {
        retained.ctbs.iter().position(|c| c.rx == rx && c.ry == ry)
    };
    for &i in &order {
        let c = &retained.ctbs[i];
        let comps: [SaoComp; 3] = match &c.sao {
            SaoParams::Explicit(comps) => *comps,
            SaoParams::MergeLeft => match pos_of(c.rx.wrapping_sub(1), c.ry) {
                Some(j) if c.rx > 0 => [
                    comp_from_resolved(&resolved[j][0]),
                    comp_from_resolved(&resolved[j][1]),
                    comp_from_resolved(&resolved[j][2]),
                ],
                _ => [SaoComp::default(); 3],
            },
            SaoParams::MergeUp => match pos_of(c.rx, c.ry.wrapping_sub(1)) {
                Some(j) if c.ry > 0 => [
                    comp_from_resolved(&resolved[j][0]),
                    comp_from_resolved(&resolved[j][1]),
                    comp_from_resolved(&resolved[j][2]),
                ],
                _ => [SaoComp::default(); 3],
            },
        };
        resolved[i] = [
            resolve_comp(&comps[0]),
            resolve_comp(&comps[1]),
            resolve_comp(&comps[2]),
        ];
    }

    for (i, ctb_data) in retained.ctbs.iter().enumerate() {
        for cidx in 0..3usize {
            let r = resolved[i][cidx];
            if r == Resolved::Off {
                continue;
            }
            if cidx == 0 && !hdr.sao_luma {
                continue;
            }
            if cidx > 0 && !hdr.sao_chroma {
                continue;
            }
            let bd = if cidx == 0 {
                sps.bit_depth_luma()
            } else {
                sps.bit_depth_chroma()
            };
            let maxv = (1i32 << bd) - 1;
            let (sub_w, sub_h) = if cidx == 0 {
                (1usize, 1usize)
            } else {
                (2usize, 2usize)
            };
            let x_c = ctb_data.rx as usize * ctb / sub_w;
            let y_c = ctb_data.ry as usize * ctb / sub_h;
            let seg_w = ctb / sub_w;
            let seg_h = ctb / sub_h;
            let plane = &mut planes[cidx];
            let srcp = &src[cidx];
            let (pw, ph) = (plane.w, plane.h);
            let x_end = (x_c + seg_w).min(pw);
            let y_end = (y_c + seg_h).min(ph);
            match r {
                Resolved::Band { offsets, position } => {
                    let shift = bd - 5;
                    for y in y_c..y_end {
                        for x in x_c..x_end {
                            let v = srcp[y * pw + x];
                            let band = ((v >> shift) as u32) & 31;
                            // bands position..position+3 (mod 32)
                            let rel = (band + 32 - position) & 31;
                            if rel < 4 {
                                plane.data[y * pw + x] = (v + offsets[rel as usize]).clamp(0, maxv);
                            }
                        }
                    }
                }
                Resolved::Edge { offsets, class } => {
                    let (hpos, vpos): ([i32; 2], [i32; 2]) = match class {
                        0 => ([-1, 1], [0, 0]),
                        1 => ([0, 0], [-1, 1]),
                        2 => ([-1, 1], [-1, 1]),
                        _ => ([1, -1], [-1, 1]),
                    };
                    for y in y_c..y_end {
                        for x in x_c..x_end {
                            let v = srcp[y * pw + x];
                            let mut edge_idx = 0i32;
                            let mut boundary = false;
                            for k in 0..2 {
                                let nx = x as i32 + hpos[k];
                                let ny = y as i32 + vpos[k];
                                if nx < 0 || ny < 0 || nx >= pw as i32 || ny >= ph as i32 {
                                    boundary = true;
                                    break;
                                }
                                let nb = srcp[ny as usize * pw + nx as usize];
                                edge_idx += (v - nb).signum();
                            }
                            if boundary {
                                continue;
                            }
                            // edgeIdx -2,-1 -> offsets[0],[1] added;
                            // +1,+2 -> offsets[2],[3] subtracted
                            // (the retained EO offsets are magnitudes).
                            let off = match edge_idx {
                                -2 => offsets[0],
                                -1 => offsets[1],
                                1 => -offsets[2],
                                2 => -offsets[3],
                                _ => 0,
                            };
                            if off != 0 {
                                plane.data[y * pw + x] = (v + off).clamp(0, maxv);
                            }
                        }
                    }
                }
                Resolved::Off => {}
            }
        }
    }
}
