//! The metadata masters read at open stay within their budgets however
//! their children are sized: every child in a `Tracks` or `Tags` tree must
//! fit its parent, no Top-Level master but a Cluster may use the unknown
//! size, `Info` fields keep their lengths, and the Cues index keeps what
//! fits its budget, seeking past that by scanning the Clusters.
//!
//! Bytes read are counted and the heap is measured by a counting global
//! allocator. Every test holds one lock, so nothing else allocates while
//! one measures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use oxideav_core::{Demuxer, Error, NullCodecResolver, ReadSeek};
use oxideav_mkv::demux::{self, DamageKind};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

struct Tracking;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

// SAFETY: every call forwards to `System` with the caller's arguments; the
// counters only observe the sizes.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(live, Ordering::SeqCst);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

/// More than any metadata master may hold.
const BIG: usize = 48 << 20;
/// What opening a file may read or hold here.
const SMALL: usize = 4 << 20;

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An element header declaring `size` octets, whatever follows it.
fn header(id: u32, size: usize) -> Vec<u8> {
    [write_element_id(id), write_vint(size as u64, 0)].concat()
}

/// An element header declaring the unknown size.
fn unknown_size(id: u32) -> Vec<u8> {
    [write_element_id(id), vec![0xFF]].concat()
}

fn elem(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = header(id, body.len());
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, v: u64) -> Vec<u8> {
    elem(id, &v.to_be_bytes())
}

/// A `Void` holding `len` zero octets.
fn void(len: usize) -> Vec<u8> {
    let mut out = header(ids::VOID, len);
    out.resize(out.len() + len, 0);
    out
}

fn file(segment: &[Vec<u8>]) -> Vec<u8> {
    let mut out = elem(ids::EBML_HEADER, &elem(ids::EBML_DOC_TYPE, b"matroska"));
    out.extend(header(ids::SEGMENT, segment.iter().map(Vec::len).sum()));
    for element in segment {
        out.extend_from_slice(element);
    }
    out
}

/// The children of subtitle track 1.
fn track_fields() -> Vec<u8> {
    [
        uint(ids::TRACK_NUMBER, 1), uint(ids::TRACK_UID, 1),
        uint(ids::TRACK_TYPE, 0x11), elem(ids::CODEC_ID, b"S_TEXT/UTF8"),
    ].concat()
}

fn tracks() -> Vec<u8> {
    elem(ids::TRACKS, &elem(ids::TRACK_ENTRY, &track_fields()))
}

/// A Cluster at `tc` holding one keyframe packet on track 1.
fn cluster(tc: u64, payload: &[u8]) -> Vec<u8> {
    let block = elem(ids::SIMPLE_BLOCK, &[&[0x81, 0, 0, 0x80][..], payload].concat());
    elem(ids::CLUSTER, &[uint(ids::TIMECODE, tc), block].concat())
}

/// A one-entry SeekHead pointing at `id`.
fn index(id: u32, position: u64) -> Vec<u8> {
    elem(ids::SEEK_HEAD, &elem(ids::SEEK, &[
        elem(ids::SEEK_ID, &write_element_id(id)), uint(ids::SEEK_POSITION, position),
    ].concat()))
}

/// Counts the bytes the demuxer reads.
struct Counted {
    inner: Cursor<Vec<u8>>,
    read: Arc<AtomicUsize>,
}

impl Read for Counted {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n, Ordering::SeqCst);
        Ok(n)
    }
}

impl Seek for Counted {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.inner.seek(to)
    }
}

/// What `run` returns from demuxing `bytes`, the bytes it read and its
/// peak heap beyond what was live before.
fn measure<T>(bytes: Vec<u8>, run: impl FnOnce(Box<dyn ReadSeek>) -> T) -> (T, usize, usize) {
    let read = Arc::new(AtomicUsize::new(0));
    let input: Box<dyn ReadSeek> = Box::new(Counted { inner: Cursor::new(bytes), read: read.clone() });
    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let out = run(input);
    let peak = PEAK.load(Ordering::SeqCst) - base;
    (out, read.load(Ordering::SeqCst), peak)
}

fn kind(e: Error) -> String {
    match e {
        Error::InvalidData(_) => "InvalidData".into(),
        Error::Eof => "Eof".into(),
        other => format!("{other}"),
    }
}

