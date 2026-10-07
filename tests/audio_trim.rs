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

/// Audio track 1 at `rate` Hz, stereo, with `codec`, its `private` data and
/// the `timing` children (`CodecDelay`, `SeekPreRoll`).
fn audio_track(codec: &str, rate: f64, private: &[u8], timing: &[u8]) -> Vec<u8> {
    let audio = elem(ids::AUDIO, &[
        elem(ids::SAMPLING_FREQUENCY, &rate.to_be_bytes()), uint(ids::CHANNELS, 2),
    ].concat());
    elem(ids::TRACKS, &elem(ids::TRACK_ENTRY, &[
        uint(ids::TRACK_NUMBER, 1), uint(ids::TRACK_UID, 1), uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO),
        elem(ids::CODEC_ID, codec.as_bytes()), elem(ids::CODEC_PRIVATE, private), audio, timing.to_vec(),
    ].concat()))
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
#[test]
fn trims_past_their_range_saturate() {
    let timing = [uint(ids::CODEC_DELAY, u64::MAX), uint(ids::SEEK_PRE_ROLL, u64::MAX)].concat();
    let blocks = [simple(0, &OPUS), padded(20, 0, &OPUS, i64::MAX), padded(40, 0, &OPUS, i64::MIN), simple(60, &OPUS)];
    let bytes = file(&[audio_track("A_OPUS", 48_000.0, &opus_head(), &timing), cluster(0, &blocks), cluster(1000, &blocks)]);
    let mut d = open(bytes);
    let trims: Vec<Option<AudioTrim>> = drained(&mut d).into_iter().map(|(_, t)| t).collect();
    let max = u32::MAX;
    assert_eq!(trims, [
        trim(max, 0, 48_000), trim(0, max, 48_000), trim(max, 0, 48_000), None,
        None, trim(0, max, 48_000), trim(max, 0, 48_000), None,
    ]);
    assert_eq!(seek(&mut d, 1000).3, trim(max, 0, 48_000));
}
