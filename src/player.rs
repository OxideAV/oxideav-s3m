//! ST3 (S3M) playback engine.
//!
//! Per-channel state is kept in `Channel`. The mixer runs at a fixed
//! output sample rate (44.1 kHz), resampling each channel via linear
//! interpolation between adjacent sample frames, and applies per-channel
//! volume, global volume, and pan.
//!
//! Timing follows the ST3 conventions:
//! - `speed` = ticks per row (default 6).
//! - `bpm`   = tempo (default 125).
//! - samples_per_tick = `sample_rate * 2.5 / bpm`  (same formula as MOD).
//!
//! Output frequency (in Hz) for a given note N on an instrument whose
//! C-5 speed is `c5` is:
//!     freq = c5 * 2^((N - C5) / 12)
//! with N = 12 * octave + semitone. We compute this directly as a float.

use crate::header::{S3mHeader, PATTERN_ROWS};
use crate::pattern::{Cell, Pattern};
use crate::samples::SampleBody;

pub const DEFAULT_SPEED: u8 = 6;
pub const DEFAULT_BPM: u8 = 125;

/// Peak active-volume value for ST3 PCM channels.
///
/// Per the multimedia.cx behavioural reference
/// (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
/// §Playback Notes): "Volumes actually peak at 63, and not 64. Setting
/// the volume to 64 will actually make it go to 63. However, on Adlib
/// channels, if the default volume is 64, it will use 64. Any further
/// operations on the volume will clip it to within the 0-63 range."
///
/// This crate decodes only the PCM path (Adlib FM synth is out of scope),
/// so every active-volume write — instrument default load, volume-column,
/// per-tick D/Dxy slide, retrigger modifier, tremor restore, tremolo
/// delta — saturates to 63 rather than 64. The file-header *global*
/// volume keeps the full 0..=64 range (it is not a per-channel active
/// volume; multimedia.cx §Header: "Global volume (range 0 &lt;= x &lt;=
/// 64)").
pub const PCM_VOLUME_PEAK: u8 = 63;

/// Cap a candidate active-volume value to [`PCM_VOLUME_PEAK`].
///
/// Accepts a `u16` so callers that compute `current + delta` can hand the
/// pre-truncated sum directly; the helper takes care of the cast back to
/// `u8`. Used at every PCM volume-write site to enforce the "peak at 63"
/// rule from the multimedia.cx behavioural reference (see
/// [`PCM_VOLUME_PEAK`]).
#[inline]
pub fn clamp_pcm_volume(v: u16) -> u8 {
    v.min(PCM_VOLUME_PEAK as u16) as u8
}

/// Command letters from the ST3 spec.
/// Stored as 1..=26 in the pattern data; translating A=1, B=2, ... Z=26.
pub mod cmd {
    pub const A_SET_SPEED: u8 = 1;
    pub const B_POS_JUMP: u8 = 2;
    pub const C_PAT_BREAK: u8 = 3;
    pub const D_VOL_SLIDE: u8 = 4;
    pub const E_SLIDE_DOWN: u8 = 5;
    pub const F_SLIDE_UP: u8 = 6;
    pub const G_TONE_PORTA: u8 = 7;
    pub const H_VIBRATO: u8 = 8;
    pub const I_TREMOR: u8 = 9;
    pub const J_ARPEGGIO: u8 = 10;
    pub const K_VIB_VOL: u8 = 11;
    pub const L_PORT_VOL: u8 = 12;
    pub const O_SAMPLE_OFFSET: u8 = 15;
    pub const Q_RETRIGGER: u8 = 17;
    pub const R_TREMOLO: u8 = 18;
    pub const S_EXTENDED: u8 = 19;
    pub const T_SET_TEMPO: u8 = 20;
    pub const U_FINE_VIBRATO: u8 = 21;
    pub const V_GLOBAL_VOL: u8 = 22;
    pub const X_SET_PAN: u8 = 24;
}

/// 16-entry S2x finetune table from `ScreamTracker-v3.20-effects.txt`.
/// Index = parameter nibble x (0..=F); value is the C4Spd (=C5 speed)
/// that the running instrument should use for the rest of its lifetime
/// on this channel.
pub const S2X_FINETUNE_TABLE: [u32; 16] = [
    7895, 7941, 7985, 8046, 8107, 8169, 8232, 8280, 8363, 8413, 8463, 8529, 8581, 8651, 8723, 8757,
];

/// Qxy retrigger "×2/3" volume table (x == 6).
///
/// The multimedia.cx ST3 behavioural reference (§Qxy) notes the `*2/3`
/// volume modifier is *not* exactly `vol * 2 / 3`; ST3 uses this 64-entry
/// lookup table indexed by the channel's current (active) volume 0..=63.
/// Transcribed verbatim from the `TwoThirds: array [0..63] of Byte` listing
/// in `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`.
pub const Q_TWO_THIRDS: [u8; 64] = [
    0, 0, 1, 1, 2, 3, 3, 4, 5, 5, 6, 6, 7, 8, 8, 9, 10, 10, 11, 11, 12, 13, 13, 14, 15, 15, 16, 16,
    17, 18, 18, 19, 20, 20, 21, 21, 22, 23, 23, 24, 25, 25, 26, 26, 27, 28, 28, 29, 30, 30, 31, 31,
    32, 33, 33, 34, 35, 35, 36, 36, 37, 38, 38, 39,
];

/// Apply the Qxy volume modifier `x` to a current active volume `vol`,
/// returning the new clamped-to-[`PCM_VOLUME_PEAK`] volume.
///
/// Table from `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
/// §Qxy ("Values for x"). `x == 0` and `x == 8` are documented no-ops
/// ("0" and "?" respectively). `x == 6` uses the [`Q_TWO_THIRDS`] table
/// rather than an exact `vol*2/3`.
///
/// The add / multiply legs cap at [`PCM_VOLUME_PEAK`] (= 63) rather than
/// 64 per the multimedia.cx §Playback Notes "volumes peak at 63" rule —
/// see [`PCM_VOLUME_PEAK`].
fn retrigger_volume(vol: u8, x: u8) -> u8 {
    let v = vol.min(PCM_VOLUME_PEAK);
    match x {
        0x1 => v.saturating_sub(1),
        0x2 => v.saturating_sub(2),
        0x3 => v.saturating_sub(4),
        0x4 => v.saturating_sub(8),
        0x5 => v.saturating_sub(16),
        0x6 => Q_TWO_THIRDS[v as usize],
        0x7 => v / 2,
        0x9 => clamp_pcm_volume(v as u16 + 1),
        0xA => clamp_pcm_volume(v as u16 + 2),
        0xB => clamp_pcm_volume(v as u16 + 4),
        0xC => clamp_pcm_volume(v as u16 + 8),
        0xD => clamp_pcm_volume(v as u16 + 16),
        0xE => clamp_pcm_volume((v as u16) * 3 / 2),
        0xF => clamp_pcm_volume(v as u16 * 2),
        // 0x0 (no slide) and 0x8 ("?") are no-ops.
        _ => v,
    }
}

/// Map an ST3 command number to its per-channel effect-memory slot.
///
/// H and U share a slot (multimedia.cx wiki: "Uxy shares memory with Hxy").
/// All Sxy subcommands collapse to a single S slot — the next row's S
/// command sees the latest nonzero `info` byte even across S-subcommand
/// boundaries.
/// Is this an SCx-freeze "resume" command?
///
/// Per the multimedia.cx behavioural reference (§SCx): "When the note is
/// cut, the volume is not set to 0. Instead playback is temporarily
/// frozen and may be resumed by a following Exx, Fxx, Gxx, Hxx, Jxx,
/// Kxx, Lxx or Uxx command." A row carrying any of those commands on a
/// frozen channel thaws it before the row's tick-0 effects fire.
fn is_scx_resume_command(command: u8) -> bool {
    matches!(
        command,
        cmd::E_SLIDE_DOWN
            | cmd::F_SLIDE_UP
            | cmd::G_TONE_PORTA
            | cmd::H_VIBRATO
            | cmd::J_ARPEGGIO
            | cmd::K_VIB_VOL
            | cmd::L_PORT_VOL
            | cmd::U_FINE_VIBRATO
    )
}

fn effect_memory_slot(command: u8) -> u8 {
    match command {
        // U → H slot.
        cmd::U_FINE_VIBRATO => cmd::H_VIBRATO,
        _ => command,
    }
}

/// Whether an effect participates in the per-channel "%"/"*" parameter
/// memory. Per the multimedia.cx effect list
/// (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
/// §Effects), only the effects marked `%` or `*` recall the latest
/// nonzero parameter; Axx / Bxx / Cxy / Txx / Vxx carry no marker and
/// take their infobyte literally every row — `A00` / `T00` are simply
/// ignored per their own rules, `B00` jumps to order 0, `C00` breaks to
/// row 0, and `V00` sets the global volume to 0.
fn effect_uses_memory(command: u8) -> bool {
    !matches!(
        command,
        cmd::A_SET_SPEED
            | cmd::B_POS_JUMP
            | cmd::C_PAT_BREAK
            | cmd::T_SET_TEMPO
            | cmd::V_GLOBAL_VOL
    )
}

/// Vibrato / tremolo waveform selector (S3x / S4x parameter low nibble).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Waveform {
    /// Standard 64-entry approximated sine — ST3 default.
    #[default]
    Sine,
    /// Linear ramp from +max down to -max over the period.
    RampDown,
    /// +max / -max square (no in-between values).
    Square,
    /// Waveform selector 3 ("random"). Per FireLight §6.8 / §6.15
    /// (`docs/audio/trackers/s3m/FireLight-S3M-Player-Tutorial.txt`,
    /// `case 3: delta = sintab[temp]; // random - just use sine.`), real
    /// Scream Tracker 3 never implemented an actual noise LFO for waveform
    /// 3 — it falls through to the sine table. We reproduce that behaviour
    /// so a module carrying an `S33` / `S43` waveform-select plays the same
    /// pitch / volume modulation ST3 itself produces, rather than audible
    /// noise.
    Random,
}

impl Waveform {
    fn from_nibble(x: u8) -> Self {
        match x & 0x03 {
            0 => Waveform::Sine,
            1 => Waveform::RampDown,
            2 => Waveform::Square,
            _ => Waveform::Random,
        }
    }
}

/// Per-channel playback state.
#[derive(Clone, Debug)]
pub struct Channel {
    /// 1-based instrument index (0 = nothing triggered yet).
    pub instrument: u8,
    /// Current playback frequency in Hz (0 = silent).
    pub frequency: f32,
    /// Fractional read cursor into the sample body.
    pub sample_pos: f64,
    /// Current per-channel active volume.
    ///
    /// Range is `0..=`[`PCM_VOLUME_PEAK`] (i.e. `0..=63`) per the
    /// multimedia.cx behavioural reference: "Volumes actually peak at 63,
    /// and not 64. Setting the volume to 64 will actually make it go to
    /// 63." All effect-side writes funnel through
    /// [`clamp_pcm_volume`], and the mixer reads it back through
    /// `volume.min(PCM_VOLUME_PEAK)` so externally-constructed `Channel`
    /// literals can't produce a gain above the documented ceiling either.
    pub volume: u8,
    /// Pan value 0..=15 (0 = hard left, 15 = hard right).
    pub pan: u8,
    /// Whether this channel is currently emitting sound.
    pub active: bool,
    /// Remembered note for tone-portamento (G) target tracking.
    pub target_frequency: f32,
    /// Current effect command 1..=26 (0 = none).
    pub command: u8,
    /// Effect parameter byte.
    pub info: u8,
    /// Vibrato phase as the full 0..=255 cycle position. Per the
    /// multimedia.cx §Playback Notes ("Vibrato and tremolo have a full
    /// cycle length of 256, though Hxy and Rxy use x*4 and y*4 as their
    /// parameters"), `Hxy`/`Uxy`/`Kxy` step this by `speed * 4` per
    /// nonzero tick and sample the 64-entry waveform table at `phase / 4`.
    /// That `phase / 4` is exactly the FireLight §6.8 signed `-32..+31`
    /// vibrato pointer incremented by `speed` — the two descriptions are
    /// the same phase model, with this field holding the finer 8-bit
    /// counter so fine vibrato (`Uxy`) shares the same stepping.
    pub vibrato_pos: u8,
    /// Tremolo phase as the full 0..=255 cycle position. Per the
    /// multimedia.cx §Playback Notes ("Vibrato and tremolo have a full
    /// cycle length of 256, though Hxy and Rxy use x*4 and y*4 as their
    /// parameters"), `Rxy` steps this by `speed * 4` per nonzero tick and
    /// samples the 64-entry waveform table at `phase / 4`.
    pub tremolo_pos: u8,
    /// Last note byte triggered on this channel — needed for arpeggio
    /// and retrigger to recompute the base frequency.
    pub last_note: u8,
    /// SDx (note delay): pending trigger buffered at tick 0 for firing at
    /// tick `x`. `None` when no delay is active.
    pub pending_delay: Option<PendingTrigger>,
    /// S1x: glissando control. When set, G (tone portamento) snaps to the
    /// nearest semitone of the target rather than gliding continuously.
    pub glissando: bool,
    /// S3x: vibrato waveform for H / U / K vibrato terms.
    pub vibrato_waveform: Waveform,
    /// S4x: tremolo waveform for R volume vibrato.
    pub tremolo_waveform: Waveform,
    /// Qxy retrigger tick counter. Incremented on every tick (including
    /// tick 0); when it reaches/exceeds the retrig value `y` the sample is
    /// retriggered and the counter resets to 0. Per the multimedia.cx
    /// behavioural reference (§Qxy) this counter is *global to the channel
    /// across rows* — a new note with Qxy does NOT reset it; only a row
    /// without the Qxy effect (or song start) clears it back to 0.
    pub retrig_counter: u8,
    /// Ixy tremor "on" counter. Per the multimedia.cx behavioural
    /// reference (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
    /// §Ixy): "Implemented with two decrementing counters per channel —
    /// the 'on' counter and the 'off' counter. On each tick, if the 'on'
    /// counter is greater than zero, it is decremented and if it reaches
    /// zero, the current volume is set to 0 and the 'off' counter is set
    /// to the 'off' time (y + 1). If the 'on' counter was zero in the
    /// beginning of the update procedure, then the 'off' counter is
    /// decremented and if it reached zero (or became less than zero),
    /// the current volume is set to the stored volume and the 'on'
    /// counter is set to the 'on' time (x + 1)." The counters are
    /// "**never reset**, except in the tremor update procedure described
    /// above. Scream Tracker doesn't even reset them on playback start.
    /// Only on tracker startup are they reset." — so this field
    /// persists across rows without an Ixy command.
    pub tremor_on_counter: u8,
    /// Ixy tremor "off" counter — see [`Channel::tremor_on_counter`].
    pub tremor_off_counter: u8,
    /// Channel's "stored" volume — the value most recently written by a
    /// stored-volume source (instrument default load, explicit volume
    /// column, or the SDx-deferred equivalents). Per the multimedia.cx
    /// behavioural reference §Ixy / §Rxy, effects that modulate the
    /// *active* volume — Dxy slides, Qxy retrigger modifier, Rxy
    /// tremolo delta, Ixy tremor — must not touch this value. Ixy uses
    /// it as the restore target on an on-phase transition, matching
    /// "the current volume is set to the stored volume" from §Ixy.
    pub stored_volume: u8,
    /// When `true`, a new note must not reset `vibrato_pos` / `tremolo_pos`.
    /// Set by `S3x`/`S4x` when bit 2 of the parameter is high (e.g. `S34`,
    /// `S3E` per the multimedia.cx clean-room behavioural reference).
    pub keep_vibrato_pos_on_new_note: bool,
    /// Same as the above, but for the tremolo (`S4x`) waveform.
    pub keep_tremolo_pos_on_new_note: bool,
    /// Effect-memory store, indexed by ST3 command 0..=26. The multimedia.cx
    /// clean-room behavioural reference labels nearly every parameter-bearing
    /// effect with "%" (use the latest nonzero parameter to show up). When a
    /// row's command is the same as one already seen on this channel and the
    /// infobyte is zero, ST3 reuses the stored value. `H` / `U` (vibrato /
    /// fine vibrato) share memory, as does `O` (sample offset).
    pub effect_memory: [u8; 27],
    /// Channel mute flag (`+128` in the file header's channel-settings
    /// byte). When set, the mixer skips this channel and emits silence on
    /// its slot; pattern parsing and per-tick state updates still run so
    /// jump / loop / delay state stays consistent.
    pub muted: bool,
    /// SCx "frozen" state. Per the multimedia.cx behavioural reference
    /// (§SCx): "the volume is *not* set to 0. Instead playback is
    /// temporarily *frozen* and may be *resumed by a following Exx, Fxx,
    /// Gxx, Hxx, Jxx, Kxx, Lxx or Uxx command*." When `true`, the mixer
    /// emits silence for the channel and the sample read cursor halts,
    /// but the channel's volume / frequency / sample_pos are preserved so
    /// a thawing E/F/G/H/J/K/L/U command (or a fresh note trigger) can
    /// resume playback from where it stopped.
    pub frozen: bool,
    /// File-defined stereo side of this channel: `false` = left, `true` =
    /// right. Derived from the low nibble of the channel-settings byte
    /// (slots 0..=7 are the left bank `L1..L8`, slots 8..=15 the right
    /// bank `R1..R8`). Needed by the legacy `SAx` ("old stereo control")
    /// effect, whose normal/reversed/center mapping is keyed on the
    /// channel's bank per the multimedia.cx behavioural reference
    /// (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
    /// §SAx: "This effect is dependent on what channel type you put it
    /// on ... SA0/SA2: Normal panning (L is left, R is right) ...").
    pub right_bank: bool,
    /// Global volume *latched into this voice* at the moment its note
    /// volume was last (re)written. Per the multimedia.cx behavioural
    /// reference §Vxx: "It does not affect past notes, that are still
    /// playing, unless their volume is changed, which applies the new
    /// global volume to that voice." A Vxx change updates the player's
    /// *current* global volume but does NOT retroactively touch this
    /// field; only a note trigger or a volume-modifying effect (volume
    /// column, Dxy/Kxy/Lxy slide, Qxy retrigger modifier, Rxy tremolo,
    /// Ixy tremor) re-latches it to the value live at the time of that
    /// write. The mixer scales by this per-voice value, not the live
    /// player global volume, so a Vxx between two notes only affects the
    /// note(s) whose volume changed after it. Range `0..=64`.
    pub voice_global_vol: u8,
}

/// Note/instrument/volume stash for the SDx (note delay) effect.
#[derive(Clone, Copy, Debug, Default)]
pub struct PendingTrigger {
    /// Tick on which to trigger.
    pub fire_tick: u8,
    /// Note byte (0xFF = none, 0xFE = cut).
    pub note: u8,
    /// 1-based instrument (0 = no change).
    pub instrument: u8,
    /// Volume 0..=64 or 0xFF = no change.
    pub volume: u8,
}

impl Default for Channel {
    fn default() -> Self {
        Channel {
            instrument: 0,
            frequency: 0.0,
            sample_pos: 0.0,
            volume: 0,
            pan: 8,
            active: false,
            target_frequency: 0.0,
            command: 0,
            info: 0,
            vibrato_pos: 0,
            tremolo_pos: 0,
            last_note: 0,
            pending_delay: None,
            glissando: false,
            vibrato_waveform: Waveform::Sine,
            tremolo_waveform: Waveform::Sine,
            retrig_counter: 0,
            tremor_on_counter: 0,
            tremor_off_counter: 0,
            stored_volume: 0,
            keep_vibrato_pos_on_new_note: false,
            keep_tremolo_pos_on_new_note: false,
            effect_memory: [0; 27],
            muted: false,
            frozen: false,
            right_bank: false,
            // Default to full scale (64/64 = unity). A freshly-constructed
            // channel that has never carried a note plays at the player's
            // global volume the first time it triggers, re-latching there.
            voice_global_vol: 64,
        }
    }
}

/// Canonical Scream Tracker 3 period table — 9 octaves × 12 semitones,
/// indexed by the linear note number `n = octave * 12 + semitone`
/// (`n = 0` is the octave-0 C; `n = 48` is the C-5 / `c2spd` reference
/// note; `n = 107` is the octave-8 B that ST3 stops at).
///
/// These are the exact integer periods from the FireLight S3M player
/// tutorial §4.2 "9 Octaves"
/// (`docs/audio/trackers/s3m/FireLight-S3M-Player-Tutorial.txt`). They
/// are *not* a literal equal-tempered table: each octave is exactly half
/// the previous (e.g. octave-4 C = 1712 ⇒ octave-5 C = 856), and the
/// within-octave ratios are ST3's own canonical values
/// (`1712,1616,1524,1440,1356,1280,1208,1140,1076,1016,960,906`), so a
/// pure `2^(n/12)` approximation drifts from real ST3 pitch by up to
/// roughly a cent near the octave's top (B = 906 vs the equal-tempered
/// 905.8; multimedia.cx's base-octave list rounds this to 907, but the
/// FireLight table's 906 is the value that keeps every octave an exact
/// halving and is what the playback path uses).
pub const PERIOD_TABLE: [u32; 108] = [
    // octave 0
    27392, 25856, 24384, 23040, 21696, 20480, 19328, 18240, 17216, 16256, 15360, 14496,
    // octave 1
    13696, 12928, 12192, 11520, 10848, 10240, 9664, 9120, 8608, 8128, 7680, 7248,
    // octave 2
    6848, 6464, 6096, 5760, 5424, 5120, 4832, 4560, 4304, 4064, 3840, 3624, // octave 3
    3424, 3232, 3048, 2880, 2712, 2560, 2416, 2280, 2152, 2032, 1920, 1812, // octave 4
    1712, 1616, 1524, 1440, 1356, 1280, 1208, 1140, 1076, 1016, 960, 906, // octave 5
    856, 808, 762, 720, 678, 640, 604, 570, 538, 508, 480, 453, // octave 6
    428, 404, 381, 360, 339, 320, 302, 285, 269, 254, 240, 226, // octave 7
    214, 202, 190, 180, 170, 160, 151, 143, 135, 127, 120, 113, // octave 8
    107, 101, 95, 90, 85, 80, 75, 71, 67, 63, 60, 56,
];

/// The C-5 reference note index (`octave 4`, semitone C) into
/// [`PERIOD_TABLE`]. Note byte `0x40` maps here, and the sample's
/// `c2spd` ("C-2 speed" — historically misnamed, it is the playback rate
/// for *this* note) is the frequency at this index.
pub const C5_NOTE_INDEX: i32 = 48;

/// Resolve a linear note index `n` (octave * 12 + semitone) plus a
/// sample's `c2spd` into a playback frequency in Hz, via the canonical
/// [`PERIOD_TABLE`].
///
/// Per the FireLight tutorial §4.1 the runtime period is
/// `period = 8363 * PERIOD_TABLE[n] / c2spd`, and the frequency is
/// `clock / period`. Substituting back, the C-5 reference note
/// (`PERIOD_TABLE[48] == 1712`) plays at exactly `c2spd` because
/// [`AMIGA_CLOCK_HZ`] `== 8363 * 1712`; FireLight's `14317056`
/// constant in the same section is the documented base-clock typo
/// (multimedia.cx §Playback Notes: "documentation incorrectly says
/// 14317056 Hz, correct value is the expected 8363 * 1712").
///
/// Indices below 0 or above the table's last entry (`n > 107`, e.g. an
/// arpeggio that adds semitones past the octave-8 ceiling) are clamped
/// to the table bounds — ST3 stops at B-8 and has no period to look up
/// beyond it.
fn note_index_to_frequency(n: i32, c2spd: u32) -> f32 {
    let idx = n.clamp(0, PERIOD_TABLE.len() as i32 - 1) as usize;
    let period = 8363u64 * PERIOD_TABLE[idx] as u64 / (c2spd.max(1) as u64);
    if period == 0 {
        return 0.0;
    }
    (AMIGA_CLOCK_HZ as f64 / period as f64) as f32
}

/// Convert an S3M note byte (octave << 4 | semitone) and C5 speed (Hz)
/// into a playback frequency through the canonical [`PERIOD_TABLE`].
///
/// ST3's note numbering displays octave 0 as "C-1", so the field the
/// header calls "C-5 speed" is actually the playback rate for note byte
/// **0x40** (octave-nibble 4, what ST3's UI labels as C-5). One octave
/// up from that is byte 0x50, two octaves up is byte 0x60, and so on.
/// Confused this for byte 0x50 once; everything played an octave low.
fn note_to_frequency(note: u8, c5_speed: u32) -> f32 {
    let octave = (note >> 4) as i32;
    let semitone = (note & 0x0F) as i32;
    let n = octave * 12 + semitone;
    note_index_to_frequency(n, c5_speed)
}

/// ProTracker half-sine table, transcribed verbatim from the FireLight
/// S3M Player Tutorial §6.8 ("This is the sine table used by Protracker.
/// If a player calls itself fully protracker compatible, it really should
/// be using this table.") in
/// `docs/audio/trackers/s3m/FireLight-S3M-Player-Tutorial.txt`.
///
/// The table is *half* a sine wave (one all-positive hump, peaking at 255
/// at index 16). ST3 walks a signed pointer (-32..+31, 64 distinct
/// positions); the low 5 bits index this 32-entry table and the sign bit
/// (pointer >= 0 vs < 0) decides whether the magnitude raises or lowers
/// the pitch — together yielding one full oscillation. Using the literal
/// table instead of a computed `sin()` matches the exact integer
/// modulation values ST3 produces (the tutorial calls its routine "100%
/// accurate").
pub const PROTRACKER_SINE: [u8; 32] = [
    0, 24, 49, 74, 97, 120, 141, 161, 180, 197, 212, 224, 235, 244, 250, 253, 255, 253, 250, 244,
    235, 224, 212, 197, 180, 161, 141, 120, 97, 74, 49, 24,
];

/// Sample the selected vibrato/tremolo waveform at table position `pos`
/// (0..=63), returning a signed integer in the range -64..=64.
///
/// `pos` is the unsigned form of ST3's signed `-32..+31` waveform pointer:
/// positions `0..=31` are the "positive" half (the modulation is added),
/// positions `32..=63` are the "negative" half (subtracted). The low five
/// bits (`pos & 31`) index the 32-entry waveform shape per the FireLight
/// tutorial §6.8 example routine.
///
/// The native shapes run 0..=255 (the documented `sintab` / ramp / square
/// magnitudes); we scale by `/4` (255/4 ≈ 63.75) so the result lands in
/// the ±64 range the depth math downstream already assumes, while the
/// *shape* now tracks ST3's integer table exactly rather than an idealized
/// floating-point sine.
///
/// Waveform selector 3 (`Random`) is *not* a noise generator: per FireLight
/// §6.8 / §6.15 (`case 3: delta = sintab[temp];`), Scream Tracker 3 reuses
/// the sine table for it, so this function treats `Random` identically to
/// `Sine`.
fn waveform_sample(wf: Waveform, pos: u8) -> i32 {
    // Sign: positive half for the low 32 positions, negative half above.
    let negative = pos & 0x20 != 0;
    let idx = (pos & 0x1F) as usize;
    let magnitude: i32 = match wf {
        // Direct table lookup per FireLight §6.8 `case 0: delta = sintab[temp]`.
        // Waveform 3 ("random") falls through to the same sine table per
        // FireLight §6.8 `case 3: delta = sintab[temp]; // random - just use
        // sine.` — ST3 never shipped an actual noise LFO.
        Waveform::Sine | Waveform::Random => PROTRACKER_SINE[idx] as i32,
        // Ramp down per FireLight §6.8 `case 1: temp <<= 3; if(vibpos<0)
        // temp = 255 - temp; delta = temp`. Within each half the magnitude
        // ramps `idx*8` (0..=248); the negative half mirrors it to 255-that.
        Waveform::RampDown => {
            let t = (idx as i32) << 3;
            if negative {
                255 - t
            } else {
                t
            }
        }
        // Square per FireLight §6.8 `case 2: delta = 255` — a constant
        // magnitude whose sign is supplied by the half-cycle below.
        Waveform::Square => 255,
    };
    // Scale the 0..=255 native magnitude into the ±64 range used by the
    // depth math, then apply the half-cycle sign.
    let scaled = (magnitude / 4).min(64);
    if negative {
        -scaled
    } else {
        scaled
    }
}

/// Quantise an arbitrary frequency to the nearest equal-tempered
/// semitone relative to `c5_speed`. Used when S1x glissando control is
/// enabled — tone portamento then slides note-by-note instead of
/// continuously.
fn snap_to_semitone(freq: f32, c5_speed: u32) -> f32 {
    if freq <= 0.0 {
        return freq;
    }
    let c5 = (c5_speed.max(1)) as f32;
    let semis = (freq / c5).log2() * 12.0;
    let n = semis.round();
    c5 * 2.0f32.powf(n / 12.0)
}

