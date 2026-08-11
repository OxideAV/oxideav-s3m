//! End-to-end AdLib (OPL2) playback through the real file format.
//!
//! The in-crate player unit tests key `OplVoice`s from hand-built
//! `PlayerState`s; this harness instead authors a complete `.s3m` byte
//! blob — AdLib melody channel in the channel-settings table, a `SCRI`
//! instrument with a real register block, a pattern with a note-on and
//! the AdLib note-off — and drives it through the same public pipeline a
//! consumer uses (`parse_header` → `extract_samples` → `unpack_all` →
//! `PlayerState::render`, plus the registered `CodecRegistry` decoder),
//! asserting the FM voice is audible, releases on note-off, and dies out.

use oxideav_core::{CodecId, CodecParameters, CodecRegistry, Frame, Packet, TimeBase};
use oxideav_s3m::header::parse_header;
use oxideav_s3m::pattern::unpack_all;
use oxideav_s3m::player::PlayerState;
use oxideav_s3m::samples::extract_samples;

const OUT_RATE: u32 = 44_100;

fn put(buf: &mut Vec<u8>, off: usize, data: &[u8]) {
    if buf.len() < off + data.len() {
        buf.resize(off + data.len(), 0);
    }
    buf[off..off + data.len()].copy_from_slice(data);
}

/// Build a valid single-channel AdLib S3M: channel 0 is OPL2 melody slot
/// `A1` (type 16), instrument 1 is a `SCRI` voice (sustained carrier,
/// instant attack, frozen decay, release nibble `rr`), and the pattern
/// carries a C-5 note-on on row 0 plus the AdLib note-off (note byte
/// `0xFE`) on row `cut_row` (skipped when `cut_row >= 64`).
fn build_adlib_module(rr: u8, cut_row: usize) -> Vec<u8> {
    let mut b = vec![0u8; 0x60];
    put(&mut b, 0x00, b"ADLIB-E2E");
    b[0x1C] = 0x1A;
    b[0x1D] = 0x10; // type byte
    put(&mut b, 0x20, &2u16.to_le_bytes()); // ord_num
    put(&mut b, 0x22, &1u16.to_le_bytes()); // ins_num
    put(&mut b, 0x24, &1u16.to_le_bytes()); // pat_num
    put(&mut b, 0x26, &0u16.to_le_bytes()); // flags
    put(&mut b, 0x28, &0x1320u16.to_le_bytes()); // cwt/v: ST3.20
    put(&mut b, 0x2A, &2u16.to_le_bytes()); // ffi
    put(&mut b, 0x2C, b"SCRM");
    b[0x30] = 64; // global volume
    b[0x31] = 6; // initial speed
    b[0x32] = 125; // initial tempo
    b[0x33] = 0x80 | 48; // master volume + stereo bit
    b[0x35] = 0x00; // no pan block
    for i in 0..32 {
        b[0x40 + i] = 0xFF;
    }
    b[0x40] = 16; // channel 0 = AdLib melody A1

    put(&mut b, 0x60, &[0x00, 0xFF]); // order: pattern 0, end
    put(&mut b, 0x62, &0x0008u16.to_le_bytes()); // instrument parapointer → 0x80
    put(&mut b, 0x64, &0x0010u16.to_le_bytes()); // pattern parapointer → 0x100

    // SCRI instrument header (80 bytes) at 0x80.
    let ins = 0x80;
    b.resize(ins + 80, 0);
    b[ins] = 2; // AdLib melody instrument
                // Register block at +0x10 (mod/car interleaved):
                //   $20: EGT=1 | MUL=1;  $40 mod: TL=63 (silent), car: TL=0;
                //   $60: AR=15 DR=0;     $80: SL=0 RR=rr;  $E0: sine;  $C0: 0.
    let regs = [
        0x21,
        0x21,
        0x3F,
        0x00,
        0xF0,
        0xF0,
        rr & 0x0F,
        rr & 0x0F,
        0x00,
        0x00,
        0x00,
    ];
    put(&mut b, ins + 0x10, &regs);
    b[ins + 0x1C] = 64; // default volume (AdLib keeps the full 64)
    put(&mut b, ins + 0x20, &8363u32.to_le_bytes()); // C frequency
    put(&mut b, ins + 0x4C, b"SCRI");

    // Packed pattern at 0x100: row 0 note-on, optional cut row, 64 rows.
    let pat = 0x100;
    let mut body = Vec::new();
    for row in 0..64usize {
        if row == 0 {
            // Channel 0 | note+instrument present: C-5 (0x40), inst 1.
            body.extend_from_slice(&[0x20, 0x40, 0x01]);
        } else if row == cut_row {
            // AdLib note-off.
            body.extend_from_slice(&[0x20, 0xFE, 0x01]);
        }
        body.push(0x00); // end of row
    }
    let length = (2 + body.len()) as u16;
    put(&mut b, pat, &length.to_le_bytes());
    put(&mut b, pat + 2, &body);
    b
}

