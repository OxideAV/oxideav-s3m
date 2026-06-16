# oxideav-s3m

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
  reference. AdLib instrument types are skipped (no OPL synth).
- **Channel mute flag** (`+128` in the channel-settings byte) — the
  decoder reads pattern cells for muted channels (so jumps, loops, and
  pattern delays stay consistent) but the mixer silences their output.
  AdLib slots without the flag are also reported as muted.
- **Effects** — the full command set: `Axx` (speed), `Bxx` (pos jump),
  `Cxx` (pattern break), `Dxy` volume slide (full case matrix including
  the fine forms and the both-nibbles-nonzero quirk), `Exx` / `Fxx`
  pitch slides (with fine + extra-fine forms), `Gxx` (tone portamento,
  including the empty-note-targets-last-note rule), `Hxy` (vibrato),
  `Ixy` (tremor), `Jxy` (arpeggio, resolved through the period table),
  `Kxy` (vibrato + volume), `Lxy` (portamento + volume), `Oxx` (sample
  offset, honouring the loop window on looped samples), `Qxy`
  (retrigger, with the full volume-modifier table), `Rxy` (tremolo),
  `Txx` (tempo), `Uxy` (fine vibrato), `Vxx` (global volume, with the
  range / tick-1 / speed-1 quirks), `Xxx` (set pan), and the `Sxy`
  family (`S1x` glissando, `S2x` finetune, `S3x`/`S4x` waveform select,
  `S80` pan, `SAx` legacy "old stereo control" — bank-keyed
  normal/reversed/center mapping (`SA0`/`SA2` keep the channel's L/R bank,
  `SA1`/`SA3` swap it, `SA4`–`SA7` centre, `SA8`–`SAF` no-op), `SBx`
  pattern loop, `SCx` note cut / freeze, `SDx` note delay, `SEx` pattern
  delay). `S0x` filter and
  `SFx` funkrepeat decode as no-ops (not implemented in ST3 itself).
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