/// Resolve an `Oxy` sample-offset byte (in samples) against a sample's
/// loop metadata.
///
/// Per the Scream Tracker 3.20 effects listing
/// (`docs/audio/trackers/s3m/ScreamTracker-v3.20-effects.txt` §Oxy):
///
/// > If the sample offset is used in a looped sample and the offset given
/// > exceeds the loop end value, the loop is taken into consideration
/// > and the offset will be calculated as if the sample had looped.
///
/// In other words: for an unlooped sample the raw offset is returned
/// (the mixer will mark the channel inactive on its own when the read
/// cursor walks past `len`). For a looped sample whose `[loop_start,
/// loop_end)` window is well-formed and where the requested offset has
/// walked past `loop_end`, fold the overage back through the loop
/// length so the channel starts inside the loop window.
fn resolve_sample_offset(
    offset_samples: u64,
    loop_start: u32,
    loop_end: u32,
    pcm_len: usize,
    looped: bool,
) -> f64 {
    let off = offset_samples as f64;
    if !looped {
        return off;
    }
    let ls = loop_start as f64;
    let le = loop_end as f64;
    // Defensive: a malformed loop window (end <= start, or end past pcm)
    // can't be folded — fall back to the raw offset and let the mixer's
    // bounds check handle it.
    if le <= ls || le as usize > pcm_len {
        return off;
    }
    if off < le {
        return off;
    }
    let span = le - ls;
    ls + (off - ls).rem_euclid(span)
}

/// Lower / upper period bound for the header's "Amiga limits" flag
/// (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html` §Flags
/// bit 4: "Amiga limits (limit periods to confine to 113 <= x <= 856)").
///
/// The PAL Amiga clock (14_317_456 Hz, the project-standard 8363 * 1712
/// constant called out in the same multimedia.cx file under "Playback
/// Notes / Base clock") relates period (clock cycles per sample) to
/// playback frequency (Hz) by `freq = AMIGA_CLOCK / period`. So a period
/// range of `[113, 856]` translates to a frequency range of roughly
/// `[16_725, 126_703]` Hz. The constants below carry the spec's
/// integer period bounds; the player converts them on demand so the
/// truncation matches whatever the active sample rate works at.
pub const AMIGA_CLOCK_HZ: u32 = 14_317_456;
pub const AMIGA_LIMIT_PERIOD_MIN: u32 = 113;
pub const AMIGA_LIMIT_PERIOD_MAX: u32 = 856;

/// Top-level player state.
pub struct PlayerState {
    pub samples: Vec<SampleBody>,
    pub patterns: Vec<Pattern>,
    pub order: Vec<u8>,

    pub channels: Vec<Channel>,
    /// Initial pan values copied from the header (0..=15).
    pub initial_pan: Vec<u8>,

    pub speed: u8,
    pub bpm: u8,
    pub global_volume: u8,
    /// Master volume from the file header. Per the multimedia.cx behavioural
    /// reference (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
    /// §Mixing volume): "Mixing volume (range 16 <= x <= 127) which is only
    /// used for Sound Blaster. It is multiplied by 11/8 when stereo is on."
    /// The constructor clamps to `[16, 127]` so out-of-range values from
    /// hand-edited headers don't pull the mixer below the nominal floor or
    /// above the documented ceiling. Applied as a global gain on top of
    /// `global_volume`; the `* 11/8` stereo multiplier lives in the mixer
    /// step rather than the stored value so the unprocessed file-header
    /// number remains inspectable for round-trip tooling.
    pub master_volume: u8,
    /// Stereo flag mirrored from the file header (bit 7 of the raw master-
    /// volume byte). Drives the multimedia.cx `* 11/8` mixing-volume
    /// multiplier in the mixer step: stereo modules get a documented +2.78 dB
    /// boost over mono modules at the same numerical master-volume setting,
    /// matching what ST3 itself did on Sound Blaster output.
    pub stereo: bool,
    /// Number of channels actually carrying PCM/AdLib in the file.
    /// Used as the mixer's normalisation divisor — dividing by all 32
    /// slots makes typical 4–8 channel modules far too quiet.
    pub active_channels: u8,

    /// Header flag bit 6 ("ST3.00 volume slides" — fast slides). Per
    /// `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
    /// §Flags: "if enabled, *all* volume slides occur *every* tick"
    /// and the bit is "automatically enabled if tracker version is
    /// == 0x1300" (i.e. CwtV == 0x1300, the original ST3.00). The
    /// Dxy per-tick path consults this when deciding whether the
    /// `D0x` / `Dx0` continuous slides also fire at tick 0; fine
    /// slides (`DFx` / `DxF` / `DFF`) are explicitly unaffected
    /// (same source: "unless we're doing a fineslide, we slide on
    /// all ticks").
    pub fast_slides: bool,
    /// Header flag bit 4 ("Amiga limits"). Per the same source: "limit
    /// periods to confine to 113 <= x <= 856". When set, the mixer
    /// clamps every channel's playback frequency to
    /// `[AMIGA_CLOCK_HZ / 856, AMIGA_CLOCK_HZ / 113]` Hz so that the
    /// effective period never escapes the PAL Amiga's hardware range.
    pub amiga_limits: bool,

    pub order_index: u8,
    pub row: u8,
    pub tick: u8,
    pub tick_sample_cursor: u32,

    pub sample_rate: u32,
    pub ended: bool,

    pending_jump: Option<Jump>,
    /// SBx (pattern loop) state. The loop start row is set by SB0; a
    /// subsequent SBx with x>0 loops back `x` times. ST3 keeps a single
    /// loop state per pattern (globally, not per-channel).
    loop_start_row: u8,
    loop_count: Option<u8>,
    /// The order-table playback position (`order_index`) whose pattern the
    /// SBx loop state currently belongs to. Pattern-loop is **per-pattern**:
    /// "you should not try looping back to a loop point in another pattern!
    /// ... If you don't enter an SB0 command, the loop defaults to the
    /// beginning of the pattern"
    /// (`docs/audio/trackers/s3m/ScreamTracker-v3.20-effects.txt` §SBx). When
    /// playback advances to a different order slot, the loop start row resets
    /// to 0 (the top of the new pattern) and any in-flight loop counter is
    /// dropped, so a stale loop point can never bleed across the boundary.
    /// `None` until the first row is entered.
    loop_pattern_order: Option<u8>,
    /// SEx pattern delay: when non-zero we hold the current row, replaying
    /// all per-tick effects but not re-triggering notes. Decremented at
    /// the end of each row-cycle until zero.
    pattern_delay_remaining: u8,
    /// `true` when `next_row` consumed an SEx replay and the *next*
    /// `enter_row` call should skip cell application. Reset every time
    /// the row is fully advanced.
    replaying_for_pattern_delay: bool,
    /// Vxx (set global volume) is "actually processed on tick 1 (that is the
    /// second tick) of the row" per
    /// `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html` §Vxx.
    /// Tick 0 stashes the validated value here; tick 1 drains it. If `speed`
    /// is `1`, tick 1 never fires before the row advances and the stash is
    /// cleared on the next `enter_row` — matching the spec's "doesn't do
    /// anything if the current speed is 1".
    pending_global_vol: Option<u8>,
}

#[derive(Clone, Copy, Debug)]
struct Jump {
    order: Option<u8>,
    row: u8,
}

impl PlayerState {
    pub fn new(
        header: &S3mHeader,
        samples: Vec<SampleBody>,
        patterns: Vec<Pattern>,
        sample_rate: u32,
    ) -> Self {
        let n_channels = header.channels.len();
        let channels = (0..n_channels)
            .map(|i| Channel {
                pan: header.pans.get(i).copied().unwrap_or(8) & 0x0F,
                muted: header.muted.get(i).copied().unwrap_or(false),
                // Stereo bank from the channel-settings low nibble: slots
                // 0..=7 = left (`L1..L8`), 8..=15 = right (`R1..R8`). Used
                // only by the legacy SAx effect. 0xFF (unused) slots fall
                // back to the left bank — they emit no audio anyway.
                right_bank: header
                    .channels
                    .get(i)
                    .map(|&c| c != 0xFF && (c & 0x0F) >= 8)
                    .unwrap_or(false),
                ..Channel::default()
            })
            .collect();
        let initial_pan = header.pans.to_vec();

        // Initial speed / tempo edge cases per the multimedia.cx behavioural
        // reference at `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
        // under "Initial speed" / "Initial tempo":
        //   * "Initial speed ... if 0 *or 255*, it is ignored and the previous
        //     value used when you loaded the song is used instead." We're a
        //     fresh-load player, so "the previous value" is whatever DEFAULT_SPEED
        //     stands in for.
        //   * "Initial tempo - if less than 33, it is ignored and the previous
        //     value used when you loaded the song is used instead." This mirrors
        //     the Txx command guard (`ch.info >= 0x20`) so the initial path
        //     stays consistent with what a same-value Txx on row 0 would do.
        let speed = if header.initial_speed == 0 || header.initial_speed == 0xFF {
            DEFAULT_SPEED
        } else {
            header.initial_speed
        };
        let bpm = if header.initial_tempo < 33 {
            DEFAULT_BPM
        } else {
            header.initial_tempo
        };

        // Header flag derivation per `multimedia-cx-scream-tracker-3.html`
        // §Flags ("Bit 4: Amiga limits ...", "Bit 6: ST3.00 volume slides ...
        // automatically enabled if tracker version is == 0x1300") and §Dxy
        // ("if fast slides are enabled ... or the version is <= 0x1300"). The
        // tracker-version coupling matches the documented behaviour that the
        // original ST3.00 release — and the earlier Scream Tracker builds at
        // or below CwtV 0x1300 — always ran the fast-slides path regardless of
        // the flag byte; later versions only do so when bit 6 is explicitly
        // set. The §Dxy `<= 0x1300` bound is the broader of the two forms the
        // reference states and is the one the volume-slide kernel keys off, so
        // the player consults `auto_fast_slides()` (Scream-Tracker-family-gated
        // `<= 0x1300`) rather than the strict `== 0x1300` sentinel.
        let fast_slides =
            (header.flags & (1 << 6)) != 0 || header.created_with_tracker().auto_fast_slides();
        let amiga_limits = (header.flags & (1 << 4)) != 0;

        PlayerState {
            samples,
            patterns,
            order: header.order.clone(),
            channels,
            initial_pan,
            speed,
            bpm,
            global_volume: header.global_volume.min(64),
            // Mixing-volume range is `[16, 127]` per the multimedia.cx wiki
            // §Mixing volume ("range 16 <= x <= 127"); clamp at load time so
            // hand-edited or truncated headers don't push the mixer outside
            // the spec window. Values below 16 would silence the file below
            // the documented floor; values above 127 occupy the bit-7 stereo
            // flag and are physically unreachable through the byte but the
            // explicit ceiling keeps the intent obvious.
            master_volume: header.master_volume.clamp(16, 127),
            stereo: header.stereo,
            active_channels: header.enabled_channels.max(1),
            fast_slides,
            amiga_limits,
            order_index: 0,
            row: 0,
            tick: 0,
            tick_sample_cursor: 0,
            sample_rate,
            ended: false,
            pending_jump: None,
            loop_start_row: 0,
            loop_count: None,
            loop_pattern_order: None,
            pattern_delay_remaining: 0,
            replaying_for_pattern_delay: false,
            pending_global_vol: None,
        }
    }

    /// Lower frequency bound (Hz) of the Amiga-limits clamp.
    ///
    /// Period `856` (max) → frequency `AMIGA_CLOCK_HZ / 856 ≈ 16725` Hz.
    /// Below this the period would exceed `856` clock units, which the
    /// PAL Amiga hardware refused to produce on the original units the
    /// flag was added to emulate.
    #[inline]
    pub fn amiga_limit_freq_low(&self) -> f32 {
        (AMIGA_CLOCK_HZ as f32) / (AMIGA_LIMIT_PERIOD_MAX as f32)
    }

    /// Upper frequency bound (Hz) of the Amiga-limits clamp.
    ///
    /// Period `113` (min) → frequency `AMIGA_CLOCK_HZ / 113 ≈ 126703` Hz.
    #[inline]
    pub fn amiga_limit_freq_high(&self) -> f32 {
        (AMIGA_CLOCK_HZ as f32) / (AMIGA_LIMIT_PERIOD_MIN as f32)
    }

    /// Apply the Amiga-limits clamp to a channel pair of (frequency,
    /// target_frequency). No-op when `enabled` is false.
    ///
    /// Both fields are clamped because tone portamento (G/L) and the
    /// continuous pitch slides (E/F) rebase off `target_frequency`; if
    /// only `frequency` were clamped, the next vibrato or porta step
    /// would silently push the audible pitch outside the legal range
    /// the moment the clamp stopped being applied.
    ///
    /// Argument is passed explicitly rather than through `&self` so the
    /// helper can be called inside the `for ch in &mut self.channels`
    /// loops without aliasing `self`.
    fn clamp_amiga(ch: &mut Channel, enabled: bool) {
        if !enabled {
            return;
        }
        let lo = (AMIGA_CLOCK_HZ as f32) / (AMIGA_LIMIT_PERIOD_MAX as f32);
        let hi = (AMIGA_CLOCK_HZ as f32) / (AMIGA_LIMIT_PERIOD_MIN as f32);
        if ch.frequency > 0.0 {
            ch.frequency = ch.frequency.clamp(lo, hi);
        }
        if ch.target_frequency > 0.0 {
            ch.target_frequency = ch.target_frequency.clamp(lo, hi);
        }
    }

    pub fn samples_per_tick(&self) -> u32 {
        ((self.sample_rate as f32) * 2.5 / self.bpm.max(1) as f32) as u32
    }

    fn find_next_playable_order(&mut self) -> Option<u8> {
        // Walk past 0xFE (marker) entries; stop at 0xFF (end).
        while (self.order_index as usize) < self.order.len() {
            let v = self.order[self.order_index as usize];
            if v == 0xFF {
                return None;
            }
            if v == 0xFE {
                self.order_index = self.order_index.saturating_add(1);
                continue;
            }
            return Some(v);
        }
        None
    }

    fn enter_row(&mut self) {
        let pat_idx = match self.find_next_playable_order() {
            Some(v) => v as usize,
            None => {
                self.ended = true;
                return;
            }
        };
        if pat_idx >= self.patterns.len() {
            self.ended = true;
            return;
        }
        let row_cells: Vec<Cell> = self.patterns[pat_idx].rows[self.row as usize].clone();

        // Pattern-loop is scoped to a single pattern. When playback crosses
        // into a different order slot (natural advance, Bxx, Cxx, or an order
        // skip past a marker — `find_next_playable_order` has already settled
        // `order_index` on the slot being played), reset the SBx loop start to
        // the top of the new pattern and discard any in-flight loop counter —
        // a loop point set in a previous pattern must never trigger a jump in
        // this one, and an SBx with no preceding SB0 defaults to row 0
        // (`docs/audio/trackers/s3m/ScreamTracker-v3.20-effects.txt` §SBx).
        // Keyed on `order_index` (the playback position), so a within-pattern
        // SBx loop back to `loop_start_row` keeps its state.
        let cur_order = self.order_index;
        if self.loop_pattern_order != Some(cur_order) {
            self.loop_pattern_order = Some(cur_order);
            self.loop_start_row = 0;
            self.loop_count = None;
        }

        // Drop any Vxx stash left over from the previous row. A speed-1
        // row never reaches tick 1, so the per-tick drain misses it; we
        // discard it here so the spec rule "doesn't do anything if the
        // current speed is 1" holds across the row boundary.
        // `pending_global_vol = None` is also the right reset for the
        // common case (previous row carried a Vxx and the tick-1 drain
        // already ran) — `take()` left it `None`, so this assignment is
        // a no-op there.
        self.pending_global_vol = None;

        // SEx pattern delay: when we're in the middle of a held row, the
        // notes are *not* re-triggered, but the row's tick-0 effects are
        // also not re-armed (per-tick effects keep firing because the
        // channel state still carries `command` / `info` from the
        // original entry). We just skip the cell-application phase.
        let replaying_held_row = self.replaying_for_pattern_delay;

        let mut row_speed: Option<u8> = None;
        let mut row_tempo: Option<u8> = None;
        let mut row_jump: Option<Jump> = None;
        // Bxx / Cxx write two *independent* pieces of playback state — Bxx
        // the target order, Cxx the target row — and ST3 merges them when
        // both appear on one row: "jump to row Cxx (decimal) of the pattern
        // at order Bxx (hex)". Within each variable it is last-writer-wins
        // across the left-to-right channel scan, but a later Bxx must not
        // discard an earlier Cxx's row (nor vice-versa) because they are
        // different variables — channel order between the pair does not
        // matter. See `docs/audio/trackers/s3m/
        // s3m-position-jump-pattern-break-and-adpcm.md` §Part 1.
        let mut jump_order: Option<u8> = None;
        let mut jump_row: Option<u8> = None;

        let mut row_loop_request: Option<u8> = None;
        let mut row_pattern_delay: Option<u8> = None;

        for (ch_idx, cell) in row_cells.iter().enumerate() {
            if ch_idx >= self.channels.len() {
                break;
            }
            let ch = &mut self.channels[ch_idx];
            if replaying_held_row {
                // Don't touch ch.command/info — keep prior row's state so
                // per-tick effects (vibrato, slides, …) keep cycling.
                continue;
            }
            ch.command = cell.command;
            ch.info = cell.info;
            // Snapshot the channel's note volume before this row's tick-0
            // writes. Any change (a note trigger reloading the instrument
            // default, a volume-column write, or a tick-0 volume slide)
            // re-latches the voice's global volume to the value live *now*
            // — which at tick 0 is still the pre-Vxx player global volume,
            // so a same-row Vxx does not reach a freshly-triggered note.
            let vol_before_row = ch.volume;
            // Capture the *raw* row infobyte before effect-memory recall.
            // Needed below to disambiguate a true S00 (raw 0, resolved by
            // memory) from a freshly-written Sxy of the same resolved value
            // — the multimedia.cx §S0x rule "When S00 is repeating a note
            // delay (SDx), the note is triggered twice: once on tick 0 ...
            // and again on tick x" only applies to the recalled form.
            let raw_info = cell.info;
            // SCx freeze resume: per multimedia.cx §SCx, any of Exx, Fxx,
            // Gxx, Hxx, Jxx, Kxx, Lxx, Uxx thaws a frozen channel. This
            // happens on the *row* the command appears, irrespective of
            // its parameter — the wiki names the commands without nibble
            // qualifications.
            if ch.frozen && is_scx_resume_command(ch.command) {
                ch.frozen = false;
            }
            // Effect-memory ("%"): if the row's infobyte is zero and the
            // channel has seen this command before with a nonzero parameter,
            // ST3 reuses the latest nonzero value. H/U share a slot. S also
            // shares a slot per the multimedia.cx behavioural reference.
            // Effects that do NOT carry memory: A (speed), B/C (jumps), T,
            // V (global volume) — they get a fresh-row interpretation each
            // time and zero-arg is per-effect specified.
            //
            // The command byte comes straight out of the packed pattern, so a
            // corrupt / hostile module can carry any value 1..=255. ST3's
            // effect alphabet is A..Z (stored 1..=26); the memory table only
            // has a slot per letter. A command outside that range is not a
            // real effect — skip the memory recall (and the dispatch below
            // falls through its `_ => {}` arm) rather than index the table out
            // of bounds.
            let slot = effect_memory_slot(ch.command) as usize;
            if ch.command != 0 && slot < ch.effect_memory.len() && effect_uses_memory(ch.command) {
                if ch.info == 0 {
                    ch.info = ch.effect_memory[slot];
                } else {
                    ch.effect_memory[slot] = ch.info;
                }
            }
            // Clear any leftover delayed trigger from a prior row.
            ch.pending_delay = None;
            // Note: Ixy tremor on/off counters are **never reset per row**.
            // Per the multimedia.cx behavioural reference §Ixy: "The 'on'
            // and 'off' counters are never reset, except in the tremor
            // update procedure described above. Scream Tracker doesn't
            // even reset them on playback start." Channels carrying Ixy
            // across rows keep the running cycle.
            // Qxy retrigger counter: per the multimedia.cx behavioural
            // reference (§Qxy), the counter is reset *only* when a row
            // without the Qxy effect is encountered (or at song start).
            // A row that *does* carry Qxy — even a brand-new note — keeps
            // the running counter so the retrig cadence is unbroken.
            if ch.command != cmd::Q_RETRIGGER {
                ch.retrig_counter = 0;
            }

            // Detect SDx (note delay) before applying the row: when x > 0,
            // we stash the cell and skip the usual tick-0 trigger so the
            // note fires at tick x instead.
            let is_note_delay =
                ch.command == cmd::S_EXTENDED && (ch.info >> 4) == 0xD && (ch.info & 0x0F) != 0;

            // S00 → SDx double-trigger per
            // `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
            // §S0x: "When S00 is repeating a note delay (SDx), the note is
            // triggered twice: once on tick 0 (as if there's no note delay)
            // and again on tick x (as with a normal note delay)." This
            // applies only when the row's *raw* infobyte was 0 and effect-
            // memory recall produced an SDx with x > 0 — a freshly-written
            // SDx still defers cleanly (single trigger at tick x).
            let is_s00_repeat_sdx =
                is_note_delay && raw_info == 0 && cell.command == cmd::S_EXTENDED;

            if is_note_delay && !is_s00_repeat_sdx {
                ch.pending_delay = Some(PendingTrigger {
                    fire_tick: ch.info & 0x0F,
                    note: cell.note,
                    instrument: cell.instrument,
                    volume: cell.volume,
                });
            } else {
                // Either: (a) no note delay at all (normal row), or
                // (b) S00 recalled an SDx — in case (b) we fall through to
                // the immediate-trigger block AND also stash the deferred
                // trigger so the note fires on tick x as well.
                if is_s00_repeat_sdx {
                    ch.pending_delay = Some(PendingTrigger {
                        fire_tick: ch.info & 0x0F,
                        note: cell.note,
                        instrument: cell.instrument,
                        volume: cell.volume,
                    });
                }
                // Instrument change reloads volume.
                if cell.instrument != 0 {
                    ch.instrument = cell.instrument;
                    if let Some(s) = self.samples.get(cell.instrument as usize - 1) {
                        // multimedia.cx §Playback Notes: "Volumes actually
                        // peak at 63" — so a sample default of 64 lands as
                        // 63 on the PCM path. (Adlib instruments are out
                        // of scope and don't reach this branch.)
                        // Instrument-default load is a stored-volume
                        // write per the multimedia.cx §Ixy / §Rxy
                        // "stored volume isn't modified by this effect"
                        // distinction — Ixy's restore target is updated
                        // here.
                        let v = clamp_pcm_volume(s.volume as u16);
                        ch.volume = v;
                        ch.stored_volume = v;
                        // An instrument reload is a note-volume write: latch
                        // the global volume even when `v` equals the prior
                        // value (the post-loop change check would miss a
                        // same-numeric-volume re-trigger after an inter-row
                        // Vxx). At tick 0 `self.global_volume` is still the
                        // pre-Vxx value, so a same-row Vxx is excluded.
                        ch.voice_global_vol = self.global_volume;
                    }
                }

                // Note cut.
                if cell.note == 0xFE {
                    ch.active = false;
                    ch.frequency = 0.0;
                } else if cell.note != 0xFF {
                    // Trigger.
                    let inst_idx = ch.instrument as usize;
                    if inst_idx > 0 && inst_idx <= self.samples.len() {
                        let c5 = self.samples[inst_idx - 1].c5_speed.max(1);
                        let freq = note_to_frequency(cell.note, c5);
                        // Tone portamento (G) or its volslide combo (L):
                        // don't retrigger, set target.
                        let porta =
                            ch.command == cmd::G_TONE_PORTA || ch.command == cmd::L_PORT_VOL;
                        if porta && ch.frequency > 0.0 {
                            ch.target_frequency = freq;
                            Self::clamp_amiga(ch, self.amiga_limits);
                            // The note still "shows up in the channel" even
                            // though the porta suppressed the retrigger, so it
                            // becomes the channel's last note. Per the
                            // multimedia.cx §Gxx peculiarity ("If the current
                            // note is empty, the destination note is set to
                            // the last note to show up in the channel"), a
                            // later Gxx/Lxy row with *no* note must slide back
                            // to this note — which only works if `last_note`
                            // tracks porta-suppressed triggers too.
                            ch.last_note = cell.note;
                        } else {
                            ch.frequency = freq;
                            ch.target_frequency = freq;
                            Self::clamp_amiga(ch, self.amiga_limits);
                            // Re-apply Oxx sample offset if present. For
                            // looped samples whose offset overshoots
                            // `loop_end`, fold the overage back into the
                            // loop window per
                            // `docs/audio/trackers/s3m/ScreamTracker-v3.20-effects.txt`
                            // §Oxy ("If the sample offset is used in a
                            // looped sample and the offset given exceeds
                            // the loop end value, the loop is taken into
                            // consideration and the offset will be
                            // calculated as if the sample had looped").
                            if ch.command == cmd::O_SAMPLE_OFFSET {
                                let off = (ch.info as u64) * 256;
                                let body = &self.samples[inst_idx - 1];
                                ch.sample_pos = resolve_sample_offset(
                                    off,
                                    body.loop_start,
                                    body.loop_end,
                                    body.pcm.len(),
                                    body.is_looped(),
                                );
                            } else {
                                ch.sample_pos = 0.0;
                            }
                            ch.active = true;
                            // A fresh note trigger thaws an SCx-frozen
                            // channel: the new note replaces the held
                            // sample so "frozen" is no longer meaningful.
                            ch.frozen = false;
                            // S3x / S4x bit 2 keeps the vibrato / tremolo
                            // phase across new notes (multimedia.cx wiki
                            // §S3x: "If the third bit is set, don't reset
                            // waveform when a new note is played.")
                            if !ch.keep_vibrato_pos_on_new_note {
                                ch.vibrato_pos = 0;
                            }
                            if !ch.keep_tremolo_pos_on_new_note {
                                ch.tremolo_pos = 0;
                            }
                            ch.last_note = cell.note;
                        }
                    }
                } else if (ch.command == cmd::G_TONE_PORTA || ch.command == cmd::L_PORT_VOL)
                    && ch.last_note != 0
                {
                    // Gxx / Lxy with NO note on this row. Per the
                    // multimedia.cx behavioural reference
                    // (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
                    // §Gxx): "If the current note is empty, the destination
                    // note is set to the last note to show up in the channel,
                    // even if it has occurred without the Gxx effect." So a
                    // bare Gxx slides back toward whatever note last played in
                    // the slot — including a note that triggered without any
                    // porta. (The companion peculiarity — "Gxx doesn't clear
                    // the target note when it is reached" — falls out for free
                    // since `target_frequency` is never zeroed on arrival.)
                    let inst_idx = ch.instrument as usize;
                    if inst_idx > 0 && inst_idx <= self.samples.len() {
                        let c5 = self.samples[inst_idx - 1].c5_speed.max(1);
                        ch.target_frequency = note_to_frequency(ch.last_note, c5);
                        Self::clamp_amiga(ch, self.amiga_limits);
                    }
                }

                // Explicit volume column.
                if cell.volume != 0xFF {
                    // Volumes peak at 63 on the PCM path — see
                    // [`PCM_VOLUME_PEAK`]. A row carrying `..|V40|..` lands
                    // as 63, matching the documented multimedia.cx
                    // behaviour ("Setting the volume to 64 will actually
                    // make it go to 63").
                    // The volume column is a stored-volume write per the
                    // multimedia.cx §Ixy / §Rxy stored-vs-active
                    // distinction; Ixy's restore target is updated.
                    let v = clamp_pcm_volume(cell.volume as u16);
                    ch.volume = v;
                    ch.stored_volume = v;
                    // Volume-column write re-latches the voice global volume
                    // (see the instrument-reload branch for why this is
                    // unconditional rather than change-gated).
                    ch.voice_global_vol = self.global_volume;
                }
            }

            // Tick-0 effects (instant / row-level).
            let x = ch.info >> 4;
            let y = ch.info & 0x0F;
            match ch.command {
                cmd::A_SET_SPEED if ch.info != 0 => {
                    row_speed = Some(ch.info);
                }
                cmd::B_POS_JUMP => {
                    // Writes only the target *order* (hex infobyte). The
                    // implicit row-0 destination of a bare Bxx comes from the
                    // merge below (`jump_row` still `None` → row 0), so a
                    // companion Cxx on the same row can override it.
                    jump_order = Some(ch.info);
                }
                cmd::C_PAT_BREAK => {
                    // Writes only the target *row*. Parameter is BCD (high
                    // nibble * 10 + low). Per the multimedia.cx behavioural
                    // reference, an out-of-range target (>= 64) makes ST3
                    // ignore the effect entirely — an earlier same-row Cxx
                    // write survives an invalid later one.
                    let r = (ch.info >> 4) * 10 + (ch.info & 0x0F);
                    if r < 64 {
                        jump_row = Some(r);
                    }
                }
                cmd::D_VOL_SLIDE => {
                    // Tick-0 leg of the Dxy table. The multimedia.cx wiki
                    // enumerates the full case matrix; the per-tick path
                    // handles the smooth slides for D0x / Dx0 and the
                    // remaining `D0F` / `DF0` units (which also fire at
                    // tick 0, per the "slide on all ticks" note).
                    //
                    //   DFF       → fine up by 15 on tick 0.
                    //   DFx (x<F) → fine down by x on tick 0.
                    //   DxF (x<F) → fine up by x on tick 0.
                    //   D0F       → slide down by 15 on all ticks, incl. 0.
                    //   DF0       → slide up   by 15 on all ticks, incl. 0.
                    if x == 0xF && y == 0xF {
                        // DFF: slide up by 15 on tick 0 — multimedia.cx
                        // contradicts an earlier reading of the v3.20
                        // manual that treated it as DFy=down-by-F. Cap at
                        // [`PCM_VOLUME_PEAK`] per §Playback Notes.
                        ch.volume = clamp_pcm_volume(ch.volume as u16 + 15);
                    } else if x == 0xF && y == 0 {
                        // DF0: slide up by 15 on tick 0 (also fires on all
                        // subsequent ticks via apply_dxy).
                        ch.volume = clamp_pcm_volume(ch.volume as u16 + 15);
                    } else if x == 0 && y == 0xF {
                        // D0F: slide down by 15 on tick 0.
                        ch.volume = ch.volume.saturating_sub(15);
                    } else if x == 0xF && y != 0 {
                        // DFy: fine down by y.
                        ch.volume = ch.volume.saturating_sub(y);
                    } else if y == 0xF && x != 0 {
                        // DxF: fine up by x.
                        ch.volume = clamp_pcm_volume(ch.volume as u16 + x as u16);
                    } else if self.fast_slides {
                        // Header flag bit 6 (or CwtV == 0x1300): the
                        // continuous Dx0 / D0x / Dxy nibble forms ALSO
                        // fire at tick 0, on top of the per-tick path's
                        // nonzero-tick steps. The doc lists this as the
                        // "fast slides" behaviour on the multimedia.cx
                        // §Dxy "Also slide on tick 0, if fast slides
                        // are enabled" lines.
                        Self::apply_dxy_tick0_fast_slide(ch, x, y);
                    }
                }
                cmd::E_SLIDE_DOWN if ch.info >= 0xE0 => {
                    // EFx: fine down by x. EEx: extra-fine (×0.25). Both
                    // are tick-0 only.
                    let amount = (ch.info & 0x0F) as f32;
                    let scale = if (ch.info & 0xF0) == 0xE0 { 0.25 } else { 1.0 };
                    let f = 2.0f32.powf(-amount * scale / 768.0);
                    ch.frequency *= f;
                    ch.target_frequency *= f;
                    Self::clamp_amiga(ch, self.amiga_limits);
                }
                cmd::F_SLIDE_UP if ch.info >= 0xE0 => {
                    // FFx: fine up. FEx: extra-fine up. Tick-0 only.
                    let amount = (ch.info & 0x0F) as f32;
                    let scale = if (ch.info & 0xF0) == 0xE0 { 0.25 } else { 1.0 };
                    let f = 2.0f32.powf(amount * scale / 768.0);
                    ch.frequency *= f;
                    ch.target_frequency *= f;
                    Self::clamp_amiga(ch, self.amiga_limits);
                }
                cmd::Q_RETRIGGER => {
                    // Qxy is processed on every tick *including tick 0*,
                    // so the counter must advance here too. A tick-0 retrig
                    // can fire immediately after the new note (per the
                    // multimedia.cx §Qxy "retrig on tick 0" note).
                    Self::apply_retrigger(ch);
                }
                cmd::I_TREMOR if (x | y) != 0 => {
                    // Ixy: "This effect is updated on every tick" per
                    // `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
                    // §Ixy — so tick 0 also advances the decrementing
                    // counter pair. The persistent counters
                    // (`tremor_on_counter` / `tremor_off_counter`) live
                    // on `Channel` and are never reset on row entry;
                    // see [`apply_tremor_step`] for the procedure.
                    Self::apply_tremor_step(ch, x, y);
                }
                cmd::T_SET_TEMPO if ch.info >= 0x20 => {
                    row_tempo = Some(ch.info);
                }
                // Vxx (set global volume). Per
                // `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
                // §Vxx:
                //   * "values higher than 0x40 are ignored" — the whole
                //     effect is dropped, not clamped, so a stray V41..VFF
                //     on row N leaves the prior global volume untouched.
                //   * "actually processed on tick 1 (that is the second
                //     tick) of the row" — so we *stash* the validated value
                //     here. The per-tick path drains it on tick 1
                //     (`apply_per_tick`), which:
                //       - leaves same-row notes (triggered at tick 0)
                //         observing the OLD global volume, matching "does
                //         not affect events on the same row";
                //       - lets SDx-delayed notes (fire_tick >= 1) and the
                //         per-tick Dxx volume slide see the NEW value
                //         implicitly because the mixer reads
                //         `self.global_volume` live;
                //       - skips the effect entirely when speed == 1 (the
                //         row advances before tick 1 ever fires).
                // The arm guards on `ch.info <= 0x40` because the spec rule
                // is *ignore*, not *clamp* — invalid values must not even
                // stash a sentinel.
                cmd::V_GLOBAL_VOL if ch.info <= 0x40 => {
                    self.pending_global_vol = Some(ch.info);
                }
                cmd::S_EXTENDED => {
                    // Sxy: extended commands. Subcommand in high nibble.
                    let sub = ch.info >> 4;
                    let p = ch.info & 0x0F;
                    match sub {
                        // S0x — Amiga filter, unimplemented per ST3 spec.
                        0x0 => {}
                        // S1x — glissando: 0 disables, non-zero enables.
                        0x1 => ch.glissando = p != 0,
                        // S2x — set finetune from the 16-entry C4Spd table.
                        // Per the spec this changes the *running C5 speed*
                        // semantically; we mirror that by updating the
                        // already-triggered note's frequency relative to
                        // its last-played note.
                        0x2 => {
                            let new_c5 = S2X_FINETUNE_TABLE[p as usize];
                            if ch.last_note != 0 {
                                let new_freq = note_to_frequency(ch.last_note, new_c5);
                                ch.frequency = new_freq;
                                ch.target_frequency = new_freq;
                                Self::clamp_amiga(ch, self.amiga_limits);
                            }
                        }
                        // S3x — vibrato waveform. Bit 2 (mask 0x04) is the
                        // "don't reset waveform position when a new note
                        // plays" flag, per the multimedia.cx behavioural
                        // reference for ST3 §S3x. Bit 3 (mask 0x08) is
                        // ignored.
                        0x3 => {
                            ch.vibrato_waveform = Waveform::from_nibble(p);
                            ch.keep_vibrato_pos_on_new_note = (p & 0x04) != 0;
                        }
                        // S4x — tremolo waveform. Same bit-2 contract.
                        0x4 => {
                            ch.tremolo_waveform = Waveform::from_nibble(p);
                            ch.keep_tremolo_pos_on_new_note = (p & 0x04) != 0;
                        }
                        0x8 => ch.pan = p,
                        // SAx — legacy "old stereo control".
                        //
                        // Per the multimedia.cx behavioural reference
                        // (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
                        // §SAx): "This effect is dependent on what channel
                        // type you put it on", and the parameter nibble
                        // selects one of four behaviours:
                        //
                        //   * SA0 / SA2 → Normal panning  (L is left,
                        //     R is right).
                        //   * SA1 / SA3 → Reversed panning (L is right,
                        //     R is left).
                        //   * SA4..SA7  → Center panning (the doc notes it
                        //     "adds DC offset to either left or right"; in
                        //     our integer pan model that is the centre slot).
                        //   * SA8..SAF  → No effect.
                        //
                        // The L/R designation comes from the channel's bank
                        // (`right_bank`, derived from the channel-settings
                        // low nibble at load time), NOT from the parameter:
                        // a left-bank channel pans hard-left under a normal
                        // SA0 and hard-right under a reversed SA1, and vice
                        // versa for a right-bank channel. We resolve to the
                        // hard-left / hard-right / centre pan slots an S8x
                        // command would land on (`0` / `15` / `7`) so
                        // back-catalogue modules carrying this legacy effect
                        // play with the intended stereo image. ST3's editor
                        // emits S8x in new files, so this path only matters
                        // for legacy playback.
                        0xA => match p {
                            0x0 | 0x2 => {
                                // Normal: bank decides the hard side.
                                ch.pan = if ch.right_bank { 0x0F } else { 0x00 };
                            }
                            0x1 | 0x3 => {
                                // Reversed: swap the bank's hard side.
                                ch.pan = if ch.right_bank { 0x00 } else { 0x0F };
                            }
                            0x4..=0x7 => {
                                // Center (DC-offset) → centre pan slot.
                                ch.pan = 0x07;
                            }
                            // 0x8..=0xF: documented "No effect".
                            _ => {}
                        },
                        0xB => {
                            // Collect loop requests across channels; ST3
                            // applies the last one on the row.
                            row_loop_request = Some(p);
                        }
                        // SCx — note cut after x ticks. Per the multimedia.cx
                        // behavioural reference (§SCx) the SC0 case is
                        // *ignored* — ST3 does not cut on tick 0. The per-
                        // tick path silences the channel when `tick == x`.
                        0xC => {}
                        // SDx (x>0) handled above as note_delay.
                        // SD0 falls through as a normal trigger.
                        0xD => {}
                        // SEx — pattern delay: hold this row x extra
                        // times. Reset back to 0 on the row that issues a
                        // non-SEx command (handled implicitly because the
                        // counter only ticks down).
                        0xE => {
                            row_pattern_delay = Some(p);
                        }
                        // SFx — funkrepeat; not implemented in ST3.
                        0xF => {}
                        _ => {}
                    }
                }
                cmd::X_SET_PAN => {
                    // Xxx: set absolute pan (0..0x80, 0=left, 0x80=right).
                    // Map to our 0..15 internal scale.
                    let pan15 = (ch.info as u16 * 15 / 0x80).min(15) as u8;
                    ch.pan = pan15;
                }
                _ => {}
            }

            // Re-latch the voice global volume if any tick-0 write changed
            // the note volume. `self.global_volume` here is the pre-Vxx
            // value (Vxx drains on tick 1), so same-row triggers keep the
            // old global volume — satisfying §Vxx "does not affect events
            // on the same row, where the effect is set".
            if ch.volume != vol_before_row {
                ch.voice_global_vol = self.global_volume;
            }
        }

        if replaying_held_row {
            // Held rows do not re-trigger anything but still walk through
            // the per-tick path for the rest of the row.
            return;
        }

        // Merge the row's Bxx / Cxx writes into one jump: order from the
        // last Bxx (or "next order" when only a Cxx appeared → `None`),
        // row from the last valid Cxx (or 0 when only a Bxx appeared).
        // `B02`+`C16` therefore lands on order 02 row 16 regardless of
        // which channel carried which command
        // (`docs/audio/trackers/s3m/
        // s3m-position-jump-pattern-break-and-adpcm.md` §Part 1).
        if jump_order.is_some() || jump_row.is_some() {
            row_jump = Some(Jump {
                order: jump_order,
                row: jump_row.unwrap_or(0),
            });
        }

        // Resolve SBx (pattern loop) after the row is scanned. SB0 marks
        // the start row; SBx (x>0) arms / decrements the counter and jumps
        // back when count reaches zero it clears the loop.
        if let Some(p) = row_loop_request {
            if p == 0 {
                // SB0: set loop start to current row.
                self.loop_start_row = self.row;
            } else {
                // SBx, x>0: loop back to loop_start_row.
                let remaining = match self.loop_count {
                    None => {
                        self.loop_count = Some(p);
                        p
                    }
                    Some(n) => n.saturating_sub(1),
                };
                if remaining > 0 {
                    self.loop_count = Some(remaining);
                    // Override any other row jump — ST3 gives SB priority
                    // over a same-row pattern-break. Stay on the current
                    // order index; `next_row` sees `Some(order_index)` and
                    // will not increment.
                    row_jump = Some(Jump {
                        order: Some(self.order_index),
                        row: self.loop_start_row,
                    });
                } else {
                    self.loop_count = None;
                }
            }
        }

        if let Some(s) = row_speed {
            self.speed = s;
        }
        if let Some(t) = row_tempo {
            self.bpm = t;
        }
        if let Some(p) = row_pattern_delay {
            // SEx arms the counter only on the first play of this row;
            // the row plays once at full effect, then `p` extra repeats
            // follow. We're in the first-play branch because
            // `replaying_held_row` is false above.
            self.pattern_delay_remaining = p;
        }
        if row_jump.is_some() {
            self.pending_jump = row_jump;
        }
    }

