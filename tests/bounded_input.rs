//! Untrusted P2P input: index chasing, retained Block/CodecPrivate output,
//! startup timestamp analysis, error classification and Cluster bounds.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxideav_core::{Demuxer, Error, NullCodecResolver, ReadSeek};
use oxideav_mkv::demux::{self, MkvDemuxer};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

struct Tracking;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

fn elem(id: u32, body: &[u8]) -> Vec<u8> {
    [write_element_id(id), write_vint(body.len() as u64, 0), body.to_vec()].concat()
}

fn uint(id: u32, v: u64) -> Vec<u8> {
    elem(id, &v.to_be_bytes())
}

fn track(number: u64, kind: u64, codec: &str, extra: &[Vec<u8>]) -> Vec<u8> {
    let mut body = [
        uint(ids::TRACK_NUMBER, number), uint(ids::TRACK_UID, number),
        uint(ids::TRACK_TYPE, kind), elem(ids::CODEC_ID, codec.as_bytes()),
    ].concat();
    for e in extra {
        body.extend_from_slice(e);
    }
    elem(ids::TRACK_ENTRY, &body)
}

fn block(track: u8, tc: i16, flags: u8, payload: &[u8]) -> Vec<u8> {
    [&[0x80 | track][..], &tc.to_be_bytes(), &[flags], payload].concat()
}

fn simple(track: u8, payload: &[u8]) -> Vec<u8> {
    elem(ids::SIMPLE_BLOCK, &block(track, 0, 0x80, payload))
}

fn cluster(tc: u64, children: &[Vec<u8>]) -> Vec<u8> {
    elem(ids::CLUSTER, &[uint(ids::TIMECODE, tc), children.concat()].concat())
}

fn file(segment: &[Vec<u8>]) -> Vec<u8> {
    [elem(ids::EBML_HEADER, &elem(ids::EBML_DOC_TYPE, b"matroska")), elem(ids::SEGMENT, &segment.concat())].concat()
}

fn subtitle_tracks() -> Vec<u8> {
    elem(ids::TRACKS, &track(1, 0x11, "S_TEXT/UTF8", &[]))
}

fn nth_cluster(bytes: &[u8], n: usize) -> usize {
    let id = write_element_id(ids::CLUSTER);
    bytes.windows(4).enumerate().filter(|(_, w)| *w == id.as_slice()).nth(n).unwrap().0
}

fn open(bytes: Vec<u8>) -> oxideav_core::Result<MkvDemuxer> {
    demux::open_typed(Box::new(Cursor::new(bytes)), &NullCodecResolver)
}

fn drain(d: &mut MkvDemuxer) -> Vec<(Option<i64>, Vec<u8>)> {
    let mut out = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => out.push((p.pts, p.data)),
            Err(Error::Eof) => return out,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}

#[derive(Clone, Default)]
struct Counters {
    bytes: Arc<AtomicUsize>,
    jumps: Arc<AtomicUsize>,
}

