//! Scream Tracker 3 Module (S3M) header parser.
//!
//! Unlike MOD, S3M is little-endian and uses "parapointers" (paragraph
//! pointers) — 16-bit values that must be left-shifted by 4 to obtain a
//! byte offset. The header layout:
//!
//! ```text
//! 0x00  28 bytes  Song name (null-padded ASCII)
//! 0x1C   1 byte   0x1A (EOF / end-of-text marker)
//! 0x1D   1 byte   Type (0x10 = S3M)
//! 0x1E   2 bytes  Reserved
//! 0x20   2 bytes  OrdNum   — entries in order table (even count)
//! 0x22   2 bytes  InsNum   — number of instruments
//! 0x24   2 bytes  PatNum   — number of patterns
//! 0x26   2 bytes  Flags
//! 0x28   2 bytes  CwtV     — tracker version
//! 0x2A   2 bytes  FFI      — file format info (1 = signed samples, 2 = unsigned)
//! 0x2C   4 bytes  "SCRM"   — signature
//! 0x30   1 byte   GV       — global volume
//! 0x31   1 byte   IS       — initial speed (ticks per row)
//! 0x32   1 byte   IT       — initial tempo (BPM)
//! 0x33   1 byte   MV       — master volume (bit 7 = stereo)
//! 0x34   1 byte   UC       — ultra-click removal
//! 0x35   1 byte   DP       — default pan flag (0xFC = use pan values below)
//! 0x36   8 bytes  Reserved
//! 0x3E   2 bytes  Special  — parapointer to special data (unused)
//! 0x40  32 bytes  Channel settings — 0..=7 left, 8..=15 right, 16..=31 adlib,
//!                  bit 7 set (e.g. 0x80 | type) = muted, 0xFF = unused
//! 0x60  OrdNum    Order table — 0xFE = marker, 0xFF = end
//! ...   InsNum*2  Instrument parapointer table
//! ...   PatNum*2  Pattern parapointer table
//! ...   (optional) 32 bytes of default pan values (if DP == 0xFC)
//! ```
//!
//! Each instrument is 80 bytes starting at its parapointer, and each
//! pattern starts with a 2-byte length followed by packed rows (see
//! `pattern.rs`).

use oxideav_core::{Error, Result};

pub const S3M_SIGNATURE: &[u8; 4] = b"SCRM";
pub const PATTERN_ROWS: usize = 64;
pub const CHANNEL_COUNT: usize = 32;
pub const INSTRUMENT_HEADER_SIZE: usize = 80;

/// Tracker IDs encoded in the high nibble of the `Cwt/v` field.
///
/// Per the multimedia.cx behavioural reference
/// (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html` §"Tracker
/// version") and the FireLight / ST3 archive-team format references
/// (`docs/audio/trackers/s3m/ScreamTracker-v3.20-s3m.txt`:
/// `Cwt/v   = Created with tracker / version: &0xfff=version, >>12=tracker`),
/// the 16-bit `Cwt/v` word splits into a 4-bit tracker ID (top nibble) and
/// a 12-bit version number (low 12 bits). Known tracker IDs:
///
/// * `0x1xyy` — Scream Tracker x.yy (the original ST3 family;
///   ST3.00 = 0x1300, ST3.01 = 0x1301, ST3.03 = 0x1303, ST3.20 = 0x1320,
///   ST3.21 = 0x1321).
/// * `0x2xyy` — Imago Orpheus x.yy.
/// * `0x3xyy` — Impulse Tracker x.yy.
/// * `0x4xyy` — Schism Tracker (its own version numbering scheme).
/// * `0x5xyy` — OpenMPT.
///
/// Anything outside these documented prefixes lands in
/// [`Tracker::Other`] with the raw nibble preserved for diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tracker {
    /// `0x1xyy` — Scream Tracker family (the canonical writer).
    ScreamTracker,
    /// `0x2xyy` — Imago Orpheus.
    ImagoOrpheus,
    /// `0x3xyy` — Impulse Tracker.
    ImpulseTracker,
    /// `0x4xyy` — Schism Tracker (versioning scheme differs from the
    /// Scream Tracker family — only the tracker ID nibble is documented).
    SchismTracker,
    /// `0x5xyy` — OpenMPT.
    OpenMpt,
    /// Any tracker whose top nibble is not documented above. Carries
    /// the raw nibble (`0`, `6..=15`) so callers can still inspect it.
    Other(u8),
}

