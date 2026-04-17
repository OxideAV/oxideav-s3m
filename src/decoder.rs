//! S3M codec decoder — drives a `PlayerState` and emits PCM.
//!
//! Two decoder variants are registered:
//!
//! - `s3m` (the default): mixed stereo S16 output, 44.1 kHz. All S3M
//!   channels are summed into one L/R pair.
//! - `s3m_multichannel`: per-channel stereo streams, still at 44.1 kHz,
//!   interleaved as `[ch0_L, ch0_R, ch1_L, ch1_R, …, chN_L, chN_R]`
//!   where `N = 32` (S3M's channel slot count — disabled slots emit
//!   silence). Consumers that want individual channel streams
//!   deinterleave by striding `2 * 32` i16 per output frame.

use oxideav_codec::{CodecRegistry, Decoder};
use oxideav_core::{
    AudioFrame, CodecCapabilities, CodecId, CodecParameters, Error, Frame, Packet, Result,
    SampleFormat, TimeBase,
};

use crate::container::OUTPUT_SAMPLE_RATE;
use crate::header::{parse_header, CHANNEL_COUNT};
use crate::pattern::unpack_all;
use crate::player::PlayerState;
use crate::samples::extract_samples;

/// Alternate codec id that exposes one stereo pair per S3M channel. See
/// module docs for the sample layout.
pub const CODEC_ID_MULTICHANNEL: &str = "s3m_multichannel";

pub fn register(reg: &mut CodecRegistry) {
    let mixed_caps = CodecCapabilities::audio("s3m_sw")
        .with_lossy(false)
        .with_lossless(true)
        .with_intra_only(false)
        .with_max_channels(32)
        .with_max_sample_rate(OUTPUT_SAMPLE_RATE);
    reg.register_decoder_impl(CodecId::new(crate::CODEC_ID_STR), mixed_caps, make_mixed);

    let mc_caps = CodecCapabilities::audio("s3m_sw_mc")
        .with_lossy(false)
        .with_lossless(true)
        .with_intra_only(false)
        // Per-channel output is 2 * 32 = 64 interleaved samples per frame.
        .with_max_channels((CHANNEL_COUNT * 2) as u16)
        .with_max_sample_rate(OUTPUT_SAMPLE_RATE);
    reg.register_decoder_impl(
        CodecId::new(CODEC_ID_MULTICHANNEL),
        mc_caps,
        make_multichannel,
    );
}

fn make_mixed(_params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(S3mDecoder {
        codec_id: CodecId::new(crate::CODEC_ID_STR),
        mode: OutputMode::MixedStereo,
        state: DecoderState::AwaitingPacket,
    }))
}

fn make_multichannel(_params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(S3mDecoder {
        codec_id: CodecId::new(CODEC_ID_MULTICHANNEL),
        mode: OutputMode::PerChannel,
        state: DecoderState::AwaitingPacket,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    MixedStereo,
    PerChannel,
}

struct S3mDecoder {
    codec_id: CodecId,
    mode: OutputMode,
    state: DecoderState,
}

enum DecoderState {
    AwaitingPacket,
    Playing {
        player: Box<PlayerState>,
        emit_pts: i64,
    },
    Done,
}

const CHUNK_FRAMES: u32 = 1024;

impl Decoder for S3mDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if !matches!(self.state, DecoderState::AwaitingPacket) {
            return Err(Error::other(
                "S3M decoder received a second packet; only one is expected per song",
            ));
        }
        let header = parse_header(&packet.data)?;
        let samples = extract_samples(&header, &packet.data);
        let patterns = unpack_all(&header, &packet.data);
        let player = PlayerState::new(&header, samples, patterns, OUTPUT_SAMPLE_RATE);
        self.state = DecoderState::Playing {
            player: Box::new(player),
            emit_pts: 0,
        };
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match &mut self.state {
            DecoderState::AwaitingPacket => Err(Error::NeedMore),
            DecoderState::Done => Err(Error::Eof),
            DecoderState::Playing { player, emit_pts } => match self.mode {
                OutputMode::MixedStereo => {
                    let mut pcm = vec![0i16; CHUNK_FRAMES as usize * 2];
                    let produced = player.render(&mut pcm);
                    if produced == 0 {
                        self.state = DecoderState::Done;
                        return Err(Error::Eof);
                    }
                    pcm.truncate(produced * 2);

                    let mut bytes = Vec::with_capacity(pcm.len() * 2);
                    for s in &pcm {
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }

                    let pts = *emit_pts;
                    *emit_pts += produced as i64;
                    Ok(Frame::Audio(AudioFrame {
                        format: SampleFormat::S16,
                        channels: 2,
                        sample_rate: OUTPUT_SAMPLE_RATE,
                        samples: produced as u32,
                        pts: Some(pts),
                        time_base: TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64),
                        data: vec![bytes],
                    }))
                }
                OutputMode::PerChannel => {
                    let stride = player.channel_count() * 2;
                    let mut pcm = vec![0i16; CHUNK_FRAMES as usize * stride];
                    let produced = player.render_per_channel(&mut pcm);
                    if produced == 0 {
                        self.state = DecoderState::Done;
                        return Err(Error::Eof);
                    }
                    pcm.truncate(produced * stride);

                    let mut bytes = Vec::with_capacity(pcm.len() * 2);
                    for s in &pcm {
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }

                    let pts = *emit_pts;
                    *emit_pts += produced as i64;
                    Ok(Frame::Audio(AudioFrame {
                        format: SampleFormat::S16,
                        channels: stride as u16,
                        sample_rate: OUTPUT_SAMPLE_RATE,
                        samples: produced as u32,
                        pts: Some(pts),
                        time_base: TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64),
                        data: vec![bytes],
                    }))
                }
            },
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        // Drop the entire PlayerState (mixer voices with sample position /
        // volume envelope, pattern-row cursor, tick counter, tempo /
        // BPM, effect memory per channel). Back to `AwaitingPacket`; the
        // S3M container re-sends the whole-file packet after a seek.
        self.state = DecoderState::AwaitingPacket;
        Ok(())
    }
}
