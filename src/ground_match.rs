//! Ground match for generated plates (docs/GROUND-MATCH.md).
//!
//! The generated plate is drawn from text, so its ground can be any
//! surface. Seen through an open frame (a bike's spokes), a ground
//! that differs from the photo's reads wrong next to the remnants of
//! the real ground baked into the product layer. At draw time the
//! source photo's own ground is sampled and reduced to a descriptor
//! (surface words, value, warm or cool); the generated plate prompt
//! gets a short clause from it, the way the angle steer sets the
//! camera. The caller's scene text keeps the setting; the clause
//! constrains the ground only.
//!
//! Sampling, on a grid of at most 512 cells on the long side:
//! - around and below the product: cells whose centre lies from 6% of
//!   the product height above the ground line (`ground::ground_line`)
//!   to 30% below it, within 60% of the product width either side of
//!   its bbox, at least 3 cells clear of any product pixel;
//! - ground remnants inside open frames: background cells enclosed by
//!   the product (not 4-connected to the bbox border), in the lower
//!   35% of the product height, 1 cell clear of product pixels.
//!
//! Stats: channel medians (value = luma / 255, warmth = (R - B) / luma
//! of the medians), median saturation, saturation-weighted hue, green
//! fraction, median in-cell luma deviation (texture). A fixed rule
//! list names the surface; see `classify`.
//!
//! After the plate comes back, the same stats are read off the fitted
//! plate below the product's ground line and reported beside the
//! sample (`ground_match.plate`, `delta`). The check reports; it never
//! refuses a draw.

use serde_json::{json, Value};

use crate::image::RgbaImage;

/// Kill switch: `AGENTHAND_GROUND_MATCH=off` (also 0, false, no).
pub const GROUND_MATCH_ENV: &str = "AGENTHAND_GROUND_MATCH";
pub const GROUND_MATCH_VERSION: i64 = 1;

/// Sampling grid: long side at most this many cells.
pub const MATCH_GRID_MAX: usize = 512;
/// Region around and below the product, x product height / width.
pub const SAMPLE_ABOVE_LINE: f64 = 0.06;
pub const SAMPLE_BELOW_LINE: f64 = 0.30;
pub const SAMPLE_SIDE: f64 = 0.60;
/// Clearance from product pixels, cells (around; remnants).
pub const SAMPLE_CLEAR_CELLS: usize = 3;
pub const REMNANT_CLEAR_CELLS: usize = 1;
/// Remnants: the lower share of the product height only.
pub const REMNANT_BAND: f64 = 0.35;
/// At least this many ground cells, else no descriptor.
pub const MIN_GROUND_CELLS: usize = 50;
/// Plate check band: from the ground line to this share of the
/// product height below it, across the product's width.
pub const PLATE_BAND: f64 = 0.15;

/// Whether ground matching runs at all (the kill switch).
pub fn ground_match_enabled() -> bool {
    !matches!(
        std::env::var(GROUND_MATCH_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "off" | "false" | "no"
    )
}

/// Pooled colour and texture statistics of a ground sample.
#[derive(Debug, Clone, PartialEq)]
pub struct GroundStats {
    pub cells: usize,
    /// Median luma / 255.
    pub value: f64,
    pub saturation: f64,
    /// Saturation-weighted mean hue in degrees; None when under 20% of
    /// the cells carry colour (saturation >= 0.08).
    pub hue_deg: Option<f64>,
    /// (R - B) / luma of the channel medians.
    pub warmth: f64,
    pub green_fraction: f64,
    /// Median in-cell luma standard deviation, 0..255 levels.
    pub texture: f64,
    pub median_rgb: [f64; 3],
}

impl GroundStats {
    pub fn to_json(&self) -> Value {
        let r = crate::round6;
        json!({
            "cells": self.cells,
            "value": r(self.value),
            "saturation": r(self.saturation),
            "hue_deg": self.hue_deg.map(|h| (h * 10.0).round() / 10.0),
            "warmth": r(self.warmth),
            "green_fraction": r(self.green_fraction),
            "texture": r(self.texture),
            "median_rgb": self.median_rgb.iter().map(|v| v.round()).collect::<Vec<_>>(),
        })
    }
}

/// The source ground in words, with the stats behind them.
#[derive(Debug, Clone, PartialEq)]
pub struct GroundDescriptor {
    pub surface: String,
    pub value_word: &'static str,
    pub temperature: &'static str,
    pub stats: GroundStats,
    pub around_cells: usize,
    pub remnant_cells: usize,
}

impl GroundDescriptor {
    /// "weathered dark grey paving, mid-dark value, neutral tone".
    pub fn phrase(&self) -> String {
        format!(
            "{}, {} value, {} tone",
            self.surface, self.value_word, self.temperature
        )
    }

    /// The generated plate prompt clause.
    pub fn clause(&self) -> String {
        format!(
            "Premium, polished listing scene; the ground surface matches the original photo's ground: {}.",
            self.phrase()
        )
    }

    pub fn to_json(&self) -> Value {
        json!({
            "surface": self.surface,
            "value": self.value_word,
            "temperature": self.temperature,
            "phrase": self.phrase(),
            "around_cells": self.around_cells,
            "remnant_cells": self.remnant_cells,
            "stats": self.stats.to_json(),
        })
    }
}

/// Value words on luma / 255.
pub fn value_word(value: f64) -> &'static str {
    match value {
        v if v < 0.20 => "dark",
        v if v < 0.35 => "mid-dark",
        v if v < 0.55 => "mid",
        v if v < 0.72 => "light",
        _ => "pale",
    }
}

