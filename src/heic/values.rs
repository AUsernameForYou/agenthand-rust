//! Value-domain derivations for reconstruction (M3).
//!
//! Termination (M2) proves the CABAC walk consumes the right bins;
//! it cannot prove the VALUES those bins carry are interpreted
//! correctly. This module holds the derivations that produce sample
//! values — sign handling, hidden-sign derivation, QP prediction and
//! the chroma QP map, chroma mode resolution — as pure functions
//! with unit tests pinned to spec text and to the staged oracle's
//! (libde265 1.1.3) implementations, which are cited per function.
//! Staged sample parity (MILESTONE-M3) is the empirical backstop.

/// Resolve the intra chroma prediction mode from the signalled
/// `intra_chroma_pred_mode` and the luma mode of the same CU
/// (§8.4.2). Value 4 is direct mode (chroma copies the luma mode);
/// values 0..3 select from the candidate list [planar, vertical(26),
/// horizontal(10), DC], with a collision against the luma mode
/// replaced by mode 34. Mirrors libde265 `map_chroma_pred_mode`
/// (slice.cc) exactly; the syntax walker calls this so the tested
/// function is the shipped one.
pub fn resolve_chroma_mode(intra_chroma_pred_mode: u32, luma_mode: u32) -> u32 {
    if intra_chroma_pred_mode == 4 {
        luma_mode
    } else {
        let cand = [0u32, 26, 10, 1][intra_chroma_pred_mode as usize];
        if cand == luma_mode {
            34
        } else {
            cand
        }
    }
}

/// Whether sign-data-hiding hides the sign of one coefficient of a
/// sub-block: PPS flag on, and the first/last significant scan
/// positions of the sub-block more than 3 apart (§9.3.4.3.6,
/// libde265 slice.cc `signHidden = coeff_scan_pos span > 3`).
/// Positions are sub-block-local scan indices.
pub fn sign_hidden(pps_sdh_enabled: bool, first_sig_scan_pos: u32, last_sig_scan_pos: u32) -> bool {
    pps_sdh_enabled && last_sig_scan_pos.abs_diff(first_sig_scan_pos) > 3
}

/// Apply the hidden-sign derivation to one sub-block's signed
/// levels. `levels` are the sub-block's signed coefficient levels in
/// scan order with the hidden coefficient's sign provisionally
/// positive; `hidden` is its index (the coefficient with the
/// smallest scan position — the one whose sign bin is not read).
/// The hidden sign is chosen so the sum of the sub-block's signed
/// levels is even (§9.3.4.3.6; libde265 accumulates the signed sum
/// and flips the hidden coefficient when the sum is odd — the
/// parity of the signed sum equals the parity of the absolute sum,
/// since a sign flip changes the sum by an even amount).
pub fn apply_hidden_sign(levels: &mut [i32], hidden: usize) {
    let sum: i32 = levels.iter().sum();
    if sum % 2 != 0 {
        levels[hidden] = -levels[hidden];
    }
}

/// Chroma QP mapping (§8.6.1, libde265 `table8_22` in transform.h):
/// qPi < 30 maps to itself, 30..=42 through the table, >= 43 to
/// qPi - 6. Input is the already-clipped qPi (see `chroma_qp_prime`).
pub fn chroma_qp_from_qpi(qpi: i32) -> i32 {
    const TAB: [i32; 13] = [29, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37];
    if qpi < 30 {
        qpi
    } else if qpi >= 43 {
        qpi - 6
    } else {
        TAB[(qpi - 30) as usize]
    }
}

/// QpBdOffset for a bit depth: 6 * (bitDepth - 8) (§7.4.3).
pub fn qp_bd_offset(bit_depth: u32) -> i32 {
    6 * (bit_depth as i32 - 8)
}

/// Wrap a predicted QP plus delta into the legal QpY range
/// [-QpBdOffset, 51] (§8.6.1, libde265's modular form).
pub fn qp_wrap(pred: i32, delta: i32, bit_depth: u32) -> i32 {
    let off = qp_bd_offset(bit_depth);
    (pred + delta + 52 + 2 * off).rem_euclid(52 + off) - off
}

