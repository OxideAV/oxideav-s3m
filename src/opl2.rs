//! OPL2 (Yamaha YM3812) operator synthesis core for S3M AdLib instruments.
//!
//! Scream Tracker 3 modules can carry **AdLib / OPL2 FM instruments**
//! (instrument type `2`, tag `SCRI`) alongside PCM samples: the format
//! reserves nine melodic OPL2 channels driven by the YM3812 on Sound
//! Blaster / AdLib cards. Each AdLib instrument stores the chip's
//! per-operator register bytes directly. This module decodes those
//! register bytes into structured operator parameters and implements the
//! YM3812 *operator core* — the phase generator plus the log-sin /
//! exponential waveform synthesis that turns a phase + attenuation into a
//! signed sample.
//!
//! # Provenance (clean-room)
//!
//! Every numeric table and formula here is sourced from silicon
//! reverse-engineering documents staged under `docs/audio/`:
//!
//! - **log-sin and exponential ROM tables** — the YM3812 contains two
//!   256-entry ROMs (a first-quadrant log-sin table and an exponential
//!   table). Their exact contents were recovered by die decapsulation and
//!   shown to be reproduced bit-for-bit by the generating formulas
//!   `y = round(-log2(sin((x+0.5)*pi/512)) * 256)` and
//!   `y = round((2^(x/256) - 1) * 1024)` (Gambrell & Niemitalo,
//!   *OPLx decapsulated*, 2008 —
//!   `docs/audio/nsf/opll-ym2413/oplx-decapsulated-gambrell-niemitalo-2008.txt`).
//!   We generate the tables from those formulas at startup; they are the
//!   factual ROM content, not authored data.
//! - **operator output formula** `out = exp(logsin(phase) + gain)` and the
//!   full-period sine reconstruction from the first quadrant (mirror +
//!   sign symmetry) — same decapsulation article, cross-checked against
//!   the independent silicon-RE writeup
//!   `docs/audio/nsf/opll-ym2413/ym2413-logsin-exp-tables-andete-2015-04-09.txt`,
//!   which establishes the ROMs are **identical across OPLL / OPL2 / OPL3**.
//! - **phase-step formula, MUL multiplier table, FB feedback table, and the
//!   per-operator register bit layout** — shared OPL-family facts
//!   transcribed in `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md`
//!   §1 (register map), §3 (MUL), §5 (FB), §9 (phase step).
//! - **S3M AdLib instrument byte layout** (10 register bytes after a 3-byte
//!   reserved prefix) — `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
//!   "Instrument format → Adlib".
//!
//! # Scope
//!
//! This module provides the *deterministic, fully-documented* half of OPL2
//! AdLib playback: instrument-register decode and the operator waveform
//! core. The OPL2 **envelope generator rates** (the per-rate attack /
//! decay / release increment schedule over OPL2's 9-bit / 96 dB envelope)
//! are not present in the staged docs — only the OPLL's 7-bit / 48 dB EG
//! is reverse-engineered, and even its attack-level recurrence is an open
//! gap. A complete audible AdLib voice therefore awaits an OPL2-specific
//! envelope-rate trace; see the crate README "AdLib" section.

/// A full sine period is divided into this many phase steps in the OPL
/// family (the phase accumulator's integer part wraps at 1024).
pub const SINE_PERIOD: u32 = 1024;

/// Number of entries in each ROM table (one quadrant of the sine; the
/// full exponential significand table).
const TABLE_LEN: usize = 256;

/// The frequency-multiplier (`MUL`) lookup. The 4-bit MUL field scales the
/// operator phase increment. The half-multiple at index 0 and the
/// duplicated 10/12/15 entries are intrinsic to the OPL family.
///
/// Source: `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §3. The
/// phase-step formula (§9) uses these multiplied by two so the "½" entry
/// becomes 1 — see [`MUL_DOUBLED`].
pub const MUL_TABLE: [u8; 16] = [1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 10, 12, 12, 15, 15];

