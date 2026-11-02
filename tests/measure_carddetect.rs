//! Card detection on the card that was actually printed (CARDDETECT,
//! 2026-11-02): strict re-verification after the size gates, the
//! printed ring geometry, ring-shaped seeding, and the print-scale
//! assumption every result carries.

use agenthand::image::decode_image;
use agenthand::measure::{detect_card, estimate, load_card_profile, MeasureFrame};
use serde_json::Value;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn project(h: &[f64; 9], p: [f64; 2]) -> [f64; 2] {
    let w = h[6] * p[0] + h[7] * p[1] + h[8];
    [
        (h[0] * p[0] + h[1] * p[1] + h[2]) / w,
        (h[3] * p[0] + h[4] * p[1] + h[5]) / w,
    ]
}

/// Paint a card-space rectangle (centre, half extents along x and y
/// in mm) into the image under H, densely enough to leave no holes.
fn paint_mm_rect(
    img: &mut agenthand::image::RgbaImage,
    h: &[f64; 9],
    centre: [f64; 2],
    half: [f64; 2],
    rgb: [u8; 3],
) {
    let (w, ht) = (img.width as i64, img.height as i64);
    let step = 0.05;
    let nx = (2.0 * half[0] / step).ceil() as i64;
    let ny = (2.0 * half[1] / step).ceil() as i64;
    for i in 0..=nx {
        for j in 0..=ny {
            let q = [
                centre[0] - half[0] + i as f64 * step,
                centre[1] - half[1] + j as f64 * step,
            ];
            let p = project(h, q);
            let (x, y) = (p[0].round() as i64, p[1].round() as i64);
            if x >= 0 && y >= 0 && x < w && y < ht {
                let k = ((y * w + x) * 4) as usize;
                img.pixels[k..k + 3].copy_from_slice(&rgb);
            }
        }
    }
}

/// The fit pipeline adopts a strict re-observation only when it
/// confirms (>= 10 strict ticks); otherwise it keeps the LOOSE pairs
/// and grades its gates on them. A card whose tick cuts only the loose
/// matcher accepts therefore measured on loose evidence alone. Here
/// every tick cut of a genuine card is widened to 13% of its arm:
/// still at the right place (the loose matcher takes runs up to 30% of
/// the arm), but no longer a tick-sized cut (strict takes at most
/// 12%). Strict re-verification at the accepted fit finds nothing, so
/// the estimate must refuse after the size gates.
#[test]
fn loose_only_ticks_refuse_under_strict_recheck() {
    let path = root().join("tests/fixtures/measure/ahbig_2x.png");
    let mut img = decode_image(&std::fs::read(&path).unwrap())
        .unwrap()
        .into_rgba();
    let profile_path = root().join("tests/fixtures/scaled-card-profiles.json");
    let card = load_card_profile(&profile_path, "AH-BIG").unwrap();
    let det = detect_card(&MeasureFrame::from_rgba(&img), &card).unwrap();

    let profile: Value =
        serde_json::from_str(&std::fs::read_to_string(&profile_path).unwrap()).unwrap();
    let c = &profile["cards"]["AH-BIG"];
    let arm_len = c["bracket_frame"]["arm_centreline_length_mm"]
        .as_f64()
        .unwrap();
    let stroke = c["bracket_frame"]["stroke_width_mm"].as_f64().unwrap();
    for tick in c["ticks"].as_array().unwrap() {
        let centre = [
            tick["centre_mm"][0].as_f64().unwrap(),
            tick["centre_mm"][1].as_f64().unwrap(),
        ];
        let along = 0.5 * 0.13 * arm_len;
        let across = 0.5 * stroke + 1.0;
        let horizontal = tick["arm"].as_str().unwrap().ends_with("-H");
        let half = if horizontal {
            [along, across]
        } else {
            [across, along]
        };
        paint_mm_rect(&mut img, &det.H, centre, half, [255, 255, 255]);
    }

    let frame = MeasureFrame::from_rgba(&img);
    match estimate(&frame, &card, "AH-BIG", None, None) {
        Ok(est) => panic!(
            "loose-only ticks measured: {} (ticks {}, card {} px, rms {})",
            est.display, est.ticks_matched, est.card_width_px, est.reprojection_rms_px
        ),
        Err(err) => {
            println!("widened cuts: {err}");
            assert!(
                err.starts_with("Card failed strict tick verification"),
                "{err}"
            );
        }
    }
}

