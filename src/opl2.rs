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
//! This module provides instrument-register decode, the operator waveform
//! core, the **envelope generator** (rate resolution + ADSR trajectory),
//! and the two-operator [`OplVoice`] synthesizer the player mixes from.
//!
//! The envelope-rate facts come from the staged acquisition record
//! `docs/audio/trackers/s3m/s3m-adlib-opl2-envelope-rates.md` (issue #262
//! closure) and its table
//! `docs/audio/trackers/s3m/tables/opl2-ksr-rate-offset.csv`, both
//! transcribed from the Yamaha Y8950 MSX-AUDIO Application Manual §3-1-17
//! (the Y8950 / YM3526 / YM3812 share this FM envelope core; the YM3812
//! adds only per-operator waveform select). What the record pins exactly:
//!
//! - `RATE = 4 * R + Rks`, with the special case `R == 0 ⇒ RATE = 0`
//!   (envelope frozen), where `R` is the 4-bit ADSR register nibble and
//!   `Rks` the key-scale offset ([`KSR_RATE_OFFSET`]).
//! - The vendor envelope-time table (Y8950 manual p.25, Table 3-6) is
//!   indexed by the *post-key-scaling* `RATE`, decomposed `RM-RL`
//!   (`RATE = RM*4 + RL`), and obeys an exact halving law: `RM + 1`
//!   halves every time. Two sample values at `RM-RL = 1-0` (RATE 4) are
//!   quoted and anchor the absolute timing here (see
//!   [`ATTACK_FULL_SCALE_MS_AT_RATE_4`] / [`DECAY_FULL_SCALE_MS_AT_RATE_4`]).
//!
//! The record deliberately does **not** transcribe the full 64×4 time
//! table (the available scan is too low-resolution to trust); the
//! `RL` sub-steps inside one `RM` group are therefore interpolated
//! geometrically (`2^(-RL/4)`, keeping the pinned exact-halving per +4)
//! until the 16 base values at `RM = 1` are staged. The attack *curve
//! shape* between its endpoints is likewise unpinned (only the full-scale
//! traversal time is quoted), so attack runs linear-in-dB here. See the
//! per-item comments below for what is anchor vs. interpolation.

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

// ===================== Envelope generator =====================

/// Key-scale rate offset `Rks`, indexed `[KSR][key_scale_number]`.
///
/// Transcription of Table III-2 "Key scales for RATE" (Y8950 Application
/// Manual §3-1-17), staged as
/// `docs/audio/trackers/s3m/tables/opl2-ksr-rate-offset.csv` (+`.meta`).
/// The closed form is `Rks = KSR ? N : N >> 2` — the staged table and the
/// closed form agree in all 32 entries; a conformance test cross-checks
/// this constant against the CSV file when the docs tree is present.
pub const KSR_RATE_OFFSET: [[u8; 16]; 2] = [
    // KSR = 0: Rks = N >> 2 (0..3)
    [0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3],
    // KSR = 1: Rks = N (0..15)
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
];

/// Upper bound of the envelope-rate domain. The vendor envelope-time
/// table is indexed by the `RM-RL` decomposition of a 6-bit RATE
/// (0..=63), but `RATE = 4*R + Rks` reaches 75 at `R = 15`, `KSR = 1`,
/// `N = 15`. The staged acquisition record flags this overflow as
/// unresolved ("A player must clamp; which clamp matches the hardware is
/// unresolved") — we saturate at 63, the table's own top row.
pub const RATE_MAX: u8 = 63;

/// The key-scale offset `Rks` for one operator ([`KSR_RATE_OFFSET`]).
#[inline]
pub fn key_scale_offset(ksr: bool, key_scale_number: u8) -> u8 {
    KSR_RATE_OFFSET[usize::from(ksr)][(key_scale_number & 0x0F) as usize]
}