/// Temperature words on (R - B) / luma.
pub fn temperature_word(warmth: f64) -> &'static str {
    match warmth {
        w if w >= 0.12 => "warm",
        w if w >= 0.05 => "warm-neutral",
        w if w > -0.02 => "neutral",
        w if w > -0.08 => "cool-neutral",
        _ => "cool",
    }
}

/// The surface words: a fixed, ordered rule list on the stats.
/// 1. green fraction >= 0.35: lawn;
/// 2. colourful (saturation >= 0.22 with a hue): red brick (hue under
///    15 or from 340), warm browns and tans (15..50: sandstone when
///    light, gravel or decking by texture at mid values, earth when
///    dark), sandy ground (50..70);
/// 3. otherwise the grey family by value: dark asphalt (< 0.22), dark
///    grey paving (< 0.40), grey concrete paving (< 0.58), light grey
///    stone paving (< 0.75), pale limestone paving (warm) or pale
///    concrete; "weathered" when the texture is at least 6 levels,
///    "smooth" under 3.
pub fn classify(stats: &GroundStats) -> String {
    if stats.green_fraction >= 0.35 {
        return if stats.value < 0.30 {
            "dark green lawn".to_string()
        } else {
            "green lawn grass".to_string()
        };
    }
    if let Some(h) = stats.hue_deg.filter(|_| stats.saturation >= 0.22) {
        if !(15.0..340.0).contains(&h) {
            return "red brick paving".to_string();
        }
        if h < 50.0 {
            return if stats.value >= 0.55 {
                "warm sandstone paving"
            } else if stats.value >= 0.30 {
                if stats.texture >= 9.0 {
                    "tan gravel"
                } else {
                    "brown wooden decking"
                }
            } else {
                "dark brown earth"
            }
            .to_string();
        }
        if h < 70.0 {
            return "sandy ground".to_string();
        }
    }
    let base = match stats.value {
        v if v < 0.22 => "dark asphalt",
        v if v < 0.40 => "dark grey paving",
        v if v < 0.58 => "grey concrete paving",
        v if v < 0.75 => "light grey stone paving",
        _ if stats.warmth >= 0.05 => "pale limestone paving",
        _ => "pale concrete paving",
    };
    if stats.texture >= 6.0 {
        format!("weathered {base}")
    } else if stats.texture < 3.0 {
        format!("smooth {base}")
    } else {
        base.to_string()
    }
}

/// Per-cell means and in-cell luma deviation on the sampling grid.
struct CellGrid {
    cell: usize,
    gw: usize,
    gh: usize,
    rgb: Vec<[f64; 3]>,
    texture: Vec<f64>,
}

