//! Hostile-input hardening harness.
//!
//! S3M files are attacker-controlled byte blobs: parapointers, order
//! tables, instrument/pattern counts and sample lengths all come straight
//! out of the file with no cross-checks the format itself guarantees. The
//! decode pipeline must therefore treat *every* byte sequence as adversarial
//! — a malformed, truncated, or randomly-corrupted module has to resolve to
//! a typed `Error` (or a bounded, silent render) and must **never** panic,
//! read out of bounds, or spin forever.
//!
//! These tests drive the whole public pipeline
//! (`parse_header` → `extract_samples` → `unpack_all` → `PlayerState::render`)
//! against three adversarial corpora:
//!
//!   * every truncation prefix of a known-good module,
//!   * PRNG-seeded random byte buffers (half stamped with the `SCRM`
//!     signature so parsing proceeds past the magic check),
//!   * single- and multi-byte mutations of the known-good module.
//!
//! A panic anywhere in the pipeline fails the test with a backtrace at the
//! offending frame; a hang fails via the harness timeout. All render loops
//! below are frame-bounded so a module that legitimately loops forever (a
//! self-jumping `Bxx`) still returns control.

use oxideav_core::{CodecId, CodecParameters, CodecRegistry, Frame, Packet, TimeBase};
use oxideav_s3m::header::parse_header;
use oxideav_s3m::pattern::unpack_all;
use oxideav_s3m::player::PlayerState;
use oxideav_s3m::samples::extract_samples;

const OUT_RATE: u32 = 44_100;

/// Splitmix64-style deterministic PRNG so the corpora are reproducible
/// across runs and platforms (no dependency on `rand`).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next_u32(&mut self) -> u32 {
        // LCG step (Knuth MMIX constants) then take the high bits, which
        // have the longest period.
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }
    fn below(&mut self, bound: u32) -> u32 {
        if bound == 0 {
            0
        } else {
            self.next_u32() % bound
        }
    }
}

fn put(buf: &mut Vec<u8>, off: usize, data: &[u8]) {
    if buf.len() < off + data.len() {
        buf.resize(off + data.len(), 0);
    }
    buf[off..off + data.len()].copy_from_slice(data);
}

/// Build a minimal but genuinely valid single-channel S3M module: one PCM
/// instrument (16 sample bytes), one pattern with a single note-on, and an
/// order table that terminates cleanly. This is the seed the truncation and
/// mutation corpora derive from.
fn build_valid_module() -> Vec<u8> {
    let mut b = vec![0u8; 0x60];
    put(&mut b, 0x00, b"HOSTILE-SEED");
    b[0x1C] = 0x1A; // conventional DOS EOF byte (not validated)
    b[0x1D] = 0x10; // required type byte
    put(&mut b, 0x20, &2u16.to_le_bytes()); // ord_num
    put(&mut b, 0x22, &1u16.to_le_bytes()); // ins_num
    put(&mut b, 0x24, &1u16.to_le_bytes()); // pat_num
    put(&mut b, 0x26, &0u16.to_le_bytes()); // flags
    put(&mut b, 0x28, &0x1320u16.to_le_bytes()); // cwt/v: ST3.20
    put(&mut b, 0x2A, &2u16.to_le_bytes()); // ffi: unsigned samples
    put(&mut b, 0x2C, b"SCRM"); // signature
    b[0x30] = 64; // global volume
    b[0x31] = 6; // initial speed
    b[0x32] = 125; // initial tempo
    b[0x33] = 0x80 | 48; // master volume + stereo bit
    b[0x35] = 0x00; // default-pan flag (no pan block)
    for i in 0..32 {
        b[0x40 + i] = 0xFF; // all channels unused …
    }
    b[0x40] = 0x00; // … except channel 0 = left PCM

    put(&mut b, 0x60, &[0x00, 0xFF]); // order table: pattern 0, then end
    put(&mut b, 0x62, &0x0008u16.to_le_bytes()); // instrument parapointer → 0x80
    put(&mut b, 0x64, &0x0010u16.to_le_bytes()); // pattern parapointer → 0x100

    // Instrument header (80 bytes) at 0x80.
    let ins = 0x80;
    b.resize(ins + 80, 0);
    b[ins] = 1; // PCM
    b[ins + 0x0D] = 0x00; // sample parapointer hi
    put(&mut b, ins + 0x0E, &0x000Eu16.to_le_bytes()); // sample parapointer lo → 0xE0
    put(&mut b, ins + 0x10, &16u32.to_le_bytes()); // length
    put(&mut b, ins + 0x14, &0u32.to_le_bytes()); // loop start
    put(&mut b, ins + 0x18, &0u32.to_le_bytes()); // loop end
    b[ins + 0x1C] = 64; // volume
    b[ins + 0x1E] = 0; // pack
    b[ins + 0x1F] = 0; // flags: no loop, mono, 8-bit
    put(&mut b, ins + 0x20, &8363u32.to_le_bytes()); // c5 speed
    put(&mut b, ins + 0x4C, b"SCRS"); // tag

    // Sample body (16 bytes) at 0xE0.
    let smp = 0xE0;
    b.resize(smp + 16, 0);
    for i in 0..16 {
        b[smp + i] = (i as u8).wrapping_mul(8);
    }

    // Pattern at 0x100: note C-5/instrument-1 on channel 0, then end-of-row.
    let pat = 0x100;
    let body = [0x20u8, 0x40, 0x01, 0x00];
    let length = (2 + body.len()) as u16;
    put(&mut b, pat, &length.to_le_bytes());
    put(&mut b, pat + 2, &body);

    b
}