/// Resolve a 4-bit ADSR register nibble `R` into the envelope RATE the
/// generator actually runs at:
///
/// ```text
/// RATE = 4 * R + Rks        and, as a special case, R == 0 => RATE = 0
/// ```
///
/// per the staged acquisition record
/// (`docs/audio/trackers/s3m/s3m-adlib-opl2-envelope-rates.md`, from
/// Y8950 Application Manual §3-1-17). `RATE = 0` means the envelope does
/// not change at all ("there is no change in the envelope when RATE is
/// 0"). The result saturates at [`RATE_MAX`] — see there for why.
///
/// Because `R >= 1` implies `RATE >= 4`, the reachable domain is
/// `{0} ∪ [4, 63]`, exactly the rows the vendor time table prints
/// (`15 3` down to `1 0`).
pub fn effective_rate(r: u8, ksr: bool, key_scale_number: u8) -> u8 {
    let r = r & 0x0F;
    if r == 0 {
        return 0;
    }
    let rate = 4 * r as u16 + key_scale_offset(ksr, key_scale_number) as u16;
    rate.min(RATE_MAX as u16) as u8
}

/// Full 0 dB → 96 dB EG attack traversal time in ms at `RATE = 4`
/// (`RM-RL = 1-0`).
///
/// Anchor value quoted verbatim by the staged acquisition record
/// (`s3m-adlib-opl2-envelope-rates.md`: "2826.24 ms at 1-0 becoming
/// 1413.12 ms at 2-0 in the 0 dB→96 dB attack column", Y8950 manual
/// p.25 Table 3-6).
pub const ATTACK_FULL_SCALE_MS_AT_RATE_4: f64 = 2826.24;

/// Full-scale (96 dB) EG decay traversal time in ms at `RATE = 4`
/// (`RM-RL = 1-0`).
///
/// Anchor value quoted verbatim by the staged acquisition record
/// ("a decay of 8212.48 ms at 1-0 becoming 4106.24 ms at 2-0"). The
/// record does not say which of the two decay columns (10%→90% vs
/// 0 dB→96 dB) this sample came from; we read it as the 0 dB→96 dB
/// column — the same measurement reference the attack quote names — and
/// flag the ambiguity for the docs follow-up that stages the full table.
/// Release uses this same constant: in EG terms a release is a decay run
/// at the RR-derived RATE, and the vendor table ("Attack and decay time
/// according to RATE") has no separate release column.
pub const DECAY_FULL_SCALE_MS_AT_RATE_4: f64 = 8212.48;

/// Total span of the OPL2 envelope: 9-bit attenuation over 96 dB.
pub const EG_RANGE_DB: f64 = 96.0;

/// Full-scale traversal time in ms for an envelope RATE `>= 4`.
///
/// The RM dimension is the staged doc's exact halving law ("incrementing
/// RM by one halves every time, verified on sampled entries in all four
/// columns"); the RL dimension inside one RM group is a geometric
/// interpolation (`2^(-RL/4)`) chosen so each +4 in RATE still halves
/// exactly — the true per-RL values await the vendor table's 16 base
/// values at RM = 1 (deliberately not yet staged; see the module doc).
pub fn full_scale_traversal_ms(base_ms_at_rate_4: f64, rate: u8) -> f64 {
    debug_assert!(rate >= 4, "RATE {rate} below the table's 1-0 row");
    base_ms_at_rate_4 * (2.0f64).powf(-((rate as f64) - 4.0) / 4.0)
}

/// dB moved per output sample at the given envelope RATE (0 = frozen).
fn db_per_sample(rate: u8, base_ms_at_rate_4: f64, sample_rate: u32) -> f64 {
    if rate == 0 {
        return 0.0;
    }
    let ms = full_scale_traversal_ms(base_ms_at_rate_4, rate);
    EG_RANGE_DB / (ms / 1000.0 * sample_rate.max(1) as f64)
}

/// Attenuation contributed by the sustain-level nibble, in dB.
///
/// One SL unit is taken as one 3 dB volume step — the `-3 dB per volume
/// step` log-domain identity the operator core's exp/log tables encode
/// (one step = 128 log units; see `attenuation_halves_amplitude_per_
/// volume_step`). The vendor SL scaling (and whether SL = 15 is
/// special-cased deeper) is not in the staged docs and is flagged as a
/// docs gap; 3 dB/step is the documented interim reading.
pub const SUSTAIN_DB_PER_STEP: f32 = 3.0;

/// log-domain attenuation units per dB. The exponential ROM halves the
/// amplitude every 256 units (one right-shift), i.e. 256 units =
/// 20·log10(2) ≈ 6.0206 dB.
pub const LOG_UNITS_PER_DB: f64 = 256.0 / 6.020_599_913;