#[test]
fn adlib_module_parses_with_the_melody_slot_live() {
    let bytes = build_adlib_module(4, 8);
    let header = parse_header(&bytes).expect("valid module");
    assert!(header.adlib[0], "channel 0 must be typed AdLib");
    assert!(!header.muted[0], "melody slot must not be muted");
    assert_eq!(header.enabled_channels, 1);
    let samples = extract_samples(&header, &bytes);
    let voice = samples[0].adlib.expect("SCRI register block decoded");
    assert_eq!(voice.carrier.attack, 15);
    assert_eq!(voice.carrier.release, 4);
    assert_eq!(voice.modulator.total_level, 63);
    assert!(voice.carrier.eg_sustained);
}

#[test]
fn adlib_module_renders_audible_audio_from_file_bytes() {
    let bytes = build_adlib_module(4, 63);
    let header = parse_header(&bytes).expect("valid module");
    let samples = extract_samples(&header, &bytes);
    let patterns = unpack_all(&header, &bytes);
    let mut player = PlayerState::new(&header, samples, patterns, OUT_RATE);
    let mut buf = vec![0i16; 8192];
    let frames = player.render(&mut buf);
    assert!(frames > 0);
    let peak = buf[..frames * 2]
        .iter()
        .map(|s| s.unsigned_abs())
        .max()
        .unwrap();
    assert!(peak > 2000, "AdLib module inaudible, peak {peak}");
}

#[test]
fn adlib_noteoff_in_file_bytes_releases_then_silences() {
    // Note-off on row 4 with a fast release (RR nibble 12 => RATE 50
    // with the C-5 key scaling => sub-millisecond): the row after the
    // cut must still carry a short audible tail, and the render must be
    // fully silent well before the pattern ends.
    let bytes = build_adlib_module(12, 4);
    let header = parse_header(&bytes).expect("valid module");
    let samples = extract_samples(&header, &bytes);
    let patterns = unpack_all(&header, &bytes);
    let mut player = PlayerState::new(&header, samples, patterns, OUT_RATE);
    // 64 rows * 6 ticks * 882 samples/tick = 338 688 frames total.
    let mut buf = vec![0i16; 338_688 * 2];
    let frames = player.render(&mut buf);
    assert!(frames > 0);
    let cut_frame = 4 * 6 * 882; // row 4 boundary
    let last_nonzero = buf[..frames * 2]
        .chunks(2)
        .rposition(|f| f[0] != 0 || f[1] != 0)
        .expect("audio expected");
    assert!(
        last_nonzero >= cut_frame,
        "voice died before its note-off row: {last_nonzero} < {cut_frame}"
    );
    assert!(
        last_nonzero < cut_frame + 882,
        "fast release must silence within the next row: {last_nonzero}"
    );
}

#[test]
fn adlib_module_decodes_through_the_registered_codec() {
    let bytes = build_adlib_module(4, 63);
    let tb = TimeBase::new(1, OUT_RATE as i64);
    for codec in ["s3m", "s3m_multichannel"] {
        let mut reg = CodecRegistry::new();
        oxideav_s3m::decoder::register(&mut reg);
        let params = CodecParameters::audio(CodecId::new(codec));
        let mut dec = reg.first_decoder(&params).expect("decoder registered");
        dec.send_packet(&Packet::new(0, tb, bytes.clone()))
            .expect("valid module accepted");
        let mut nonzero = false;
        let mut frames = 0u32;
        while let Ok(frame) = dec.receive_frame() {
            if let Frame::Audio(a) = frame {
                nonzero |= a.data.iter().flatten().any(|&b| b != 0);
                frames += 1;
            }
            if frames >= 4 {
                break;
            }
        }
        assert!(frames > 0, "{codec}: no frames decoded");
        assert!(
            nonzero,
            "{codec}: AdLib audio all-zero through the codec API"
        );
    }
}
