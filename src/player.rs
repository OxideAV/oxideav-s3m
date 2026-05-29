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
/// returning the new clamped (0..=64) volume.
///
/// Table from `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
/// §Qxy ("Values for x"). `x == 0` and `x == 8` are documented no-ops
/// ("0" and "?" respectively). `x == 6` uses the [`Q_TWO_THIRDS`] table
/// rather than an exact `vol*2/3`.
fn retrigger_volume(vol: u8, x: u8) -> u8 {
    let v = vol.min(64);
    match x {
        0x1 => v.saturating_sub(1),
        0x2 => v.saturating_sub(2),
        0x3 => v.saturating_sub(4),
        0x4 => v.saturating_sub(8),
        0x5 => v.saturating_sub(16),
        0x6 => Q_TWO_THIRDS[v.min(63) as usize],
        0x7 => v / 2,
        0x9 => (v as u16 + 1).min(64) as u8,
        0xA => (v as u16 + 2).min(64) as u8,
        0xB => (v as u16 + 4).min(64) as u8,
        0xC => (v as u16 + 8).min(64) as u8,
        0xD => (v as u16 + 16).min(64) as u8,
        0xE => ((v as u16) * 3 / 2).min(64) as u8,
        0xF => (v as u16 * 2).min(64) as u8,
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
    /// Random sample-per-tick LFO. Implemented with a 32-bit LCG so the
    /// output is reproducible across runs of the same module.
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
    /// Current per-channel volume 0..=64.
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
    /// Vibrato phase in table units (0..=63).
    pub vibrato_pos: u8,
    /// Tremolo phase in table units (0..=63).
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
    /// Per-channel LFO state for the `Random` waveform. Updated lazily;
    /// only stepped when a vibrato / tremolo tick samples it.
    pub random_state: u32,
    /// Qxy retrigger tick counter. Incremented on every tick (including
    /// tick 0); when it reaches/exceeds the retrig value `y` the sample is
    /// retriggered and the counter resets to 0. Per the multimedia.cx
    /// behavioural reference (§Qxy) this counter is *global to the channel
    /// across rows* — a new note with Qxy does NOT reset it; only a row
    /// without the Qxy effect (or song start) clears it back to 0.
    pub retrig_counter: u8,
    /// Ixy tremor: counts ticks within the on/off cycle. Bit 7 = "in off
    /// phase"; low 7 bits = ticks remaining in the current half-cycle.
    pub tremor_phase: u8,
    /// Volume captured when a row's Ixy command first fires, so we can
    /// restore it after an off-phase. 0xFF = "no captured value yet".
    pub tremor_base_volume: u8,
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
            random_state: 0x1234_5678,
            retrig_counter: 0,
            tremor_phase: 0,
            tremor_base_volume: 0xFF,
            keep_vibrato_pos_on_new_note: false,
            keep_tremolo_pos_on_new_note: false,
            effect_memory: [0; 27],
            muted: false,
            frozen: false,
        }
    }
}

/// Convert an S3M note byte (octave << 4 | semitone) and C5 speed (Hz)
/// into a playback frequency.
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
    // Byte 0x40 → n = 48 is the c5_speed reference.
    let c5_n = 4 * 12;
    let delta = n - c5_n;
    (c5_speed as f32) * 2.0f32.powf(delta as f32 / 12.0)
}

