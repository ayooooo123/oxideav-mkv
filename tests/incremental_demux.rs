//! Incremental demuxing: the demuxer reads what the next packet needs, so a
//! file arriving over a network plays after its first kilobytes.
//!
//! * A Cluster's `CRC-32` (RFC 8794 §11.3.1) is computed as the walk reads
//!   the Cluster: the first packet comes out before the rest of the Cluster
//!   is read, and the status still lands once the walk has read it all —
//!   including bytes it skips (a `Void`) and the prefix a Cue-driven seek
//!   jumps over.
//! * The open reads the EBML header and the masters before the first
//!   Cluster, and never walks the Cluster run looking for Cues.
//! * A Cues-less seek (RFC 9559 §22.1 only RECOMMENDS Cues) finds the last
//!   keyframe at or before the target in the Clusters themselves, landing
//!   inside a Cluster that doesn't start with one.

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::MkvDemuxer;
use oxideav_mkv::ebml::{crc32_ieee, write_element_id, write_vint};
use oxideav_mkv::ids;

fn elem_uint(id: u32, value: u64) -> Vec<u8> {
    let n = if value == 0 {
        1
    } else {
        (64 - value.leading_zeros()).div_ceil(8) as usize
    };
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(n as u64, 0));
    for i in (0..n).rev() {
        out.push(((value >> (i * 8)) & 0xFF) as u8);
    }
    out
}

/// A uint element encoded on 8 bytes, so its length doesn't depend on the
/// value (for offsets patched after the layout is known).
fn elem_uint8(id: u32, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(8, 0));
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn elem_str(id: u32, s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(s.len() as u64, 0));
    out.extend_from_slice(s.as_bytes());
    out
}

fn elem_float_be_f64(id: u32, value: f64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(8, 0));
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

/// A Cluster whose first child is a `CRC-32` over `body`; `corrupt` flips a
/// bit of the stored value.
fn cluster_with_crc(body: &[u8], corrupt: bool) -> Vec<u8> {
    let mut crc = crc32_ieee(body).to_le_bytes();
    if corrupt {
        crc[3] ^= 0x01;
    }
    let mut full = Vec::new();
    full.extend_from_slice(&write_element_id(ids::CRC32));
    full.extend_from_slice(&write_vint(4, 0));
    full.extend_from_slice(&crc);
    full.extend_from_slice(body);
    elem_master(ids::CLUSTER, &full)
}

fn simple_block(tc_offset: i16, keyframe: bool, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(1, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.extend_from_slice(payload);
    elem_master(ids::SIMPLE_BLOCK, &body)
}

fn void(len: usize) -> Vec<u8> {
    elem_master(ids::VOID, &vec![0u8; len])
}

fn ebml_header() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    b.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    elem_master(ids::EBML_HEADER, &b)
}

/// Info + Tracks (one PCM track, TrackNumber 1, 1 ms ticks).
fn head() -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info.extend_from_slice(&elem_float_be_f64(ids::DURATION, 10_000.0));
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 0xA1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio = Vec::new();
    audio.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    tb.extend_from_slice(&elem_master(ids::AUDIO, &audio));
    let mut out = elem_master(ids::INFO, &info);
    out.extend_from_slice(&elem_master(
        ids::TRACKS,
        &elem_master(ids::TRACK_ENTRY, &tb),
    ));
    out
}

fn file(segment_body: &[u8]) -> Vec<u8> {
    let mut out = ebml_header();
    out.extend_from_slice(&elem_master(ids::SEGMENT, segment_body));
    out
}

/// A reader that records the furthest byte any read reached.
struct Tracked {
    inner: Cursor<Vec<u8>>,
    furthest: Arc<AtomicU64>,
}

