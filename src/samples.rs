//! Sample data extraction for S3M files.
//!
//! Each PCM instrument's sample body lives at `instrument.sample_parapointer
//! << 4`. S3M samples can be:
//!
//! - **8-bit unsigned** (FFI = 2 in the header, the ST3-standard format).
//! - **8-bit signed** (FFI = 1 — rare, older tools).
//! - **16-bit** (flag bit 2 set) — LE unsigned by convention.
//! - **Stereo** (flag bit 1 set) — block-sequential: the whole left
//!   channel (`length` samples) is followed by the whole right channel,
//!   not per-frame interleaved.
//! - **DP30ADPCM-packed** (pack byte = 1) — 4-bit delta-packed 8-bit mono
//!   data: a 16-entry signed delta table followed by one nibble per output
//!   sample, low nibble first (see `decode_dp30adpcm` for the layout).
//!
//! We convert everything up to signed 16-bit. For mono samples `pcm_right`
//! is `None` and mixing uses the single `pcm` buffer as both L and R. For
//! true stereo samples we decode both sides into `pcm` (left) and
//! `pcm_right` (right) and the mixer routes them through the channel pan.

use crate::header::{Instrument, S3mHeader, PACK_DP30ADPCM};

/// Decoded sample body ready for mixing.
#[derive(Clone, Debug, Default)]
pub struct SampleBody {
    /// Signed 16-bit PCM; for stereo samples this is the left channel. Empty
    /// if the instrument had no data.
    pub pcm: Vec<i16>,
    /// Right channel for true-stereo samples. `None` for mono samples —
    /// the mixer then uses `pcm` for both L and R before panning.
    pub pcm_right: Option<Vec<i16>>,
    /// Loop start in samples (0 if not looped).
    pub loop_start: u32,
    /// Loop end in samples (exclusive).
    pub loop_end: u32,
    /// Whether this sample should loop on playback.
    pub looped: bool,
    /// Default volume 0..=64.
    pub volume: u8,
    /// C5 (middle-C) playback rate in Hz.
    pub c5_speed: u32,
    /// Decoded OPL2 register block for an AdLib (`SCRI`, type 2..=7)
    /// instrument. `None` for PCM instruments and empty slots. AdLib
    /// instruments carry no PCM (`pcm` stays empty) — the player
    /// synthesizes them through [`crate::opl2::OplVoice`] instead of the
    /// sample mixer.
    pub adlib: Option<crate::opl2::AdLibInstrument>,
}

impl SampleBody {
    pub fn is_looped(&self) -> bool {
        self.looped && self.loop_end > self.loop_start
    }

    pub fn loop_length(&self) -> u32 {
        self.loop_end.saturating_sub(self.loop_start)
    }
}