/// Counts bytes read and position-changing seeks; optionally fails every
/// read at or after an offset with a transport error.
struct Source {
    inner: Cursor<Vec<u8>>,
    counters: Counters,
    fail_from: Option<(u64, io::ErrorKind)>,
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let pos = self.inner.position();
        let mut len = buf.len();
        if let Some((at, kind)) = self.fail_from {
            if pos >= at {
                return Err(io::Error::new(kind, "injected transport failure"));
            }
            len = len.min((at - pos) as usize);
        }
        let n = self.inner.read(&mut buf[..len])?;
        self.counters.bytes.fetch_add(n, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for Source {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let before = self.inner.position();
        let after = self.inner.seek(from)?;
        if after != before {
            self.counters.jumps.fetch_add(1, Ordering::Relaxed);
        }
        Ok(after)
    }
}

fn source(bytes: Vec<u8>, fail_from: Option<(u64, io::ErrorKind)>) -> (Box<dyn ReadSeek>, Counters) {
    let counters = Counters::default();
    (Box::new(Source { inner: Cursor::new(bytes), counters: counters.clone(), fail_from }), counters)
}

#[test]
fn seek_heads_follow_one_index_and_one_target_per_master() {
    const TARGETS: usize = 1000;
    let seek = |id: u32, pos: u64| {
        elem(ids::SEEK, &[elem(ids::SEEK_ID, &write_element_id(id)), uint(ids::SEEK_POSITION, pos)].concat())
    };
    let entry_len = seek(ids::TAGS, 0).len();
    let tracks = subtitle_tracks();
    let first = cluster(0, &[simple(1, b"cue")]);
    let sh1_len = elem(ids::SEEK_HEAD, &seek(ids::SEEK_HEAD, 0)).len();
    let sh2_pos = (sh1_len + tracks.len() + first.len()) as u64;
    let sh2_len = elem(ids::SEEK_HEAD, &vec![0; (TARGETS * 3 + 2) * entry_len]).len() as u64;
    let tags = elem(ids::TAGS, &[]);
    let tags_pos = sh2_pos + sh2_len;
    let sh3_pos = tags_pos + (TARGETS * tags.len()) as u64;
    let mut sh2 = Vec::new();
    for i in 0..TARGETS {
        sh2.extend(seek(ids::TAGS, tags_pos + (i * tags.len()) as u64));
    }
    for _ in 0..TARGETS * 2 {
        sh2.extend(seek(ids::TAGS, tags_pos));
    }
    sh2.extend(seek(ids::SEEK_HEAD, sh2_pos));
    sh2.extend(seek(ids::SEEK_HEAD, sh3_pos));
    let sh2 = elem(ids::SEEK_HEAD, &sh2);
    assert_eq!(sh2.len() as u64, sh2_len);
    // A third index overlapping every empty Tags target again.
    let sh3: Vec<u8> = (0..TARGETS).flat_map(|i| seek(ids::TAGS, tags_pos + (i * tags.len()) as u64 + 1)).collect();
    let sh1 = elem(ids::SEEK_HEAD, &seek(ids::SEEK_HEAD, sh2_pos));
    assert_eq!(sh1.len(), sh1_len);
    let bytes = file(&[sh1, tracks, first, sh2, tags.repeat(TARGETS), elem(ids::SEEK_HEAD, &sh3)]);
    let (input, counters) = source(bytes, None);
    let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
    let jumps = counters.jumps.load(Ordering::Relaxed);
    assert!(jumps < 32, "open made {jumps} seeks");
    assert!(d.seek_entries().len() <= TARGETS + 3, "{} retained SeekHead entries", d.seek_entries().len());
    assert_eq!(drain(&mut d), [(Some(0), b"cue".to_vec())]);
}

fn encoded_tracks(scope: u64, settings: &[u8], private: &[u8]) -> Vec<u8> {
    let compression = elem(ids::CONTENT_COMPRESSION, &[
        uint(ids::CONTENT_COMP_ALGO, 3), elem(ids::CONTENT_COMP_SETTINGS, settings),
    ].concat());
    let encoding = elem(ids::CONTENT_ENCODING, &[uint(ids::CONTENT_ENCODING_SCOPE, scope), compression].concat());
    let mut extra = vec![elem(ids::CONTENT_ENCODINGS, &encoding)];
    if !private.is_empty() {
        extra.push(elem(ids::CODEC_PRIVATE, private));
    }
    elem(ids::TRACKS, &track(1, 0x11, "S_TEXT/UTF8", &extra))
}

#[test]
fn header_stripped_laces_share_one_block_budget() {
    let tracks = encoded_tracks(1, &vec![7; 1 << 20], &[]);
    // 256 one-byte frames of a fixed-size lace each regain a 1 MiB header.
    let laced = elem(ids::SIMPLE_BLOCK, &block(1, 0, 0x84, &[&[255u8][..], &[1; 256]].concat()));
    let bytes = file(&[tracks, cluster(0, &[laced]), cluster(1000, &[simple(1, b"z")])]);
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let mut d = open(bytes).unwrap();
    let packets = drain(&mut d);
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    assert!(peak < 96 << 20, "peak heap {peak} bytes");
    // The whole over-budget Block is rejected; the next Cluster still plays.
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].0, Some(1000));
    assert_eq!(packets[0].1.len(), (1 << 20) + 1);
}

#[test]
fn codec_private_expanding_past_its_budget_is_invalid_data() {
    let tracks = encoded_tracks(2, &vec![1; 3 << 20], &vec![2; 3 << 19]);
    let bytes = file(&[tracks, cluster(0, &[simple(1, b"x")])]);
    assert!(matches!(open(bytes), Err(Error::InvalidData(_))));
}

#[test]
fn more_than_256_tracks_is_invalid_data() {
    let entries: Vec<u8> = (1..=257).flat_map(|n| track(n, 0x11, "S_TEXT/UTF8", &[])).collect();
    let bytes = file(&[elem(ids::TRACKS, &entries), cluster(0, &[simple(1, b"x")])]);
    assert!(matches!(open(bytes), Err(Error::InvalidData(_))));
}

