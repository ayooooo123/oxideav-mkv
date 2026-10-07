//! Untrusted P2P input: index chasing, retained Block/CodecPrivate output,
//! startup timestamp analysis, error classification, lacing, Cluster
//! bounds and recovery.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxideav_core::{Demuxer, Error, NullCodecResolver, PacketMetadata, ReadSeek};
use oxideav_mkv::demux::{self, MkvDemuxer};
use oxideav_mkv::ebml::{crc32_ieee, write_element_id, write_vint};
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
/// read that starts in a byte range with a transport error, and optionally
/// rejects a seek past the end with `InvalidInput`, as an HTTP range
/// source does.
struct Source {
    inner: Cursor<Vec<u8>>,
    counters: Counters,
    fail: Option<(Range<u64>, io::ErrorKind)>,
    reject_past_end: bool,
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let pos = self.inner.position();
        let mut len = buf.len();
        if let Some((range, kind)) = &self.fail {
            if range.contains(&pos) {
                return Err(io::Error::new(*kind, "injected transport failure"));
            }
            if pos < range.start {
                len = len.min((range.start - pos) as usize);
            }
        }
        let n = self.inner.read(&mut buf[..len])?;
        self.counters.bytes.fetch_add(n, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for Source {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let before = self.inner.position();
        if self.reject_past_end {
            let end = self.inner.get_ref().len() as u64;
            let target = match from {
                SeekFrom::Start(n) => Some(n),
                SeekFrom::End(d) => end.checked_add_signed(d),
                SeekFrom::Current(d) => before.checked_add_signed(d),
            };
            if target.map_or(true, |t| t > end) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek past end"));
            }
        }
        let after = self.inner.seek(from)?;
        if after != before {
            self.counters.jumps.fetch_add(1, Ordering::Relaxed);
        }
        Ok(after)
    }
}

fn source(bytes: Vec<u8>, fail_from: Option<(u64, io::ErrorKind)>) -> (Box<dyn ReadSeek>, Counters) {
    let counters = Counters::default();
    let fail = fail_from.map(|(at, kind)| (at..u64::MAX, kind));
    let source = Source { inner: Cursor::new(bytes), counters: counters.clone(), fail, reject_past_end: false };
    (Box::new(source), counters)
}

/// Like [`source`], failing only the reads that start inside `range`: a
/// span the transport cannot deliver, while the rest of the file can.
fn failing(bytes: Vec<u8>, range: Range<u64>, kind: io::ErrorKind) -> Box<dyn ReadSeek> {
    let fail = Some((range, kind));
    Box::new(Source { inner: Cursor::new(bytes), counters: Counters::default(), fail, reject_past_end: false })
}

