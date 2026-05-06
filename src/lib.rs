//! Scream Tracker 3 Module (S3M) support.
//!
//! S3M is the next generation up from MOD: up to 32 channels, richer
//! instruments (mono / stereo / 8-bit / 16-bit), a letter-based effect
//! command set (`Axx` through `Zxx`), per-channel pan, and a packed
//! pattern encoding that skips empty cells.
//!
//! This crate registers:
//!
//! - A **container** (`s3m`) that slurps the whole file and emits it as a
//!   single packet (the same approach as `oxideav-mod`).
//! - A **codec** (`s3m`) whose decoder parses header + instruments +
//!   patterns + sample bodies and drives a 44.1 kHz stereo-S16 mixer.
//! - A **per-channel codec** (`s3m_multichannel`) that emits one stereo
//!   pair per S3M channel instead of a single summed stereo stream —
//!   useful for DAWs, visualizers, and per-instrument remastering. See
//!   [`decoder::CODEC_ID_MULTICHANNEL`] and [`player::PlayerState::render_per_channel`].

pub mod container;
pub mod decoder;
pub mod header;
pub mod pattern;
pub mod player;
pub mod samples;

use oxideav_core::CodecRegistry;
use oxideav_core::ContainerRegistry;
use oxideav_core::RuntimeContext;

pub const CODEC_ID_STR: &str = "s3m";
pub use decoder::CODEC_ID_MULTICHANNEL;

pub fn register_codecs(reg: &mut CodecRegistry) {
    decoder::register(reg);
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    container::register(reg);
}

/// Unified entry point: install every codec and container provided by
/// `oxideav-s3m` into a [`RuntimeContext`].
///
/// Also wired into [`oxideav_meta::register_all`] via the
/// [`oxideav_core::register!`] macro below.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("s3m", register);

#[cfg(test)]
mod register_tests {
    use super::*;

    #[test]
    fn register_via_runtime_context_installs_factories() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        assert!(
            ctx.codecs.decoder_ids().next().is_some(),
            "register(ctx) should install codec decoder factories"
        );
        assert!(
            ctx.containers.demuxer_names().next().is_some(),
            "register(ctx) should install container demuxer factories"
        );
    }
}