fn cell_grid(image: &RgbaImage) -> CellGrid {
    let (w, h) = (image.width as usize, image.height as usize);
    let cell = w.max(h).div_ceil(MATCH_GRID_MAX).max(1);
    let (gw, gh) = (w.div_ceil(cell), h.div_ceil(cell));
    let mut sums = vec![[0f64; 5]; gw * gh];
    for y in 0..h {
        let gy = y / cell;
        for x in 0..w {
            let i = (y * w + x) * 4;
            let p = &image.pixels[i..i + 3];
            let l = 0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64;
            let s = &mut sums[gy * gw + x / cell];
            s[0] += p[0] as f64;
            s[1] += p[1] as f64;
            s[2] += p[2] as f64;
            s[3] += l;
            s[4] += l * l;
        }
    }
    let mut rgb = vec![[0f64; 3]; gw * gh];
    let mut texture = vec![0f64; gw * gh];
    for gy in 0..gh {
        let ch = (cell.min(h - gy * cell)) as f64;
        for gx in 0..gw {
            let cw = (cell.min(w - gx * cell)) as f64;
            let n = cw * ch;
            let s = sums[gy * gw + gx];
            rgb[gy * gw + gx] = [s[0] / n, s[1] / n, s[2] / n];
            let mean = s[3] / n;
            texture[gy * gw + gx] = (s[4] / n - mean * mean).max(0.0).sqrt();
        }
    }
    CellGrid {
        cell,
        gw,
        gh,
        rgb,
        texture,
    }
}

/// Cells holding any product pixel (alpha > 0), dilated by `r` cells.
fn product_cells(foreground: &RgbaImage, grid: &CellGrid, r: usize) -> Vec<bool> {
    let (w, h) = (foreground.width as usize, foreground.height as usize);
    let mut on = vec![false; grid.gw * grid.gh];
    for y in 0..h {
        let row = y * w;
        let gy = y / grid.cell;
        for x in 0..w {
            if foreground.pixels[(row + x) * 4 + 3] > 0 {
                on[gy * grid.gw + x / grid.cell] = true;
            }
        }
    }
    if r == 0 {
        return on;
    }
    crate::composite::dilate_square(
        &on.iter().map(|&b| b as u8).collect::<Vec<_>>(),
        grid.gw,
        grid.gh,
        r,
    )
    .into_iter()
    .map(|v| v > 0)
    .collect()
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

fn hsv(rgb: [f64; 3]) -> (f64, f64) {
    let (r, g, b) = (rgb[0] / 255.0, rgb[1] / 255.0, rgb[2] / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let sat = if max > 0.0 { d / max } else { 0.0 };
    let hue = if d <= 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / d).rem_euclid(6.0))
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (hue, sat)
}

/// Pool the stats over the selected cells.
fn pool(grid: &CellGrid, cells: &[usize]) -> GroundStats {
    let n = cells.len();
    let ch = |c: usize| median(cells.iter().map(|&i| grid.rgb[i][c]).collect());
    let median_rgb = [ch(0), ch(1), ch(2)];
    let luma = 0.299 * median_rgb[0] + 0.587 * median_rgb[1] + 0.114 * median_rgb[2];
    let mut sats = Vec::with_capacity(n);
    let (mut hx, mut hy, mut wsum) = (0f64, 0f64, 0f64);
    let mut chromatic = 0usize;
    let mut green = 0usize;
    for &i in cells {
        let (h, s) = hsv(grid.rgb[i]);
        sats.push(s);
        if s >= 0.08 {
            chromatic += 1;
            hx += s * h.to_radians().cos();
            hy += s * h.to_radians().sin();
            wsum += s;
        }
        let [r, g, b] = grid.rgb[i];
        if s >= 0.12 && (70.0..170.0).contains(&h) && g > r && g > b {
            green += 1;
        }
    }
    let hue_deg = (n > 0 && chromatic * 5 >= n && wsum > 0.0)
        .then(|| hy.atan2(hx).to_degrees().rem_euclid(360.0));
    GroundStats {
        cells: n,
        value: luma / 255.0,
        saturation: median(sats),
        hue_deg,
        warmth: (median_rgb[0] - median_rgb[2]) / luma.max(1.0),
        green_fraction: if n > 0 { green as f64 / n as f64 } else { 0.0 },
        texture: median(cells.iter().map(|&i| grid.texture[i]).collect()),
        median_rgb,
    }
}