/// Like [`source`], over a transport that rejects seeks past its end.
fn http_source(bytes: Vec<u8>, fail_from: Option<(u64, io::ErrorKind)>) -> Box<dyn ReadSeek> {
    let fail = fail_from.map(|(at, kind)| (at..u64::MAX, kind));
    Box::new(Source { inner: Cursor::new(bytes), counters: Counters::default(), fail, reject_past_end: true })
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

/// An AVC track 1 whose startup reorder analysis never completes: it
/// carries no frames, so the other tracks' packets are held for it.
fn avc_track() -> Vec<u8> {
    let sps = [0x67, 0x42, 0x00, 0x1e, 0xf4, 0xf2];
    let pps = [0x68, 0xce, 0x38, 0x80];
    let avcc = [&[1, 0x42, 0x00, 0x1e, 0xff, 0xe1, 0, sps.len() as u8][..], &sps, &[1, 0, pps.len() as u8], &pps].concat();
    let video = elem(ids::VIDEO, &[uint(ids::PIXEL_WIDTH, 16), uint(ids::PIXEL_HEIGHT, 16)].concat());
    track(1, 1, "V_MPEG4/ISO/AVC", &[elem(ids::CODEC_PRIVATE, &avcc), video])
}

#[test]
fn unsatisfied_avc_probe_returns_first_packet_after_bounded_queue() {
    let tracks = elem(ids::TRACKS, &[avc_track(), track(2, 0x11, "S_TEXT/UTF8", &[])].concat());
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

fn expect_transport_error(result: oxideav_core::Result<oxideav_core::Packet>, kind: io::ErrorKind) {
    match result {
        Err(Error::Io(e)) => assert_eq!(e.kind(), kind),
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
    assert!(d.packet_metadata().container_keyframe);
    expect_transport_error(d.next_packet(), io::ErrorKind::TimedOut);
    // The failed read exposes nothing from the previous packet.
    assert_eq!(d.packet_metadata(), PacketMetadata::default());
}

/// A read error is the source's, whatever its kind: `InvalidInput` from an
/// ordinary read, or `UnexpectedEof` from an HTTP body that stayed short
/// after the transport's retries. Neither is a malformed file to resync
/// past or end as if complete.
#[test]
fn source_read_errors_are_returned_whatever_their_kind() {
    for kind in [io::ErrorKind::InvalidInput, io::ErrorKind::UnexpectedEof] {
        let bytes = file(&[
            subtitle_tracks(), cluster(0, &[simple(1, b"a")]),
            cluster(1000, &[simple(1, b"b")]), cluster(2000, &[simple(1, b"c")]),
        ]);
        let at = nth_cluster(&bytes, 1) as u64 + 2;
        let (input, _) = source(bytes, Some((at, kind)));
        let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
        assert_eq!(d.next_packet().unwrap().data, b"a");
        expect_transport_error(d.next_packet(), kind);
    }
}

fn seek_entry(id: u32, pos: u64) -> Vec<u8> {
    elem(ids::SEEK, &[elem(ids::SEEK_ID, &write_element_id(id)), uint(ids::SEEK_POSITION, pos)].concat())
}

/// SeekHead → Cues stored after three Clusters, as FFmpeg muxes.
fn indexed_file() -> Vec<u8> {
    let tracks = subtitle_tracks();
    let clusters = [
        cluster(0, &[simple(1, b"a")]), cluster(1000, &[simple(1, b"b")]), cluster(2000, &[simple(1, b"c")]),
    ];
    let seek_head_len = elem(ids::SEEK_HEAD, &seek_entry(ids::CUES, 0)).len();
    let first_cluster = (seek_head_len + tracks.len()) as u64;
    let cues_at = first_cluster + clusters.iter().map(Vec::len).sum::<usize>() as u64;
    let cues = elem(ids::CUES, &elem(ids::CUE_POINT, &[
        uint(ids::CUE_TIME, 0),
        elem(ids::CUE_TRACK_POSITIONS, &[uint(ids::CUE_TRACK, 1), uint(ids::CUE_CLUSTER_POSITION, first_cluster)].concat()),
    ].concat()));
    let seek_head = elem(ids::SEEK_HEAD, &seek_entry(ids::CUES, cues_at));
    file(&[seek_head, tracks, clusters.concat(), cues])
}

#[test]
fn open_returns_source_errors_while_following_the_seek_head() {
    let bytes = indexed_file();
    let cues_at = bytes.len() as u64 - 3;
    let (input, _) = source(bytes, Some((cues_at, io::ErrorKind::InvalidInput)));
    match demux::open_typed(input, &NullCodecResolver) {
        Err(Error::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::InvalidInput),
        Err(e) => panic!("source failure became {e}"),
        Ok(_) => panic!("source failure was ignored as a stale SeekHead entry"),
    }
}

/// A download cut inside its last Cluster keeps a SeekHead pointing past
/// the physical end, which an HTTP source refuses to seek to. That is
/// truncation: the open succeeds, the packets that exist play, then EOF.
#[test]
fn truncated_http_input_treats_seeks_past_its_end_as_truncation() {
    let mut bytes = indexed_file();
    bytes.truncate(nth_cluster(&bytes, 2) + 8);
    let mut d = demux::open_typed(http_source(bytes, None), &NullCodecResolver).unwrap();
    let expected = [(Some(0), b"a".to_vec()), (Some(1000), b"b".to_vec())];
    assert_eq!(drain(&mut d), expected);
    assert!(matches!(d.next_packet(), Err(Error::Eof)));
    d.seek_to(0, 0).unwrap();
    assert_eq!(drain(&mut d), expected);
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
    expect_transport_error(d.next_packet(), io::ErrorKind::TimedOut);
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

/// What became of a source failure: its kind, when it reached the caller
/// as itself.
fn surfaced<T: std::fmt::Debug>(result: oxideav_core::Result<T>) -> Result<io::ErrorKind, String> {
    match result {
        Err(Error::Io(e)) => Ok(e.kind()),
        Err(e) => Err(format!("became {e}")),
        Ok(v) => Err(format!("was hidden behind {v:?}")),
    }
}

/// A source that times out, and an HTTP body that stayed short after the
/// transport's retries: both the source's errors, not damage.
const SOURCE_ERRORS: [io::ErrorKind; 2] = [io::ErrorKind::TimedOut, io::ErrorKind::UnexpectedEof];

/// The Cluster walk reaches a `Tags` element stored after the last Cluster
/// (RFC 9559 §23.2). A body that never arrives is not the end of the file.
#[test]
fn source_errors_reading_trailing_tags_are_returned() {
    let tags = elem(ids::TAG, &elem(ids::SIMPLE_TAG, &[
        elem(ids::TAG_NAME, b"TITLE"), elem(ids::TAG_STRING, b"end"),
    ].concat()));
    let outcomes: Vec<_> = SOURCE_ERRORS.into_iter().map(|kind| {
        let bytes = file(&[subtitle_tracks(), cluster(0, &[simple(1, b"a")]), elem(ids::TAGS, &tags)]);
        let body = (bytes.len() - tags.len()) as u64;
        let (input, _) = source(bytes, Some((body, kind)));
        let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
        assert_eq!(d.next_packet().unwrap().data, b"a");
        surfaced(d.next_packet().map(|p| p.pts))
    }).collect();
    assert_eq!(outcomes, SOURCE_ERRORS.map(Ok));
}

/// The walk steps over a Cluster's `CRC-32` once it is being checked, so a
/// value that cannot be read is never read again: the error is the only
/// sign of it.
#[test]
fn source_errors_reading_a_cluster_crc_are_returned() {
    let outcomes: Vec<_> = SOURCE_ERRORS.into_iter().map(|kind| {
        let rest = [uint(ids::TIMECODE, 1000), simple(1, b"b")].concat();
        let checked = elem(ids::CLUSTER, &[elem(ids::CRC32, &crc32_ieee(&rest).to_le_bytes()), rest].concat());
        let bytes = file(&[subtitle_tracks(), cluster(0, &[simple(1, b"a")]), checked]);
        // After the Cluster's ID and size, and the CRC-32's own header.
        let value = nth_cluster(&bytes, 1) as u64 + 5 + 2;
        let mut d = demux::open_typed(failing(bytes, value..value + 4, kind), &NullCodecResolver).unwrap();
        assert_eq!(d.next_packet().unwrap().data, b"a");
        surfaced(d.next_packet().map(|p| p.pts))
    }).collect();
    assert_eq!(outcomes, SOURCE_ERRORS.map(Ok));
}

/// A Cues-less seek reads Block headers to find its keyframe. One it cannot
/// read does not make the seek land on an earlier Cluster instead.
#[test]
fn source_errors_during_a_cueless_seek_are_returned() {
    let outcomes: Vec<_> = SOURCE_ERRORS.into_iter().map(|kind| {
        let bytes = file(&[
            subtitle_tracks(), cluster(0, &[simple(1, b"a")]),
            cluster(1000, &[simple(1, b"b")]), cluster(2000, &[simple(1, b"c")]),
        ]);
        // The second Cluster's SimpleBlock body: after the Cluster's ID and
        // size, its 10-octet Timestamp and the SimpleBlock's own header.
        let block = nth_cluster(&bytes, 1) as u64 + 5 + 10 + 2;
        let mut d = demux::open_typed(failing(bytes, block..block + 1, kind), &NullCodecResolver).unwrap();
        surfaced(d.seek_to(0, 1500))
    }).collect();
    assert_eq!(outcomes, SOURCE_ERRORS.map(Ok));
}

/// Landing on a Cue: the resilient check of the Cue's Cluster, and the walk
/// to its `CueRelativePosition` (RFC 9559 §5.1.5.1.2.3), do not fall back to
/// another landing when the source fails.
#[test]
fn source_errors_landing_on_a_cue_are_returned() {
    let tracks = subtitle_tracks();
    let first = cluster(0, &[simple(1, b"a")]);
    // The Cue names "c": after the second Cluster's 10-octet Timestamp and
    // the 7-octet "b".
    let second = cluster(1000, &[simple(1, b"b"), simple(1, b"c")]);
    let cues = |position: u64| elem(ids::CUES, &elem(ids::CUE_POINT, &[
        uint(ids::CUE_TIME, 1000),
        elem(ids::CUE_TRACK_POSITIONS, &[
            uint(ids::CUE_TRACK, 1), uint(ids::CUE_CLUSTER_POSITION, position),
            uint(ids::CUE_RELATIVE_POSITION, 17),
        ].concat()),
    ].concat()));
    let position = (tracks.len() + cues(0).len() + first.len()) as u64;
    let bytes = file(&[tracks, cues(position), first, second]);
    let landing = nth_cluster(&bytes, 1) as u64;
    // "b"'s header, which the walk to "c" reads, whether resilient or not;
    // and the Cluster header, which a resilient seek first reads to check
    // the Cue.
    let b = landing + 5 + 10;
    let cases = [("walk", b, false), ("resilient walk", b, true), ("resilient check", landing, true)];
    let mut outcomes = Vec::new();
    let mut expected: Vec<(&str, Result<io::ErrorKind, String>)> = Vec::new();
    for kind in SOURCE_ERRORS {
        for (case, at, resilient) in cases {
            let input = failing(bytes.clone(), at..at + 1, kind);
            let mut d = if resilient {
                demux::open_resilient_typed(input, &NullCodecResolver)
            } else {
                demux::open_typed(input, &NullCodecResolver)
            }.unwrap();
            outcomes.push((case, surfaced(d.seek_to(0, 1000))));
            expected.push((case, Ok(kind)));
        }
    }
    assert_eq!(outcomes, expected);
}

/// An EBML lace whose head counts one frame (RFC 9559 §10.3: lacing never
/// stores a single frame) but whose first size would split it into two.
fn one_frame_ebml_lace() -> Vec<u8> {
    elem(ids::SIMPLE_BLOCK, &block(1, 0, 0x86, &[0x00, 0x81, 0x61, 0x62]))
}

#[test]
fn a_one_frame_ebml_lace_is_rejected_rather_than_split() {
    let bytes = file(&[subtitle_tracks(), cluster(0, &[one_frame_ebml_lace()]), cluster(1000, &[simple(1, b"z")])]);
    let mut d = open(bytes).unwrap();
    assert_eq!(drain(&mut d), [(Some(1000), b"z".to_vec())]);
}

/// 1023 joins of one track copy each of its frames 1023 times: one Block
/// may then hold one frame, its 1024 packets exactly filling the cap.
#[test]
fn virtual_copies_of_a_one_frame_lace_stay_within_the_cap() {
    let joins: Vec<u8> = (0..1023).flat_map(|_| uint(ids::TRACK_JOIN_UID, 1)).collect();
    let tracks = elem(ids::TRACKS, &[
        track(1, 0x11, "S_TEXT/UTF8", &[]),
        track(2, 0x11, "S_TEXT/UTF8", &[elem(ids::TRACK_OPERATION, &elem(ids::TRACK_JOIN_BLOCKS, &joins))]),
    ].concat());
    let bytes = file(&[tracks, cluster(0, &[one_frame_ebml_lace()]), cluster(1000, &[simple(1, b"z")])]);
    let mut d = open(bytes).unwrap();
    d.set_apply_track_operations(true);
    let packets = drain(&mut d);
    assert_eq!(packets.len(), 1024);
    assert!(packets.iter().all(|p| *p == (Some(1000), b"z".to_vec())));
}

/// A Block that waited for queue room and then fails is recovered from where
/// it was stored, not from wherever the walk had read on to: here the very
/// next byte, which starts a Cluster.
#[test]
fn a_failing_deferred_block_recovers_the_cluster_right_after_it() {
    let tracks = elem(ids::TRACKS, &[
        avc_track(), track(2, 0x11, "S_TEXT/UTF8", &[]), track(3, 0x11, "D_WEBVTT/SUBTITLES", &[]),
    ].concat());
    // 1023 held packets, then two Xiph-laced frames that cannot both join
    // them. Their lace is sound; neither frame is a WebVTT cue.
    let mut first: Vec<Vec<u8>> = (0..1023u16)
        .map(|i| elem(ids::SIMPLE_BLOCK, &block(2, i as i16, 0x80, &i.to_be_bytes())))
        .collect();
    first.push(elem(ids::SIMPLE_BLOCK, &block(3, 0, 0x82, &[&[1, 3][..], b"bad", b"x"].concat())));
    let next = cluster(5000, &[elem(ids::SIMPLE_BLOCK, &block(2, 0, 0x80, b"next"))]);
    let mut d = open(file(&[tracks, cluster(0, &first), next])).unwrap();
    let data: Vec<Vec<u8>> = drain(&mut d).into_iter().map(|(_, data)| data).collect();
    // The held packets intact and in order, nothing of the failed Block,
    // then the next Cluster's packet.
    let held: Vec<Vec<u8>> = (0..1023u16).map(|i| i.to_be_bytes().to_vec()).collect();
    let (returned, after) = data.split_at(data.len().min(held.len()));
    assert_eq!(returned, held);
    assert_eq!(after, [b"next".to_vec()]);
}