#[test]
fn unsatisfied_avc_probe_returns_first_packet_after_bounded_queue() {
    let sps = [0x67, 0x42, 0x00, 0x1e, 0xf4, 0xf2];
    let pps = [0x68, 0xce, 0x38, 0x80];
    let avcc = [&[1, 0x42, 0x00, 0x1e, 0xff, 0xe1, 0, sps.len() as u8][..], &sps, &[1, 0, pps.len() as u8], &pps].concat();
    let video = elem(ids::VIDEO, &[uint(ids::PIXEL_WIDTH, 16), uint(ids::PIXEL_HEIGHT, 16)].concat());
    let tracks = elem(ids::TRACKS, &[
        track(1, 1, "V_MPEG4/ISO/AVC", &[elem(ids::CODEC_PRIVATE, &avcc), video]),
        track(2, 0x11, "S_TEXT/UTF8", &[]),
    ].concat());
    let addition = elem(ids::BLOCK_ADDITIONS, &elem(ids::BLOCK_MORE, &[
        uint(ids::BLOCK_ADD_ID, 1), elem(ids::BLOCK_ADDITIONAL, b"x"),
    ].concat()));
    // Zero-length subtitle frames kept alive by BlockAdditions; the AVC
    // track never delivers the frames its reorder analysis waits for.
    let group = elem(ids::BLOCK_GROUP, &[elem(ids::BLOCK, &block(2, 0, 0, &[])), addition].concat());
    let bytes = file(&[tracks, cluster(0, &[group.repeat(4000)])]);
    let len = bytes.len();
    let (input, counters) = source(bytes, None);
    let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
    let first = d.next_packet().unwrap();
    assert_eq!(first.stream_index, 1);
    assert!(first.data.is_empty());
    assert!(!d.block_additions().is_empty());
    let read = counters.bytes.load(Ordering::Relaxed);
    assert!(read < len / 2, "first packet after reading {read} of {len} bytes");
}

fn expect_transport_error(result: oxideav_core::Result<oxideav_core::Packet>) {
    match result {
        Err(Error::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::TimedOut),
        Err(e) => panic!("transport failure became {e}"),
        Ok(p) => panic!("transport failure produced a packet at {:?}", p.pts),
    }
}

#[test]
fn transport_errors_are_returned_instead_of_ending_the_stream() {
    let bytes = file(&[subtitle_tracks(), cluster(0, &[simple(1, b"a")]), cluster(1000, &[simple(1, b"b")])]);
    let at = nth_cluster(&bytes, 1) as u64 + 2;
    let (input, _) = source(bytes, Some((at, io::ErrorKind::TimedOut)));
    let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
    assert_eq!(d.next_packet().unwrap().data, b"a");
    expect_transport_error(d.next_packet());
}

#[test]
fn transport_errors_during_resync_are_returned() {
    let mut bytes = file(&[
        subtitle_tracks(), cluster(0, &[simple(1, b"a")]),
        cluster(1000, &[simple(1, b"b")]), cluster(2000, &[simple(1, b"c")]),
    ]);
    let damaged = nth_cluster(&bytes, 1);
    bytes[damaged..damaged + 4].fill(0);
    let at = nth_cluster(&bytes, 1) as u64;
    let (input, _) = source(bytes, Some((at, io::ErrorKind::TimedOut)));
    let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
    assert_eq!(d.next_packet().unwrap().data, b"a");
    expect_transport_error(d.next_packet());
}

#[test]
fn seeking_before_damage_recovers_the_same_cluster_every_time() {
    let mut bytes = file(&[
        subtitle_tracks(), cluster(0, &[simple(1, b"a")]), cluster(1000, &[simple(1, b"b")]),
        cluster(2000, &[simple(1, b"c")]), cluster(3000, &[simple(1, b"d")]),
    ]);
    let damaged = nth_cluster(&bytes, 1);
    bytes[damaged..damaged + 4].fill(0);
    let mut d = open(bytes).unwrap();
    let expected = vec![(Some(0), b"a".to_vec()), (Some(2000), b"c".to_vec()), (Some(3000), b"d".to_vec())];
    assert_eq!(drain(&mut d), expected);
    for _ in 0..3 {
        d.seek_to(0, 0).unwrap();
        assert_eq!(drain(&mut d), expected);
    }
}

#[test]
fn forged_signature_slot_cannot_skip_later_clusters() {
    let forged = [write_element_id(ids::SIGNATURE_SLOT), write_vint(1 << 30, 0), b"zz".to_vec()].concat();
    let bytes = file(&[
        subtitle_tracks(), cluster(0, &[simple(1, b"a"), forged]), cluster(1000, &[simple(1, b"b")]),
    ]);
    let mut d = open(bytes).unwrap();
    assert_eq!(drain(&mut d), [(Some(0), b"a".to_vec()), (Some(1000), b"b".to_vec())]);
}