/// Convert one instrument's raw bytes to a `SampleBody`.
///
/// `signed_samples` selects how to interpret 8-bit PCM (FFI = 1 in the
/// file-format-info field); 16-bit samples follow ST3's convention of
/// "unsigned" regardless of FFI (but in practice, modern players assume
/// signed for 16-bit too — we follow ST3).
pub fn decode_instrument(inst: &Instrument, bytes: &[u8], signed_samples: bool) -> SampleBody {
    if !inst.is_pcm() || inst.length == 0 {
        // AdLib (`SCRI`) instruments land here: no PCM body, but the
        // decoded OPL2 register block rides along for the FM voice path.
        return SampleBody {
            volume: inst.volume,
            c5_speed: inst.c5_speed.max(1),
            adlib: inst.adlib_instrument(),
            ..Default::default()
        };
    }
    let off = inst.sample_byte_offset();
    let len = inst.length as usize;
    if inst.pack == PACK_DP30ADPCM {
        // DP30ADPCM (pack type 1): 4-bit delta-packed 8-bit samples. The
        // packing is defined only for 8-bit mono data — there is no 16-bit
        // or stereo variant — so a hostile pack=1 body carrying those flag
        // bits is still decoded as the documented 8-bit mono layout. The
        // FFI signed/unsigned convention likewise does not apply: the
        // decoded stream is signed 8-bit by definition. Total packed size
        // on disk = 16-byte delta table + one nibble per sample (rounded
        // up), bounded to the bytes actually present
        // (`docs/audio/trackers/s3m/
        // s3m-position-jump-pattern-break-and-adpcm.md` §Part 2).
        let needed = 16usize.saturating_add(len.div_ceil(2));
        let end = off.saturating_add(needed).min(bytes.len());
        let pcm = if off < end {
            decode_dp30adpcm(&bytes[off..end], len)
        } else {
            Vec::new()
        };
        let loop_start = inst.loop_start.min(pcm.len() as u32);
        let loop_end = inst.loop_end.min(pcm.len() as u32);
        let looped = inst.is_looped() && loop_end > loop_start;
        return SampleBody {
            pcm,
            pcm_right: None,
            loop_start,
            loop_end,
            looped,
            volume: inst.volume,
            c5_speed: inst.c5_speed.max(1),
            adlib: None,
        };
    }
    let is_16 = inst.is_16bit();
    let is_stereo = inst.is_stereo();
    let bytes_per_frame = if is_16 { 2 } else { 1 } * if is_stereo { 2 } else { 1 };
    let needed = len.saturating_mul(bytes_per_frame);
    let end = (off + needed).min(bytes.len());
    if off >= end {
        return SampleBody {
            volume: inst.volume,
            c5_speed: inst.c5_speed.max(1),
            ..Default::default()
        };
    }
    let raw = &bytes[off..end];
    let actual_samples = raw.len() / bytes_per_frame;
    let mut pcm: Vec<i16> = Vec::with_capacity(actual_samples);
    let mut pcm_right: Option<Vec<i16>> = if is_stereo {
        Some(Vec::with_capacity(actual_samples))
    } else {
        None
    };

    // ST3 stereo sample layout: the full left block is followed by the
    // full right block (not interleaved per-frame). MemSeg gives the
    // start of the left block; the right block starts `length * bps`
    // bytes later — i.e. the split is at the *declared* per-channel length,
    // not the number of complete stereo frames that happen to be present.
    // Splitting at the declared length keeps the left/right boundary the
    // file intended even when the sample body is truncated: the left block
    // gets its full `length * bps` bytes (clamped to what exists) and the
    // right block reads from `length * bps` onward. `raw` is already bounded
    // to `length * bytes_per_frame`, so every `.min(raw.len())` below is the
    // real end.
    let bps = if is_16 { 2 } else { 1 };
    let left_block_bytes = len * bps;
    let left_end = left_block_bytes.min(raw.len());
    let left_raw = &raw[..left_end];
    let right_raw: &[u8] = if is_stereo {
        let start = left_block_bytes.min(raw.len());
        let end = (2 * left_block_bytes).min(raw.len());
        if start >= end {
            &[]
        } else {
            &raw[start..end]
        }
    } else {
        &[]
    };

    let decode_into = |dst: &mut Vec<i16>, src: &[u8]| {
        if is_16 {
            let mut i = 0;
            while i + 2 <= src.len() {
                let lo = src[i];
                let hi = src[i + 1];
                let s16_unsigned = u16::from_le_bytes([lo, hi]);
                // ST3 stores 16-bit as unsigned (bias 0x8000).
                let s = if signed_samples {
                    i16::from_le_bytes([lo, hi])
                } else {
                    (s16_unsigned as i32 - 0x8000) as i16
                };
                dst.push(s);
                i += 2;
            }
        } else {
            for &b in src {
                let s = if signed_samples {
                    (b as i8 as i32) * 256
                } else {
                    (b as i32 - 128) * 256
                };
                dst.push(s.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
            }
        }
    };

    decode_into(&mut pcm, left_raw);
    if let Some(ref mut r) = pcm_right {
        decode_into(r, right_raw);
        // Pad or truncate the right channel to match the left length so
        // a single sample_pos cursor can index both without bounds checks.
        if r.len() < pcm.len() {
            r.resize(pcm.len(), 0);
        } else if r.len() > pcm.len() {
            r.truncate(pcm.len());
        }
    }

    let loop_start = inst.loop_start.min(pcm.len() as u32);
    let loop_end = inst.loop_end.min(pcm.len() as u32);
    let looped = inst.is_looped() && loop_end > loop_start;

    SampleBody {
        pcm,
        pcm_right,
        loop_start,
        loop_end,
        looped,
        volume: inst.volume,
        c5_speed: inst.c5_speed.max(1),
        adlib: None,
    }
}

/// Decode a `DP30ADPCM` (pack type 1) sample body into signed-16-bit PCM.
///
/// Layout (`docs/audio/trackers/s3m/
/// s3m-position-jump-pattern-break-and-adpcm.md` §Part 2):
///
/// ```text
/// +0x00 .. +0x0F : int8 delta lookup table[16]
/// +0x10 ..       : ceil(n / 2) packed bytes, two 4-bit codes per byte
/// ```
///
/// Each nibble — **low nibble first, then high** — indexes the signed
/// delta table; the delta accumulates into a signed 8-bit value that
/// wraps on overflow, and the accumulator's value *after* each add is
/// the next output sample. If `n` is odd the final byte's high nibble is
/// padding and is not emitted. The decoded signed-8-bit points are scaled
/// by 256 to match the crate's i16 mixing convention. A `raw` shorter
/// than the 16-byte table yields no samples; a truncated packed block
/// yields only the samples its bytes cover.
fn decode_dp30adpcm(raw: &[u8], n: usize) -> Vec<i16> {
    let Some(table) = raw.get(..16) else {
        return Vec::new();
    };
    let packed = &raw[16..];
    let mut out: Vec<i16> = Vec::with_capacity(n.min(packed.len().saturating_mul(2)));
    let mut delta: i8 = 0;
    'bytes: for &b in packed {
        for code in [b & 0x0F, b >> 4] {
            if out.len() >= n {
                break 'bytes;
            }
            delta = delta.wrapping_add(table[code as usize] as i8);
            out.push(delta as i16 * 256);
        }
    }
    out
}