/// MUL table with each entry doubled (the "½" becomes 1). This is the
/// `mlTab` used directly in the phase-step formula.
///
/// Source: `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §9.
pub const MUL_DOUBLED: [u8; 16] = [1, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 20, 24, 24, 30, 30];

/// Feedback (`FB`) modulation index, expressed in units of `pi/16`
/// (so index 1 = pi/16, 7 = 4*pi). Index 0 disables self-feedback.
///
/// Source: `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §5
/// (`0, pi/16, pi/8, pi/4, pi/2, pi, 2pi, 4pi`).
pub const FB_PI_OVER_16: [u16; 8] = [0, 1, 2, 4, 8, 16, 32, 64];

/// OPL2 operator output waveform select. Only the two shapes that are
/// reverse-engineered in the staged docs are represented: the full sine
/// and the half-wave-rectified sine (the `DC`/`DM` "distortion" bit).
/// The OPL2's other two waveforms (absolute-sine and quarter / pulse-sine,
/// selectable via the OPL2-only `$E0` wave-select register) are not
/// documented in the staged material and are intentionally omitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waveform {
    /// Full sine (WF = 0).
    Sine,
    /// Half-wave-rectified sine: the negative half is silenced (WF = 1).
    /// Source: `ym2413-logsin-exp-tables-andete-2015-04-09.txt` (`DC`/`DM`
    /// half-rectification) and `opll-ym2413-tables.md` §1.
    HalfSine,
}

/// One OPL2 operator's decoded register parameters.
///
/// Field meanings are the register-content semantics from
/// `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Operator {
    /// Amplitude modulation (tremolo) enable (`AM`, register `$2x` D7).
    pub am: bool,
    /// Vibrato enable (`VIB`, `$2x` D6).
    pub vib: bool,
    /// Envelope type: `true` = sustained, `false` = percussive
    /// (`EG-TYP`, `$2x` D5).
    pub eg_sustained: bool,
    /// Key-scale-of-rate (`KSR`, `$2x` D4): speeds the envelope at higher pitch.
    pub ksr: bool,
    /// Frequency multiple (`MUL`, `$2x` D3..D0): index into [`MUL_TABLE`].
    pub mul: u8,
    /// Key-scale-level (`KSL`, `$4x` D7..D6): extra attenuation slope vs pitch.
    pub ksl: u8,
    /// Total level / base attenuation (`TL`, `$4x` D5..D0), 0 = loudest,
    /// 63 = quietest. One TL step = 0.75 dB.
    pub total_level: u8,
    /// Attack rate (`AR`, `$6x` D7..D4).
    pub attack: u8,
    /// Decay rate (`DR`, `$6x` D3..D0).
    pub decay: u8,
    /// Sustain level (`SL`, `$8x` D7..D4).
    pub sustain: u8,
    /// Release rate (`RR`, `$8x` D3..D0).
    pub release: u8,
    /// Output waveform (`WS`, `$Ex` D1..D0 on OPL2; here restricted to the
    /// two documented shapes — see [`Waveform`]).
    pub waveform: Waveform,
}

/// A decoded 2-operator OPL2 AdLib instrument (one modulator + one carrier).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdLibInstrument {
    /// The modulator operator (operator 1 in the FM chain).
    pub modulator: Operator,
    /// The carrier operator (operator 2; its output is the channel output).
    pub carrier: Operator,
    /// Modulator self-feedback (`FB`, `$Cx` D3..D1): index into [`FB_PI_OVER_16`].
    pub feedback: u8,
    /// Connection / algorithm bit (`CNT`, `$Cx` D0): `false` = FM
    /// (modulator phase-modulates carrier), `true` = additive (both
    /// operators sum directly).
    pub additive: bool,
}