/// One authored pattern cell: `(channel, note, instrument, volume, command,
/// info)`. `0xFF` note / vol means "no change"; a `0` command means "no
/// effect".
type PatCell = (u8, u8, u8, u8, u8, u8);

/// Pack a list of pattern rows into the S3M packed-pattern byte stream
/// (2-byte length prefix + records + per-row `0x00` terminator). Every
/// record is emitted with all three optional field groups present so the
/// unpacker's flag-driven length arithmetic is exercised.
fn pack_pattern(rows: &[&[PatCell]]) -> Vec<u8> {
    let mut body = Vec::new();
    for row in rows {
        for &(channel, note, instrument, volume, command, info) in row.iter() {
            let flags = (channel & 0x1F) | 0x20 | 0x40 | 0x80;
            body.push(flags);
            body.push(note);
            body.push(instrument);
            body.push(volume);
            body.push(command);
            body.push(info);
        }
        body.push(0x00); // end-of-row
    }
    let length = (2 + body.len()) as u16;
    let mut out = length.to_le_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

/// Build a feature-rich but valid module: four channels (two left / two
/// right PCM), one looped 8-bit mono instrument, one true-stereo 16-bit
/// instrument, one `DP30ADPCM` delta-packed instrument (pack byte = 1),
/// an explicit 32-byte pan block, Amiga-limits + fast-slides flags, an
/// order table carrying a `0xFE` marker, and two patterns whose rows
/// spread vibrato / porta / arpeggio / retrigger / tremolo / tremor /
/// combined-slide / sample-offset / extended / global-volume / tempo /
/// same-row position-jump + pattern-break effects across the channels.
/// Fuzzing mutations of this exercises the stereo sample decoder, the
/// ADPCM depacker, the pan-block parser, the Amiga clamp, and every
/// per-tick effect kernel — far deeper than the minimal seed reaches.
fn build_rich_module() -> Vec<u8> {
    let mut b = vec![0u8; 0x60];
    put(&mut b, 0x00, b"RICH-SEED");
    b[0x1C] = 0x1A;
    b[0x1D] = 0x10;
    put(&mut b, 0x20, &4u16.to_le_bytes()); // ord_num
    put(&mut b, 0x22, &3u16.to_le_bytes()); // ins_num
    put(&mut b, 0x24, &2u16.to_le_bytes()); // pat_num
    put(&mut b, 0x26, &((1u16 << 4) | (1u16 << 6)).to_le_bytes()); // Amiga + fast slides
    put(&mut b, 0x28, &0x1320u16.to_le_bytes());
    put(&mut b, 0x2A, &2u16.to_le_bytes()); // unsigned samples
    put(&mut b, 0x2C, b"SCRM");
    b[0x30] = 48; // global volume
    b[0x31] = 6; // speed
    b[0x32] = 125; // tempo
    b[0x33] = 0x80 | 48; // master + stereo
    b[0x35] = 0xFC; // pan block present
    for i in 0..32 {
        b[0x40 + i] = 0xFF;
    }
    b[0x40] = 0x00; // left PCM
    b[0x41] = 0x01; // left PCM
    b[0x42] = 0x08; // right PCM
    b[0x43] = 0x09; // right PCM

    put(&mut b, 0x60, &[0x00, 0xFE, 0x01, 0xFF]); // order: pat0, marker, pat1, end
    put(&mut b, 0x64, &0x0010u16.to_le_bytes()); // inst1 → 0x100
    put(&mut b, 0x66, &0x0018u16.to_le_bytes()); // inst2 → 0x180
    put(&mut b, 0x68, &0x001Du16.to_le_bytes()); // inst3 → 0x1D0
    put(&mut b, 0x6A, &0x0040u16.to_le_bytes()); // pat0 → 0x400
    put(&mut b, 0x6C, &0x0050u16.to_le_bytes()); // pat1 → 0x500
                                                 // Pan block at 0x6E: bit 5 set + explicit low nibble on every channel.
    b.resize(0x6E + 32, 0);
    for i in 0..32 {
        b[0x6E + i] = 0x20 | ((i as u8 * 3) & 0x0F);
    }

    // Instrument 1 — looped 8-bit mono, length 32, loop [8, 24).
    let i1 = 0x100;
    b.resize(i1 + 80, 0);
    b[i1] = 1;
    put(&mut b, i1 + 0x0E, &0x0020u16.to_le_bytes()); // sample → 0x200
    put(&mut b, i1 + 0x10, &32u32.to_le_bytes());
    put(&mut b, i1 + 0x14, &8u32.to_le_bytes());
    put(&mut b, i1 + 0x18, &24u32.to_le_bytes());
    b[i1 + 0x1C] = 64;
    b[i1 + 0x1F] = 0x01; // loop flag
    put(&mut b, i1 + 0x20, &8363u32.to_le_bytes());
    put(&mut b, i1 + 0x4C, b"SCRS");

    // Instrument 2 — true-stereo 16-bit, length 12.
    let i2 = 0x180;
    b.resize(i2 + 80, 0);
    b[i2] = 1;
    put(&mut b, i2 + 0x0E, &0x0030u16.to_le_bytes()); // sample → 0x300
    put(&mut b, i2 + 0x10, &12u32.to_le_bytes());
    b[i2 + 0x1C] = 48;
    b[i2 + 0x1F] = 0x02 | 0x04; // stereo | 16-bit
    put(&mut b, i2 + 0x20, &8363u32.to_le_bytes());
    put(&mut b, i2 + 0x4C, b"SCRS");

    // Instrument 3 — DP30ADPCM delta-packed (pack byte 1), 16 samples.
    let i3 = 0x1D0;
    b.resize(i3 + 80, 0);
    b[i3] = 1;
    put(&mut b, i3 + 0x0E, &0x0038u16.to_le_bytes()); // sample → 0x380
    put(&mut b, i3 + 0x10, &16u32.to_le_bytes());
    b[i3 + 0x1C] = 40;
    b[i3 + 0x1E] = 1; // pack = DP30ADPCM
    put(&mut b, i3 + 0x20, &8363u32.to_le_bytes());
    put(&mut b, i3 + 0x4C, b"SCRS");

    // Sample 1 body (32 bytes) at 0x200.
    let s1 = 0x200;
    b.resize(s1 + 32, 0);
    for i in 0..32 {
        b[s1 + i] = (i as u8).wrapping_mul(7);
    }
    // Sample 2 body: 12 stereo 16-bit frames = 24 left + 24 right bytes at 0x300.
    let s2 = 0x300;
    b.resize(s2 + 48, 0);
    for i in 0..48 {
        b[s2 + i] = (i as u8).wrapping_mul(5).wrapping_add(3);
    }
    // Sample 3 body at 0x380: 16-byte ADPCM delta table (slot 1 = +2) then
    // 8 packed bytes of nibble-code 1 pairs → a 2,4,…,32 ramp.
    let s3 = 0x380;
    b.resize(s3 + 24, 0);
    b[s3 + 1] = 2; // table[1] = +2, all other deltas 0
    for i in 0..8 {
        b[s3 + 16 + i] = 0x11;
    }

    // Pattern 0 — spread effects across the four channels and four rows.
    let pat0 = pack_pattern(&[
        &[
            (0, 0x40, 1, 0xFF, 8, 0x42),  // H42 vibrato
            (1, 0x48, 2, 40, 0, 0),       // plain note (porta target next row)
            (2, 0x40, 1, 0xFF, 10, 0x37), // J37 arpeggio
            (3, 0x50, 2, 0xFF, 17, 0x83), // Q83 retrigger
        ],
        &[
            (0, 0xFF, 0, 0xFF, 4, 0x40),  // D40 volslide
            (1, 0x50, 2, 0xFF, 7, 0x04),  // G04 tone porta
            (2, 0xFF, 0, 0xFF, 18, 0x42), // R42 tremolo
            (3, 0xFF, 0, 0xFF, 19, 0x31), // S31 vibrato waveform
        ],
        &[
            (0, 0xFF, 0, 0xFF, 21, 0x21), // U21 fine vibrato
            (1, 0xFF, 0, 0xFF, 11, 0x40), // K40 combined vib+volslide
            (2, 0xFF, 0, 0xFF, 9, 0x24),  // I24 tremor
            (3, 0x50, 2, 0xFF, 15, 0x02), // O02 sample offset
        ],
        &[
            (0, 0xFF, 0, 0xFF, 22, 0x20), // V20 global volume
            (1, 0xFF, 0, 0xFF, 20, 0x50), // T50 tempo
            (2, 0xFF, 0, 0xFF, 19, 0xE1), // SE1 pattern delay
            (3, 0xFE, 0, 0xFF, 0, 0),     // note cut
        ],
    ]);
    put(&mut b, 0x400, &pat0);

    // Pattern 1 — a note, the ADPCM instrument, then a same-row
    // position-jump + pattern-break pair (order from B, row from C).
    let pat1 = pack_pattern(&[
        &[
            (0, 0x44, 1, 0xFF, 0, 0),
            (1, 0x40, 3, 0xFF, 0, 0), // play the DP30ADPCM instrument
        ],
        &[
            (0, 0xFF, 0, 0xFF, 3, 0x02), // C02 break to row 2 …
            (1, 0xFF, 0, 0xFF, 2, 0x02), // … merged with B02 → order 2, row 2
        ],
    ]);
    put(&mut b, 0x500, &pat1);

    b
}

/// Run the whole public decode pipeline against `bytes`. Any panic here is
/// the test failing. Render is frame-bounded so a looping module returns.
fn drive_pipeline(bytes: &[u8]) {
    let Ok(header) = parse_header(bytes) else {
        return;
    };

    // Mixed-stereo path.
    {
        let samples = extract_samples(&header, bytes);
        let patterns = unpack_all(&header, bytes);
        let mut player = PlayerState::new(&header, samples, patterns, OUT_RATE);
        let mut buf = vec![0i16; 2048];
        for _ in 0..10 {
            if player.render(&mut buf) == 0 {
                break;
            }
        }
    }

    // Per-channel path (different mixer with its own stride assertions).
    {
        let samples = extract_samples(&header, bytes);
        let patterns = unpack_all(&header, bytes);
        let mut player = PlayerState::new(&header, samples, patterns, OUT_RATE);
        let stride = player.channel_count() * 2;
        if stride > 0 {
            let mut buf = vec![0i16; stride * 256];
            for _ in 0..10 {
                if player.render_per_channel(&mut buf) == 0 {
                    break;
                }
            }
        }
    }
}

/// Drive the *registered decoder* public API — the entry point real
/// consumers use — over `bytes`: build each of the two S3M decoders, feed
/// the whole blob as one packet, and pump `receive_frame` to EOF. A frame
/// cap keeps a legitimately-looping module from spinning forever. Exercises
/// the demuxer/decoder state machine (parse-in-`send_packet`, the
/// `Playing`/`Done` transitions, and error propagation) that the direct
/// `PlayerState` path skips.
fn drive_decoder_api(bytes: &[u8]) {
    let tb = TimeBase::new(1, OUT_RATE as i64);
    for codec in ["s3m", "s3m_multichannel"] {
        let mut reg = CodecRegistry::new();
        oxideav_s3m::decoder::register(&mut reg);
        let params = CodecParameters::audio(CodecId::new(codec));
        let Ok(mut dec) = reg.first_decoder(&params) else {
            continue;
        };
        let pkt = Packet::new(0, tb, bytes.to_vec());
        // A malformed header makes `send_packet` return a typed error; that
        // is the correct outcome, not a panic.
        if dec.send_packet(&pkt).is_err() {
            continue;
        }
        // A few frames are enough to cross the Playing -> Done transition and
        // reach the second `receive_frame`; the deep audio path is already
        // fuzzed by `drive_pipeline`, so keep this light for CI.
        let mut frames = 0u32;
        while let Ok(frame) = dec.receive_frame() {
            if let Frame::Audio(_) = frame {
                frames += 1;
            }
            if frames >= 3 {
                break;
            }
        }
    }
}

#[test]
fn seed_module_decodes_cleanly() {
    // Sanity floor: the seed the corpora derive from must itself parse and
    // render some audio, otherwise the corpora would only exercise the
    // early-error path and prove nothing about the deep pipeline.
    let m = build_valid_module();
    let header = parse_header(&m).expect("seed module must parse");
    assert_eq!(header.ord_num, 2);
    assert_eq!(header.ins_num, 1);
    assert_eq!(header.pat_num, 1);
    let samples = extract_samples(&header, &m);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].pcm.len(), 16);
    let patterns = unpack_all(&header, &m);
    let mut player = PlayerState::new(&header, samples, patterns, OUT_RATE);
    let mut buf = vec![0i16; 4096];
    let produced = player.render(&mut buf);
    assert!(produced > 0, "seed module must render at least one frame");
}

