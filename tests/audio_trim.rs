//! Encoder delay and end padding exposed per packet as
//! `PacketMetadata::audio_trim`, as FFmpeg's matroska demuxer attaches
//! `AV_PKT_DATA_SKIP_SAMPLES`: a track's `CodecDelay` on its first packet,
//! with every timestamp of the track moved back by it; a Block's
//! `DiscardPadding` on each of its packets; and after a seek, the track's
//! `SeekPreRoll` on the first packet that follows, or its `CodecDelay`
//! where the seek lands on the track's start.

use std::io::Cursor;

use oxideav_core::{AudioTrim, Demuxer, NullCodecResolver};
use oxideav_mkv::demux::{self, MkvDemuxer};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

fn elem(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = write_element_id(id);
    out.extend(write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, v: u64) -> Vec<u8> {
    elem(id, &v.to_be_bytes())
}

fn file(segment: &[Vec<u8>]) -> Vec<u8> {
    let mut out = elem(ids::EBML_HEADER, &elem(ids::EBML_DOC_TYPE, b"matroska"));
    out.extend(write_element_id(ids::SEGMENT));
    out.extend(write_vint(segment.iter().map(Vec::len).sum::<usize>() as u64, 0));
    for element in segment {
        out.extend_from_slice(element);
    }
    out
}

/// An Audio master at `rate` Hz, stereo, with an OutputSamplingFrequency
/// of `output` when given.
fn audio(rate: f64, output: Option<f64>) -> Vec<u8> {
    let output = output.map_or(Vec::new(), |o| elem(ids::OUTPUT_SAMPLING_FREQUENCY, &o.to_be_bytes()));
    elem(ids::AUDIO, &[elem(ids::SAMPLING_FREQUENCY, &rate.to_be_bytes()), uint(ids::CHANNELS, 2), output].concat())
}

/// TrackEntry `number` of type `kind` with `codec`, its `private` data and
/// the `extra` children.
fn entry(number: u64, kind: u64, codec: &str, private: &[u8], extra: &[u8]) -> Vec<u8> {
    elem(ids::TRACK_ENTRY, &[
        uint(ids::TRACK_NUMBER, number), uint(ids::TRACK_UID, number), uint(ids::TRACK_TYPE, kind),
        elem(ids::CODEC_ID, codec.as_bytes()), elem(ids::CODEC_PRIVATE, private), extra.to_vec(),
    ].concat())
}

/// Audio track 1 at `rate` Hz, stereo, with `codec`, its `private` data and
/// the `timing` children (`CodecDelay`, `SeekPreRoll`).
fn audio_track(codec: &str, rate: f64, private: &[u8], timing: &[u8]) -> Vec<u8> {
    elem(ids::TRACKS, &entry(1, ids::TRACK_TYPE_AUDIO, codec, private, &[audio(rate, None), timing.to_vec()].concat()))
}

/// An `OpusHead` for two channels with a 312-sample pre-skip.
fn opus_head() -> Vec<u8> {
    [&b"OpusHead"[..], &[1, 2], &312u16.to_le_bytes(), &48_000u32.to_le_bytes(), &[0, 0, 0]].concat()
}

/// `CodecDelay` of 6.5 ms (312 samples) and `SeekPreRoll` of 80 ms (3840
/// samples), as FFmpeg writes them for Opus.
fn opus_timing() -> Vec<u8> {
    [uint(ids::CODEC_DELAY, 6_500_000), uint(ids::SEEK_PRE_ROLL, 80_000_000)].concat()
}

/// A 20 ms Opus packet: CELT fullband, stereo, one frame.
const OPUS: [u8; 3] = [0xFC, 0xFF, 0xFE];

/// A keyframe SimpleBlock on track 1 at `offset` ms into its Cluster.
fn simple(offset: i16, payload: &[u8]) -> Vec<u8> {
    elem(ids::SIMPLE_BLOCK, &[&[0x81][..], &offset.to_be_bytes(), &[0x80], payload].concat())
}

/// A BlockGroup on track 1 at `offset`, laced as `flags` says, whose
/// `DiscardPadding` is `padding` ns.
fn padded(offset: i16, flags: u8, body: &[u8], padding: i64) -> Vec<u8> {
    let block = elem(ids::BLOCK, &[&[0x81][..], &offset.to_be_bytes(), &[flags], body].concat());
    elem(ids::BLOCK_GROUP, &[block, elem(ids::DISCARD_PADDING, &padding.to_be_bytes())].concat())
}

fn cluster(tc: u64, blocks: &[Vec<u8>]) -> Vec<u8> {
    elem(ids::CLUSTER, &[uint(ids::TIMECODE, tc), blocks.concat()].concat())
}

/// Three Clusters a second apart, each of fifty 20 ms Opus packets. The
/// last one's Block carries a `DiscardPadding` of 13.5 ms: 648 samples.
fn opus_file(timing: &[u8]) -> Vec<u8> {
    let mut segment = vec![audio_track("A_OPUS", 48_000.0, &opus_head(), timing)];
    for c in 0..3u64 {
        let mut blocks: Vec<Vec<u8>> = (0..50).map(|i| simple(i * 20, &OPUS)).collect();
        if c == 2 {
            blocks[49] = padded(980, 0, &OPUS, 13_500_000);
        }
        segment.push(cluster(c * 1000, &blocks));
    }
    file(&segment)
}

/// Two Clusters a second apart, each of three AAC-LC frames 21 ms apart,
/// with a `CodecDelay` of 1024 samples at 48 kHz.
fn aac_file() -> Vec<u8> {
    let track = audio_track("A_AAC", 48_000.0, &[0x11, 0x90], &uint(ids::CODEC_DELAY, 21_333_333));
    let blocks: Vec<Vec<u8>> = (0..3).map(|i| simple(i * 21, &[0x21, 0x10, 0x04])).collect();
    file(&[track, cluster(0, &blocks), cluster(1000, &blocks)])
}

fn open(bytes: Vec<u8>) -> MkvDemuxer {
    demux::open_typed(Box::new(Cursor::new(bytes)), &NullCodecResolver).unwrap()
}

fn trim(skip_samples: u32, discard_padding: u32, sample_rate: u32) -> Option<AudioTrim> {
    Some(AudioTrim { skip_samples, discard_padding, sample_rate })
}

/// The pts and audio trim of each packet `d` returns from where it is.
fn drained(d: &mut MkvDemuxer) -> Vec<(Option<i64>, Option<AudioTrim>)> {
    std::iter::from_fn(|| d.next_packet().ok().map(|p| (p.pts, d.packet_metadata().audio_trim))).collect()
}

/// The first packet after a seek to `target` ms: where the seek landed,
/// the metadata straight after it, and the packet's pts and trim.
fn seek(d: &mut MkvDemuxer, target: i64) -> (i64, Option<AudioTrim>, Option<i64>, Option<AudioTrim>) {
    let landed = d.seek_to(0, target).unwrap();
    let before = d.packet_metadata().audio_trim;
    let pts = d.next_packet().unwrap().pts;
    (landed, before, pts, d.packet_metadata().audio_trim)
}

/// The track's first packet skips its `CodecDelay`, counted at 48 kHz for
/// Opus and at the track's rate otherwise, and every timestamp of the track
/// moves back by it, rounded to the nearest tick: 7 ms for Opus, 21 ms for
/// AAC. The last packet discards its Block's padding.
#[test]
fn codec_delay_trims_the_first_packet_and_moves_every_timestamp_back() {
    let opus = drained(&mut open(opus_file(&opus_timing())));
    assert_eq!(opus.len(), 150);
    assert_eq!(opus[0], (Some(-7), trim(312, 0, 48_000)));
    assert_eq!(opus[1], (Some(13), None));
    assert!(opus[1..149].iter().all(|(_, t)| t.is_none()), "{opus:?}");
    assert_eq!(opus[149], (Some(2973), trim(0, 648, 48_000)));
    let aac = drained(&mut open(aac_file()));
    let pts: Vec<Option<i64>> = aac.iter().map(|(pts, _)| *pts).collect();
    assert_eq!(pts, [-21, 0, 21, 979, 1000, 1021].map(Some));
    assert_eq!(aac[0].1, trim(1024, 0, 48_000));
    assert!(aac[1..].iter().all(|(_, t)| t.is_none()), "{aac:?}");
    // Without a CodecDelay nothing moves and nothing is skipped.
    let plain = drained(&mut open(opus_file(&[])));
    assert_eq!(plain[0], (Some(0), None));
    assert_eq!(plain[149], (Some(2980), trim(0, 648, 48_000)));
}

/// A Block's `DiscardPadding` trims the end of every packet laced in it, or
/// its start when negative, as FFmpeg attaches it to each frame, at the
/// track's rate: 48 kHz for Opus whatever its `SamplingFrequency`, as
/// FFmpeg's Opus decoder sets it. Padding that rounds to no sample trims
/// nothing.
#[test]
fn discard_padding_trims_each_packet_of_its_block() {
    // Two frames in one Xiph-laced Block: one lace size, then both.
    let laced = [&[1u8, OPUS.len() as u8][..], &OPUS, &OPUS].concat();
    let blocks = [
        simple(0, &OPUS),
        padded(20, 0, &OPUS, -2_500_000),
        padded(40, 0x02, &laced, 5_000_000),
        padded(80, 0, &OPUS, 5),
    ];
    let trims = |track: Vec<u8>| -> Vec<Option<AudioTrim>> {
        drained(&mut open(file(&[track, cluster(0, &blocks)]))).into_iter().map(|(_, t)| t).collect()
    };
    let opus = [None, trim(120, 0, 48_000), trim(0, 240, 48_000), trim(0, 240, 48_000), None];
    assert_eq!(trims(audio_track("A_OPUS", 48_000.0, &opus_head(), &[])), opus);
    assert_eq!(trims(audio_track("A_OPUS", 44_100.0, &opus_head(), &[])), opus);
    // 2.5 ms and 5 ms at 44.1 kHz: 110.25 and 220.5 samples, to the nearest.
    let aac = trims(audio_track("A_AAC", 44_100.0, &[0x12, 0x10], &[]));
    assert_eq!(aac, [None, trim(110, 0, 44_100), trim(0, 221, 44_100), trim(0, 221, 44_100), None]);
}

/// After a seek the first packet skips the track's `SeekPreRoll`, decoded
/// and dropped while the decoder settles, or, where the seek lands on the
/// track's start, its `CodecDelay` as at the open. The metadata is empty
/// until that packet is read, and the packets after it trim nothing.
#[test]
fn a_seek_skips_the_seek_pre_roll_or_the_codec_delay_at_the_start() {
    let mut d = open(opus_file(&opus_timing()));
    assert_eq!(seek(&mut d, 1000), (1000, None, Some(993), trim(3840, 0, 48_000)));
    assert_eq!(drained(&mut d)[0], (Some(1013), None));
    assert_eq!(seek(&mut d, 0), (0, None, Some(-7), trim(312, 0, 48_000)));
    // Without a SeekPreRoll a seek inside the stream skips nothing.
    let mut d = open(aac_file());
    assert_eq!(seek(&mut d, 1000), (1000, None, Some(979), None));
    assert_eq!(seek(&mut d, 0), (0, None, Some(-21), trim(1024, 0, 48_000)));
}

/// Counts too large for a trim saturate; they never wrap or panic: the
/// largest `CodecDelay`, `SeekPreRoll` and `DiscardPadding` either way.
/// At 1 ms ticks the largest `CodecDelay` moves every timestamp back by
/// 18,446,744,073,710 ticks.
#[test]
fn trims_past_their_range_saturate() {
    let timing = [uint(ids::CODEC_DELAY, u64::MAX), uint(ids::SEEK_PRE_ROLL, u64::MAX)].concat();
    let blocks = [simple(0, &OPUS), padded(20, 0, &OPUS, i64::MAX), padded(40, 0, &OPUS, i64::MIN), simple(60, &OPUS)];
    let bytes = file(&[audio_track("A_OPUS", 48_000.0, &opus_head(), &timing), cluster(0, &blocks), cluster(1000, &blocks)]);
    let mut d = open(bytes);
    let (pts, trims): (Vec<Option<i64>>, Vec<Option<AudioTrim>>) = drained(&mut d).into_iter().unzip();
    assert_eq!(pts, [0, 20, 40, 60, 1000, 1020, 1040, 1060].map(|t| Some(t - 18_446_744_073_710)));
    let max = u32::MAX;
    assert_eq!(trims, [
        trim(max, 0, 48_000), trim(0, max, 48_000), trim(max, 0, 48_000), None,
        None, trim(0, max, 48_000), trim(max, 0, 48_000), None,
    ]);
    assert_eq!(seek(&mut d, 1000).3, trim(max, 0, 48_000));
}

/// An Opus track with `extra` children, after an Info with a TimestampScale
/// of `scale` ns.
fn scaled_opus(scale: u64, extra: &[u8]) -> [Vec<u8>; 2] {
    let track = entry(1, ids::TRACK_TYPE_AUDIO, "A_OPUS", &opus_head(), &[audio(48_000.0, None), extra.to_vec()].concat());
    [elem(ids::INFO, &uint(ids::TIMECODE_SCALE, scale)), elem(ids::TRACKS, &track)]
}

/// Timestamps past the range of the time base saturate instead of
/// wrapping. With 1 ns ticks the largest `CodecDelay` is more ticks than a
/// timestamp holds: it moves the track back by `i64::MAX`, a packet at the
/// largest Cluster Timestamp back to 0, and one that a TrackTimestampScale
/// of 1.5 starts before zero to `i64::MIN`. With 4 s ticks, half a tick of
/// `CodecDelay` rounds to one.
#[test]
fn timestamps_at_the_bounds_of_the_time_base_saturate() {
    let delay = |ns: u64| uint(ids::CODEC_DELAY, ns);
    let one_ns = [
        scaled_opus(1, &delay(u64::MAX)).to_vec(),
        vec![cluster(0, &[simple(0, &OPUS)]), cluster(i64::MAX as u64, &[simple(0, &OPUS)])],
    ].concat();
    let max = trim(u32::MAX, 0, 48_000);
    assert_eq!(drained(&mut open(file(&one_ns))), [(Some(-i64::MAX), max), (Some(0), None)]);
    // Cluster 60 scaled by 1.5 is tick 40; the Block is 50 before it.
    let scale = elem(ids::TRACK_TIMESTAMP_SCALE, &1.5f64.to_be_bytes());
    let before_zero = [scaled_opus(1, &[delay(u64::MAX), scale].concat()).to_vec(), vec![cluster(60, &[simple(-50, &OPUS)])]].concat();
    assert_eq!(drained(&mut open(file(&before_zero))), [(Some(i64::MIN), max)]);
    let four_s = [
        scaled_opus(4_000_000_000, &delay(2_000_000_000)).to_vec(),
        vec![cluster(0, &[simple(0, &OPUS)]), cluster(1, &[simple(0, &OPUS)])],
    ].concat();
    assert_eq!(drained(&mut open(file(&four_s))), [(Some(-1), trim(96_000, 0, 48_000)), (Some(0), None)]);
}

/// Opus track 1 starting at 100 ms: forty-five 20 ms packets from there in
/// the first Cluster, then fifty in a Cluster at 1000 ms.
fn late_opus_file() -> Vec<u8> {
    let first: Vec<Vec<u8>> = (0..45).map(|i| simple(100 + i * 20, &OPUS)).collect();
    let second: Vec<Vec<u8>> = (0..50).map(|i| simple(i * 20, &OPUS)).collect();
    file(&[audio_track("A_OPUS", 48_000.0, &opus_head(), &opus_timing()), cluster(0, &first), cluster(1000, &second)])
}

/// The start of a track is its first packet, whatever its timestamp: a
/// seek back to a track that starts at 100 ms skips its `CodecDelay`, not
/// its `SeekPreRoll`, whether the track's first packet was read before the
/// seek or not. So does a seek to the start of a file whose audio starts
/// in its second Cluster, behind a subtitle. A seek into the track skips
/// its `SeekPreRoll`.
#[test]
fn a_seek_to_a_tracks_first_packet_skips_its_codec_delay() {
    let mut d = open(late_opus_file());
    assert_eq!(drained(&mut d)[0], (Some(93), trim(312, 0, 48_000)));
    assert_eq!(seek(&mut d, 0), (0, None, Some(93), trim(312, 0, 48_000)));
    assert_eq!(seek(&mut d, 1000), (1000, None, Some(993), trim(3840, 0, 48_000)));
    let mut d = open(late_opus_file());
    assert_eq!(seek(&mut d, 0), (0, None, Some(93), trim(312, 0, 48_000)));
    // Opus track 1 in the second Cluster; subtitle track 2 in the first.
    let tracks = elem(ids::TRACKS, &[
        entry(1, ids::TRACK_TYPE_AUDIO, "A_OPUS", &opus_head(), &[audio(48_000.0, None), opus_timing()].concat()),
        entry(2, ids::TRACK_TYPE_SUBTITLE, "S_TEXT/UTF8", &[], &[]),
    ].concat());
    let subtitle = elem(ids::SIMPLE_BLOCK, &[0x82, 0, 0, 0x80, b's']);
    let audio_packets: Vec<Vec<u8>> = (0..50).map(|i| simple(i * 20, &OPUS)).collect();
    let mut d = open(file(&[tracks, cluster(0, &[subtitle]), cluster(1000, &audio_packets)]));
    let landed = d.seek_to(0, 0).unwrap();
    let first_audio = std::iter::from_fn(|| d.next_packet().ok().map(|p| (p.stream_index, p.pts, d.packet_metadata().audio_trim)))
        .find(|(stream, _, _)| *stream == 0);
    assert_eq!((landed, first_audio), (0, Some((0, Some(993), trim(312, 0, 48_000)))));
}

/// After a seek into the track, a Block's own leading skip (a negative
/// `DiscardPadding`) still applies when it is longer than the
/// `SeekPreRoll`: the larger skip wins, and the Block's end padding stays.
#[test]
fn a_seek_keeps_the_larger_of_its_pre_roll_and_the_blocks_own_skip() {
    // The Cluster at 1000 ms starts with a Block padded by `padding` ns.
    let padded_at_1000 = |padding: i64| {
        let mut segment = vec![audio_track("A_OPUS", 48_000.0, &opus_head(), &opus_timing())];
        for c in 0..3u64 {
            let mut blocks: Vec<Vec<u8>> = (0..50).map(|i| simple(i * 20, &OPUS)).collect();
            if c == 1 {
                blocks[0] = padded(0, 0, &OPUS, padding);
            }
            segment.push(cluster(c * 1000, &blocks));
        }
        file(&segment)
    };
    for (padding, expected) in [
        (-100_000_000, trim(4800, 0, 48_000)),
        (-20_000_000, trim(3840, 0, 48_000)),
        (5_000_000, trim(3840, 240, 48_000)),
    ] {
        let mut d = open(padded_at_1000(padding));
        assert_eq!(seek(&mut d, 1000).3, expected, "DiscardPadding {padding} ns");
    }
}

/// A track's OutputSamplingFrequency is the rate of its counts, as FFmpeg's
/// `out_samplerate`: 10 µs is one sample at 96 kHz but none at the 48 kHz
/// SamplingFrequency. Opus counts stay at 48 kHz.
#[test]
fn the_output_sampling_frequency_is_the_rate_of_the_counts() {
    let pcm = |output: Option<f64>| {
        let timing = [uint(ids::CODEC_DELAY, 10_000), uint(ids::SEEK_PRE_ROLL, 10_000)].concat();
        let track = entry(1, ids::TRACK_TYPE_AUDIO, "A_PCM/INT/LIT", &[], &[audio(48_000.0, output), timing].concat());
        let blocks = [simple(0, &[0; 4]), padded(20, 0, &[0; 4], 10_000), simple(40, &[0; 4])];
        file(&[elem(ids::TRACKS, &track), cluster(0, &blocks), cluster(1000, &blocks)])
    };
    let mut d = open(pcm(Some(96_000.0)));
    let trims: Vec<Option<AudioTrim>> = drained(&mut d).into_iter().map(|(_, t)| t).collect();
    assert_eq!(trims, [trim(1, 0, 96_000), trim(0, 1, 96_000), None, None, trim(0, 1, 96_000), None]);
    assert_eq!(seek(&mut d, 1000).3, trim(1, 0, 96_000));
    assert!(drained(&mut open(pcm(None))).iter().all(|(_, t)| t.is_none()));
    let opus = entry(1, ids::TRACK_TYPE_AUDIO, "A_OPUS", &opus_head(), &[audio(44_100.0, Some(96_000.0)), opus_timing()].concat());
    let bytes = file(&[elem(ids::TRACKS, &opus), cluster(0, &[simple(0, &OPUS)])]);
    assert_eq!(drained(&mut open(bytes)), [(Some(-7), trim(312, 0, 48_000))]);
}