/// The print master: the lockup on its 1200 x 880-unit canvas at 2x
/// (2400 x 1760 px), copied byte for byte from
/// film/v8/sources/cards/card-final.png (the art in AH-FAMILY v4).
/// Every card in the layout profile is that canvas scaled by its
/// `image.scale_mm_per_unit` and placed at its `image.offset_*_mm`.
fn printed_art() -> agenthand::image::RgbaImage {
    let path = root().join("tests/fixtures/measure/printed/card-final.png");
    decode_image(&std::fs::read(path).unwrap())
        .unwrap()
        .into_rgba()
}

fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    let rank = p / 100.0 * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    sorted[lo] + (rank - lo as f64) * (sorted[hi] - sorted[lo])
}

/// The profile must describe the card that is printed. Measured on
/// the print master, in canvas units (pixel centres, 2 px per unit):
/// the ring's centre, inner and outer radius, and every tick cut's
/// centre and length along its arm, for every card in the profile.
/// Tolerance: 1 canvas unit (0.19 mm on AH-BIG).
#[test]
fn profile_ring_and_ticks_match_the_printed_art() {
    let art = printed_art();
    let (w, h) = (art.width as usize, art.height as usize);
    assert_eq!((w, h), (2400, 1760));
    let px = |x: usize, y: usize| -> [f64; 3] {
        let k = (y * w + x) * 4;
        [
            art.pixels[k] as f64,
            art.pixels[k + 1] as f64,
            art.pixels[k + 2] as f64,
        ]
    };
    let grey_at = |u: [f64; 2]| -> f64 {
        // Bilinear on the 2x raster at canvas unit u.
        let (x, y) = (u[0] * 2.0 - 0.5, u[1] * 2.0 - 0.5);
        let (x0, y0) = (x.floor(), y.floor());
        let (dx, dy) = (x - x0, y - y0);
        let g = |xx: f64, yy: f64| -> f64 {
            let p = px(xx as usize, yy as usize);
            (p[0] + p[1] + p[2]) / 3.0
        };
        (1.0 - dx) * (1.0 - dy) * g(x0, y0)
            + dx * (1.0 - dy) * g(x0 + 1.0, y0)
            + (1.0 - dx) * dy * g(x0, y0 + 1.0)
            + dx * dy * g(x0 + 1.0, y0 + 1.0)
    };
    let profile: Value = serde_json::from_str(
        &std::fs::read_to_string(root().join("tests/fixtures/layout-profile.json")).unwrap(),
    )
    .unwrap();
    let mut failures: Vec<String> = Vec::new();
    for (id, c) in profile["cards"].as_object().unwrap() {
        let s = c["image"]["scale_mm_per_unit"].as_f64().unwrap();
        let ox = c["image"]["offset_x_mm"].as_f64().unwrap();
        let oy = c["image"]["offset_y_mm"].as_f64().unwrap();
        let to_units = |mm: [f64; 2]| -> [f64; 2] { [(mm[0] - ox) / s, (mm[1] - oy) / s] };
        let ring = &c["ring"];
        let centre_mm = [
            ring["centre_mm"][0].as_f64().unwrap(),
            ring["centre_mm"][1].as_f64().unwrap(),
        ];
        let centre_u = to_units(centre_mm);

        // Ring: cyan pixels (R < 128, B > 128, G > 100: the half-way
        // edge of #00AFE1 on white) within 120 units of the centre.
        let (mut n, mut sx, mut sy) = (0usize, 0.0f64, 0.0f64);
        let mut pts: Vec<[f64; 2]> = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let p = px(x, y);
                if p[0] < 128.0 && p[2] > 128.0 && p[1] > 100.0 {
                    let u = [(x as f64 + 0.5) / 2.0, (y as f64 + 0.5) / 2.0];
                    if (u[0] - centre_u[0]).hypot(u[1] - centre_u[1]) < 120.0 {
                        n += 1;
                        sx += u[0];
                        sy += u[1];
                        pts.push(u);
                    }
                }
            }
        }
        assert!(n > 10_000, "{id}: printed ring not found");
        let (cx, cy) = (sx / n as f64, sy / n as f64);
        let mut radii: Vec<f64> = pts
            .iter()
            .map(|u| (u[0] - cx).hypot(u[1] - cy))
            .collect();
        radii.sort_by(|a, b| a.total_cmp(b));
        let inner_u = percentile_sorted(&radii, 0.1);
        let outer_u = percentile_sorted(&radii, 99.9);
        let prof_inner_u = ring["inner_radius_mm"].as_f64().unwrap() / s;
        let prof_outer_u = ring["outer_radius_mm"].as_f64().unwrap() / s;
        println!(
            "{id}: printed ring centre ({cx:.2}, {cy:.2}) u, inner {inner_u:.2} u = {:.3} mm, \
             outer {outer_u:.2} u = {:.3} mm; profile inner {:.3} mm, outer {:.3} mm",
            inner_u * s,
            outer_u * s,
            prof_inner_u * s,
            prof_outer_u * s
        );
        if (cx - centre_u[0]).hypot(cy - centre_u[1]) > 1.0 {
            failures.push(format!("{id}: ring centre off by more than 1 unit"));
        }
        if (inner_u - prof_inner_u).abs() > 1.0 || (outer_u - prof_outer_u).abs() > 1.0 {
            failures.push(format!(
                "{id}: ring radii printed {:.3}/{:.3} mm vs profile {:.3}/{:.3} mm",
                inner_u * s,
                outer_u * s,
                prof_inner_u * s,
                prof_outer_u * s
            ));
        }

        // Ticks: the white cut through the arm at each tick centre,
        // walked out along the arm to the half-grey crossings.
        for tick in c["ticks"].as_array().unwrap() {
            let t_mm = [
                tick["centre_mm"][0].as_f64().unwrap(),
                tick["centre_mm"][1].as_f64().unwrap(),
            ];
            let t_u = to_units(t_mm);
            let horizontal = tick["arm"].as_str().unwrap().ends_with("-H");
            let dir = if horizontal { [1.0, 0.0] } else { [0.0, 1.0] };
            let along = |d: f64| [t_u[0] + d * dir[0], t_u[1] + d * dir[1]];
            let edge = |sign: f64| -> f64 {
                let mut d = 0.0;
                while grey_at(along(sign * (d + 0.05))) > 127.5 {
                    d += 0.05;
                    assert!(d < 40.0, "{id}: tick cut never closes");
                }
                d + 0.025
            };
            assert!(grey_at(t_u) > 127.5, "{id}: tick centre not on a cut");
            let (lo, hi) = (edge(-1.0), edge(1.0));
            let centre_off = (hi - lo) / 2.0;
            let cut_u = lo + hi;
            let prof_cut_u = tick["cut_along_arm_mm"].as_f64().unwrap() / s;
            if centre_off.abs() > 1.0 || (cut_u - prof_cut_u).abs() > 1.0 {
                failures.push(format!(
                    "{id} {}@{}: cut centre off {:.2} u, length {:.2} vs {:.2} u",
                    tick["arm"], tick["fraction_from_vertex"], centre_off, cut_u, prof_cut_u
                ));
            }
        }
    }
    assert!(failures.is_empty(), "profile vs print:\n{}", failures.join("\n"));
}