/// Decode one OPL2 operator from its five register bytes, laid out the way
/// the YM3812 register map orders them: `$2x $4x $6x $8x $Ex`.
fn decode_operator(reg_20: u8, reg_40: u8, reg_60: u8, reg_80: u8, reg_e0: u8) -> Operator {
    Operator {
        am: reg_20 & 0x80 != 0,
        vib: reg_20 & 0x40 != 0,
        eg_sustained: reg_20 & 0x20 != 0,
        ksr: reg_20 & 0x10 != 0,
        mul: reg_20 & 0x0F,
        ksl: reg_40 >> 6,
        total_level: reg_40 & 0x3F,
        attack: reg_60 >> 4,
        decay: reg_60 & 0x0F,
        sustain: reg_80 >> 4,
        release: reg_80 & 0x0F,
        // OPL2 WS register $E0 carries a 2-bit selector; we only model the
        // two documented shapes. WF=0 → Sine, WF=1 → HalfSine; the
        // undocumented values 2/3 fall back to Sine.
        waveform: if reg_e0 & 0x03 == 1 {
            Waveform::HalfSine
        } else {
            Waveform::Sine
        },
    }
}

impl AdLibInstrument {
    /// Decode an AdLib instrument from the 10 (or more) register bytes stored
    /// in an S3M `SCRI` instrument body.
    ///
    /// Per `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
    /// ("Instrument format → Adlib"), the AdLib register block begins after
    /// the type byte, the 12-byte DOS name, and 3 reserved bytes (so at
    /// instrument-header offset `0x10`). The classic OPL2 instrument
    /// register order — the same one used on the Sound Blaster — is, per the
    /// YM3812 register map (`opll-ym2413-tables.md` §1):
    ///
    /// ```text
    /// byte 0: $20 modulator  AM VIB EGT KSR MUL
    /// byte 1: $20 carrier    AM VIB EGT KSR MUL
    /// byte 2: $40 modulator  KSL TL
    /// byte 3: $40 carrier    KSL TL
    /// byte 4: $60 modulator  AR DR
    /// byte 5: $60 carrier    AR DR
    /// byte 6: $80 modulator  SL RR
    /// byte 7: $80 carrier    SL RR
    /// byte 8: $E0 modulator  WS (low 2 bits)
    /// byte 9: $E0 carrier    WS (low 2 bits)
    /// byte 10: $C0           FB CNT   (feedback + connection)
    /// ```
    ///
    /// ST3 documents "10 bytes: Adlib registers"; the eleventh `$C0` byte
    /// (feedback / connection) immediately follows in the instrument body.
    /// When fewer than 11 bytes are available the missing registers decode
    /// as zero (full sine, no feedback, FM connection), matching a silent /
    /// default OPL2 voice.
    pub fn from_registers(regs: &[u8]) -> Self {
        let g = |i: usize| regs.get(i).copied().unwrap_or(0);
        let modulator = decode_operator(g(0), g(2), g(4), g(6), g(8));
        let carrier = decode_operator(g(1), g(3), g(5), g(7), g(9));
        let cx = g(10);
        AdLibInstrument {
            modulator,
            carrier,
            feedback: (cx >> 1) & 0x07,
            additive: cx & 0x01 != 0,
        }
    }
}

/// The YM3812 operator core: the two decapsulated ROM tables plus the
/// lookup logic that turns a 10-bit phase and an attenuation into a signed
/// output sample.
#[derive(Clone)]
pub struct OperatorCore {
    /// First-quadrant log-sin ROM: `-log2(sin)` scaled by 256 (12-bit).
    logsin: [u16; TABLE_LEN],
    /// Exponential ROM significand: `(2^(x/256) - 1) * 1024` (10-bit).
    exp: [u16; TABLE_LEN],
}

impl Default for OperatorCore {
    fn default() -> Self {
        Self::new()
    }
}