    fn apply_per_tick(&mut self) {
        let tick = self.tick;
        // Vxx is "actually processed on tick 1" per
        // `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
        // §Vxx. The row stashed the validated value in
        // `pending_global_vol` at tick 0; drain it here. Same-row notes
        // (triggered at tick 0) have already been mixed against the old
        // global volume, satisfying the "does not affect events on the
        // same row" rule. Channels mixing from tick 1 onward (and SDx-
        // deferred triggers with fire_tick >= 1) read the new value
        // implicitly because the mixer consults `self.global_volume`
        // live. When `speed == 1`, tick 1 never fires before the row
        // advances and the stash is cleared by `enter_row` — matching
        // the spec's "doesn't do anything if the current speed is 1".
        if tick == 1 {
            if let Some(g) = self.pending_global_vol.take() {
                self.global_volume = g;
            }
        }
        // Snapshot the (post-Vxx-drain) player global volume so the channel
        // loop can re-latch it into any voice whose note volume a per-tick
        // effect changes. Per §Vxx, a Vxx "is also applied on the same row
        // if anything updates the note volume on tick 1 or tick 2 ... like
        // the Dxx effect (when doing a non-fine slide)" — with the
        // per-voice latch, that falls out naturally: the volume-modifying
        // effect changes `ch.volume`, which re-latches this value (now the
        // new global volume from this row's Vxx).
        let global_vol_now = self.global_volume;
        // Clone sample metadata we need for deferred SDx triggers. Can't
        // borrow `&self.samples` inside the mutable-channel loop.
        let samples_snapshot: Vec<(u32, u8)> = self
            .samples
            .iter()
            .map(|s| (s.c5_speed.max(1), s.volume))
            .collect();
        // Snapshot the header-derived flag so the channel loop can clamp
        // each frequency-mutating effect without re-borrowing `self`.
        let amiga_limits = self.amiga_limits;
        for ch in &mut self.channels {
            let x = ch.info >> 4;
            let y = ch.info & 0x0F;
            // Snapshot the note volume before this tick's effects. Any
            // change (SDx-deferred trigger, Dxy/Kxy/Lxy slide, Qxy
            // retrigger modifier, Rxy tremolo, Ixy tremor) re-latches the
            // voice's global volume to `global_vol_now`.
            let vol_before_tick = ch.volume;

            // SDx (note delay): fire the stashed trigger at tick x.
            if let Some(pd) = ch.pending_delay {
                if tick == pd.fire_tick {
                    // Apply the stashed cell data like `enter_row` would.
                    if pd.instrument != 0 {
                        ch.instrument = pd.instrument;
                        let idx = pd.instrument as usize;
                        if idx > 0 && idx <= samples_snapshot.len() {
                            // PCM volumes peak at 63 — see [`PCM_VOLUME_PEAK`].
                            // SDx-deferred instrument load is still a
                            // stored-volume write — Ixy's restore target
                            // tracks it just like the immediate trigger.
                            let v = clamp_pcm_volume(samples_snapshot[idx - 1].1 as u16);
                            ch.volume = v;
                            ch.stored_volume = v;
                            // SDx-deferred instrument reload fires on tick x
                            // (>= 1), so `global_vol_now` is the post-Vxx
                            // value — the delayed note correctly picks up a
                            // same-row Vxx, per §Vxx "applied to notes ...
                            // that have a note delay (SDx with x >= 1)".
                            ch.voice_global_vol = global_vol_now;
                        }
                    }
                    if pd.note == 0xFE {
                        ch.active = false;
                        ch.frequency = 0.0;
                    } else if pd.note != 0xFF {
                        let inst_idx = ch.instrument as usize;
                        if inst_idx > 0 && inst_idx <= samples_snapshot.len() {
                            let c5 = samples_snapshot[inst_idx - 1].0;
                            let freq = note_to_frequency(pd.note, c5);
                            ch.frequency = freq;
                            ch.target_frequency = freq;
                            Self::clamp_amiga(ch, amiga_limits);
                            ch.sample_pos = 0.0;
                            ch.active = true;
                            // SDx-deferred trigger thaws SCx freeze.
                            ch.frozen = false;
                            if !ch.keep_vibrato_pos_on_new_note {
                                ch.vibrato_pos = 0;
                            }
                            if !ch.keep_tremolo_pos_on_new_note {
                                ch.tremolo_pos = 0;
                            }
                            ch.last_note = pd.note;
                        }
                    }
                    if pd.volume != 0xFF {
                        // PCM volumes peak at 63 — see [`PCM_VOLUME_PEAK`].
                        // SDx-deferred volume-column write is a
                        // stored-volume write, same as the immediate
                        // form — Ixy's restore target tracks it.
                        let v = clamp_pcm_volume(pd.volume as u16);
                        ch.volume = v;
                        ch.stored_volume = v;
                        // SDx-deferred volume-column write re-latches the
                        // (post-Vxx) global volume; see the deferred
                        // instrument-reload branch above.
                        ch.voice_global_vol = global_vol_now;
                    }
                    ch.pending_delay = None;
                }
            }

            // SCx (note cut): freeze the channel at tick x. Per the
            // multimedia.cx behavioural reference §SCx, the volume is
            // *not* zeroed — playback is temporarily frozen so that a
            // later Exx/Fxx/Gxx/Hxx/Jxx/Kxx/Lxx/Uxx command (or a fresh
            // note trigger on a subsequent row) can resume it. The
            // mixer reads `frozen` and emits silence while holding
            // sample_pos / volume / frequency intact.
            if ch.command == cmd::S_EXTENDED && (ch.info >> 4) == 0xC {
                let cut_tick = ch.info & 0x0F;
                if tick == cut_tick {
                    ch.frozen = true;
                }
            }

            match ch.command {
                // Jxy: cycle through note, note+x semitones, note+y
                // semitones across consecutive ticks (0, 1, 2, 0, 1, 2…).
                // Per the FireLight tutorial §5.1 step 9 ("Arpeggio …
                // All we have to do is add the parameter given!") and
                // §6.10, the semitone offset is added to the *note index*
                // and the result looked up in the canonical period table
                // — NOT applied as an equal-tempered frequency multiply.
                // The arpeggio leg keeps the channel's own `c2spd`
                // (resolved via the current instrument), so an out-of-tune
                // sample stays out of tune across the chord.
                cmd::J_ARPEGGIO if (x | y) != 0 && ch.last_note != 0 => {
                    let semis = match tick % 3 {
                        0 => 0,
                        1 => x as i32,
                        _ => y as i32,
                    };
                    let octave = (ch.last_note >> 4) as i32;
                    let semitone = (ch.last_note & 0x0F) as i32;
                    let n = octave * 12 + semitone + semis;
                    let inst_idx = ch.instrument as usize;
                    if inst_idx > 0 && inst_idx <= samples_snapshot.len() {
                        let c2spd = samples_snapshot[inst_idx - 1].0;
                        ch.frequency = note_index_to_frequency(n, c2spd);
                        Self::clamp_amiga(ch, amiga_limits);
                    }
                }
                // Kxy = "H00 + Dxy" (multimedia.cx §Kxy). The vibrato leg is
                // H00 — it *continues the vibrato already running* on the
                // channel using the remembered H speed/depth, NOT Kxy's own
                // (x, y) nibbles (those are the volume slide).
                //
                // §Kxy also notes the volume slide differs from Dxy: when a
                // *fine* slide is requested (DFy / DxF / DFF forms) the slide
                // does nothing AND the "other" effect (H00) is also
                // suppressed (the guard below). D0F / DF0 (slide-on-all-ticks)
                // are not fine and still run.
                cmd::K_VIB_VOL if !Self::is_fine_volslide(x, y) => {
                    let (h_speed, h_depth) = Self::vibrato_memory(ch);
                    Self::apply_vibrato(ch, h_speed, h_depth, /* fine: */ false);
                    Self::clamp_amiga(ch, amiga_limits);
                    Self::apply_dxy(ch, x, y);
                }
                // Lxy = "G00 + Dxy" (multimedia.cx §Lxy). The porta leg is G00
                // — it *continues the tone portamento already running* using
                // the remembered G rate, NOT Lxy's own infobyte. Same
                // fine-slide suppression rule as Kxy (the guard disables both
                // the volume slide and the G00 porta).
                cmd::L_PORT_VOL if !Self::is_fine_volslide(x, y) => {
                    let g_rate = ch.effect_memory[cmd::G_TONE_PORTA as usize];
                    Self::apply_tone_porta(ch, g_rate);
                    Self::clamp_amiga(ch, amiga_limits);
                    Self::apply_dxy(ch, x, y);
                }
                // Qxy: retrigger note every y ticks with volume modifier x.
                // The full per-tick counter logic (incl. tick 0, run from
                // `enter_row`) lives in `apply_retrigger`.
                cmd::Q_RETRIGGER => Self::apply_retrigger(ch),
                cmd::R_TREMOLO => Self::apply_tremolo(ch, x, y),
                cmd::D_VOL_SLIDE => Self::apply_dxy(ch, x, y),
                // Exx: portamento down. Each tick: freq *= 2^(-param/768).
                // Fine / extra-fine slides (0xEy / 0xFy) are tick-0 only
                // — skip on per-tick path.
                cmd::E_SLIDE_DOWN if ch.info != 0 && ch.info < 0xE0 => {
                    let f = 2.0f32.powf(-(ch.info as f32) / 768.0);
                    ch.frequency *= f;
                    ch.target_frequency *= f;
                    Self::clamp_amiga(ch, amiga_limits);
                }
                cmd::F_SLIDE_UP if ch.info != 0 && ch.info < 0xE0 => {
                    let f = 2.0f32.powf((ch.info as f32) / 768.0);
                    ch.frequency *= f;
                    ch.target_frequency *= f;
                    Self::clamp_amiga(ch, amiga_limits);
                }
                // Gxx: slide toward target at rate info/per tick.
                cmd::G_TONE_PORTA if ch.info != 0 && ch.target_frequency > 0.0 => {
                    Self::apply_tone_porta(ch, ch.info);
                    Self::clamp_amiga(ch, amiga_limits);
                }
                cmd::H_VIBRATO => {
                    // Hxy: vibrato. x = speed, y = depth.
                    Self::apply_vibrato(ch, x, y, /* fine: */ false);
                    Self::clamp_amiga(ch, amiga_limits);
                }
                // Ixy: tremor. The per-tick step (decrementing-counter
                // procedure with persistent counters across rows) lives
                // in [`apply_tremor_step`] so it can be reused from the
                // tick-0 path in `enter_row` as well — §Ixy says the
                // effect "is updated on every tick" including tick 0.
                // The else-branch here handles ticks 1..speed-1; the
                // tick-0 step is fired from `enter_row` after the
                // stored-volume sources (cell.instrument / cell.volume)
                // have been applied.
                cmd::I_TREMOR if (x | y) != 0 => {
                    Self::apply_tremor_step(ch, x, y);
                }
                // Uxy: fine vibrato — four times more accurate than Hxy.
                cmd::U_FINE_VIBRATO => {
                    Self::apply_vibrato(ch, x, y, /* fine: */ true);
                    Self::clamp_amiga(ch, amiga_limits);
                }
                _ => {}
            }

            // Re-latch the voice global volume if this tick changed the
            // note volume (§Vxx: a same-row Vxx reaches any voice whose
            // volume is updated on a later tick).
            if ch.volume != vol_before_tick {
                ch.voice_global_vol = global_vol_now;
            }
        }
    }

    /// Is the (x, y) infobyte a *fine* volume slide? (`DFy` / `DxF` / `DFF`.)
    ///
    /// Per multimedia.cx §Kxy, a fine volume-slide form in a Kxy/Lxy infobyte
    /// disables the slide *and* the dual "other" effect (H00 / G00). The
    /// slide-on-all-ticks forms `D0F` (x==0,y==F) and `DF0` (x==F,y==0) are
    /// **not** fine and must keep working, so they are excluded here.
    fn is_fine_volslide(x: u8, y: u8) -> bool {
        (x == 0xF && y != 0) || (y == 0xF && x != 0)
    }

    /// Remembered vibrato speed/depth for the H00 leg of Kxy.
    ///
    /// Kxy continues whatever vibrato the channel already had running, so the
    /// speed/depth come from the shared H/U effect-memory slot rather than
    /// Kxy's own nibbles. Returns `(speed, depth)` from `effect_memory[H]`.
    fn vibrato_memory(ch: &Channel) -> (u8, u8) {
        let info = ch.effect_memory[cmd::H_VIBRATO as usize];
        (info >> 4, info & 0x0F)
    }

    /// Hxy / Uxy / Kxy vibrato kernel. `fine = true` is the U variant
    /// (4× more accurate, i.e. depth divided by 4).
    fn apply_vibrato(ch: &mut Channel, speed: u8, depth: u8, fine: bool) {
        if speed == 0 && depth == 0 {
            return;
        }
        if ch.target_frequency <= 0.0 {
            return;
        }
        // §Playback Notes: "Vibrato and tremolo have a full cycle length of
        // 256, though Hxy and Rxy use x*4 and y*4 as their parameters." The
        // phase counter is the full 0..=255 cycle stepped by `speed * 4`
        // (wrapping at 256), and the 64-entry waveform table is sampled at
        // `phase / 4`. This `phase / 4` reproduces FireLight §6.8's signed
        // `-32..+31` pointer incremented by `speed`: e.g. speed 4 advances
        // the pointer by 1 table slot per tick, completing the full
        // oscillation in 64 ticks — NOT 16. (The earlier `(pos + speed*4) &
        // 0x3F` form masked to 6 bits *and* sampled directly, advancing the
        // pointer 4× too fast and quartering the vibrato period.)
        ch.vibrato_pos = ch.vibrato_pos.wrapping_add(speed * 4);
        let wf = ch.vibrato_waveform;
        let s = waveform_sample(wf, ch.vibrato_pos >> 2);
        let div = if fine { 512 } else { 128 };
        let delta = (s * depth as i32) / div;
        let mult = 2.0f32.powf(delta as f32 / 48.0);
        ch.frequency = ch.target_frequency * mult;
    }

    /// Rxy tremolo per-tick kernel, per the multimedia.cx behavioural
    /// reference (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
    /// §Rxy):
    ///
    /// > On tick 1 (the second tick of the row) set the active volume to
    /// > the stored volume plus (depth * value) / (max_amplitude * 2)
    /// > (Rxy peaks at 32 in each direction), and for each nonzero tick
    /// > increase the tremolo position by the speed. The stored volume is
    /// > untouched. ... Tremolo will not work if the stored volume is 0.
    ///
    /// Key consequences encoded here:
    ///
    /// - The active volume is **recomputed from the stored volume** every
    ///   tick (`stored + delta`), never accumulated onto the previous
    ///   active value — so the modulation cannot drift.
    /// - §Playback Notes gives the phase convention: "Vibrato and tremolo
    ///   have a full cycle length of 256, though Hxy and Rxy use x*4 and
    ///   y*4 as their parameters." `tremolo_pos` therefore holds the full
    ///   0..=255 phase, stepped by `speed * 4` per nonzero tick (period =
    ///   256 / (4·x) ticks); the 64-entry waveform table is sampled at
    ///   `phase / 4`.
    /// - The depth parameter is likewise `y * 4`; with our waveform
    ///   amplitude of ±64 the §Rxy formula reduces to
    ///   `delta = (4·y · value) / 128`, peaking at ±30 for `y = 0xF`
    ///   (the documented "peaks at 32" is the formula's theoretical bound).
    /// - A zero **stored** volume disables the effect entirely.
    /// - The "song speed 1 leaves the active volume untouched" rule is
    ///   structural: this kernel only runs from the per-tick path
    ///   (ticks >= 1), and a speed-1 row never reaches tick 1.
    ///
    /// The result is capped to [`PCM_VOLUME_PEAK`] per the §Playback
    /// Notes "volumes actually peak at 63" rule.
    fn apply_tremolo(ch: &mut Channel, speed: u8, depth: u8) {
        if speed == 0 && depth == 0 {
            return;
        }
        // §Rxy: "Tremolo will not work if the stored volume is 0."
        if ch.stored_volume == 0 {
            return;
        }
        ch.tremolo_pos = ch.tremolo_pos.wrapping_add(speed * 4);
        let wf = ch.tremolo_waveform;
        let s = waveform_sample(wf, ch.tremolo_pos >> 2);
        // delta = (depth * value) / (max_amplitude * 2) with depth = y*4
        // and max_amplitude = 64 (our waveform_sample range).
        let delta = (depth as i32 * 4 * s) / 128;
        let v = (ch.stored_volume as i32 + delta).clamp(0, PCM_VOLUME_PEAK as i32);
        ch.volume = v as u8;
    }

    /// Gxx / Lxy tone-portamento kernel. `step` is the info byte (units
    /// 1/64ths of a semitone). Honours the per-channel glissando flag
    /// (S1x) by snapping to the nearest semitone when the slide step
    /// would otherwise overshoot the target.
    fn apply_tone_porta(ch: &mut Channel, step: u8) {
        if step == 0 || ch.target_frequency <= 0.0 {
            return;
        }
        let s = step as f32;
        let prev = ch.frequency;
        if prev < ch.target_frequency {
            let f = 2.0f32.powf(s / 768.0);
            ch.frequency = (prev * f).min(ch.target_frequency);
        } else if prev > ch.target_frequency {
            let f = 2.0f32.powf(-s / 768.0);
            ch.frequency = (prev * f).max(ch.target_frequency);
        }
        if ch.glissando {
            // S1x: snap each step to the nearest semitone of the channel's
            // current C5 reference. We derive C5 from target_frequency by
            // assuming last_note: target = c5 * 2^((note-48)/12).
            if ch.last_note != 0 {
                let oct = (ch.last_note >> 4) as i32;
                let semi = (ch.last_note & 0x0F) as i32;
                let n = oct * 12 + semi;
                let delta = n - 48;
                let c5 = ch.target_frequency / 2.0f32.powf(delta as f32 / 12.0);
                ch.frequency = snap_to_semitone(ch.frequency, c5.round().max(1.0) as u32);
            }
        }
    }