impl Read for Tracked {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.furthest
            .fetch_max(self.inner.position(), Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for Tracked {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(to)
    }
}

fn open_tracked(bytes: Vec<u8>) -> (MkvDemuxer, Arc<AtomicU64>) {
    let furthest = Arc::new(AtomicU64::new(0));
    let rs: Box<dyn ReadSeek> = Box::new(Tracked {
        inner: Cursor::new(bytes),
        furthest: furthest.clone(),
    });
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    (dmx, furthest)
}

fn cluster_statuses(dmx: &MkvDemuxer) -> Vec<oxideav_mkv::demux::CrcStatus> {
    dmx.crc_status()
        .iter()
        .filter(|s| s.element_id == ids::CLUSTER)
        .copied()
        .collect()
}

const BLOCK: usize = 16 * 1024;
const BLOCKS: usize = 64;

/// One 1 MiB Cluster (64 blocks of 16 KiB) carrying a leading CRC-32.
fn big_crc_cluster_file(corrupt: bool) -> (Vec<u8>, u64) {
    let mut body = elem_uint(ids::TIMECODE, 0);
    for i in 0..BLOCKS {
        body.extend_from_slice(&simple_block(i as i16 * 10, true, &[i as u8; BLOCK]));
    }
    let head = head();
    let mut seg = head.clone();
    seg.extend_from_slice(&cluster_with_crc(&body, corrupt));
    let bytes = file(&seg);
    let cluster_at = (bytes.len() - seg.len() + head.len()) as u64;
    (bytes, cluster_at)
}

#[test]
fn crc_cluster_streams_its_packets() {
    for corrupt in [false, true] {
        let (bytes, cluster_at) = big_crc_cluster_file(corrupt);
        let (mut dmx, furthest) = open_tracked(bytes);
        let first = dmx.next_packet().expect("first packet");
        assert_eq!(first.data, vec![0u8; BLOCK]);
        let read = furthest.load(Ordering::Relaxed);
        assert!(
            read < cluster_at + 2 * BLOCK as u64,
            "first packet of a CRC'd 1 MiB Cluster read up to byte {read} (Cluster at {cluster_at})"
        );
        assert!(
            cluster_statuses(&dmx).is_empty(),
            "the CRC is only known once the Cluster has been read"
        );
        let mut n = 1;
        while dmx.next_packet().is_ok() {
            n += 1;
        }
        assert_eq!(n, BLOCKS);
        let statuses = cluster_statuses(&dmx);
        assert_eq!(statuses.len(), 1, "{statuses:?}");
        assert_eq!(statuses[0].is_valid(), !corrupt, "{:?}", statuses[0]);
    }
}

#[test]
fn crc_covers_skipped_elements() {
    let mut body = elem_uint(ids::TIMECODE, 0);
    body.extend_from_slice(&simple_block(0, true, &[0x11; 32]));
    body.extend_from_slice(&void(300));
    body.extend_from_slice(&simple_block(10, true, &[0x22; 32]));
    let mut seg = head();
    seg.extend_from_slice(&cluster_with_crc(&body, false));
    let (mut dmx, _) = open_tracked(file(&seg));
    let mut n = 0;
    while dmx.next_packet().is_ok() {
        n += 1;
    }
    assert_eq!(n, 2);
    let statuses = cluster_statuses(&dmx);
    assert_eq!(statuses.len(), 1);
    assert!(statuses[0].is_valid(), "{:?}", statuses[0]);
}

#[test]
fn crc_covers_the_prefix_a_cue_seek_jumps_over() {
    // Cues (before the Cluster) point at the Cluster's third Block through
    // CueRelativePosition; the seek lands there, and the CRC still covers
    // the Blocks it jumped over.
    let timestamp = elem_uint(ids::TIMECODE, 0);
    let blocks = [
        simple_block(0, true, &[0x11; 64]),
        simple_block(10, true, &[0x22; 64]),
        simple_block(20, true, &[0x33; 64]),
    ];
    let mut body = timestamp.clone();
    for b in &blocks {
        body.extend_from_slice(b);
    }
    let cluster = cluster_with_crc(&body, false);
    // Relative to the Cluster body: the CRC-32 element, the Timestamp and
    // two Blocks come first.
    let relative = (6 + timestamp.len() + blocks[0].len() + blocks[1].len()) as u64;
    let cues = |cluster_position: u64| {
        let mut ctp = elem_uint(ids::CUE_TRACK, 1);
        ctp.extend_from_slice(&elem_uint8(ids::CUE_CLUSTER_POSITION, cluster_position));
        ctp.extend_from_slice(&elem_uint8(ids::CUE_RELATIVE_POSITION, relative));
        let mut cp = elem_uint(ids::CUE_TIME, 20);
        cp.extend_from_slice(&elem_master(ids::CUE_TRACK_POSITIONS, &ctp));
        elem_master(ids::CUES, &elem_master(ids::CUE_POINT, &cp))
    };
    let head = head();
    let cluster_position = (head.len() + cues(0).len()) as u64;
    let mut seg = head;
    seg.extend_from_slice(&cues(cluster_position));
    seg.extend_from_slice(&cluster);

    let (mut dmx, _) = open_tracked(file(&seg));
    assert_eq!(dmx.seek_to(0, 25).expect("seek"), 20);
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(pkt.data, vec![0x33; 64]);
    assert!(dmx.next_packet().is_err());
    let statuses = cluster_statuses(&dmx);
    assert_eq!(statuses.len(), 1);
    assert!(statuses[0].is_valid(), "{:?}", statuses[0]);
}

#[test]
fn open_never_walks_the_cluster_run() {
    // Twenty 50 KB Clusters and no Cues: the open stops at the first
    // Cluster header instead of walking to the Segment end.
    let head = head();
    let mut seg = head.clone();
    for c in 0..20u64 {
        let mut body = elem_uint(ids::TIMECODE, c * 1000);
        body.extend_from_slice(&simple_block(0, true, &[c as u8; 50_000]));
        seg.extend_from_slice(&elem_master(ids::CLUSTER, &body));
    }
    let bytes = file(&seg);
    let cluster_at = (bytes.len() - seg.len() + head.len()) as u64;
    let (mut dmx, furthest) = open_tracked(bytes);
    let read = furthest.load(Ordering::Relaxed);
    assert!(
        read <= cluster_at + 16,
        "open read up to byte {read}; the first Cluster is at {cluster_at}"
    );
    assert!(dmx.cue_points().is_empty());
    let pkt = dmx.next_packet().expect("first packet");
    assert_eq!(pkt.data, vec![0u8; 50_000]);
}

/// Two Clusters: A (Timestamp 0) starts with a keyframe; B (Timestamp 500)
/// starts mid-GOP, with its keyframe at 700.
fn mid_gop_file() -> Vec<u8> {
    let cluster = |tc: u64, blocks: &[(i16, bool)]| {
        let mut body = elem_uint(ids::TIMECODE, tc);
        for &(offset, key) in blocks {
            body.extend_from_slice(&simple_block(offset, key, &[0]));
        }
        elem_master(ids::CLUSTER, &body)
    };
    let mut seg = head();
    seg.extend_from_slice(&cluster(
        0,
        &[
            (0, true),
            (100, false),
            (200, false),
            (300, true),
            (400, false),
        ],
    ));
    seg.extend_from_slice(&cluster(
        500,
        &[(0, false), (100, false), (200, true), (300, false)],
    ));
    file(&seg)
}

#[test]
fn cues_less_seek_lands_on_a_keyframe_inside_a_cluster() {
    let (mut dmx, _) = open_tracked(mid_gop_file());
    // Cluster B starts on a non-keyframe: the seek lands on its keyframe.
    assert_eq!(dmx.seek_to(0, 750).expect("seek"), 700);
    let pts: Vec<_> = std::iter::from_fn(|| dmx.next_packet().ok())
        .map(|p| p.pts)
        .collect();
    assert_eq!(pts, vec![Some(700), Some(800)]);
}

#[test]
fn cues_less_seek_lands_on_a_cluster_that_decodes_from_its_start() {
    let (mut dmx, _) = open_tracked(mid_gop_file());
    // The last keyframe at or before 650 is the one at 300, in Cluster A,
    // which starts with a keyframe: the seek lands on the Cluster's start.
    assert_eq!(dmx.seek_to(0, 650).expect("seek"), 0);
    let first = dmx.next_packet().expect("packet");
    assert_eq!(first.pts, Some(0));
    // Before the first keyframe lands on the first Cluster too.
    assert_eq!(dmx.seek_to(0, -5).expect("seek"), 0);
    assert_eq!(dmx.next_packet().expect("packet").pts, Some(0));
}
