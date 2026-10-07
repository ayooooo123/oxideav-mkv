//! Encoder delay and end padding of audio tracks, exposed per packet as
//! `PacketMetadata::audio_trim` with the values FFmpeg's matroska demuxer
//! (2da55bf `libavformat/matroskadec.c`, with `libavformat/demux.c`)
//! attaches as `AV_PKT_DATA_SKIP_SAMPLES`. The decoder still sees every
//! sample; a consumer removes the trimmed ones after decoding.
//!
//! - `CodecDelay` (RFC 9559 §5.1.4.1.25): the track's first packet skips
//!   it (`initial_padding`). It replaces that packet's own padding, as
//!   `read_frame_internal` replaces the side data. Every timestamp of the
//!   track moves back by it, rounded to the nearest tick
//!   (`codec_delay_in_track_tb`), so the first sample kept plays at the
//!   track's start. Seeks stay in the Blocks' own timeline, as FFmpeg's
//!   Cue index does.
//! - `DiscardPadding` (§5.1.3.5.7): every packet of the Block drops it from
//!   its end, or skips it from its start when negative
//!   (`matroska_parse_frame`). Padding that rounds to no sample trims
//!   nothing.
//! - `SeekPreRoll` (§5.1.4.1.26): FFmpeg only reports it; RFC 9559 says the
//!   decoded output is not valid until that much has been dropped after a
//!   seek. After a seek the track's first packet skips it, or the Block's
//!   own leading skip when that is longer, and keeps the Block's end
//!   padding. A seek that lands on the track's first packet needs no
//!   pre-roll: that packet skips the `CodecDelay` instead, as after the
//!   open. The track's first packet is the first Block of the track the
//!   demuxer reads walking from the first Cluster: after the open, or after
//!   a seek that lands on the first Cluster's start. A seek that lands
//!   further on before that Block was ever read counts its next packet as
//!   one inside the track.
//!
//! Counts are in the track's rate: 48 kHz for Opus, the rate FFmpeg's Opus
//! decoder sets for the stream whatever its `SamplingFrequency` says, else
//! FFmpeg's `out_samplerate` (see [`track_rate`]). They are FFmpeg's whole
//! counts: an Opus decoder's own `OpusHead` pre-skip is a default a
//! consumer replaces with a container's skip, as libavcodec does. Counts
//! too large for a trim saturate. A track without a rate gets no trims;
//! its timestamps still move back by its `CodecDelay`.

use oxideav_core::{AudioTrim, TimeBase};

use super::timing::rescale_near;

const NS_PER_SECOND: i128 = 1_000_000_000;

/// Where the next packet of a track takes its start trim from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pending {
    /// Nothing: the packet trims only its own padding.
    None,
    /// The open: the track's `CodecDelay`.
    Open,
    /// A seek: the track's `SeekPreRoll`, or its `CodecDelay` on its first
    /// packet.
    Seek,
}

/// What one audio track's start skips and padding count, in samples.
#[derive(Clone, Copy, Debug)]
pub(super) struct TrackTrims {
    /// The rate every count is in.
    rate: u32,
    /// `CodecDelay` in samples.
    delay: u32,
    /// `SeekPreRoll` in samples.
    pre_roll: u32,
}

impl TrackTrims {
    /// The trims of a track at `rate` Hz, or of an Opus track, whose
    /// `CodecDelay` and `SeekPreRoll` are `codec_delay` and `seek_pre_roll`
    /// ns. `None` without a rate.
    pub(super) fn new(codec_delay: u64, seek_pre_roll: u64, rate: u32, opus: bool) -> Option<Self> {
        let rate = if opus { 48_000 } else { rate };
        (rate > 0).then(|| Self {
            rate,
            delay: samples(codec_delay.into(), rate),
            pre_roll: samples(seek_pre_roll.into(), rate),
        })
    }

    /// The trim of each packet of a Block whose `DiscardPadding` is
    /// `padding` ns.
    pub(super) fn padding(&self, padding: Option<i64>) -> Option<AudioTrim> {
        let padding = i128::from(padding?);
        let count = samples(padding.abs(), self.rate);
        (count > 0).then_some(AudioTrim {
            skip_samples: if padding < 0 { count } else { 0 },
            discard_padding: if padding > 0 { count } else { 0 },
            sample_rate: self.rate,
        })
    }

    /// The trim of the first packet after the open or a seek (`pending`),
    /// the track's `first` or not, whose own trim is `own`.
    pub(super) fn start(&self, pending: Pending, first: bool, own: Option<AudioTrim>) -> Option<AudioTrim> {
        let delay = pending == Pending::Open || (pending == Pending::Seek && first);
        if delay && self.delay > 0 {
            Some(AudioTrim { skip_samples: self.delay, discard_padding: 0, sample_rate: self.rate })
        } else if pending == Pending::Seek && !first && self.pre_roll > 0 {
            Some(AudioTrim {
                skip_samples: self.pre_roll.max(own.map_or(0, |t| t.skip_samples)),
                discard_padding: own.map_or(0, |t| t.discard_padding),
                sample_rate: self.rate,
            })
        } else {
            own
        }
    }
}

/// The rate FFmpeg counts a track's trims in (`par->sample_rate`), its
/// `out_samplerate` narrowed to an integer: the `OutputSamplingFrequency`
/// when present and nonzero, else the `SamplingFrequency`, which is 8000
/// when absent, negative, past `i32::MAX` or NaN. An
/// `OutputSamplingFrequency` out of that range counts as absent.
pub(super) fn track_rate(sampling: Option<f64>, output: Option<f64>) -> u32 {
    let valid = |rate: f64| (0.0..=f64::from(i32::MAX)).contains(&rate);
    let sampling = sampling.filter(|&rate| valid(rate)).unwrap_or(8000.0);
    output.filter(|&rate| rate != 0.0 && valid(rate)).unwrap_or(sampling) as u32
}

/// `ns` nanoseconds in samples at `rate` Hz, rounded to the nearest with
/// halves away from zero (`av_rescale_q`), saturating at the trim's range.
fn samples(ns: i128, rate: u32) -> u32 {
    rescale_near(ns, rate.into(), NS_PER_SECOND).map_or(u32::MAX, |n| n.clamp(0, u32::MAX.into()) as u32)
}

/// `CodecDelay` of `ns` nanoseconds in ticks of `tb`, rounded to the
/// nearest with halves away from zero (`codec_delay_in_track_tb`),
/// saturating.
pub(super) fn delay_ticks(ns: u64, tb: TimeBase) -> i64 {
    let tb = tb.as_rational();
    let ticks = rescale_near(ns.into(), tb.den.into(), i128::from(tb.num) * NS_PER_SECOND);
    ticks.map_or(0, |t| t.clamp(0, i64::MAX.into()) as i64)
}
