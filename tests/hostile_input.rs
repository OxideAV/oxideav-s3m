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
        for _ in 0..16 {
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
            for _ in 0..16 {
                if player.render_per_channel(&mut buf) == 0 {
                    break;
                }
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
    for _ in 0..8000 {
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