/// The first packet of `input`, or the kind of error opening it or reading
/// that packet gives.
fn first(input: Box<dyn ReadSeek>, resilient: bool) -> Result<Vec<u8>, String> {
    let mut d = if resilient {
        demux::open_resilient_typed(input, &NullCodecResolver)
    } else {
        demux::open_typed(input, &NullCodecResolver)
    }.map_err(kind)?;
    d.next_packet().map(|p| p.data).map_err(kind)
}

/// Every packet of `input`, opened strictly.
fn all(input: Box<dyn ReadSeek>) -> Result<Vec<Vec<u8>>, String> {
    let mut d = demux::open_typed(input, &NullCodecResolver).map_err(kind)?;
    let mut out = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => out.push(p.data),
            Err(Error::Eof) => return Ok(out),
            Err(e) => return Err(kind(e)),
        }
    }
}

/// Measures each case and lists those whose first packet differs from
/// `expected`, or that read or held `SMALL` or more.
#[derive(Default)]
struct Cases(Vec<String>);

impl Cases {
    fn check(&mut self, case: &str, bytes: Vec<u8>, resilient: bool, expected: Result<&[u8], &str>) {
        let (outcome, read, peak) = measure(bytes, |input| first(input, resilient));
        let expected = expected.map(<[u8]>::to_vec).map_err(str::to_string);
        if outcome != expected || read >= SMALL || peak >= SMALL {
            self.0.push(format!("{case}: {outcome:?} after reading {read} bytes, peak {peak} heap bytes"));
        }
    }
}

/// A Tags master whose SimpleTag ends with a TagString declaring BIG octets
/// that none of its parents hold.
fn overrun_tags() -> Vec<u8> {
    let simple_tag = [elem(ids::TAG_NAME, b"T"), header(ids::TAG_STRING, BIG)].concat();
    elem(ids::TAGS, &elem(ids::TAG, &elem(ids::SIMPLE_TAG, &simple_tag)))
}

#[test]
fn tracks_and_tags_children_must_fit_their_parents() {
    let _serial = serial();
    let mut cases = Cases::default();
    let c = cluster(0, b"a");
    // Ahead of the Clusters, with BIG octets physically following.
    let inline = file(&[tracks(), overrun_tags(), void(BIG), c.clone()]);
    cases.check("Tags in line", inline.clone(), false, Err("InvalidData"));
    cases.check("Tags in line, resilient", inline, true, Ok(b"a"));
    // After the Clusters, reached through the SeekHead.
    let t = tracks();
    let at = (index(ids::TAGS, 0).len() + t.len() + c.len()) as u64;
    let followed = file(&[index(ids::TAGS, at), t, c.clone(), overrun_tags(), void(BIG)]);
    cases.check("Tags followed", followed, false, Ok(b"a"));
    // A TrackEntry's Name, and a ContentCompSettings four levels down.
    let name = [track_fields(), header(ids::NAME, BIG)].concat();
    let compression = [uint(ids::CONTENT_COMP_ALGO, 3), header(ids::CONTENT_COMP_SETTINGS, BIG)].concat();
    let encodings = elem(ids::CONTENT_ENCODINGS, &elem(ids::CONTENT_ENCODING, &elem(ids::CONTENT_COMPRESSION, &compression)));
    for (child, entry) in [("Name", name), ("ContentCompSettings", [track_fields(), encodings].concat())] {
        let bytes = file(&[elem(ids::TRACKS, &elem(ids::TRACK_ENTRY, &entry)), void(BIG), c.clone()]);
        cases.check(&format!("Tracks {child}"), bytes.clone(), false, Err("InvalidData"));
        cases.check(&format!("Tracks {child}, resilient"), bytes, true, Err("InvalidData"));
    }
    // After the last Cluster: the walk skips the malformed Tags.
    let (packets, read, peak) = measure(file(&[tracks(), c, overrun_tags(), void(BIG)]), all);
    if packets != Ok(vec![b"a".to_vec()]) || read >= SMALL || peak >= SMALL {
        cases.0.push(format!("Tags trailing: {packets:?} after reading {read} bytes, peak {peak} heap bytes"));
    }
    assert!(cases.0.is_empty(), "{:#?}", cases.0);
}

