# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **`SCx` (note cut) now freezes the channel instead of zeroing volume**,
  matching the multimedia.cx behavioural reference §SCx ("the volume is
  *not* set to 0. Instead playback is temporarily *frozen* and may be
  *resumed by a following Exx, Fxx, Gxx, Hxx, Jxx, Kxx, Lxx or Uxx
  command*"). Previously the cut-tick handler wrote `volume = 0`, which
  prevented spec-correct resumption: a later vibrato / portamento /
  arpeggio on the same channel needed an explicit volume command to
  become audible again. The new `Channel.frozen` flag silences the mixer
  output AND halts the sample read cursor, while leaving volume,
  frequency, and sample position intact for resume. A `new helper
  is_scx_resume_command` thaws the channel on the eight listed commands
  (E/F/G/H/J/K/L/U) and on any fresh note trigger — both immediate and
  `SDx`-deferred. Adds three tests (`frozen_channel_emits_silence_and_holds_position`,
  `is_scx_resume_command_covers_the_eight_resume_effects`, plus an
  integration test `effect_scx_thaws_on_following_h_command` exercising
  the full row-cycle).

### Added

- **Channel mute flag** (`+128` in the file header's channel-settings byte,
  per the ST3 archive-team format reference
  `docs/audio/trackers/s3m/ScreamTracker-v3.20-s3m.txt`: "Channel settings
  for 32 channels, 255=unused, +128=disabled"). The decoder now exposes a
  per-slot `S3mHeader::muted` array and the mixer silences muted channels
  while still parsing their pattern data — pattern jumps, SBx loop counters,
  and SEx pattern delays stay consistent with the real ST3 behaviour. AdLib
  slots (channel type 16..=31) without the `+128` flag are also reported as
  muted since the PCM mixer does not synthesise OPL voices. Adds three
  header parser tests, three mixer-level player tests, and one end-to-end
  multichannel-decoder integration test covering the silence guarantee.

### Changed

- **Kxy / Lxy dual commands** corrected to the multimedia.cx behavioural
  reference (§Kxy / §Lxy). `Kxy` is `H00 + Dxy`: the vibrato leg now
  *continues the channel's running vibrato* using the remembered H/U
  speed/depth from effect memory rather than re-using Kxy's own `(x, y)`
  nibbles (which are the volume slide). `Lxy` is `G00 + Dxy`: the porta
  leg now continues tone portamento at the remembered G rate instead of
  passing Lxy's infobyte to the porta kernel. Additionally, a *fine*
  volume-slide form (`DFy` / `DxF` / `DFF`) in either infobyte now
  suppresses both the volume slide and the dual H00/G00 leg — the wiki's
  "fine slides do not work, and the other effect is also not performed"
  rule. The slide-on-all-ticks forms `D0F` / `DF0` are not fine and keep
  running. New helpers `is_fine_volslide` / `vibrato_memory` cover this.

### Added

- **Qxy retrigger** rebuilt to the multimedia.cx behavioural reference
  (§Qxy): a per-channel tick counter that increments on *every* tick
  (including tick 0), retriggers the sample when it reaches the retrig
  value `y`, and resets to 0. The counter is global to the channel across
  rows — a new note carrying Qxy does **not** restart it; only a row
  without Qxy (or song start) clears it. `Q?0` (retrig value 0) is ignored.
- Exact Qxy volume-modifier table (`retrigger_volume`) including the
  64-entry `Q_TWO_THIRDS` lookup for the `x == 6` ("×2/3") case
  transcribed verbatim from the wiki's `TwoThirds[64]` listing, and the
  documented `x == 8` ("?") no-op.

### Changed

- Qxy no longer keys off `tick % y` (which excluded tick 0 and restarted
  each row); it now uses the persistent per-channel counter above, so
  retrig values that don't evenly divide the song speed keep their cadence
  across row boundaries and a retrig can land on tick 0.

### Added (earlier)

- ST3 **effect memory** ("%" semantics from the multimedia.cx behavioural
  reference): per-channel storage of the latest nonzero parameter for
  each command; a row with the same command and parameter 0 reuses the
  stored value. `H` / `U` share a slot (fine vibrato shares memory with
  vibrato per spec), and the entire `Sxy` family collapses onto a single
  slot. Covers `D`, `E`, `F`, `H`, `I`, `J`, `K`, `L`, `O`, `Q`, `R`,
  `S`, `U`.
- `S3x` / `S4x` waveform select now respects bit 2 ("don't reset
  waveform position when a new note plays"), e.g. `S34`, `S3E`.
- ST3 effect set widened to match the v3.20 spec:
  - `Ixy` (tremor) — on/off cycle per channel, base-volume restoration.
  - `Uxy` (fine vibrato) — same vibrato kernel as `Hxy` with 4× finer depth.
  - `SE x` (pattern delay) — replays the current row `x` extra times without
    re-triggering notes; per-tick effects keep cycling.
  - `S1x` (glissando control) — `Gxx` / `Lxy` slides snap to the nearest
    semitone of the channel's running C5 reference when enabled.
  - `S2x` (finetune) — switches the running C5 speed to one of the 16
    `ScreamTracker-v3.20-effects.txt` C4Spd table values.
  - `S3x` / `S4x` (vibrato / tremolo waveform select) — sine (default), ramp
    down, square, random. Random is a per-channel LCG seeded for
    reproducibility.
  - Fine pitch slides `EFx`, `EEx` (extra-fine), `FFx`, `FEx` — tick-0 only.
  - Fine vol slides `DFy`, `DxF`, and the `DF0` "treat as fine-up by 15" case.
  - `SC0` — immediate note cut at tick 0.
- `Lxy` (tone-porta + vol-slide) now uses the actual Gxx tone-porta kernel
  (per-tick rate from infobyte) instead of the constant-step approximation.
- Glissando snap helper `snap_to_semitone` and waveform sampler
  `waveform_sample` exposed for unit testing.

### Changed

- `Exx` / `Fxx` continuous pitch slides now apply to both `frequency` and
  `target_frequency` so a subsequent vibrato (which rebases off
  `target_frequency`) tracks the slide rather than snapping back.
- `Dxy` volume-slide cases aligned with the multimedia.cx behavioural
  reference: `DFF` is fine slide *up* by 15 (previously slid down 15),
  `D0F` / `DF0` now slide on every tick (including tick 0) per the wiki,
  and a `Dxy` with both nibbles in 1..=E is treated as `D0y` (slide down
  by y) per the documented ST3 quirk.
- `SC0` (note-cut tick 0) is now a no-op — per multimedia.cx §SCx, an
  `SC0` is *ignored* by ST3 rather than cutting immediately. The earlier
  immediate-silence implementation was rolled back.
- `Cxx` (pattern break) with a decoded row >= 64 is now ignored per the
  multimedia.cx behavioural reference (was previously clamped to row 63
  which produced an unintended jump).

## [0.0.6](https://github.com/OxideAV/oxideav-s3m/compare/v0.0.5...v0.0.6) - 2026-05-06

### Other

- reframe FFI claim — HW-engine crates use OS FFI by necessity
- drop stale REGISTRARS / with_all_features intra-doc links
- drop dead `linkme` dep
- registry calls: rename make_decoder/make_encoder → first_decoder/first_encoder
- auto-register via oxideav_core::register! macro (linkme distributed slice)
- unify entry point on register(&mut RuntimeContext) ([#502](https://github.com/OxideAV/oxideav-s3m/pull/502))

## [0.0.5](https://github.com/OxideAV/oxideav-s3m/compare/v0.0.4...v0.0.5) - 2026-05-03

### Other

- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- adopt slim VideoFrame/AudioFrame shape
- pin release-plz to patch-only bumps

## [0.0.4](https://github.com/OxideAV/oxideav-s3m/compare/v0.0.3...v0.0.4) - 2026-04-25

### Other

- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- bump oxideav-container dep to "0.1"
- drop Cargo.lock — this crate is a library
- bump to oxideav-core 0.1.1 + codec 0.1.1
- migrate register() to CodecInfo builder
- bump oxideav-core + oxideav-codec deps to "0.1"
- thread &dyn CodecResolver through open()