/// Sample the source photo's ground around and below the product and
/// inside its open frames, and name it. Source and layer share a frame.
pub fn ground_descriptor(source: &RgbaImage, foreground: &RgbaImage) -> Result<GroundDescriptor, String> {
    if (source.width, source.height) != (foreground.width, foreground.height) {
        return Err("source and product layer frames differ".to_string());
    }
    let subject = crate::staging::analyze_subject(foreground);
    if subject.fills_frame {
        return Err("subject is a surface or fills the frame; no ground around it".to_string());
    }
    let line = crate::ground::ground_line(foreground)
        .ok_or_else(|| "the product layer has no opaque pixel".to_string())?;
    let (bx0, by0, bx1, by1) = line.bbox;
    let (hp, wp) = ((by1 - by0) as f64, (bx1 - bx0) as f64);
    let grid = cell_grid(source);
    let clear = product_cells(foreground, &grid, SAMPLE_CLEAR_CELLS);
    let near = product_cells(foreground, &grid, REMNANT_CLEAR_CELLS);
    let raw = product_cells(foreground, &grid, 0);
    let c = grid.cell as f64;
    let centre = |g: usize| (g as f64 + 0.5) * c;

    // Enclosed background inside the bbox: flood from the bbox border.
    let (gx0, gy0) = (bx0 as usize / grid.cell, by0 as usize / grid.cell);
    let (gx1, gy1) = (
        (bx1 as usize).div_ceil(grid.cell).min(grid.gw),
        (by1 as usize).div_ceil(grid.cell).min(grid.gh),
    );
    let mut outside = vec![false; grid.gw * grid.gh];
    let mut stack = Vec::new();
    for gy in gy0..gy1 {
        for gx in gx0..gx1 {
            let border = gx == gx0 || gx + 1 == gx1 || gy == gy0 || gy + 1 == gy1;
            let i = gy * grid.gw + gx;
            if border && !raw[i] {
                outside[i] = true;
                stack.push((gx, gy));
            }
        }
    }
    while let Some((gx, gy)) = stack.pop() {
        let mut visit = |x: usize, y: usize| {
            let i = y * grid.gw + x;
            if !outside[i] && !raw[i] {
                outside[i] = true;
                stack.push((x, y));
            }
        };
        if gx > gx0 {
            visit(gx - 1, gy);
        }
        if gx + 1 < gx1 {
            visit(gx + 1, gy);
        }
        if gy > gy0 {
            visit(gx, gy - 1);
        }
        if gy + 1 < gy1 {
            visit(gx, gy + 1);
        }
    }

    let mut cells = Vec::new();
    let (mut around, mut remnant) = (0usize, 0usize);
    for gy in 0..grid.gh {
        for gx in 0..grid.gw {
            let i = gy * grid.gw + gx;
            let (xc, yc) = (centre(gx), centre(gy));
            let l = line.at(xc);
            let inside_bbox = (gx0..gx1).contains(&gx) && (gy0..gy1).contains(&gy);
            if inside_bbox && !raw[i] && !outside[i] {
                if !near[i] && yc >= l - REMNANT_BAND * hp {
                    cells.push(i);
                    remnant += 1;
                }
                continue;
            }
            if clear[i] {
                continue;
            }
            if yc >= l - SAMPLE_ABOVE_LINE * hp
                && yc <= l + SAMPLE_BELOW_LINE * hp
                && xc >= bx0 as f64 - SAMPLE_SIDE * wp
                && xc <= bx1 as f64 + SAMPLE_SIDE * wp
            {
                cells.push(i);
                around += 1;
            }
        }
    }
    if cells.len() < MIN_GROUND_CELLS {
        return Err(format!(
            "too little ground visible around the product ({} cells, at least {MIN_GROUND_CELLS})",
            cells.len()
        ));
    }
    let stats = pool(&grid, &cells);
    Ok(GroundDescriptor {
        surface: classify(&stats),
        value_word: value_word(stats.value),
        temperature: temperature_word(stats.warmth),
        stats,
        around_cells: around,
        remnant_cells: remnant,
    })
}

