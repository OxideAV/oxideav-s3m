# oxideav-s3m

[![CI](https://github.com/OxideAV/oxideav-s3m/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-s3m/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-s3m.svg)](https://crates.io/crates/oxideav-s3m) [![docs.rs](https://docs.rs/oxideav-s3m/badge.svg)](https://docs.rs/oxideav-s3m) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Scream Tracker 3 Module (S3M) container + codec for oxideav.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework — a pure-Rust media transcoding and streaming stack. Codec,
container, and filter crates are implemented from the spec (no C codec
libraries linked or wrapped, no `*-sys` crates).

## Features

- **Container** (`s3m`): probes the 4-byte `SCRM` magic at offset 44,
  parses header + instruments + patterns + pan table, and delivers the
  whole song as a single packet to the decoder.
- **Mixed-stereo codec** (`s3m`): renders 44.1 kHz interleaved
  signed-16-bit PCM with every S3M channel summed into one L/R pair.
  Linear-interpolation mixer, `sqrt(active-channels)` normalisation,
  master/global-volume gain.
- **Per-channel codec** (`s3m_multichannel`): same sample rate and
  format, but every S3M channel slot gets its own stereo pair, output
  interleaved as `[ch0_L, ch0_R, ch1_L, ch1_R, …]` (`channels = 64` on
  the emitted `AudioFrame`). Useful for DAWs, visualizers, and
  per-instrument remastering tools.

**Decode-only** by design — S3M is a tracker *source* format;
re-emitting one is out of scope.

## Decoder coverage

- **Tracker / version decode** — `S3mHeader::created_with_tracker()`
  splits the `Cwt/v` word into a 4-bit tracker ID + 12-bit version,
  returning a `CreatedWithTracker { raw, tracker, version }` with a
  `Tracker` enum (Scream Tracker, Imago Orpheus, Impulse Tracker,
  Schism Tracker, OpenMPT, `Other`). `is_st3_00()` is the strict
  `0x1300` sentinel; `auto_fast_slides()` is the broader
  Scream-Tracker-family `<= 0x1300` bound the fast-slides derivation
  keys off.
- **Canonical period-table pitch** (`note_to_frequency` /
  `PERIOD_TABLE`) — playback rate is derived from ST3's own 9-octave
  (108-entry) integer period table
  (`period = 8363 * PERIOD_TABLE[note] / c2spd`, then
  `freq = AMIGA_CLOCK_HZ / period`), preserving the within-octave
  ratios and the integer period truncation of real ST3 rather than a
  pure equal-tempered approximation.
- **PCM instruments** — 8-bit signed/unsigned, 16-bit, mono and
  true-stereo. The Length / Loop start / Loop end / C-frequency fields
  are each masked to their lower 16 bits, per the ST3 instrument-format
  reference. Looped voices wrap at `loop_end` (the half-open
  `[loop_start, loop_end)` window), never the physical PCM buffer
  length — the post-loop tail is silent and the linear interpolator's
  next-frame folds back to `loop_start` at the boundary, matching ST3's
  loop-clipping (FireLight §2.10). One-shot voices still run to the
  buffer end and deactivate.
- **AdLib / OPL2 instruments** — `SCRI` instruments (type 2..=7) now have
  their YM3812 register block decoded. `Instrument::adlib_instrument()`
  unpacks the modulator + carrier operator parameters (AM/VIB/EGT/KSR/MUL,
  KSL/TL, AR/DR, SL/RR, waveform) plus the channel feedback/connection
  byte, per the ST3 AdLib instrument layout and the OPL register map. The
  [`opl2`] module implements the YM3812 **operator core**: the
  bit-for-bit decapsulated log-sin and exponential ROM tables, the phase
  generator (`((fnum * mlTab[ML]) << block) >> 1`), full-period sine
  reconstruction from the stored first quadrant, the half-wave-rectified
  sine, and the MUL / FB tables. Each piece is unit-tested against the
  decapsulation-article anchor values and the −3 dB-per-volume-step
  exp/log identity. This is the deterministic, fully-documented foundation
  for AdLib playback. It does **not yet synthesize audio**: the OPL2
  envelope-generator *rate schedule* (the 9-bit / 96 dB EG attack / decay /
  release increment timing) is not present in the staged clean-room docs —
  only the OPLL's 7-bit / 48 dB EG is reverse-engineered, and even its
  attack-level recurrence is an open gap. An OPL2-specific envelope-rate
  trace is needed before the operator core can be wired into the mixer.
- **Channel mute flag** (`+128` in the channel-settings byte) — the
  decoder reads pattern cells for muted channels (so jumps, loops, and
  pattern delays stay consistent) but the mixer silences their output.
  AdLib slots without the flag are also reported as muted.
- **Effects** — the full command set: `Axx` (speed), `Bxx` (pos jump),
  `Cxx` (pattern break), `Dxy` volume slide (full case matrix including
  the fine forms and the both-nibbles-nonzero quirk), `Exx` / `Fxx`
  pitch slides (with fine + extra-fine forms), `Gxx` (tone portamento,
  including the empty-note-targets-last-note rule), `Hxy` (vibrato — a
  256-cycle phase stepped by `speed * 4` and sampled at `phase / 4`,
  matching the FireLight §6.8 signed `-32..+31` pointer and the same
  phase model `Rxy` tremolo uses, so a full oscillation takes `64 / speed`
  ticks rather than a quarter of that),
  `Ixy` (tremor), `Jxy` (arpeggio, resolved through the period table),
  `Kxy` (vibrato + volume), `Lxy` (portamento + volume), `Oxx` (sample
  offset, honouring the loop window on looped samples), `Qxy`
  (retrigger, with the full volume-modifier table), `Rxy` (tremolo),
  `Txx` (tempo), `Uxy` (fine vibrato), `Vxx` (global volume, with the
  range / tick-1 / speed-1 quirks **and the per-voice latch — see
  below**), `Xxx` (set pan), and the `Sxy`
  family (`S1x` glissando, `S2x` finetune, `S3x`/`S4x` waveform select
  (selectors 0/1/2 are sine/ramp-down/square; selector 3 "random"
  reuses the sine table per FireLight §6.8/§6.15 `case 3: just use
  sine`, matching ST3's never-shipped noise LFO),
  `S80` pan, `SAx` legacy "old
  stereo control" — bank-keyed
  normal/reversed/center mapping (`SA0`/`SA2` keep the channel's L/R bank,
  `SA1`/`SA3` swap it, `SA4`–`SA7` centre, `SA8`–`SAF` no-op), `SBx`
  pattern loop (per-pattern scoped — the `SB0` loop start resets to the
  top of the pattern at every pattern boundary, and an `SBx` with no
  preceding `SB0` defaults to row 0, so a stale loop point can never
  bleed across patterns), `SCx` note cut / freeze, `SDx` note delay,
  `SEx` pattern delay). `S0x` filter and
  `SFx` funkrepeat decode as no-ops (not implemented in ST3 itself).
- **Per-voice global volume (`Vxx`)** — global volume is *latched into
  each voice* at the moment its note volume was last written, not applied
  as a single player-wide scalar at mix-down. Per the multimedia.cx
  behavioural reference §Vxx ("It does not affect past notes, that are
  still playing, unless their volume is changed, which applies the new
  global volume to that voice"), a `Vxx` updates the player-wide global
  volume but does **not** retroactively rescale voices that are already
  sounding — only a note trigger, volume-column write, instrument reload
  (including the `SDx`-deferred forms), or any active-volume effect
  (`Dxy`/`Kxy`/`Lxy` slide, `Qxy` modifier, `Rxy` tremolo, `Ixy` tremor)
  re-latches the global volume live at that write. The tick ordering
  follows for free: a same-row trigger (tick 0) keeps the pre-`Vxx`
  value because `Vxx` drains on tick 1, while an `SDx`-deferred trigger
  or a later-tick volume slide picks up the post-`Vxx` value — matching
  the §Vxx "applied to notes that have a note delay" and "applied if
  anything updates the note volume on tick 1 or tick 2" clauses. The
  mixer scales each voice by its own latched value in both the
  mixed-stereo and per-channel render paths.
- **Effect memory** — channels remember the latest nonzero parameter
  per command and substitute it back when a row carries the same
  command with parameter 0; `H` / `U` and the `Sxy` family share their
  slots.
- **Header-flag-driven playback modes** — fast slides (flag bit 6, or
  `CwtV == 0x1300`) and Amiga period limits (flag bit 4, clamping
  playback to the PAL Amiga period range `[113, 856]` across note
  triggers, finetune, pitch slides, portamento, vibrato, and arpeggio).
- **Spec edge cases** — the initial-speed byte is ignored when `0` or
  `255` and the initial-tempo byte when below `33` (both fall back to
  `speed = 6`, `bpm = 125`); per-channel default pan resolves from the
  master-volume stereo flag with the mono-mode centre override; the
  master-volume byte is clamped to `[16, 127]` with the `* 11/8` stereo
  multiplier; and active volume peaks at 63 (not 64) on the PCM path.
- **Hostile-input hardening** — every byte of a `.s3m` is
  attacker-controlled, so the whole pipeline (`parse_header` →
  `extract_samples` → `unpack_all` → render, plus the registered
  `CodecRegistry` decoder API) is fuzzed against truncation prefixes,
  random buffers, and byte-mutations of both a minimal and a
  feature-rich seed module. A corrupt effect-command byte outside the
  `A`–`Z` range no longer indexes the effect-memory table out of bounds,
  and a truncated true-stereo sample body is split at the declared
  per-channel length so its left/right boundary matches the file's
  intent. Malformed modules resolve to a typed error or a bounded, silent
  render — never a panic, out-of-bounds read, or hang.
- **Known format gaps** — the `DP30ADPCM` packed-sample format is left
  as raw PCM because the format reference states no documentation exists
  for how it works, and the same-row `Bxx` + `Cxx` (position-jump plus
  pattern-break) combination is not specified by the staged references,
  so its precedence is left as the last-cell-wins default rather than
  guessed.

## Usage

```toml
[dependencies]
oxideav-s3m = "0.0"
```

```rust,no_run
use oxideav_codec::CodecRegistry;
use oxideav_container::ContainerRegistry;

let mut containers = ContainerRegistry::new();
let mut codecs = CodecRegistry::new();
oxideav_s3m::register_containers(&mut containers);
oxideav_s3m::register_codecs(&mut codecs);

// Mixed-stereo output: build a decoder under the `s3m` id.
// Per-channel output: use `oxideav_s3m::CODEC_ID_MULTICHANNEL` instead.
```

For lower-level access, [`player::PlayerState`] exposes both `render`
(mixed) and `render_per_channel` (one stereo pair per S3M channel)
directly, bypassing the codec-registry wrapper.

## License

MIT — see [LICENSE](LICENSE).