#[test]
fn only_a_cluster_may_use_the_unknown_size() {
    let _serial = serial();
    let mut cases = Cases::default();
    let tag = elem(ids::TAG, &elem(ids::SIMPLE_TAG, &[elem(ids::TAG_NAME, b"T"), elem(ids::TAG_STRING, b"v")].concat()));
    let tags = [unknown_size(ids::TAGS), tag].concat();
    let inline = file(&[tracks(), tags.clone(), cluster(0, b"a")]);
    cases.check("Tags in line", inline.clone(), false, Err("InvalidData"));
    cases.check("Tags in line, resilient", inline, true, Ok(b"a"));
    let unsized_tracks = file(&[[unknown_size(ids::TRACKS), elem(ids::TRACK_ENTRY, &track_fields())].concat(), cluster(0, b"a")]);
    cases.check("Tracks in line", unsized_tracks.clone(), false, Err("InvalidData"));
    cases.check("Tracks in line, resilient", unsized_tracks, true, Err("InvalidData"));
    // Between Clusters: damage, and the walk resumes at the next Cluster.
    let (packets, _, _) = measure(file(&[tracks(), cluster(0, b"a"), tags, cluster(1000, b"b")]), all);
    if packets != Ok(vec![b"a".to_vec(), b"b".to_vec()]) {
        cases.0.push(format!("Tags between Clusters: {packets:?}"));
    }
    // Every other Top-Level master, and an unassigned one, follows the
    // same rule.
    let unassigned = 0x1F00_0001;
    for (name, id, body) in [
        ("Info", ids::INFO, uint(ids::TIMECODE_SCALE, 1_000_000)),
        ("Cues", ids::CUES, void(4)),
        ("Chapters", ids::CHAPTERS, void(4)),
        ("Attachments", ids::ATTACHMENTS, void(4)),
        ("SeekHead", ids::SEEK_HEAD, void(4)),
        ("An unassigned master", unassigned, void(4)),
    ] {
        let master = [unknown_size(id), body].concat();
        let inline = file(&[tracks(), master.clone(), cluster(0, b"a")]);
        cases.check(&format!("{name} in line"), inline.clone(), false, Err("InvalidData"));
        cases.check(&format!("{name} in line, resilient"), inline, true, Ok(b"a"));
        let (packets, _, _) = measure(file(&[tracks(), cluster(0, b"a"), master, cluster(1000, b"b")]), all);
        if packets != Ok(vec![b"a".to_vec(), b"b".to_vec()]) {
            cases.0.push(format!("{name} between Clusters: {packets:?}"));
        }
    }
    assert!(cases.0.is_empty(), "{:#?}", cases.0);
}

#[test]
fn info_fields_keep_their_sizes() {
    let _serial = serial();
    let mut cases = Cases::default();
    let info = |children: Vec<u8>| elem(ids::INFO, &[uint(ids::TIMECODE_SCALE, 1_000_000), children].concat());
    let with = |info: Vec<u8>| file(&[info, tracks(), cluster(0, b"a")]);
    // RFC 9559 §5.1.2: the three Segment UIDs are 16 octets.
    let short_uid = with(info(elem(ids::SEGMENT_UID, &[1; 15])));
    cases.check("15-octet SegmentUUID", short_uid.clone(), false, Err("InvalidData"));
    cases.check("15-octet SegmentUUID, resilient", short_uid, true, Ok(b"a"));
    cases.check("17-octet NextUUID", with(info(elem(ids::NEXT_UID, &[2; 17]))), false, Err("InvalidData"));
    cases.check("16-octet PrevUUID", with(info(elem(ids::PREV_UID, &[3; 16]))), false, Ok(b"a"));
    // A text field holds at most 64 KiB.
    cases.check("64 KiB Title", with(info(elem(ids::TITLE, &vec![b'x'; 64 << 10]))), false, Ok(b"a"));
    cases.check("64 KiB + 1 Title", with(info(elem(ids::TITLE, &vec![b'x'; (64 << 10) + 1]))), false, Err("InvalidData"));
    cases.check("48 MiB Title", with(info(elem(ids::TITLE, &vec![b'x'; BIG]))), false, Err("InvalidData"));
    // The whole Info keeps at most 1 MiB.
    let families: Vec<u8> = (0..70_000u32).flat_map(|i| elem(ids::SEGMENT_FAMILY, &[i.to_be_bytes(); 4].concat())).collect();
    let families = with(info(families));
    cases.check("70,000 SegmentFamilies", families.clone(), false, Err("InvalidData"));
    cases.check("70,000 SegmentFamilies, resilient", families, true, Ok(b"a"));
    // A Void is stepped over unread.
    cases.check("48 MiB Void", with(info(void(BIG))), false, Ok(b"a"));
    assert!(cases.0.is_empty(), "{:#?}", cases.0);
}