    /// Dxy volume slide step (per-tick path).
    ///
    /// Cases per the multimedia.cx behavioural reference:
    /// - `Dx0` (x in 1..=E): slide up by x on every nonzero tick.
    /// - `D0y` (y in 1..=E): slide down by y on every nonzero tick.
    /// - `D0F`: slide down by 15 on *all* ticks (incl. tick 0). The wiki
    ///   explicitly notes this overrides the usual "fine-on-tick-0" rule.
    /// - `DF0`: slide up by 15 on *all* ticks. Same note.
    /// - `Dxy` (both 1..=E): ST3 treats as `D0y` (slide down by y) —
    ///   Impulse Tracker does nothing, ST3 doesn't. We honour ST3's path.
    /// - Fine slides (`DFy`, `DxF`, `DFF`) — tick-0 only, handled at row
    ///   entry; this function intentionally no-ops for those.
    fn apply_dxy(ch: &mut Channel, x: u8, y: u8) {
        if x == 0 && y == 0xF {
            // D0F: slide down by 15 on all ticks (also fires at tick 0
            // via row-entry — both legs add up to the spec'd amount).
            ch.volume = ch.volume.saturating_sub(15);
        } else if x == 0xF && y == 0 {
            // DF0: slide up by 15 on all ticks. Cap at [`PCM_VOLUME_PEAK`].
            ch.volume = clamp_pcm_volume(ch.volume as u16 + 15);
        } else if x == 0xF || y == 0xF {
            // DFy / DxF / DFF — fine, tick-0 only.
        } else if x != 0 && y == 0 {
            // Dx0: slide up.
            ch.volume = clamp_pcm_volume(ch.volume as u16 + x as u16);
        } else if y != 0 && x == 0 {
            // D0y: slide down.
            ch.volume = ch.volume.saturating_sub(y);
        } else if x != 0 && y != 0 {
            // Dxy with both nibbles 1..=E: ST3 quirk — slide DOWN by y.
            ch.volume = ch.volume.saturating_sub(y);
        }
    }

    /// Tick-0 leg of `Dx0` / `D0x` (continuous-slide nibbles) when the
    /// header's fast-slides flag is on.
    ///
    /// Per the multimedia.cx behavioural reference (§Dxy):
    /// "D0x, 1 <= x <= 0xE: slide down by x on all nonzero ticks. **Also
    /// slide on tick 0, if fast slides are enabled.**"  Same wording for
    /// `Dx0` (slide up). The fine forms (`DFy`, `DxF`, `DFF`) are not
    /// affected by the flag — those are handled by the existing tick-0
    /// table in `enter_row` regardless. `D0F` / `DF0` are already
    /// slide-on-all-ticks unconditionally; they also live in the existing
    /// tick-0 table, so this helper deliberately skips them.
    fn apply_dxy_tick0_fast_slide(ch: &mut Channel, x: u8, y: u8) {
        if x != 0 && y == 0 && x != 0xF {
            // Dx0 — slide up by x. Cap at [`PCM_VOLUME_PEAK`].
            ch.volume = clamp_pcm_volume(ch.volume as u16 + x as u16);
        } else if y != 0 && x == 0 && y != 0xF {
            // D0y — slide down by y.
            ch.volume = ch.volume.saturating_sub(y);
        } else if x != 0 && y != 0 && x != 0xF && y != 0xF {
            // ST3 quirk Dxy (both 1..=E): treat as D0y on the fast-slides
            // tick-0 leg too, so the behaviour matches the per-tick path.
            ch.volume = ch.volume.saturating_sub(y);
        }
        // Fine forms / D0F / DF0 / D00 fall through unchanged — they're
        // already covered by the row-entry table.
    }

    /// Qxy retrigger step (runs on EVERY tick, including tick 0).
    ///
    /// Per the multimedia.cx behavioural reference (§Qxy):
    /// - `y == 0` (retrig value) → the effect is ignored.
    /// - A per-channel counter is incremented on each tick. When it
    ///   reaches/exceeds `y`, the sample is retriggered (sample_pos → 0),
    ///   the active volume is modified by the `x` table ([`retrigger_volume`]),
    ///   and the counter resets to 0.
    /// - The counter is *not* reset by a new note carrying Qxy — only a
    ///   row without Qxy clears it (handled in `enter_row`). It is
    ///   independent of song speed and can retrig on tick 0.
    fn apply_retrigger(ch: &mut Channel) {
        let x = ch.info >> 4;
        let y = ch.info & 0x0F;
        if y == 0 {
            return;
        }
        ch.retrig_counter = ch.retrig_counter.saturating_add(1);
        if ch.retrig_counter >= y {
            ch.sample_pos = 0.0;
            ch.volume = retrigger_volume(ch.volume, x);
            ch.retrig_counter = 0;
        }
    }

    /// Ixy tremor step — applied on **every** tick (including tick 0)
    /// per the multimedia.cx behavioural reference §Ixy.
    ///
    /// Implements the two-decrementing-counter procedure verbatim from
    /// `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`:
    ///
    /// > Implemented with two decrementing counters per channel — the "on"
    /// > counter and the "off" counter. On each tick, if the "on" counter
    /// > is greater than zero, it is decremented and if it reaches zero,
    /// > the current volume is set to 0 and the "off" counter is set to
    /// > the "off" time (y + 1). If the "on" counter was zero in the
    /// > beginning of the update procedure, then the "off" counter is
    /// > decremented and if it reached zero (or became less than zero),
    /// > the current volume is set to the stored volume and the "on"
    /// > counter is set to the "on" time (x + 1).
    ///
    /// The counters live on [`Channel`] (`tremor_on_counter` /
    /// `tremor_off_counter`) and are never reset on row entry — the
    /// "playback start" / "no reset" rule from the same source — so a
    /// channel that ends a row mid-cycle continues from the same phase
    /// on the next row that carries Ixy.
    ///
    /// "The stored volume isn't modified by this effect" — restoration
    /// reads [`Channel::stored_volume`] (capped to [`PCM_VOLUME_PEAK`]).
    fn apply_tremor_step(ch: &mut Channel, x: u8, y: u8) {
        // Snapshot "on counter was zero in the beginning of the update
        // procedure" before any mutation, per the spec's wording.
        let was_on_zero = ch.tremor_on_counter == 0;
        if !was_on_zero {
            ch.tremor_on_counter -= 1;
            if ch.tremor_on_counter == 0 {
                ch.volume = 0;
                ch.tremor_off_counter = y.saturating_add(1);
            }
        } else {
            // off-counter decrement is saturating so the "reached zero
            // or became less than zero" branch fires when the counter was
            // already 0 (initial / cold-start state).
            let next_off = ch.tremor_off_counter.saturating_sub(1);
            ch.tremor_off_counter = next_off;
            if next_off == 0 {
                ch.volume = ch.stored_volume.min(PCM_VOLUME_PEAK);
                ch.tremor_on_counter = x.saturating_add(1);
            }
        }
    }

    fn advance_tick(&mut self) {
        if self.tick == 0 {
            self.enter_row();
        } else {
            self.apply_per_tick();
        }
    }

    fn next_row(&mut self) {
        // SEx pattern delay: when the counter is still positive, replay
        // the same row instead of advancing. Each replay consumes one
        // unit. A pattern jump (B/C) and pattern-loop SBx still take
        // priority, matching ST3 behaviour where those commands end the
        // delay early.
        if self.pattern_delay_remaining > 0 && self.pending_jump.is_none() {
            self.pattern_delay_remaining -= 1;
            self.replaying_for_pattern_delay = true;
            return;
        }
        // Drop any stale delay carryover (a row with no SEx clears it).
        self.pattern_delay_remaining = 0;
        self.replaying_for_pattern_delay = false;
        if let Some(jump) = self.pending_jump.take() {
            if let Some(order) = jump.order {
                self.order_index = order;
            } else {
                self.order_index = self.order_index.saturating_add(1);
            }
            self.row = jump.row;
        } else {
            self.row += 1;
            if self.row as usize >= PATTERN_ROWS {
                self.row = 0;
                self.order_index = self.order_index.saturating_add(1);
            }
        }
        if self.order_index as usize >= self.order.len() {
            self.ended = true;
        }
    }

    /// Mix one sample from one channel. Returns (left, right) in -1..=1.
    ///
    /// Muted channels (`+128` in the file's channel-settings byte, or AdLib
    /// slots we don't synthesise) still advance their sample cursor — that
    /// keeps a Q/G/H state-bearing pattern row consistent with what a real
    /// ST3 would render if you later unmute the slot mid-song — but emit
    /// silence into the bus.
    fn mix_channel(ch: &mut Channel, samples: &[SampleBody], out_rate: f32) -> (f32, f32) {
        if !ch.active || ch.frequency <= 0.0 {
            return (0.0, 0.0);
        }
        if ch.frozen {
            // SCx-frozen channel: emit silence and *do not* advance the
            // sample read cursor — per multimedia.cx §SCx, "playback is
            // temporarily frozen" until an Exx/Fxx/Gxx/Hxx/Jxx/Kxx/Lxx/
            // Uxx command (or a fresh note) thaws it. Holding sample_pos
            // lets the resume pick up exactly where the cut landed.
            return (0.0, 0.0);
        }
        if ch.muted {
            // Still advance the read cursor so loop counters / sample-end
            // detection match the unmuted path, then return silence.
            let idx = ch.instrument as usize;
            if idx > 0 && idx <= samples.len() {
                let body = &samples[idx - 1];
                // Mirror the unmuted boundary: looped samples advance up to
                // their `loop_end` window, one-shots up to the buffer length.
                let end = if body.is_looped() {
                    (body.loop_end as f64).min(body.pcm.len() as f64)
                } else {
                    body.pcm.len() as f64
                };
                if ch.sample_pos < end {
                    let step = (ch.frequency as f64) / (out_rate as f64);
                    ch.sample_pos += step;
                }
            }
            return (0.0, 0.0);
        }
        let idx = ch.instrument as usize;
        if idx == 0 || idx > samples.len() {
            return (0.0, 0.0);
        }
        let body = &samples[idx - 1];
        if body.pcm.is_empty() {
            return (0.0, 0.0);
        }

        // A looped sample wraps back to `loop_start` the instant the read
        // cursor reaches `loop_end` (the loop window is the half-open
        // `[loop_start, loop_end)`), regardless of how much PCM body sits
        // past that point. ST3 never plays the post-loop tail of a looping
        // sample — FireLight §2.10 notes FMOD even "clips samples at their
        // loop end points." So the effective end-of-playback boundary is
        // `loop_end` for a looped sample, and the full buffer length only
        // for a one-shot.
        let len = body.pcm.len() as f64;
        let looped = body.is_looped();
        let end = if looped {
            (body.loop_end as f64).min(len)
        } else {
            len
        };
        if ch.sample_pos >= end {
            if looped {
                let ls = body.loop_start as f64;
                let le = end;
                let span = le - ls;
                if span > 0.0 {
                    let over = ch.sample_pos - ls;
                    ch.sample_pos = ls + over.rem_euclid(span);
                } else {
                    ch.active = false;
                    return (0.0, 0.0);
                }
            } else {
                ch.active = false;
                return (0.0, 0.0);
            }
        }

        let i = ch.sample_pos as usize;
        let frac = (ch.sample_pos - i as f64) as f32;
        let n = body.pcm.len();
        // At the last frame before the loop boundary, the next sample the
        // linear interpolator should reach for is `loop_start` (the loop
        // wraps there), not the frame physically past `loop_end` in the
        // buffer. Key the wrap on the effective `end`, not the buffer length.
        let end_idx = end as usize;
        let next_idx = if looped {
            if i + 1 < end_idx {
                i + 1
            } else {
                body.loop_start as usize
            }
        } else if i + 1 < n {
            i + 1
        } else {
            i
        };
        let interp_channel = |buf: &[i16]| -> f32 {
            let s0 = buf[i.min(n - 1)] as f32 / 32768.0;
            let s1 = buf[next_idx.min(n - 1)] as f32 / 32768.0;
            s0 + (s1 - s0) * frac
        };
        // True stereo samples: interpolate L and R independently. Mono
        // samples collapse to a single interpolated value used for both.
        let (interp_l, interp_r) = if let Some(ref right) = body.pcm_right {
            (interp_channel(&body.pcm), interp_channel(right))
        } else {
            let m = interp_channel(&body.pcm);
            (m, m)
        };

        // PCM volumes peak at 63 per [`PCM_VOLUME_PEAK`] — every effect-
        // side writer funnels through [`clamp_pcm_volume`], and this
        // defensive read-side `min` keeps externally-constructed `Channel`
        // literals (test scaffolding, FFI handoff) from sneaking a 64
        // through. The divisor stays 64 so the resulting gain ratio
        // tracks ST3's `vol / 64` mixer math: max is 63/64 ≈ 0.984, a
        // ~0.14 dB drop from the naive 64/64 = 1.0, matching the audible
        // ceiling of an unmodified ST3 PCM channel.
        let v = (ch.volume.min(PCM_VOLUME_PEAK) as f32) / 64.0;

        // Per-voice global volume. Unlike the channel volume, this is the
        // *global* volume that was live the last time this voice's note
        // volume was written (see [`Channel::voice_global_vol`]). Applying
        // it here — rather than as a single player-wide scalar at the
        // mix-down step — is what makes a Vxx between two notes affect only
        // the voices whose volume changed after it, per the multimedia.cx
        // §Vxx rule "It does not affect past notes, that are still playing,
        // unless their volume is changed". The divisor is 64 (global volume
        // is a 0..=64 quantity), so a voice latched at the default 64 plays
        // at unity.
        let gv = (ch.voice_global_vol.min(64) as f32) / 64.0;
        let v = v * gv;

        // Advance position.
        let step = (ch.frequency as f64) / (out_rate as f64);
        ch.sample_pos += step;

        // Pan: 0 = left, 15 = right. Equal-power-ish linear split.
        // For stereo samples this weights the two source channels by
        // position; at pan=0 only the sample's left survives, at pan=15
        // only the right. Mono samples degenerate to the prior behavior
        // since interp_l == interp_r.
        let pan = (ch.pan as f32) / 15.0;
        let left = interp_l * v * (1.0 - pan);
        let right = interp_r * v * pan;
        (left, right)
    }

    fn render_one(&mut self, out: &mut [i16]) {
        let out_rate = self.sample_rate as f32;
        let mut l = 0.0f32;
        let mut r = 0.0f32;
        for ch in &mut self.channels {
            let (cl, cr) = Self::mix_channel(ch, &self.samples, out_rate);
            l += cl;
            r += cr;
        }
        // Mix-down gain. ST3's nominal master_volume is 48 (out of 127),
        // which is what F10/Setup defaults the SOUNDCFG slider to; we
        // treat that as the "neutral" 1.0 setting. Channel-count
        // compensation uses sqrt rather than linear division — typical
        // S3M content has only a few channels at their peak
        // simultaneously, so dividing by N (instead of √N) crushes the
        // perceived loudness by ~6-12 dB on big modules. Final clamp
        // catches the rare actual peak.
        //
        // Stereo `* 11/8` boost per the multimedia.cx behavioural reference
        // (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
        // §Mixing volume): "Mixing volume ... is multiplied by 11/8 when
        // stereo is on." The factor sits at the mixer step (rather than
        // baked into `self.master_volume`) so the stored value still matches
        // the raw header byte for tooling.
        let mv_raw = (self.master_volume.max(1) as f32) / 48.0;
        let mv = if self.stereo {
            mv_raw * (11.0 / 8.0)
        } else {
            mv_raw
        };
        // Global volume is applied per-voice inside `mix_channel` (each
        // voice carries the global volume latched at its last volume write,
        // per §Vxx), so it is *not* re-applied here.
        let norm = (self.active_channels as f32).max(1.0).sqrt();
        let scale = mv / norm;
        l = (l * scale).clamp(-1.0, 1.0);
        r = (r * scale).clamp(-1.0, 1.0);
        out[0] = (l * 32767.0) as i16;
        out[1] = (r * 32767.0) as i16;
    }

    /// Per-channel mixdown: writes one stereo frame per S3M channel into
    /// `out`, which must be exactly `2 * self.channels.len()` i16 wide.
    /// Layout: `[ch0_L, ch0_R, ch1_L, ch1_R, … chN_L, chN_R]`.
    ///
    /// Pan, volume, global-volume and master-volume are applied per
    /// channel identically to the mixed path, but the sqrt(active) mixing
    /// compensation is omitted — each output stream carries exactly one
    /// channel's signal, so there's nothing to compensate for.
    fn render_one_per_channel(&mut self, out: &mut [i16]) {
        let out_rate = self.sample_rate as f32;
        // Same `* 11/8` stereo mixing-volume multiplier the mixed path
        // applies — keep per-channel output bit-equivalent for a
        // single-active-channel module, so visualizer / DAW consumers see
        // exactly what the mixed path would emit before the sqrt(active)
        // normalisation step.
        let mv_raw = (self.master_volume.max(1) as f32) / 48.0;
        let mv = if self.stereo {
            mv_raw * (11.0 / 8.0)
        } else {
            mv_raw
        };
        // Global volume is applied per-voice inside `mix_channel`; not
        // re-applied here (see `render_one`).
        let scale = mv;
        for (i, ch) in self.channels.iter_mut().enumerate() {
            let (cl, cr) = Self::mix_channel(ch, &self.samples, out_rate);
            let l = (cl * scale).clamp(-1.0, 1.0);
            let r = (cr * scale).clamp(-1.0, 1.0);
            let off = i * 2;
            out[off] = (l * 32767.0) as i16;
            out[off + 1] = (r * 32767.0) as i16;
        }
    }

    /// Render up to `dst.len()/2` stereo frames.
    pub fn render(&mut self, dst: &mut [i16]) -> usize {
        assert!(dst.len() % 2 == 0);
        let mut produced = 0usize;
        let total_frames = dst.len() / 2;
        while produced < total_frames {
            if self.ended {
                break;
            }
            if self.tick_sample_cursor == 0 {
                self.advance_tick();
                if self.ended {
                    break;
                }
            }
            let spt = self.samples_per_tick().max(1);
            let remaining_in_tick = spt.saturating_sub(self.tick_sample_cursor);
            let want = (total_frames - produced).min(remaining_in_tick as usize);
            for _ in 0..want {
                let off = produced * 2;
                self.render_one(&mut dst[off..off + 2]);
                produced += 1;
            }
            self.tick_sample_cursor += want as u32;
            if self.tick_sample_cursor >= spt {
                self.tick_sample_cursor = 0;
                self.tick += 1;
                if self.tick >= self.speed {
                    self.tick = 0;
                    self.next_row();
                }
            }
        }
        produced
    }

