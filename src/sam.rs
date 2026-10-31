//! SAM 3.1 mask wire format — port of the Python boundary module
//! `sam_masks.py` and of Meta's official parser package
//! (`meta_sam_parser` 0.1.0, `_mask_codec.py` + the image path of
//! `_segmentation.py`).
//!
//! Wire format: output text is one line per frame, `<Nf>` then
//! comma-separated records
//! `id<|box;x1=..;y1=..;x2=..;y2=..;w=<frameW>;h=<frameH>|><|mask;x=0;y=0;data=<H>,<W>,<payload>|>`.
//! Box corners are inclusive on the wire, normalized to half-open
//! bounds here. The mask raster is box-local. The payload's first
//! character selects the encoding: `~` lossless (coverage raster,
//! thresholded at >= 129), `!` one_bit. Payloads are base85 (custom
//! alphabet) over an adaptive arithmetic/range code — all ported as
//! exact integer arithmetic.
//!
//! Failure semantics match the Python stack: malformed lines/records
//! and undecodable payloads drop the affected record (the parser's
//! diagnostics), they do not raise; `parse_sam_masks` errors only if
//! the text itself cannot be processed.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Base85 pack/unpack (the codec's `_pack` / `_unpack`).
// ---------------------------------------------------------------------------

const RADIX: u64 = 85;
const PREFIX_LENGTH: usize = 5;
const MAX_SAFE: u64 = (1 << 53) - 1;

fn alphabet() -> Vec<u8> {
    let excluded = *b"\"\\,;<>|";
    (0x21u8..0x7F).filter(|c| !excluded.contains(c)).collect()
}

fn digit_table() -> [i32; 128] {
    let mut table = [-1i32; 128];
    for (i, c) in alphabet().into_iter().enumerate() {
        table[c as usize] = i as i32;
    }
    table
}

fn digit(table: &[i32; 128], ch: u8) -> Result<u64, String> {
    let value = if (ch as usize) < 128 {
        table[ch as usize]
    } else {
        -1
    };
    if value < 0 || value as u64 >= RADIX {
        return Err("Mask payload contains an invalid character.".to_string());
    }
    Ok(value as u64)
}

fn pack(input: &[u8]) -> Result<String, String> {
    let alpha = alphabet();
    let length = input.len() as u64;
    let mut remaining = length;
    let mut prefix = vec![0u8; PREFIX_LENGTH];
    for index in (0..PREFIX_LENGTH).rev() {
        prefix[index] = alpha[(remaining % RADIX) as usize];
        remaining /= RADIX;
    }
    if remaining != 0 {
        return Err("Mask payload is too large.".to_string());
    }
    let mut output: Vec<u8> = prefix;
    let full = input.len() - (input.len() % 4);
    let mut offset = 0;
    while offset < full {
        let mut value = ((input[offset] as u32) << 24)
            | ((input[offset + 1] as u32) << 16)
            | ((input[offset + 2] as u32) << 8)
            | input[offset + 3] as u32;
        let mut digits = [0usize; 5];
        for index in (0..5).rev() {
            digits[index] = (value % 85) as usize;
            value = (value - digits[index] as u32) / 85;
        }
        output.extend(digits.iter().map(|&d| alpha[d]));
        offset += 4;
    }
    let remainder = input.len() - offset;
    if remainder > 0 {
        let mut value: u32 = 0;
        for index in 0..4 {
            let b = if index < remainder {
                input[offset + index]
            } else {
                0
            };
            value = (value << 8) | b as u32;
        }
        let mut digits = [0usize; 5];
        for index in (0..5).rev() {
            digits[index] = (value % 85) as usize;
            value = (value - digits[index] as u32) / 85;
        }
        output.extend((0..remainder + 1).map(|i| alpha[digits[i]]));
    }
    String::from_utf8(output).map_err(|_| "pack produced non-ASCII".to_string())
}