/// A CuePoint at `time` for track 1, in the Cluster at Segment Position
/// `cluster`, with a CueReference when `reference`.
fn cue_point(time: u64, cluster: u64, reference: bool) -> Vec<u8> {
    let mut positions = [uint(ids::CUE_TRACK, 1), uint(ids::CUE_CLUSTER_POSITION, cluster)].concat();
    if reference {
        positions.extend(elem(ids::CUE_REFERENCE, &uint(ids::CUE_REF_TIME, time)));
    }
    elem(ids::CUE_POINT, &[uint(ids::CUE_TIME, time), elem(ids::CUE_TRACK_POSITIONS, &positions)].concat())
}

/// CuePoints in the first Cluster this many milliseconds apart, before one
/// for each later Cluster: more than the 32 MiB index budget keeps.
const CROWDED: u64 = 100_000;

fn crowded_cues(clusters: [u64; 3]) -> Vec<u8> {
    let mut points: Vec<u8> = (0..CROWDED).flat_map(|t| cue_point(t, clusters[0], false)).collect();
    points.extend(cue_point(200_000, clusters[1], false));
    points.extend(cue_point(400_000, clusters[2], false));
    elem(ids::CUES, &points)
}

#[test]
fn cues_past_their_budget_keep_what_fits_and_seek_on_by_scanning() {
    let _serial = serial();
    let [c0, c1, c2] = [cluster(0, b"a"), cluster(200_000, b"b"), cluster(400_000, b"c")];
    let at = |first: u64| [first, first + c0.len() as u64, first + (c0.len() + c1.len()) as u64];
    let t = tracks();
    // Ahead of the Clusters.
    let clusters = at((t.len() + crowded_cues([0; 3]).len()) as u64);
    let inline = file(&[t.clone(), crowded_cues(clusters), c0.clone(), c1.clone(), c2.clone()]);
    // After them, reached through the SeekHead.
    let clusters = at((index(ids::CUES, 0).len() + t.len()) as u64);
    let cues_at = clusters[2] + c2.len() as u64;
    let followed = file(&[index(ids::CUES, cues_at), t, c0.clone(), c1.clone(), c2.clone(), crowded_cues(clusters)]);
    let mut failures = Vec::new();
    for (layout, bytes) in [("in line", inline), ("followed", followed)] {
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
        let base = LIVE.load(Ordering::SeqCst);
        let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
        let held = LIVE.load(Ordering::SeqCst).saturating_sub(base);
        let damaged = d.damage_events().iter().filter(|e| e.kind() == DamageKind::DamagedMaster(ids::CUES)).count();
        let kept = d.cue_points().len();
        // Past the kept points the Clusters are scanned; within them the
        // index lands.
        let far = d.seek_to(0, 400_000).ok().zip(d.next_packet().ok().map(|p| p.data));
        let near = d.seek_to(0, 50).ok().zip(d.next_packet().ok().map(|p| p.data));
        let ok = held < (32 << 20) + (1 << 20)
            && damaged == 1
            && kept > 0
            && kept < CROWDED as usize
            && far == Some((400_000, b"c".to_vec()))
            && near == Some((50, b"a".to_vec()));
        if !ok {
            failures.push(format!(
                "{layout}: held {held} heap bytes, {damaged} damage events, kept {kept} CuePoints, far {far:?}, near {near:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Subtitle tracks 1 and 2.
fn two_tracks() -> Vec<u8> {
    let second = [
        uint(ids::TRACK_NUMBER, 2), uint(ids::TRACK_UID, 2),
        uint(ids::TRACK_TYPE, 0x11), elem(ids::CODEC_ID, b"S_TEXT/UTF8"),
    ].concat();
    elem(ids::TRACKS, &[elem(ids::TRACK_ENTRY, &track_fields()), elem(ids::TRACK_ENTRY, &second)].concat())
}

/// A Cluster at `tc` holding a keyframe packet on track 2, then one on
/// track 1.
fn two_track_cluster(tc: u64, second: &[u8], first: &[u8]) -> Vec<u8> {
    let block = |track: u8, payload: &[u8]| elem(ids::SIMPLE_BLOCK, &[&[0x80 | track, 0, 0, 0x80][..], payload].concat());
    elem(ids::CLUSTER, &[uint(ids::TIMECODE, tc), block(2, second), block(1, first)].concat())
}

/// A CuePoint at `time` for track 2 only, in the Cluster at Segment
/// Position `cluster`.
fn track_two_cue_point(time: u64, cluster: u64) -> Vec<u8> {
    let positions = [uint(ids::CUE_TRACK, 2), uint(ids::CUE_CLUSTER_POSITION, cluster)].concat();
    elem(ids::CUE_POINT, &[uint(ids::CUE_TIME, time), elem(ids::CUE_TRACK_POSITIONS, &positions)].concat())
}

/// Where `d` lands seeking `stream` to `target`, and the packet it reads
/// there.
fn seek_and_read(d: &mut dyn Demuxer, stream: u32, target: i64) -> Result<(i64, Vec<u8>), String> {
    let landed = d.seek_to(stream, target).map_err(kind)?;
    Ok((landed, d.next_packet().map_err(kind)?.data))
}

/// The crowded index indexes track 1 first and track 2 only after it, so
/// the CuePoints the budget keeps index track 1 alone. A seek on track 2,
/// even before the last kept CueTime, scans the Clusters like a seek past
/// it does.
#[test]
fn a_track_the_kept_cues_miss_seeks_by_scanning() {
    let _serial = serial();
    let [c0, c1, c2] = [
        two_track_cluster(0, b"A", b"a"),
        two_track_cluster(200_000, b"B", b"b"),
        two_track_cluster(400_000, b"C", b"c"),
    ];
    let cues = |at: [u64; 3]| {
        let mut points: Vec<u8> = (0..CROWDED).flat_map(|t| cue_point(t, at[0], false)).collect();
        points.extend(track_two_cue_point(200_000, at[1]));
        points.extend(track_two_cue_point(400_000, at[2]));
        elem(ids::CUES, &points)
    };
    let t = two_tracks();
    let first = (t.len() + cues([0; 3]).len()) as u64;
    let at = [first, first + c0.len() as u64, first + (c0.len() + c1.len()) as u64];
    let mut d = demux::open_typed(Box::new(Cursor::new(file(&[t, cues(at), c0, c1, c2]))), &NullCodecResolver).unwrap();
    let landings = [(1, 50), (1, 400_000), (0, 50)].map(|(stream, target)| seek_and_read(&mut d, stream, target));
    let expected = [Ok((0, b"A".to_vec())), Ok((400_000, b"C".to_vec())), Ok((50, b"A".to_vec()))];
    assert_eq!(landings, expected);
}

/// Forty thousand Clusters a second apart, each indexed by a CuePoint and
/// every hundredth point with a CueReference: a large index within budget,
/// whose points still decide where a seek lands.
#[test]
fn a_large_valid_cues_index_still_seeks_by_its_points() {
    let _serial = serial();
    const CLUSTERS: u16 = 40_000;
    let clusters: Vec<Vec<u8>> = (0..CLUSTERS).map(|i| cluster(u64::from(i) * 1000, &i.to_be_bytes())).collect();
    let step = clusters[0].len() as u64;
    let cues = |first: u64| {
        let points: Vec<u8> = (0..CLUSTERS)
            .flat_map(|i| cue_point(u64::from(i) * 1000, first + u64::from(i) * step, i % 100 == 0))
            .collect();
        elem(ids::CUES, &points)
    };
    let t = tracks();
    let first = (t.len() + cues(0).len()) as u64;
    let segment = [vec![t, cues(first)], clusters].concat();
    let mut d = demux::open_typed(Box::new(Cursor::new(file(&segment))), &NullCodecResolver).unwrap();
    let mut landings = Vec::new();
    for target in [500, 12_345_678, 39_999_999] {
        let landed = d.seek_to(0, target).ok();
        landings.push((landed, d.next_packet().ok().map(|p| p.data)));
    }
    let expected = [(0u16, 0u64), (12_345, 12_345_000), (39_999, 39_999_000)]
        .map(|(i, at)| (Some(at as i64), Some(i.to_be_bytes().to_vec())));
    assert_eq!(landings, expected);
    assert_eq!(d.cue_points().len(), CLUSTERS as usize);
    assert!(d.damage_events().is_empty(), "{:?}", d.damage_events());
}
