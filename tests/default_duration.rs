//! A Block without a `BlockDuration` lasts its track's `DefaultDuration`
//! per frame, as FFmpeg 2da55bf's matroska demuxer computes it
//! (`libavformat/matroskadec.c` 4383–4386): the whole Block's duration is
//! `av_rescale_q(default_duration * laces, 1/1000000000, st->time_base)`,
//! rounded to the nearest tick of the stream's time base (which includes
//! the `TrackTimestampScale`), and each lace takes its share
//! (`block_duration * (n + 1) / laces - block_duration * n / laces`,
//! 4392–4393), its timestamp following the lace before it.

use std::io::Cursor;

use oxideav_core::{Demuxer, NullCodecResolver};
use oxideav_mkv::demux;
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

/// A file of subtitle track 1 with `timing` children, then one Cluster at
/// 0 holding `block`.
fn file(timing: &[u8], block: Vec<u8>) -> Vec<u8> {
    let track = elem(ids::TRACK_ENTRY, &[
        uint(ids::TRACK_NUMBER, 1), uint(ids::TRACK_UID, 1), uint(ids::TRACK_TYPE, ids::TRACK_TYPE_SUBTITLE),
        elem(ids::CODEC_ID, b"S_TEXT/UTF8"), timing.to_vec(),
    ].concat());
    let segment = [elem(ids::TRACKS, &track), elem(ids::CLUSTER, &[uint(ids::TIMECODE, 0), block].concat())].concat();
    let mut out = elem(ids::EBML_HEADER, &elem(ids::EBML_DOC_TYPE, b"matroska"));
    out.extend(write_element_id(ids::SEGMENT));
    out.extend(write_vint(segment.len() as u64, 0));
    out.extend(segment);
    out
}

/// A keyframe SimpleBlock on track 1 at 0, laced as `flags` says.
fn simple(flags: u8, body: &[u8]) -> Vec<u8> {
    elem(ids::SIMPLE_BLOCK, &[&[0x81, 0, 0, 0x80 | flags][..], body].concat())
}

/// Each packet's pts and duration.
fn timing(bytes: Vec<u8>) -> Vec<(Option<i64>, Option<i64>)> {
    let mut d = demux::open_typed(Box::new(Cursor::new(bytes)), &NullCodecResolver).unwrap();
    std::iter::from_fn(|| d.next_packet().ok().map(|p| (p.pts, p.duration))).collect()
}

/// 15 frames a second: 66,666,667 ns.
const FIFTEENTH: u64 = 66_666_667;

#[test]
fn a_default_duration_rounds_to_the_nearest_tick() {
    let fifteenth = uint(ids::DEFAULT_DURATION, FIFTEENTH);
    // One frame: 66.666667 ms is 67 ticks of 1 ms, not 66.
    assert_eq!(timing(file(&fifteenth, simple(0, b"a"))), [(Some(0), Some(67))]);
    // Four Xiph-laced frames: 266.666668 ms is 267 ticks, shared 66, 67,
    // 67, 67, so the last starts at 200, not 199.
    let laced = [&[3u8, 1, 1, 1][..], b"abcd"].concat();
    assert_eq!(
        timing(file(&fifteenth, simple(0x02, &laced))),
        [(Some(0), Some(66)), (Some(66), Some(67)), (Some(133), Some(67)), (Some(200), Some(67))],
    );
}

#[test]
fn a_default_duration_counts_in_ticks_of_the_tracks_time_base() {
    // A TrackTimestampScale of 2 makes the track's tick 2 ms: 40 ms is 20
    // ticks, and 66.666667 ms rounds to 33.
    let scale = elem(ids::TRACK_TIMESTAMP_SCALE, &2f64.to_be_bytes());
    let forty = [uint(ids::DEFAULT_DURATION, 40_000_000), scale.clone()].concat();
    assert_eq!(timing(file(&forty, simple(0, b"a"))), [(Some(0), Some(20))]);
    let fifteenth = [uint(ids::DEFAULT_DURATION, FIFTEENTH), scale].concat();
    assert_eq!(timing(file(&fifteenth, simple(0, b"a"))), [(Some(0), Some(33))]);
}
