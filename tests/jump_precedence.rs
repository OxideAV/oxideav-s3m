//! End-to-end same-row `Bxx` + `Cxx` precedence over real `.s3m` bytes.
//!
//! Per `docs/audio/trackers/s3m/s3m-position-jump-pattern-break-and-adpcm.md`
//! §Part 1, ST3 holds a target *order* and a target *row* as two separate
//! pieces of playback state: `Bxx` writes the order, `Cxx` writes the row,
//! and a same-row pair merges into "row `Cxx` (decimal) of the pattern at
//! order `Bxx` (hex)" regardless of which channel carries which command.
//!
//! These tests build byte-level modules whose only difference is the
//! jump-cell arrangement on pattern 0 row 0, drive them through the
//! *registered decoder* (the real consumer path), and compare total
//! rendered frame counts — the merge is directly observable as song
//! length, since the row a break lands on decides how many rows play.

use oxideav_core::{CodecId, CodecParameters, CodecRegistry, Frame, Packet, TimeBase};
use oxideav_s3m::container::OUTPUT_SAMPLE_RATE;
use oxideav_s3m::decoder;

/// Write `data` at `off`, growing the buffer as needed.
fn put(buf: &mut Vec<u8>, off: usize, data: &[u8]) {
    if buf.len() < off + data.len() {
        buf.resize(off + data.len(), 0);
    }
    buf[off..off + data.len()].copy_from_slice(data);
}

/// One authored cell: `(channel, note, instrument, volume, command, info)`.
type PatCell = (u8, u8, u8, u8, u8, u8);

/// Pack rows into the S3M packed-pattern stream (2-byte length prefix +
/// records + per-row `0x00` terminator). Rows past the authored ones are
/// left empty by the unpacker.
fn pack_pattern(rows: &[&[PatCell]]) -> Vec<u8> {
    let mut body = Vec::new();
    for row in rows {
        for &(channel, note, instrument, volume, command, info) in row.iter() {
            body.push((channel & 0x1F) | 0x20 | 0x40 | 0x80);
            body.push(note);
            body.push(instrument);
            body.push(volume);
            body.push(command);
            body.push(info);
        }
        body.push(0x00);
    }
    let length = (2 + body.len()) as u16;
    let mut out = length.to_le_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

/// Build a two-channel module: order `[pat0, pat1, end]`, one 16-byte PCM
/// instrument, pattern 0 row 0 carrying the caller's cells, pattern 1
/// fully empty. Speed 6, tempo 125.
fn build_jump_module(row0: &[PatCell]) -> Vec<u8> {
    let mut b = vec![0u8; 0x60];
    put(&mut b, 0x00, b"JUMP-FIXTURE");
    b[0x1C] = 0x1A;
    b[0x1D] = 0x10;
    put(&mut b, 0x20, &4u16.to_le_bytes()); // ord_num (padded even)
    put(&mut b, 0x22, &1u16.to_le_bytes()); // ins_num
    put(&mut b, 0x24, &2u16.to_le_bytes()); // pat_num
    put(&mut b, 0x26, &0u16.to_le_bytes()); // flags
    put(&mut b, 0x28, &0x1320u16.to_le_bytes()); // ST3.20
    put(&mut b, 0x2A, &2u16.to_le_bytes()); // unsigned samples
    put(&mut b, 0x2C, b"SCRM");
    b[0x30] = 64; // global volume
    b[0x31] = 6; // initial speed
    b[0x32] = 125; // initial tempo
    b[0x33] = 0x80 | 48; // master volume + stereo
    for i in 0..32 {
        b[0x40 + i] = 0xFF;
    }
    b[0x40] = 0x00; // channel 0: left PCM
    b[0x41] = 0x01; // channel 1: left PCM

    put(&mut b, 0x60, &[0x00, 0x01, 0xFF, 0xFF]); // order: pat0, pat1, end
    put(&mut b, 0x64, &0x0008u16.to_le_bytes()); // instrument → 0x80
    put(&mut b, 0x66, &0x0010u16.to_le_bytes()); // pat0 → 0x100
    put(&mut b, 0x68, &0x0020u16.to_le_bytes()); // pat1 → 0x200

    // Instrument header at 0x80, sample body (16 bytes) at 0xE0.
    let ins = 0x80;
    b.resize(ins + 80, 0);
    b[ins] = 1; // PCM
    put(&mut b, ins + 0x0E, &0x000Eu16.to_le_bytes());
    put(&mut b, ins + 0x10, &16u32.to_le_bytes());
    b[ins + 0x1C] = 64;
    put(&mut b, ins + 0x20, &8363u32.to_le_bytes());
    put(&mut b, ins + 0x4C, b"SCRS");
    let smp = 0xE0;
    b.resize(smp + 16, 0);
    for i in 0..16 {
        b[smp + i] = (i as u8).wrapping_mul(8);
    }

    put(&mut b, 0x100, &pack_pattern(&[row0]));
    put(&mut b, 0x200, &pack_pattern(&[])); // pattern 1: all rows empty

    b
}

/// Total frames the registered decoder renders for `bytes`.
fn total_frames(bytes: Vec<u8>) -> u64 {
    let mut reg = CodecRegistry::new();
    decoder::register(&mut reg);
    let params = CodecParameters::audio(CodecId::new("s3m"));
    let mut dec = reg.first_decoder(&params).expect("s3m decoder registered");
    let tb = TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64);
    dec.send_packet(&Packet::new(0, tb, bytes))
        .expect("fixture must parse");
    let mut total = 0u64;
    while let Ok(frame) = dec.receive_frame() {
        if let Frame::Audio(a) = frame {
            total += a.samples as u64;
        }
    }
    total
}