#[test]
fn rich_module_decodes_cleanly() {
    // The feature-rich seed must itself parse and render, so its mutation
    // corpus exercises the stereo / pan-block / effect-heavy deep paths
    // rather than only the early-error path.
    let m = build_rich_module();
    let header = parse_header(&m).expect("rich module must parse");
    assert_eq!(header.ins_num, 3);
    assert_eq!(header.pat_num, 2);
    assert_eq!(header.enabled_channels, 4);
    let samples = extract_samples(&header, &m);
    assert_eq!(samples.len(), 3);
    assert_eq!(samples[0].pcm.len(), 32);
    assert!(samples[0].is_looped());
    // Instrument 2 is true stereo: both channel buffers populated.
    assert_eq!(samples[1].pcm.len(), 12);
    assert!(samples[1].pcm_right.is_some());
    // Instrument 3 is DP30ADPCM-packed: the +2-delta nibble ramp must
    // depack to 2, 4, …, 32 (scaled by 256) end-to-end from the file bytes.
    let ramp: Vec<i16> = (1..=16).map(|i| i * 2 * 256).collect();
    assert_eq!(samples[2].pcm, ramp);
    assert!(samples[2].pcm_right.is_none());
    let patterns = unpack_all(&header, &m);
    let mut player = PlayerState::new(&header, samples, patterns, OUT_RATE);
    let mut buf = vec![0i16; 8192];
    let mut total = 0usize;
    for _ in 0..32 {
        let n = player.render(&mut buf);
        total += n;
        if n == 0 {
            break;
        }
    }
    assert!(total > 0, "rich module must render audio");
}