fn unpack(payload: &str) -> Result<Vec<u8>, String> {
    let table = digit_table();
    let bytes = payload.as_bytes();
    // The Python length check counts UTF-16 code units; payloads are
    // ASCII in every valid stream (the alphabet is ASCII), so byte
    // length is the same count for anything that can succeed.
    if bytes.len() < PREFIX_LENGTH {
        return Err("Mask payload is missing its length prefix.".to_string());
    }
    let mut length: u64 = 0;
    for &ch in &bytes[..PREFIX_LENGTH] {
        length = length * RADIX + digit(&table, ch)?;
    }
    if length > MAX_SAFE {
        return Err("Mask payload length is unsupported.".to_string());
    }
    let length = length as usize;
    let remainder = length % 4;
    let expected =
        PREFIX_LENGTH + (length / 4) * 5 + if remainder == 0 { 0 } else { remainder + 1 };
    if bytes.len() != expected {
        return Err("Mask payload length does not match its prefix.".to_string());
    }
    let mut output = vec![0u8; length];
    let mut source = PREFIX_LENGTH;
    let mut destination = 0usize;
    let full = length - remainder;
    while destination < full {
        let mut value: u64 = 0;
        for index in 0..5 {
            value = value * RADIX + digit(&table, bytes[source + index])?;
        }
        if value > 0xFFFF_FFFF {
            return Err("Mask payload contains an out-of-range group.".to_string());
        }
        let value = value as u32;
        source += 5;
        output[destination] = (value >> 24) as u8;
        output[destination + 1] = (value >> 16) as u8;
        output[destination + 2] = (value >> 8) as u8;
        output[destination + 3] = value as u8;
        destination += 4;
    }
    if remainder > 0 {
        let mut value: u64 = 0;
        for index in 0..5 {
            let d = if index < remainder + 1 {
                digit(&table, bytes[source + index])?
            } else {
                RADIX - 1
            };
            value = value * RADIX + d;
        }
        if value > 0xFFFF_FFFF {
            return Err("Mask payload contains an out-of-range tail.".to_string());
        }
        let value = value as u32;
        for index in 0..remainder {
            output[destination + index] = (value >> (24 - index * 8)) as u8;
        }
    }
    if pack(&output)? != payload {
        return Err("Mask payload is not in canonical form.".to_string());
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// Range coder (the codec's `_RangeEncoder` / `_RangeDecoder`).
// ---------------------------------------------------------------------------

const TOP: u32 = 1 << 24;
const BOTTOM: u32 = 1 << 16;

struct RangeEncoder {
    low: u32,
    range: u32,
    output: Vec<u8>,
}

impl RangeEncoder {
    fn new() -> Self {
        RangeEncoder {
            low: 0,
            range: 0xFFFF_FFFF,
            output: Vec::new(),
        }
    }

    fn encode(&mut self, cumulative: u32, frequency: u32, total: u32) {
        let scaled = self.range / total;
        self.low = self.low.wrapping_add(scaled.wrapping_mul(cumulative));
        self.range = scaled.wrapping_mul(frequency);
        loop {
            if self.low ^ self.low.wrapping_add(self.range) < TOP {
                // pass
            } else if self.range < BOTTOM {
                self.range = (0u32.wrapping_sub(self.low)) & (BOTTOM - 1);
            } else {
                break;
            }
            self.output.push((self.low >> 24) as u8);
            self.low <<= 8;
            self.range <<= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        for _ in 0..4 {
            self.output.push((self.low >> 24) as u8);
            self.low <<= 8;
        }
        self.output
    }
}

struct RangeDecoder<'a> {
    input: &'a [u8],
    position: usize,
    low: u32,
    range: u32,
    code: u32,
    scaled: u32,
}

impl<'a> RangeDecoder<'a> {
    fn new(input: &'a [u8]) -> Result<Self, String> {
        if input.len() < 4 {
            return Err("Mask payload is missing its finalization.".to_string());
        }
        let mut dec = RangeDecoder {
            input,
            position: 0,
            low: 0,
            range: 0xFFFF_FFFF,
            code: 0,
            scaled: 0,
        };
        for _ in 0..4 {
            let b = dec.read()?;
            dec.code = (dec.code << 8) | b as u32;
        }
        Ok(dec)
    }

    fn read(&mut self) -> Result<u8, String> {
        if self.position >= self.input.len() {
            return Err("Mask payload ended before decoding completed.".to_string());
        }
        let value = self.input[self.position];
        self.position += 1;
        Ok(value)
    }

    fn frequency(&mut self, total: u32) -> Result<u32, String> {
        self.scaled = self.range / total;
        if self.scaled == 0 {
            return Err("Mask payload has invalid coding state.".to_string());
        }
        let value = self.code.wrapping_sub(self.low) / self.scaled;
        Ok(if value >= total { total - 1 } else { value })
    }

    fn decode(&mut self, cumulative: u32, frequency: u32) -> Result<(), String> {
        self.low = self.low.wrapping_add(self.scaled.wrapping_mul(cumulative));
        self.range = self.scaled.wrapping_mul(frequency);
        loop {
            if self.low ^ self.low.wrapping_add(self.range) < TOP {
                // pass
            } else if self.range < BOTTOM {
                self.range = (0u32.wrapping_sub(self.low)) & (BOTTOM - 1);
            } else {
                break;
            }
            let b = self.read()?;
            self.code = (self.code << 8) | b as u32;
            self.low <<= 8;
            self.range <<= 8;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// one_bit codec (context model over a padded raster).
// ---------------------------------------------------------------------------

const INCREMENT: u16 = 14;
const COUNT_LIMIT: u32 = 4096;
const CONTEXT_COUNT: usize = 1 << 12;
const PADDING: usize = 2;
const SYMBOLS: usize = 256;

fn context_at(raster: &[u8], position: usize, prior: usize, earlier: usize) -> usize {
    (raster[position - 1] as usize)
        | ((raster[position - 2] as usize) << 1)
        | ((raster[prior - 2] as usize) << 2)
        | ((raster[prior - 1] as usize) << 3)
        | ((raster[prior] as usize) << 4)
        | ((raster[prior + 1] as usize) << 5)
        | ((raster[prior + 2] as usize) << 6)
        | ((raster[earlier - 2] as usize) << 7)
        | ((raster[earlier - 1] as usize) << 8)
        | ((raster[earlier] as usize) << 9)
        | ((raster[earlier + 1] as usize) << 10)
        | ((raster[earlier + 2] as usize) << 11)
}

/// Python's `(v >> 1) or 1` rescale step.
fn halve_or_one_u16(v: u16) -> u16 {
    let halved = v >> 1;
    if halved == 0 {
        1
    } else {
        halved
    }
}

fn halve_or_one_u32(v: u32) -> u32 {
    let halved = v >> 1;
    if halved == 0 {
        1
    } else {
        halved
    }
}

fn update_counts(counts: &mut [u16], offset: usize, bit: usize) {
    counts[offset + bit] = counts[offset + bit].wrapping_add(INCREMENT);
    if counts[offset] as u32 + counts[offset + 1] as u32 >= COUNT_LIMIT {
        counts[offset] = halve_or_one_u16(counts[offset]);
        counts[offset + 1] = halve_or_one_u16(counts[offset + 1]);
    }
}

fn encode_raster(input: &[u8], width: usize, height: usize) -> Result<String, String> {
    let mut encoder = RangeEncoder::new();
    let mut counts = vec![1u16; CONTEXT_COUNT * 2];
    let padded_width = width + 2 * PADDING;
    let mut padded = vec![0u8; padded_width * (height + PADDING)];
    for y in 0..height {
        let row = (y + PADDING) * padded_width + PADDING;
        for x in 0..width {
            let position = row + x;
            let context = context_at(
                &padded,
                position,
                position - padded_width,
                position - 2 * padded_width,
            );
            let offset = context << 1;
            let zero = counts[offset] as u32;
            let one = counts[offset + 1] as u32;
            let bit = input[y * width + x];
            if bit > 1 {
                return Err("Decoded mask is not binary.".to_string());
            }
            if bit == 0 {
                encoder.encode(0, zero, zero + one);
            } else {
                encoder.encode(zero, one, zero + one);
            }
            padded[position] = bit;
            update_counts(&mut counts, offset, bit as usize);
        }
    }
    Ok(format!("!{}", pack(&encoder.finish())?))
}

fn decode_raster(payload: &str, width: usize, height: usize) -> Result<Vec<u8>, String> {
    let packed = unpack(&payload[1..])?;
    let mut decoder = RangeDecoder::new(&packed)?;
    let mut counts = vec![1u16; CONTEXT_COUNT * 2];
    let padded_width = width + 2 * PADDING;
    let mut padded = vec![0u8; padded_width * (height + PADDING)];
    let mut output = vec![0u8; width * height];
    for y in 0..height {
        let row = (y + PADDING) * padded_width + PADDING;
        for x in 0..width {
            let position = row + x;
            let context = context_at(
                &padded,
                position,
                position - padded_width,
                position - 2 * padded_width,
            );
            let offset = context << 1;
            let zero = counts[offset] as u32;
            let one = counts[offset + 1] as u32;
            let value = decoder.frequency(zero + one)?;
            let bit: u8 = if value < zero { 0 } else { 1 };
            if bit == 0 {
                decoder.decode(0, zero)?;
            } else {
                decoder.decode(zero, one)?;
            }
            padded[position] = bit;
            output[y * width + x] = bit;
            update_counts(&mut counts, offset, bit as usize);
        }
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// lossless codec (predictor + zig-zag residuals + adaptive models).
// ---------------------------------------------------------------------------

const ZERO_INCREMENT: u32 = 32;
const ZERO_LIMIT: u32 = 16384;
const ORDER_ONE_INCREMENT: u16 = 56;
const ORDER_ONE_LIMIT: i32 = 8192;
const ORDER_TWO_STEP: u16 = 14 * 44;
const ORDER_TWO_LIMIT: u16 = (14 * 3584) as u16;
const ORDER_ZERO_STEP: u16 = 2 * 16;
const ORDER_ZERO_LIMIT: i32 = 2 * 2048;
const ORDER_ZERO_INITIAL: u16 = 2;
const ORDER_ONE_INITIAL: u16 = 1;
const SPATIAL_MODES: i32 = 5;

fn paeth(left: i32, up: i32, upper_left: i32) -> i32 {
    let estimate = left + up - upper_left;
    let left_distance = (estimate - left).abs();
    let up_distance = (estimate - up).abs();
    let upper_left_distance = (estimate - upper_left).abs();
    if left_distance <= up_distance && left_distance <= upper_left_distance {
        left
    } else if up_distance <= upper_left_distance {
        up
    } else {
        upper_left
    }
}

fn predict_byte(
    output: &[u8],
    offset: usize,
    x: usize,
    y: usize,
    mode: i32,
    width: usize,
) -> Result<u8, String> {
    if !(0..SPATIAL_MODES).contains(&mode) {
        return Err("Lossless mask payload uses an invalid predictor.".to_string());
    }
    let left = if x > 0 { output[offset - 1] } else { 0 };
    let up = if y > 0 { output[offset - width] } else { 0 };
    if mode == 1 {
        return Ok(up);
    }
    if mode == 2 {
        return Ok(left);
    }
    if mode == 3 {
        return Ok(((left as u32 + up as u32) >> 1) as u8);
    }
    let upper_left = if x > 0 && y > 0 {
        output[offset - width - 1]
    } else {
        0
    };
    if mode == 0 {
        return Ok(paeth(left as i32, up as i32, upper_left as i32) as u8);
    }
    // mode == 4: gradient predictor.
    let (l, u, ul) = (left as i32, up as i32, upper_left as i32);
    if ul >= l.max(u) {
        return Ok(l.min(u) as u8);
    }
    if ul <= l.min(u) {
        return Ok(l.max(u) as u8);
    }
    Ok((l + u - ul) as u8)
}

fn unzig(value: u8, prediction: u8) -> u8 {
    let v = value as i32;
    let delta = if v & 1 != 0 { -((v + 1) >> 1) } else { v >> 1 };
    (prediction as i32 + delta) as u8
}

fn zero_context(residuals: &[u8], offset: usize, x: usize, y: usize, width: usize) -> usize {
    let left = if x > 0 {
        (residuals[offset - 1] == 0) as usize
    } else {
        1
    };
    let up = if y > 0 {
        (residuals[offset - width] == 0) as usize
    } else {
        1
    };
    let upper_left = if x > 0 && y > 0 {
        (residuals[offset - width - 1] == 0) as usize
    } else {
        1
    };
    let upper_right = if y > 0 && x < width - 1 {
        (residuals[offset - width + 1] == 0) as usize
    } else {
        1
    };
    left | (up << 1) | (upper_left << 2) | (upper_right << 3)
}

fn decode_lossless_raster(payload: &str, width: usize, height: usize) -> Result<Vec<u8>, String> {
    if !payload.starts_with('~') {
        return Err("lossless mask payloads must start with ~.".to_string());
    }
    let packed = unpack(&payload[1..])?;
    if packed.len() < 5 {
        return Err("Lossless mask payload is truncated.".to_string());
    }
    let selector = packed[0] as i32;
    let mut decoder = RangeDecoder::new(&packed[1..])?;
    let length = width * height;
    let mut output = vec![0u8; length];
    let mut residuals = vec![0u8; length];
    let mut zero_counts = [1u32; 32];
    let mut order_one = vec![ORDER_ONE_INITIAL; SYMBOLS * SYMBOLS];
    let mut order_one_totals = vec![SYMBOLS as i32 * ORDER_ONE_INITIAL as i32; SYMBOLS];
    let mut order_zero = vec![ORDER_ZERO_INITIAL; SYMBOLS];
    let mut order_zero_total: i32 = SYMBOLS as i32 * ORDER_ZERO_INITIAL as i32;
    let mut order_two: HashMap<i32, Vec<u16>> = HashMap::new();
    let mut previous: i32 = 0;

    for offset in 0..length {
        let x = offset % width;
        let y = offset / width;
        let zero_offset = zero_context(&residuals, offset, x, y, width) << 1;
        let zero = zero_counts[zero_offset];
        let nonzero = zero_counts[zero_offset + 1];
        let zero_frequency = decoder.frequency(zero + nonzero)?;
        let bit: u32 = if zero_frequency < zero { 0 } else { 1 };
        if bit == 0 {
            decoder.decode(0, zero)?;
        } else {
            decoder.decode(zero, nonzero)?;
        }
        zero_counts[zero_offset + bit as usize] =
            zero_counts[zero_offset + bit as usize].wrapping_add(ZERO_INCREMENT);
        if zero_counts[zero_offset] + zero_counts[zero_offset + 1] >= ZERO_LIMIT {
            zero_counts[zero_offset] = halve_or_one_u32(zero_counts[zero_offset]);
            zero_counts[zero_offset + 1] = halve_or_one_u32(zero_counts[zero_offset + 1]);
        }

        let mut symbol: u8 = 0;
        if bit == 1 {
            let above = if offset >= width {
                residuals[offset - width]
            } else {
                0
            };
            let key = (previous << 8) | above as i32;
            let second = order_two
                .entry(key)
                .or_insert_with(|| vec![0u16; SYMBOLS + 1]);
            let first_offset = (previous as usize) << 8;
            let excluded_zero =
                second[0] as i32 + order_one[first_offset] as i32 + order_zero[0] as i32;
            let total =
                second[SYMBOLS] as i32 + order_one_totals[previous as usize] + order_zero_total
                    - excluded_zero;
            let target = decoder.frequency(total as u32)?;
            let mut cumulative: i32 = 0;
            let mut sym: usize = 1;
            let mut frequency =
                second[1] as i32 + order_one[first_offset + 1] as i32 + order_zero[1] as i32;
            while cumulative + frequency <= target as i32 {
                cumulative += frequency;
                sym += 1;
                if sym >= SYMBOLS {
                    return Err("Lossless mask payload is malformed.".to_string());
                }
                frequency = second[sym] as i32
                    + order_one[first_offset + sym] as i32
                    + order_zero[sym] as i32;
            }
            decoder.decode(cumulative as u32, frequency as u32)?;
            // Model updates (second is re-borrowed after decode).
            let second = order_two.get_mut(&key).expect("inserted above");
            second[sym] = second[sym].wrapping_add(ORDER_TWO_STEP);
            second[SYMBOLS] = second[SYMBOLS].wrapping_add(ORDER_TWO_STEP);
            if second[SYMBOLS] >= ORDER_TWO_LIMIT {
                let mut sum: u32 = 0;
                for v in second.iter_mut().take(SYMBOLS) {
                    *v >>= 1;
                    sum += *v as u32;
                }
                second[SYMBOLS] = sum as u16;
            }
            order_one[first_offset + sym] =
                order_one[first_offset + sym].wrapping_add(ORDER_ONE_INCREMENT);
            order_one_totals[previous as usize] += ORDER_ONE_INCREMENT as i32;
            if order_one_totals[previous as usize] >= ORDER_ONE_LIMIT {
                let mut sum: i32 = 0;
                for v in 0..SYMBOLS {
                    let scaled = halve_or_one_u16(order_one[first_offset + v]);
                    order_one[first_offset + v] = scaled;
                    sum += scaled as i32;
                }
                order_one_totals[previous as usize] = sum;
            }
            order_zero[sym] = order_zero[sym].wrapping_add(ORDER_ZERO_STEP);
            order_zero_total += ORDER_ZERO_STEP as i32;
            if order_zero_total >= ORDER_ZERO_LIMIT {
                let mut sum: i32 = 0;
                for v in order_zero.iter_mut() {
                    *v >>= 1;
                    sum += *v as i32;
                }
                order_zero_total = sum;
            }
            symbol = sym as u8;
        }
        residuals[offset] = symbol;
        output[offset] = unzig(
            symbol,
            predict_byte(&output, offset, x, y, selector, width)?,
        );
        previous = symbol as i32;
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// decode_mask_to_raster.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MaskEncoding {
    OneBit,
    Lossless,
}

#[derive(Clone, Debug)]
pub struct SegmentationMask {
    pub encoding: MaskEncoding,
    /// The payload INCLUDING its `!`/`~` marker (as on the wire).
    pub payload: String,
    pub width: i64,
    pub height: i64,
}

/// Strictly decode one mask payload to a 0/1 raster (row-major,
/// width x height). Port of `decode_mask_to_raster`.
pub fn decode_mask_to_raster(mask: &SegmentationMask) -> Result<Vec<u8>, String> {
    if mask.width <= 0 || mask.height <= 0 {
        return Err("Mask dimensions must be positive safe integers.".to_string());
    }
    let width = mask.width as usize;
    let height = mask.height as usize;
    match mask.encoding {
        MaskEncoding::Lossless => {
            if !mask.payload.starts_with('~') {
                return Err("lossless mask payloads must start with ~.".to_string());
            }
            let coverage = decode_lossless_raster(&mask.payload, width, height)?;
            Ok(coverage
                .iter()
                .map(|&v| if v >= 129 { 1 } else { 0 })
                .collect())
        }
        MaskEncoding::OneBit => {
            if !mask.payload.starts_with('!') {
                return Err("one_bit mask payloads must start with !.".to_string());
            }
            let raster = decode_raster(&mask.payload, width, height)?;
            if encode_raster(&raster, width, height)? != mask.payload {
                return Err(
                    "Mask payload is not canonical or has invalid finalization.".to_string()
                );
            }
            Ok(raster)
        }
    }
}

// ---------------------------------------------------------------------------
// Wire text parsing (the image path of `_SegmentationParser`).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct DecodedMask {
    pub object_id: String,
    /// Half-open bounds inside the frame: (left, top, right, bottom).
    pub bounds: (i64, i64, i64, i64),
    /// Mask pixels inside the record's own raster.
    pub mask_pixels: u64,
    /// Full-frame binary mask as an L raster (0/255), row-major.
    pub image: Vec<u8>,
    pub frame_width: usize,
    pub frame_height: usize,
}

fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn trim_js(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// Parse `;`-separated fields the way `_read_fields` does. Returns
/// None when a known key repeats (other than `c`).
fn read_fields(
    body: Option<&str>,
    known: &[&str],
) -> Option<std::collections::HashMap<String, Option<String>>> {
    let mut values: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    let Some(body) = body else {
        return Some(values);
    };
    for part in body.split(';') {
        let field = trim_js(part);
        if field.is_empty() {
            continue;
        }
        let (key_text, value) = match field.split_once('=') {
            Some((k, v)) => (k, Some(trim_js(v).to_string())),
            None => (field, None),
        };
        let key = trim_js(key_text);
        if !known.contains(&key) {
            continue; // unknown field: ignored with a warning upstream
        }
        if values.contains_key(key) {
            if key != "c" {
                return None;
            }
            continue;
        }
        values.insert(key.to_string(), value);
    }
    Some(values)
}

fn is_signed_int(s: &str) -> bool {
    let t = s.strip_prefix('-').unwrap_or(s);
    !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit())
}

fn is_unsigned_int(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn parse_safe_int(s: &str) -> Option<i64> {
    let v: i64 = s.parse().ok()?;
    if v.unsigned_abs() > MAX_SAFE {
        None
    } else {
        Some(v)
    }
}

struct ApiRecord {
    object_id: String,
    coords_dims: (String, String, String, String, String, String),
    mask_height: i64,
    mask_width: i64,
    payload: String,
}

const BOX_KEYS: [&str; 7] = ["x1", "y1", "x2", "y2", "w", "h", "c"];
const MASK_KEYS: [&str; 4] = ["x", "y", "data", "c"];

fn read_api_record(object_id: &str, tokens: &[(String, Option<String>)]) -> Option<ApiRecord> {
    let box_tokens: Vec<_> = tokens.iter().filter(|(n, _)| n == "box").collect();
    let mask_tokens: Vec<_> = tokens.iter().filter(|(n, _)| n == "mask").collect();
    if box_tokens.len() != 1 || mask_tokens.len() != 1 {
        return None;
    }
    let box_fields = read_fields(box_tokens[0].1.as_deref(), &BOX_KEYS)?;
    let mask_fields = read_fields(mask_tokens[0].1.as_deref(), &MASK_KEYS)?;
    let get = |fields: &std::collections::HashMap<String, Option<String>>, key: &str| {
        fields.get(key).cloned().flatten()
    };
    let coords: Vec<Option<String>> = ["x1", "y1", "x2", "y2"]
        .iter()
        .map(|k| get(&box_fields, k))
        .collect();
    let dims: Vec<Option<String>> = ["w", "h"].iter().map(|k| get(&box_fields, k)).collect();
    if coords
        .iter()
        .any(|v| v.as_deref().map(|s| !is_signed_int(s)).unwrap_or(true))
        || dims
            .iter()
            .any(|v| v.as_deref().map(|s| !is_unsigned_int(s)).unwrap_or(true))
        || get(&mask_fields, "x").as_deref() != Some("0")
        || get(&mask_fields, "y").as_deref() != Some("0")
    {
        return None;
    }
    let data = get(&mask_fields, "data")?;
    // data fullmatch: ([0-9]+),([0-9]+),([!~][^|]+)
    let mut parts = data.splitn(3, ',');
    let h_part = parts.next()?;
    let w_part = parts.next()?;
    let payload = parts.next()?;
    if !is_unsigned_int(h_part) || !is_unsigned_int(w_part) {
        return None;
    }
    if payload.is_empty() || !(payload.starts_with('!') || payload.starts_with('~')) {
        return None;
    }
    if payload.contains('|') {
        return None;
    }
    Some(ApiRecord {
        object_id: object_id.to_string(),
        coords_dims: (
            coords[0].clone().unwrap(),
            coords[1].clone().unwrap(),
            coords[2].clone().unwrap(),
            coords[3].clone().unwrap(),
            dims[0].clone().unwrap(),
            dims[1].clone().unwrap(),
        ),
        mask_height: h_part.parse().ok()?,
        mask_width: w_part.parse().ok()?,
        payload: payload.to_string(),
    })
}

/// Place a box-local 0/1 raster into a full-frame L image (0/255),
/// clipped against the frame edges (port of `_place_in_frame`).
///
/// The record's bounds can outrun the raster: SAM returned a
/// border-touching fridge mask on IMG_1351 whose box was larger than
/// its box-local raster, in the wild. The Python reference places the
/// raster through PIL crop, which zero-pads any overhang, so source
/// coordinates outside the raster read as 0 here too. Indexing the
/// raster unchecked panicked the server (index out of bounds at the
/// old line 863) and killed the process mid-call.
fn place_in_frame(
    raster: &[u8],
    width: usize,
    height: usize,
    bounds: (i64, i64, i64, i64),
    frame_width: usize,
    frame_height: usize,
) -> Vec<u8> {
    let (left, top, right, bottom) = bounds;
    let mut frame = vec![0u8; frame_width * frame_height];
    let dst_left = left.max(0);
    let dst_top = top.max(0);
    let dst_right = right.min(frame_width as i64);
    let dst_bottom = bottom.min(frame_height as i64);
    if dst_right <= dst_left || dst_bottom <= dst_top {
        return frame;
    }
    for dy in dst_top..dst_bottom {
        for dx in dst_left..dst_right {
            let sx = (dx - left) as usize;
            let sy = (dy - top) as usize;
            if sx < width && sy < height && raster[sy * width + sx] != 0 {
                frame[dy as usize * frame_width + dx as usize] = 255;
            }
        }
    }
    frame
}

/// Decode every mask record in one SAM image response (port of
/// `parse_sam_masks`). An empty result is a normal zero-match.
pub fn parse_sam_masks(
    output_text: &str,
    frame_width: usize,
    frame_height: usize,
) -> Result<Vec<DecodedMask>, String> {
    let mut masks: Vec<DecodedMask> = Vec::new();
    for raw_line in output_text.split('\n') {
        let raw_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let line = trim_js(raw_line);
        if line.is_empty() || !line.starts_with('<') {
            continue; // blank or text record: no masks
        }
        // API header: ^<([0-9]+)f(?:;([^>]*))?>(.*)$ — the remainder
        // must not contain line terminators (already split away).
        let Some(rest) = line.strip_prefix('<') else {
            continue;
        };
        let digit_end = rest.find(|c: char| !c.is_ascii_digit());
        let Some(digit_end) = digit_end else { continue };
        if digit_end == 0 {
            continue;
        }
        let (frame_digits, after_digits) = rest.split_at(digit_end);
        let Some(after_f) = after_digits.strip_prefix('f') else {
            continue;
        };
        // Optional ;fields then '>'.
        let body: &str = if let Some(stripped) = after_f.strip_prefix(';') {
            match stripped.find('>') {
                Some(idx) => &stripped[idx + 1..],
                None => continue,
            }
        } else if let Some(stripped) = after_f.strip_prefix('>') {
            stripped
        } else {
            continue;
        };
        let frame_index = match parse_safe_int(frame_digits) {
            Some(v) if v >= 0 => v,
            _ => continue, // invalid frame: diagnostic upstream, line dropped
        };
        if frame_index != 0 {
            continue; // image media requires frame zero
        }
        if body.is_empty() {
            continue; // malformed record diagnostic upstream
        }
        accept_records(body, frame_width, frame_height, &mut masks);
    }
    Ok(masks)
}

/// Scan records out of one API line body; a malformed record stops the
/// line (records already accepted stay), matching the Python parser.
fn accept_records(
    body: &str,
    frame_width: usize,
    frame_height: usize,
    masks: &mut Vec<DecodedMask>,
) {
    let bytes = body.as_bytes();
    let mut position = 0usize;
    while position < bytes.len() {
        // Record start: ,?([0-9]+)(?=<\|)
        let mut p = position;
        if bytes[p] == b',' {
            p += 1;
        }
        let digit_start = p;
        while p < bytes.len() && bytes[p].is_ascii_digit() {
            p += 1;
        }
        if p == digit_start || !body[p..].starts_with("<|") {
            return; // malformed record: line stops
        }
        let object_id = &body[digit_start..p];
        let mut end = p;
        let mut tokens: Vec<(String, Option<String>)> = Vec::new();
        // Tokens: <\|([A-Za-z][A-Za-z0-9_.-]*)(?:;([^|]*))?\|\>
        loop {
            if !body[end..].starts_with("<|") {
                break;
            }
            let token_body_start = end + 2;
            let Some(close) = body[token_body_start..].find("|>") else {
                break;
            };
            let token_text = &body[token_body_start..token_body_start + close];
            let (name, fields) = match token_text.split_once(';') {
                Some((n, f)) => (n.to_string(), Some(f.to_string())),
                None => (token_text.to_string(), None),
            };
            // Validate the token name shape: [A-Za-z][A-Za-z0-9_.-]*
            let name_ok = name
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic())
                .unwrap_or(false)
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
            if !name_ok {
                break;
            }
            tokens.push((name, fields));
            end = token_body_start + close + 2;
        }
        // Boundary check: next char must be ',' or a digit (or end).
        if end < bytes.len() && bytes[end] != b',' && !bytes[end].is_ascii_digit() {
            return;
        }
        let Some(record) = read_api_record(object_id, &tokens) else {
            return; // malformed record: line stops
        };
        let coords: Vec<Option<i64>> = [
            &record.coords_dims.0,
            &record.coords_dims.1,
            &record.coords_dims.2,
            &record.coords_dims.3,
            &record.coords_dims.4,
            &record.coords_dims.5,
        ]
        .iter()
        .map(|s| parse_safe_int(s))
        .collect();
        if coords.iter().any(|c| c.is_none()) {
            return; // invalid box: line stops
        }
        let (left, top, incl_right, incl_bottom, source_w, source_h) = (
            coords[0].unwrap(),
            coords[1].unwrap(),
            coords[2].unwrap(),
            coords[3].unwrap(),
            coords[4].unwrap(),
            coords[5].unwrap(),
        );
        if source_w <= 0
            || source_h <= 0
            || left < 0
            || top < 0
            || incl_right < left
            || incl_bottom < top
            || incl_right >= source_w
            || incl_bottom >= source_h
        {
            return; // invalid box: line stops
        }
        let bounds = (left, top, incl_right + 1, incl_bottom + 1);
        let mask = SegmentationMask {
            encoding: if record.payload.starts_with('~') {
                MaskEncoding::Lossless
            } else {
                MaskEncoding::OneBit
            },
            payload: record.payload.clone(),
            width: record.mask_width,
            height: record.mask_height,
        };
        // _accept_mask: size check, then a trial decode; failure
        // diagnoses and skips this mask (the line continues).
        if mask.width <= 0 || mask.height <= 0 {
            position = end;
            continue;
        }
        match decode_mask_to_raster(&mask) {
            Ok(raster) => {
                let mask_pixels: u64 = raster.iter().map(|&v| v as u64).sum();
                let image = place_in_frame(
                    &raster,
                    mask.width as usize,
                    mask.height as usize,
                    bounds,
                    frame_width,
                    frame_height,
                );
                masks.push(DecodedMask {
                    object_id: record.object_id.clone(),
                    bounds,
                    mask_pixels,
                    image,
                    frame_width,
                    frame_height,
                });
            }
            Err(_) => {
                // invalid payload: skipped, line continues
            }
        }
        position = end;
    }
}