/// Frames for `rows` pattern rows at speed 6 / tempo 125: each row is 6
/// ticks of `rate * 2.5 / bpm` frames.
fn frames_for_rows(rows: u64) -> u64 {
    let per_tick = (OUTPUT_SAMPLE_RATE as f32 * 2.5 / 125.0) as u64;
    rows * 6 * per_tick
}

#[test]
fn bxx_then_cxx_lands_on_row_of_named_order() {
    // B01 (ch0) + C60 (ch1) on pattern 0 row 0 → jump to order 01 row 60:
    // one row of pattern 0 plays, then rows 60..=63 of pattern 1 → 5 rows.
    let m = build_jump_module(&[
        (0, 0x40, 1, 0xFF, 2, 0x01), // note + B01
        (1, 0xFF, 0, 0xFF, 3, 0x60), // C60 (decimal row 60)
    ]);
    assert_eq!(total_frames(m), frames_for_rows(5));
}

#[test]
fn cxx_then_bxx_renders_the_same_song_length() {
    // The mirrored arrangement — C60 on ch0, B01 on ch1 — must merge
    // identically: a last-cell-wins reading would let the later B01
    // discard the row-60 target and play all 64 rows of pattern 1.
    let m = build_jump_module(&[
        (0, 0x40, 1, 0xFF, 3, 0x60), // C60 first …
        (1, 0xFF, 0, 0xFF, 2, 0x01), // … then B01 on a later channel
    ]);
    assert_eq!(total_frames(m), frames_for_rows(5));
}

#[test]
fn bare_bxx_plays_the_named_order_from_row_zero() {
    // B01 alone → order 01 row 0: one row of pattern 0 plus the full 64
    // rows of pattern 1. This pins the baseline the merge tests shorten.
    let m = build_jump_module(&[(0, 0x40, 1, 0xFF, 2, 0x01)]);
    assert_eq!(total_frames(m), frames_for_rows(1 + 64));
}

#[test]
fn bare_cxx_breaks_into_the_next_order() {
    // C60 alone → next order (pattern 1) at row 60 → the same 5-row song
    // as the merged pair, confirming the merge adds the order retarget
    // without disturbing the break row.
    let m = build_jump_module(&[(0, 0x40, 1, 0xFF, 3, 0x60)]);
    assert_eq!(total_frames(m), frames_for_rows(5));
}