impl Tracker {
    /// Decode the tracker ID from the raw `Cwt/v` word's top nibble.
    ///
    /// Equivalent to `cwt_v >> 12`, mapped onto the documented IDs.
    pub fn from_cwt_v(cwt_v: u16) -> Tracker {
        match (cwt_v >> 12) as u8 {
            0x1 => Tracker::ScreamTracker,
            0x2 => Tracker::ImagoOrpheus,
            0x3 => Tracker::ImpulseTracker,
            0x4 => Tracker::SchismTracker,
            0x5 => Tracker::OpenMpt,
            other => Tracker::Other(other),
        }
    }
}

/// Decomposed `Cwt/v` field: a 4-bit tracker ID plus a 12-bit version
/// number, paired with the original raw word.
///
/// `Cwt/v` is the file header's "Created with tracker / version" word.
/// The split semantics (`>>12` = tracker, `&0xfff` = version) are
/// quoted verbatim from `ScreamTracker-v3.20-s3m.txt` and corroborated
/// by `multimedia-cx-scream-tracker-3.html` §"Tracker version".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreatedWithTracker {
    /// Raw 16-bit `Cwt/v` word as stored in the file.
    pub raw: u16,
    /// Typed tracker ID (top nibble).
    pub tracker: Tracker,
    /// Low 12 bits — the tracker's own version number. For the Scream
    /// Tracker family this is BCD-shaped (`0x300` = 3.00, `0x320` =
    /// 3.20); for Schism Tracker the spec notes the numbering scheme
    /// differs, so the field is exposed as-is and the caller decides
    /// how to interpret it.
    pub version: u16,
}

impl CreatedWithTracker {
    /// Decompose a raw `Cwt/v` word.
    pub fn from_raw(raw: u16) -> Self {
        Self {
            raw,
            tracker: Tracker::from_cwt_v(raw),
            version: raw & 0x0FFF,
        }
    }

    /// True iff this file was written by the original Scream Tracker 3.00
    /// release (`Cwt/v == 0x1300`).
    ///
    /// Per `multimedia-cx-scream-tracker-3.html` §Flags (bit 6): "ST3.00
    /// volume slides ... **automatically enabled if tracker version is
    /// == 0x1300**". The fast-slides arming inside the player consults
    /// exactly this predicate.
    pub fn is_st3_00(&self) -> bool {
        self.raw == 0x1300
    }
}

/// Sample type codes in the instrument header.
pub const INST_TYPE_EMPTY: u8 = 0;
pub const INST_TYPE_PCM: u8 = 1;
pub const INST_TYPE_ADLIB_MELODY: u8 = 2;
// 3..=7: AdLib drum types (ignored for now).

/// Sample flag bits (byte 0x1F of instrument header).
pub const SAMPLE_FLAG_LOOP: u8 = 0x01;
pub const SAMPLE_FLAG_STEREO: u8 = 0x02;
pub const SAMPLE_FLAG_16BIT: u8 = 0x04;

/// An S3M instrument / sample definition (80 bytes in the file).
#[derive(Clone, Debug, Default)]
pub struct Instrument {
    /// 1 = PCM sample, 0 = empty, 2..=7 = AdLib (unsupported).
    pub kind: u8,
    /// Original DOS filename (12 bytes).
    pub dos_name: String,
    /// Parapointer to sample data (shift-left-by-4 to get byte offset).
    /// High byte at offset 0x0D, low word at 0x0E..0x10 (LE).
    pub sample_parapointer: u32,
    /// Length in samples.
    pub length: u32,
    /// Loop start in samples.
    pub loop_start: u32,
    /// Loop end in samples.
    pub loop_end: u32,
    /// Default volume 0..=64.
    pub volume: u8,
    /// Packing scheme (should be 0 for uncompressed PCM).
    pub pack: u8,
    /// Flags: bit0 loop, bit1 stereo, bit2 16-bit.
    pub flags: u8,
    /// C5 (middle-C) playback rate in Hz.
    pub c5_speed: u32,
    /// Instrument display name (28 bytes).
    pub name: String,
    /// Last 4 bytes should be "SCRS" for PCM, "SCRI" for AdLib.
    pub tag: [u8; 4],
}