/// ADSR stage of one operator's envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EgStage {
    /// Attenuation ramps from its current value down toward 0 dB.
    Attack,
    /// Attenuation ramps up toward the sustain level.
    Decay,
    /// Holding at the sustain level (sustained instruments, key still on).
    Sustain,
    /// Attenuation ramps up toward the 96 dB floor.
    Release,
    /// Past the 96 dB floor — the operator is silent and finished.
    Off,
}

/// One operator's envelope generator.
///
/// Timing is anchored to the two staged vendor-table samples (see
/// [`ATTACK_FULL_SCALE_MS_AT_RATE_4`] / [`DECAY_FULL_SCALE_MS_AT_RATE_4`])
/// with the halving law giving every other RATE. The trajectory between
/// endpoints runs linear-in-dB in all stages: exact for decay/release
/// (a constant-rate attenuation increase is what a log-domain EG does),
/// an approximation for attack, whose curve shape the staged record does
/// not pin (only its full-scale traversal time).
#[derive(Clone, Debug)]
pub struct Envelope {
    stage: EgStage,
    /// Current attenuation in dB, `0.0..=96.0`.
    atten_db: f64,
    attack_db_per_sample: f64,
    decay_db_per_sample: f64,
    release_db_per_sample: f64,
    sustain_db: f64,
    /// EG-TYP register bit: `true` holds at the sustain level until
    /// key-off; `false` (percussive) releases through it immediately.
    sustained: bool,
}

impl Envelope {
    /// Build the envelope for one operator sounding at key-scale number
    /// `key_scale_number`, keyed on (worst case, clamped) at `sample_rate`.
    /// Starts keyed-on in the attack stage from the 96 dB floor.
    pub fn new(op: &Operator, key_scale_number: u8, sample_rate: u32) -> Self {
        let ar = effective_rate(op.attack, op.ksr, key_scale_number);
        let dr = effective_rate(op.decay, op.ksr, key_scale_number);
        let rr = effective_rate(op.release, op.ksr, key_scale_number);
        Envelope {
            stage: EgStage::Attack,
            atten_db: EG_RANGE_DB,
            attack_db_per_sample: db_per_sample(ar, ATTACK_FULL_SCALE_MS_AT_RATE_4, sample_rate),
            decay_db_per_sample: db_per_sample(dr, DECAY_FULL_SCALE_MS_AT_RATE_4, sample_rate),
            release_db_per_sample: db_per_sample(rr, DECAY_FULL_SCALE_MS_AT_RATE_4, sample_rate),
            sustain_db: (op.sustain & 0x0F) as f64 * SUSTAIN_DB_PER_STEP as f64,
            sustained: op.eg_sustained,
        }
    }

    /// Re-key the envelope: restart the attack from the *current*
    /// attenuation (a retrigger on a still-sounding voice attacks from
    /// wherever the envelope is, not from silence).
    pub fn key_on(&mut self) {
        self.stage = EgStage::Attack;
    }

    /// Key-off: enter the release stage from the current attenuation.
    /// A voice already past the floor stays off.
    pub fn key_off(&mut self) {
        if self.stage != EgStage::Off {
            self.stage = EgStage::Release;
        }
    }

    /// `true` once the envelope has run past the 96 dB floor.
    #[inline]
    pub fn is_off(&self) -> bool {
        self.stage == EgStage::Off
    }

    /// Current stage (for tests / introspection).
    #[inline]
    pub fn stage(&self) -> EgStage {
        self.stage
    }

    /// Current attenuation in dB.
    #[inline]
    pub fn attenuation_db(&self) -> f64 {
        self.atten_db
    }

