//! Position-trace harness for the order sequencer (`Bxx` / `Cxx` / `SBx`
//! and the song-end path), against the black-box ScreamTracker 3.21
//! position traces.
//!
//! The twelve probe modules and `trace.txt` under
//! `tests/fixtures/st321-jump-probes/` are a verbatim copy of the docs
//! collaborator's self-authored fixtures
//! (`docs/audio/trackers/s3m/fixtures/st321-jump-probes/`). Each probe was
//! played in the original tracker under an emulator and the play position
//! was read off the tracker's own status line
//! (`Playing; loop:L ord:O/N pat:P row:R`), consecutive duplicates
//! collapsed. The trace lists the first 70 displayed states of every probe
//! and, as its header notes, no listed row was skipped by the sampler, so
//! the trace is an exact prefix of the row-entry sequence.
//!
//! The harness plays each probe through `PlayerState` with
//! `SongEndPolicy::Restart` (the tracker restarts at order 0 and counts the
//! `loop`), records one `(loop, order, pattern, row)` state per row entry,
//! collapses consecutive duplicates exactly as the trace does, and demands
//! an exact prefix match. It also checks the displayed last order `N`
//! against `song_length() - 1`.

use std::path::{Path, PathBuf};

use oxideav_s3m::header::parse_header;
use oxideav_s3m::pattern::unpack_all;
use oxideav_s3m::player::{PlayPosition, PlayerState, SongEndPolicy};
use oxideav_s3m::samples::extract_samples;

/// One state of the tracker status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shown {
    song_loops: u32,
    order: u8,
    last_order: u16,
    pattern: u8,
    row: u8,
}

const PROBES: [&str; 12] = [
    "BMARK", "BONMK", "BAFTER", "BEND", "BPAST", "CLAST", "BC", "CB", "SBC", "CSB", "SBB", "BSB",
];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/st321-jump-probes")
}

/// Parse `trace.txt` into `(probe name, states)` sections.
fn parse_trace(text: &str) -> Vec<(String, Vec<Shown>)> {
    let mut out: Vec<(String, Vec<Shown>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("## ") {
            let name = rest.split_whitespace().next().unwrap().to_string();
            out.push((name, Vec::new()));
            continue;
        }
        if !line.starts_with("loop=") {
            continue;
        }
        let mut song_loops = None;
        let mut order = None;
        let mut last_order = None;
        let mut pattern = None;
        let mut row = None;
        for field in line.split_whitespace() {
            let (k, v) = field.split_once('=').unwrap();
            match k {
                "loop" => song_loops = Some(v.parse().unwrap()),
                "ord" => {
                    let (o, n) = v.split_once('/').unwrap();
                    order = Some(o.parse().unwrap());
                    last_order = Some(n.parse().unwrap());
                }
                "pat" => pattern = Some(v.parse().unwrap()),
                "row" => row = Some(v.parse().unwrap()),
                other => panic!("unexpected trace field {other}"),
            }
        }
        let state = Shown {
            song_loops: song_loops.unwrap(),
            order: order.unwrap(),
            last_order: last_order.unwrap(),
            pattern: pattern.unwrap(),
            row: row.unwrap(),
        };
        out.last_mut()
            .expect("trace state before any section header")
            .1
            .push(state);
    }
    out
}

/// Low output rate: the sequencer is rate-independent and 8 kHz keeps the
/// render cheap (160 frames per tick at tempo 125).
const RATE: u32 = 8_000;

fn load_player(name: &str, policy: SongEndPolicy) -> PlayerState {
    let bytes = std::fs::read(fixture_dir().join(format!("{name}.S3M"))).unwrap();
    let header = parse_header(&bytes).unwrap();
    let samples = extract_samples(&header, &bytes);
    let patterns = unpack_all(&header, &bytes);
    PlayerState::new(&header, samples, patterns, RATE).with_song_end_policy(policy)
}

/// Play `name` with the tracker's restart behaviour and return its
/// collapsed row-entry trace, at most `limit` states long.
fn play_trace(name: &str, limit: usize) -> Vec<Shown> {
    play_trace_with(name, SongEndPolicy::restart(1_000), limit)
}

fn fmt(s: &Shown) -> String {
    format!(
        "loop={} ord={:03}/{:03} pat={:02} row={:02}",
        s.song_loops, s.order, s.last_order, s.pattern, s.row
    )
}

