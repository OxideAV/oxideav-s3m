# oxideav-s3m

Scream Tracker 3 Module (S3M) container + codec for oxideav.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace) framework — a pure-Rust media transcoding and streaming stack. Codec, container, and filter crates are implemented from the spec (no C codec libraries linked or wrapped, no `*-sys` crates). Optional hardware-engine crates (`oxideav-videotoolbox` / `-audiotoolbox` / `-vaapi` / `-vdpau` / `-nvidia` / `-vulkan-video`) bridge to OS APIs via runtime `libloading`; pass `--no-hwaccel` (or omit the `hwaccel` feature) to opt out.

## Features

- **Container** (`s3m`): probes the 4-byte `SCRM` magic at offset 44, parses
  header + instruments + patterns + pan table, and delivers the whole song
  as a single packet to the decoder.
- **Mixed-stereo codec** (`s3m`): renders 44.1 kHz interleaved signed-16-bit
  PCM with every S3M channel summed into one L/R pair. Linear-interpolation
  mixer, sqrt(active-channels) normalisation, master/global-volume gain.
- **Per-channel codec** (`s3m_multichannel`): same sample rate and format,
  but every S3M channel slot gets its own stereo pair in the output —
  interleaved as `[ch0_L, ch0_R, ch1_L, ch1_R, …, ch31_L, ch31_R]`
  (`channels = 64` on the emitted `AudioFrame`). Useful for DAWs,
  visualizers, and per-instrument remastering tools.

## Decoder coverage

- PCM instruments (8-bit signed/unsigned, 16-bit, mono and true-stereo).
- AdLib instrument types are skipped (no OPL synth).
- Effects: `Axx` (speed), `Bxx` (pos jump), `Cxx` (pattern break — rows
  64+ ignored per ST3), `Dxy` volume slide — full multimedia.cx case
  matrix including `D0F`/`DF0` (slide-on-all-ticks), `DFF` (fine up by
  15), `DFy`/`DxF` fine variants, and the ST3 `Dxy` both-nibbles-nonzero
  quirk (treats as `D0y`/slide-down). `Exx` / `Fxx` pitch slides (with
  fine `EFx`/`FFx` and extra-fine `EEx`/`FEx`), `Gxx` (tone portamento),
  `Hxy` (vibrato), `Ixy` (tremor), `Jxy` (arpeggio), `Kxy` (vib+vol),
  `Lxy` (porta+vol), `Oxx` (sample offset), `Qxy` (retrigger — persistent
  per-channel tick counter that survives across rows and can fire on
  tick 0, with the full `x` volume-modifier table including the exact
  64-entry `TwoThirds` lookup for the `x=6` ×2/3 case), `Rxy`
  (tremolo), `Txx` (tempo), `Uxy` (fine vibrato — shares memory with
  `Hxy`), `Vxx` (global volume), `Xxx` (set pan), plus the `Sxy`
  family (`S1x` glissando, `S2x` finetune from the spec C4Spd table,
  `S3x`/`S4x` vibrato/tremolo waveform select [sine, ramp-down, square,
  random] with bit-2 "keep position across new note" support, `S80`
  pan, `SBx` pattern loop, `SCx` note cut [`SC0` ignored per spec],
  `SDx` note delay, `SEx` pattern delay). `S0x` filter and `SFx`
  funkrepeat are spec'd as not implemented in ST3 itself and decode
  as no-ops.
- **Effect memory** (the ST3 "%" semantics): channels remember the
  latest nonzero parameter for each command and substitute it back in
  when a row carries the same command with parameter 0. `H` / `U` and
  the entire `Sxy` family share their slots per the multimedia.cx
  behavioural reference.
- Per-channel pan (default-pan block or synthesised from channel settings).

**Decode-only** — no S3M encoder is provided, by design. S3M is a tracker
*source* format; re-emitting one is out of scope.

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

For lower-level access, [`player::PlayerState`] exposes both `render` (mixed)
and `render_per_channel` (one stereo pair per S3M channel) directly, bypassing
the codec-registry wrapper.

## License

MIT — see [LICENSE](LICENSE).