#[test]
fn rich_module_truncation_prefixes_never_panic() {
    let m = build_rich_module();
    for len in 0..=m.len() {
        drive_pipeline(&m[..len]);
    }
}

#[test]
fn registered_decoder_api_survives_truncation_and_mutation() {
    // The consumer-facing path (CodecRegistry -> first_decoder ->
    // send_packet -> receive_frame) must be as robust as the direct pipeline:
    // truncations and byte-mutations of both seeds resolve to a typed error
    // or a bounded, panic-free frame stream. The deep audio rendering is
    // fuzzed exhaustively by `drive_pipeline`; here we keep the frame pump
    // shallow and sample the truncation space so the state-machine coverage
    // stays cheap.
    for seed in [build_valid_module(), build_rich_module()] {
        let mut len = 0;
        while len <= seed.len() {
            drive_decoder_api(&seed[..len]);
            len += 8;
        }
    }

    let mut rng = Rng::new(0x0A11CE5E_ED0DDBAD);
    for seed in [build_valid_module(), build_rich_module()] {
        for _ in 0..600 {
            let mut m = seed.clone();
            let mutations = 1 + rng.below(8);
            for _ in 0..mutations {
                let idx = rng.below(m.len() as u32) as usize;
                m[idx] = rng.next_u32() as u8;
            }
            drive_decoder_api(&m);
        }
    }
}

