//! Input that physically carries oversized data: what the demuxer reads
//! and holds stays bounded. A Top-Level master is read for its checksum
//! only when its first child is a `CRC-32`, and then in fixed chunks; a
//! `SeekHead` beyond the two RFC 9559 allows, or a master larger than its
//! budget, is refused before any of it is read; a `BlockGroup` child must
//! fit its parent; records that occupy more memory than their on-disk
//! bytes share the Block's retention budget, as does a Block waiting for
//! queue room; duplicate `BlockAddID`s cost linear time to drop; and the
//! `EncryptedBlock`s kept on Cluster records share one fixed budget.
//!
//! The heap is measured by a counting global allocator. Every test holds
//! one lock, so nothing else allocates while one measures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use oxideav_core::{Demuxer, Error, NullCodecResolver, ReadSeek};
use oxideav_mkv::demux::{self, CrcStatus, DamageKind, MkvDemuxer};
use oxideav_mkv::ebml::{crc32_ieee, write_element_id, write_vint};
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

/// More than the 32 MiB one Block may retain.
const BIG: usize = 48 << 20;
/// What reaching the packets may read or hold.
const SMALL: usize = 4 << 20;

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An element header declaring `size` octets, whatever follows it.
fn header(id: u32, size: usize) -> Vec<u8> {
    [write_element_id(id), write_vint(size as u64, 0)].concat()
}

fn elem(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = header(id, body.len());
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, v: u64) -> Vec<u8> {
    elem(id, &v.to_be_bytes())
}

fn file(segment: &[Vec<u8>]) -> Vec<u8> {
    let mut out = elem(ids::EBML_HEADER, &elem(ids::EBML_DOC_TYPE, b"matroska"));
    out.extend(header(ids::SEGMENT, segment.iter().map(Vec::len).sum()));
    for element in segment {
        out.extend_from_slice(element);
    }
    out
}

fn track(number: u64, kind: u64, codec: &str, extra: &[u8]) -> Vec<u8> {
    elem(ids::TRACK_ENTRY, &[
        uint(ids::TRACK_NUMBER, number), uint(ids::TRACK_UID, number),
        uint(ids::TRACK_TYPE, kind), elem(ids::CODEC_ID, codec.as_bytes()), extra.to_vec(),
    ].concat())
}

fn simple(track: u8, tc: i16, payload: &[u8]) -> Vec<u8> {
    elem(ids::SIMPLE_BLOCK, &[&[0x80 | track][..], &tc.to_be_bytes(), &[0x80], payload].concat())
}