    /// Advance one output sample; returns the attenuation (dB) to apply
    /// to this sample. A frozen rate (`RATE = 0` — "no change in the
    /// envelope") leaves the stage and attenuation untouched forever.
    pub fn step(&mut self) -> f64 {
        let out = self.atten_db;
        match self.stage {
            EgStage::Attack => {
                if self.attack_db_per_sample > 0.0 {
                    self.atten_db -= self.attack_db_per_sample;
                    if self.atten_db <= 0.0 {
                        self.atten_db = 0.0;
                        self.stage = EgStage::Decay;
                    }
                }
            }
            EgStage::Decay => {
                if self.atten_db >= self.sustain_db {
                    // Sustain level reached: EG-TYP picks hold-vs-release.
                    self.stage = if self.sustained {
                        EgStage::Sustain
                    } else {
                        EgStage::Release
                    };
                } else if self.decay_db_per_sample > 0.0 {
                    self.atten_db = (self.atten_db + self.decay_db_per_sample).min(self.sustain_db);
                }
                // decay RATE 0: frozen where the attack left it.
            }
            EgStage::Sustain => {
                // Held until key_off().
            }
            EgStage::Release => {
                if self.release_db_per_sample > 0.0 {
                    self.atten_db += self.release_db_per_sample;
                    if self.atten_db >= EG_RANGE_DB {
                        self.atten_db = EG_RANGE_DB;
                        self.stage = EgStage::Off;
                    }
                }
                // release RATE 0: frozen — the voice never dies down.
            }
            EgStage::Off => {}
        }
        out
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

    /// A plain operator to hang envelope parameters off in tests.
    fn test_op(
        attack: u8,
        decay: u8,
        sustain: u8,
        release: u8,
        ksr: bool,
        sustained: bool,
    ) -> Operator {
        Operator {
            am: false,
            vib: false,
            eg_sustained: sustained,
            ksr,
            mul: 1,
            ksl: 0,
            total_level: 0,
            attack,
            decay,
            sustain,
            release,
            waveform: Waveform::Sine,
        }
    }

    #[test]
    fn ksr_rate_offset_matches_staged_csv() {
        // Conformance against the staged table itself
        // (`docs/audio/trackers/s3m/tables/opl2-ksr-rate-offset.csv`).
        // The docs tree only exists in the umbrella workspace checkout —
        // skip (without failing) when it is absent, e.g. on standalone CI.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/audio/trackers/s3m/tables/opl2-ksr-rate-offset.csv");
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("skipping: staged CSV not present at {}", path.display());
            return;
        };
        let mut lines = text.lines();
        assert_eq!(
            lines.next().map(str::trim),
            Some("key_scale_number,rks_ksr0,rks_ksr1"),
            "staged CSV layout changed"
        );
        let mut rows = 0usize;
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut cols = line.split(',');
            let n: usize = cols.next().unwrap().parse().unwrap();
            let k0: u8 = cols.next().unwrap().parse().unwrap();
            let k1: u8 = cols.next().unwrap().parse().unwrap();
            assert_eq!(KSR_RATE_OFFSET[0][n], k0, "KSR=0 mismatch at N={n}");
            assert_eq!(KSR_RATE_OFFSET[1][n], k1, "KSR=1 mismatch at N={n}");
            rows += 1;
        }
        assert_eq!(rows, 16, "staged CSV must carry 16 key-scale rows");
    }

    #[test]
    fn ksr_rate_offset_matches_closed_form() {
        // The staged record's validation: Rks = KSR ? N : N >> 2.
        for n in 0..16u8 {
            assert_eq!(key_scale_offset(false, n), n >> 2);
            assert_eq!(key_scale_offset(true, n), n);
        }
    }

    #[test]
    fn effective_rate_is_4r_plus_rks_with_r0_special_case() {
        // R == 0 => RATE = 0 regardless of key scaling.
        assert_eq!(effective_rate(0, false, 0), 0);
        assert_eq!(effective_rate(0, true, 15), 0);
        // Plain 4*R when the offset is 0.
        assert_eq!(effective_rate(1, false, 0), 4);
        assert_eq!(effective_rate(15, false, 3), 60);
        // KSR = 0: offset N >> 2; KSR = 1: offset N. Same nibble, same
        // note — the KSR bit makes the rate 4x more pitch-sensitive.
        assert_eq!(effective_rate(3, false, 13), 12 + 3);
        assert_eq!(effective_rate(3, true, 13), 12 + 13);
        // Monotone (non-decreasing) in the key-scale number.
        for &ksr in &[false, true] {
            let mut prev = 0;
            for n in 0..16 {
                let r = effective_rate(5, ksr, n);
                assert!(r >= prev, "RATE not monotone at N={n}, KSR={ksr}");
                prev = r;
            }
        }
    }

    #[test]
    fn effective_rate_saturates_at_63() {
        // The staged record: 4*15 + 15 = 75 overflows the 0..=63 RM-RL
        // domain of the vendor time table; a player must clamp.
        assert_eq!(effective_rate(15, true, 15), 63);
        assert_eq!(effective_rate(15, true, 12), 63);
        // Just inside the domain: 60 + 3 = 63.
        assert_eq!(effective_rate(15, true, 3), 63);
        assert_eq!(effective_rate(14, true, 7), 63);
        assert_eq!(effective_rate(14, true, 6), 62);
    }

    #[test]
    fn traversal_time_halves_every_plus_4_rate() {
        // The staged doc's exact halving law: RM+1 halves every time.
        for base in [
            ATTACK_FULL_SCALE_MS_AT_RATE_4,
            DECAY_FULL_SCALE_MS_AT_RATE_4,
        ] {
            for rate in 4..=59u8 {
                let t = full_scale_traversal_ms(base, rate);
                let t4 = full_scale_traversal_ms(base, rate + 4);
                assert!(
                    (t / 2.0 - t4).abs() < 1e-9 * t,
                    "halving law broken at RATE {rate}"
                );
            }
        }
        // And the two quoted RM=2 samples come out exactly.
        assert!((full_scale_traversal_ms(DECAY_FULL_SCALE_MS_AT_RATE_4, 8) - 4106.24).abs() < 1e-9);
        assert!(
            (full_scale_traversal_ms(ATTACK_FULL_SCALE_MS_AT_RATE_4, 8) - 1413.12).abs() < 1e-9
        );
    }

    /// Count envelope steps until `pred` becomes true (bounded).
    fn steps_until(env: &mut Envelope, limit: usize, pred: impl Fn(&Envelope) -> bool) -> usize {
        for i in 0..limit {
            if pred(env) {
                return i;
            }
            env.step();
        }
        panic!("predicate not reached within {limit} envelope steps");
    }

    const FS: u32 = 44_100;

    #[test]
    fn attack_traversal_matches_staged_anchor() {
        // AR nibble 1, KSR=0, N=0 => RATE 4 => 2826.24 ms full scale.
        let mut env = Envelope::new(&test_op(1, 15, 0, 15, false, true), 0, FS);
        let n = steps_until(&mut env, 200_000, |e| e.stage() == EgStage::Decay);
        let expected = ATTACK_FULL_SCALE_MS_AT_RATE_4 / 1000.0 * FS as f64;
        assert!(
            (n as f64 - expected).abs() <= 2.0,
            "attack took {n} samples, expected ~{expected:.1}"
        );
    }

    #[test]
    fn attack_rate_8_takes_half_the_time() {
        // AR nibble 2 => RATE 8 => the doc's quoted 1413.12 ms.
        let mut env = Envelope::new(&test_op(2, 15, 0, 15, false, true), 0, FS);
        let n = steps_until(&mut env, 200_000, |e| e.stage() == EgStage::Decay);
        let expected = 1413.12 / 1000.0 * FS as f64;
        assert!(
            (n as f64 - expected).abs() <= 2.0,
            "attack took {n} samples, expected ~{expected:.1}"
        );
    }

    #[test]
    fn release_traversal_matches_staged_decay_anchor() {
        // Fast attack straight to 0 dB, then key-off: the release runs
        // the full 96 dB at the RR-derived RATE. RR nibble 1, KSR=0, N=0
        // => RATE 4 => 8212.48 ms.
        let mut env = Envelope::new(&test_op(15, 15, 0, 1, false, true), 0, FS);
        steps_until(&mut env, 100_000, |e| e.stage() == EgStage::Sustain);
        assert_eq!(env.attenuation_db(), 0.0);
        env.key_off();
        let n = steps_until(&mut env, 800_000, |e| e.is_off());
        let expected = DECAY_FULL_SCALE_MS_AT_RATE_4 / 1000.0 * FS as f64;
        assert!(
            (n as f64 - expected).abs() <= 2.0,
            "release took {n} samples, expected ~{expected:.1}"
        );
    }

    #[test]
    fn decay_slope_reaches_sustain_level_proportionally() {
        // DR nibble 1 (RATE 4); SL = 15 => 45 dB. Linear-in-dB decay
        // covers 45/96 of the full-scale time.
        let mut env = Envelope::new(&test_op(15, 1, 15, 15, false, true), 0, FS);
        steps_until(&mut env, 1_000, |e| e.stage() == EgStage::Decay);
        let n = steps_until(&mut env, 800_000, |e| e.stage() == EgStage::Sustain);
        let expected = 45.0 / EG_RANGE_DB * DECAY_FULL_SCALE_MS_AT_RATE_4 / 1000.0 * FS as f64;
        assert!(
            (n as f64 - expected).abs() <= 3.0,
            "decay took {n} samples, expected ~{expected:.1}"
        );
        assert!((env.attenuation_db() - 45.0).abs() < 0.01);
    }

    #[test]
    fn ksr_bit_speeds_the_envelope_at_high_notes() {
        // Same AR nibble 1 at key-scale number 13:
        //   KSR=0 => RATE 4 + 3 = 7;  KSR=1 => RATE 4 + 13 = 17.
        let t = |ksr: bool| {
            let mut env = Envelope::new(&test_op(1, 15, 0, 15, ksr, true), 13, FS);
            steps_until(&mut env, 200_000, |e| e.stage() == EgStage::Decay)
        };
        let slow = t(false);
        let fast = t(true);
        let expect_slow =
            full_scale_traversal_ms(ATTACK_FULL_SCALE_MS_AT_RATE_4, 7) / 1000.0 * FS as f64;
        let expect_fast =
            full_scale_traversal_ms(ATTACK_FULL_SCALE_MS_AT_RATE_4, 17) / 1000.0 * FS as f64;
        assert!(
            (slow as f64 - expect_slow).abs() <= 2.0,
            "KSR=0 took {slow}"
        );
        assert!(
            (fast as f64 - expect_fast).abs() <= 2.0,
            "KSR=1 took {fast}"
        );
        assert!(fast < slow, "KSR must speed the envelope up");
    }

    #[test]
    fn rate_zero_freezes_the_envelope() {
        // AR nibble 0 => RATE 0 => "no change in the envelope": the
        // attack never leaves the 96 dB floor — the voice stays silent.
        let mut env = Envelope::new(&test_op(0, 15, 0, 15, false, true), 0, FS);
        for _ in 0..50_000 {
            env.step();
        }
        assert_eq!(env.stage(), EgStage::Attack);
        assert_eq!(env.attenuation_db(), EG_RANGE_DB);

        // DR nibble 0 with a nonzero sustain level: frozen at 0 dB after
        // the attack, never reaching the sustain level.
        let mut env = Envelope::new(&test_op(15, 0, 15, 15, false, true), 0, FS);
        steps_until(&mut env, 1_000, |e| e.stage() == EgStage::Decay);
        for _ in 0..50_000 {
            env.step();
        }
        assert_eq!(env.stage(), EgStage::Decay);
        assert_eq!(env.attenuation_db(), 0.0);
        // Key-off still releases it.
        env.key_off();
        assert!(env.stage() == EgStage::Release);
    }

    #[test]
    fn percussive_envelope_releases_through_the_sustain_level() {
        // EG-TYP = 0 (percussive): on reaching the sustain level the
        // envelope keeps decaying at the release rate without a key-off.
        let mut env = Envelope::new(&test_op(15, 15, 4, 15, false, false), 0, FS);
        let n = steps_until(&mut env, 400_000, |e| e.is_off());
        assert!(n > 0);
        // A sustained twin with the same nibbles holds instead.
        let mut held = Envelope::new(&test_op(15, 15, 4, 15, false, true), 0, FS);
        steps_until(&mut held, 400_000, |e| e.stage() == EgStage::Sustain);
        for _ in 0..10_000 {
            held.step();
        }
        assert_eq!(held.stage(), EgStage::Sustain);
        assert!((held.attenuation_db() - 4.0 * SUSTAIN_DB_PER_STEP as f64).abs() < 0.01);
    }

    #[test]
    fn retrigger_attacks_from_current_attenuation() {
        let mut env = Envelope::new(&test_op(1, 15, 0, 2, false, true), 0, FS);
        // Part-way through the attack, note where we are…
        for _ in 0..10_000 {
            env.step();
        }
        let mid = env.attenuation_db();
        assert!(mid > 0.0 && mid < EG_RANGE_DB);
        // …release a while, then re-key: attack resumes from the current
        // attenuation, not from the floor.
        env.key_off();
        for _ in 0..1_000 {
            env.step();
        }
        let released = env.attenuation_db();
        env.key_on();
        assert_eq!(env.stage(), EgStage::Attack);
        assert_eq!(env.attenuation_db(), released);
        env.step();
        assert!(env.attenuation_db() < released);
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