#[test]
fn decoder_rejects_a_second_packet() {
    // The S3M decoder consumes the whole song as one packet; a second
    // packet must produce a typed error rather than corrupt state or panic.
    let m = build_valid_module();
    let mut reg = CodecRegistry::new();
    oxideav_s3m::decoder::register(&mut reg);
    let params = CodecParameters::audio(CodecId::new("s3m"));
    let mut dec = reg.first_decoder(&params).expect("s3m decoder");
    let tb = TimeBase::new(1, OUT_RATE as i64);
    dec.send_packet(&Packet::new(0, tb, m.clone()))
        .expect("first packet accepted");
    assert!(
        dec.send_packet(&Packet::new(0, tb, m)).is_err(),
        "a second packet must be rejected"
    );
}

#[test]
fn rich_module_byte_mutations_never_panic() {
    // Same mutation strategy as the minimal seed but over the effect-heavy /
    // stereo / pan-block module, so corruption reaches the per-tick effect
    // kernels, the stereo sample splitter, the Amiga clamp, and the pan
    // resolver.
    let seed = build_rich_module();
    let mut rng = Rng::new(0xC0FFEE11_5EED1234);
    for _ in 0..4000 {
        let mut m = seed.clone();
        let mutations = 1 + rng.below(10);
        for _ in 0..mutations {
            let idx = rng.below(m.len() as u32) as usize;
            m[idx] = rng.next_u32() as u8;
        }
        drive_pipeline(&m);
    }
}