/// The same stats read off a fitted plate (same frame as the layer),
/// from the product's ground line down `PLATE_BAND` of its height,
/// across its width. The plate holds no product, so every cell counts.
pub fn plate_ground_stats(plate: &RgbaImage, foreground: &RgbaImage) -> Option<GroundStats> {
    if (plate.width, plate.height) != (foreground.width, foreground.height) {
        return None;
    }
    let line = crate::ground::ground_line(foreground)?;
    let (bx0, by0, bx1, by1) = line.bbox;
    let hp = (by1 - by0) as f64;
    let grid = cell_grid(plate);
    let c = grid.cell as f64;
    let mut cells = Vec::new();
    for gy in 0..grid.gh {
        let yc = (gy as f64 + 0.5) * c;
        for gx in 0..grid.gw {
            let xc = (gx as f64 + 0.5) * c;
            if xc < bx0 as f64 || xc > bx1 as f64 {
                continue;
            }
            let l = line.at(xc);
            if yc >= l && yc <= l + PLATE_BAND * hp {
                cells.push(gy * grid.gw + gx);
            }
        }
    }
    (!cells.is_empty()).then(|| pool(&grid, &cells))
}

/// What a generated draw does about the ground: the descriptor (or why
/// none), and the prompt clause when there is one.
#[derive(Debug, Clone, PartialEq)]
pub struct GroundMatchPlan {
    pub descriptor: Option<GroundDescriptor>,
    pub reason: Option<String>,
}

impl GroundMatchPlan {
    pub fn clause(&self) -> Option<String> {
        self.descriptor.as_ref().map(GroundDescriptor::clause)
    }

    /// The draw result's `ground_descriptor` record.
    pub fn to_json(&self) -> Value {
        match &self.descriptor {
            Some(d) => {
                let mut v = d.to_json();
                v["stated"] = json!(true);
                v["clause"] = json!(d.clause());
                v
            }
            None => json!({"stated": false, "reason": self.reason}),
        }
    }

    /// The receipt's `ground_match` record: the sample, what the fitted
    /// plate shows below the product's ground line, and the difference.
    pub fn record(&self, fitted_plate: &RgbaImage, foreground: &RgbaImage) -> Value {
        let Some(d) = &self.descriptor else {
            return json!({"version": GROUND_MATCH_VERSION, "applied": false, "reason": self.reason});
        };
        let plate = plate_ground_stats(fitted_plate, foreground);
        let r = crate::round6;
        let (plate_json, delta) = match &plate {
            Some(p) => (
                {
                    let mut v = p.to_json();
                    v["surface"] = json!(classify(p));
                    v["value_word"] = json!(value_word(p.value));
                    v["temperature"] = json!(temperature_word(p.warmth));
                    v
                },
                json!({
                    "value": r(p.value - d.stats.value),
                    "warmth": r(p.warmth - d.stats.warmth),
                    "saturation": r(p.saturation - d.stats.saturation),
                    "same_value_word": value_word(p.value) == d.value_word,
                    "same_temperature": temperature_word(p.warmth) == d.temperature,
                }),
            ),
            None => (Value::Null, Value::Null),
        };
        json!({
            "version": GROUND_MATCH_VERSION,
            "applied": true,
            "reason": null,
            "clause": d.clause(),
            "sampled": d.to_json(),
            "plate": plate_json,
            "delta": delta,
        })
    }
}

/// The plan for one generated draw: the kill switch, then the sample.
/// `source` is None when the photo could not be read.
pub fn plan_for_draw(source: Option<&RgbaImage>, foreground: &RgbaImage) -> GroundMatchPlan {
    if !ground_match_enabled() {
        return GroundMatchPlan {
            descriptor: None,
            reason: Some(format!("disabled by {GROUND_MATCH_ENV}")),
        };
    }
    let Some(source) = source else {
        return GroundMatchPlan {
            descriptor: None,
            reason: Some("source photo unreadable; ground not sampled".to_string()),
        };
    };
    match ground_descriptor(source, foreground) {
        Ok(d) => GroundMatchPlan {
            descriptor: Some(d),
            reason: None,
        },
        Err(reason) => GroundMatchPlan {
            descriptor: None,
            reason: Some(reason),
        },
    }
}