    /// Number of channels emitted by `render_per_channel`.
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Render up to `dst.len() / (2 * self.channel_count())` frames in
    /// per-channel mode. Each frame is `2 * channel_count()` i16 wide:
    /// `[ch0_L, ch0_R, ch1_L, ch1_R, … chN_L, chN_R]`, repeated for each
    /// frame.
    ///
    /// Unlike `render`, the output is *not* mixed down: every S3M channel
    /// gets its own stereo pair (panned per the channel's pan, scaled by
    /// volume/global/master). Consumers that want individual streams
    /// (DAWs, visualizers, per-instrument remastering) deinterleave the
    /// result into `channel_count()` stereo buffers.
    ///
    /// Returns the number of frames produced.
    pub fn render_per_channel(&mut self, dst: &mut [i16]) -> usize {
        let stride = self.channels.len() * 2;
        assert!(
            stride > 0 && dst.len() % stride == 0,
            "per-channel destination must be a multiple of 2 * channel_count"
        );
        let total_frames = dst.len() / stride;
        let mut produced = 0usize;
        while produced < total_frames {
            if self.ended {
                break;
            }
            if self.tick_sample_cursor == 0 {
                self.advance_tick();
                if self.ended {
                    break;
                }
            }
            let spt = self.samples_per_tick().max(1);
            let remaining_in_tick = spt.saturating_sub(self.tick_sample_cursor);
            let want = (total_frames - produced).min(remaining_in_tick as usize);
            for _ in 0..want {
                let off = produced * stride;
                self.render_one_per_channel(&mut dst[off..off + stride]);
                produced += 1;
            }
            self.tick_sample_cursor += want as u32;
            if self.tick_sample_cursor >= spt {
                self.tick_sample_cursor = 0;
                self.tick += 1;
                if self.tick >= self.speed {
                    self.tick = 0;
                    self.next_row();
                }
            }
        }
        produced
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn note_freq_st3_c5_is_c5_speed() {
        // ST3's "C-5" = note byte 0x40 (octave nibble 4).
        let f = note_to_frequency(0x40, 8363);
        assert!((f - 8363.0).abs() < 0.5, "got {}", f);
    }

    #[test]
    fn note_freq_octave_doubles() {
        let f4 = note_to_frequency(0x40, 8363);
        let f5 = note_to_frequency(0x50, 8363);
        assert!((f5 / f4 - 2.0).abs() < 0.001);
    }

    #[test]
    fn period_table_matches_published_corners_and_octave4() {
        // The table is transcribed verbatim from the FireLight §4.2
        // "9 Octaves" listing. Spot-check the published corners and the
        // full octave-4 reference row so a transcription slip is caught.
        assert_eq!(PERIOD_TABLE[0], 27392); // octave-0 C
        assert_eq!(PERIOD_TABLE[C5_NOTE_INDEX as usize], 1712); // C-5 ref
        assert_eq!(PERIOD_TABLE[107], 56); // octave-8 B
                                           // Octave-4 row (the canonical "1712,1616,…,906" semitone ratios).
        let oct4 = [
            1712, 1616, 1524, 1440, 1356, 1280, 1208, 1140, 1076, 1016, 960, 906,
        ];
        assert_eq!(&PERIOD_TABLE[48..60], &oct4);
        // Periods are strictly decreasing from low note to high note.
        for n in 1..PERIOD_TABLE.len() {
            assert!(
                PERIOD_TABLE[n] < PERIOD_TABLE[n - 1],
                "table not monotonically decreasing at index {n}"
            );
        }
    }

    #[test]
    fn note_index_freq_uses_integer_period_truncation() {
        // Note 0x50 (index 60, octave-5 C, period 856) at C2SPD 7895:
        //   period = 8363 * 856 / 7895 = 906 (integer truncation)
        //   freq   = 14317456 / 906 ≈ 15803 Hz
        // The equal-tempered ratio 7895 * 2 = 15790 differs because ST3
        // truncates the period to an integer (FireLight §4.1).
        let f = note_index_to_frequency(60, 7895);
        let period = 8363u64 * 856 / 7895;
        let expected = AMIGA_CLOCK_HZ as f64 / period as f64;
        assert!((f as f64 - expected).abs() < 0.5, "got {f}");
        assert!(
            (f - 15790.0).abs() > 5.0,
            "integer truncation should diverge from the equal-tempered \
             approximation; got {f}"
        );
    }

    #[test]
    fn note_index_freq_clamps_above_b8() {
        // Adding arpeggio semitones past the octave-8 ceiling (index 107)
        // must clamp to B-8, not panic / index OOB. ST3 has no period for
        // notes above B-8.
        let top = note_index_to_frequency(107, 8363);
        let over = note_index_to_frequency(130, 8363);
        assert_eq!(top, over);
    }

    #[test]
    fn waveform_square_is_bipolar_constant() {
        // FireLight §6.8 `case 2: delta = 255`; the half-cycle supplies the
        // sign. 255/4 = 63 (capped to the ±64 range). First half (pos < 32)
        // → +63; second half → -63.
        assert_eq!(waveform_sample(Waveform::Square, 0), 63);
        assert_eq!(waveform_sample(Waveform::Square, 31), 63);
        assert_eq!(waveform_sample(Waveform::Square, 32), -63);
        assert_eq!(waveform_sample(Waveform::Square, 63), -63);
    }

    #[test]
    fn waveform_rampdown_matches_firelight_case1() {
        // FireLight §6.8 `case 1: temp = (idx)<<3; if(vibpos<0) temp=255-temp`.
        // Within the positive half the magnitude rises idx*8 (scaled /4);
        // the negative half mirrors it through 255 and flips the sign,
        // producing the documented descending ramp across the full cycle.
        // Positive half: 0, +16, +32, +62 at idx 0/8/16/31.
        assert_eq!(waveform_sample(Waveform::RampDown, 0), 0);
        assert_eq!(waveform_sample(Waveform::RampDown, 8), 16);
        assert_eq!(waveform_sample(Waveform::RampDown, 16), 32);
        assert_eq!(waveform_sample(Waveform::RampDown, 31), 62);
        // Negative half: -(255-idx*8)/4 → -63, -31, -1 at idx 0/16/31.
        assert_eq!(waveform_sample(Waveform::RampDown, 32), -63);
        assert_eq!(waveform_sample(Waveform::RampDown, 48), -31);
        assert_eq!(waveform_sample(Waveform::RampDown, 63), -1);
    }

    #[test]
    fn waveform_random_falls_through_to_sine() {
        // FireLight §6.8 / §6.15 `case 3: delta = sintab[temp]; // random -
        // just use sine.` — ST3's waveform selector 3 is NOT a noise LFO; it
        // reuses the sine table. Every table position must therefore match
        // the Sine waveform exactly, across both half-cycles, so a module
        // carrying `S33` / `S43` modulates pitch / volume identically to the
        // default sine rather than producing audible noise.
        for pos in 0u8..64 {
            assert_eq!(
                waveform_sample(Waveform::Random, pos),
                waveform_sample(Waveform::Sine, pos),
                "random waveform must alias the sine table at pos={pos}"
            );
        }
    }

    #[test]
    fn protracker_sine_table_matches_firelight_listing() {
        // Verbatim spot-checks of the 32-entry half-sine table from
        // docs/audio/trackers/s3m/FireLight-S3M-Player-Tutorial.txt §6.8.
        assert_eq!(PROTRACKER_SINE[0], 0);
        assert_eq!(PROTRACKER_SINE[1], 24);
        assert_eq!(PROTRACKER_SINE[8], 180);
        assert_eq!(PROTRACKER_SINE[16], 255); // peak
        assert_eq!(PROTRACKER_SINE[31], 24);
        // Symmetric about the index-16 peak: entry k == entry 32-k.
        for k in 1..16 {
            assert_eq!(
                PROTRACKER_SINE[k],
                PROTRACKER_SINE[32 - k],
                "table must be symmetric about its peak at k={k}"
            );
        }
    }

    #[test]
    fn waveform_sine_uses_protracker_table_with_signed_pointer() {
        // Positive half (pos 0..=31): +sintab[pos]/4.
        assert_eq!(waveform_sample(Waveform::Sine, 0), 0);
        // pos 16 → 255/4 = 63 (capped to the ±64 working range).
        assert_eq!(waveform_sample(Waveform::Sine, 16), 63);
        // pos 1 → 24/4 = 6.
        assert_eq!(waveform_sample(Waveform::Sine, 1), 6);
        // Negative half (pos 32..=63): -sintab[pos & 31]/4.
        assert_eq!(waveform_sample(Waveform::Sine, 32), 0);
        assert_eq!(waveform_sample(Waveform::Sine, 48), -63);
        assert_eq!(waveform_sample(Waveform::Sine, 33), -6);
    }

    #[test]
    fn snap_to_semitone_rounds_to_exact_note() {
        let c5 = 8363u32;
        // 8363 * 2^(7/12) ≈ 12544 (~G-5).  Add 2.4% drift — should snap
        // back to the exact semitone value.
        let drifted = 12544.0 * 1.005;
        let snapped = snap_to_semitone(drifted, c5);
        let exact = (c5 as f32) * 2.0f32.powf(7.0 / 12.0);
        assert!(
            (snapped - exact).abs() < 1.0,
            "snapped={snapped}, exact={exact}"
        );
    }

    #[test]
    fn q_two_thirds_table_endpoints_and_spread() {
        // Verbatim spot-checks of the TwoThirds[64] table from
        // docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html §Qxy.
        assert_eq!(Q_TWO_THIRDS[0], 0);
        assert_eq!(Q_TWO_THIRDS[1], 0);
        assert_eq!(Q_TWO_THIRDS[2], 1);
        assert_eq!(Q_TWO_THIRDS[3], 1);
        assert_eq!(Q_TWO_THIRDS[63], 39);
        // The table is monotonic non-decreasing.
        for w in Q_TWO_THIRDS.windows(2) {
            assert!(w[1] >= w[0]);
        }
    }

    #[test]
    fn retrigger_volume_modifiers_match_spec_table() {
        // §Qxy "Values for x". Use a base volume of 32 (mid-range) so the
        // subtract/add/multiply cases are all observable without clamping.
        assert_eq!(retrigger_volume(32, 0x0), 32); // no change
        assert_eq!(retrigger_volume(32, 0x1), 31); // -1
        assert_eq!(retrigger_volume(32, 0x2), 30); // -2
        assert_eq!(retrigger_volume(32, 0x3), 28); // -4
        assert_eq!(retrigger_volume(32, 0x4), 24); // -8
        assert_eq!(retrigger_volume(32, 0x5), 16); // -16
        assert_eq!(retrigger_volume(32, 0x6), Q_TWO_THIRDS[32]); // *2/3 table
        assert_eq!(retrigger_volume(32, 0x7), 16); // *1/2
        assert_eq!(retrigger_volume(32, 0x8), 32); // "?" — no change
        assert_eq!(retrigger_volume(32, 0x9), 33); // +1
        assert_eq!(retrigger_volume(32, 0xA), 34); // +2
        assert_eq!(retrigger_volume(32, 0xB), 36); // +4
        assert_eq!(retrigger_volume(32, 0xC), 40); // +8
        assert_eq!(retrigger_volume(32, 0xD), 48); // +16
        assert_eq!(retrigger_volume(32, 0xE), 48); // *3/2
        assert_eq!(retrigger_volume(32, 0xF), PCM_VOLUME_PEAK); // *2 clamped to 63
                                                                // Clamping: subtract saturates at 0; add/multiply cap at the
                                                                // PCM peak (63 per multimedia.cx §Playback Notes), NOT 64.
        assert_eq!(retrigger_volume(4, 0x5), 0);
        assert_eq!(retrigger_volume(60, 0xF), PCM_VOLUME_PEAK);
    }

    #[test]
    fn retrigger_counter_persists_and_fires_at_y() {
        // Drive apply_retrigger directly with Q05 (y=5) and confirm the
        // counter walks 1..5, fires (resets to 0, retrig sample_pos→0)
        // on the 5th call, and that it carries across calls (not reset
        // by the helper itself).
        let mut ch = Channel {
            info: 0x05,
            command: cmd::Q_RETRIGGER,
            volume: 32,
            sample_pos: 100.0,
            ..Channel::default()
        };
        for expected in 1..5 {
            PlayerState::apply_retrigger(&mut ch);
            assert_eq!(ch.retrig_counter, expected);
            assert_eq!(ch.sample_pos, 100.0, "no retrig before counter hits y");
        }
        PlayerState::apply_retrigger(&mut ch);
        assert_eq!(ch.retrig_counter, 0, "counter resets after firing");
        assert_eq!(ch.sample_pos, 0.0, "sample retriggered to start");
    }

    #[test]
    fn retrigger_y0_is_ignored() {
        let mut ch = Channel {
            info: 0x90, // Q90: y == 0
            command: cmd::Q_RETRIGGER,
            volume: 32,
            sample_pos: 50.0,
            ..Channel::default()
        };
        PlayerState::apply_retrigger(&mut ch);
        // y == 0 → effect ignored: counter untouched, no retrig.
        assert_eq!(ch.retrig_counter, 0);
        assert_eq!(ch.sample_pos, 50.0);
    }

    #[test]
    fn is_fine_volslide_matches_dxy_fine_forms() {
        // multimedia.cx §Kxy: a *fine* slide (DFy / DxF / DFF) suppresses
        // both the volume slide and the H00/G00 dual effect.
        assert!(PlayerState::is_fine_volslide(0xF, 0x3)); // DF3 (fine down 3)
        assert!(PlayerState::is_fine_volslide(0x3, 0xF)); // D3F (fine up 3)
        assert!(PlayerState::is_fine_volslide(0xF, 0xF)); // DFF (fine up 15)
                                                          // Slide-on-all-ticks forms are NOT fine and must keep running.
        assert!(!PlayerState::is_fine_volslide(0x0, 0xF)); // D0F
        assert!(!PlayerState::is_fine_volslide(0xF, 0x0)); // DF0
                                                           // Ordinary continuous slides are not fine either.
        assert!(!PlayerState::is_fine_volslide(0x2, 0x0)); // D20
        assert!(!PlayerState::is_fine_volslide(0x0, 0x4)); // D04
    }

    #[test]
    fn vibrato_memory_reads_h_slot_not_k_nibbles() {
        // Kxy's H00 leg must read the channel's remembered H speed/depth,
        // never Kxy's own (x, y) nibbles. Stash H82 in the shared H/U slot.
        let mut ch = Channel::default();
        ch.effect_memory[cmd::H_VIBRATO as usize] = 0x82; // speed 8, depth 2
        assert_eq!(PlayerState::vibrato_memory(&ch), (8, 2));
    }

    #[test]
    fn kxy_vibrato_uses_remembered_h_params() {
        // Per §Kxy ("H00 + Dxy"), a K02 continues the vibrato that an
        // earlier H82 began — so the pitch must wobble at H82's depth, not
        // K02's. Drive the vibrato kernel with the *remembered* params and
        // confirm the frequency moves away from the base.
        let mut ch = Channel {
            target_frequency: 8363.0,
            frequency: 8363.0,
            ..Channel::default()
        };
        ch.effect_memory[cmd::H_VIBRATO as usize] = 0x24; // remembered H24
        let (h_speed, h_depth) = PlayerState::vibrato_memory(&ch);
        assert_eq!((h_speed, h_depth), (2, 4));
        // Each vibrato step advances the phase by speed*4 = 8 and samples the
        // sine at phase/4. Step three ticks so the phase (24) lands at table
        // index 6 (PROTRACKER_SINE[6] = 141), where the depth-4 wobble is
        // large enough to shift the carrier off-pitch — proving the
        // remembered H depth is what drives the modulation.
        for _ in 0..3 {
            PlayerState::apply_vibrato(&mut ch, h_speed, h_depth, false);
        }
        assert_eq!(ch.vibrato_pos, 24);
        assert_ne!(
            ch.frequency, 8363.0,
            "Kxy vibrato must move pitch using the remembered H depth"
        );
        // A would-be K02-as-H02 (depth 2, speed 0) would NOT advance the
        // phase (speed 0 → no-op): confirm the depth-only path is inert so
        // the remembered-params path is demonstrably different.
        let mut ch2 = Channel {
            target_frequency: 8363.0,
            frequency: 8363.0,
            ..Channel::default()
        };
        PlayerState::apply_vibrato(&mut ch2, 0, 2, false); // K02 nibbles direct
        assert_eq!(
            ch2.frequency, 8363.0,
            "speed-0 vibrato is a no-op; proves K02 nibbles are wrong params"
        );
    }

    #[test]
    fn vibrato_phase_steps_speed_times_4_over_256_cycle() {
        // §Playback Notes: "Vibrato and tremolo have a full cycle length of
        // 256, though Hxy and Rxy use x*4 and y*4 as their parameters."
        // Vibrato must share the tremolo phase convention: speed 8 → 32
        // phase units per tick → a full cycle every 8 ticks (256 / (4·8)),
        // NOT the quartered period the old `& 0x3F` form produced.
        let mut ch = Channel {
            target_frequency: 8363.0,
            frequency: 8363.0,
            ..Channel::default()
        };
        for _ in 0..3 {
            PlayerState::apply_vibrato(&mut ch, 8, 1, false);
        }
        assert_eq!(ch.vibrato_pos, 96, "3 ticks at speed 8 → phase 96");
        for _ in 0..5 {
            PlayerState::apply_vibrato(&mut ch, 8, 1, false);
        }
        assert_eq!(ch.vibrato_pos, 0, "8 ticks wrap the full 256 cycle");
    }

    #[test]
    fn vibrato_completes_one_oscillation_in_16_ticks_at_speed_4() {
        // FireLight §6.8: the signed -32..+31 pointer is incremented by the
        // vibrato speed; the half-sine table is used twice for one full
        // oscillation (64 pointer steps). Our 8-bit phase steps by speed*4
        // and is sampled at phase/4, so the FireLight pointer advances by
        // exactly `speed` slots per tick. At speed 4 the pointer covers all
        // 64 slots in 64/4 = 16 ticks, so phase returns to 0 after 16 ticks.
        // The old `& 0x3F` form (stepping the 6-bit pointer by speed*4)
        // would have wrapped in just 4 ticks — a quartered period.
        let base = 8363.0f32;
        let mut ch = Channel {
            target_frequency: base,
            frequency: base,
            vibrato_waveform: Waveform::Sine,
            ..Channel::default()
        };
        // Tick 1: phase 0→16, sampled at phase/4 = table idx 4,
        // PROTRACKER_SINE[4] = 97 — a positive (added) modulation.
        PlayerState::apply_vibrato(&mut ch, 4, 8, false);
        assert_eq!(ch.vibrato_pos, 16);
        assert!(
            ch.frequency > base,
            "first quarter of the sine adds to the carrier"
        );
        // Into the negative half: phase 144 (pointer 36) samples table idx
        // 4 on the subtracted half (PROTRACKER_SINE[4] = 97), so the carrier
        // drops below base. From phase 16, 8 more ticks (×16) → phase 144.
        for _ in 0..8 {
            PlayerState::apply_vibrato(&mut ch, 4, 8, false);
        }
        assert_eq!(ch.vibrato_pos, 144, "9 ticks → into the negative half");
        assert!(
            ch.frequency < base,
            "the second half of the cycle subtracts from the carrier"
        );
        // Run out the remaining 7 ticks (16 total) — phase wraps to 0.
        for _ in 0..7 {
            PlayerState::apply_vibrato(&mut ch, 4, 8, false);
        }
        assert_eq!(
            ch.vibrato_pos, 0,
            "16 ticks at speed 4 complete exactly one full oscillation"
        );
    }

    #[test]
    fn lxy_porta_uses_remembered_g_rate() {
        // Lxy = "G00 + Dxy": the porta leg continues at the remembered G
        // rate. With a remembered G of 0x04 and a target above the current
        // pitch, the channel must glide up toward the target.
        let mut ch = Channel {
            frequency: 8000.0,
            target_frequency: 8363.0,
            ..Channel::default()
        };
        ch.effect_memory[cmd::G_TONE_PORTA as usize] = 0x04; // remembered G04
        let before = ch.frequency;
        let g_rate = ch.effect_memory[cmd::G_TONE_PORTA as usize];
        PlayerState::apply_tone_porta(&mut ch, g_rate);
        assert!(
            ch.frequency > before && ch.frequency <= ch.target_frequency,
            "Lxy must glide toward target using remembered G rate (got {})",
            ch.frequency
        );
        // A remembered G of 0 (no prior G) means no glide — the porta leg
        // is a genuine no-op, matching "G00 with empty memory".
        let mut ch0 = Channel {
            frequency: 8000.0,
            target_frequency: 8363.0,
            ..Channel::default()
        };
        PlayerState::apply_tone_porta(&mut ch0, 0);
        assert_eq!(ch0.frequency, 8000.0);
    }

    /// Build a one-channel player driven by an explicit list of cells for
    /// channel 0 (one cell per row). Instrument 1 is a long looping-free
    /// constant sample at C5SPD 8363 so note frequencies are predictable.
    fn porta_target_player(cells: &[Cell]) -> PlayerState {
        let mut h = synth_header();
        h.flags = 0;
        h.tracker_version = 0x1320;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.global_volume = 64;
        h.initial_speed = 6;
        h.initial_tempo = 125;
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let mut pat = Pattern::empty(1);
        for (row, cell) in cells.iter().enumerate() {
            pat.rows[row][0] = *cell;
        }
        let samples = vec![dummy_sample(4096)];
        PlayerState::new(&h, samples, vec![pat], 44_100)
    }

    #[test]
    fn out_of_range_effect_command_does_not_index_memory_out_of_bounds() {
        // The command byte is taken verbatim from the packed pattern, so a
        // corrupt module can carry any value 1..=255. ST3's effect alphabet
        // is A..Z (stored 1..=26) and the per-channel effect-memory table has
        // exactly one slot per letter. A command byte above 26 (here 0x40)
        // must be treated as "no effect" — the memory recall is skipped
        // rather than indexing the 27-entry table out of bounds. Regression
        // for a panic surfaced by the hostile-input mutation fuzzer.
        let cells = [Cell {
            note: 0x40,
            instrument: 1,
            volume: 0xFF,
            command: 0x40, // 64 — far past the Z (26) ceiling.
            info: 0x55,
        }];
        let mut p = porta_target_player(&cells);
        let mut buf = vec![0i16; 4096];
        // Must render without panicking; the out-of-range command is inert.
        let produced = p.render(&mut buf);
        assert!(produced > 0, "player should still render the note");
        // The bogus command must not have written into any memory slot.
        assert!(
            p.channels[0].effect_memory.iter().all(|&v| v == 0),
            "an out-of-range command must not populate effect memory"
        );
    }

    #[test]
    fn bare_gxx_targets_last_note_even_without_prior_porta() {
        // multimedia.cx §Gxx peculiarity: "If the current note is empty, the
        // destination note is set to the last note to show up in the channel,
        // even if it has occurred without the Gxx effect." So a plain note
        // trigger (no porta) on row 0, followed by a Gxx with NO note on a
        // later row, must set the porta target to that row-0 note.
        let note_c5 = 0x40; // ST3 C-5 → 8363 Hz at C5SPD 8363
        let cells = [
            // Row 0: plain trigger of C-5, no porta involved.
            Cell {
                note: note_c5,
                instrument: 1,
                volume: 0xFF,
                command: 0,
                info: 0,
            },
            // Row 1: bare Gxx (G02) with no note. Target must become C-5.
            Cell {
                note: 0xFF,
                instrument: 0,
                volume: 0xFF,
                command: cmd::G_TONE_PORTA,
                info: 0x02,
            },
        ];
        let mut p = porta_target_player(&cells);
        let c5_freq = note_to_frequency(note_c5, 8363);

        // Row 0 (tick 0): the note triggers. last_note records it.
        p.enter_row();
        assert_eq!(p.channels[0].last_note, note_c5);
        assert!((p.channels[0].frequency - c5_freq).abs() < 1.0);
        // Move the live pitch away so the bare-Gxx target choice is visible:
        // pretend a slide dragged the frequency down before row 1.
        p.channels[0].frequency = c5_freq * 0.5;

        // Advance to row 1.
        p.tick = 0;
        p.row = 1;
        p.enter_row();
        // The bare Gxx must have re-armed the target to the row-0 note.
        assert!(
            (p.channels[0].target_frequency - c5_freq).abs() < 1.0,
            "bare Gxx must target the last note (C-5 = {c5_freq}), got {}",
            p.channels[0].target_frequency
        );
        // And a per-tick Gxx step must now slide the (lowered) frequency UP
        // toward that target rather than sitting still.
        let before = p.channels[0].frequency;
        p.tick = 1;
        p.apply_per_tick();
        assert!(
            p.channels[0].frequency > before,
            "bare Gxx must glide toward the recalled target (before {before}, \
             after {})",
            p.channels[0].frequency
        );
    }

    #[test]
    fn bare_gxx_does_not_clear_target_on_arrival() {
        // Companion §Gxx peculiarity: "Gxx doesn't clear the target note when
        // it is reached, so any future Gxx with no note will keep sliding back
        // to this particular note." Once the slide has converged, a later bare
        // Gxx (still no note) must keep the same target instead of dropping to
        // a no-op / zero.
        let note_c5 = 0x40;
        let cells = [
            Cell {
                note: note_c5,
                instrument: 1,
                volume: 0xFF,
                command: 0,
                info: 0,
            },
            Cell {
                note: 0xFF,
                instrument: 0,
                volume: 0xFF,
                command: cmd::G_TONE_PORTA,
                info: 0x02,
            },
            Cell {
                note: 0xFF,
                instrument: 0,
                volume: 0xFF,
                command: cmd::G_TONE_PORTA,
                info: 0x02,
            },
        ];
        let mut p = porta_target_player(&cells);
        let c5_freq = note_to_frequency(note_c5, 8363);

        p.enter_row(); // row 0: trigger.
        p.tick = 0;
        p.row = 1;
        p.enter_row(); // row 1: bare Gxx targets C-5 (already there).
        p.tick = 0;
        p.row = 2;
        p.enter_row(); // row 2: another bare Gxx — target must persist.
        assert!(
            (p.channels[0].target_frequency - c5_freq).abs() < 1.0,
            "target must persist across a second bare Gxx (expected {c5_freq}, \
             got {})",
            p.channels[0].target_frequency
        );
    }

    #[test]
    fn porta_suppressed_trigger_updates_last_note() {
        // A note that triggers WITH a Gxx (so the retrigger is suppressed)
        // still "shows up in the channel" and must become `last_note`, so a
        // subsequent bare Gxx slides back to *it*, not to an older note.
        let note_c5 = 0x40; // 8363 Hz
        let note_d5 = 0x42; // two semitones up
        let cells = [
            // Row 0: plain C-5 so there is a live note for porta to glide.
            Cell {
                note: note_c5,
                instrument: 1,
                volume: 0xFF,
                command: 0,
                info: 0,
            },
            // Row 1: D-5 WITH Gxx → porta-suppressed retrigger, target = D-5.
            Cell {
                note: note_d5,
                instrument: 0,
                volume: 0xFF,
                command: cmd::G_TONE_PORTA,
                info: 0x10,
            },
            // Row 2: bare Gxx — must target D-5 (the porta-suppressed note),
            // proving `last_note` was updated on row 1.
            Cell {
                note: 0xFF,
                instrument: 0,
                volume: 0xFF,
                command: cmd::G_TONE_PORTA,
                info: 0x02,
            },
        ];
        let mut p = porta_target_player(&cells);
        let d5_freq = note_to_frequency(note_d5, 8363);

        p.enter_row(); // row 0
        assert_eq!(p.channels[0].last_note, note_c5);
        p.tick = 0;
        p.row = 1;
        p.enter_row(); // row 1: porta to D-5, retrigger suppressed.
        assert_eq!(
            p.channels[0].last_note, note_d5,
            "a porta-suppressed trigger must still update last_note"
        );
        p.tick = 0;
        p.row = 2;
        p.enter_row(); // row 2: bare Gxx targets the row-1 note.
        assert!(
            (p.channels[0].target_frequency - d5_freq).abs() < 1.0,
            "bare Gxx must target the porta-suppressed D-5 (expected {d5_freq}, \
             got {})",
            p.channels[0].target_frequency
        );
    }

    #[test]
    fn bare_gxx_with_no_prior_note_is_noop() {
        // Guard: a bare Gxx on a channel that has never played a note
        // (last_note == 0) must not touch target_frequency — there is no
        // "last note to show up" to slide toward.
        let cells = [Cell {
            note: 0xFF,
            instrument: 0,
            volume: 0xFF,
            command: cmd::G_TONE_PORTA,
            info: 0x02,
        }];
        let mut p = porta_target_player(&cells);
        p.enter_row();
        assert_eq!(p.channels[0].last_note, 0);
        assert_eq!(p.channels[0].target_frequency, 0.0);
    }

    #[test]
    fn s2x_table_endpoints_match_spec() {
        // From docs/audio/trackers/s3m/ScreamTracker-v3.20-effects.txt §S2x:
        //  S20 → 7895 Hz, S28 → 8363 Hz (no finetune), S2F → 8757 Hz.
        assert_eq!(S2X_FINETUNE_TABLE[0x0], 7895);
        assert_eq!(S2X_FINETUNE_TABLE[0x8], 8363);
        assert_eq!(S2X_FINETUNE_TABLE[0xF], 8757);
    }

    /// Build a one-channel player whose single channel is in the chosen
    /// stereo bank (false = left slot 0, true = right slot 8). Cells drive
    /// channel 0, one cell per row.
    fn banked_sax_player(right_bank: bool, cells: &[Cell]) -> PlayerState {
        let mut h = synth_header();
        h.flags = 0;
        h.tracker_version = 0x1320;
        h.channels = [0xFFu8; 32];
        // Channel-settings low nibble selects the bank: 0 = L1, 8 = R1.
        h.channels[0] = if right_bank { 8 } else { 0 };
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.global_volume = 64;
        h.initial_speed = 6;
        h.initial_tempo = 125;
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let mut pat = Pattern::empty(cells.len().max(1));
        for (row, cell) in cells.iter().enumerate() {
            pat.rows[row][0] = *cell;
        }
        let samples = vec![dummy_sample(4096)];
        PlayerState::new(&h, samples, vec![pat], 44_100)
    }

    /// SAx ("old stereo control") is bank-dependent per the multimedia.cx
    /// behavioural reference §SAx: SA0/SA2 "Normal panning (L is left, R is
    /// right)", SA1/SA3 "Reversed". A right-bank channel must pan hard-right
    /// under a normal SA0 and hard-left under a reversed SA1 — the exact
    /// mirror of a left-bank channel under the same parameters.
    #[test]
    fn sax_normal_and_reversed_are_bank_dependent() {
        let note = Cell {
            note: 0x40,
            instrument: 1,
            volume: 0xFF,
            command: cmd::S_EXTENDED,
            info: 0xA0, // SA0 — normal
        };
        // Right-bank, normal → hard right (15).
        let mut pr = banked_sax_player(true, &[note]);
        assert!(pr.channels[0].right_bank);
        pr.enter_row();
        assert_eq!(
            pr.channels[0].pan, 15,
            "SA0 (normal) on a right-bank channel must pan hard-right"
        );

        // Left-bank, normal → hard left (0).
        let mut pl = banked_sax_player(false, &[note]);
        assert!(!pl.channels[0].right_bank);
        pl.enter_row();
        assert_eq!(
            pl.channels[0].pan, 0,
            "SA0 (normal) on a left-bank channel must pan hard-left"
        );

        // Right-bank, reversed (SA1) → hard left (0).
        let rev = Cell { info: 0xA1, ..note };
        let mut prr = banked_sax_player(true, &[rev]);
        prr.enter_row();
        assert_eq!(
            prr.channels[0].pan, 0,
            "SA1 (reversed) on a right-bank channel must pan hard-left"
        );
    }

    /// SAx center group (SA4..SA7) lands on the centre slot and the no-op
    /// group (SA8..SAF) leaves the pan untouched, per multimedia.cx §SAx.
    #[test]
    fn sax_center_and_noop_groups() {
        let base = Cell {
            note: 0x40,
            instrument: 1,
            volume: 0xFF,
            command: cmd::S_EXTENDED,
            info: 0xA5, // SA5 — center
        };
        let mut pc = banked_sax_player(true, &[base]);
        // Right-bank default pan is 8 (our synth fills pans with 8).
        pc.enter_row();
        assert_eq!(
            pc.channels[0].pan, 7,
            "SA5 (center) must land on the centre pan slot"
        );

        // SAC (no effect) — pan must stay at the channel's prior value.
        let noop = Cell { info: 0xAC, ..base };
        let mut pn = banked_sax_player(true, &[noop]);
        let before = pn.channels[0].pan;
        pn.enter_row();
        assert_eq!(
            pn.channels[0].pan, before,
            "SAC (8..F group) is a documented no-op and must not change pan"
        );
    }

    /// Build a `SampleBody` of `len` constant-value PCM frames so the
    /// mixer has something non-zero to read when the channel is unmuted.
    fn dummy_sample(len: usize) -> SampleBody {
        SampleBody {
            pcm: vec![16_000i16; len],
            pcm_right: None,
            loop_start: 0,
            loop_end: 0,
            looped: false,
            volume: 64,
            c5_speed: 8363,
        }
    }

    #[test]
    fn muted_channel_emits_silence_in_mixer() {
        // A muted channel with an active voice + non-zero volume must
        // produce (0.0, 0.0) per mix step — never any audio. Confirm
        // both branches of the mute flag (unmuted = audio, muted = silence)
        // with the same sample / volume / frequency setup so the mute is
        // the only variable.
        let samples = vec![dummy_sample(64)];
        let mut unmuted = Channel {
            instrument: 1,
            frequency: 8363.0,
            sample_pos: 0.0,
            volume: 64,
            pan: 8,
            active: true,
            target_frequency: 8363.0,
            ..Channel::default()
        };
        let mut muted = unmuted.clone();
        muted.muted = true;
        let (l1, r1) = PlayerState::mix_channel(&mut unmuted, &samples, 44_100.0);
        let (l2, r2) = PlayerState::mix_channel(&mut muted, &samples, 44_100.0);
        assert!(l1.abs() + r1.abs() > 0.01, "unmuted must emit audio");
        assert_eq!((l2, r2), (0.0, 0.0), "muted must emit silence");
    }

    #[test]
    fn muted_channel_still_advances_sample_position() {
        // A muted channel must keep its read cursor consistent with the
        // unmuted equivalent so unmuting mid-song picks up at the right
        // sample — not at offset 0. Drive one mix tick on each, confirm
        // both ended at the same `sample_pos`.
        let samples = vec![dummy_sample(1024)];
        let mut unmuted = Channel {
            instrument: 1,
            frequency: 8363.0,
            sample_pos: 0.0,
            volume: 64,
            active: true,
            target_frequency: 8363.0,
            ..Channel::default()
        };
        let mut muted = unmuted.clone();
        muted.muted = true;
        for _ in 0..200 {
            PlayerState::mix_channel(&mut unmuted, &samples, 44_100.0);
            PlayerState::mix_channel(&mut muted, &samples, 44_100.0);
        }
        // Same step size on both branches, so cursors must match exactly.
        assert_eq!(
            muted.sample_pos, unmuted.sample_pos,
            "muted cursor must advance like the unmuted equivalent"
        );
        assert!(
            muted.sample_pos > 0.0,
            "muted cursor must move off zero (got {})",
            muted.sample_pos
        );
    }

    #[test]
    fn muted_channel_silent_when_voice_inactive() {
        // Symmetric sanity: muted + inactive (no current note) must also
        // emit (0,0) and not advance anything.
        let samples = vec![dummy_sample(64)];
        let mut ch = Channel {
            instrument: 1,
            frequency: 0.0,
            active: false,
            muted: true,
            ..Channel::default()
        };
        let (l, r) = PlayerState::mix_channel(&mut ch, &samples, 44_100.0);
        assert_eq!((l, r), (0.0, 0.0));
        assert_eq!(ch.sample_pos, 0.0);
    }

    #[test]
    fn frozen_channel_emits_silence_and_holds_position() {
        // Per multimedia.cx §SCx, SCx freezes playback: the mixer emits
        // silence AND the sample read cursor does not advance, so a
        // later resume picks up exactly where the cut landed. Symmetric
        // to the unfrozen baseline: same instrument / frequency / volume.
        let samples = vec![dummy_sample(1024)];
        let mut thawed = Channel {
            instrument: 1,
            frequency: 8363.0,
            sample_pos: 100.0,
            volume: 64,
            pan: 8,
            active: true,
            target_frequency: 8363.0,
            ..Channel::default()
        };
        let mut frozen = thawed.clone();
        frozen.frozen = true;
        for _ in 0..200 {
            PlayerState::mix_channel(&mut thawed, &samples, 44_100.0);
            PlayerState::mix_channel(&mut frozen, &samples, 44_100.0);
        }
        // The thawed branch must have advanced past the starting cursor.
        assert!(
            thawed.sample_pos > 100.0,
            "thawed cursor must move (got {})",
            thawed.sample_pos
        );
        // The frozen branch must have produced silence AND not advanced.
        // We re-mix one more step on each and check the frozen output.
        let (fl, fr) = PlayerState::mix_channel(&mut frozen, &samples, 44_100.0);
        assert_eq!((fl, fr), (0.0, 0.0), "frozen must emit silence");
        assert_eq!(
            frozen.sample_pos, 100.0,
            "frozen cursor must hold its starting value (got {})",
            frozen.sample_pos
        );
        // Channel state is preserved so a resume can take over.
        assert_eq!(frozen.volume, 64);
        assert!(frozen.active);
        assert_eq!(frozen.frequency, 8363.0);
    }

    #[test]
    fn looped_sample_wraps_at_loop_end_not_buffer_length() {
        // A looping sample whose loop window `[loop_start, loop_end)` ends
        // *before* the physical PCM buffer must wrap back to `loop_start`
        // the moment the read cursor reaches `loop_end` — the post-loop
        // tail of the buffer is never played. FireLight §2.10: ST3 (and
        // FMOD) "clip samples at their loop end points." Here loop_end is 8
        // but the buffer is 16 frames long; the trailing 8 frames carry a
        // poison value the mixer must never reach.
        let mut pcm = vec![1_000i16; 8];
        pcm.extend(std::iter::repeat(-30_000i16).take(8)); // post-loop tail
        let sample = SampleBody {
            pcm,
            pcm_right: None,
            loop_start: 2,
            loop_end: 8,
            looped: true,
            volume: 64,
            c5_speed: 8363,
        };
        let samples = vec![sample];
        let mut ch = Channel {
            instrument: 1,
            // Step the cursor one PCM frame per output frame so the loop is
            // exercised quickly (out_rate == frequency).
            frequency: 44_100.0,
            sample_pos: 0.0,
            volume: 64,
            pan: 8,
            active: true,
            target_frequency: 44_100.0,
            ..Channel::default()
        };
        for _ in 0..200 {
            let (l, r) = PlayerState::mix_channel(&mut ch, &samples, 44_100.0);
            // The poison tail (-30_000) would drive |sample| ~0.9; the loop
            // window value (+1_000) sits near 0.03. If we ever read the tail
            // the magnitude jumps well past the loop-window level.
            assert!(
                l.abs() < 0.2 && r.abs() < 0.2,
                "mixer read past loop_end into the post-loop tail (l={l}, r={r})"
            );
            // The cursor may momentarily land exactly on `loop_end` after a
            // step (the wrap fires at the *start* of the next mix call,
            // before any read), but it must never advance past it — the
            // frame at `loop_end` and beyond is the poison tail.
            assert!(
                ch.sample_pos <= 8.0,
                "cursor walked past loop_end (got {})",
                ch.sample_pos
            );
        }
        // And the channel must still be playing — a looped voice never ends.
        assert!(ch.active, "looped voice must not deactivate");
    }

    #[test]
    fn looped_sample_interp_next_frame_wraps_to_loop_start() {
        // At the last frame before `loop_end`, the linear interpolator's
        // "next" sample must be `loop_start`, not the frame physically past
        // loop_end in the buffer. Construct a sample where the frame at
        // loop_end-1 and the frame at loop_end differ sharply, but the
        // loop_start frame matches loop_end-1 — so a correct wrap keeps the
        // interpolated value flat while a buggy read past loop_end dips.
        let pcm = vec![
            0i16,   // 0
            8_000,  // 1 = loop_start
            8_000,  // 2
            8_000,  // 3 = loop_end - 1 (last in-window frame)
            -8_000, // 4 = loop_end (post-loop poison — must not be reached)
            -8_000, // 5
        ];
        let sample = SampleBody {
            pcm,
            pcm_right: None,
            loop_start: 1,
            loop_end: 4,
            looped: true,
            volume: 64,
            c5_speed: 8363,
        };
        let samples = vec![sample];
        // Park the cursor mid-way between frame 3 (loop_end-1) and the wrap.
        let mut ch = Channel {
            instrument: 1,
            frequency: 44_100.0,
            sample_pos: 3.5,
            volume: 64,
            pan: 8,
            active: true,
            target_frequency: 44_100.0,
            ..Channel::default()
        };
        let (l, _r) = PlayerState::mix_channel(&mut ch, &samples, 44_100.0);
        // s0 = frame 3 (+8_000), s1 = loop_start frame 1 (+8_000): the
        // interpolation stays at +8_000 (≈0.24 after volume/pan). A buggy
        // read of frame 4 (-8_000) would land near the midpoint ≈0.0.
        // Correct wrap: ≈ +0.11. Buggy read of frame 4 (-8_000) at frac 0.5
        // collapses the interpolation to ≈ 0.0. The 0.05 gate cleanly
        // separates the two outcomes.
        assert!(
            l > 0.05,
            "interp at loop boundary read past loop_end instead of wrapping to loop_start (l={l})"
        );
    }

    /// Build a minimal-but-valid S3mHeader purely in-memory so the
    /// `PlayerState::new` flag-derivation logic can be exercised
    /// without round-tripping a full byte fixture for every variation.
    /// All array fields are zeroed; only the four fields the test cares
    /// about (flags, tracker_version, initial_speed, initial_tempo) are
    /// expected to be overridden by the caller.
    fn synth_header() -> S3mHeader {
        S3mHeader {
            song_name: String::new(),
            ord_num: 0,
            ins_num: 0,
            pat_num: 0,
            flags: 0,
            tracker_version: 0x1320,
            ffi: 2,
            global_volume: 64,
            initial_speed: 6,
            initial_tempo: 125,
            master_volume: 0x30,
            stereo: true,
            default_pan_flag: 0,
            channels: [0xFFu8; 32],
            pans: [8u8; 32],
            muted: [true; 32],
            order: vec![0xFF],
            instruments: Vec::new(),
            pattern_offsets: Vec::new(),
            enabled_channels: 1,
        }
    }

    #[test]
    fn header_flag_bit_6_enables_fast_slides() {
        // Per `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
        // §Flags bit 6: "ST3.00 volume slides (automatically enabled if
        // tracker version is == 0x1300) — if enabled, all volume slides
        // occur every tick." Explicit bit 6 set with a modern CwtV
        // (0x1320) must arm `fast_slides`.
        let mut h = synth_header();
        h.flags = 1 << 6;
        h.tracker_version = 0x1320;
        let player = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert!(player.fast_slides);
        assert!(!player.amiga_limits);
    }

    #[test]
    fn header_tracker_version_0x1300_enables_fast_slides_automatically() {
        // Same source: the bit is "automatically enabled if tracker
        // version is == 0x1300". So a file with bit 6 *clear* but
        // CwtV == 0x1300 still gets fast slides.
        let mut h = synth_header();
        h.flags = 0;
        h.tracker_version = 0x1300;
        let player = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert!(player.fast_slides, "CwtV 0x1300 must auto-arm fast slides");
        // A neighbouring version (0x1301) must NOT auto-arm.
        let mut h2 = synth_header();
        h2.flags = 0;
        h2.tracker_version = 0x1301;
        let p2 = PlayerState::new(&h2, Vec::new(), Vec::new(), 44_100);
        assert!(!p2.fast_slides, "CwtV != 0x1300 must leave the flag off");
    }

    #[test]
    fn header_pre_st3_00_version_below_0x1300_auto_arms_fast_slides() {
        // The §Dxy form of the rule in `multimedia-cx-scream-tracker-3.html`
        // ("if fast slides are enabled ... or the version is <= 0x1300") is
        // broader than the §Flags `== 0x1300` form: an earlier Scream Tracker
        // family build whose CwtV is below 0x1300 (e.g. a 0x12xx beta) also
        // runs the per-tick path. The previous `is_st3_00()`-only arming
        // missed these; `auto_fast_slides()` covers them.
        let mut h = synth_header();
        h.flags = 0;
        h.tracker_version = 0x12FF;
        let player = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert!(
            player.fast_slides,
            "Scream Tracker CwtV 0x12FF (<= 0x1300) must auto-arm fast slides"
        );

        // A non-Scream-Tracker writer whose raw word happens to fall below
        // 0x1300 (top nibble 0x0) must NOT be misclassified — the bound is
        // gated on the Scream Tracker family.
        let mut h2 = synth_header();
        h2.flags = 0;
        h2.tracker_version = 0x0ABC;
        let p2 = PlayerState::new(&h2, Vec::new(), Vec::new(), 44_100);
        assert!(
            !p2.fast_slides,
            "non-Scream-Tracker CwtV 0x0ABC must not auto-arm despite < 0x1300"
        );
    }

    #[test]
    fn header_flag_bit_4_enables_amiga_limits() {
        // Same source §Flags bit 4: "Amiga limits (limit periods to
        // confine to 113 <= x <= 856)".
        let mut h = synth_header();
        h.flags = 1 << 4;
        let player = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert!(player.amiga_limits);
        assert!(!player.fast_slides);
    }

    #[test]
    fn initial_speed_0xff_falls_back_to_default() {
        // multimedia.cx "Initial speed ... if 0 *or 255*, it is ignored
        // and the previous value used when you loaded the song is used
        // instead." A fresh-load player has no previous song, so the
        // built-in DEFAULT_SPEED stands in.
        let mut h = synth_header();
        h.initial_speed = 0xFF;
        let p = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert_eq!(p.speed, DEFAULT_SPEED);
        // The existing speed==0 path also still falls back.
        let mut h2 = synth_header();
        h2.initial_speed = 0;
        let p2 = PlayerState::new(&h2, Vec::new(), Vec::new(), 44_100);
        assert_eq!(p2.speed, DEFAULT_SPEED);
        // Any value in 1..=254 (other than 0/255) is taken as-is.
        let mut h3 = synth_header();
        h3.initial_speed = 9;
        let p3 = PlayerState::new(&h3, Vec::new(), Vec::new(), 44_100);
        assert_eq!(p3.speed, 9);
    }

    #[test]
    fn initial_tempo_below_33_falls_back_to_default() {
        // multimedia.cx "Initial tempo - if less than 33, it is ignored
        // and the previous value used when you loaded the song is used
        // instead." Mirrors the Txx tick-0 guard (`ch.info >= 0x20`).
        for bad in [0u8, 1, 16, 32] {
            let mut h = synth_header();
            h.initial_tempo = bad;
            let p = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
            assert_eq!(
                p.bpm, DEFAULT_BPM,
                "tempo {bad} (< 33) must fall back to default"
            );
        }
        // 33 itself is the first accepted value.
        let mut h = synth_header();
        h.initial_tempo = 33;
        let p = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert_eq!(p.bpm, 33);
    }

    #[test]
    fn fast_slides_dx0_fires_on_tick_0() {
        // When fast slides are on, `Dx0` (continuous up by x) must add
        // x at tick 0 too — that's the multimedia.cx "Also slide on
        // tick 0, if fast slides are enabled" rule applied to Dx0.
        // The helper covers the tick-0 leg in isolation.
        let mut ch = Channel {
            volume: 30,
            ..Channel::default()
        };
        // D40 — x=4, y=0.
        PlayerState::apply_dxy_tick0_fast_slide(&mut ch, 0x4, 0x0);
        assert_eq!(ch.volume, 34);
    }

    #[test]
    fn fast_slides_d0y_fires_on_tick_0() {
        // Mirror of the above for `D0y`: continuous slide *down* by y
        // on every nonzero tick (the standard path) PLUS tick 0 when
        // fast slides are on. D03 → -3 at tick 0.
        let mut ch = Channel {
            volume: 30,
            ..Channel::default()
        };
        PlayerState::apply_dxy_tick0_fast_slide(&mut ch, 0x0, 0x3);
        assert_eq!(ch.volume, 27);
    }

    #[test]
    fn fast_slides_tick0_leg_skips_fine_forms_and_d0f_df0() {
        // The fast-slides tick-0 leg must NOT touch DFx / DxF / DFF
        // (those are fine, tick-0-only, handled in the row-entry
        // table) and must NOT double-fire D0F / DF0 (those are
        // already unconditional slide-on-all-ticks). The wiki §Dxy:
        // "unless we're doing a fineslide, we slide on all ticks."
        // and "D0F slides down 15 on all ticks ... DF0 slides up 15
        // on all ticks. Not affected at all by the fast slides flag."
        for (x, y) in [
            (0xF, 0x3), // DF3 — fine down
            (0x3, 0xF), // D3F — fine up
            (0xF, 0xF), // DFF — fine up by 15
            (0x0, 0xF), // D0F — already slide-all-ticks, no fast-slide leg
            (0xF, 0x0), // DF0 — same
            (0x0, 0x0), // D00 — memory lookup, no slide
        ] {
            let mut ch = Channel {
                volume: 30,
                ..Channel::default()
            };
            PlayerState::apply_dxy_tick0_fast_slide(&mut ch, x, y);
            assert_eq!(
                ch.volume, 30,
                "fast-slides tick-0 leg must not act on D{x:X}{y:X}"
            );
        }
    }

    #[test]
    fn fast_slides_st3_quirk_dxy_treats_as_down() {
        // Dxy with both nibbles in 1..=E: ST3 treats it as D0y (slide
        // down by y) on the standard per-tick path. The fast-slides
        // tick-0 leg must mirror that so the row-cycle effect is
        // consistent. D34 → -4 at tick 0.
        let mut ch = Channel {
            volume: 30,
            ..Channel::default()
        };
        PlayerState::apply_dxy_tick0_fast_slide(&mut ch, 0x3, 0x4);
        assert_eq!(ch.volume, 26);
    }

    #[test]
    fn amiga_clamp_low_floor_is_a4_period_856() {
        // Period 856 → frequency AMIGA_CLOCK_HZ / 856 ≈ 16725 Hz.
        // A note carrying a sub-floor frequency must be lifted to the
        // floor and the target tracked.
        let mut ch = Channel {
            frequency: 8000.0,
            target_frequency: 8000.0,
            ..Channel::default()
        };
        PlayerState::clamp_amiga(&mut ch, true);
        let expected = (AMIGA_CLOCK_HZ as f32) / (AMIGA_LIMIT_PERIOD_MAX as f32);
        assert!(
            (ch.frequency - expected).abs() < 0.01,
            "frequency {:?} must be clamped up to {expected}",
            ch.frequency
        );
        assert!(
            (ch.target_frequency - expected).abs() < 0.01,
            "target_frequency {:?} must be clamped up to {expected}",
            ch.target_frequency
        );
    }

    #[test]
    fn amiga_clamp_high_ceiling_is_period_113() {
        // Period 113 → ≈126703 Hz. A super-bright sample (250k Hz)
        // must be brought down to the ceiling, with the target
        // tracked the same way.
        let mut ch = Channel {
            frequency: 250_000.0,
            target_frequency: 250_000.0,
            ..Channel::default()
        };
        PlayerState::clamp_amiga(&mut ch, true);
        let expected = (AMIGA_CLOCK_HZ as f32) / (AMIGA_LIMIT_PERIOD_MIN as f32);
        assert!(
            (ch.frequency - expected).abs() < 0.5,
            "frequency {:?} must be clamped down to {expected}",
            ch.frequency
        );
        assert!(
            (ch.target_frequency - expected).abs() < 0.5,
            "target_frequency {:?} must be clamped down to {expected}",
            ch.target_frequency
        );
    }

    #[test]
    fn amiga_clamp_is_a_noop_when_disabled() {
        // The flag must gate the clamp completely — modules that
        // don't ask for Amiga limits keep their full pitch range.
        let mut ch = Channel {
            frequency: 8000.0,
            target_frequency: 8000.0,
            ..Channel::default()
        };
        PlayerState::clamp_amiga(&mut ch, false);
        assert_eq!(ch.frequency, 8000.0);
        assert_eq!(ch.target_frequency, 8000.0);
    }

    #[test]
    fn amiga_clamp_preserves_legal_frequencies() {
        // A frequency already in the legal window must pass through
        // unchanged so the clamp can't introduce inaudible drift on
        // modules that fit naturally inside the Amiga range.
        let mid = ((AMIGA_CLOCK_HZ as f32) / (AMIGA_LIMIT_PERIOD_MAX as f32)
            + (AMIGA_CLOCK_HZ as f32) / (AMIGA_LIMIT_PERIOD_MIN as f32))
            / 2.0;
        let mut ch = Channel {
            frequency: mid,
            target_frequency: mid,
            ..Channel::default()
        };
        PlayerState::clamp_amiga(&mut ch, true);
        assert_eq!(ch.frequency, mid);
        assert_eq!(ch.target_frequency, mid);
    }

    #[test]
    fn amiga_clamp_skips_zero_frequencies() {
        // A silent / inactive channel (`frequency == 0.0`) must NOT
        // be lifted to the floor — that would create phantom audio.
        let mut ch = Channel {
            frequency: 0.0,
            target_frequency: 0.0,
            ..Channel::default()
        };
        PlayerState::clamp_amiga(&mut ch, true);
        assert_eq!(ch.frequency, 0.0);
        assert_eq!(ch.target_frequency, 0.0);
    }

    #[test]
    fn is_scx_resume_command_covers_the_eight_resume_effects() {
        // The wiki §SCx lists exactly E, F, G, H, J, K, L, U as the
        // commands that thaw a frozen channel. Any other ST3 command
        // does not thaw.
        for c in [
            cmd::E_SLIDE_DOWN,
            cmd::F_SLIDE_UP,
            cmd::G_TONE_PORTA,
            cmd::H_VIBRATO,
            cmd::J_ARPEGGIO,
            cmd::K_VIB_VOL,
            cmd::L_PORT_VOL,
            cmd::U_FINE_VIBRATO,
        ] {
            assert!(
                is_scx_resume_command(c),
                "command {c} must thaw an SCx-frozen channel"
            );
        }
        // Commands that explicitly DO NOT thaw — including the ones the
        // wiki omits: A, B, C, D, I, O, Q, R, S, T, V, X.
        for c in [
            0u8, // no command
            cmd::A_SET_SPEED,
            cmd::B_POS_JUMP,
            cmd::C_PAT_BREAK,
            cmd::D_VOL_SLIDE,
            cmd::I_TREMOR,
            cmd::O_SAMPLE_OFFSET,
            cmd::Q_RETRIGGER,
            cmd::R_TREMOLO,
            cmd::S_EXTENDED,
            cmd::T_SET_TEMPO,
            cmd::V_GLOBAL_VOL,
            cmd::X_SET_PAN,
        ] {
            assert!(
                !is_scx_resume_command(c),
                "command {c} must NOT thaw a frozen channel"
            );
        }
    }

    /// Build a 1-channel, 1-pattern player whose row 0 carries a Vxx with
    /// the supplied parameter byte and whose row 1 is empty. Speed is
    /// caller-controlled so the speed-1 branch of §Vxx can be exercised
    /// alongside the standard speed-6 case.
    ///
    /// Used by the §Vxx tests below to drive `enter_row` / `apply_per_tick`
    /// without standing up a full byte fixture for each scenario.
    fn vxx_test_player(vxx_param: u8, speed: u8) -> PlayerState {
        let mut h = synth_header();
        h.flags = 0;
        h.tracker_version = 0x1320;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.global_volume = 64;
        h.initial_speed = speed;
        h.initial_tempo = 125;
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let mut pat = Pattern::empty(1);
        pat.rows[0][0] = Cell {
            note: 0xFF,
            instrument: 0,
            volume: 0xFF,
            command: cmd::V_GLOBAL_VOL,
            info: vxx_param,
        };
        // Row 1 carries no command so the per-row clear path is exercised.
        pat.rows[1][0] = Cell::EMPTY;
        PlayerState::new(&h, Vec::new(), vec![pat], 44_100)
    }

    #[test]
    fn vxx_param_above_0x40_is_ignored() {
        // multimedia.cx §Vxx: "Vxx with parameter values higher than 0x40
        // are ignored." The whole effect is dropped — neither tick 0 nor
        // tick 1 may touch `global_volume`, and the prior value (64) must
        // remain intact.
        let mut p = vxx_test_player(0x41, 6);
        assert_eq!(p.global_volume, 64);
        p.enter_row();
        // Tick-0 stash MUST stay empty for an out-of-range parameter.
        assert!(p.pending_global_vol.is_none());
        for t in 1..p.speed {
            p.tick = t;
            p.apply_per_tick();
        }
        assert_eq!(
            p.global_volume, 64,
            "V41 must be ignored — global volume must not move"
        );
    }

    #[test]
    fn vxx_param_0xff_is_ignored() {
        // Boundary check for the > 0x40 rule: a V-effect with infobyte
        // 0xFF (the highest 8-bit value) must also be dropped completely.
        let mut p = vxx_test_player(0xFF, 6);
        p.enter_row();
        assert!(p.pending_global_vol.is_none());
        for t in 1..p.speed {
            p.tick = t;
            p.apply_per_tick();
        }
        assert_eq!(p.global_volume, 64);
    }

    #[test]
    fn vxx_param_0x40_is_the_upper_boundary() {
        // §Vxx says values *higher* than 0x40 are ignored, so V40 itself
        // is accepted — it sets the global volume to the documented
        // maximum (64). This is the row-line boundary; V41 (test above)
        // must be the first dropped value.
        let mut p = vxx_test_player(0x40, 6);
        p.global_volume = 30; // visibly different from 0x40 so we can see the assignment
        p.enter_row();
        assert_eq!(p.pending_global_vol, Some(0x40));
        // Tick 1 drains the stash.
        p.tick = 1;
        p.apply_per_tick();
        assert_eq!(p.global_volume, 0x40);
        assert!(p.pending_global_vol.is_none());
    }

    #[test]
    fn vxx_applies_on_tick_1_not_tick_0() {
        // §Vxx: "actually processed on tick 1". After `enter_row`
        // (tick 0), the player's `global_volume` MUST still hold the
        // previous value, but the stash MUST carry the new value. The
        // first per-tick call (tick 1) drains the stash and updates
        // `global_volume`.
        let mut p = vxx_test_player(0x20, 6);
        p.global_volume = 64;
        p.enter_row();
        assert_eq!(
            p.global_volume, 64,
            "Vxx must NOT affect tick 0 — same-row notes see the old value"
        );
        assert_eq!(p.pending_global_vol, Some(0x20));
        p.tick = 1;
        p.apply_per_tick();
        assert_eq!(p.global_volume, 0x20, "Vxx must take effect on tick 1");
        assert!(
            p.pending_global_vol.is_none(),
            "pending stash must be drained after tick 1"
        );
    }

    #[test]
    fn vxx_does_nothing_when_speed_is_1() {
        // §Vxx: "The effect doesn't do anything, if the current speed
        // is 1." With speed 1, tick 1 never fires before the row
        // advances; the stash must be discarded on the next row entry
        // so a later Vxx-free row doesn't silently inherit it.
        let mut p = vxx_test_player(0x20, 1);
        p.global_volume = 64;
        // Row 0: stash set on tick 0.
        p.enter_row();
        assert_eq!(p.pending_global_vol, Some(0x20));
        // Speed == 1: the row immediately advances to row 1 without
        // running per-tick. The mixer's row dispatcher would call
        // `enter_row` for row 1 here, which drops the stash.
        p.row = 1;
        p.enter_row();
        assert!(
            p.pending_global_vol.is_none(),
            "speed-1 row must drop the stash without applying"
        );
        assert_eq!(
            p.global_volume, 64,
            "speed-1 row must NOT update global_volume"
        );
    }

    #[test]
    fn master_volume_clamped_to_spec_range_16_127() {
        // multimedia.cx §Mixing volume: "Mixing volume (range 16 <= x <=
        // 127)". The constructor must lift sub-16 values to 16 (the
        // documented floor) and ceiling-clamp anything above 127.
        let mut h = synth_header();
        // Below-floor: anything in 0..=15 maps to 16.
        h.master_volume = 0;
        let p = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert_eq!(p.master_volume, 16, "MV=0 must clamp up to spec floor 16");

        h.master_volume = 15;
        let p = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert_eq!(p.master_volume, 16, "MV=15 must clamp up to spec floor 16");

        // In-range values pass through.
        h.master_volume = 16;
        let p = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert_eq!(p.master_volume, 16);

        h.master_volume = 48; // ST3 SOUNDCFG default
        let p = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert_eq!(p.master_volume, 48);

        h.master_volume = 127;
        let p = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert_eq!(p.master_volume, 127);
    }

    #[test]
    fn stereo_flag_mirrored_into_player_state() {
        // The stereo flag (bit 7 of the raw master-volume byte) must reach
        // the player so the mixer can apply the §Mixing volume `* 11/8`
        // multiplier. Both polarities must round-trip.
        let mut h = synth_header();
        h.stereo = true;
        let p = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert!(p.stereo, "stereo header flag must reach PlayerState");

        h.stereo = false;
        let p = PlayerState::new(&h, Vec::new(), Vec::new(), 44_100);
        assert!(!p.stereo, "mono header flag must reach PlayerState");
    }

    /// Render one mixer step on a player whose only audible state is a
    /// single full-volume mono sample at centre pan. The amplitude
    /// returned is the sum of `|L|` and `|R|` of the produced i16 frame,
    /// i.e. a scalar proxy for "how loud was the master/global mixing
    /// stage." Used by the §Mixing volume tests to compare stereo vs
    /// mono `* 11/8` scaling.
    fn render_master_volume_amplitude(stereo: bool, master_volume: u8) -> i32 {
        let mut h = synth_header();
        h.stereo = stereo;
        h.master_volume = master_volume;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [7u8; 32]; // centre pan
        h.global_volume = 64;
        h.enabled_channels = 1;
        h.order = vec![0u8];

        // Build one mono sample: a constant +0x4000 i16 PCM block long
        // enough that mix_channel won't run off the end during the
        // single step we exercise.
        let pcm = vec![0x4000i16; 1024];
        let sample = SampleBody {
            pcm,
            pcm_right: None,
            loop_start: 0,
            loop_end: 0,
            looped: false,
            volume: 64,
            c5_speed: 8363,
        };

        let mut p = PlayerState::new(&h, vec![sample], vec![Pattern::empty(1)], 44_100);
        // Wire a live channel straight into the mixer — the row-state path
        // is not under test here.
        p.channels[0].instrument = 1;
        p.channels[0].volume = 64;
        p.channels[0].pan = 7;
        p.channels[0].active = true;
        p.channels[0].frequency = 8363.0;
        p.channels[0].sample_pos = 0.0;

        let mut buf = [0i16; 2];
        p.render_one(&mut buf);
        (buf[0] as i32).abs() + (buf[1] as i32).abs()
    }

    #[test]
    fn stereo_mixing_volume_gets_11_over_8_boost() {
        // multimedia.cx §Mixing volume: "Mixing volume ... is multiplied
        // by 11/8 when stereo is on." With the same numerical MV (48,
        // the ST3 SOUNDCFG default), a stereo file must produce a
        // measurably louder mix than the mono variant. We need the ratio
        // to be ~1.375× (= 11/8) — pre-clamp, since post-clamp peaks
        // would compress the boost.
        //
        // Use a low MV (16, the spec floor) so even after the boost the
        // resulting amplitude is well below the i16 ceiling and the
        // 11/8 ratio survives intact.
        let mono = render_master_volume_amplitude(false, 16);
        let stereo = render_master_volume_amplitude(true, 16);
        assert!(mono > 0, "mono baseline must be non-zero");
        assert!(
            stereo > mono,
            "stereo MV must exceed mono MV at same setting"
        );
        let ratio = stereo as f64 / mono as f64;
        // The mixer applies `* 11/8` = 1.375 exactly; allow a small
        // tolerance for the i16 quantisation step.
        assert!(
            (ratio - 1.375).abs() < 0.02,
            "stereo/mono ratio {ratio} must be 11/8 (= 1.375) within 0.02"
        );
    }

    #[test]
    fn mono_mixing_volume_has_no_stereo_boost() {
        // Inverse check: with the stereo flag clear, two different MVs
        // must scale linearly (16 → 32 doubles the output) and there
        // must be no hidden 11/8 multiplier anywhere in the mono path.
        let lo = render_master_volume_amplitude(false, 16);
        let hi = render_master_volume_amplitude(false, 32);
        assert!(lo > 0);
        let ratio = hi as f64 / lo as f64;
        assert!(
            (ratio - 2.0).abs() < 0.05,
            "doubling MV must double the mono amplitude, got ratio {ratio}"
        );
    }

    #[test]
    fn vxx_stash_cleared_on_next_row_entry() {
        // Defence in depth: if a row carried a Vxx with speed >= 2 and
        // the per-tick drain ran normally, the next row's `enter_row`
        // must still find the stash empty (because `take()` left it
        // None on tick 1). And a brand-new row without a Vxx command
        // must not somehow resurrect the previous row's value.
        let mut p = vxx_test_player(0x30, 6);
        p.enter_row();
        p.tick = 1;
        p.apply_per_tick();
        assert_eq!(p.global_volume, 0x30);
        // Row 1 (empty cell) entry — stash must be None throughout.
        p.row = 1;
        p.enter_row();
        assert!(p.pending_global_vol.is_none());
        for t in 1..p.speed {
            p.tick = t;
            p.apply_per_tick();
        }
        assert_eq!(
            p.global_volume, 0x30,
            "row 1's empty cell must NOT touch the global volume"
        );
    }

    // ----- Per-voice latched global volume (§Vxx) ---------------------
    //
    // §Vxx (multimedia.cx behavioural reference):
    //   "It does not affect past notes, that are still playing, unless
    //    their volume is changed, which applies the new global volume to
    //    that voice."
    // The player models this with a per-voice `voice_global_vol` latch: a
    // note trigger / volume write captures the global volume live at that
    // moment; a Vxx change updates the player-wide global volume but does
    // NOT retroactively re-latch already-playing voices. The mixer scales
    // each voice by its own latched value.

    /// Build a one-PCM-channel player whose pattern is supplied by the
    /// caller, with a non-trivial instrument default volume so triggers
    /// are observable. `global_volume` starts at 64 (unity).
    fn voice_gv_player(rows: Vec<[Cell; 32]>) -> PlayerState {
        let mut h = synth_header();
        h.flags = 0;
        h.tracker_version = 0x1320;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.global_volume = 64;
        h.initial_speed = 6;
        h.initial_tempo = 125;
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let n = rows.len().max(1);
        let mut pat = Pattern::empty(1);
        for (i, row) in rows.into_iter().enumerate() {
            pat.rows[i] = row.to_vec();
        }
        // A 32-frame constant PCM sample with default volume 40 so an
        // instrument reload writes an observable stored volume.
        let sample = SampleBody {
            pcm: vec![0x4000i16; 64],
            pcm_right: None,
            loop_start: 0,
            loop_end: 0,
            looped: false,
            volume: 40,
            c5_speed: 8363,
        };
        let mut p = PlayerState::new(&h, vec![sample], vec![pat], 44_100);
        // Keep the harness from running off the synthesized pattern.
        p.order = vec![0u8; n.max(1)];
        p
    }

    fn note_cell(note: u8) -> Cell {
        Cell {
            note,
            instrument: 1,
            volume: 0xFF,
            command: 0,
            info: 0,
        }
    }

    fn vxx_cell(param: u8) -> Cell {
        Cell {
            note: 0xFF,
            instrument: 0,
            volume: 0xFF,
            command: cmd::V_GLOBAL_VOL,
            info: param,
        }
    }

    /// Drive a full row: tick 0 (`enter_row`) then ticks `1..speed`
    /// (`apply_per_tick`), then advance the row pointer like `render` does.
    fn play_full_row(p: &mut PlayerState) {
        p.tick = 0;
        p.enter_row();
        for t in 1..p.speed {
            p.tick = t;
            p.apply_per_tick();
        }
        p.tick = 0;
        p.next_row();
    }

    #[test]
    fn note_trigger_latches_current_global_volume() {
        // A fresh note latches the global volume live at trigger time.
        let empty = [Cell::EMPTY; 32];
        let mut r0 = empty;
        r0[0] = note_cell(0x40); // C-5
        let p_rows = vec![r0, empty];
        let mut p = voice_gv_player(p_rows);
        p.global_volume = 50;
        p.channels[0].voice_global_vol = 64; // stale; must be overwritten
        play_full_row(&mut p);
        assert_eq!(
            p.channels[0].voice_global_vol, 50,
            "note trigger must latch the global volume live at tick 0"
        );
    }

    #[test]
    fn vxx_does_not_retroactively_change_a_held_voice() {
        // Row 0 triggers a note (latches GV=64). Row 1 carries only a Vxx
        // dropping the global volume — the still-playing voice from row 0
        // must KEEP its latched 64, not pick up the new value.
        let empty = [Cell::EMPTY; 32];
        let mut r0 = empty;
        r0[0] = note_cell(0x40);
        let mut r1 = empty;
        r1[0] = vxx_cell(0x10); // global volume -> 16
        let mut r2 = empty;
        r2[0] = Cell::EMPTY;
        let mut p = voice_gv_player(vec![r0, r1, r2]);

        play_full_row(&mut p); // row 0: trigger
        assert_eq!(p.channels[0].voice_global_vol, 64);
        assert!(p.channels[0].active);

        play_full_row(&mut p); // row 1: Vxx 0x10, no new note
        assert_eq!(
            p.global_volume, 0x10,
            "player-wide global volume must update on the Vxx row"
        );
        assert_eq!(
            p.channels[0].voice_global_vol, 64,
            "a held voice must NOT be retroactively rescaled by Vxx"
        );
    }

    #[test]
    fn note_after_vxx_picks_up_the_new_global_volume() {
        // Row 0: note (latch 64). Row 1: Vxx -> 16. Row 2: a NEW note must
        // latch the now-current global volume (16), even though its
        // numeric channel volume (instrument default 40) is unchanged from
        // the row-0 trigger — the latch is unconditional on a trigger.
        let empty = [Cell::EMPTY; 32];
        let mut r0 = empty;
        r0[0] = note_cell(0x40);
        let mut r1 = empty;
        r1[0] = vxx_cell(0x10);
        let mut r2 = empty;
        r2[0] = note_cell(0x40); // same note + instrument as row 0
        let mut p = voice_gv_player(vec![r0, r1, r2]);

        play_full_row(&mut p); // row 0
        play_full_row(&mut p); // row 1 (Vxx)
        assert_eq!(p.global_volume, 0x10);
        play_full_row(&mut p); // row 2 (re-trigger)
        assert_eq!(
            p.channels[0].voice_global_vol, 0x10,
            "a note re-triggered after a Vxx must latch the new global volume \
             even when its numeric channel volume is unchanged"
        );
    }

    #[test]
    fn volume_column_after_vxx_relatches() {
        // §Vxx "unless their volume is changed": a held voice that gets an
        // explicit volume-column write on a later row re-latches the
        // (by-then current) global volume.
        let empty = [Cell::EMPTY; 32];
        let mut r0 = empty;
        r0[0] = note_cell(0x40); // latch 64
        let mut r1 = empty;
        r1[0] = vxx_cell(0x20); // GV -> 32
        let mut r2 = empty;
        r2[0] = Cell {
            note: 0xFF,
            instrument: 0,
            volume: 30, // volume-column write, no new note
            command: 0,
            info: 0,
        };
        let mut p = voice_gv_player(vec![r0, r1, r2]);
        play_full_row(&mut p); // row 0
        play_full_row(&mut p); // row 1
        assert_eq!(p.channels[0].voice_global_vol, 64);
        play_full_row(&mut p); // row 2: volume column
        assert_eq!(
            p.channels[0].voice_global_vol, 0x20,
            "a volume-column write re-latches the current global volume"
        );
        assert_eq!(p.channels[0].volume, 30);
    }

    #[test]
    fn dxy_slide_after_vxx_relatches_voice() {
        // §Vxx: a Vxx "is also applied on the same row if anything updates
        // the note volume on tick 1 or tick 2 ... like the Dxx effect
        // (when doing a non-fine slide)". A held voice running a Dxy slide
        // on the same row as a Vxx re-latches the new global volume because
        // the slide changes the active volume on a later tick.
        let empty = [Cell::EMPTY; 32];
        let mut r0 = empty;
        r0[0] = note_cell(0x40); // latch 64, volume 40
        let mut r1 = empty;
        // Same row: Vxx is global so only one channel carries it; put the
        // Dxy on the same channel after the GV has been raised on a prior
        // row. Use two rows to keep the single-channel model clean:
        //   row 1: Vxx -> 16
        //   row 2: D01 (slide down by 1 on nonzero ticks)
        r1[0] = vxx_cell(0x10);
        let mut r2 = empty;
        r2[0] = Cell {
            note: 0xFF,
            instrument: 0,
            volume: 0xFF,
            command: cmd::D_VOL_SLIDE,
            info: 0x01, // D01: slide down 1 per nonzero tick
        };
        let mut p = voice_gv_player(vec![r0, r1, r2]);
        play_full_row(&mut p); // row 0 trigger
        play_full_row(&mut p); // row 1 Vxx
        assert_eq!(p.channels[0].voice_global_vol, 64);
        play_full_row(&mut p); // row 2: D01 changes active volume
        assert!(
            p.channels[0].volume < 40,
            "D01 must have lowered the active volume"
        );
        assert_eq!(
            p.channels[0].voice_global_vol, 0x10,
            "a volume slide re-latches the current global volume onto the voice"
        );
    }

    #[test]
    fn untouched_voice_keeps_old_gv_while_sibling_relatches() {
        // Two channels: ch0 holds a note untouched across a Vxx; ch1
        // re-triggers after the Vxx. Only ch1 must pick up the new GV.
        let mut h = synth_header();
        h.flags = 0;
        h.tracker_version = 0x1320;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.channels[1] = 1;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.muted[1] = false;
        h.pans = [8u8; 32];
        h.global_volume = 64;
        h.initial_speed = 6;
        h.initial_tempo = 125;
        h.enabled_channels = 2;
        h.order = vec![0u8];

        let mut pat = Pattern::empty(32);
        // Row 0: both channels trigger (latch 64).
        pat.rows[0][0] = note_cell(0x40);
        pat.rows[0][1] = note_cell(0x40);
        // Row 1: Vxx on ch0 drops GV. ch0's note keeps playing untouched;
        // ch1's note also keeps playing untouched.
        pat.rows[1][0] = vxx_cell(0x18);
        // Row 2: ch1 re-triggers (on a *later* row, after the Vxx drained),
        // so it must pick up the new global volume. ch0 stays untouched.
        pat.rows[2][1] = note_cell(0x41);
        let sample = SampleBody {
            pcm: vec![0x4000i16; 64],
            pcm_right: None,
            loop_start: 0,
            loop_end: 0,
            looped: false,
            volume: 40,
            c5_speed: 8363,
        };
        let mut p = PlayerState::new(&h, vec![sample], vec![pat], 44_100);
        p.order = vec![0u8, 0u8];

        play_full_row(&mut p); // row 0
        assert_eq!(p.channels[0].voice_global_vol, 64);
        assert_eq!(p.channels[1].voice_global_vol, 64);
        play_full_row(&mut p); // row 1: Vxx (no trigger)
        assert_eq!(p.global_volume, 0x18);
        assert_eq!(
            p.channels[0].voice_global_vol, 64,
            "the untouched held voice keeps its old global volume"
        );
        assert_eq!(
            p.channels[1].voice_global_vol, 64,
            "a held voice not retriggered on the Vxx row keeps its old GV"
        );
        play_full_row(&mut p); // row 2: ch1 re-triggers
        assert_eq!(
            p.channels[0].voice_global_vol, 64,
            "ch0, still untouched, must keep its old global volume"
        );
        assert_eq!(
            p.channels[1].voice_global_vol, 0x18,
            "ch1, re-triggered after the Vxx, latches the new global volume"
        );
    }

    #[test]
    fn note_on_same_row_as_vxx_keeps_old_global_volume() {
        // §Vxx "does not affect events on the same row, where the effect is
        // set". A note triggered on the *same* row as a Vxx keeps the
        // pre-Vxx global volume — the Vxx only drains on tick 1, after the
        // tick-0 trigger has already latched. This holds even though, in
        // ST3's single-channel-per-slot model, the Vxx and the note may sit
        // on the same channel.
        let empty = [Cell::EMPTY; 32];
        let mut r0 = empty;
        // Same channel carries both a fresh note and the Vxx.
        r0[0] = Cell {
            note: 0x40,
            instrument: 1,
            volume: 0xFF,
            command: cmd::V_GLOBAL_VOL,
            info: 0x10, // GV -> 16 on tick 1
        };
        let mut p = voice_gv_player(vec![r0, empty]);
        play_full_row(&mut p);
        assert_eq!(p.global_volume, 0x10, "Vxx still drains on tick 1");
        assert_eq!(
            p.channels[0].voice_global_vol, 64,
            "a same-row trigger keeps the pre-Vxx global volume"
        );
    }

    #[test]
    fn mixer_scales_by_per_voice_global_volume() {
        // End-to-end: two identical voices differing only in their latched
        // global volume produce proportional output amplitudes.
        let mut h = synth_header();
        h.stereo = false;
        h.master_volume = 48;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [7u8; 32];
        h.global_volume = 64;
        h.enabled_channels = 1;
        h.order = vec![0u8];
        let sample = SampleBody {
            pcm: vec![0x4000i16; 1024],
            pcm_right: None,
            loop_start: 0,
            loop_end: 0,
            looped: false,
            volume: 64,
            c5_speed: 8363,
        };
        let render_at = |gv: u8| -> i32 {
            let mut p = PlayerState::new(&h, vec![sample.clone()], vec![Pattern::empty(1)], 44_100);
            p.channels[0].instrument = 1;
            p.channels[0].volume = 63;
            p.channels[0].pan = 7;
            p.channels[0].active = true;
            p.channels[0].frequency = 8363.0;
            p.channels[0].sample_pos = 0.0;
            p.channels[0].voice_global_vol = gv;
            let mut buf = [0i16; 2];
            p.render_one(&mut buf);
            (buf[0] as i32).abs() + (buf[1] as i32).abs()
        };
        let full = render_at(64);
        let half = render_at(32);
        assert!(full > 0);
        // Half global volume should produce roughly half amplitude.
        let ratio = half as f32 / full as f32;
        assert!(
            (ratio - 0.5).abs() < 0.05,
            "voice at GV=32 should be ~half the GV=64 amplitude (got {ratio})"
        );
    }

    // ----- Oxy loop-aware sample-offset helper ------------------------

    #[test]
    fn oxy_offset_unlooped_is_raw_value() {
        // Per `docs/audio/trackers/s3m/ScreamTracker-v3.20-effects.txt`
        // §Oxy the loop-wrap rule only kicks in for *looped* samples; an
        // unlooped sample takes the requested byte offset verbatim and
        // the mixer's own bounds check deactivates the channel when the
        // cursor walks past `pcm_len`. Confirm offsets both inside and
        // past `pcm_len` pass through untouched.
        assert_eq!(resolve_sample_offset(1024, 0, 0, 8192, false), 1024.0);
        // Past pcm_len: still returned raw — caller / mixer handles it.
        assert_eq!(resolve_sample_offset(20_000, 0, 0, 8192, false), 20_000.0);
    }

    #[test]
    fn oxy_offset_inside_loop_window_is_unchanged() {
        // Looped sample, offset within `[loop_start, loop_end)` — no
        // folding, the cursor lands exactly where requested. Same goes
        // for an offset *before* the loop window (the spec only calls
        // out the `> loop_end` overflow case).
        // Loop: [256, 1024); offset 512 lands mid-loop.
        assert_eq!(
            resolve_sample_offset(512, 256, 1024, 4096, true),
            512.0,
            "offset inside the loop window must not be folded"
        );
        // Offset 100 < loop_start: pre-loop region, no fold either.
        assert_eq!(
            resolve_sample_offset(100, 256, 1024, 4096, true),
            100.0,
            "offset before the loop window must not be folded"
        );
    }

    #[test]
    fn oxy_offset_exceeding_loop_end_wraps_into_loop_window() {
        // Spec quote: "If the sample offset is used in a looped sample
        // and the offset given exceeds the loop end value, the loop is
        // taken into consideration and the offset will be calculated as
        // if the sample had looped." Concretely: loop window
        // `[loop_start, loop_end)` of length `span`; for an offset of
        // `loop_end + k`, the effective position is `loop_start + k
        // mod span`.
        // Loop [200, 1000), span = 800. Offset 1500 → overshoots by 500
        // → 200 + (1500 - 200) % 800 = 200 + 500 = 700.
        assert_eq!(
            resolve_sample_offset(1500, 200, 1000, 4096, true),
            700.0,
            "1500 with loop [200,1000) must fold to 700"
        );
        // Offset exactly at loop_end: span lands us back at loop_start.
        assert_eq!(
            resolve_sample_offset(1000, 200, 1000, 4096, true),
            200.0,
            "an offset exactly at loop_end must fold to loop_start"
        );
        // Two full loops past the end:
        // loop_start + (2*span + 50) % span = loop_start + 50.
        assert_eq!(
            resolve_sample_offset(200 + 2 * 800 + 50, 200, 1000, 4096, true),
            250.0,
            "multi-loop overshoot must fold to (loop_start + remainder)"
        );
    }

    #[test]
    fn oxy_offset_malformed_loop_falls_back_to_raw() {
        // Defensive: a corrupt instrument header whose loop_end <=
        // loop_start (or whose loop_end overshoots pcm_len) can't be
        // folded mathematically. The helper returns the raw offset so
        // the mixer's own bounds check decides what to do; never panic,
        // never divide by a zero / negative span.
        // Degenerate: loop_end == loop_start (zero span).
        assert_eq!(
            resolve_sample_offset(2000, 500, 500, 4096, true),
            2000.0,
            "zero-span loop must fall back to raw offset"
        );
        // loop_end past pcm_len.
        assert_eq!(
            resolve_sample_offset(2000, 100, 9000, 4096, true),
            2000.0,
            "loop_end past pcm_len must fall back to raw offset"
        );
    }

    // ---------------------------------------------------------------
    // Ixy tremor — persistent decrementing-counter coverage. The
    // multimedia.cx behavioural reference §Ixy specifies a two-counter
    // procedure with cross-row persistence ("counters are never reset")
    // that fires on **every** tick, including tick 0. The unit tests
    // below drive [`PlayerState::apply_tremor_step`] / `enter_row` /
    // `apply_per_tick` directly so each branch of the spec is locked
    // in without standing up a full byte fixture.
    // ---------------------------------------------------------------

    #[test]
    fn tremor_step_initial_state_restores_volume_and_arms_on_counter() {
        // Spec § Ixy "If the 'on' counter was zero in the beginning of
        // the update procedure, then the 'off' counter is decremented
        // and if it reached zero (or became less than zero), the current
        // volume is set to the stored volume and the 'on' counter is
        // set to the 'on' time (x + 1)."
        // Cold start: both counters 0 → the off-decrement underflows,
        // volume restores from `stored_volume`, on_counter = x+1.
        let mut ch = Channel {
            stored_volume: 50,
            volume: 0,
            tremor_on_counter: 0,
            tremor_off_counter: 0,
            ..Channel::default()
        };
        PlayerState::apply_tremor_step(&mut ch, /*x*/ 2, /*y*/ 2);
        assert_eq!(ch.volume, 50, "cold-start tick restores stored volume");
        assert_eq!(ch.tremor_on_counter, 3, "on_counter armed to x+1 = 3");
        assert_eq!(ch.tremor_off_counter, 0);
    }

    #[test]
    fn tremor_step_on_phase_decrements_until_off_transition() {
        // Walk a 3-on / 3-off cycle from an armed state and verify the
        // spec's "decrement on_counter; when it reaches zero, set
        // volume=0 and load off_counter=y+1" branch fires exactly once,
        // at the on→off transition.
        let mut ch = Channel {
            stored_volume: 50,
            volume: 50,
            tremor_on_counter: 3, // armed at x+1
            tremor_off_counter: 0,
            ..Channel::default()
        };
        // First two on-steps: decrement but stay audible.
        PlayerState::apply_tremor_step(&mut ch, 2, 2);
        assert_eq!(ch.tremor_on_counter, 2);
        assert_eq!(ch.volume, 50);
        PlayerState::apply_tremor_step(&mut ch, 2, 2);
        assert_eq!(ch.tremor_on_counter, 1);
        assert_eq!(ch.volume, 50);
        // Third on-step: counter hits 0 → silence + off_counter = y+1.
        PlayerState::apply_tremor_step(&mut ch, 2, 2);
        assert_eq!(ch.tremor_on_counter, 0);
        assert_eq!(ch.volume, 0);
        assert_eq!(ch.tremor_off_counter, 3, "off_counter armed to y+1 = 3");
    }

    #[test]
    fn tremor_step_off_phase_returns_to_stored_volume() {
        // After the on→off transition the next ticks drain the
        // off_counter without touching volume; once it underflows the
        // restore branch fires and on_counter re-arms.
        let mut ch = Channel {
            stored_volume: 50,
            volume: 0,
            tremor_on_counter: 0,
            tremor_off_counter: 3,
            ..Channel::default()
        };
        PlayerState::apply_tremor_step(&mut ch, 2, 2);
        assert_eq!(ch.tremor_off_counter, 2);
        assert_eq!(ch.volume, 0);
        PlayerState::apply_tremor_step(&mut ch, 2, 2);
        assert_eq!(ch.tremor_off_counter, 1);
        assert_eq!(ch.volume, 0);
        PlayerState::apply_tremor_step(&mut ch, 2, 2);
        // Off_counter saturating-subs from 1 → 0; "reached zero" branch
        // fires: vol restores from stored, on_counter rearms.
        assert_eq!(ch.tremor_off_counter, 0);
        assert_eq!(ch.volume, 50, "restore reads stored_volume");
        assert_eq!(ch.tremor_on_counter, 3);
    }

    #[test]
    fn tremor_step_restore_reads_stored_volume_not_active() {
        // Spec § Ixy "The stored volume isn't modified by this effect"
        // — so a Dxy slide that pulled the *active* volume to 0 before
        // Ixy's restore must NOT be the value Ixy restores to. We model
        // this by pre-loading `stored_volume = 50` and `volume = 0`;
        // an off→on transition must lift volume to 50, not stay at 0.
        let mut ch = Channel {
            stored_volume: 50,
            volume: 0,
            tremor_on_counter: 0,
            tremor_off_counter: 1, // one tick away from underflow.
            ..Channel::default()
        };
        PlayerState::apply_tremor_step(&mut ch, 2, 2);
        assert_eq!(
            ch.volume, 50,
            "stored_volume drives the restore, not active volume"
        );
    }

    #[test]
    fn tremor_step_caps_restore_to_pcm_volume_peak() {
        // Defensive: an externally constructed `Channel` literal could
        // sneak a `stored_volume > PCM_VOLUME_PEAK` past the helper. The
        // restore branch must still cap the active volume to the spec
        // ceiling (63), matching the documented "volumes peak at 63".
        let mut ch = Channel {
            stored_volume: 200,
            volume: 0,
            tremor_on_counter: 0,
            tremor_off_counter: 0,
            ..Channel::default()
        };
        PlayerState::apply_tremor_step(&mut ch, 2, 2);
        assert_eq!(ch.volume, PCM_VOLUME_PEAK);
    }

    #[test]
    fn tremor_counters_persist_across_rows_without_ixy() {
        // Spec § Ixy "The 'on' and 'off' counters are never reset,
        // except in the tremor update procedure described above." So a
        // row carrying Ixy followed by a row without Ixy must leave the
        // counters intact — the next Ixy row resumes the same cycle.
        // Drive `enter_row` directly to confirm.
        let mut h = synth_header();
        h.flags = 0;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let mut pat = Pattern::empty(2);
        // Row 1 has no Ixy — bare empty cell.
        pat.rows[1][0] = Cell {
            note: 0xFF,
            instrument: 0,
            volume: 0xFF,
            command: 0,
            info: 0,
        };
        let mut p = PlayerState::new(&h, vec![], vec![pat], 44_100);
        // Manually arm tremor state mid-cycle, simulating a previous
        // row's Ixy step having mutated the counters.
        p.channels[0].stored_volume = 40;
        p.channels[0].tremor_on_counter = 2;
        p.channels[0].tremor_off_counter = 0;
        p.channels[0].volume = 40;
        // Advance to row 1 (no Ixy command). `enter_row` MUST NOT
        // touch the tremor counters.
        p.row = 1;
        p.enter_row();
        assert_eq!(
            p.channels[0].tremor_on_counter, 2,
            "row without Ixy must not reset on_counter"
        );
        assert_eq!(
            p.channels[0].tremor_off_counter, 0,
            "row without Ixy must not reset off_counter"
        );
    }

    #[test]
    fn tremor_volume_stays_zero_when_no_ixy_on_next_row() {
        // Spec § Ixy "If the current volume was 0 at the end of the
        // effect and there is no tremor effect on the next row, the
        // current volume stays 0. It isn't reset back to the stored
        // volume or its previous value from before the tremor effect."
        // Set up a row 1 with no Ixy, no note, no volume column; confirm
        // a row-0-induced 0 volume is preserved.
        let mut h = synth_header();
        h.flags = 0;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let mut pat = Pattern::empty(2);
        pat.rows[1][0] = Cell {
            note: 0xFF,
            instrument: 0,
            volume: 0xFF,
            command: 0,
            info: 0,
        };
        let mut p = PlayerState::new(&h, vec![], vec![pat], 44_100);
        p.channels[0].volume = 0;
        p.channels[0].stored_volume = 40;
        p.row = 1;
        p.enter_row();
        assert_eq!(
            p.channels[0].volume, 0,
            "row without Ixy must not restore from stored_volume"
        );
        assert_eq!(p.channels[0].stored_volume, 40, "stored_volume untouched");
    }

    #[test]
    fn tremor_step_with_zero_params_is_guarded_at_callsite() {
        // The §Ixy guard `(x | y) != 0` lives at the dispatch site
        // (enter_row / apply_per_tick). The helper itself, when called
        // with x = 0 and y = 0 from a cold-counter state, fires the
        // off-underflow branch — that's correct given the guard upstream
        // and documents the helper's contract: it is the *caller's*
        // job to gate "no-op tremor". (Confirmed by the dispatch site
        // matching `cmd::I_TREMOR if (x | y) != 0`.)
        let mut ch = Channel {
            stored_volume: 50,
            volume: 0,
            tremor_on_counter: 0,
            tremor_off_counter: 0,
            ..Channel::default()
        };
        // Calling with x=y=0 underflows the off-counter branch — vol
        // restores and on_counter arms to 1.
        PlayerState::apply_tremor_step(&mut ch, 0, 0);
        assert_eq!(ch.volume, 50);
        assert_eq!(ch.tremor_on_counter, 1);
    }

    // ---------------------------------------------------------------
    // Rxy tremolo — multimedia.cx behavioural reference §Rxy. The tests
    // below drive [`PlayerState::apply_tremolo`] / `enter_row` /
    // `apply_per_tick` directly so each documented rule is locked in.
    // ---------------------------------------------------------------

    #[test]
    fn tremolo_recomputes_from_stored_volume_not_accumulating() {
        // §Rxy: "set the active volume to the stored volume plus
        // (depth * value) / (max_amplitude * 2) ... The stored volume is
        // untouched." The active volume is stored + delta, recomputed
        // every tick — NOT the previous active value + delta, which would
        // accumulate drift. With a square waveform (deterministic ±63, the
        // documented FireLight §6.8 `case 2: delta = 255` scaled to the ±64
        // working range), R44 gives |delta| = (4·4 · 63) / 128 = 7, so the
        // active volume must stay pinned to {stored+7, stored-7}.
        let mut ch = Channel {
            stored_volume: 40,
            volume: 40,
            tremolo_waveform: Waveform::Square,
            ..Channel::default()
        };
        for tick in 1..=32 {
            PlayerState::apply_tremolo(&mut ch, 4, 4);
            // Phase steps by 4·4 = 16 per tick over the 256 cycle; the
            // square wave is high for phase < 128 → ticks 1..=7 read +63,
            // ticks 8..=15 read -63, then the cycle repeats.
            let expected = if (tick * 16) % 256 < 128 { 47 } else { 33 };
            assert_eq!(
                ch.volume, expected,
                "tick {tick}: active volume must be stored ± 7, never drift"
            );
            assert_eq!(ch.stored_volume, 40, "stored volume is untouched");
        }
    }

    #[test]
    fn tremolo_depth_scaling_and_peak_clamp() {
        // §Rxy depth is y*4 — for y = 0xF the square-wave delta is
        // (15·4 · 63) / 128 = 29 (square magnitude is the FireLight §6.8
        // `delta = 255` scaled to ±63; the documented "peaks at 32 in each
        // direction" is the formula's theoretical bound).
        let mut ch = Channel {
            stored_volume: 32,
            volume: 32,
            tremolo_waveform: Waveform::Square,
            ..Channel::default()
        };
        PlayerState::apply_tremolo(&mut ch, 4, 0xF);
        assert_eq!(ch.volume, 61, "32 + 29 on the high half-cycle");
        // Walk the phase into the low half-cycle (phase >= 128).
        ch.tremolo_pos = 128;
        PlayerState::apply_tremolo(&mut ch, 4, 0xF);
        assert_eq!(ch.volume, 3, "32 - 29 on the low half-cycle");
        // §Playback Notes: volumes peak at 63 — a stored volume high
        // enough that stored + 29 overflows must clamp.
        ch.tremolo_pos = 0;
        ch.stored_volume = 50;
        PlayerState::apply_tremolo(&mut ch, 4, 0xF);
        assert_eq!(ch.volume, PCM_VOLUME_PEAK, "50 + 30 caps at 63");
        // And the negative side floors at 0.
        ch.tremolo_pos = 128;
        ch.stored_volume = 10;
        PlayerState::apply_tremolo(&mut ch, 4, 0xF);
        assert_eq!(ch.volume, 0, "10 - 30 floors at 0");
    }

    #[test]
    fn tremolo_noop_when_stored_volume_is_zero() {
        // §Rxy: "Tremolo will not work if the stored volume is 0."
        // Neither the active volume nor the phase may move.
        let mut ch = Channel {
            stored_volume: 0,
            volume: 7,
            tremolo_pos: 12,
            tremolo_waveform: Waveform::Square,
            ..Channel::default()
        };
        PlayerState::apply_tremolo(&mut ch, 4, 4);
        assert_eq!(ch.volume, 7, "zero stored volume disables tremolo");
        assert_eq!(ch.tremolo_pos, 12, "phase must not advance either");
    }

    #[test]
    fn tremolo_phase_steps_speed_times_4_over_256_cycle() {
        // §Playback Notes: "Vibrato and tremolo have a full cycle length
        // of 256, though Hxy and Rxy use x*4 and y*4 as their
        // parameters." Speed 8 → 32 phase units per tick → a full cycle
        // every 8 ticks (256 / (4·8)).
        let mut ch = Channel {
            stored_volume: 40,
            volume: 40,
            tremolo_waveform: Waveform::Square,
            ..Channel::default()
        };
        for _ in 0..3 {
            PlayerState::apply_tremolo(&mut ch, 8, 1);
        }
        assert_eq!(ch.tremolo_pos, 96, "3 ticks at speed 8 → phase 96");
        for _ in 0..5 {
            PlayerState::apply_tremolo(&mut ch, 8, 1);
        }
        assert_eq!(ch.tremolo_pos, 0, "8 ticks wrap the full 256 cycle");
    }

    #[test]
    fn tremolo_applies_from_tick_1_not_tick_0() {
        // §Rxy: "On tick 1 (the second tick of the row) set the active
        // volume ..." — the row-entry (tick 0) pass must leave the active
        // volume alone; the first per-tick pass applies the delta. This
        // also locks the §Rxy speed-1 rule ("the active volume is also
        // untouched. It is not set to the stored volume!") structurally:
        // a speed-1 row never reaches tick 1, so `enter_row` alone is the
        // whole row.
        let mut h = synth_header();
        h.flags = 0;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let mut pat = Pattern::empty(1);
        pat.rows[0][0] = Cell {
            note: 0xFF,
            instrument: 0,
            volume: 0xFF,
            command: cmd::R_TREMOLO,
            info: 0x44,
        };
        let mut p = PlayerState::new(&h, Vec::new(), vec![pat], 44_100);
        p.channels[0].stored_volume = 40;
        p.channels[0].volume = 40;
        p.channels[0].tremolo_waveform = Waveform::Square;
        p.enter_row();
        assert_eq!(
            p.channels[0].volume, 40,
            "tick 0 must not apply the tremolo delta (speed-1 rows therefore never do)"
        );
        p.tick = 1;
        p.apply_per_tick();
        assert_eq!(
            p.channels[0].volume, 47,
            "tick 1 applies stored + (4·4 · 63)/128 = 40 + 7"
        );
    }

    #[test]
    fn stored_volume_tracks_instrument_default_load() {
        // Loading a fresh instrument on a row is a stored-volume write
        // per the §Ixy / §Rxy stored-vs-active distinction. Drive
        // `enter_row` with `cell.instrument = 1` and confirm both
        // `volume` and `stored_volume` land at the instrument default
        // (clamped to PCM_VOLUME_PEAK).
        let mut h = synth_header();
        h.flags = 0;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let sample = SampleBody {
            pcm: vec![0i16; 16],
            pcm_right: None,
            loop_start: 0,
            loop_end: 16,
            looped: true,
            // Instrument default volume = 64 — clamps to PCM_VOLUME_PEAK
            // (63) on the PCM path per §Playback Notes.
            volume: 64,
            c5_speed: 8363,
        };

        let mut pat = Pattern::empty(1);
        pat.rows[0][0] = Cell {
            note: 0x50,
            instrument: 1,
            volume: 0xFF,
            command: 0,
            info: 0,
        };
        let mut p = PlayerState::new(&h, vec![sample], vec![pat], 44_100);
        p.enter_row();
        assert_eq!(p.channels[0].volume, PCM_VOLUME_PEAK);
        assert_eq!(p.channels[0].stored_volume, PCM_VOLUME_PEAK);
    }

    #[test]
    fn stored_volume_tracks_explicit_volume_column() {
        // An explicit volume-column write (cell.volume != 0xFF) is also
        // a stored-volume source. Drive `enter_row` with `cell.volume =
        // 0x20` and confirm both fields move together.
        let mut h = synth_header();
        h.flags = 0;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let mut pat = Pattern::empty(1);
        pat.rows[0][0] = Cell {
            note: 0xFF,
            instrument: 0,
            volume: 0x20,
            command: 0,
            info: 0,
        };
        let mut p = PlayerState::new(&h, vec![], vec![pat], 44_100);
        p.enter_row();
        assert_eq!(p.channels[0].volume, 0x20);
        assert_eq!(p.channels[0].stored_volume, 0x20);
    }

    #[test]
    fn ixy_fires_on_tick_zero_too() {
        // Spec § Ixy "This effect is updated on every tick" — including
        // tick 0. Set up a row that fires Ixy at tick 0 from a cold
        // counter state: the off-underflow branch must lift `volume`
        // from 0 → `stored_volume` before any per-tick work happens.
        let mut h = synth_header();
        h.flags = 0;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let sample = SampleBody {
            pcm: vec![0i16; 16],
            pcm_right: None,
            loop_start: 0,
            loop_end: 16,
            looped: true,
            volume: 40,
            c5_speed: 8363,
        };

        let mut pat = Pattern::empty(1);
        pat.rows[0][0] = Cell {
            note: 0x50,
            instrument: 1,
            volume: 0xFF,
            command: cmd::I_TREMOR,
            info: 0x22, // x=2, y=2 → on=3, off=3.
        };
        let mut p = PlayerState::new(&h, vec![sample], vec![pat], 44_100);
        // Pre-arm the tremor counters to the cold state so the tick-0
        // step exercises the off-underflow branch (was-zero side).
        p.channels[0].tremor_on_counter = 0;
        p.channels[0].tremor_off_counter = 0;
        p.enter_row();
        // After tick 0 of an Ixy row from the cold state: vol restored
        // to stored_volume (40), on_counter armed to x+1 = 3.
        assert_eq!(p.channels[0].volume, 40);
        assert_eq!(p.channels[0].stored_volume, 40);
        assert_eq!(p.channels[0].tremor_on_counter, 3);
    }

    #[test]
    fn oxy_trigger_inside_looped_sample_lands_in_loop_window() {
        // End-to-end: an Oxy trigger of `O40` (offset = 0x40 * 256 =
        // 16384 samples) on a looped sample whose loop window is
        // `[1024, 8192)` (span = 7168) must land at `1024 + (16384 -
        // 1024) mod 7168 = 1024 + 1024 = 2048`, not at 16384 (past the
        // sample-data end). Wire up a one-row pattern with the trigger
        // and confirm the player's channel sample_pos resolves to 2048.
        let mut h = synth_header();
        h.flags = 0;
        h.tracker_version = 0x1320;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let sample = SampleBody {
            pcm: vec![0i16; 8192],
            pcm_right: None,
            loop_start: 1024,
            loop_end: 8192,
            looped: true,
            volume: 64,
            c5_speed: 8363,
        };

        let mut pat = Pattern::empty(1);
        pat.rows[0][0] = Cell {
            note: 0x50, // C-5 by S3M's nibble-octave encoding
            instrument: 1,
            volume: 0xFF,
            command: cmd::O_SAMPLE_OFFSET,
            info: 0x40, // 0x40 * 256 = 16384 samples — past loop_end.
        };
        let mut p = PlayerState::new(&h, vec![sample], vec![pat], 44_100);
        p.enter_row();
        // Loop window is [1024, 8192) so 16384 folds to 2048.
        assert!(
            (p.channels[0].sample_pos - 2048.0).abs() < 1e-6,
            "Oxy on a looped sample must wrap into the loop window; got {}",
            p.channels[0].sample_pos
        );
    }

    /// Build a minimal one-channel, two-row pattern carrying the given
    /// effect commands so the §S0x double-trigger test can drive
    /// `enter_row` directly without spinning up a full module.
    fn s00_double_trigger_pattern(row0: Cell, row1: Cell) -> (S3mHeader, SampleBody, Pattern) {
        let mut h = synth_header();
        h.flags = 0;
        h.tracker_version = 0x1320;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let sample = SampleBody {
            pcm: vec![0i16; 4096],
            pcm_right: None,
            loop_start: 0,
            loop_end: 0,
            looped: false,
            volume: 32,
            c5_speed: 8363,
        };

        let mut pat = Pattern::empty(2);
        pat.rows[0][0] = row0;
        pat.rows[1][0] = row1;
        (h, sample, pat)
    }

    #[test]
    fn s00_repeating_sdx_double_triggers_at_tick_0_and_tick_x() {
        // Per the multimedia.cx behavioural reference
        // (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`)
        // §S0x: "When S00 is repeating a note delay (SDx), the note is
        // triggered twice: once on tick 0 (as if there's no note delay)
        // and again on tick x (as with a normal note delay)."
        //
        // Row 0: SD3 with a fresh note. ST3 stashes the trigger; no
        // tick-0 trigger fires. After this row the S effect-memory slot
        // holds 0xD3.
        //
        // Row 1: S00 with a fresh note. The infobyte resolves via the S
        // memory slot to 0xD3, so the row turns into "SD3 by recall". The
        // double-trigger rule applies: the new note must trigger at tick 0
        // AND the deferred copy must fire at tick 3.
        let row0 = Cell {
            note: 0x50,
            instrument: 1,
            volume: 0xFF,
            command: cmd::S_EXTENDED,
            info: 0xD3,
        };
        let row1 = Cell {
            note: 0x53, // distinct from row 0 so we can tell triggers apart
            instrument: 1,
            volume: 0xFF,
            command: cmd::S_EXTENDED,
            info: 0x00, // raw infobyte 0 — must recall 0xD3 from memory
        };
        let (h, sample, pat) = s00_double_trigger_pattern(row0, row1);
        let mut p = PlayerState::new(&h, vec![sample], vec![pat], 44_100);

        // Row 0 (SD3): stash only — channel must NOT be triggered at tick 0.
        p.enter_row();
        assert!(
            !p.channels[0].active,
            "row-0 SD3 must defer; channel must not be active at tick 0"
        );
        assert_eq!(
            p.channels[0].pending_delay.map(|pd| pd.fire_tick),
            Some(3),
            "row-0 SD3 must stash fire_tick=3"
        );
        assert_eq!(p.channels[0].effect_memory[cmd::S_EXTENDED as usize], 0xD3);

        // Drive ticks 1..speed so the SD3 fires at tick 3, ending row 0
        // with the row-0 note as the last triggered note.
        for t in 1..p.speed {
            p.tick = t;
            p.apply_per_tick();
        }
        assert_eq!(
            p.channels[0].last_note, 0x50,
            "SD3 must fire at tick 3 — row-0 note must be the last trigger"
        );

        // Advance to row 1 — S00 by raw bytes, but effect memory recalls
        // 0xD3. The double-trigger rule says: trigger NOW (tick 0) and
        // also stash the deferred copy for tick 3.
        p.row = 1;
        p.enter_row();
        assert!(
            p.channels[0].active,
            "S00→SDx tick-0 leg must trigger immediately (channel active)"
        );
        assert_eq!(
            p.channels[0].last_note, 0x53,
            "S00→SDx must trigger row-1's note at tick 0"
        );
        // The deferred copy must also be armed for tick 3.
        assert_eq!(
            p.channels[0].pending_delay.map(|pd| pd.fire_tick),
            Some(3),
            "S00→SDx must ALSO stash a deferred trigger for tick x"
        );
        assert_eq!(
            p.channels[0].pending_delay.map(|pd| pd.note),
            Some(0x53),
            "deferred trigger must carry the same row's note"
        );

        // Walk to tick 3 — the stashed trigger fires again, resetting
        // sample_pos to 0 and re-driving the note. To observe the second
        // trigger distinctly, advance the cursor away from zero first.
        p.channels[0].sample_pos = 1234.0;
        for t in 1..=3u8 {
            p.tick = t;
            p.apply_per_tick();
        }
        assert!(
            p.channels[0].pending_delay.is_none(),
            "deferred trigger must be cleared after firing"
        );
        assert_eq!(
            p.channels[0].sample_pos, 0.0,
            "tick-3 SDx fire must retrigger the sample (sample_pos reset)"
        );
    }

    #[test]
    fn freshly_written_sdx_keeps_single_trigger() {
        // Negative control for the S00 double-trigger rule: a row that
        // carries an SDx with a *nonzero raw infobyte* (i.e. a regular
        // SDx, not an S00 memory recall) must still defer the trigger
        // *only* — no double-fire. Confirms `is_s00_repeat_sdx` is not
        // matching the generic SDx path.
        let row0 = Cell {
            note: 0x50,
            instrument: 1,
            volume: 0xFF,
            command: cmd::S_EXTENDED,
            info: 0xD3, // raw SD3, not recalled
        };
        let row1 = Cell::EMPTY;
        let (h, sample, pat) = s00_double_trigger_pattern(row0, row1);
        let mut p = PlayerState::new(&h, vec![sample], vec![pat], 44_100);

        p.enter_row();
        assert!(
            !p.channels[0].active,
            "raw SD3 must NOT trigger at tick 0 (the single-trigger contract)"
        );
        assert_eq!(
            p.channels[0].pending_delay.map(|pd| pd.fire_tick),
            Some(3),
            "raw SD3 must stash the deferred trigger for tick 3"
        );
    }

    /// Build a single-PCM-channel player from an explicit list of patterns and
    /// an order table, so multi-pattern SBx (pattern-loop) scope can be
    /// exercised directly.
    fn multi_pattern_player(order: Vec<u8>, patterns: Vec<Pattern>) -> PlayerState {
        let mut h = synth_header();
        h.flags = 0;
        h.tracker_version = 0x1320;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.global_volume = 64;
        h.initial_speed = 6;
        h.initial_tempo = 125;
        h.enabled_channels = 1;
        h.order = order;
        let samples = vec![dummy_sample(4096)];
        PlayerState::new(&h, samples, patterns, 44_100)
    }

    #[test]
    fn pattern_loop_start_resets_at_pattern_boundary() {
        // §SBx: "you should not try looping back to a loop point in another
        // pattern! ... If you don't enter an SB0 command, the loop defaults to
        // the beginning of the pattern."
        //
        // Pattern 0 sets a loop start at row 3 (SB0). We then advance into
        // pattern 1, whose row 0 carries SB1 *without* a preceding SB0. The
        // loop must default to the top of pattern 1 (row 0), NOT jump back to
        // the stale row-3 start inherited from pattern 0.
        let mut pat0 = Pattern::empty(1);
        pat0.rows[3][0] = Cell {
            note: 0xFF,
            instrument: 0,
            volume: 0xFF,
            command: cmd::S_EXTENDED,
            info: 0xB0, // SB0 — set loop start at row 3
        };
        let mut pat1 = Pattern::empty(1);
        pat1.rows[0][0] = Cell {
            note: 0xFF,
            instrument: 0,
            volume: 0xFF,
            command: cmd::S_EXTENDED,
            info: 0xB1, // SB1 — loop once, no prior SB0 this pattern
        };
        let mut p = multi_pattern_player(vec![0u8, 1u8], vec![pat0, pat1]);

        // Drive pattern 0 up to and including row 3 so SB0 records row 3.
        for r in 0..=3u8 {
            p.tick = 0;
            p.row = r;
            p.enter_row();
        }
        assert_eq!(
            p.loop_start_row, 3,
            "pattern 0's SB0 must record the loop start at row 3"
        );

        // Move into pattern 1, row 0.
        p.order_index = 1;
        p.tick = 0;
        p.row = 0;
        p.enter_row();
        // The boundary crossing must have reset the stale loop start to 0.
        assert_eq!(
            p.loop_start_row, 0,
            "entering a new pattern must reset the loop start to row 0"
        );
        assert_eq!(
            p.loop_pattern_order,
            Some(1),
            "loop scope must now be bound to the new order slot"
        );

        // SB1 on pattern 1 row 0 must loop back to row 0 of pattern 1, not to
        // pattern 0's row 3.
        let jump = p.pending_jump.expect("SB1 must arm a loop jump");
        assert_eq!(
            jump.order,
            Some(1),
            "loop stays within pattern 1's order slot"
        );
        assert_eq!(
            jump.row, 0,
            "loop target must default to the top of pattern 1"
        );
    }

    #[test]
    fn within_pattern_loop_keeps_its_start_row() {
        // A within-pattern SBx loop must NOT have its loop start clobbered by
        // the per-pattern reset: the reset only fires when `order_index`
        // changes, and an SBx loop-back stays on the same order slot.
        let mut pat0 = Pattern::empty(1);
        pat0.rows[2][0] = Cell {
            note: 0xFF,
            instrument: 0,
            volume: 0xFF,
            command: cmd::S_EXTENDED,
            info: 0xB0, // SB0 at row 2
        };
        pat0.rows[5][0] = Cell {
            note: 0xFF,
            instrument: 0,
            volume: 0xFF,
            command: cmd::S_EXTENDED,
            info: 0xB2, // SB2 — loop back to row 2 twice
        };
        let mut p = multi_pattern_player(vec![0u8], vec![pat0]);

        for r in 0..=2u8 {
            p.tick = 0;
            p.row = r;
            p.enter_row();
        }
        assert_eq!(p.loop_start_row, 2, "SB0 records loop start at row 2");

        // Row 5: SB2 fires; loop target must be the recorded row 2.
        p.tick = 0;
        p.row = 5;
        p.enter_row();
        let jump = p.pending_jump.expect("SB2 must arm a loop jump");
        assert_eq!(jump.order, Some(0), "within-pattern loop stays on order 0");
        assert_eq!(
            jump.row, 2,
            "within-pattern loop must honour the SB0 start row, not reset to 0"
        );
        // Re-entering row 2 on the *same* order slot must not wipe the count.
        p.order_index = 0;
        p.tick = 0;
        p.row = 2;
        p.enter_row();
        assert_eq!(
            p.loop_pattern_order,
            Some(0),
            "staying on the same order slot must not re-arm the per-pattern reset"
        );
        assert_eq!(
            p.loop_start_row, 2,
            "the loop start row must survive a within-pattern loop-back"
        );
    }

    /// Build an 8-channel player whose pattern-0 row 0 carries the given
    /// `(channel, command, info)` effect cells, so same-row Bxx / Cxx
    /// channel precedence can be exercised. `order` is the order table.
    fn bc_row_player(cells: &[(usize, u8, u8)], order: Vec<u8>) -> PlayerState {
        let mut h = synth_header();
        h.flags = 0;
        h.tracker_version = 0x1320;
        h.channels = [0xFFu8; 32];
        h.muted = [true; 32];
        for i in 0..8 {
            h.channels[i] = i as u8;
            h.muted[i] = false;
        }
        h.pans = [8u8; 32];
        h.global_volume = 64;
        h.initial_speed = 6;
        h.initial_tempo = 125;
        h.enabled_channels = 8;
        let n_pats = order.iter().copied().max().unwrap_or(0) as usize + 1;
        h.order = order;
        let mut pat = Pattern::empty(8);
        for &(ch, command, info) in cells {
            pat.rows[0][ch] = Cell {
                note: 0xFF,
                instrument: 0,
                volume: 0xFF,
                command,
                info,
            };
        }
        let patterns = vec![pat; n_pats];
        PlayerState::new(&h, vec![dummy_sample(4096)], patterns, 44_100)
    }

    // ---- Same-row Bxx + Cxx precedence, per
    // `docs/audio/trackers/s3m/s3m-position-jump-pattern-break-and-adpcm.md`
    // §Part 1: Bxx writes the target order, Cxx the target row; the two
    // merge instead of overwriting each other.

    #[test]
    fn bxx_plus_cxx_same_row_merge_order_from_b_row_from_c() {
        // Worked example 1: B02 in ch0, C16 in ch5 → order 02, row 16
        // (decimal from the BCD infobyte 0x16).
        let mut p = bc_row_player(
            &[(0, cmd::B_POS_JUMP, 0x02), (5, cmd::C_PAT_BREAK, 0x16)],
            vec![0, 1, 2, 3],
        );
        p.enter_row();
        let jump = p.pending_jump.expect("B+C must arm a jump");
        assert_eq!(jump.order, Some(2), "order comes from the Bxx");
        assert_eq!(jump.row, 16, "row comes from the Cxx (decimal)");
        p.next_row();
        assert_eq!(p.order_index, 2);
        assert_eq!(p.row, 16);
    }

    #[test]
    fn cxx_before_bxx_merges_identically() {
        // Worked example 2: the merge is channel-order-insensitive because
        // the two effects write *different* variables — C16 in ch0 with
        // B02 in ch5 must land on the same order 02, row 16.
        let mut p = bc_row_player(
            &[(0, cmd::C_PAT_BREAK, 0x16), (5, cmd::B_POS_JUMP, 0x02)],
            vec![0, 1, 2, 3],
        );
        p.enter_row();
        let jump = p.pending_jump.expect("C+B must arm a jump");
        assert_eq!(
            jump.order,
            Some(2),
            "a later Bxx must not discard the earlier Cxx's row — order still from B"
        );
        assert_eq!(jump.row, 16, "row survives from the earlier Cxx");
    }

    #[test]
    fn bare_bxx_targets_row_zero_of_the_named_order() {
        // Worked example 3: B02 alone → order 02, row 0.
        let mut p = bc_row_player(&[(3, cmd::B_POS_JUMP, 0x02)], vec![0, 1, 2, 3]);
        p.enter_row();
        let jump = p.pending_jump.expect("Bxx must arm a jump");
        assert_eq!(jump.order, Some(2));
        assert_eq!(jump.row, 0);
    }

    #[test]
    fn bare_cxx_targets_next_order_at_its_row() {
        // Worked example 4: C16 alone on order 05 → order 06, row 16.
        let mut p = bc_row_player(&[(0, cmd::C_PAT_BREAK, 0x16)], vec![0; 8]);
        p.order_index = 5;
        p.enter_row();
        let jump = p.pending_jump.expect("Cxx must arm a jump");
        assert_eq!(jump.order, None, "bare Cxx advances to the next order");
        assert_eq!(jump.row, 16);
        p.next_row();
        assert_eq!(p.order_index, 6);
        assert_eq!(p.row, 16);
    }

    #[test]
    fn b00_plus_c00_restarts_the_song() {
        // Worked example 5: B00 + C00 → order 0, row 0. Both infobytes are
        // literal zeros — B/C carry no effect memory, so a zero parameter
        // must never be replaced by a stale remembered value.
        let mut p = bc_row_player(
            &[(0, cmd::B_POS_JUMP, 0x00), (1, cmd::C_PAT_BREAK, 0x00)],
            vec![0, 1],
        );
        p.order_index = 1;
        // Poison the channel memory slots to prove B00/C00 stay literal.
        p.channels[0].effect_memory[cmd::B_POS_JUMP as usize] = 0x07;
        p.channels[1].effect_memory[cmd::C_PAT_BREAK as usize] = 0x32;
        p.enter_row();
        let jump = p.pending_jump.expect("B00+C00 must arm a jump");
        assert_eq!(
            jump.order,
            Some(0),
            "B00 means order 0, not a memory recall"
        );
        assert_eq!(jump.row, 0, "C00 means row 0, not a memory recall");
    }

    #[test]
    fn two_bxx_on_one_row_rightmost_order_wins() {
        // Within one variable it is last-writer-wins across the
        // left-to-right channel scan.
        let mut p = bc_row_player(
            &[(0, cmd::B_POS_JUMP, 0x01), (4, cmd::B_POS_JUMP, 0x03)],
            vec![0, 1, 2, 3],
        );
        p.enter_row();
        let jump = p.pending_jump.expect("jump armed");
        assert_eq!(jump.order, Some(3), "right-most Bxx's order wins");
        assert_eq!(jump.row, 0);
    }

    #[test]
    fn two_cxx_on_one_row_rightmost_row_wins() {
        let mut p = bc_row_player(
            &[(0, cmd::C_PAT_BREAK, 0x08), (4, cmd::C_PAT_BREAK, 0x12)],
            vec![0, 0],
        );
        p.enter_row();
        let jump = p.pending_jump.expect("jump armed");
        assert_eq!(jump.order, None);
        assert_eq!(jump.row, 12, "right-most Cxx's (decimal) row wins");
    }

    #[test]
    fn invalid_cxx_leaves_an_earlier_valid_row_write_intact() {
        // A Cxx targeting row >= 64 is ignored entirely (multimedia.cx
        // §Cxy), so it must not clobber a valid same-row Cxx to its left.
        let mut p = bc_row_player(
            &[
                (0, cmd::C_PAT_BREAK, 0x16), // row 16 — valid
                (4, cmd::C_PAT_BREAK, 0x99), // row 99 — ignored
            ],
            vec![0, 0],
        );
        p.enter_row();
        let jump = p.pending_jump.expect("the valid Cxx still arms a jump");
        assert_eq!(jump.row, 16);
    }

    #[test]
    fn bxx_with_only_an_invalid_cxx_falls_back_to_row_zero() {
        let mut p = bc_row_player(
            &[
                (0, cmd::B_POS_JUMP, 0x02),
                (1, cmd::C_PAT_BREAK, 0x64), // row 64 — out of range, ignored
            ],
            vec![0, 1, 2, 3],
        );
        p.enter_row();
        let jump = p.pending_jump.expect("Bxx alone still arms a jump");
        assert_eq!(jump.order, Some(2));
        assert_eq!(jump.row, 0, "the ignored Cxx contributes no row");
    }

    #[test]
    fn sbx_pattern_loop_still_overrides_a_same_row_b_c_merge() {
        // The SBx loop-back keeps its documented priority over a same-row
        // jump: the merged B/C destination is discarded while the loop
        // counter is live.
        let mut p = bc_row_player(
            &[
                (0, cmd::B_POS_JUMP, 0x02),
                (1, cmd::C_PAT_BREAK, 0x16),
                (2, cmd::S_EXTENDED, 0xB1), // SB1 — loop back once
            ],
            vec![0, 1, 2, 3],
        );
        p.enter_row();
        let jump = p.pending_jump.expect("SB1 arms the loop jump");
        assert_eq!(jump.order, Some(0), "loop stays on the current order");
        assert_eq!(jump.row, 0, "loop target is the (default) loop start row");
    }

    #[test]
    fn axx_and_txx_zero_parameters_are_ignored_not_memory_recalled() {
        // Axx / Txx carry no effect memory (unmarked in the multimedia.cx
        // effect list): A00 / T00-with-value-below-33 are ignored per their
        // own rules even when the channel holds a stale nonzero parameter.
        let mut p = bc_row_player(
            &[(0, cmd::A_SET_SPEED, 0x00), (1, cmd::T_SET_TEMPO, 0x00)],
            vec![0, 0],
        );
        p.channels[0].effect_memory[cmd::A_SET_SPEED as usize] = 0x02;
        p.channels[1].effect_memory[cmd::T_SET_TEMPO as usize] = 0xF0;
        let (speed, bpm) = (p.speed, p.bpm);
        p.enter_row();
        assert_eq!(p.speed, speed, "A00 must be ignored, not recall speed 2");
        assert_eq!(p.bpm, bpm, "T00 must be ignored, not recall tempo 0xF0");
    }

    #[test]
    fn v00_sets_global_volume_to_zero_literally() {
        // V00 is a *valid* "set global volume to 0" — with no effect
        // memory, a stale remembered Vxx parameter must not resurrect it.
        let mut p = bc_row_player(&[(0, cmd::V_GLOBAL_VOL, 0x00)], vec![0, 0]);
        p.channels[0].effect_memory[cmd::V_GLOBAL_VOL as usize] = 0x20;
        p.enter_row();
        p.tick = 1;
        p.apply_per_tick(); // Vxx drains on tick 1
        assert_eq!(
            p.global_volume, 0,
            "V00 must set the global volume to 0, not recall 0x20"
        );
    }

    /// Build a single-channel player carrying two distinct PCM instruments so
    /// the note-vs-sample handling rules can be exercised. Instrument 1 has a
    /// loud default volume + C5 8363; instrument 2 has a quieter default + a
    /// different C5 so a sample swap is observable.
    fn two_sample_player(cells: &[Cell]) -> PlayerState {
        let mut h = synth_header();
        h.flags = 0;
        h.tracker_version = 0x1320;
        h.channels = [0xFFu8; 32];
        h.channels[0] = 0;
        h.muted = [true; 32];
        h.muted[0] = false;
        h.pans = [8u8; 32];
        h.global_volume = 64;
        h.initial_speed = 6;
        h.initial_tempo = 125;
        h.enabled_channels = 1;
        h.order = vec![0u8];

        let mut pat = Pattern::empty(1);
        for (row, cell) in cells.iter().enumerate() {
            pat.rows[row][0] = *cell;
        }
        let s1 = SampleBody {
            volume: 60,
            c5_speed: 8363,
            ..dummy_sample(4096)
        };
        let s2 = SampleBody {
            volume: 20,
            c5_speed: 16000,
            ..dummy_sample(4096)
        };
        PlayerState::new(&h, vec![s1, s2], vec![pat], 44_100)
    }

    #[test]
    fn note_without_instrument_retriggers_keeping_volume() {
        // §"note/sample handling": "Note w/o sample results in retriggering
        // the note without resetting the volume." A bare note (no instrument
        // column) must reset the sample cursor to 0 (retrigger) yet preserve
        // whatever active volume the channel already carried.
        let cells = [
            // Row 0: full trigger of instrument 1 (loads default volume 60).
            Cell {
                note: 0x40,
                instrument: 1,
                volume: 0xFF,
                command: 0,
                info: 0,
            },
            // Row 1: bare note, no instrument number.
            Cell {
                note: 0x48,
                instrument: 0,
                volume: 0xFF,
                command: 0,
                info: 0,
            },
        ];
        let mut p = two_sample_player(&cells);
        p.enter_row();
        // Force a quieter live volume + advance the cursor so the retrigger /
        // volume-preservation are both observable.
        p.channels[0].volume = 30;
        p.channels[0].sample_pos = 1234.0;

        p.tick = 0;
        p.row = 1;
        p.enter_row();
        assert_eq!(
            p.channels[0].sample_pos, 0.0,
            "a note with no instrument must retrigger (cursor → 0)"
        );
        assert_eq!(
            p.channels[0].volume, 30,
            "a note with no instrument must NOT reset the volume"
        );
        assert_eq!(
            p.channels[0].instrument, 1,
            "the instrument is unchanged by a bare note"
        );
    }

    #[test]
    fn instrument_without_note_swaps_sample_resets_volume_no_retrigger() {
        // §"note/sample handling": "Sample w/o note results in resetting the
        // volume and switching samples w/o retriggering the note." An
        // instrument column with no note must (a) load the new sample's
        // default volume, (b) swap the instrument index, and (c) leave the
        // sample cursor where it was (no retrigger). The doc also notes the
        // pitch is NOT reconverted, so the running frequency stays put.
        let cells = [
            // Row 0: full trigger of instrument 1.
            Cell {
                note: 0x40,
                instrument: 1,
                volume: 0xFF,
                command: 0,
                info: 0,
            },
            // Row 1: instrument 2 with NO note.
            Cell {
                note: 0xFF,
                instrument: 2,
                volume: 0xFF,
                command: 0,
                info: 0,
            },
        ];
        let mut p = two_sample_player(&cells);
        p.enter_row();
        let freq_before = p.channels[0].frequency;
        p.channels[0].sample_pos = 2048.0;

        p.tick = 0;
        p.row = 1;
        p.enter_row();
        assert_eq!(
            p.channels[0].instrument, 2,
            "an instrument with no note must switch the sample"
        );
        assert_eq!(
            p.channels[0].volume, 20,
            "an instrument with no note must reset to the new sample's default volume"
        );
        assert_eq!(
            p.channels[0].sample_pos, 2048.0,
            "an instrument with no note must NOT retrigger (cursor unchanged)"
        );
        assert_eq!(
            p.channels[0].frequency, freq_before,
            "an instrument swap with no note keeps the running pitch (no period reconvert)"
        );
    }

    #[test]
    fn instrument_without_note_on_an_off_channel_stays_off() {
        // §"note/sample handling": "If the note is off, it will stay off — it
        // will not retrigger." An instrument number with no note arriving on a
        // channel that never sounded must not wake the voice.
        let cells = [Cell {
            note: 0xFF,
            instrument: 2,
            volume: 0xFF,
            command: 0,
            info: 0,
        }];
        let mut p = two_sample_player(&cells);
        assert!(!p.channels[0].active, "channel starts inactive");
        p.enter_row();
        assert!(
            !p.channels[0].active,
            "an instrument with no note must not activate a silent channel"
        );
    }
}