impl Instrument {
    pub fn is_pcm(&self) -> bool {
        self.kind == INST_TYPE_PCM
    }

    pub fn is_looped(&self) -> bool {
        self.flags & SAMPLE_FLAG_LOOP != 0 && self.loop_end > self.loop_start
    }

    pub fn is_16bit(&self) -> bool {
        self.flags & SAMPLE_FLAG_16BIT != 0
    }

    pub fn is_stereo(&self) -> bool {
        self.flags & SAMPLE_FLAG_STEREO != 0
    }

    /// Byte offset where sample bytes begin.
    pub fn sample_byte_offset(&self) -> usize {
        (self.sample_parapointer as usize) << 4
    }
}

/// Parsed S3M top-level header + order table + parapointer tables +
/// channel settings + default pan.
#[derive(Clone, Debug)]
pub struct S3mHeader {
    pub song_name: String,
    pub ord_num: u16,
    pub ins_num: u16,
    pub pat_num: u16,
    pub flags: u16,
    pub tracker_version: u16,
    /// 1 = signed samples, 2 = unsigned samples.
    pub ffi: u16,
    pub global_volume: u8,
    pub initial_speed: u8,
    pub initial_tempo: u8,
    pub master_volume: u8,
    /// True if bit 7 of master_volume is set.
    pub stereo: bool,
    /// Default-pan flag — 0xFC means the 32 pan bytes at end of header are valid.
    pub default_pan_flag: u8,
    /// Channel settings, 32 entries. Per the ST3 archive-team format reference
    /// (`docs/audio/trackers/s3m/ScreamTracker-v3.20-s3m.txt`):
    ///   `0xFF` = slot unused (not assigned to any output);
    ///   bit 7 set (`0x80 | type`, e.g. `0x83` = "muted left PCM channel 4")
    ///     = channel is *muted* but its pattern data is still read;
    ///   otherwise the low bits are the channel type
    ///     (0..=7 left PCM, 8..=15 right PCM, 16..=31 AdLib).
    pub channels: [u8; CHANNEL_COUNT],
    /// Pan values for 32 channels (0..=15); populated from the default
    /// pan block or synthesized from `channels` (left/right bank split).
    pub pans: [u8; CHANNEL_COUNT],
    /// Per-channel muted flag — true when the channel settings byte has
    /// bit 7 set (`0x80 | type`). Muted channels still parse pattern data
    /// (so jumps, loops, pattern-delay see consistent state) but produce
    /// no audio in the mixer. `0xFF` (unused) is treated as muted, since
    /// no output channel is mapped to it.
    pub muted: [bool; CHANNEL_COUNT],
    /// Raw order list (0xFE marker rows and 0xFF end markers preserved).
    pub order: Vec<u8>,
    /// Per-instrument definitions (parsed from parapointers).
    pub instruments: Vec<Instrument>,
    /// Per-pattern byte offsets in the file (shifted parapointers).
    pub pattern_offsets: Vec<u32>,
    /// Number of enabled (non-0xFF) channels used by the module.
    pub enabled_channels: u8,
}

impl S3mHeader {
    /// Decomposed view of the `Cwt/v` ("Created with tracker / version")
    /// field. See [`CreatedWithTracker`] for the bit layout and the
    /// documented tracker IDs.
    pub fn created_with_tracker(&self) -> CreatedWithTracker {
        CreatedWithTracker::from_raw(self.tracker_version)
    }
}

fn read_u16_le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn read_u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn read_padded_ascii(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end])
        .trim_end()
        .to_string()
}

