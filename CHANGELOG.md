# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Vibrato / tremolo waveforms now use the exact ProTracker `sintab`**
  (`docs/audio/trackers/s3m/FireLight-S3M-Player-Tutorial.txt` §6.8). The
  sine waveform previously computed `sin(2π·pos/64)·64` per tick; it now
  reads the 32-entry ProTracker half-sine table the tutorial transcribes
  ("This is the sine table used by Protracker. If a player calls itself
  fully protracker compatible, it really should be using this table.")
  through ST3's documented signed-pointer convention: positions `0..=31`
  are the additive half-cycle, `32..=63` the subtractive half, and the low
  five bits index the table. The ramp-down (`case 1: temp = idx<<3;
  if(vibpos<0) temp = 255-temp`) and square (`case 2: delta = 255`) shapes
  follow the same §6.8 example routine the tutorial calls "100% accurate".
  Native 0..=255 magnitudes scale by `/4` into the existing ±64 working
  range so the depth math (`Hxy`/`Uxy`/`Rxy`/`Kxy`) is unchanged in
  structure but the modulation now lands on ST3's integer waveform values
  (peak ±63, not ±64) rather than floating-point approximations. The table
  is exposed as the public `PROTRACKER_SINE` constant. New unit tests lock
  the table values, its symmetry about the index-16 peak, the signed-
  pointer sine lookup, and the ramp-down half-cycle mirror; the three
  square-wave tremolo tests are updated from the old ±64 peak to the
  documented ±63.
- **Canonical Scream Tracker 3 period table + period-based pitch**
  (`docs/audio/trackers/s3m/FireLight-S3M-Player-Tutorial.txt` §4 / §5.1).
  `note_to_frequency` previously approximated pitch with a pure
  equal-tempered `2^(delta/12)` multiply. It now resolves through ST3's
  own 9-octave (108-entry) integer period table from the tutorial's §4.2
  "9 Octaves" listing, exposed as the public `PERIOD_TABLE` constant plus
  a `C5_NOTE_INDEX` reference and a `note_index_to_frequency(n, c2spd)`
  helper. The runtime formula matches §4.1:
  `period = 8363 * PERIOD_TABLE[n] / c2spd` (integer truncation), then
  `freq = AMIGA_CLOCK_HZ / period`. The base clock is the
  multimedia.cx-corrected `8363 * 1712 == 14_317_456` (FireLight's
  `14317056` is the documented base-clock typo). The C-5 reference note
  (`PERIOD_TABLE[48] == 1712`) still plays at exactly `c2spd` and octave
  shifts still double, so the existing pitch tests hold; the integer
  period truncation now matches real ST3, which makes finetuned /
  off-`c2spd` notes land on the spec-accurate quantised frequency rather
  than the idealised ratio (e.g. note `0x50` at `c2spd == 7895` → period
  `906` → ~15803 Hz, not `15790`). The
  `effect_s2x_finetune_changes_playback_rate` integration test is updated
  to assert the period-table value with the correct ±5 Hz window.
- **`Jxy` arpeggio now uses the period table** (FireLight §5.1 step 9 +
  §6.10). Each per-tick leg (`tick % 3` → `0`, `+x`, `+y`) adds the
  semitone offset to the note *index* and looks the result up in
  `PERIOD_TABLE` with the channel's `c2spd`, instead of multiplying the
  base frequency by `2^(semis/12)`. Legs above the octave-8 B ceiling
  clamp to B-8. New unit tests cover the period-table corner values,
  monotonic decrease, integer-period truncation, and the B-8 clamp; a new
  `effect_jxy_arpeggio_uses_period_table_note_index` integration test
  walks the base / `+4` / `+7` legs of a `J47` chord on a C-5 note.
- **`CreatedWithTracker::auto_fast_slides()` — typed §Dxy fast-slides bound**
  (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`). The
  reference states the ST3.00 "fast slides" auto-arming rule **twice with
  different bounds**: §Flags bit 6 says "automatically enabled if tracker
  version is `== 0x1300`", while §Dxy says "if fast slides are enabled (if
  they are set as a flag or the version is `<= 0x1300`)". The existing
  `is_st3_00()` predicate captured only the strict `== 0x1300` sentinel, so
  the player missed earlier Scream Tracker family builds whose `Cwt/v` falls
  below `0x1300` (e.g. a `0x12xx` beta) that the §Dxy volume-slide kernel
  would still run on the per-tick path. The new `auto_fast_slides()`
  accessor on `CreatedWithTracker` encodes the broader §Dxy form —
  `matches!(tracker, Tracker::ScreamTracker) && raw <= 0x1300` — gated on
  the Scream Tracker family (top nibble `0x1`) so a numerically-smaller
  word from an undocumented `0x0xyy` writer is not misclassified. The
  player's `fast_slides` derivation in `PlayerState::new` now consults
  `auto_fast_slides()` (the bound the kernel actually keys off) instead of
  the strict `is_st3_00()` sentinel; `is_st3_00()` stays available for
  callers needing the §Flags-exact form. The existing
  `header_tracker_version_0x1300_enables_fast_slides_automatically` test is
  unchanged (0x1300 arms, 0x1301 does not). Four new tests:
  `auto_fast_slides_covers_dxy_le_0x1300_bound` and
  `auto_fast_slides_gated_on_scream_tracker_family` (header.rs) drive the
  predicate's boundary + family gate directly;
  `header_pre_st3_00_version_below_0x1300_auto_arms_fast_slides` (player.rs)
  confirms a `0x12FF` word arms `fast_slides` while a non-family `0x0ABC`
  word does not.

- **`Tracker` enum + `CreatedWithTracker` typed decomposition of the `Cwt/v`
  field** on `S3mHeader` (`docs/audio/trackers/s3m/ScreamTracker-v3.20-s3m.txt`
  `Cwt/v   = Created with tracker / version: &0xfff=version, >>12=tracker` +
  `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html` §"Tracker
  version"). The 16-bit "Created with tracker / version" header word splits
  into a 4-bit tracker ID (top nibble) plus a 12-bit version number. The new
  `S3mHeader::created_with_tracker()` accessor returns a `CreatedWithTracker
  { raw, tracker, version }` triple, where `tracker` is a `Tracker` enum with
  arms for every documented writer (`ScreamTracker` = 0x1, `ImagoOrpheus` =
  0x2, `ImpulseTracker` = 0x3, `SchismTracker` = 0x4, `OpenMpt` = 0x5) plus
  an `Other(u8)` arm that preserves any undocumented top nibble so forensic
  callers can still inspect a raw value rather than misclassify as a known
  writer. The accessor also exposes `is_st3_00()`, which returns true iff
  the raw word is exactly `0x1300` — the sentinel multimedia.cx §Flags bit 6
  calls out as auto-arming "ST3.00 volume slides ... regardless of the flag
  byte". The player's fast-slides arming now consults
  `header.created_with_tracker().is_st3_00()` instead of inlining the
  `header.tracker_version == 0x1300` literal, so the behavioural rule has
  one place to evolve from. Four new unit tests under `src/header.rs`
  cover the documented-prefix mapping, the `Other(nibble)` preservation,
  the raw / tracker / version split for ST3.00 / ST3.20 / OpenMPT-shaped
  words, and a round-trip from a parsed `S3mHeader` whose on-disk Cwt/v
  is patched to ST3.01 — all without changing the existing `tracker_version:
  u16` field, so no downstream caller breaks.

### Changed

- **`Gxx` / `Lxy` tone portamento now honours the "empty note targets the
  last note" peculiarity** per the multimedia.cx behavioural reference §Gxx
  (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`): "If the
  current note is empty, the destination note is set to the last note to
  show up in the channel, **even if it has occurred without the Gxx
  effect**." Previously the porta target (`target_frequency`) was only ever
  set when a row carried an explicit note, so a bare `Gxx` row (no note)
  was a per-tick no-op against a stale target. Now a `Gxx` / `Lxy` row with
  no note re-arms the target to the channel's `last_note` frequency
  (resolved through the current instrument's C5SPD and the canonical period
  table), so the slide glides back toward whatever note last played in the
  slot. Two supporting corrections make the rule hold: (a) a
  porta-suppressed trigger (a note that appears *with* `Gxx`/`Lxy`, so the
  retrigger is skipped) now also updates `last_note`, since that note still
  "shows up in the channel"; and (b) the companion §Gxx rule "Gxx doesn't
  clear the target note when it is reached, so any future Gxx with no note
  will keep sliding back to this particular note" falls out for free — the
  target is never zeroed on arrival, and a second bare `Gxx` recomputes the
  same target from the unchanged `last_note`. A bare `Gxx` on a channel that
  has never played a note (`last_note == 0`) stays a no-op. Four new unit
  tests under `src/player.rs` drive a synthetic single-channel player
  through `enter_row`: `bare_gxx_targets_last_note_even_without_prior_porta`,
  `bare_gxx_does_not_clear_target_on_arrival`,
  `porta_suppressed_trigger_updates_last_note`, and
  `bare_gxx_with_no_prior_note_is_noop`.

- **`Rxy` tremolo rebuilt to the multimedia.cx behavioural reference §Rxy
  (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`).** Four
  spec gaps in the previous per-tick handler now resolved:
  1. **Active volume recomputed from the stored volume, never
     accumulated.** The wiki says "set the active volume to the stored
     volume plus (depth * value) / (max_amplitude * 2) ... The stored
     volume is untouched." The previous code added the delta onto the
     *previous active* volume (`ch.volume += delta`), so consecutive
     same-sign waveform samples compounded and the modulation drifted
     away from the note's volume instead of oscillating around it. The
     new `apply_tremolo` kernel computes `stored_volume + delta` fresh
     on every nonzero tick.
  2. **Documented depth scaling.** §Playback Notes gives "Hxy and Rxy
     use x*4 and y*4 as their parameters"; with the crate's ±64 waveform
     amplitude the §Rxy formula reduces to `delta = (4·y · value) / 128`,
     peaking at ±30 for `y = 0xF` (the wiki's "Rxy peaks at 32 in each
     direction" is the formula's theoretical bound). The previous
     `(value · y) / 64` produced half the documented swing (±15 max).
  3. **256-unit cycle length.** §Playback Notes: "Vibrato and tremolo
     have a full cycle length of 256". The previous code masked the
     phase to a 64-entry table while still stepping by `speed * 4`,
     making the LFO cycle four times faster than documented
     (period 16/x ticks instead of 256/(4·x) = 64/x). `tremolo_pos` now
     holds the full 0..=255 phase (natural `u8` wrap) and the 64-entry
     waveform table is sampled at `phase / 4`.
  4. **Zero stored volume disables the effect.** §Rxy: "Tremolo will not
     work if the stored volume is 0." New early-out; neither the active
     volume nor the phase moves.
  The §Rxy "song speed 1 leaves the active volume untouched — it is not
  set to the stored volume!" rule is locked structurally (the kernel only
  runs from the per-tick path, which a speed-1 row never reaches) and by
  the `tremolo_applies_from_tick_1_not_tick_0` test. Five new unit tests
  under `src/player.rs` drive the kernel, the clamp ends (63 ceiling /
  0 floor), the phase cadence, and the tick-0/tick-1 dispatch split.
  Follow-up: the `Hxy`/`Uxy` vibrato kernel still uses the legacy
  64-masked phase convention and should get the same 256-cycle treatment.

- **`S00` repeating an `SDx` now double-triggers** per the multimedia.cx
  behavioural reference (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`)
  §S0x: "When `S00` is repeating a note delay (`SDx`), the note is triggered
  twice: once on tick 0 (as if there's no note delay) and again on tick x
  (as with a normal note delay)." The previous code applied effect-memory
  recall first and then treated the resolved `SDx` byte identically to a
  freshly-written `SDx`, so the immediate tick-0 leg was lost. The
  row-application path now captures the row's *raw* infobyte and detects
  the "S-command recalled into SDx" case before the note-delay branch.
  When the case fires, the trigger is performed at tick 0 *and* the
  deferred copy is also armed in `pending_delay` so the SDx tick-x path
  re-triggers the note. A freshly-written `SDx` (nonzero raw infobyte)
  keeps the single-trigger contract and its dedicated unit test confirms
  no regression.

- **`Ixy` tremor rebuilt to the multimedia.cx behavioural reference §Ixy
  (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`).** Three
  spec gaps in the previous implementation now resolved:
  1. **Persistent decrementing counters across rows.** The wiki specifies
     the effect "Implemented with two decrementing counters per channel —
     the 'on' counter and the 'off' counter" and that they are "**never
     reset**, except in the tremor update procedure described above.
     Scream Tracker doesn't even reset them on playback start." The
     previous code reset `tremor_phase = 0` every `enter_row` so a
     channel in the middle of an off-phase at end-of-row would restart
     the audible on-phase at the top of the next row even when the spec
     would keep it silent for several more ticks. The new state lives in
     two `u8` fields (`tremor_on_counter` / `tremor_off_counter`) on
     `Channel`; row entry no longer touches them.
  2. **Tick-0 firing.** The wiki says "This effect is updated on **every
     tick**" — including tick 0. The previous code only fired Ixy in
     `apply_per_tick` (ticks 1..speed-1), so a cold-counter cycle stayed
     in the wrong state for the first tick of every row. The new
     dispatch site mirrors the existing Qxy tick-0 wiring: a dedicated
     arm in the `enter_row` tick-0 match calls the shared
     `apply_tremor_step` helper.
  3. **Restore reads "stored" volume, not active.** The wiki §Ixy says
     "**the stored volume isn't modified by this effect**" and that the
     off→on transition sets the current volume "to the stored volume" —
     not the value the channel happens to hold mid-Dxy-slide. The
     previous code captured `ch.volume` on the first Ixy tick of a row
     and treated that as the restore target, which fired the wrong
     value when an upstream Dxy slide had already pulled the active
     volume to 0. The new code adds a `stored_volume: u8` field
     populated by the four documented stored-volume sources (instrument
     default load, explicit volume column, and the SDx-deferred forms of
     both); Ixy's restore reads this field and clamps to `PCM_VOLUME_PEAK`.
  Public-API impact: the `tremor_phase` and `tremor_base_volume` fields
  on `Channel` are gone (replaced by `tremor_on_counter`,
  `tremor_off_counter`, `stored_volume`). The crate is at 0.0.x so this
  is a non-breaking-by-policy refactor; no other crate in the workspace
  reads these fields. The existing integration test
  `effect_ixy_tremor_alternates_volume` keeps its assertions (the
  ticks-1/2 audible, ticks-3/4/5 silent pattern continues to hold for
  the standard I22 case). Eleven new unit tests under
  `src/player.rs` cover: cold-counter on/off-transition,
  on-phase decrement, off-phase restore, restore-reads-stored (not
  active), restore caps to PCM peak, cross-row persistence,
  vol-stays-zero-without-Ixy edge case, x=y=0 callsite-guard
  contract, stored-volume tracking on instrument load and on
  explicit volume column, and the new tick-0 firing path.

- **`Oxy` sample-offset honours the loop window on looped samples**
  (`docs/audio/trackers/s3m/ScreamTracker-v3.20-effects.txt` §Oxy:
  "If the sample offset is used in a looped sample and the offset
  given exceeds the loop end value, the loop is taken into
  consideration and the offset will be calculated as if the sample
  had looped"). Previously the player set `sample_pos = info * 256`
  unconditionally — for a looped sample whose `[loop_start,
  loop_end)` window started below the requested offset, this landed
  the read cursor *past* the addressable sample data, leaving the
  mixer to immediately mark the channel inactive instead of playing
  the intended loop region. The fix introduces a single helper,
  `resolve_sample_offset(off, loop_start, loop_end, pcm_len,
  looped)`, that folds an over-`loop_end` offset back through the
  loop span via `loop_start + (off − loop_start) mod (loop_end −
  loop_start)`; an unlooped sample returns the raw offset
  (the mixer's bounds check still deactivates a cursor past
  `pcm_len`), and a malformed loop window (zero / negative span or
  `loop_end > pcm_len`) also returns the raw offset rather than
  panic on a degenerate modulo. Used by the Oxy branch in
  `enter_row` so a fresh `O40` (`0x40 * 256 = 16384` samples) on a
  sample with loop `[1024, 8192)` lands at `2048` instead of
  silently dropping the channel. Five new tests
  (`oxy_offset_unlooped_is_raw_value`,
  `oxy_offset_inside_loop_window_is_unchanged`,
  `oxy_offset_exceeding_loop_end_wraps_into_loop_window`,
  `oxy_offset_malformed_loop_falls_back_to_raw`, and an end-to-end
  `oxy_trigger_inside_looped_sample_lands_in_loop_window`) cover
  the fold formula, the no-fold branches, the defensive fall-back,
  and the through-player trigger.
- **PCM active-volume peaks at 63 (not 64) per multimedia.cx
  §Playback Notes** (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`):
  "Volumes actually peak at 63, and not 64. Setting the volume to 64 will
  actually make it go to 63. However, on Adlib channels, if the default
  volume is 64, it will use 64. Any further operations on the volume
  will clip it to within the 0-63 range." The crate decodes only the
  PCM path (Adlib FM synth is out of scope), so every active-volume
  write — instrument default load (a sample stored at 64 lands at 63),
  explicit volume column (a row's `V40` lands at 63), per-tick `Dxy`
  add legs (`DF0` / `Dx0` / fast-slides tick-0 leg / fine `DxF` and
  `DFF`), `Qxy` retrigger volume modifier (`Q?F` doubles cap at 63),
  `Rxy` tremolo delta (`clamp(0, 63)`), and `Ixy` tremor on-phase
  restore — now funnels through a single
  `clamp_pcm_volume(u16) -> u8` helper that caps the result to the
  new public `PCM_VOLUME_PEAK = 63` constant. The mixer additionally
  reads the field through `volume.min(PCM_VOLUME_PEAK)` so an
  externally-constructed `Channel` literal (test scaffolding, FFI
  handoff) can't slip a 64 past the spec ceiling either. The naive
  `vol / 64` gain stays unchanged — the maximum 63/64 ≈ 0.984 matches
  the audible ceiling of an unmodified ST3 PCM channel (~0.14 dB drop
  from the previous 64/64 = 1.0). Decoder-side header parsing keeps
  the file-byte representation intact (instrument default volume
  field, header global-volume field) so round-trip tooling still
  inspects the raw values; only the active mixer chain is capped.
  Two new integration tests (`pcm_volume_peak_is_63_not_64` driving
  the three independent entry points, and
  `mixer_caps_externally_supplied_volume_64_to_pcm_peak` showing
  bit-identical output for an external 64 vs the clamped 63) lock the
  new behaviour in; existing `effect_dff_is_fine_slide_up_by_fifteen`,
  `effect_dfy_fine_vol_slide_down`,
  `effect_dxy_both_nibbles_nonzero_slides_down_by_y`, and
  `retrigger_volume_modifiers_match_spec_table` were updated to expect
  the spec-compliant 63 ceiling.
- **Mixing-volume range + stereo `* 11/8` multiplier per the spec's
  §Mixing volume.** Per
  `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
  ("Mixing volume (range 16 <= x <= 127) ... It is multiplied by 11/8
  when stereo is on"), the player now (a) clamps the file-header
  master-volume byte to `[16, 127]` at load — sub-floor values used to
  be admitted as-is and could silence the mix below the documented
  minimum — and (b) applies a `* 11/8` (1.375×) gain to the mixing
  volume when the header's stereo bit (high bit of the raw MV byte)
  is set. The stereo flag is plumbed into a new
  `PlayerState::stereo: bool`, and both the mixed renderer
  (`render_one`) and the per-channel renderer
  (`render_one_per_channel`) apply the multiplier so per-channel
  output stays bit-equivalent to the mixed path on a
  single-active-channel module. Five new unit tests cover the value
  clamp (`master_volume_clamped_to_spec_range_16_127`), the flag
  plumbing (`stereo_flag_mirrored_into_player_state`), the stereo
  gain ratio (`stereo_mixing_volume_gets_11_over_8_boost` — measures
  `stereo_amp / mono_amp ≈ 1.375` within 0.02 of the spec value), and
  the inverse mono-path verification
  (`mono_mixing_volume_has_no_stereo_boost`).
- **`Vxx` (set global volume) now honours the multimedia.cx behavioural
  reference §Vxx in three places where the previous implementation
  was off-spec**
  (`docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`):
  1. **Parameter range** — values higher than `0x40` are now
     *ignored* per "Vxx with parameter values higher than 0x40 are
     ignored". The earlier path silently `min`-clamped to `64`,
     which masked the rule and produced a spurious update for any
     `V41`..`VFF` infobyte; the new path leaves the prior global
     volume untouched.
  2. **Tick-1 application** — Vxx is now deferred to tick 1 per
     "This effect is actually processed on tick 1 (that is the
     second tick) of the row". Tick 0 stashes the validated value
     in a new `PlayerState::pending_global_vol`; the per-tick path
     drains it when `tick == 1`. This makes same-row notes
     triggered at tick 0 observe the *old* global volume — matching
     the wiki's "does not affect events on the same row" rule —
     while letting later ticks (including SDx-delayed triggers and
     the per-tick Dxx volume slide) read the new value live.
  3. **Speed-1 skip** — when the current row speed is `1`, tick 1
     never fires before the row advances, so the stash is dropped
     on the next `enter_row`. This satisfies "doesn't do anything,
     if the current speed is 1" without a separate guard. Six new
     unit tests (`vxx_param_above_0x40_is_ignored`,
     `vxx_param_0xff_is_ignored`, `vxx_param_0x40_is_the_upper_boundary`,
     `vxx_applies_on_tick_1_not_tick_0`,
     `vxx_does_nothing_when_speed_is_1`,
     `vxx_stash_cleared_on_next_row_entry`) drive `enter_row` /
     `apply_per_tick` directly through a single-channel synthetic
     player to lock in every branch of the new behaviour.

### Added

- **Header flag bit 6 — fast slides / "ST3.00 volume slides"** wired
  into the Dxy tick-0 path. Per the multimedia.cx behavioural reference
  at `docs/audio/trackers/s3m/multimedia-cx-scream-tracker-3.html`
  §Flags: "if enabled, *all* volume slides occur *every* tick" and
  "automatically enabled if tracker version is == 0x1300." Modules
  that set the flag (or that carry CwtV `0x1300`, the original ST3.00
  release) now get an extra slide step at tick 0 for the continuous
  `Dx0` / `D0y` / `Dxy` (1..=E) forms — matching what a real ST3 would
  emit. Fine slides (`DFx` / `DxF` / `DFF`) are explicitly excluded
  per the same source ("unless we're doing a fineslide, we slide on
  all ticks"); `D0F` / `DF0` are unaffected because they were already
  slide-on-all-ticks regardless of the flag. New `apply_dxy_tick0_fast_slide`
  helper covers the leg in isolation and is exercised by four targeted
  unit tests (the `Dx0` and `D0y` cases, the negative skip-list for
  fine / D0F / DF0 / D00, and the ST3 `Dxy` quirk).
- **Header flag bit 4 — Amiga limits** clamp the channel's playback
  frequency to the PAL Amiga's hardware period range `[113, 856]`
  clock units (≈ `[16 725, 126 703]` Hz), per the same source §Flags:
  "Amiga limits (limit periods to confine to 113 <= x <= 856)". The
  base clock comes from the documented `8363 * 1712 = 14 317 456 Hz`
  constant (also under §Playback Notes). New `clamp_amiga` helper plus
  `AMIGA_CLOCK_HZ` / `AMIGA_LIMIT_PERIOD_MIN` / `AMIGA_LIMIT_PERIOD_MAX`
  constants. The clamp is applied at every frequency-mutating site:
  note triggers (immediate and `SDx`-deferred), `S2x` finetune, the
  fine + continuous `E*` / `F*` pitch slides, the `Gxx` and `Lxy`-G00
  tone portamento legs, the `Hxy` / `Uxy` / `Kxy`-H00 vibrato legs and
  `Jxy` arpeggio. Both `frequency` and `target_frequency` are clamped
  because tone portamento and the pitch slides rebase off
  `target_frequency`; clamping only `frequency` would let the next
  vibrato step silently re-escape the legal window. Five unit tests
  cover the low / high / disabled / legal-pass-through / zero-skip
  branches.
- **Initial speed / tempo spec edge cases.** Per the multimedia.cx
  reference ("Initial speed ... if 0 *or 255*, it is ignored ..."
  and "Initial tempo - if less than 33, it is ignored ..."), the
  ` PlayerState::new` constructor now treats `initial_speed` of `0`
  or `0xFF` as "use the default" (was: only `0` fell back), and
  `initial_tempo < 33` as the same (was: only `0` fell back). This
  matches the per-row `Txx` command's `info >= 0x20` guard so a row-0
  `T20` and an `initial_tempo` byte of `32` behave identically.
  Three new unit tests cover the speed-0xFF case, the tempo<33 sweep,
  and the new "33 is the first accepted tempo" boundary.

## [0.0.7](https://github.com/OxideAV/oxideav-s3m/compare/v0.0.6...v0.0.7) - 2026-05-29

### Other

- implement legacy stereo control per FireLight tutorial §6.23
- spec-correct default-pan resolution + mono override
- freeze/resume semantics per multimedia.cx — no longer zeroes volume
- honour the channel-settings `+128` mute flag
- continue running vibrato/porta (H00/G00 + Dxy) per multimedia.cx
- Qxy retrigger: persistent per-channel counter + exact volume-modifier table
- ST3 effect-memory + DFF/D0F/DF0/SC0/Cxx multimedia.cx alignment
- round-75 effect-set widening (Ixy / Uxy / SEx / S1x..S4x / fine slides / SC0)

### Added

- **SAx (legacy stereo control) is now implemented** per the FireLight
  S3M Player Tutorial §6.23
  (`docs/audio/trackers/s3m/FireLight-S3M-Player-Tutorial.txt`). The
  command swaps the high bit of its parameter nibble before writing the
  result to the channel pan slot — `SA0` lands on pan 8, `SA7` on pan
  15, `SA8` on pan 0, `SAF` on pan 7. The pseudocode `if (eparmy > 7)
  then temp = eparmy - 8 else temp = eparmy + 8; setpan(temp)` is
  exactly equivalent to `pan ^ 0x08`. ST3 itself stopped emitting `SAx`
  in new files (the editor uses `S8x` now), but the ScreamTracker 3.20
  effects reference
  (`docs/audio/trackers/s3m/ScreamTracker-v3.20-effects.txt` §SAx)
  documents PANIC.S3M by Future Crew as the canonical dependent for
  back-catalogue playback, and the FireLight tutorial adds STRSHINE.S3M
  to the list. The previous implementation silently dropped the
  command; modules using it would have their channel pan inherited
  from the default-pan path, mis-routing the stereo image. Two new
  tests under `tests/parse_header.rs` cover the `SA0` → pan 8 case
  in isolation plus a full sweep of every value in 0..=0xF asserting
  the XOR-0x8 mapping holds across the whole nibble.

### Fixed

- **Default-pan resolution now matches the ScreamTracker 3.20 spec**
  (`docs/audio/trackers/s3m/ScreamTracker-v3.20-s3m.txt` §"Channel pan
  settings") and the FireLight S3M Player Tutorial §2.8 / §2.8.1
  (`docs/audio/trackers/s3m/FireLight-S3M-Player-Tutorial.txt`). The
  spec says that when a pan byte's bit 5 is clear, the channel's pan
  must fall back to a default keyed by the master-volume stereo flag
  — `3` for left-bank PCM slots (channel type `0..=7`), `C` for
  right-bank slots (`8..=15`) in stereo mode; `7` (centre) for every
  channel in mono mode. The previous implementation collapsed the
  bit-5-clear case to `0x08` regardless of mode, so stereo modules
  whose pan block contained well-formed entries with bit 5 unset
  would lose their bank separation, and mono modules would not be
  forced to the centre. Adds the FireLight §2.8.1 mono override —
  in mono mode every channel pans to `7`, regardless of any explicit
  pan byte read from the pan block. Five new tests under
  `tests/parse_header.rs` cover the four resolution branches (stereo
  / mono × no-block / bit-5-clear / bit-5-set) plus the mono
  override.

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