/// Final per-component QP (the "prime" value dequantization uses,
/// in [0, 51 + QpBdOffset]) for a TU, from the CU's QpY (§8.6.1):
/// luma is QpY + QpBdOffsetY; chroma maps QpY + pps offset + slice
/// offset through the chroma table after clipping to
/// [-QpBdOffsetC, 57], then adds QpBdOffsetC (floored at 0, as in
/// libde265). `offset` is pps + slice offset for the component.
pub fn tu_qp_prime(
    qp_y: i32,
    cidx: usize,
    offset: i32,
    bit_depth_luma: u32,
    bit_depth_chroma: u32,
) -> i32 {
    if cidx == 0 {
        qp_y + qp_bd_offset(bit_depth_luma)
    } else {
        let off_c = qp_bd_offset(bit_depth_chroma);
        let qpi = (qp_y + offset).clamp(-off_c, 57);
        (chroma_qp_from_qpi(qpi) + off_c).max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chroma_mode_resolution() {
        // Direct mode copies the luma mode.
        assert_eq!(resolve_chroma_mode(4, 17), 17);
        assert_eq!(resolve_chroma_mode(4, 0), 0);
        // Candidate list [0, 26, 10, 1]; collision -> 34.
        assert_eq!(resolve_chroma_mode(0, 5), 0);
        assert_eq!(resolve_chroma_mode(0, 0), 34);
        assert_eq!(resolve_chroma_mode(1, 5), 26);
        assert_eq!(resolve_chroma_mode(1, 26), 34);
        assert_eq!(resolve_chroma_mode(2, 5), 10);
        assert_eq!(resolve_chroma_mode(2, 10), 34);
        assert_eq!(resolve_chroma_mode(3, 5), 1);
        assert_eq!(resolve_chroma_mode(3, 1), 34);
    }

    #[test]
    fn sdh_condition() {
        assert!(sign_hidden(true, 0, 4));
        assert!(sign_hidden(true, 3, 15));
        assert!(!sign_hidden(true, 0, 3));
        assert!(!sign_hidden(true, 5, 5));
        assert!(!sign_hidden(false, 0, 15));
    }

    #[test]
    fn sdh_hidden_sign_parity() {
        // Hidden coefficient is index 3 (smallest scan position,
        // provisionally positive). Sum +3+5+2+7 = 17 (odd) -> flip.
        let mut lv = [3, -5, 2, 7];
        apply_hidden_sign(&mut lv, 3);
        assert_eq!(lv, [3, -5, 2, -7]);
        // Sum even -> unchanged.
        let mut lv = [3, -5, 2, 6];
        apply_hidden_sign(&mut lv, 3);
        assert_eq!(lv, [3, -5, 2, 6]);
        // Negative odd sum also flips.
        let mut lv = [-3, -5, -2, 7];
        apply_hidden_sign(&mut lv, 3);
        assert_eq!(lv, [-3, -5, -2, -7]);
    }

    #[test]
    fn sign_mapping_convention() {
        // coeff_sign_flag 1 means a negative level (§7.3.8.10 /
        // §9.3.4.3.6: level * (1 - 2*sign)); pinned here because the
        // residual capture applies it inline.
        let map = |sign: u32, level: i32| if sign == 1 { -level } else { level };
        assert_eq!(map(0, 9), 9);
        assert_eq!(map(1, 9), -9);
    }

    #[test]
    fn chroma_qp_table() {
        assert_eq!(chroma_qp_from_qpi(0), 0);
        assert_eq!(chroma_qp_from_qpi(29), 29);
        assert_eq!(chroma_qp_from_qpi(30), 29);
        assert_eq!(chroma_qp_from_qpi(34), 33);
        assert_eq!(chroma_qp_from_qpi(39), 35);
        assert_eq!(chroma_qp_from_qpi(42), 37);
        assert_eq!(chroma_qp_from_qpi(43), 37);
        assert_eq!(chroma_qp_from_qpi(51), 45);
        assert_eq!(chroma_qp_from_qpi(57), 51);
    }

    #[test]
    fn qp_wrap_cases() {
        // 8-bit: range -0..51, modulus 52.
        assert_eq!(qp_wrap(26, 0, 8), 26);
        assert_eq!(qp_wrap(51, 1, 8), 0);
        assert_eq!(qp_wrap(0, -1, 8), 51);
        assert_eq!(qp_wrap(30, -25, 8), 5);
        // 10-bit: offset 12, range -12..51, modulus 64.
        assert_eq!(qp_wrap(38, 0, 10), 38);
        assert_eq!(qp_wrap(51, 1, 10), -12);
        assert_eq!(qp_wrap(-12, -1, 10), 51);
    }

    #[test]
    fn tu_qp_prime_cases() {
        // 8-bit luma: identity shift.
        assert_eq!(tu_qp_prime(30, 0, 0, 8, 8), 30);
        // 8-bit chroma with the Apple/Canon +2 offset: qPi 32 -> 31.
        assert_eq!(tu_qp_prime(30, 1, 2, 8, 8), 31);
        // 10-bit luma: +12.
        assert_eq!(tu_qp_prime(30, 0, 0, 10, 10), 42);
        // 10-bit chroma: qPi = 30+2 = 32 -> table 31, +12 = 43.
        assert_eq!(tu_qp_prime(30, 2, 2, 10, 10), 43);
    }
}