fn cluster(tc: u64, children: &[Vec<u8>]) -> Vec<u8> {
    elem(ids::CLUSTER, &[uint(ids::TIMECODE, tc), children.concat()].concat())
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

fn first_packet(input: Box<dyn ReadSeek>) -> (MkvDemuxer, Vec<u8>) {
    let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
    let first = d.next_packet().unwrap().data;
    (d, first)
}

fn drain(input: Box<dyn ReadSeek>) -> Vec<Vec<u8>> {
    let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
    let mut out = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => out.push(p.data),
            Err(Error::Eof) => return out,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}

fn subtitle_tracks() -> Vec<u8> {
    elem(ids::TRACKS, &track(1, 0x11, "S_TEXT/UTF8", &[]))
}

/// One `AttachedFile` of `len` bytes, as a font or a cover image is stored.
fn attached_file(len: usize) -> Vec<u8> {
    let names = [elem(ids::FILE_NAME, b"font.ttf"), elem(ids::FILE_MIME_TYPE, b"font/ttf"), uint(ids::FILE_UID, 7)].concat();
    let data = header(ids::FILE_DATA, len);
    let mut out = header(ids::ATTACHED_FILE, names.len() + data.len() + len);
    out.extend(names);
    out.extend(data);
    out.resize(out.len() + len, 0x5a);
    out
}

/// A one-entry SeekHead pointing at `id`.
fn index(id: u32, position: u64) -> Vec<u8> {
    elem(ids::SEEK_HEAD, &elem(ids::SEEK, &[
        elem(ids::SEEK_ID, &write_element_id(id)), uint(ids::SEEK_POSITION, position),
    ].concat()))
}

#[test]
fn a_master_without_a_crc_is_not_read_for_one() {
    let _serial = serial();
    let first = cluster(0, &[simple(1, 0, b"a")]);
    // In line ahead of the Clusters, where mkvmerge stores attachments.
    let inline = file(&[subtitle_tracks(), elem(ids::ATTACHMENTS, &attached_file(BIG)), first.clone()]);
    // After the Clusters, reached through the SeekHead.
    let tracks = subtitle_tracks();
    let at = (index(ids::ATTACHMENTS, 0).len() + tracks.len() + first.len()) as u64;
    let followed = file(&[index(ids::ATTACHMENTS, at), tracks, first, elem(ids::ATTACHMENTS, &attached_file(BIG))]);
    let mut failures = Vec::new();
    for (layout, bytes) in [("in line", inline), ("followed", followed)] {
        let ((d, packet), read, peak) = measure(bytes, first_packet);
        // The attachment is still parsed, from its headers alone.
        let parsed = packet == b"a" && d.attachments()[0].data_size == BIG as u64;
        if !parsed || read >= SMALL || peak >= SMALL {
            failures.push(format!("{layout}: parsed {parsed}, read {read} bytes, peaked at {peak} heap bytes"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn a_third_seek_head_is_skipped_unread() {
    let _serial = serial();
    // RFC 9559 §5.1.1 allows two SeekHeads. A third declaring a CRC-32 would
    // otherwise be read whole to check it, then ignored.
    let void = [header(ids::VOID, BIG), vec![0; BIG]].concat();
    let crc = elem(ids::CRC32, &[0; 4]);
    let third = [header(ids::SEEK_HEAD, crc.len() + void.len()), crc, void].concat();
    let empty = elem(ids::SEEK_HEAD, &[]);
    let bytes = file(&[empty.clone(), empty, third, subtitle_tracks(), cluster(0, &[simple(1, 0, b"a")])]);
    let ((_, packet), read, peak) = measure(bytes, first_packet);
    assert_eq!(packet, b"a");
    assert!(read < SMALL && peak < SMALL, "read {read} bytes, peaked at {peak} heap bytes");
}

#[test]
fn a_present_crc_is_checked_in_fixed_chunks() {
    let _serial = serial();
    let checked = attached_file(BIG);
    let crc = crc32_ieee(&checked);
    let crc_child = elem(ids::CRC32, &crc.to_le_bytes());
    let attachments = [header(ids::ATTACHMENTS, crc_child.len() + checked.len()), crc_child, checked].concat();
    let bytes = file(&[subtitle_tracks(), attachments, cluster(0, &[simple(1, 0, b"a")])]);
    let ((d, packet), _, peak) = measure(bytes, first_packet);
    assert_eq!(packet, b"a");
    let status = d.crc_status().iter().find(|s| s.element_id == ids::ATTACHMENTS);
    assert_eq!(status, Some(&CrcStatus { element_id: ids::ATTACHMENTS, stored: crc, computed: crc }));
    assert!(peak < SMALL, "peaked at {peak} heap bytes");
}

/// Subtitle track 2 behind an AVC track 1 whose startup reorder analysis
/// never completes: packets are held until 1024 of them wait.
fn held_tracks() -> Vec<u8> {
    let sps = [0x67, 0x42, 0x00, 0x1e, 0xf4, 0xf2];
    let pps = [0x68, 0xce, 0x38, 0x80];
    let avcc = [&[1, 0x42, 0x00, 0x1e, 0xff, 0xe1, 0, sps.len() as u8][..], &sps, &[1, 0, pps.len() as u8], &pps].concat();
    let video = elem(ids::VIDEO, &[uint(ids::PIXEL_WIDTH, 16), uint(ids::PIXEL_HEIGHT, 16)].concat());
    elem(ids::TRACKS, &[
        track(1, 1, "V_MPEG4/ISO/AVC", &[elem(ids::CODEC_PRIVATE, &avcc), video].concat()),
        track(2, 0x11, "S_TEXT/UTF8", &[]),
    ].concat())
}

/// Two Xiph-laced frames on track 2, "y" and "z": behind 1023 held packets
/// the Block has to wait for room.
fn laced() -> Vec<u8> {
    [&[0x82, 0, 0, 0x82, 1, 1][..], b"yz"].concat()
}

/// `held` packets, then `group` closing their Cluster, then a Cluster whose
/// packet must still play, then a `Void` of `tail` bytes that an overrunning
/// child can reach into.
fn group_file(held: u16, group: Vec<u8>, tail: usize) -> Vec<u8> {
    let mut first: Vec<Vec<u8>> = (0..held).map(|i| simple(2, i as i16, &i.to_be_bytes())).collect();
    first.push(group);
    let mut segment = vec![held_tracks(), cluster(0, &first), cluster(2000, &[simple(2, 0, b"next")])];
    if tail > 0 {
        segment.push([header(ids::VOID, tail), vec![0; tail]].concat());
    }
    file(&segment)
}

/// The held packets in order, then the following Cluster's.
fn expected(held: u16) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = (0..held).map(|i| i.to_be_bytes().to_vec()).collect();
    out.push(b"next".to_vec());
    out
}

#[test]
fn block_group_children_must_fit_their_parents() {
    let _serial = serial();
    let block = elem(ids::BLOCK, &laced());
    // Each child declares BIG bytes that the file physically holds, while
    // its parent ends right after the child's header.
    let overruns = [
        ("Block", elem(ids::BLOCK_GROUP, &[header(ids::BLOCK, BIG), laced()[..6].to_vec()].concat())),
        ("CodecState", elem(ids::BLOCK_GROUP, &[block.clone(), header(ids::CODEC_STATE, BIG)].concat())),
        ("BlockAdditional", elem(ids::BLOCK_GROUP, &[
            block, elem(ids::BLOCK_ADDITIONS, &elem(ids::BLOCK_MORE, &header(ids::BLOCK_ADDITIONAL, BIG))),
        ].concat())),
    ];
    let mut failures = Vec::new();
    for (child, group) in overruns {
        for held in [0, 1023] {
            let (packets, read, peak) = measure(group_file(held, group.clone(), BIG), drain);
            let played = packets == expected(held);
            if !played || read >= SMALL || peak >= SMALL {
                failures.push(format!(
                    "{child}, {held} held: played {played}, read {read} bytes, peaked at {peak} heap bytes"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn block_group_records_share_the_block_budget() {
    let _serial = serial();
    // Two million empty TimeSlices: 4 MiB on disk, 80 bytes each once read.
    let count = 1 << 21;
    let slices = [header(ids::SLICES, 2 * count), [ids::TIME_SLICE as u8, 0x80].repeat(count)].concat();
    let group = elem(ids::BLOCK_GROUP, &[elem(ids::BLOCK, &laced()), slices].concat());
    let mut failures = Vec::new();
    for held in [0, 1023] {
        let (packets, _, peak) = measure(group_file(held, group.clone(), 0), drain);
        let played = packets == expected(held);
        // At most the 32 MiB a Block may retain, plus a growing buffer's
        // previous half while it moves.
        if !played || peak >= 48 << 20 {
            failures.push(format!("{held} held: played {played}, peaked at {peak} heap bytes"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A Block within its 32 MiB budget as stored, a 17 MiB two-frame Block
/// and a 9 MiB CodecState, has to wait behind 1023 held packets. While it
/// waits it holds no more than that budget, then it plays whole.
#[test]
fn a_waiting_block_holds_no_more_than_its_budget() {
    let _serial = serial();
    let mut stored = [&[0x82, 0, 0, 0x82, 1, 1][..], b"y"].concat();
    stored.resize(17 << 20, 0x5a);
    let group = elem(ids::BLOCK_GROUP, &[
        elem(ids::BLOCK, &stored), elem(ids::CODEC_STATE, &vec![0x33; 9 << 20]),
    ].concat());
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(group_file(1023, group, 0)));
    let base = LIVE.load(Ordering::SeqCst);
    let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
    let mut packets = vec![d.next_packet().unwrap().data];
    // The held packets now return while the Block waits.
    let holding = LIVE.load(Ordering::SeqCst).saturating_sub(base);
    loop {
        match d.next_packet() {
            Ok(p) => packets.push(p.data),
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    let mut expected = expected(1023);
    let next = expected.pop().unwrap();
    expected.extend([b"y".to_vec(), stored[7..].to_vec(), next]);
    let played = packets == expected;
    // The budget, plus the held packets and the demuxer's own state.
    assert!(
        played && holding < (32 << 20) + (1 << 20),
        "held {holding} heap bytes while the Block waited; played whole: {played}"
    );
}

/// 100,000 distinct BlockAddIDs, then each again with another payload: the
/// first of each id is kept in stored order, and finding the duplicates
/// takes linear time, not a scan of every addition kept so far.
#[test]
fn duplicate_block_addition_ids_are_dropped_in_linear_time() {
    let _serial = serial();
    let count = 100_000u64;
    let more = |id: u64, data: &[u8]| {
        elem(ids::BLOCK_MORE, &[uint(ids::BLOCK_ADD_ID, id), elem(ids::BLOCK_ADDITIONAL, data)].concat())
    };
    let additions: Vec<u8> = (1..=count).map(|id| more(id, b"a"))
        .chain((1..=count).map(|id| more(id, b"b")))
        .flatten()
        .collect();
    let group = elem(ids::BLOCK_GROUP, &[
        elem(ids::BLOCK, &[0x81, 0, 0, 0, b'x']), elem(ids::BLOCK_ADDITIONS, &additions),
    ].concat());
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(file(&[subtitle_tracks(), cluster(0, &[group])])));
    let started = Instant::now();
    let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
    assert_eq!(d.next_packet().unwrap().data, b"x");
    let took = started.elapsed();
    let kept = d.block_additions();
    let first_of_each = kept.len() == count as usize
        && kept.iter().zip(1..).all(|(a, id)| a.block_add_id() == id && a.data() == b"a");
    assert!(
        first_of_each && took < Duration::from_secs(10),
        "kept {} additions, the first of each id: {first_of_each}; took {took:?}",
        kept.len()
    );
}

/// The damage the open of `input` noted and its first packet, or `None`
/// when the open is invalid data.
fn opened(input: Box<dyn ReadSeek>, resilient: bool) -> Option<(Vec<DamageKind>, Vec<u8>)> {
    let opened = if resilient {
        demux::open_resilient_typed(input, &NullCodecResolver)
    } else {
        demux::open_typed(input, &NullCodecResolver)
    };
    match opened {
        Ok(mut d) => {
            let damage = d.damage_events().iter().map(|e| e.kind()).collect();
            Some((damage, d.next_packet().unwrap().data))
        }
        Err(Error::InvalidData(_)) => None,
        Err(e) => panic!("unexpected error: {e}"),
    }
}

/// A master larger than its budget is refused before it is read, even for
/// its CRC-32: a Tracks master is invalid data like contents over their
/// budgets, and an optional SeekHead or Tags master is dropped by either
/// open with one damage event, in line or followed through the SeekHead,
/// as a trailing one is passed over. A master within its budget is still
/// checked.
#[test]
fn masters_over_their_budget_are_refused_unread() {
    let _serial = serial();
    // `contents`, then BIG bytes of Void behind a CRC-32 (its value is not
    // the point): more than a SeekHead, Tracks or Tags master may hold.
    let padded = |id: u32, contents: &[u8]| {
        let crc = elem(ids::CRC32, &[0; 4]);
        let void = header(ids::VOID, BIG);
        let mut out = header(id, crc.len() + contents.len() + void.len() + BIG);
        out.extend(crc);
        out.extend_from_slice(contents);
        out.extend(void);
        out.resize(out.len() + BIG, 0);
        out
    };
    let first = cluster(0, &[simple(1, 0, b"a")]);
    let entry = track(1, 0x11, "S_TEXT/UTF8", &[]);
    // Packet "a" after one damage event for the master `id`.
    let a = |id: u32| Some((vec![DamageKind::DamagedMaster(id)], b"a".to_vec()));
    let mut failures = Vec::new();
    let mut check = |case: &str, bytes: Vec<u8>, resilient: bool, expected: Option<(Vec<DamageKind>, Vec<u8>)>| {
        let (outcome, read, _) = measure(bytes, |input| opened(input, resilient));
        if outcome != expected || read >= SMALL {
            failures.push(format!("{case}, resilient {resilient}: damage and first packet {outcome:?} after reading {read} bytes"));
        }
    };
    let tracks = subtitle_tracks();
    let at = (index(ids::TAGS, 0).len() + tracks.len() + first.len()) as u64;
    for resilient in [false, true] {
        check("SeekHead", file(&[padded(ids::SEEK_HEAD, &[]), subtitle_tracks(), first.clone()]), resilient, a(ids::SEEK_HEAD));
        let tags = file(&[subtitle_tracks(), padded(ids::TAGS, &[]), first.clone()]);
        check("Tags", tags, resilient, a(ids::TAGS));
        let tracks_over = file(&[padded(ids::TRACKS, &entry), first.clone()]);
        check("Tracks", tracks_over, resilient, None);
        let followed = file(&[index(ids::TAGS, at), tracks.clone(), first.clone(), padded(ids::TAGS, &[])]);
        check("Tags followed", followed, resilient, a(ids::TAGS));
    }
    let trailing = file(&[subtitle_tracks(), first.clone(), padded(ids::TAGS, &[])]);
    let (packets, read, _) = measure(trailing, drain);
    if packets != [b"a".to_vec()] || read >= SMALL {
        failures.push(format!("Tags trailing: {} packets after reading {read} bytes", packets.len()));
    }
    // Within its budget, a 3 MiB CodecPrivate's Tracks is still checked.
    let entry = track(1, 0x11, "S_TEXT/UTF8", &elem(ids::CODEC_PRIVATE, &vec![1; 3 << 20]));
    let crc = crc32_ieee(&entry);
    let crc_child = elem(ids::CRC32, &crc.to_le_bytes());
    let tracks = [header(ids::TRACKS, crc_child.len() + entry.len()), crc_child, entry].concat();
    let ((d, packet), _, _) = measure(file(&[tracks, first]), first_packet);
    let status = d.crc_status().iter().find(|s| s.element_id == ids::TRACKS).copied();
    if packet != b"a" || status != Some(CrcStatus { element_id: ids::TRACKS, stored: crc, computed: crc }) {
        failures.push(format!("Tracks within budget: CRC-32 status {status:?}"));
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Ten Clusters, each a packet and then a 5 MiB EncryptedBlock: 50 MiB that
/// the Cluster records would keep for as long as the file is open. They keep
/// the blocks that fit the 32 MiB budget, in order. A block past it is
/// damage, recovered from as any is and recorded once, so every Cluster's
/// packet still plays.
#[test]
fn encrypted_blocks_share_one_retention_budget() {
    let _serial = serial();
    const BLOCK: usize = 5 << 20;
    let mut segment = vec![subtitle_tracks()];
    for i in 0..10u8 {
        let mut body = vec![i; BLOCK];
        body[..4].copy_from_slice(&[0x81, 0, 0, 0x80]);
        segment.push(cluster(u64::from(i) * 1000, &[simple(1, 0, &[b'p', i]), elem(ids::ENCRYPTED_BLOCK, &body)]));
    }
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(file(&segment)));
    let base = LIVE.load(Ordering::SeqCst);
    let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
    let mut packets = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => packets.push(p.data),
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    let retained = LIVE.load(Ordering::SeqCst).saturating_sub(base);
    let kept: Vec<u8> = d.cluster_records().iter().flat_map(|r| &r.encrypted_blocks).map(|b| b[BLOCK - 1]).collect();
    let damaged = d.damage_events().len();
    let played = packets == (0..10u8).map(|i| vec![b'p', i]).collect::<Vec<_>>();
    // Six 5 MiB blocks fit the 32 MiB budget; the other four are damage.
    assert!(
        played && kept == [0, 1, 2, 3, 4, 5] && damaged == 4 && retained < (32 << 20) + (1 << 20),
        "played {played}, kept blocks {kept:?}, {damaged} damage events, retained {retained} heap bytes"
    );
}

/// Opens `segment`, drains it and returns the demuxer with its packets and
/// the heap it still holds beyond what was live before the open.
fn drained(segment: &[Vec<u8>]) -> (MkvDemuxer, Vec<Vec<u8>>, usize) {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(file(segment)));
    let base = LIVE.load(Ordering::SeqCst);
    let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
    let mut packets = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => packets.push(p.data),
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    let held = LIVE.load(Ordering::SeqCst).saturating_sub(base);
    (d, packets, held)
}

/// A 31 MiB EncryptedBlock, then 16,000 Clusters each holding an empty one.
/// Every record's list of blocks costs memory besides the blocks, and the
/// lists and blocks together stay within the 32 MiB budget.
#[test]
fn encrypted_block_lists_count_against_the_budget() {
    let _serial = serial();
    let mut big = vec![0x5a; 31 << 20];
    big[..4].copy_from_slice(&[0x81, 0, 0, 0x80]);
    let mut segment = vec![subtitle_tracks(), cluster(0, &[simple(1, 0, b"a"), elem(ids::ENCRYPTED_BLOCK, &big)])];
    for i in 1..=16_000 {
        segment.push(cluster(i, &[elem(ids::ENCRYPTED_BLOCK, &[])]));
    }
    segment.push(cluster(20_000, &[simple(1, 0, b"z")]));
    let (d, packets, _) = drained(&segment);
    let records = d.cluster_records();
    let lists: usize = records.iter().map(|r| r.encrypted_blocks.capacity() * std::mem::size_of::<Vec<u8>>()).sum();
    let blocks: usize = records.iter().flat_map(|r| &r.encrypted_blocks).map(Vec::capacity).sum();
    let big_kept = records[0].encrypted_blocks.first().map(Vec::len) == Some(31 << 20);
    let played = packets == [b"a".to_vec(), b"z".to_vec()];
    assert!(
        played && big_kept && lists + blocks <= 32 << 20,
        "played {played}, 31 MiB block kept {big_kept}, lists {lists} + blocks {blocks} bytes"
    );
}

/// Four Clusters, each a packet and then a SilentTracks master of 1.5
/// million SilentTrackNumbers: 24 MiB on disk that the records would keep
/// as 48 MB of numbers. They keep what fits the 32 MiB budget they share
/// with EncryptedBlocks; a master past it is damage, recovered from as any
/// is, so every Cluster's packet still plays.
#[test]
fn silent_tracks_share_the_cluster_record_budget() {
    let _serial = serial();
    let numbers = elem(ids::SILENT_TRACK_NUMBER, &[1]).repeat(1_500_000);
    let mut segment = vec![subtitle_tracks()];
    for i in 0..4u8 {
        segment.push(cluster(u64::from(i) * 1000, &[simple(1, 0, &[b'p', i]), elem(ids::SILENT_TRACKS, &numbers)]));
    }
    let (d, packets, held) = drained(&segment);
    let kept: Vec<usize> = d.cluster_records().iter().map(|r| r.silent_track_numbers.len()).collect();
    let lists: usize = d.cluster_records().iter().map(|r| r.silent_track_numbers.capacity() * 8).sum();
    let played = packets == (0..4u8).map(|i| vec![b'p', i]).collect::<Vec<_>>();
    // Two masters fit the budget; the other two are damage.
    assert!(
        played && kept == [1_500_000, 1_500_000, 0, 0] && d.damage_events().len() == 2
            && lists <= 32 << 20 && held < (32 << 20) + (1 << 20),
        "played {played}, kept {kept:?}, {} damage events, lists {lists} bytes, held {held} heap bytes",
        d.damage_events().len()
    );
}

/// An 8 MiB AV1 frame of four million empty frame OBUs is read for its key
/// frame flag without listing them: the packet costs no more than its
/// Block budget, with or without a sequence header in the CodecPrivate.
#[test]
fn an_av1_frame_of_empty_obus_is_read_within_its_block_budget() {
    let _serial = serial();
    let frame = [0x32u8, 0x00].repeat(4 << 20);
    let mut failures = Vec::new();
    // An `av1C` record whose configOBUs hold a sequence header.
    let config = elem(ids::CODEC_PRIVATE, &[0x81, 0, 0, 0, 0x0a, 0x01, 0x00]);
    for (case, private) in [("with a sequence header", config), ("without one", vec![])] {
        let tracks = elem(ids::TRACKS, &track(1, 1, "V_AV1", &private));
        let bytes = file(&[tracks, cluster(0, &[simple(1, 0, &frame)])]);
        let mut d = demux::open_typed(Box::new(Cursor::new(bytes)), &NullCodecResolver).unwrap();
        let base = LIVE.load(Ordering::SeqCst);
        PEAK.store(base, Ordering::SeqCst);
        let packet = d.next_packet().map(|p| p.data.len()).map_err(|e| format!("{e}"));
        let peak = PEAK.load(Ordering::SeqCst) - base;
        if packet != Ok(frame.len()) || peak >= 32 << 20 {
            failures.push(format!("{case}: packet {packet:?}, peak {peak} heap bytes"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