#[test]
fn truncation_prefixes_never_panic() {
    // Every prefix of a valid module — from the empty slice up to the full
    // file — must parse-or-error and (if it parses) render without panicking.
    // This walks the buffer bounds of every length-derived read in the
    // header, parapointer, instrument, sample and pattern parsers.
    let m = build_valid_module();
    for len in 0..=m.len() {
        drive_pipeline(&m[..len]);
    }
}

#[test]
fn random_buffers_never_panic() {
    // Purely random byte buffers. Half are stamped with the `SCRM`
    // signature and the type byte so `parse_header` proceeds past the magic
    // gate into the count-driven parapointer / instrument / pattern loops
    // with fully adversarial field values.
    let mut rng = Rng::new(0x0BADC0DE_5C4EA3D5);
    for _ in 0..4000 {
        let len = rng.below(600) as usize;
        let mut buf = vec![0u8; len];
        for byte in buf.iter_mut() {
            *byte = rng.next_u32() as u8;
        }
        if len >= 0x60 && rng.below(2) == 0 {
            buf[0x1D] = 0x10;
            buf[0x2C..0x30].copy_from_slice(b"SCRM");
        }
        drive_pipeline(&buf);
    }
}

#[test]
fn byte_mutations_of_valid_module_never_panic() {
    // Take the known-good module and corrupt 1..=8 random bytes with random
    // values, many thousands of times. This is the corpus most likely to
    // reach deep code paths (valid magic + counts, but poisoned parapointers,
    // lengths, loop windows, note bytes, effect commands, order entries).
    let seed = build_valid_module();
    let mut rng = Rng::new(0xDEADBEEF_C0DE600D);
    for _ in 0..4000 {
        let mut m = seed.clone();
        let mutations = 1 + rng.below(8);
        for _ in 0..mutations {
            let idx = rng.below(m.len() as u32) as usize;
            m[idx] = rng.next_u32() as u8;
        }
        drive_pipeline(&m);
    }
}

#[test]
fn appended_and_shrunk_lengths_never_panic() {
    // Grow the valid module with random trailing garbage and shrink it by
    // random amounts — decouples the file length from the parapointer
    // targets so sample / pattern / instrument offsets land past EOF or
    // straddle the boundary.
    let seed = build_valid_module();
    let mut rng = Rng::new(0xFACEFEED_C0FFEE00);
    for _ in 0..3000 {
        let mut m = seed.clone();
        match rng.below(3) {
            0 => {
                let extra = rng.below(256) as usize;
                for _ in 0..extra {
                    m.push(rng.next_u32() as u8);
                }
            }
            1 => {
                let cut = rng.below(m.len() as u32) as usize;
                m.truncate(m.len() - cut);
            }
            _ => {
                // Poison just the parapointer tables so offsets fly off into
                // space while the header stays otherwise well-formed.
                for off in [0x62usize, 0x63, 0x64, 0x65] {
                    m[off] = rng.next_u32() as u8;
                }
            }
        }
        drive_pipeline(&m);
    }
}
