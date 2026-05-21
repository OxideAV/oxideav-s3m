# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