/// Validate the S3M signature and parse the full header.
pub fn parse_header(bytes: &[u8]) -> Result<S3mHeader> {
    if bytes.len() < 0x60 {
        return Err(Error::invalid("S3M: file shorter than minimum header"));
    }
    // 'SCRM' signature at 0x2C.
    if &bytes[0x2C..0x30] != S3M_SIGNATURE {
        return Err(Error::invalid(
            "S3M: missing 'SCRM' signature at offset 0x2C",
        ));
    }
    // Type byte at 0x1D must be 0x10.
    if bytes[0x1D] != 0x10 {
        return Err(Error::invalid(format!(
            "S3M: expected type byte 0x10, got 0x{:02X}",
            bytes[0x1D]
        )));
    }

    let song_name = read_padded_ascii(&bytes[0x00..0x1C]);
    let ord_num = read_u16_le(bytes, 0x20);
    let ins_num = read_u16_le(bytes, 0x22);
    let pat_num = read_u16_le(bytes, 0x24);
    let flags = read_u16_le(bytes, 0x26);
    let tracker_version = read_u16_le(bytes, 0x28);
    let ffi = read_u16_le(bytes, 0x2A);
    let global_volume = bytes[0x30];
    let initial_speed = bytes[0x31];
    let initial_tempo = bytes[0x32];
    let master_volume_raw = bytes[0x33];
    let default_pan_flag = bytes[0x35];

    let master_volume = master_volume_raw & 0x7F;
    let stereo = (master_volume_raw & 0x80) != 0;

    let mut channels = [0u8; CHANNEL_COUNT];
    channels.copy_from_slice(&bytes[0x40..0x40 + CHANNEL_COUNT]);

    // Order table starts at 0x60.
    let ord_start = 0x60usize;
    let ord_end = ord_start + ord_num as usize;
    if bytes.len() < ord_end {
        return Err(Error::invalid("S3M: file shorter than order table"));
    }
    let order: Vec<u8> = bytes[ord_start..ord_end].to_vec();

    // Instrument parapointer table.
    let ins_table_start = ord_end;
    let ins_table_end = ins_table_start + ins_num as usize * 2;
    if bytes.len() < ins_table_end {
        return Err(Error::invalid(
            "S3M: truncated instrument parapointer table",
        ));
    }
    let mut instruments = Vec::with_capacity(ins_num as usize);
    for i in 0..ins_num as usize {
        let pp_off = ins_table_start + i * 2;
        let parapointer = read_u16_le(bytes, pp_off) as u32;
        let inst_byte_off = (parapointer as usize) << 4;
        instruments.push(parse_instrument(bytes, inst_byte_off)?);
    }

    // Pattern parapointer table.
    let pat_table_start = ins_table_end;
    let pat_table_end = pat_table_start + pat_num as usize * 2;
    if bytes.len() < pat_table_end {
        return Err(Error::invalid("S3M: truncated pattern parapointer table"));
    }
    let mut pattern_offsets = Vec::with_capacity(pat_num as usize);
    for i in 0..pat_num as usize {
        let pp_off = pat_table_start + i * 2;
        let parapointer = read_u16_le(bytes, pp_off) as u32;
        pattern_offsets.push(parapointer << 4);
    }

    // Default pan resolution per the ST3 archive-team format reference
    // (`docs/audio/trackers/s3m/ScreamTracker-v3.20-s3m.txt` §"Channel pan
    // settings") and FireLight tutorial §2.8 / §2.8.1
    // (`docs/audio/trackers/s3m/FireLight-S3M-Player-Tutorial.txt`).
    //
    // The spec rule for each pan byte's bit 5: "1=default pan position
    // specified, 0=use defaults: for mono 7, for stereo 3 or C." Bit 5
    // is also the per-channel fallback selector inside the optional 32-byte
    // pan block (present when `d.p == 0xFC`). Channels without bit 5 set
    // — or modules with no pan block at all — fall back to spec defaults
    // keyed by the master-volume stereo flag:
    //   * Stereo: left PCM slots (channel type 0..=7) → 3, right PCM
    //     slots (8..=15) → C. Unused slots (0xFF) get the centre value
    //     since they emit no audio.
    //   * Mono: every channel → 7 ("middle"). FireLight §2.8.1 also says
    //     that in mono mode "all the panning values should be set to the
    //     MIDDLE, regardless of any other panning information given before"
    //     — i.e. the mono override beats an explicitly-specified pan byte.
    //     We implement that as a final sweep over the resolved pan array.
    fn stereo_default_pan(channel_settings: u8) -> u8 {
        // Mute / AdLib slots have no PCM output mapping — assign the
        // centre value so the field is well-defined without affecting
        // anything audible.
        if channel_settings == 0xFF {
            return 0x08;
        }
        let slot = channel_settings & 0x0F;
        if slot < 8 {
            0x03
        } else {
            0x0C
        }
    }

    let mut pans = [0u8; CHANNEL_COUNT];
    if default_pan_flag == 0xFC && bytes.len() >= pat_table_end + CHANNEL_COUNT {
        for (i, p) in pans.iter_mut().enumerate() {
            let raw = bytes[pat_table_end + i];
            *p = if raw & 0x20 != 0 {
                // Bit 5 set: low nibble is the explicit pan position.
                raw & 0x0F
            } else if stereo {
                stereo_default_pan(channels[i])
            } else {
                // Mono fallback per spec.
                0x07
            };
        }
    } else {
        // No pan block: use the spec default keyed by stereo flag.
        for (i, c) in channels.iter().enumerate() {
            pans[i] = if stereo { stereo_default_pan(*c) } else { 0x07 };
        }
    }

    // FireLight §2.8.1 mono override — in mono mode every channel pans
    // to the centre regardless of the bytes we just resolved.
    if !stereo {
        for p in pans.iter_mut() {
            *p = 0x07;
        }
    }

    let enabled_channels = channels.iter().filter(|&&c| c != 0xFF && c < 16).count() as u8;

    // Per-channel mute flag: bit 7 (`+128`) marks a channel as disabled in
    // the file-format spec. We treat 0xFF (unused) as muted too — there's no
    // mapped output for it. The pattern parser still reads cells for muted
    // channels so jumps / loops / pattern-delay see consistent state; the
    // mixer silences them in `render`.
    let mut muted = [false; CHANNEL_COUNT];
    for (i, c) in channels.iter().enumerate() {
        // 0xFF = unused. Otherwise bit 7 set (mask 0x80) = explicitly muted.
        // AdLib slots (16..=31) without bit 7 are valid AdLib channels in
        // ST3 — we don't render those (no OPL synth), so report them as
        // muted to keep the audio path silent without confusing the parser.
        muted[i] = *c == 0xFF || (*c & 0x80) != 0 || (*c & 0x7F) >= 16;
    }

    Ok(S3mHeader {
        song_name,
        ord_num,
        ins_num,
        pat_num,
        flags,
        tracker_version,
        ffi,
        global_volume,
        initial_speed,
        initial_tempo,
        master_volume,
        stereo,
        default_pan_flag,
        channels,
        pans,
        muted,
        order,
        instruments,
        pattern_offsets,
        enabled_channels,
    })
}