/// Sample the selected vibrato/tremolo waveform at table position `pos`
/// (0..=63), returning a signed integer in the range -64..=64.
///
/// `Random` consumes one step of the channel's LCG. The LCG (Numerical
/// Recipes' 32-bit ranqd1) is reproducible across runs and lets each
/// channel keep its own independent noise without pulling rand crates.
fn waveform_sample(wf: Waveform, pos: u8, rng: &mut u32) -> i32 {
    match wf {
        Waveform::Sine => {
            // Sine in [-64,+64] sampled from a 64-entry period.
            let phase = (pos as f32) * (std::f32::consts::PI * 2.0 / 64.0);
            (phase.sin() * 64.0) as i32
        }
        Waveform::RampDown => {
            // Ramp from +64 at pos=0 down to ~-62 at pos=63.
            let p = pos & 0x3F;
            64 - (p as i32) * 2
        }
        Waveform::Square => {
            if pos & 0x20 == 0 {
                64
            } else {
                -64
            }
        }
        Waveform::Random => {
            // Numerical-recipes ranqd1: x' = 1664525 * x + 1013904223.
            *rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // Take the top 8 bits as a signed value, scaled to ±64.
            let signed = (*rng >> 24) as i8;
            (signed as i32 / 2).clamp(-64, 64)
        }
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
    /// Master volume from the file header (0..=127). Applied as a global
    /// gain on top of `global_volume`.
    pub master_volume: u8,
    /// Number of channels actually carrying PCM/AdLib in the file.
    /// Used as the mixer's normalisation divisor — dividing by all 32
    /// slots makes typical 4–8 channel modules far too quiet.
    pub active_channels: u8,

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
    /// SEx pattern delay: when non-zero we hold the current row, replaying
    /// all per-tick effects but not re-triggering notes. Decremented at
    /// the end of each row-cycle until zero.
    pattern_delay_remaining: u8,
    /// `true` when `next_row` consumed an SEx replay and the *next*
    /// `enter_row` call should skip cell application. Reset every time
    /// the row is fully advanced.
    replaying_for_pattern_delay: bool,
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
                ..Channel::default()
            })
            .collect();
        let initial_pan = header.pans.to_vec();

        let speed = if header.initial_speed == 0 {
            DEFAULT_SPEED
        } else {
            header.initial_speed
        };
        let bpm = if header.initial_tempo == 0 {
            DEFAULT_BPM
        } else {
            header.initial_tempo
        };

        PlayerState {
            samples,
            patterns,
            order: header.order.clone(),
            channels,
            initial_pan,
            speed,
            bpm,
            global_volume: header.global_volume.min(64),
            master_volume: header.master_volume.min(127),
            active_channels: header.enabled_channels.max(1),
            order_index: 0,
            row: 0,
            tick: 0,
            tick_sample_cursor: 0,
            sample_rate,
            ended: false,
            pending_jump: None,
            loop_start_row: 0,
            loop_count: None,
            pattern_delay_remaining: 0,
            replaying_for_pattern_delay: false,
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

        // SEx pattern delay: when we're in the middle of a held row, the
        // notes are *not* re-triggered, but the row's tick-0 effects are
        // also not re-armed (per-tick effects keep firing because the
        // channel state still carries `command` / `info` from the
        // original entry). We just skip the cell-application phase.
        let replaying_held_row = self.replaying_for_pattern_delay;

        let mut row_speed: Option<u8> = None;
        let mut row_tempo: Option<u8> = None;
        let mut row_global_vol: Option<u8> = None;
        let mut row_jump: Option<Jump> = None;

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
            if ch.command != 0 {
                let slot = effect_memory_slot(ch.command);
                if ch.info == 0 {
                    ch.info = ch.effect_memory[slot as usize];
                } else {
                    ch.effect_memory[slot as usize] = ch.info;
                }
            }
            // Clear any leftover delayed trigger from a prior row.
            ch.pending_delay = None;
            // Reset Ixy tremor each row (matches the spec: each Ixy event
            // starts the on-cycle fresh from the channel's current vol).
            ch.tremor_phase = 0;
            ch.tremor_base_volume = 0xFF;
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

            if is_note_delay {
                ch.pending_delay = Some(PendingTrigger {
                    fire_tick: ch.info & 0x0F,
                    note: cell.note,
                    instrument: cell.instrument,
                    volume: cell.volume,
                });
            } else {
                // Instrument change reloads volume.
                if cell.instrument != 0 {
                    ch.instrument = cell.instrument;
                    if let Some(s) = self.samples.get(cell.instrument as usize - 1) {
                        ch.volume = s.volume;
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
                        } else {
                            ch.frequency = freq;
                            ch.target_frequency = freq;
                            // Re-apply Oxx sample offset if present.
                            if ch.command == cmd::O_SAMPLE_OFFSET {
                                let off = (ch.info as u64) * 256;
                                ch.sample_pos = off as f64;
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
                }

                // Explicit volume column.
                if cell.volume != 0xFF {
                    ch.volume = cell.volume.min(64);
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
                    row_jump = Some(Jump {
                        order: Some(ch.info),
                        row: 0,
                    });
                }
                cmd::C_PAT_BREAK => {
                    // Parameter is BCD (high nibble * 10 + low). Per the
                    // multimedia.cx behavioural reference, an out-of-range
                    // target (>= 64) makes ST3 ignore the effect entirely.
                    let r = (ch.info >> 4) * 10 + (ch.info & 0x0F);
                    if r < 64 {
                        row_jump = Some(Jump {
                            order: None,
                            row: r,
                        });
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
                        // manual that treated it as DFy=down-by-F.
                        ch.volume = (ch.volume as u16 + 15).min(64) as u8;
                    } else if x == 0xF && y == 0 {
                        // DF0: slide up by 15 on tick 0 (also fires on all
                        // subsequent ticks via apply_dxy).
                        ch.volume = (ch.volume as u16 + 15).min(64) as u8;
                    } else if x == 0 && y == 0xF {
                        // D0F: slide down by 15 on tick 0.
                        ch.volume = ch.volume.saturating_sub(15);
                    } else if x == 0xF && y != 0 {
                        // DFy: fine down by y.
                        ch.volume = ch.volume.saturating_sub(y);
                    } else if y == 0xF && x != 0 {
                        // DxF: fine up by x.
                        ch.volume = (ch.volume as u16 + x as u16).min(64) as u8;
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
                }
                cmd::F_SLIDE_UP if ch.info >= 0xE0 => {
                    // FFx: fine up. FEx: extra-fine up. Tick-0 only.
                    let amount = (ch.info & 0x0F) as f32;
                    let scale = if (ch.info & 0xF0) == 0xE0 { 0.25 } else { 1.0 };
                    let f = 2.0f32.powf(amount * scale / 768.0);
                    ch.frequency *= f;
                    ch.target_frequency *= f;
                }
                cmd::Q_RETRIGGER => {
                    // Qxy is processed on every tick *including tick 0*,
                    // so the counter must advance here too. A tick-0 retrig
                    // can fire immediately after the new note (per the
                    // multimedia.cx §Qxy "retrig on tick 0" note).
                    Self::apply_retrigger(ch);
                }
                cmd::T_SET_TEMPO if ch.info >= 0x20 => {
                    row_tempo = Some(ch.info);
                }
                cmd::V_GLOBAL_VOL => {
                    row_global_vol = Some(ch.info.min(64));
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
                        // SAx — legacy "stereo control" 16-position pan.
                        //
                        // Per the FireLight S3M Player Tutorial §6.23
                        // (`docs/audio/trackers/s3m/FireLight-S3M-Player-Tutorial.txt`)
                        // this is a pre-ST3 panning command kept around for
                        // compatibility with legacy modules such as
                        // PANIC.S3M and STRSHINE.S3M. The tutorial spells
                        // out the bit-swap algorithm:
                        //
                        //     if (eparmy > 7), then temp = eparmy - 8
                        //     else                  temp = eparmy + 8
                        //     setpan(temp)
                        //
                        // i.e. the high bit of the parameter nibble is
                        // toggled before it is written to the channel pan
                        // slot. The ST3.20 effects reference
                        // (`ScreamTracker-v3.20-effects.txt` §SAx) calls
                        // out PANIC by Future Crew as the canonical
                        // dependent: "The only .S3M file released that
                        // would support it is the soundtrack from Panic by
                        // Future Crew." ST3 itself stopped emitting SAx in
                        // new files (the editor uses S8x now), so this
                        // path only matters for back-catalogue playback.
                        0xA => ch.pan = p ^ 0x08,
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
        }

        if replaying_held_row {
            // Held rows do not re-trigger anything but still walk through
            // the per-tick path for the rest of the row.
            return;
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
        if let Some(g) = row_global_vol {
            self.global_volume = g;
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
        // Clone sample metadata we need for deferred SDx triggers. Can't
        // borrow `&self.samples` inside the mutable-channel loop.
        let samples_snapshot: Vec<(u32, u8)> = self
            .samples
            .iter()
            .map(|s| (s.c5_speed.max(1), s.volume))
            .collect();
        for ch in &mut self.channels {
            let x = ch.info >> 4;
            let y = ch.info & 0x0F;

            // SDx (note delay): fire the stashed trigger at tick x.
            if let Some(pd) = ch.pending_delay {
                if tick == pd.fire_tick {
                    // Apply the stashed cell data like `enter_row` would.
                    if pd.instrument != 0 {
                        ch.instrument = pd.instrument;
                        let idx = pd.instrument as usize;
                        if idx > 0 && idx <= samples_snapshot.len() {
                            ch.volume = samples_snapshot[idx - 1].1;
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
                        ch.volume = pd.volume.min(64);
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
                cmd::J_ARPEGGIO if (x | y) != 0 && ch.last_note != 0 => {
                    let semis = match tick % 3 {
                        0 => 0,
                        1 => x as i32,
                        _ => y as i32,
                    };
                    let mult = 2.0f32.powf(semis as f32 / 12.0);
                    ch.frequency = ch.target_frequency * mult;
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
                    Self::apply_dxy(ch, x, y);
                }
                // Qxy: retrigger note every y ticks with volume modifier x.
                // The full per-tick counter logic (incl. tick 0, run from
                // `enter_row`) lives in `apply_retrigger`.
                cmd::Q_RETRIGGER => Self::apply_retrigger(ch),
                cmd::R_TREMOLO => {
                    // Rxy: like vibrato but applied to volume.
                    let speed = x;
                    let depth = y;
                    if speed != 0 || depth != 0 {
                        ch.tremolo_pos = (ch.tremolo_pos.wrapping_add(speed * 4)) & 0x3F;
                        let wf = ch.tremolo_waveform;
                        let s = waveform_sample(wf, ch.tremolo_pos, &mut ch.random_state);
                        let delta = (s * depth as i32) / 64;
                        let v = (ch.volume as i32 + delta).clamp(0, 64);
                        ch.volume = v as u8;
                    }
                }
                cmd::D_VOL_SLIDE => Self::apply_dxy(ch, x, y),
                // Exx: portamento down. Each tick: freq *= 2^(-param/768).
                // Fine / extra-fine slides (0xEy / 0xFy) are tick-0 only
                // — skip on per-tick path.
                cmd::E_SLIDE_DOWN if ch.info != 0 && ch.info < 0xE0 => {
                    let f = 2.0f32.powf(-(ch.info as f32) / 768.0);
                    ch.frequency *= f;
                    ch.target_frequency *= f;
                }
                cmd::F_SLIDE_UP if ch.info != 0 && ch.info < 0xE0 => {
                    let f = 2.0f32.powf((ch.info as f32) / 768.0);
                    ch.frequency *= f;
                    ch.target_frequency *= f;
                }
                // Gxx: slide toward target at rate info/per tick.
                cmd::G_TONE_PORTA if ch.info != 0 && ch.target_frequency > 0.0 => {
                    Self::apply_tone_porta(ch, ch.info);
                }
                cmd::H_VIBRATO => {
                    // Hxy: vibrato. x = speed, y = depth.
                    Self::apply_vibrato(ch, x, y, /* fine: */ false);
                }
                // Ixy: tremor — flip the note on/off in (x+1)-on /
                // (y+1)-off cycles. Spec lets x=y=0 mean "always on" /
                // ineffective so the guard mirrors that.
                cmd::I_TREMOR if (x | y) != 0 => {
                    // Capture the row's starting volume the first time
                    // tremor fires, so we can restore it during on-phases.
                    if ch.tremor_base_volume == 0xFF {
                        ch.tremor_base_volume = ch.volume;
                    }
                    // tremor_phase counter resets each row in enter_row;
                    // every tick we step through the on→off→on cycle.
                    let on_ticks = x as u16 + 1;
                    let off_ticks = y as u16 + 1;
                    let period = (on_ticks + off_ticks).max(1);
                    let pos = (tick as u16) % period;
                    if pos < on_ticks {
                        ch.volume = ch.tremor_base_volume.min(64);
                    } else {
                        ch.volume = 0;
                    }
                }
                // Uxy: fine vibrato — four times more accurate than Hxy.
                cmd::U_FINE_VIBRATO => {
                    Self::apply_vibrato(ch, x, y, /* fine: */ true);
                }
                _ => {}
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
        ch.vibrato_pos = (ch.vibrato_pos.wrapping_add(speed * 4)) & 0x3F;
        let wf = ch.vibrato_waveform;
        let s = waveform_sample(wf, ch.vibrato_pos, &mut ch.random_state);
        let div = if fine { 512 } else { 128 };
        let delta = (s * depth as i32) / div;
        let mult = 2.0f32.powf(delta as f32 / 48.0);
        ch.frequency = ch.target_frequency * mult;
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
            // DF0: slide up by 15 on all ticks.
            ch.volume = (ch.volume as u16 + 15).min(64) as u8;
        } else if x == 0xF || y == 0xF {
            // DFy / DxF / DFF — fine, tick-0 only.
        } else if x != 0 && y == 0 {
            // Dx0: slide up.
            ch.volume = (ch.volume as u16 + x as u16).min(64) as u8;
        } else if y != 0 && x == 0 {
            // D0y: slide down.
            ch.volume = ch.volume.saturating_sub(y);
        } else if x != 0 && y != 0 {
            // Dxy with both nibbles 1..=E: ST3 quirk — slide DOWN by y.
            ch.volume = ch.volume.saturating_sub(y);
        }
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
                let len = samples[idx - 1].pcm.len() as f64;
                if ch.sample_pos < len {
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

        let len = body.pcm.len() as f64;
        if ch.sample_pos >= len {
            if body.is_looped() {
                let ls = body.loop_start as f64;
                let le = body.loop_end as f64;
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
        let next_idx = if i + 1 < n {
            i + 1
        } else if body.is_looped() {
            body.loop_start as usize
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

        let v = (ch.volume as f32) / 64.0;

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
        let mv = (self.master_volume.max(1) as f32) / 48.0;
        let gv = (self.global_volume as f32) / 64.0;
        let norm = (self.active_channels as f32).max(1.0).sqrt();
        let scale = mv * gv / norm;
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
        let mv = (self.master_volume.max(1) as f32) / 48.0;
        let gv = (self.global_volume as f32) / 64.0;
        let scale = mv * gv;
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
    fn waveform_square_is_bipolar_constant() {
        let mut rng = 0u32;
        // First half (pos < 32) → +64; second half → -64.
        assert_eq!(waveform_sample(Waveform::Square, 0, &mut rng), 64);
        assert_eq!(waveform_sample(Waveform::Square, 31, &mut rng), 64);
        assert_eq!(waveform_sample(Waveform::Square, 32, &mut rng), -64);
        assert_eq!(waveform_sample(Waveform::Square, 63, &mut rng), -64);
    }

    #[test]
    fn waveform_rampdown_decreases() {
        let mut rng = 0u32;
        let a = waveform_sample(Waveform::RampDown, 0, &mut rng);
        let b = waveform_sample(Waveform::RampDown, 16, &mut rng);
        let c = waveform_sample(Waveform::RampDown, 32, &mut rng);
        assert!(a > b && b > c, "ramp should be strictly decreasing");
        assert_eq!(a, 64);
        assert_eq!(c, 0);
    }

    #[test]
    fn waveform_random_changes_state() {
        let mut rng = 0x1234_5678u32;
        let initial = rng;
        let _ = waveform_sample(Waveform::Random, 0, &mut rng);
        assert_ne!(rng, initial, "random must advance the LCG");
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
        assert_eq!(retrigger_volume(32, 0xF), 64); // *2
                                                   // Clamping: subtract saturates at 0, add/multiply clamp to 64.
        assert_eq!(retrigger_volume(4, 0x5), 0);
        assert_eq!(retrigger_volume(60, 0xF), 64);
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
        // First vibrato step from pos 0 advances by speed*4 = 8; the sine
        // sample at pos 8 (sin(π/4)) is nonzero, so the depth-4 wobble must
        // shift the pitch off the carrier.
        PlayerState::apply_vibrato(&mut ch, h_speed, h_depth, false);
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

    #[test]
    fn s2x_table_endpoints_match_spec() {
        // From docs/audio/trackers/s3m/ScreamTracker-v3.20-effects.txt §S2x:
        //  S20 → 7895 Hz, S28 → 8363 Hz (no finetune), S2F → 8757 Hz.
        assert_eq!(S2X_FINETUNE_TABLE[0x0], 7895);
        assert_eq!(S2X_FINETUNE_TABLE[0x8], 8363);
        assert_eq!(S2X_FINETUNE_TABLE[0xF], 8757);
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
}
