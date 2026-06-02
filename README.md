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
- **Channel mute flag (`+128` in the header's channel-settings byte)**:
  per the ST3 format reference, a channel byte of `0x80 | type` marks
  the slot as disabled while keeping its pattern data live. The decoder
  now reads pattern cells for muted channels (so jumps, loops, and
  pattern delays stay consistent with what a real ST3 would compute)
  but the mixer silences their output. AdLib slots (type 16..=31)
  without the `+128` flag are also reported as muted in the PCM path,
  since OPL synthesis is out of scope.
- Effects: `Axx` (speed), `Bxx` (pos jump), `Cxx` (pattern break — rows
  64+ ignored per ST3), `Dxy` volume slide — full multimedia.cx case
  matrix including `D0F`/`DF0` (slide-on-all-ticks), `DFF` (fine up by
  15), `DFy`/`DxF` fine variants, and the ST3 `Dxy` both-nibbles-nonzero
  quirk (treats as `D0y`/slide-down). `Exx` / `Fxx` pitch slides (with
  fine `EFx`/`FFx` and extra-fine `EEx`/`FEx`), `Gxx` (tone portamento),
  `Hxy` (vibrato), `Ixy` (tremor), `Jxy` (arpeggio), `Kxy` (vib+vol —
  literally `H00 + Dxy`: the vibrato leg *continues* the channel's running
  H/U vibrato from effect memory, not Kxy's own nibbles), `Lxy`
  (porta+vol — `G00 + Dxy`: the porta leg continues the running G tone
  portamento at its remembered rate; a *fine* volume-slide form in either
  `Kxy`/`Lxy` infobyte suppresses both the slide and the dual H00/G00 leg
  per multimedia.cx §Kxy), `Oxx` (sample offset), `Qxy` (retrigger — persistent
  per-channel tick counter that survives across rows and can fire on
  tick 0, with the full `x` volume-modifier table including the exact
  64-entry `TwoThirds` lookup for the `x=6` ×2/3 case), `Rxy`
  (tremolo), `Txx` (tempo), `Uxy` (fine vibrato — shares memory with
  `Hxy`), `Vxx` (global volume), `Xxx` (set pan), plus the `Sxy`
  family (`S1x` glissando, `S2x` finetune from the spec C4Spd table,
  `S3x`/`S4x` vibrato/tremolo waveform select [sine, ramp-down, square,
  random] with bit-2 "keep position across new note" support, `S80`
  pan, `SAx` legacy stereo control [pre-ST3 16-position pan with the
  XOR-0x8 nibble swap from FireLight §6.23, kept around for
  PANIC.S3M / STRSHINE.S3M compatibility], `SBx` pattern loop, `SCx`
  note cut [`SC0` ignored per spec], `SDx` note delay, `SEx` pattern
  delay). `S0x` filter and `SFx` funkrepeat are spec'd as not
  implemented in ST3 itself and decode
  as no-ops.
- **`SCx` freeze/resume semantics**: per the multimedia.cx behavioural
  reference, an `SCx` cut does **not** zero the channel volume — it
  *freezes* playback so the mixer emits silence and the sample read
  cursor stops advancing, while the channel keeps its volume /
  frequency / sample position intact. A subsequent `Exx`, `Fxx`, `Gxx`,
  `Hxx`, `Jxx`, `Kxx`, `Lxx`, or `Uxx` command on a later row (or any
  fresh note trigger, including the `SDx`-deferred form) thaws the
  channel and playback resumes from where the cut landed.
- **Effect memory** (the ST3 "%" semantics): channels remember the
  latest nonzero parameter for each command and substitute it back in
  when a row carries the same command with parameter 0. `H` / `U` and
  the entire `Sxy` family share their slots per the multimedia.cx
  behavioural reference.
- **Per-channel default pan resolution**: the pan byte's bit 5 selects
  between an explicit low-nibble value and the spec defaults. When a
  channel's bit 5 is clear (or no `d.p == 0xFC` pan block is present)
  the parser falls back to the spec defaults keyed by the master-volume
  stereo flag — stereo mode resolves left PCM slots (channel type
  `0..=7`) to pan `3` and right PCM slots (`8..=15`) to pan `C`, while
  mono mode (bit 7 of master volume clear) sets every pan to the
  centre `7`. The FireLight tutorial §2.8.1 mono override is also
  honoured: in mono mode every channel is forced to centre regardless
  of any explicit pan byte read from the pan block.
- **Header-flag-driven playback modes**
  - **Fast slides (header flag bit 6, or CwtV == `0x1300`)** — the
    original ST3.00 release shipped with a "fast slides" mode where the
    `Dx0` / `D0y` / `Dxy` (1..=E) volume slides also fire at tick 0,
    on top of the per-tick path's nonzero-tick steps. Per the
    multimedia.cx behavioural reference (§Dxy: "Also slide on tick 0,
    if fast slides are enabled"), later versions only do so when bit 6
    of the file's flag word is explicitly set; a CwtV of `0x1300` auto-
    arms the mode regardless of the flag. Fine forms (`DFx` / `DxF` /
    `DFF`) are unaffected, as are the always-slide-on-all-ticks `D0F`
    / `DF0` units.
  - **Amiga limits (header flag bit 4)** — modules that opt in get their
    playback frequency clamped to the PAL Amiga's hardware period range
    `[113, 856]` clock units, i.e. `[AMIGA_CLOCK_HZ / 856,
    AMIGA_CLOCK_HZ / 113]` ≈ `[16 725, 126 703]` Hz. The clamp covers
    note triggers (immediate and `SDx`-deferred), `S2x` finetune, the
    fine + continuous pitch slides (`E*` / `F*`), tone portamento
    (`Gxx`, `Lxy`'s G00 leg), vibrato (`Hxy`, `Uxy`, `Kxy`'s H00 leg)
    and `Jxy` arpeggio, so neither the audible pitch nor the
    target-tracking pitch ever escape the range.
- **Initial speed / tempo spec edge cases** — per the multimedia.cx
  behavioural reference, the file's "initial speed" byte is ignored
  when its value is `0` *or* `255` (Axx with parameter `0xFF` still
  works, so the magic number is unusual but legal), and the "initial
  tempo" byte is ignored when below `33` (matching the same
  `Txx >= 0x20` guard the per-row tempo command honours). Both fall
  back to the spec defaults (`speed = 6`, `bpm = 125`) when ignored.
- **`Vxx` (set global volume) — spec-compliant timing + value range**.
  Per the multimedia.cx behavioural reference §Vxx, the command has
  three quirks the decoder now honours:
  1. **Parameter range** — values higher than `0x40` are *ignored*
     (not clamped). A stray `V41`..`VFF` on a row leaves the previous
     global volume untouched.
  2. **Tick-1 application** — the effect is "actually processed on
     tick 1 (that is the second tick) of the row", so same-row notes
     triggered at tick 0 still see the *old* global volume. The new
     value lands one tick later and is then read live by every
     subsequent mixer step, so per-tick `Dxx` slides and any
     `SDx`-deferred note triggers with `x >= 1` automatically pick up
     the new value.
  3. **Speed-1 skip** — when the current row speed is `1`, tick 1
     never fires before the row advances and the stash is dropped on
     the next row entry. This matches the spec's "doesn't do anything
     if the current speed is 1" rule.
- **Mixing volume range + stereo `* 11/8` multiplier**. Per the
  multimedia.cx wiki §Mixing volume ("range 16 <= x <= 127 ... It is
  multiplied by 11/8 when stereo is on"), the file-header master-
  volume byte is now clamped to `[16, 127]` at load — values below
  the documented floor used to silence the mix below the legal
  minimum — and the mixer applies a `* 11/8` (1.375×) gain whenever
  the header's stereo flag (bit 7 of the raw MV byte) is set. The
  flag is plumbed into `PlayerState::stereo`; both the mixed and
  per-channel renderers apply the multiplier so a one-channel module
  produces bit-equivalent output across both APIs.

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