/// Parse a single 80-byte instrument header starting at `off`.
///
/// Layout:
/// ```text
/// 0x00   1 byte   Type (1 = PCM)
/// 0x01  12 bytes  DOS filename
/// 0x0D   1 byte   MemSeg hi
/// 0x0E   2 bytes  MemSeg lo (LE)  — combined: (hi << 16) | lo = parapointer
/// 0x10   4 bytes  Length (LE)
/// 0x14   4 bytes  Loop start (LE)
/// 0x18   4 bytes  Loop end (LE)
/// 0x1C   1 byte   Default volume
/// 0x1D   1 byte   Reserved
/// 0x1E   1 byte   Pack (0 = unpacked)
/// 0x1F   1 byte   Flags
/// 0x20   4 bytes  C5 speed (LE)
/// 0x24  12 bytes  Reserved
/// 0x30  28 bytes  Sample name
/// 0x4C   4 bytes  "SCRS" tag (for PCM)
/// ```
pub fn parse_instrument(bytes: &[u8], off: usize) -> Result<Instrument> {
    if off == 0 {
        // Parapointer of 0 means "empty slot".
        return Ok(Instrument::default());
    }
    if bytes.len() < off + INSTRUMENT_HEADER_SIZE {
        return Err(Error::invalid("S3M: truncated instrument header"));
    }
    let h = &bytes[off..off + INSTRUMENT_HEADER_SIZE];
    let kind = h[0];
    let dos_name = read_padded_ascii(&h[0x01..0x0D]);
    let mem_hi = h[0x0D] as u32;
    let mem_lo = read_u16_le(h, 0x0E) as u32;
    let sample_parapointer = (mem_hi << 16) | mem_lo;
    let length = read_u32_le(h, 0x10);
    let loop_start = read_u32_le(h, 0x14);
    let loop_end = read_u32_le(h, 0x18);
    let volume = h[0x1C].min(64);
    let pack = h[0x1E];
    let flags = h[0x1F];
    let c5_speed = read_u32_le(h, 0x20);
    let name = read_padded_ascii(&h[0x30..0x4C]);
    let mut tag = [0u8; 4];
    tag.copy_from_slice(&h[0x4C..0x50]);
    Ok(Instrument {
        kind,
        dos_name,
        sample_parapointer,
        length,
        loop_start,
        loop_end,
        volume,
        pack,
        flags,
        c5_speed,
        name,
        tag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wrong_signature() {
        let mut bytes = vec![0u8; 0x100];
        bytes[0x1D] = 0x10;
        // Leave SCRM missing.
        assert!(parse_header(&bytes).is_err());
    }

    #[test]
    fn rejects_wrong_type_byte() {
        let mut bytes = vec![0u8; 0x100];
        bytes[0x2C..0x30].copy_from_slice(S3M_SIGNATURE);
        bytes[0x1D] = 0x00;
        assert!(parse_header(&bytes).is_err());
    }

    /// Build a minimal header byte string with a given 32-byte channel
    /// settings array. Used to exercise the muted-flag derivation.
    ///
    /// The header references zero instruments and zero patterns so we only
    /// need the 0x60-byte fixed header plus a 2-byte order table with an
    /// 0xFF end marker.
    fn build_min_header(channel_settings: [u8; CHANNEL_COUNT]) -> Vec<u8> {
        let mut b = vec![0u8; 0x60];
        b[0x1D] = 0x10;
        // Counts: 1 order, 0 ins, 0 pat (empty module).
        b[0x20..0x22].copy_from_slice(&1u16.to_le_bytes());
        b[0x22..0x24].copy_from_slice(&0u16.to_le_bytes());
        b[0x24..0x26].copy_from_slice(&0u16.to_le_bytes());
        b[0x2C..0x30].copy_from_slice(S3M_SIGNATURE);
        b[0x30] = 64;
        b[0x31] = 6;
        b[0x32] = 125;
        b[0x33] = 0x30;
        b[0x40..0x40 + CHANNEL_COUNT].copy_from_slice(&channel_settings);
        // Order: one 0xFF end marker.
        b.push(0xFF);
        b
    }

    #[test]
    fn muted_flag_set_for_unused_slots() {
        // Channel 0 active (left-PCM-1), all other slots 0xFF (unused).
        let mut settings = [0xFFu8; CHANNEL_COUNT];
        settings[0] = 0x00;
        let bytes = build_min_header(settings);
        let h = parse_header(&bytes).unwrap();
        assert!(!h.muted[0], "channel 0 (type 0x00) must not be muted");
        for slot in 1..CHANNEL_COUNT {
            assert!(h.muted[slot], "unused slot {slot} must report muted");
        }
    }

    #[test]
    fn muted_flag_set_for_plus128_disabled_channels() {
        // Per `docs/audio/trackers/s3m/ScreamTracker-v3.20-s3m.txt`
        // ("Channel settings ... 255=unused,+128=disabled"), bit 7 set
        // marks a channel as muted while the low bits still describe the
        // mapped output type. Channels 0 and 1 = active; 2 = muted-with-
        // type-2 (`0x82`); 3 = muted-with-type-3 (`0x83`).
        let mut settings = [0xFFu8; CHANNEL_COUNT];
        settings[0] = 0x00;
        settings[1] = 0x01;
        settings[2] = 0x82;
        settings[3] = 0x83;
        let bytes = build_min_header(settings);
        let h = parse_header(&bytes).unwrap();
        assert!(!h.muted[0]);
        assert!(!h.muted[1]);
        assert!(h.muted[2], "+128 disabled channel must be muted");
        assert!(h.muted[3]);
        // enabled_channels counts unmuted PCM channels only (used as the
        // mixer normalisation divisor) — must report 2, not 4.
        assert_eq!(h.enabled_channels, 2);
    }

    #[test]
    fn tracker_from_cwt_v_documented_prefixes() {
        // Per the multimedia.cx behavioural reference §"Tracker version"
        // (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`)
        // and `docs/audio/trackers/s3m/ScreamTracker-v3.20-s3m.txt`
        // (`Cwt/v ... >>12=tracker`). The four documented Scream Tracker
        // versions all share the `0x1xyy` prefix.
        assert_eq!(Tracker::from_cwt_v(0x1300), Tracker::ScreamTracker);
        assert_eq!(Tracker::from_cwt_v(0x1301), Tracker::ScreamTracker);
        assert_eq!(Tracker::from_cwt_v(0x1303), Tracker::ScreamTracker);
        assert_eq!(Tracker::from_cwt_v(0x1320), Tracker::ScreamTracker);
        assert_eq!(Tracker::from_cwt_v(0x1321), Tracker::ScreamTracker);
        // Other documented writers.
        assert_eq!(Tracker::from_cwt_v(0x2000), Tracker::ImagoOrpheus);
        assert_eq!(Tracker::from_cwt_v(0x3215), Tracker::ImpulseTracker);
        assert_eq!(Tracker::from_cwt_v(0x4050), Tracker::SchismTracker);
        assert_eq!(Tracker::from_cwt_v(0x5104), Tracker::OpenMpt);
    }

    #[test]
    fn tracker_from_cwt_v_undocumented_nibbles_preserve_id() {
        // Any prefix outside the documented set is exposed via
        // `Tracker::Other(nibble)` so a forensic inspector can still
        // see the raw value rather than misclassify as a known writer.
        assert_eq!(Tracker::from_cwt_v(0x0123), Tracker::Other(0));
        assert_eq!(Tracker::from_cwt_v(0x6abc), Tracker::Other(6));
        assert_eq!(Tracker::from_cwt_v(0xF000), Tracker::Other(0xF));
    }

    #[test]
    fn created_with_tracker_splits_top_nibble_and_low_12_bits() {
        // ST3.20: 0x1320 = (tracker=1, version=0x320).
        let st320 = CreatedWithTracker::from_raw(0x1320);
        assert_eq!(st320.raw, 0x1320);
        assert_eq!(st320.tracker, Tracker::ScreamTracker);
        assert_eq!(st320.version, 0x320);
        assert!(
            !st320.is_st3_00(),
            "0x1320 must NOT trigger the 0x1300 fast-slides auto-arm"
        );

        // ST3.00 sentinel: the multimedia.cx wiki specifically calls out
        // `Cwt/v == 0x1300` as the value that auto-arms fast slides
        // regardless of header flag bit 6.
        let st300 = CreatedWithTracker::from_raw(0x1300);
        assert_eq!(st300.tracker, Tracker::ScreamTracker);
        assert_eq!(st300.version, 0x300);
        assert!(st300.is_st3_00());

        // OpenMPT-shaped writer: high nibble routes to Tracker::OpenMpt,
        // the low 12 bits remain available verbatim.
        let mpt = CreatedWithTracker::from_raw(0x51AB);
        assert_eq!(mpt.tracker, Tracker::OpenMpt);
        assert_eq!(mpt.version, 0x1AB);
        assert!(!mpt.is_st3_00());
    }

    #[test]
    fn header_created_with_tracker_round_trips_raw_word() {
        // The accessor on `S3mHeader` must decompose whatever raw word the
        // parser stored — so it stays in lock-step with `tracker_version`.
        let mut settings = [0xFFu8; CHANNEL_COUNT];
        settings[0] = 0x00;
        let mut bytes = build_min_header(settings);
        // Patch the on-disk Cwt/v to ST3.01.
        bytes[0x28..0x2A].copy_from_slice(&0x1301u16.to_le_bytes());
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.tracker_version, 0x1301);
        let cwt = h.created_with_tracker();
        assert_eq!(cwt.raw, 0x1301);
        assert_eq!(cwt.tracker, Tracker::ScreamTracker);
        assert_eq!(cwt.version, 0x301);
        assert!(!cwt.is_st3_00(), "0x1301 is past the ST3.00 sentinel");
    }

    #[test]
    fn muted_flag_set_for_adlib_slots() {
        // AdLib melody (0x10..=0x18) and drum (0x19..=0x1D) slots are
        // valid file-format entries but ST3 doesn't synthesise AdLib in
        // the PCM mixer path. Mark them muted so the output stays silent
        // instead of running an uninitialised PCM voice.
        let mut settings = [0xFFu8; CHANNEL_COUNT];
        settings[0] = 0x10; // melody 1
        settings[1] = 0x18; // melody 9
        settings[2] = 0x1D; // drum 5
        let bytes = build_min_header(settings);
        let h = parse_header(&bytes).unwrap();
        assert!(h.muted[0]);
        assert!(h.muted[1]);
        assert!(h.muted[2]);
        // No PCM channels enabled → enabled_channels = 0.
        assert_eq!(h.enabled_channels, 0);
    }
}