/// Decode every instrument's sample body.
pub fn extract_samples(header: &S3mHeader, bytes: &[u8]) -> Vec<SampleBody> {
    // FFI: 1 = signed, 2 = unsigned. Default to unsigned (the common ST3 case).
    let signed_samples = header.ffi == 1;
    header
        .instruments
        .iter()
        .map(|i| decode_instrument(i, bytes, signed_samples))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{
        Instrument, INST_TYPE_PCM, SAMPLE_FLAG_16BIT, SAMPLE_FLAG_LOOP, SAMPLE_FLAG_STEREO,
    };

    /// Build a PCM instrument whose sample body sits at byte offset
    /// `parapointer << 4` in the buffer, so the tests exercise the same
    /// `sample_byte_offset()` path the real parser feeds `decode_instrument`.
    fn pcm_instrument(parapointer: u32, length: u32, flags: u8) -> Instrument {
        Instrument {
            kind: INST_TYPE_PCM,
            sample_parapointer: parapointer,
            length,
            flags,
            volume: 40,
            c5_speed: 8363,
            ..Instrument::default()
        }
    }

    /// Place `sample` bytes at the instrument's byte offset in a fresh buffer.
    fn buffer_with_sample(parapointer: u32, sample: &[u8]) -> Vec<u8> {
        let off = (parapointer as usize) << 4;
        let mut buf = vec![0u8; off + sample.len()];
        buf[off..off + sample.len()].copy_from_slice(sample);
        buf
    }

    #[test]
    fn empty_or_zero_length_instrument_yields_no_pcm_but_keeps_metadata() {
        // A non-PCM (or zero-length) instrument must still surface its stored
        // default volume and a floored c5 speed so a note that references it
        // has a sane pitch/volume even though there's no audio to mix.
        let mut inst = pcm_instrument(0x10, 0, 0);
        inst.volume = 55;
        inst.c5_speed = 0; // must floor to 1, never divide-by-zero downstream.
        let body = decode_instrument(&inst, &[], false);
        assert!(body.pcm.is_empty());
        assert_eq!(body.volume, 55);
        assert_eq!(body.c5_speed, 1);
    }

    #[test]
    fn eight_bit_unsigned_uses_128_bias() {
        // ST3-standard 8-bit PCM is unsigned: 0x80 is the zero crossing,
        // 0x00 the negative peak, 0xFF just shy of the positive peak. Each
        // byte maps to `(b - 128) * 256`.
        let inst = pcm_instrument(0x10, 4, 0);
        let buf = buffer_with_sample(0x10, &[0x80, 0x00, 0xFF, 0xC0]);
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.pcm, vec![0, -32768, 32512, 16384]);
        assert!(body.pcm_right.is_none());
    }

    #[test]
    fn eight_bit_signed_selected_by_ffi_is_two_s_complement() {
        // FFI = 1 flags signed samples: the byte is a raw two's-complement
        // i8 scaled by 256 with no bias.
        let inst = pcm_instrument(0x10, 4, 0);
        let buf = buffer_with_sample(0x10, &[0x00, 0x7F, 0x80, 0xFF]);
        let body = decode_instrument(&inst, &buf, true);
        assert_eq!(body.pcm, vec![0, 32512, -32768, -256]);
    }

    #[test]
    fn sixteen_bit_unsigned_uses_0x8000_bias() {
        // 16-bit LE, unsigned convention: subtract 0x8000. 0x8000 → 0,
        // 0x0000 → -32768, 0xFFFF → +32767.
        let inst = pcm_instrument(0x10, 3, SAMPLE_FLAG_16BIT);
        let buf = buffer_with_sample(0x10, &[0x00, 0x80, 0x00, 0x00, 0xFF, 0xFF]);
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.pcm, vec![0, -32768, 32767]);
    }

    #[test]
    fn sixteen_bit_signed_selected_by_ffi_is_raw_le() {
        // With signed samples the 16-bit words are read as raw little-endian
        // i16 without the 0x8000 bias.
        let inst = pcm_instrument(0x10, 2, SAMPLE_FLAG_16BIT);
        let buf = buffer_with_sample(0x10, &[0x00, 0x00, 0xFF, 0x7F]);
        let body = decode_instrument(&inst, &buf, true);
        assert_eq!(body.pcm, vec![0, 32767]);
    }

    #[test]
    fn true_stereo_splits_left_then_right_blocks() {
        // ST3 stereo layout is block-sequential, not per-frame interleaved:
        // the whole left channel precedes the whole right channel. For an
        // 8-bit stereo sample of length 3 the first 3 bytes are left, the
        // next 3 are right.
        let inst = pcm_instrument(0x10, 3, SAMPLE_FLAG_STEREO);
        let buf = buffer_with_sample(0x10, &[0x80, 0xC0, 0x00, 0x80, 0x40, 0xFF]);
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.pcm, vec![0, 16384, -32768]);
        let right = body
            .pcm_right
            .expect("stereo sample must carry a right channel");
        assert_eq!(right, vec![0, -16384, 32512]);
    }

    #[test]
    fn stereo_with_truncated_right_block_pads_right_to_left_length() {
        // A sample whose right block is cut short must still yield a right
        // channel exactly as long as the left (zero-padded), so the mixer's
        // single sample-position cursor can index both without bounds checks.
        let inst = pcm_instrument(0x10, 4, SAMPLE_FLAG_STEREO);
        // 4 left bytes + only 2 right bytes present.
        let buf = buffer_with_sample(0x10, &[0x80, 0x81, 0x82, 0x83, 0x80, 0xFF]);
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.pcm.len(), 4);
        let right = body.pcm_right.expect("right channel present");
        assert_eq!(right.len(), 4);
        assert_eq!(&right[..2], &[0, 32512]);
        assert_eq!(&right[2..], &[0, 0]); // padded tail
    }

    #[test]
    fn loop_window_clamps_to_decoded_pcm_length() {
        // A loop end past the actual decoded length must be clamped to it,
        // and the `looped` flag only holds when the (clamped) window is
        // non-empty. Here loop_end 999 clamps to 4 and the loop survives.
        let mut inst = pcm_instrument(0x10, 4, SAMPLE_FLAG_LOOP);
        inst.loop_start = 1;
        inst.loop_end = 999;
        let buf = buffer_with_sample(0x10, &[0x80, 0x90, 0xA0, 0xB0]);
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.loop_start, 1);
        assert_eq!(body.loop_end, 4);
        assert!(body.is_looped());
        assert_eq!(body.loop_length(), 3);
    }

    #[test]
    fn loop_flag_with_empty_window_is_not_looped() {
        // loop_start >= loop_end after clamping means there's no window to
        // loop over, so `is_looped` must report false even with the flag set.
        let mut inst = pcm_instrument(0x10, 4, SAMPLE_FLAG_LOOP);
        inst.loop_start = 3;
        inst.loop_end = 2;
        let buf = buffer_with_sample(0x10, &[0x80, 0x90, 0xA0, 0xB0]);
        let body = decode_instrument(&inst, &buf, false);
        assert!(!body.is_looped());
    }

    #[test]
    fn declared_length_beyond_buffer_decodes_only_available_frames() {
        // A hostile length field that overruns the file must decode only the
        // bytes that actually exist rather than reading past the buffer.
        let inst = pcm_instrument(0x10, 1000, 0);
        let buf = buffer_with_sample(0x10, &[0x80, 0x81, 0x82]);
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.pcm.len(), 3);
    }

    // ---- DP30ADPCM (pack type 1) — layout per
    // `docs/audio/trackers/s3m/s3m-position-jump-pattern-break-and-adpcm.md`
    // §Part 2: 16-byte signed delta table, then two 4-bit codes per byte,
    // low nibble first, accumulated into a wrapping signed-8-bit value.

    /// A pack=1 PCM instrument at `parapointer << 4`.
    fn adpcm_instrument(parapointer: u32, length: u32, flags: u8) -> Instrument {
        let mut inst = pcm_instrument(parapointer, length, flags);
        inst.pack = crate::header::PACK_DP30ADPCM;
        inst
    }

    /// Build a packed DP30ADPCM body: the 16-entry signed delta table
    /// followed by the packed nibble bytes.
    fn adpcm_body(table: &[i8; 16], packed: &[u8]) -> Vec<u8> {
        let mut body: Vec<u8> = table.iter().map(|&d| d as u8).collect();
        body.extend_from_slice(packed);
        body
    }

    #[test]
    fn adpcm_worked_example_low_nibble_first_accumulation() {
        // The staged doc's worked micro-example: table[1] = +4,
        // table[15] = -1, packed bytes 0x11 0xF1 → samples 4, 8, 12, 11
        // (low nibble of each byte decodes before the high nibble; a
        // high-first reading would give 4, 8, 7, 11 instead).
        let mut table = [0i8; 16];
        table[1] = 4;
        table[15] = -1;
        let inst = adpcm_instrument(0x10, 4, 0);
        let buf = buffer_with_sample(0x10, &adpcm_body(&table, &[0x11, 0xF1]));
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.pcm, vec![4 * 256, 8 * 256, 12 * 256, 11 * 256]);
        assert!(body.pcm_right.is_none());
    }

    #[test]
    fn adpcm_odd_length_drops_the_padding_high_nibble() {
        // N = 3 over the same two packed bytes: the final byte's high
        // nibble is padding and must not be emitted.
        let mut table = [0i8; 16];
        table[1] = 4;
        table[15] = -1;
        let inst = adpcm_instrument(0x10, 3, 0);
        let buf = buffer_with_sample(0x10, &adpcm_body(&table, &[0x11, 0xF1]));
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.pcm, vec![4 * 256, 8 * 256, 12 * 256]);
    }

    #[test]
    fn adpcm_accumulator_wraps_as_signed_8_bit() {
        // The running delta is a plain wrapping int8: three +100 steps
        // give 100, then 200 → wraps to -56, then -56 + 100 = 44.
        let mut table = [0i8; 16];
        table[2] = 100;
        let inst = adpcm_instrument(0x10, 3, 0);
        let buf = buffer_with_sample(0x10, &adpcm_body(&table, &[0x22, 0x02]));
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.pcm, vec![100 * 256, -56 * 256, 44 * 256]);
    }

    #[test]
    fn adpcm_truncated_packed_block_decodes_only_present_nibbles() {
        // Length claims 8 samples (4 packed bytes) but only 2 packed
        // bytes exist: the decode is bounded to the 4 nibbles present.
        let mut table = [0i8; 16];
        table[1] = 1;
        let inst = adpcm_instrument(0x10, 8, 0);
        let buf = buffer_with_sample(0x10, &adpcm_body(&table, &[0x11, 0x11]));
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.pcm, vec![256, 512, 768, 1024]);
    }

    #[test]
    fn adpcm_body_shorter_than_the_delta_table_yields_no_samples() {
        // Fewer than 16 bytes cannot even hold the lookup table; the
        // body decodes to silence while keeping the metadata.
        let inst = adpcm_instrument(0x10, 4, 0);
        let buf = buffer_with_sample(0x10, &[1, 2, 3, 4, 5]);
        let body = decode_instrument(&inst, &buf, false);
        assert!(body.pcm.is_empty());
        assert_eq!(body.volume, 40);
        assert_eq!(body.c5_speed, 8363);
    }

    #[test]
    fn adpcm_output_is_signed_regardless_of_ffi() {
        // The packed stream is signed 8-bit by definition — the header's
        // FFI signed/unsigned convention applies to raw PCM only, so both
        // FFI readings must decode identically.
        let mut table = [0i8; 16];
        table[3] = -5;
        let inst = adpcm_instrument(0x10, 2, 0);
        let buf = buffer_with_sample(0x10, &adpcm_body(&table, &[0x33]));
        let unsigned_ffi = decode_instrument(&inst, &buf, false);
        let signed_ffi = decode_instrument(&inst, &buf, true);
        assert_eq!(unsigned_ffi.pcm, vec![-5 * 256, -10 * 256]);
        assert_eq!(unsigned_ffi.pcm, signed_ffi.pcm);
    }

    #[test]
    fn adpcm_ignores_hostile_16bit_and_stereo_flags() {
        // The packing is defined only for 8-bit mono data; a pack=1 body
        // carrying the 16-bit / stereo flag bits still decodes as the
        // documented layout — N mono samples, no right channel.
        let mut table = [0i8; 16];
        table[1] = 2;
        let inst = adpcm_instrument(0x10, 4, SAMPLE_FLAG_16BIT | SAMPLE_FLAG_STEREO);
        let buf = buffer_with_sample(0x10, &adpcm_body(&table, &[0x11, 0x11]));
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.pcm, vec![512, 1024, 1536, 2048]);
        assert!(body.pcm_right.is_none());
    }

    #[test]
    fn adpcm_loop_window_clamps_to_decoded_length() {
        // Loop points beyond the decoded sample count clamp exactly like
        // the raw-PCM path so the mixer's loop wrap stays in bounds.
        let mut table = [0i8; 16];
        table[1] = 1;
        let mut inst = adpcm_instrument(0x10, 4, SAMPLE_FLAG_LOOP);
        inst.loop_start = 1;
        inst.loop_end = 999;
        let buf = buffer_with_sample(0x10, &adpcm_body(&table, &[0x11, 0x11]));
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.loop_start, 1);
        assert_eq!(body.loop_end, 4);
        assert!(body.is_looped());
    }

    #[test]
    fn adpcm_offset_past_buffer_end_yields_empty_body() {
        // A parapointer landing at/after EOF has nothing to decode; the
        // metadata-only body comes back exactly like the raw-PCM path.
        let inst = adpcm_instrument(0x40, 8, 0);
        let buf = vec![0u8; 0x100];
        let body = decode_instrument(&inst, &buf, false);
        assert!(body.pcm.is_empty());
        assert_eq!(body.volume, 40);
    }

    #[test]
    fn unknown_pack_values_fall_back_to_the_unpacked_path() {
        // The spec names only pack 0 and 1; any other value is treated as
        // unpacked raw PCM (the pre-existing behaviour for garbage bytes)
        // rather than rejected, keeping hostile files playable-but-bounded.
        let mut inst = pcm_instrument(0x10, 2, 0);
        inst.pack = 0x7F;
        let buf = buffer_with_sample(0x10, &[0x80, 0xC0]);
        let body = decode_instrument(&inst, &buf, false);
        assert_eq!(body.pcm, vec![0, 16384]);
    }

    #[test]
    fn sample_offset_at_or_past_buffer_end_yields_empty_body() {
        // A parapointer that lands at/after EOF has no readable bytes; the
        // decoder returns an empty body while preserving volume / c5 speed.
        let inst = pcm_instrument(0x40, 8, 0); // offset 0x400, buffer shorter
        let buf = vec![0u8; 0x100];
        let body = decode_instrument(&inst, &buf, false);
        assert!(body.pcm.is_empty());
        assert_eq!(body.volume, 40);
        assert_eq!(body.c5_speed, 8363);
    }
}
