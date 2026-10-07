//! The metadata masters read at open stay within their budgets however
//! their children are sized: every child in a `Tracks` or `Tags` tree must
//! fit its parent, no Top-Level master but a Cluster may use the unknown
//! size, `Info` fields keep their lengths, Chapters, Attachments and the
//! Cues index keep what fits their budgets, seeks past the kept Cues scan
//! the Clusters, and an attachment payload is read only from inside its
//! parents.
//!
//! Bytes read are counted and the heap is measured by a counting global
//! allocator. Every test holds one lock, so nothing else allocates while
//! one measures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use oxideav_core::{Demuxer, Error, NullCodecResolver, ReadSeek};
use oxideav_mkv::demux::{self, Chapter, DamageKind, TargetUid};
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

/// Every child in a `Tracks` or `Tags` tree must fit its parent, checked
/// before it is read. Tracks are essential, so an overrun there fails the
/// open; a Tags master is optional, so it is dropped as damage and the
/// open goes on, strict or resilient.
#[test]
fn tracks_and_tags_children_must_fit_their_parents() {
    let _serial = serial();
    let mut cases = Cases::default();
    let c = cluster(0, b"a");
    // Ahead of the Clusters, with BIG octets physically following.
    let inline = file(&[tracks(), overrun_tags(), void(BIG), c.clone()]);
    cases.check("Tags in line", inline.clone(), false, Ok(b"a"));
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

/// RFC 9559 allows the unknown size on a Segment and a Cluster alone. Any
/// other Top-Level master using it is damage: an essential one (Info,
/// Tracks) fails a strict open, an optional one is dropped and the open
/// goes on.
#[test]
fn only_a_cluster_may_use_the_unknown_size() {
    let _serial = serial();
    let mut cases = Cases::default();
    let tag = elem(ids::TAG, &elem(ids::SIMPLE_TAG, &[elem(ids::TAG_NAME, b"T"), elem(ids::TAG_STRING, b"v")].concat()));
    let tags = [unknown_size(ids::TAGS), tag].concat();
    let inline = file(&[tracks(), tags.clone(), cluster(0, b"a")]);
    cases.check("Tags in line", inline.clone(), false, Ok(b"a"));
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
        let strict = if id == ids::INFO { Err("InvalidData") } else { Ok(&b"a"[..]) };
        cases.check(&format!("{name} in line"), inline.clone(), false, strict);
        cases.check(&format!("{name} in line, resilient"), inline, true, Ok(b"a"));
        // A header with nothing after it: the Cluster starts right where
        // its body would.
        let bare = file(&[tracks(), unknown_size(id), cluster(0, b"a")]);
        cases.check(&format!("{name} header alone, resilient"), bare, true, Ok(b"a"));
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

/// A Chapters master holding one chapter titled `title`.
fn chapters_titled(title: &[u8]) -> Vec<u8> {
    elem(ids::CHAPTERS, &elem(ids::EDITION_ENTRY, &chapter_atom(1, title)))
}

/// A ChapterAtom with UID `uid`, starting at 0, titled `title`.
fn chapter_atom(uid: u64, title: &[u8]) -> Vec<u8> {
    elem(ids::CHAPTER_ATOM, &[
        uint(ids::CHAPTER_UID, uid), uint(ids::CHAPTER_TIME_START, 0),
        elem(ids::CHAPTER_DISPLAY, &elem(ids::CHAP_STRING, title)),
    ].concat())
}

/// An AttachedFile with UID `uid`, named `name`, with `data` as its payload.
fn attached_file(uid: u64, name: &[u8], data: &[u8]) -> Vec<u8> {
    elem(ids::ATTACHED_FILE, &[
        elem(ids::FILE_NAME, name), elem(ids::FILE_MIME_TYPE, b"font/ttf"),
        uint(ids::FILE_UID, uid), elem(ids::FILE_DATA, data),
    ].concat())
}

fn attachments_named(name: &[u8]) -> Vec<u8> {
    elem(ids::ATTACHMENTS, &attached_file(1, name, b"d"))
}

/// What an open keeps of one text field: its length, the damage noted and
/// the first packet.
type KeptText = (Option<usize>, Vec<DamageKind>, Vec<u8>);

/// Builds a master holding the text it is given.
type TextMaster = fn(&[u8]) -> Vec<u8>;

/// The length of the one chapter title or attachment file name an open of
/// `bytes` keeps, the damage it noted and its first packet.
fn kept_text(bytes: Vec<u8>, resilient: bool) -> Result<KeptText, String> {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut d = if resilient {
        demux::open_resilient_typed(input, &NullCodecResolver)
    } else {
        demux::open_typed(input, &NullCodecResolver)
    }.map_err(kind)?;
    let title = d.chapters().first().and_then(|e| e.chapters.first()).and_then(|c| c.displays.first()).map(|t| t.string.len());
    let text = title.or_else(|| d.attachments().first().map(|a| a.filename.len()));
    let damage = d.damage_events().iter().map(|e| e.kind()).collect();
    Ok((text, damage, d.next_packet().map(|p| p.data).map_err(kind)?))
}

/// A Chapters or Attachments text field holds at most 64 KiB, checked
/// before it is read. A longer one is damage: the chapter or attachment is
/// dropped with one damage event, even where the master's 1 MiB budget
/// would hold it, and the open goes on, strict or resilient.
#[test]
fn chapter_and_attachment_text_fields_hold_64_kib() {
    let _serial = serial();
    let mut cases = Cases::default();
    let with = |master: Vec<u8>| file(&[tracks(), master, cluster(0, b"a")]);
    let builders: [(&str, u32, TextMaster); 2] =
        [("ChapString", ids::CHAPTERS, chapters_titled), ("FileName", ids::ATTACHMENTS, attachments_named)];
    for (field, id, master) in builders {
        let at_limit = with(master(&vec![b'x'; 64 << 10]));
        let over = with(master(&vec![b'x'; (64 << 10) + 1]));
        for resilient in [false, true] {
            let kept = kept_text(at_limit.clone(), resilient);
            if kept != Ok((Some(64 << 10), vec![], b"a".to_vec())) {
                cases.0.push(format!("64 KiB {field}, resilient {resilient}: {kept:?}"));
            }
            let dropped = kept_text(over.clone(), resilient);
            if dropped != Ok((None, vec![DamageKind::DamagedMaster(id)], b"a".to_vec())) {
                cases.0.push(format!("64 KiB + 1 {field}, resilient {resilient}: {dropped:?}"));
            }
        }
        cases.check(&format!("48 MiB {field}"), with(master(&vec![b'x'; BIG])), false, Ok(b"a"));
    }
    // A 48 MiB payload stays on disk at open and is read on request.
    let payload = with(elem(ids::ATTACHMENTS, &attached_file(1, b"font.ttf", &vec![7; BIG])));
    cases.check("48 MiB FileData", payload.clone(), false, Ok(b"a"));
    let fetched = demux::open_typed(Box::new(Cursor::new(payload)), &NullCodecResolver)
        .and_then(|mut d| d.attachment_data(1))
        .map(|data| data.len());
    if fetched.as_ref().ok() != Some(&BIG) {
        cases.0.push(format!("48 MiB FileData on request: {fetched:?}"));
    }
    assert!(cases.0.is_empty(), "{:#?}", cases.0);
}

/// The chapters of `chapters` depth first, as (index, UID) pairs.
fn chapter_order(chapters: &[Chapter], out: &mut Vec<(u64, u64)>) {
    for c in chapters {
        out.push((u64::from(c.index), c.uid.unwrap_or(0)));
        chapter_order(&c.children, out);
    }
}

/// What an open of `input` keeps: its first packet, the chapters depth
/// first and the attachments, each as (index, UID) pairs, how many flat
/// metadata keys name each index under `scope`, and the damage noted.
#[derive(Debug)]
struct Kept {
    packet: Result<Vec<u8>, String>,
    chapters: Vec<(u64, u64)>,
    attachments: Vec<(u64, u64)>,
    flat: BTreeMap<u64, usize>,
    damage: Vec<DamageKind>,
}

fn kept(input: Box<dyn ReadSeek>, resilient: bool, scope: &str) -> Result<Kept, String> {
    let mut d = if resilient {
        demux::open_resilient_typed(input, &NullCodecResolver)
    } else {
        demux::open_typed(input, &NullCodecResolver)
    }.map_err(kind)?;
    let mut chapters = Vec::new();
    for edition in d.chapters() {
        chapter_order(&edition.chapters, &mut chapters);
    }
    let attachments = d.attachments().iter().map(|a| (u64::from(a.index), a.uid)).collect();
    let mut flat = BTreeMap::new();
    for (key, _) in d.metadata() {
        let mut parts = key.split(':');
        if parts.next() == Some(scope) {
            if let Some(i) = parts.next().and_then(|i| i.parse().ok()) {
                *flat.entry(i).or_insert(0) += 1;
            }
        }
    }
    let damage = d.damage_events().iter().map(|e| e.kind()).collect();
    let packet = d.next_packet().map(|p| p.data).map_err(kind);
    Ok(Kept { packet, chapters, attachments, flat, damage })
}

/// What the open may hold when a list overruns its 1 MiB budget: the
/// budget, and the rest of the open.
const CUT_PEAK: usize = (1 << 20) + (512 << 10);

/// Builds a master holding the number of records it is given.
type Master<'a> = &'a dyn Fn(u64) -> Vec<u8>;

/// Past their 1 MiB budget, Chapters and Attachments keep the records that
/// fit, in order and with their flat metadata, note the cut as one damage
/// event and let the open go on, strict or resilient, in line or found
/// through the SeekHead: a long list never stops playback.
#[test]
fn chapters_and_attachments_past_1_mib_keep_what_fit() {
    let _serial = serial();
    let mut failures = Vec::new();
    let flat_chapters = |n: u64| {
        elem(ids::CHAPTERS, &elem(ids::EDITION_ENTRY, &(1..=n).flat_map(|i| chapter_atom(i, b"c")).collect::<Vec<u8>>()))
    };
    // One chapter holding all the others.
    let nested_chapters = |n: u64| {
        let children: Vec<u8> = (2..=n).flat_map(|i| chapter_atom(i, b"c")).collect();
        let parent = [uint(ids::CHAPTER_UID, 1), elem(ids::CHAPTER_DISPLAY, &elem(ids::CHAP_STRING, b"p")), children].concat();
        elem(ids::CHAPTERS, &elem(ids::EDITION_ENTRY, &elem(ids::CHAPTER_ATOM, &parent)))
    };
    let attachments = |n: u64| elem(ids::ATTACHMENTS, &(1..=n).flat_map(|i| attached_file(i, b"f", b"d")).collect::<Vec<u8>>());
    // Each list, the flat metadata scope and how many keys each record gets
    // there: start time and title, or name, MIME type and size.
    let lists: [(&str, u32, &str, usize, Master); 3] = [
        ("chapters", ids::CHAPTERS, "chapter", 2, &flat_chapters),
        ("nested chapters", ids::CHAPTERS, "chapter", 2, &nested_chapters),
        ("attached files", ids::ATTACHMENTS, "attachment", 3, &attachments),
    ];
    for (name, id, scope, keys, list) in lists {
        for n in [100, 20_000u64] {
            let (t, master, c) = (tracks(), list(n), cluster(0, b"a"));
            let position = (index(id, 0).len() + t.len() + c.len()) as u64;
            let layouts = [
                ("in line", file(&[t.clone(), master.clone(), c.clone()])),
                ("after the Clusters", file(&[index(id, position), t, c, master])),
            ];
            for (layout, bytes) in layouts {
                for resilient in [false, true] {
                    let case = format!("{n} {name} {layout}{}", if resilient { ", resilient" } else { "" });
                    let (outcome, _, peak) = measure(bytes.clone(), |input| kept(input, resilient, scope));
                    let Ok(kept) = outcome else {
                        failures.push(format!("{case}: open {outcome:?}"));
                        continue;
                    };
                    let records = if id == ids::CHAPTERS { &kept.chapters } else { &kept.attachments };
                    let k = records.len() as u64;
                    let prefix: Vec<(u64, u64)> = (1..=k).map(|i| (i, i)).collect();
                    let flat: BTreeMap<u64, usize> = (1..=k).map(|i| (i, keys)).collect();
                    let (cut, bound) = if n == 100 { (vec![], SMALL) } else { (vec![DamageKind::DamagedMaster(id)], CUT_PEAK) };
                    let whole = if n == 100 { k == n } else { k > 0 && k < n };
                    if kept.packet != Ok(b"a".to_vec()) || !whole || *records != prefix || kept.flat != flat || kept.damage != cut || peak >= bound {
                        failures.push(format!(
                            "{case}: packet {:?}, kept {k} in order {}, flat keys {}, damage {:?}, peak {peak} heap bytes",
                            kept.packet, *records == prefix, kept.flat == flat, kept.damage,
                        ));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// An attachment payload is read only from inside its AttachedFile and
/// the Segment, its buffer growing as the bytes arrive. A FileData
/// declaring 4 GiB that its AttachedFile does not hold is refused unread,
/// and the open steps over it to the Clusters.
#[test]
fn attachment_payloads_are_read_only_inside_their_parents() {
    let _serial = serial();
    let mut failures = Vec::new();
    // A real payload spanning several reads.
    let payload: Vec<u8> = (0..200u32 << 10).map(|i| (i % 251) as u8).collect();
    let forged = elem(ids::ATTACHED_FILE, &[
        elem(ids::FILE_NAME, b"forged.ttf"), uint(ids::FILE_UID, 2),
        header(ids::FILE_DATA, 1 << 32), b"abcdefgh".to_vec(),
    ].concat());
    let attachments = elem(ids::ATTACHMENTS, &[attached_file(1, b"real.ttf", &payload), forged].concat());
    let layouts = [
        ("in a tiny file", file(&[tracks(), attachments.clone(), cluster(0, b"a")])),
        ("before 4 MiB more", file(&[tracks(), attachments, cluster(0, b"a"), void(4 << 20)])),
    ];
    for (layout, bytes) in layouts {
        let read = Arc::new(AtomicUsize::new(0));
        let input = Box::new(Counted { inner: Cursor::new(bytes), read: read.clone() });
        let mut d = match demux::open_typed(input, &NullCodecResolver) {
            Ok(d) => d,
            Err(e) => {
                failures.push(format!("{layout}: open {}", kind(e)));
                continue;
            }
        };
        let packet = d.next_packet().map(|p| p.data).map_err(kind);
        let real = d.attachment_data(1).map_err(kind);
        read.store(0, Ordering::SeqCst);
        let base = LIVE.load(Ordering::SeqCst);
        PEAK.store(base, Ordering::SeqCst);
        let forged = d.attachment_data(2).map(|data| data.len()).map_err(kind);
        let (forged_read, peak) = (read.load(Ordering::SeqCst), PEAK.load(Ordering::SeqCst) - base);
        let real_exact = real.as_ref() == Ok(&payload);
        if packet != Ok(b"a".to_vec()) || !real_exact || forged != Err("InvalidData".to_string()) || forged_read > 8 || peak >= 64 << 10 {
            failures.push(format!(
                "{layout}: packet {packet:?}, real payload exact {real_exact}, forged {forged:?} after reading {forged_read} bytes, peak {peak} heap bytes",
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
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

/// What an open of `bytes` plays and keeps: every packet, the damage it
/// noted, the chapters depth first and the attachments as (index, UID)
/// pairs, how many CuePoints it kept, and where seeking stream 0 to 0,
/// 1 s and 2 s lands with the packet read there.
#[derive(Debug, PartialEq)]
struct Played {
    packets: Vec<Vec<u8>>,
    damage: Vec<DamageKind>,
    chapters: Vec<(u64, u64)>,
    attachments: Vec<(u64, u64)>,
    cue_points: usize,
    seeks: Vec<Result<(i64, Vec<u8>), String>>,
}

fn played(bytes: Vec<u8>, resilient: bool) -> Result<Played, String> {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut d = if resilient {
        demux::open_resilient_typed(input, &NullCodecResolver)
    } else {
        demux::open_typed(input, &NullCodecResolver)
    }.map_err(kind)?;
    let mut packets = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => packets.push(p.data),
            Err(Error::Eof) => break,
            Err(e) => return Err(format!("after {} packets: {}", packets.len(), kind(e))),
        }
    }
    let damage = d.damage_events().iter().map(|e| e.kind()).collect();
    let mut chapters = Vec::new();
    for edition in d.chapters() {
        chapter_order(&edition.chapters, &mut chapters);
    }
    let attachments = d.attachments().iter().map(|a| (u64::from(a.index), a.uid)).collect();
    let cue_points = d.cue_points().len();
    let seeks = [0, 1000, 2000].map(|target| seek_and_read(&mut d, 0, target)).to_vec();
    Ok(Played { packets, damage, chapters, attachments, cue_points, seeks })
}

/// Optional masters never stop an open, strict or resilient. A child whose
/// size runs past its parent inside Chapters, Cues, Tags, Attachments or a
/// SeekHead is damage, noted once, and every packet still plays. Chapters,
/// Attachments and Cues keep what came before the broken child, and a seek
/// the Cues kept do not cover scans the Clusters.
#[test]
fn optional_masters_with_a_child_past_its_parent_are_damage() {
    let _serial = serial();
    let mut failures = Vec::new();
    let t = tracks();
    let clusters = [cluster(0, b"a"), cluster(1000, b"b"), cluster(2000, b"c")];
    // Cues ahead of the Clusters: `points` given the first Cluster's
    // Segment Position.
    let cues = |points: &dyn Fn(u64) -> Vec<u8>| {
        let first = (t.len() + elem(ids::CUES, &points(0)).len()) as u64;
        elem(ids::CUES, &points(first))
    };
    let past = |id: u32| header(id, 1 << 20);
    let second_atom = elem(ids::CHAPTER_ATOM, &[uint(ids::CHAPTER_UID, 2), past(ids::CHAPTER_TIME_START)].concat());
    let second_file = elem(ids::ATTACHED_FILE, &[elem(ids::FILE_NAME, b"two.ttf"), past(ids::FILE_MIME_TYPE)].concat());
    let tag = |value: Vec<u8>| elem(ids::TAG, &elem(ids::SIMPLE_TAG, &[elem(ids::TAG_NAME, b"T"), value].concat()));
    let cases = [
        ("a ChapterAtom past its EditionEntry", ids::CHAPTERS,
            elem(ids::CHAPTERS, &elem(ids::EDITION_ENTRY, &[chapter_atom(1, b"one"), past(ids::CHAPTER_ATOM)].concat()))),
        ("a ChapterTimeStart past its ChapterAtom", ids::CHAPTERS,
            elem(ids::CHAPTERS, &elem(ids::EDITION_ENTRY, &[chapter_atom(1, b"one"), second_atom].concat()))),
        ("a CuePoint past its Cues", ids::CUES, cues(&|first| [cue_point(0, first, false), past(ids::CUE_POINT)].concat())),
        ("the first CuePoint past its Cues", ids::CUES, cues(&|_| past(ids::CUE_POINT))),
        ("a CueTrack past its CueTrackPositions", ids::CUES, cues(&|first| {
            let broken = elem(ids::CUE_TRACK_POSITIONS, &[uint(ids::CUE_CLUSTER_POSITION, first), past(ids::CUE_TRACK)].concat());
            [cue_point(0, first, false), elem(ids::CUE_POINT, &[uint(ids::CUE_TIME, 1000), broken].concat())].concat()
        })),
        ("a TagString past its SimpleTag", ids::TAGS, elem(ids::TAGS, &[tag(elem(ids::TAG_STRING, b"v")), tag(past(ids::TAG_STRING))].concat())),
        ("a FileMimeType past its AttachedFile", ids::ATTACHMENTS,
            elem(ids::ATTACHMENTS, &[attached_file(1, b"one.ttf", b"d"), second_file].concat())),
        ("a Seek past its SeekHead", ids::SEEK_HEAD, elem(ids::SEEK_HEAD, &past(ids::SEEK))),
    ];
    let every = vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()];
    let landings: Vec<Result<(i64, Vec<u8>), String>> = vec![Ok((0, b"a".to_vec())), Ok((1000, b"b".to_vec())), Ok((2000, b"c".to_vec()))];
    for (case, id, master) in cases {
        let bytes = file(&[vec![t.clone(), master], clusters.to_vec()].concat());
        for resilient in [false, true] {
            let case = format!("{case}{}", if resilient { ", resilient" } else { "" });
            let got = match played(bytes.clone(), resilient) {
                Ok(got) => got,
                Err(e) => {
                    failures.push(format!("{case}: {e}"));
                    continue;
                }
            };
            // What came before the broken child is kept.
            let kept = match id {
                ids::CHAPTERS => got.chapters == [(1, 1)],
                ids::ATTACHMENTS => got.attachments == [(1, 1)],
                ids::CUES => got.cue_points == usize::from(!case.starts_with("the first")),
                _ => true,
            };
            if got.packets != every || got.damage != [DamageKind::DamagedMaster(id)] || !kept || got.seeks != landings {
                failures.push(format!("{case}: {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Fails each read at or past `from` with the source's own
/// `UnexpectedEof`, as a transport that drops mid-read does.
struct Dropping {
    inner: Cursor<Vec<u8>>,
    from: Arc<AtomicU64>,
}

impl Read for Dropping {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.inner.position() >= self.from.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "transport dropped"));
        }
        self.inner.read(buf)
    }
}

impl Seek for Dropping {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.inner.seek(to)
    }
}

/// A source that fails while a payload is read gets its own error back,
/// not one for a payload the input cuts short, and the reader is back
/// where it was: the next packet plays and a later fetch succeeds.
#[test]
fn a_source_failure_fetching_an_attachment_is_returned_as_itself() {
    let _serial = serial();
    let attachments = elem(ids::ATTACHMENTS, &attached_file(1, b"font.ttf", &[7; 64]));
    let bytes = file(&[tracks(), attachments, cluster(0, b"a"), cluster(1000, b"b")]);
    let from = Arc::new(AtomicU64::new(u64::MAX));
    let input = Box::new(Dropping { inner: Cursor::new(bytes), from: from.clone() });
    let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
    let first = d.next_packet().map(|p| p.data).map_err(kind);
    from.store(d.attachments()[0].data_offset, Ordering::SeqCst);
    let fetched = d.attachment_data(1);
    from.store(u64::MAX, Ordering::SeqCst);
    let own = matches!(&fetched, Err(Error::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof);
    let next = d.next_packet().map(|p| p.data).map_err(kind);
    let again = d.attachment_data(1).map(|data| data == [7; 64]).map_err(kind);
    assert!(
        first == Ok(b"a".to_vec()) && own && next == Ok(b"b".to_vec()) && again == Ok(true),
        "first {first:?}, fetch {:?}, next {next:?}, again {again:?}",
        fetched.map(|data| data.len()),
    );
}

/// How far the open's resync scan reads past junk before it gives up.
const RESYNC: usize = 1 << 20;

/// Junk where a Top-Level element should start, before the first Cluster,
/// is skipped by either open: the walk scans for the next Top-Level
/// element, as FFmpeg's `matroska_resync` does for `matroska_read_header`,
/// notes each skipped run as one damage event, and every packet plays.
#[test]
fn junk_before_the_first_cluster_is_skipped_by_either_open() {
    let _serial = serial();
    let mut failures = Vec::new();
    let junk = vec![0u8; 1000];
    let clusters = vec![cluster(0, b"a"), cluster(1000, b"b")];
    let layouts = [
        ("before Tracks", vec![junk.clone(), tracks()], 1),
        ("before the first Cluster", vec![tracks(), junk.clone()], 1),
        ("before both", vec![junk.clone(), tracks(), junk.clone()], 2),
    ];
    for (layout, head, runs) in layouts {
        let bytes = file(&[head, clusters.clone()].concat());
        for resilient in [false, true] {
            let got = played(bytes.clone(), resilient).map(|got| (got.packets, got.damage));
            let expected = (vec![b"a".to_vec(), b"b".to_vec()], vec![DamageKind::GarbageData; runs]);
            if got.as_ref() != Ok(&expected) {
                failures.push(format!("{layout}, resilient {resilient}: {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A run of junk longer than the resync scan's budget fails a strict open
/// once the budget is spent, and neither open reads much past it.
#[test]
fn junk_past_the_resync_budget_fails_a_strict_open_after_a_bounded_read() {
    let _serial = serial();
    let bytes = file(&[tracks(), vec![0; 4 << 20], cluster(0, b"a")]);
    let (strict, strict_read, _) = measure(bytes.clone(), |input| first(input, false));
    let (resilient, resilient_read, _) = measure(bytes, |input| {
        demux::open_resilient_typed(input, &NullCodecResolver).map(|_| ()).map_err(kind)
    });
    let bound = RESYNC + (128 << 10);
    assert!(
        strict == Err("InvalidData".to_string()) && strict_read < bound && resilient.is_ok() && resilient_read < bound,
        "strict {strict:?} after reading {strict_read} bytes; resilient open {resilient:?} after reading {resilient_read} bytes",
    );
}

/// An optional master whose declared size runs past its Segment does not
/// hide the Clusters behind it: it is damage, and the walk rescans from the
/// end of its header.
#[test]
fn an_optional_master_past_its_segment_does_not_hide_the_clusters() {
    let _serial = serial();
    let mut failures = Vec::new();
    for (name, id, child) in [
        ("Cues", ids::CUES, ids::CUE_POINT),
        ("Chapters", ids::CHAPTERS, ids::EDITION_ENTRY),
        ("Attachments", ids::ATTACHMENTS, ids::ATTACHED_FILE),
        ("Tags", ids::TAGS, ids::TAG),
    ] {
        // 4096 octets declared, a child declaring 8192, then the Cluster:
        // the Segment ends right after it.
        let master = [header(id, 4096), header(child, 8192)].concat();
        let bytes = file(&[tracks(), master, cluster(0, b"a")]);
        for resilient in [false, true] {
            let got = played(bytes.clone(), resilient).map(|got| (got.packets, got.damage));
            let expected = (vec![b"a".to_vec()], vec![DamageKind::DamagedMaster(id)]);
            if got.as_ref() != Ok(&expected) {
                failures.push(format!("{name}, resilient {resilient}: {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The four optional masters of the Segment-end tests: name, ID and the ID
/// of a child each may hold.
const OPTIONAL: [(&str, u32, u32); 4] = [
    ("Cues", ids::CUES, ids::CUE_POINT),
    ("Chapters", ids::CHAPTERS, ids::EDITION_ENTRY),
    ("Attachments", ids::ATTACHMENTS, ids::ATTACHED_FILE),
    ("Tags", ids::TAGS, ids::TAG),
];

/// Between Clusters too, an optional master whose declared end runs past
/// its Segment is damage: the walk resynchronises on the next Cluster
/// instead of skipping past the end of the Segment.
#[test]
fn an_optional_master_past_its_segment_between_clusters_is_damage() {
    let _serial = serial();
    let mut failures = Vec::new();
    for (name, id, child) in OPTIONAL {
        let master = [header(id, 4096), header(child, 8192)].concat();
        let bytes = file(&[tracks(), cluster(0, b"a"), master, cluster(1000, b"b")]);
        for resilient in [false, true] {
            let got = played(bytes.clone(), resilient).map(|got| (got.packets, got.damage.len()));
            if got != Ok((vec![b"a".to_vec(), b"b".to_vec()], 1)) {
                failures.push(format!("{name}, resilient {resilient}: {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Where a seek to 1000 ms lands and the packet it gives.
type Landing = Result<(i64, Vec<u8>), String>;

/// A seek to 1000 ms straight after the open, then another once every
/// packet is drained.
fn seeks_around_a_drain(bytes: Vec<u8>, resilient: bool) -> Result<[Landing; 2], String> {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut d = if resilient {
        demux::open_resilient_typed(input, &NullCodecResolver)
    } else {
        demux::open_typed(input, &NullCodecResolver)
    }.map_err(kind)?;
    let before = seek_and_read(&mut d, 0, 1000);
    while d.next_packet().is_ok() {}
    Ok([before, seek_and_read(&mut d, 0, 1000)])
}

/// A seek scanning the Clusters steps past an optional master whose
/// declared end runs past its Segment as playback does: it rescans from
/// the end of the master's header, so the Cluster behind it is found
/// before the packets are drained and after, without Cues or past a Cues
/// index cut short. The keyframe search rescans the same way, so it finds
/// a keyframe in a Cluster behind the master whose Timestamp the scan
/// could not read.
#[test]
fn seeks_step_past_an_optional_master_past_its_segment() {
    let _serial = serial();
    let mut failures = Vec::new();
    let t = tracks();
    let (a, b) = (cluster(0, b"a"), cluster(1000, b"b"));
    // Cues whose second CuePoint runs past them: the index keeps only the
    // CuePoint at 0, for the first Cluster at Segment Position `first`.
    let cut = |first: u64| elem(ids::CUES, &[cue_point(0, first, false), header(ids::CUE_POINT, 1 << 20)].concat());
    let first = (t.len() + cut(0).len()) as u64;
    // A Cluster without a Timestamp: a delta frame at 900 ms, then a
    // keyframe at 1000 ms on track 1.
    let untimed = elem(ids::CLUSTER, &[
        elem(ids::SIMPLE_BLOCK, &[0x81, 0x03, 0x84, 0x00, b'x']),
        elem(ids::SIMPLE_BLOCK, &[0x81, 0x03, 0xE8, 0x80, b'b']),
    ].concat());
    let expected = Ok([Ok((1000, b"b".to_vec())), Ok((1000, b"b".to_vec()))]);
    for (name, id, child) in OPTIONAL {
        let master = [header(id, 4096), header(child, 8192)].concat();
        let layouts = [
            ("no Cues", file(&[t.clone(), a.clone(), master.clone(), b.clone()])),
            ("cut Cues", file(&[t.clone(), cut(first), a.clone(), master.clone(), b.clone()])),
            ("an untimed Cluster", file(&[t.clone(), a.clone(), master, untimed.clone()])),
        ];
        for (layout, bytes) in layouts {
            for resilient in [false, true] {
                let got = seeks_around_a_drain(bytes.clone(), resilient);
                if got != expected {
                    failures.push(format!("{name}, {layout}, resilient {resilient}: {got:?}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A Cluster `tc` whose first child is a Void, holding packet `payload`.
fn void_led_cluster(tc: u64, payload: &[u8]) -> Vec<u8> {
    let block = elem(ids::SIMPLE_BLOCK, &[&[0x81, 0, 0, 0x80][..], payload].concat());
    elem(ids::CLUSTER, &[void(2), uint(ids::TIMECODE, tc), block].concat())
}

/// RFC 8794 §11.3.2 allows a Void anywhere, so a Cluster may start with
/// one: recovery past junk and a seek by scanning the Clusters still find
/// it.
#[test]
fn a_cluster_led_by_a_void_is_found_by_recovery_and_seeks() {
    let _serial = serial();
    let mut failures = Vec::new();
    let recovered = file(&[tracks(), vec![0; 1000], void_led_cluster(0, b"a"), cluster(1000, b"b")]);
    for resilient in [false, true] {
        let got = played(recovered.clone(), resilient).map(|got| (got.packets, got.damage));
        if got != Ok((vec![b"a".to_vec(), b"b".to_vec()], vec![DamageKind::GarbageData])) {
            failures.push(format!("junk, then a Void-led Cluster, resilient {resilient}: {got:?}"));
        }
    }
    // No Cues: a seek to 500 ms scans the Clusters and lands on the first.
    let seeking = file(&[tracks(), void_led_cluster(0, b"a"), cluster(1000, b"b")]);
    let mut d = demux::open_typed(Box::new(Cursor::new(seeking)), &NullCodecResolver).unwrap();
    let landed = seek_and_read(&mut d, 0, 500);
    if landed != Ok((0, b"a".to_vec())) {
        failures.push(format!("seek to 500 ms: {landed:?}"));
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A one-Seek SeekHead pointing at `id` at Segment Position `position`.
fn seek(id: u32, position: u64) -> Vec<u8> {
    elem(ids::SEEK, &[elem(ids::SEEK_ID, &write_element_id(id)), uint(ids::SEEK_POSITION, position)].concat())
}

/// What an open keeps straight away: how many Tags, the damage it noted and
/// the first packet.
fn tags_kept(bytes: Vec<u8>, resilient: bool) -> Result<(usize, Vec<DamageKind>, Vec<u8>), String> {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut d = if resilient {
        demux::open_resilient_typed(input, &NullCodecResolver)
    } else {
        demux::open_typed(input, &NullCodecResolver)
    }.map_err(kind)?;
    let tags = d.tags().len();
    let damage = d.damage_events().iter().map(|e| e.kind()).collect();
    Ok((tags, damage, d.next_packet().map(|p| p.data).map_err(kind)?))
}

/// A Tags master or a SeekHead found through the SeekHead keeps the
/// complete records before its damage, as one in line does, and a Seek
/// kept that way is still followed.
#[test]
fn seekhead_followed_tags_and_seekheads_keep_their_complete_records() {
    let _serial = serial();
    let mut failures = Vec::new();
    let (t, c) = (tracks(), cluster(0, b"a"));
    let tag = |value: Vec<u8>| elem(ids::TAG, &elem(ids::SIMPLE_TAG, &[elem(ids::TAG_NAME, b"T"), value].concat()));
    let good = tag(elem(ids::TAG_STRING, b"v"));
    // A complete Tag, then one whose TagString runs past its SimpleTag.
    let damaged = elem(ids::TAGS, &[good.clone(), tag(header(ids::TAG_STRING, 1 << 20))].concat());
    let tags_at = (index(ids::TAGS, 0).len() + t.len() + c.len()) as u64;
    // A second SeekHead whose first Seek leads to the Tags after it and
    // whose second Seek's SeekPosition runs past it.
    let first_len = index(ids::SEEK_HEAD, 0).len();
    let second_at = (first_len + t.len() + c.len()) as u64;
    let second_len = elem(ids::SEEK_HEAD, &[seek(ids::TAGS, 0), elem(ids::SEEK, &header(ids::SEEK_POSITION, 1 << 20))].concat()).len();
    let second = elem(ids::SEEK_HEAD, &[
        seek(ids::TAGS, second_at + second_len as u64),
        elem(ids::SEEK, &header(ids::SEEK_POSITION, 1 << 20)),
    ].concat());
    let layouts = [
        ("Tags in line", file(&[t.clone(), damaged.clone(), c.clone()]), ids::TAGS),
        ("Tags followed", file(&[index(ids::TAGS, tags_at), t.clone(), c.clone(), damaged]), ids::TAGS),
        (
            "SeekHead followed",
            file(&[index(ids::SEEK_HEAD, second_at), t, c, second, elem(ids::TAGS, &good)]),
            ids::SEEK_HEAD,
        ),
    ];
    for (layout, bytes, id) in layouts {
        for resilient in [false, true] {
            let got = tags_kept(bytes.clone(), resilient);
            if got != Ok((1, vec![DamageKind::DamagedMaster(id)], b"a".to_vec())) {
                failures.push(format!("{layout}, resilient {resilient}: {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Only a Segment or a Cluster may leave its size open: an AttachedFile or
/// a FileData of unknown size is damage, so the Attachments keep the
/// attachments before it and the open goes on.
#[test]
fn unknown_size_attached_files_and_payloads_are_damage() {
    let _serial = serial();
    let mut failures = Vec::new();
    let valid = attached_file(1, b"one.ttf", b"d");
    let unsized_file = [unknown_size(ids::ATTACHED_FILE), elem(ids::FILE_NAME, b"two.ttf"), uint(ids::FILE_UID, 2)].concat();
    let unsized_data = elem(ids::ATTACHED_FILE, &[
        elem(ids::FILE_NAME, b"two.ttf"), uint(ids::FILE_UID, 2), unknown_size(ids::FILE_DATA), b"dd".to_vec(),
    ].concat());
    for (case, broken) in [("AttachedFile", unsized_file), ("FileData", unsized_data)] {
        let bytes = file(&[tracks(), elem(ids::ATTACHMENTS, &[valid.clone(), broken].concat()), cluster(0, b"a")]);
        for resilient in [false, true] {
            let got = played(bytes.clone(), resilient).map(|got| (got.packets, got.attachments, got.damage));
            let expected = (vec![b"a".to_vec()], vec![(1, 1)], vec![DamageKind::DamagedMaster(ids::ATTACHMENTS)]);
            if got.as_ref() != Ok(&expected) {
                failures.push(format!("{case} of unknown size, resilient {resilient}: {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// An AttachedFile dropped as damage takes nothing from the attachments
/// kept before it: one reusing a kept attachment's FileUID leaves that UID
/// naming the kept attachment, so a Tag targeting the UID still resolves
/// to it.
#[test]
fn a_dropped_attachment_leaves_the_uids_of_the_kept_ones() {
    let _serial = serial();
    let mut failures = Vec::new();
    let reused = elem(ids::ATTACHED_FILE, &[
        elem(ids::FILE_NAME, b"two.ttf"), uint(ids::FILE_UID, 1), unknown_size(ids::FILE_DATA), b"dd".to_vec(),
    ].concat());
    let attachments = elem(ids::ATTACHMENTS, &[attached_file(1, b"one.ttf", b"d"), reused].concat());
    let tagged = elem(ids::TAGS, &elem(ids::TAG, &[
        elem(ids::TARGETS, &uint(ids::TAG_ATTACHMENT_UID, 1)),
        elem(ids::SIMPLE_TAG, &[elem(ids::TAG_NAME, b"T"), elem(ids::TAG_STRING, b"v")].concat()),
    ].concat()));
    let bytes = file(&[tracks(), attachments, tagged, cluster(0, b"a")]);
    let target = TargetUid::Attachment { attachment_index: 1, attachment_uid: 1 };
    let expected = (vec![(1, 1)], vec![vec![target]], vec![DamageKind::DamagedMaster(ids::ATTACHMENTS)], b"a".to_vec());
    for resilient in [false, true] {
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
        let opened = if resilient {
            demux::open_resilient_typed(input, &NullCodecResolver)
        } else {
            demux::open_typed(input, &NullCodecResolver)
        };
        let got = opened.map_err(kind).and_then(|mut d| {
            let attachments: Vec<_> = d.attachments().iter().map(|a| (a.index, a.uid)).collect();
            let targets: Vec<_> = d.tags().iter().map(|t| t.targets.uids.clone()).collect();
            let damage: Vec<_> = d.damage_events().iter().map(|e| e.kind()).collect();
            Ok((attachments, targets, damage, d.next_packet().map_err(kind)?.data))
        });
        if got.as_ref() != Ok(&expected) {
            failures.push(format!("resilient {resilient}: {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The EBML header keeps its strings and DocTypeExtensions within 16 MiB,
/// FFmpeg's limit for one EBML string, and each of its children must fit
/// its parent, a DocTypeExtension's children included: past either, both
/// opens fail after a bounded read, holding little. A header of unknown
/// size is refused before any of its children is read. An ordinary header
/// keeps its extension.
#[test]
fn the_ebml_header_keeps_its_strings_and_extensions_within_16_mib() {
    let _serial = serial();
    let mut failures = Vec::new();
    // The Segment carries 2 MiB of Void, so a header child running past
    // its parent has bytes to read.
    let segment = {
        let body = [tracks(), cluster(0, b"a"), void(2 << 20)].concat();
        [header(ids::SEGMENT, body.len()), body].concat()
    };
    let with_header = |children: Vec<u8>| [elem(ids::EBML_HEADER, &children), segment.clone()].concat();
    let doc_type = elem(ids::EBML_DOC_TYPE, b"matroska");
    let extension = |name: &[u8]| {
        elem(ids::DOC_TYPE_EXTENSION, &[elem(ids::DOC_TYPE_EXTENSION_NAME, name), uint(ids::DOC_TYPE_EXTENSION_VERSION, 1)].concat())
    };
    let padded = elem(ids::EBML_DOC_TYPE, &[&b"matroska"[..], &vec![0; BIG]].concat());
    let long_name = [doc_type.clone(), extension(&vec![b'x'; BIG])].concat();
    // A million extensions of one-octet names: 11 MB on disk.
    let many = [doc_type.clone(), extension(b"x").repeat(1_000_000)].concat();
    // A DocType declaring 1 MiB in a header that holds its first 8 octets.
    let short = [header(ids::EBML_DOC_TYPE, 1 << 20), b"matroska".to_vec()].concat();
    // An extension holding the header of a 1 MiB name and 8 octets of it;
    // the rest of the name follows inside the EBML header.
    let name = header(ids::DOC_TYPE_EXTENSION_NAME, 1 << 20);
    let crossing = [doc_type.clone(), header(ids::DOC_TYPE_EXTENSION, name.len() + 8), name, vec![b'x'; 1 << 20]].concat();
    let unsized_header = [unknown_size(ids::EBML_HEADER), doc_type.clone(), segment.clone()].concat();
    // Each case with the most the open may read: what is refused is not
    // read at all, of the million extensions (18 MB on disk) only those
    // read before their budget runs out, about a quarter, and of a header
    // of unknown size its ID and size alone.
    let unread = 64 << 10;
    let cases = [
        ("a 48 MiB padded DocType", with_header(padded), unread),
        ("a 48 MiB extension name", with_header(long_name), unread),
        ("a million extensions", with_header(many), 8 << 20),
        ("a DocType past its header", with_header(short), unread),
        ("an extension name past its extension", with_header(crossing), unread),
        ("a header of unknown size", unsized_header, unknown_size(ids::EBML_HEADER).len() + 1),
    ];
    for (case, bytes, bound) in cases {
        for resilient in [false, true] {
            let (outcome, read, peak) = measure(bytes.clone(), |input| first(input, resilient));
            if outcome != Err("InvalidData".to_string()) || read >= bound || peak >= (16 << 20) + (1 << 20) {
                failures.push(format!("{case}, resilient {resilient}: {outcome:?} after reading {read} bytes, peak {peak} heap bytes"));
            }
        }
    }
    let ordinary = with_header([doc_type, extension(b"ext")].concat());
    let opened = demux::open_typed(Box::new(Cursor::new(ordinary)), &NullCodecResolver).map_err(kind);
    let extensions = opened.map(|d| {
        d.ebml_header().doc_type_extensions.iter().map(|e| (e.name.clone(), e.version)).collect::<Vec<_>>()
    });
    if extensions != Ok(vec![("ext".to_string(), 1)]) {
        failures.push(format!("an ordinary header: {extensions:?}"));
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
