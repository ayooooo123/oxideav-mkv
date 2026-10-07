//! The records the open builds from a `Tags` or `Tracks` master, tag
//! resolution and the per-stream views included, stay within the master's
//! 32 MiB limit at their peak and once the open is done, measured on
//! masters at that full limit. A CodecPrivate replaced by its decoded form
//! gives its room back, and so do the flat entries of a Tags master
//! replaced between Clusters, in one pass.
//!
//! The heap is measured by a counting global allocator that refuses to go
//! past 512 MiB live, so an unbounded parse aborts the test binary instead
//! of exhausting the machine. Every test holds one lock, so nothing else
//! allocates while one measures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use oxideav_core::{Demuxer, Error, NullCodecResolver};
use oxideav_mkv::demux::{self, DamageKind, MkvDemuxer, SimpleTagValue};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

struct Tracking;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static SERIAL: Mutex<()> = Mutex::new(());
/// The most this test binary may hold live.
const CEILING: usize = 512 << 20;

// SAFETY: every call forwards to `System` with the caller's arguments, or
// refuses an allocation past `CEILING` by returning null; the counters only
// observe the sizes.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
        if live > CEILING {
            LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
            return std::ptr::null_mut();
        }
        let p = unsafe { System.alloc(layout) };
        if p.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        } else {
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

/// The `Tags` and `Tracks` master limit.
const LIMIT: usize = 32 << 20;
/// What the rest of the demuxer may hold besides the master's records.
const SLACK: usize = 1 << 20;

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

/// The children of subtitle track `n`.
fn track_fields(n: u64) -> Vec<u8> {
    [
        uint(ids::TRACK_NUMBER, n), uint(ids::TRACK_UID, n),
        uint(ids::TRACK_TYPE, 0x11), elem(ids::CODEC_ID, b"S_TEXT/UTF8"),
    ].concat()
}

fn tracks() -> Vec<u8> {
    elem(ids::TRACKS, &elem(ids::TRACK_ENTRY, &track_fields(1)))
}

/// A Cluster at 0 holding one keyframe packet on track 1.
fn cluster() -> Vec<u8> {
    let block = elem(ids::SIMPLE_BLOCK, &[0x81, 0, 0, 0x80, b'a']);
    elem(ids::CLUSTER, &[uint(ids::TIMECODE, 0), block].concat())
}

/// The open of `bytes`, its peak heap and the heap it holds once done,
/// both beyond what was live before.
fn opened(bytes: Vec<u8>, resilient: bool) -> (Result<MkvDemuxer, String>, usize, usize) {
    let input = Box::new(Cursor::new(bytes));
    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let d = if resilient {
        demux::open_resilient_typed(input, &NullCodecResolver)
    } else {
        demux::open_typed(input, &NullCodecResolver)
    };
    let peak = PEAK.load(Ordering::SeqCst) - base;
    let held = LIVE.load(Ordering::SeqCst).saturating_sub(base);
    let d = d.map_err(|e| match e {
        Error::InvalidData(_) => "InvalidData".to_string(),
        other => format!("{other}"),
    });
    (d, peak, held)
}

/// A Tags master as large as the limit allows: Tags of 64 SimpleTags each
/// naming "x" with the string "y", eleven bytes apiece on disk.
fn full_tags() -> Vec<u8> {
    let simple = elem(ids::SIMPLE_TAG, &[elem(ids::TAG_NAME, b"x"), elem(ids::TAG_STRING, b"y")].concat());
    let tag = elem(ids::TAG, &simple.repeat(64));
    elem(ids::TAGS, &tag.repeat(LIMIT / tag.len()))
}

#[test]
fn tags_stay_within_their_limit() {
    let _serial = serial();
    let mut failures = Vec::new();
    let bytes = file(&[tracks(), full_tags(), cluster()]);
    // Either open keeps the Tags that fit; the rest is damage.
    for resilient in [false, true] {
        let (d, peak, held) = opened(bytes.clone(), resilient);
        match d {
            Ok(mut d) => {
                let kept = d.tags().len();
                let damaged = d.damage_events().iter().any(|e| e.kind() == DamageKind::DamagedMaster(ids::TAGS));
                let played = d.next_packet().map(|p| p.data).ok();
                if kept == 0 || !damaged || played != Some(b"a".to_vec()) || peak > LIMIT + SLACK || held > LIMIT + SLACK {
                    failures.push(format!(
                        "full Tags, resilient {resilient}: kept {kept} Tags, damaged {damaged}, played {played:?}, peak {peak}, held {held} heap bytes"
                    ));
                }
            }
            Err(e) => failures.push(format!("full Tags, resilient {resilient}: {e}, peak {peak} heap bytes")),
        }
    }
    // A thousand Tags of four 256-byte values each fit and are kept whole.
    let value = vec![b'v'; 256];
    let simple = elem(ids::SIMPLE_TAG, &[elem(ids::TAG_NAME, b"NAME"), elem(ids::TAG_STRING, &value)].concat());
    let tags = elem(ids::TAGS, &elem(ids::TAG, &simple.repeat(4)).repeat(1000));
    let (d, peak, held) = opened(file(&[tracks(), tags, cluster()]), false);
    let kept = d.as_ref().map(|d| d.tags().len());
    if kept != Ok(1000) || peak > LIMIT + SLACK || held > LIMIT + SLACK {
        failures.push(format!("1000 Tags: {kept:?}, peak {peak}, held {held} heap bytes"));
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A Tracks master as large as the limit allows: a subtitle track, then a
/// second whose remaining bytes are `child` repeated.
fn full_tracks(child: &[u8]) -> Vec<u8> {
    let first = elem(ids::TRACK_ENTRY, &track_fields(1));
    let fields = track_fields(2);
    let room = LIMIT - first.len() - fields.len() - 16;
    let second = elem(ids::TRACK_ENTRY, &[fields, child.repeat(room / child.len())].concat());
    elem(ids::TRACKS, &[first, second].concat())
}

/// TrackEntry `n` with 64 KiB of CodecPrivate and a BlockAdditionMapping
/// holding `extra` octets of BlockAddIDExtraData.
fn filled_entry(n: u64, extra: usize) -> Vec<u8> {
    let mapping = elem(ids::BLOCK_ADDITION_MAPPING, &elem(ids::BLOCK_ADD_ID_EXTRA_DATA, &vec![2; extra]));
    elem(ids::TRACK_ENTRY, &[track_fields(n), elem(ids::CODEC_PRIVATE, &vec![1; 64 << 10]), mapping].concat())
}

#[test]
fn tracks_stay_within_their_limit() {
    let _serial = serial();
    let mut failures = Vec::new();
    // Children that cost the most to keep per byte on disk: empty
    // BlockAdditionMapping masters, one ContentEncodings of empty
    // ContentEncoding masters, and TrackOverlay numbers.
    let mappings = full_tracks(&elem(ids::BLOCK_ADDITION_MAPPING, &[]));
    let encodings = {
        let first = elem(ids::TRACK_ENTRY, &track_fields(1));
        let fields = track_fields(2);
        let one = elem(ids::CONTENT_ENCODING, &[]);
        let room = LIMIT - first.len() - fields.len() - 32;
        let list = elem(ids::CONTENT_ENCODINGS, &one.repeat(room / one.len()));
        elem(ids::TRACKS, &[first, elem(ids::TRACK_ENTRY, &[fields, list].concat())].concat())
    };
    let overlays = full_tracks(&elem(ids::TRACK_OVERLAY, &[1]));
    for (name, master) in [("BlockAdditionMappings", mappings), ("ContentEncodings", encodings), ("TrackOverlays", overlays)] {
        let bytes = file(&[master, cluster()]);
        let (strict, peak, _) = opened(bytes.clone(), false);
        if strict.as_ref().err().map(String::as_str) != Some("InvalidData") || peak > LIMIT + SLACK {
            failures.push(format!("{name}, strict: {:?}, peak {peak} heap bytes", strict.as_ref().map(|_| ())));
        }
        drop(strict);
        // Resilient: the second track is damage; the first plays.
        let (d, peak, held) = opened(bytes, true);
        let played = d.map(|mut d| (d.streams().len(), d.next_packet().map(|p| p.data).ok()));
        if played != Ok((1, Some(b"a".to_vec()))) || peak > LIMIT + SLACK || held > LIMIT + SLACK {
            failures.push(format!("{name}, resilient: {played:?}, peak {peak}, held {held} heap bytes"));
        }
    }
    // 256 tracks of 64 KiB CodecPrivate each, the whole CodecPrivate
    // budget, and a 24 KiB BlockAddIDExtraData each: kept whole.
    let entries: Vec<u8> = (1..=256u64).flat_map(|n| filled_entry(n, 24 << 10)).collect();
    let (d, peak, held) = opened(file(&[elem(ids::TRACKS, &entries), cluster()]), false);
    let kept = d.as_ref().map(|d| {
        let private = d.streams().iter().filter(|s| s.params.extradata.len() == 64 << 10).count();
        let extra = d
            .all_block_addition_mappings()
            .iter()
            .filter(|m| m.first().and_then(|m| m.extra_data.as_ref()).map(Vec::len) == Some(24 << 10))
            .count();
        (private, extra)
    });
    if kept != Ok((256, 256)) || peak > LIMIT + SLACK || held > LIMIT + SLACK {
        failures.push(format!("256 tracks of 88 KiB: {kept:?}, peak {peak}, held {held} heap bytes"));
    }
    // A 4 MiB H.264 CodecPrivate in Annex B form, packed with start codes or
    // holding one SPS NAL unit: parsing the codec configuration needs
    // working room besides the bytes kept.
    let packed = [0u8, 0, 1].repeat((4 << 20) / 3);
    let sps = [&[0u8, 0, 1, 0x67][..], &vec![0x42; (4 << 20) - 4]].concat();
    for (name, private) in [("packed start codes", packed), ("one SPS", sps)] {
        let fields = [
            uint(ids::TRACK_NUMBER, 1), uint(ids::TRACK_UID, 1), uint(ids::TRACK_TYPE, 1),
            elem(ids::CODEC_ID, b"V_MPEG4/ISO/AVC"), elem(ids::CODEC_PRIVATE, &private),
        ].concat();
        let (d, peak, held) = opened(file(&[elem(ids::TRACKS, &elem(ids::TRACK_ENTRY, &fields)), cluster()]), false);
        let kept = d.as_ref().map(|d| d.streams()[0].params.extradata.len());
        if kept != Ok(private.len()) || peak > LIMIT + SLACK || held > LIMIT + SLACK {
            failures.push(format!("H.264 CodecPrivate of {name}: {kept:?}, peak {peak}, held {held} heap bytes"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A CodecPrivate compressed with LZO is decompressed only as far as its
/// budget allows, so a Tracks master near its limit cannot grow past it
/// through the decompressor's output.
#[test]
fn an_lzo_codec_private_stays_within_the_tracks_limit() {
    let _serial = serial();
    // 255 tracks with 64 KiB of CodecPrivate and 40 KiB of extra data each
    // hold most of the Tracks budget, and all but 64 KiB of the
    // CodecPrivate budget.
    let mut entries: Vec<u8> = (1..=255u64).flat_map(|n| filled_entry(n, 40 << 10)).collect();
    // Track 256 keeps 10 MB of zeros, which LZO packs into about 39 KB.
    let mut packed = Vec::new();
    compcol::lzo::block::encode_block(&vec![0; 10_000_000], &mut packed);
    let compression = elem(ids::CONTENT_COMPRESSION, &uint(ids::CONTENT_COMP_ALGO, ids::CONTENT_COMP_ALGO_LZO1X));
    let encoding = elem(ids::CONTENT_ENCODING, &[uint(ids::CONTENT_ENCODING_SCOPE, ids::CONTENT_ENCODING_SCOPE_PRIVATE), compression].concat());
    entries.extend(elem(ids::TRACK_ENTRY, &[
        track_fields(256), elem(ids::CONTENT_ENCODINGS, &encoding), elem(ids::CODEC_PRIVATE, &packed),
    ].concat()));
    let (d, peak, _) = opened(file(&[elem(ids::TRACKS, &entries), cluster()]), false);
    let outcome = d.as_ref().map(|_| ()).map_err(String::as_str);
    assert!(
        outcome == Err("InvalidData") && peak <= LIMIT + SLACK,
        "{} bytes of LZO: {outcome:?}, peak {peak} heap bytes",
        packed.len(),
    );
}

/// Four CodecPrivates of 4 MiB less one octet, each restored to 4 MiB by
/// header stripping, make up the 16 MiB CodecPrivate total exactly. Their
/// decoding may use the working room kept for parsing a codec
/// configuration, and a stored buffer's charge passes to its decoded form,
/// so all four open byte for byte in either open, as the only tracks or
/// among 256, every stream kept. Two octets more are past the total.
#[test]
fn codec_privates_restored_to_their_16_mib_total_open() {
    let _serial = serial();
    let stripping = |prefix: &[u8]| {
        let compression = elem(ids::CONTENT_COMPRESSION, &[
            uint(ids::CONTENT_COMP_ALGO, ids::CONTENT_COMP_ALGO_HEADER_STRIPPING), elem(ids::CONTENT_COMP_SETTINGS, prefix),
        ].concat());
        let encoding = elem(ids::CONTENT_ENCODING, &[uint(ids::CONTENT_ENCODING_SCOPE, ids::CONTENT_ENCODING_SCOPE_PRIVATE), compression].concat());
        elem(ids::CONTENT_ENCODINGS, &encoding)
    };
    let entry = |n: u64, private: &[u8]| {
        elem(ids::TRACK_ENTRY, &[track_fields(n), stripping(&[n as u8]), elem(ids::CODEC_PRIVATE, private)].concat())
    };
    let stored: Vec<u8> = (0..(4u32 << 20) - 1).map(|i| (i % 251) as u8).collect();
    let four: Vec<u8> = (1..=4u64).flat_map(|n| entry(n, &stored)).collect();
    let many = [four.clone(), (5..=256u64).flat_map(|n| elem(ids::TRACK_ENTRY, &track_fields(n))).collect()].concat();
    let mut failures = Vec::new();
    for (case, entries, count) in [("four tracks", &four, 4), ("256 tracks", &many, 256)] {
        for resilient in [false, true] {
            let (d, _, _) = opened(file(&[elem(ids::TRACKS, entries), cluster()]), resilient);
            let got = d.map(|mut d| {
                let streams = d.streams();
                let restored = streams.iter().take(4).zip(1u8..).all(|(s, n)| {
                    s.params.extradata.first() == Some(&n) && s.params.extradata.get(1..) == Some(&stored[..])
                });
                (streams.len(), restored, d.next_packet().map(|p| p.data).ok())
            });
            if got != Ok((count, true, Some(b"a".to_vec()))) {
                failures.push(format!("{case}, resilient {resilient}: (streams, first four restored, first packet) {got:?}"));
            }
        }
    }
    let five = [four, entry(5, &[5])].concat();
    let (d, _, _) = opened(file(&[elem(ids::TRACKS, &five), cluster()]), false);
    if d.as_ref().err().map(String::as_str) != Some("InvalidData") {
        failures.push(format!("a fifth past the 16 MiB total: {:?}", d.as_ref().map(|_| ())));
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A Cluster at `tc` holding packet `payload` on track 1.
fn cluster_at(tc: u64, payload: &[u8]) -> Vec<u8> {
    let block = elem(ids::SIMPLE_BLOCK, &[&[0x81, 0, 0, 0x80][..], payload].concat());
    elem(ids::CLUSTER, &[uint(ids::TIMECODE, tc), block].concat())
}

/// A SimpleTag naming `name` with the TagString or TagBinary `value`.
fn simple_tag(name: &[u8], value: Vec<u8>) -> Vec<u8> {
    elem(ids::SIMPLE_TAG, &[elem(ids::TAG_NAME, name), value].concat())
}

/// Every packet of `d` to the end.
fn drained(d: &mut MkvDemuxer) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => packets.push(p.data),
            Err(Error::Eof) => return packets,
            Err(e) => panic!("unexpected error after {} packets: {e}", packets.len()),
        }
    }
}

/// A Tags master replacing another between Clusters gives back the room
/// the old one's flat entries held before their charge is released: after
/// forty thousand entries, an empty Tags and then one 31.5 MiB value, the
/// tags hold no more than their 32 MiB limit.
#[test]
fn replaced_tags_give_back_the_room_their_entries_held() {
    let _serial = serial();
    let many = elem(ids::TAGS, &elem(ids::TAG, &simple_tag(b"x", elem(ids::TAG_STRING, b"y")).repeat(64)).repeat(625));
    let value = vec![7u8; (63 << 20) / 2];
    let large = elem(ids::TAGS, &elem(ids::TAG, &simple_tag(b"BLOB", elem(ids::TAG_BINARY, &value))));
    let bytes = file(&[
        tracks(), cluster_at(0, b"a"), many, cluster_at(1000, b"b"), elem(ids::TAGS, &[]),
        cluster_at(2000, b"c"), large, cluster_at(3000, b"d"),
    ]);
    let mut d = opened(bytes, false).0.unwrap();
    let base = LIVE.load(Ordering::SeqCst);
    let packets = drained(&mut d);
    let held = LIVE.load(Ordering::SeqCst).saturating_sub(base);
    let kept = d.tags().first().and_then(|t| t.simple_tags.first()).map(|s| match &s.value {
        SimpleTagValue::Binary(b) => b.len(),
        _ => 0,
    });
    let order = [b"a", b"b", b"c", b"d"].map(|p| p.to_vec());
    assert!(
        packets == order && kept == Some(value.len()) && held <= LIMIT,
        "{} packets, kept a value of {kept:?} octets, held {held} heap bytes",
        packets.len(),
    );
}

/// Replacing fifty thousand flat tag entries takes one pass, not one per
/// entry, and keeps exactly the Info entry they duplicate.
#[test]
fn a_tags_reset_replaces_its_entries_in_one_pass() {
    let _serial = serial();
    let info = elem(ids::INFO, &[uint(ids::TIMECODE_SCALE, 1_000_000), elem(ids::TITLE, b"y")].concat());
    let many = elem(ids::TAGS, &elem(ids::TAG, &simple_tag(b"TITLE", elem(ids::TAG_STRING, b"y")).repeat(50)).repeat(1000));
    let one = elem(ids::TAGS, &elem(ids::TAG, &simple_tag(b"Z", elem(ids::TAG_STRING, b"w"))));
    let bytes = file(&[info, tracks(), cluster_at(0, b"a"), many, cluster_at(1000, b"b"), one, cluster_at(2000, b"c")]);
    let mut d = opened(bytes, false).0.unwrap();
    let first = [d.next_packet(), d.next_packet()].map(|p| p.map(|p| p.data).ok());
    let titles_before = d.metadata().iter().filter(|(k, v)| k == "title" && v == "y").count();
    let started = Instant::now();
    let last = d.next_packet().map(|p| p.data).ok();
    let took = started.elapsed();
    let count = |key: &str, value: &str| d.metadata().iter().filter(|(k, v)| k == key && v == value).count();
    let (titles, zs) = (count("title", "y"), count("z", "w"));
    assert!(
        first == [Some(b"a".to_vec()), Some(b"b".to_vec())] && last == Some(b"c".to_vec())
            && titles_before == 50_001 && titles == 1 && zs == 1 && took < Duration::from_secs(1),
        "packets {first:?} {last:?}; title entries {titles_before} then {titles}, z entries {zs}; the reset took {took:?}"
    );
}