impl OperatorCore {
    /// Build the operator core, generating the two ROM tables from the
    /// decapsulated generating formulas (the factual YM3812 ROM content).
    pub fn new() -> Self {
        let mut logsin = [0u16; TABLE_LEN];
        let mut exp = [0u16; TABLE_LEN];
        for (i, slot) in logsin.iter_mut().enumerate() {
            // y = round(-log2(sin((i + 0.5) * pi / 256 / 2)) * 256)
            let angle = (i as f64 + 0.5) * std::f64::consts::PI / 256.0 / 2.0;
            *slot = (-(angle.sin().log2()) * 256.0).round() as u16;
        }
        for (i, slot) in exp.iter_mut().enumerate() {
            // y = round((2^(i/256) - 1) * 1024)
            *slot = ((2f64.powf(i as f64 / 256.0) - 1.0) * 1024.0).round() as u16;
        }
        OperatorCore { logsin, exp }
    }

    /// Read the 12-bit log-sin value for a full-period phase (0..1024),
    /// reconstructing quadrants 2/3/4 from the stored first quadrant. The
    /// returned value is the attenuation magnitude; `negative` reports the
    /// sign (quadrants 3 & 4).
    ///
    /// Source: `ym2413-logsin-exp-tables-andete-2015-04-09.txt` —
    /// mirror around pi/2 = flip the low 8 index bits; sign for the lower
    /// half of the period.
    fn logsin_lookup(&self, phase: u32) -> (u16, bool) {
        let p = (phase % SINE_PERIOD) as usize;
        let negative = p & 0x200 != 0; // bit 9: lower half of the period
        let mirror = p & 0x100 != 0; // bit 8: second/fourth quadrant
        let idx = p & 0xFF;
        let table_idx = if mirror { idx ^ 0xFF } else { idx };
        (self.logsin[table_idx], negative)
    }

    /// Convert an attenuation (in the log domain, 8 fractional bits) back to
    /// a linear significand via the exponential ROM, applying the integer
    /// part as a right-shift.
    ///
    /// Source: `oplx-decapsulated-gambrell-niemitalo-2008.txt` (the
    /// table value + 1024 is the significand; the MSBs are the exponent) and
    /// `ym2413-logsin-exp-tables-andete-2015-04-09.txt`.
    fn exp_lookup(&self, atten: u32) -> u32 {
        let frac = (atten & 0xFF) as usize;
        let int = (atten >> 8).min(31);
        // Complement the fractional index (the log-sin table stores the
        // *negative* logarithm), restore the hidden bit-10, then shift by the
        // integer part.
        let significand = (self.exp[frac ^ 0xFF] as u32) | 0x400;
        significand >> int
    }

    /// Compute one operator sample for the given phase and total attenuation.
    ///
    /// `phase` is the 10-bit phase accumulator integer part (the full
    /// period is [`SINE_PERIOD`]). `atten` is the total attenuation applied
    /// in the log domain (8 fractional bits): the sum of total-level,
    /// envelope, key-scale-level, and any modulation already folded into the
    /// phase. The result is a signed amplitude.
    pub fn operator_sample(&self, phase: u32, atten: u32, waveform: Waveform) -> i32 {
        let p = phase % SINE_PERIOD;
        // Half-wave rectification (WF=1): silence the negative half period.
        if waveform == Waveform::HalfSine && (p & 0x200) != 0 {
            return 0;
        }
        let (logsin, negative) = self.logsin_lookup(p);
        let magnitude = self.exp_lookup(logsin as u32 + atten) as i32;
        if negative && waveform == Waveform::Sine {
            -magnitude
        } else {
            magnitude
        }
    }

    /// Advance a phase accumulator by one output sample for a note.
    ///
    /// The OPL phase step is `((fnum * mlTab[ML]) << block) >> 1`, with the
    /// phase carrying 9 fractional bits. Returns the updated 19-bit
    /// fixed-point accumulator (10 integer + 9 fractional bits).
    ///
    /// Source: `opll-ym2413-tables.md` §9.
    pub fn phase_step(fnum: u16, block: u8, mul: u8) -> u32 {
        let ml = MUL_DOUBLED[(mul & 0x0F) as usize] as u32;
        ((fnum as u32 * ml) << (block & 0x07)) >> 1
    }