#[test]
fn every_probe_matches_the_tracker_position_trace() {
    let text = std::fs::read_to_string(fixture_dir().join("trace.txt")).unwrap();
    let sections = parse_trace(&text);
    let names: Vec<&str> = sections.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, PROBES, "trace sections");

    let mut failures = Vec::new();
    for (name, want) in &sections {
        assert!(!want.is_empty(), "{name}: empty trace section");
        let got = play_trace(name, want.len());
        if got != *want {
            let first_bad = got
                .iter()
                .zip(want.iter())
                .position(|(g, w)| g != w)
                .unwrap_or(got.len().min(want.len()));
            failures.push(format!(
                "{name}: diverges at state {first_bad}: got {} / want {} (got {} states, want {})",
                got.get(first_bad)
                    .map(fmt)
                    .unwrap_or_else(|| "<end>".into()),
                want.get(first_bad)
                    .map(fmt)
                    .unwrap_or_else(|| "<end>".into()),
                got.len(),
                want.len()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn stop_policy_ends_the_decode_at_the_first_song_end() {
    // Under the default policy the first song end terminates the decode:
    // the trace's `loop=1` states are never reached, and every state that
    // *is* reached matches the tracker's `loop=0` prefix.
    let text = std::fs::read_to_string(fixture_dir().join("trace.txt")).unwrap();
    for (name, want) in parse_trace(&text) {
        let mut p = load_player(&name, SongEndPolicy::default());
        let mut buf = vec![0i16; 4096];
        let mut guard = 0;
        while !p.ended && guard < 100_000 {
            p.render(&mut buf);
            guard += 1;
        }
        assert!(p.ended, "{name}: Stop policy must end the decode");
        assert_eq!(p.song_loops(), 0, "{name}: Stop never restarts");
        // Every loop=0 state in the trace precedes the stop.
        let loop0 = want.iter().filter(|s| s.song_loops == 0).count();
        let got = play_trace_with(&name, SongEndPolicy::default(), want.len());
        assert_eq!(got.len(), loop0, "{name}: states before the song end");
        assert_eq!(&got[..], &want[..loop0], "{name}: loop-0 prefix");
    }
}

/// Play `name` under `policy`; one state per row entry, consecutive
/// duplicates collapsed, at most `limit` states.
fn play_trace_with(name: &str, policy: SongEndPolicy, limit: usize) -> Vec<Shown> {
    let mut p = load_player(name, policy);
    let last_order = (p.song_length() as u16).saturating_sub(1);
    let mut out: Vec<Shown> = Vec::new();
    let mut buf = vec![0i16; 2];
    for _ in 0..200_000 {
        if out.len() >= limit || p.ended {
            break;
        }
        let entering_row = p.tick == 0 && p.tick_sample_cursor == 0;
        // Render exactly one tick so the row entry (if any) happens now.
        let spt = p.samples_per_tick().max(1) as usize;
        buf.resize(spt * 2, 0);
        p.render(&mut buf);
        if entering_row && !p.ended {
            let pos: PlayPosition = p.position();
            let s = Shown {
                song_loops: pos.song_loops,
                order: pos.order,
                last_order,
                pattern: pos.pattern,
                row: pos.row,
            };
            if out.last() != Some(&s) {
                out.push(s);
            }
        }
    }
    out
}

#[test]
fn restart_budget_is_honoured() {
    // BEND restarts every 9 rows; a budget of 3 restarts plays loop 0..=3
    // and then stops at the fourth song end.
    let mut p = load_player("BEND", SongEndPolicy::restart(3));
    let mut buf = vec![0i16; 4096];
    let mut guard = 0;
    while !p.ended && guard < 100_000 {
        p.render(&mut buf);
        guard += 1;
    }
    assert!(p.ended);
    assert_eq!(p.song_loops(), 3);
}

#[test]
fn vendored_fixtures_match_the_docs_corpus_when_present() {
    // The vendored copy must stay byte-identical to the staged docs
    // fixtures; skipped when the docs corpus is not checked out next to
    // the crate (standalone CI).
    let docs = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/audio/trackers/s3m/fixtures/st321-jump-probes");
    if !docs.is_dir() {
        eprintln!("skip: {} not present", docs.display());
        return;
    }
    let mut files: Vec<String> = PROBES.iter().map(|n| format!("{n}.S3M")).collect();
    files.push("trace.txt".into());
    for f in files {
        let a = std::fs::read(fixture_dir().join(&f)).unwrap();
        let b = std::fs::read(docs.join(&f)).unwrap();
        assert_eq!(a, b, "{f}: vendored copy differs from the docs corpus");
    }
}
