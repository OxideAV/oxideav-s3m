# oxideav-s3m

Scream Tracker 3 Module (S3M) container + codec for oxideav.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace) framework — a
100% pure Rust media transcoding and streaming stack. No C libraries, no FFI
wrappers, no `*-sys` crates.

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
- Effects: `Axx` (speed), `Bxx` (pos jump), `Cxx` (pattern break), `Dxy`
  (volume slide), `Exx` / `Fxx` (pitch slides), `Gxx` (tone portamento),
  `Hxy` (vibrato), `Jxy` (arpeggio), `Kxy` (vib+vol), `Lxy` (porta+vol),
  `Oxx` (sample offset), `Qxy` (retrigger), `Rxy` (tremolo), `Txx`
  (tempo), `Vxx` (global volume), `Xxx` (set pan), plus the `Sxy` family
  (`S80` pan, `SBx` pattern loop, `SCx` note cut, `SDx` note delay).
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