    /// The integer (0..1024) phase from a 19-bit fixed-point accumulator.
    pub fn phase_int(accumulator: u32) -> u32 {
        (accumulator >> 9) & (SINE_PERIOD - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logsin_table_matches_decapsulated_anchors() {
        let core = OperatorCore::new();
        // Anchor values from Table II of
        // oplx-decapsulated-gambrell-niemitalo-2008.txt.
        assert_eq!(core.logsin[0], 2137);
        assert_eq!(core.logsin[1], 1731);
        assert_eq!(core.logsin[2], 1543);
        assert_eq!(core.logsin[128], 127);
        assert_eq!(core.logsin[255], 0);
    }

    #[test]
    fn exp_table_matches_decapsulated_anchors() {
        let core = OperatorCore::new();
        // Anchor values from Table I of the decapsulation article.
        assert_eq!(core.exp[0], 0);
        assert_eq!(core.exp[1], 3);
        assert_eq!(core.exp[2], 6);
        assert_eq!(core.exp[255], 1018);
        assert_eq!(core.exp[128], 424);
    }

    #[test]
    fn full_sine_has_quadrant_symmetry() {
        let core = OperatorCore::new();
        // A max-volume (zero extra attenuation) sine over the 1024-step
        // period must be symmetric: q1 rises, then mirrors, then the second
        // half is the negation of the first.
        let s = |p: u32| core.operator_sample(p, 0, Waveform::Sine);
        // Peak is near a quarter period.
        let peak = s(256);
        assert!(
            peak > 240,
            "peak amplitude near quarter period too low: {peak}"
        );
        // Sign symmetry: phase p and p+512 are negatives of each other.
        for p in 0..512 {
            assert_eq!(s(p), -s(p + 512), "sign symmetry broken at phase {p}");
        }
        // Mirror symmetry within the first half (around the quarter point).
        for d in 0..256 {
            assert_eq!(s(d), s(511 - d), "mirror symmetry broken at offset {d}");
        }
    }

    #[test]
    fn half_sine_silences_negative_half() {
        let core = OperatorCore::new();
        // The full lower half of the period (phase bit 9 set) is silent.
        for p in 512..1024 {
            assert_eq!(
                core.operator_sample(p, 0, Waveform::HalfSine),
                0,
                "half-sine should be silent at phase {p}"
            );
        }
        // The upper half matches the full sine there.
        for p in 0..512 {
            assert_eq!(
                core.operator_sample(p, 0, Waveform::HalfSine),
                core.operator_sample(p, 0, Waveform::Sine)
            );
        }
    }

    #[test]
    fn attenuation_halves_amplitude_per_volume_step() {
        let core = OperatorCore::new();
        // One "volume step" is +128 in the log domain (= -3 dB ≈ *0.7071).
        // Compare the peak amplitude at successive attenuations near the
        // quarter-period peak.
        let peak0 = core.operator_sample(256, 0, Waveform::Sine);
        let peak1 = core.operator_sample(256, 128, Waveform::Sine);
        let peak2 = core.operator_sample(256, 256, Waveform::Sine);
        // -3 dB per step → roughly 1/sqrt(2) each time.
        let target = std::f64::consts::FRAC_1_SQRT_2;
        let ratio1 = peak1 as f64 / peak0 as f64;
        let ratio2 = peak2 as f64 / peak1 as f64;
        assert!((ratio1 - target).abs() < 0.05, "step 1 ratio {ratio1}");
        assert!((ratio2 - target).abs() < 0.05, "step 2 ratio {ratio2}");
    }

    #[test]
    fn phase_step_matches_formula() {
        // fnum, block, mul → ((fnum * mlTab[mul]) << block) >> 1
        // mul=1 → mlTab=2; fnum=512, block=0 → (512*2)>>1 = 512.
        assert_eq!(OperatorCore::phase_step(512, 0, 1), 512);
        // block raises the octave: block=1 doubles the step.
        assert_eq!(OperatorCore::phase_step(512, 1, 1), 1024);
        // mul=0 → mlTab=1 (the "½" multiple): (512*1)>>1 = 256.
        assert_eq!(OperatorCore::phase_step(512, 0, 0), 256);
        // mul=A (index 10) → mlTab=20.
        assert_eq!(OperatorCore::phase_step(100, 0, 10), (100 * 20) >> 1);
    }

    #[test]
    fn phase_accumulator_wraps_at_period() {
        // Accumulate fnum=512,block=0,mul=1 (step 512 in 9-frac fixed point
        // means 512/512 = 1 integer phase unit per sample). After 1024
        // samples the integer phase has advanced one full period.
        let step = OperatorCore::phase_step(512, 0, 1);
        let mut acc = 0u32;
        let start = OperatorCore::phase_int(acc);
        for _ in 0..SINE_PERIOD {
            acc = acc.wrapping_add(step);
        }
        assert_eq!(OperatorCore::phase_int(acc), start);
    }

    #[test]
    fn decode_operator_unpacks_register_fields() {
        // $20 = 0xB5 = 1011_0101: AM=1 VIB=0 EGT=1 KSR=1 MUL=5
        // $40 = 0x4E = 01_001110: KSL=1 TL=14
        // $60 = 0xF3 = 1111_0011: AR=15 DR=3
        // $80 = 0x28 = 0010_1000: SL=2 RR=8
        // $E0 = 0x01 → HalfSine
        let op = decode_operator(0xB5, 0x4E, 0xF3, 0x28, 0x01);
        assert!(op.am);
        assert!(!op.vib);
        assert!(op.eg_sustained);
        assert!(op.ksr);
        assert_eq!(op.mul, 5);
        assert_eq!(op.ksl, 1);
        assert_eq!(op.total_level, 14);
        assert_eq!(op.attack, 15);
        assert_eq!(op.decay, 3);
        assert_eq!(op.sustain, 2);
        assert_eq!(op.release, 8);
        assert_eq!(op.waveform, Waveform::HalfSine);
    }

    #[test]
    fn adlib_instrument_decodes_two_operators_plus_feedback() {
        // 11 bytes: mod $20, car $20, mod $40, car $40, mod $60, car $60,
        // mod $80, car $80, mod $E0, car $E0, $C0.
        // $C0 = 0x0D = 0000_1101: FB = (0x0D>>1)&7 = 6, CNT = 1 (additive).
        let regs = [
            0x21, 0x61, 0x1D, 0x07, 0x82, 0x81, 0x10, 0x07, 0x00, 0x00, 0x0D,
        ];
        let inst = AdLibInstrument::from_registers(&regs);
        assert_eq!(inst.modulator.mul, 1);
        assert_eq!(inst.carrier.mul, 1);
        assert_eq!(inst.feedback, 6);
        assert!(inst.additive);
        assert_eq!(inst.modulator.waveform, Waveform::Sine);
    }

    #[test]
    fn adlib_instrument_tolerates_short_register_block() {
        // Fewer than 11 bytes → missing registers read as zero defaults.
        let inst = AdLibInstrument::from_registers(&[0x01, 0x01]);
        assert_eq!(inst.modulator.mul, 1);
        assert_eq!(inst.carrier.mul, 1);
        assert_eq!(inst.feedback, 0);
        assert!(!inst.additive);
        assert_eq!(inst.carrier.waveform, Waveform::Sine);
    }

    #[test]
    fn mul_doubled_is_mul_table_with_half_promoted() {
        // MUL_DOUBLED entry should be 2x the rational multiple, with the
        // "½" (index 0) promoted to 1.
        assert_eq!(MUL_DOUBLED[0], 1);
        for i in 1..16 {
            assert_eq!(MUL_DOUBLED[i], MUL_TABLE[i] * 2, "mismatch at index {i}");
        }
    }
}
