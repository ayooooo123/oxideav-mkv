//! Input that physically carries oversized data: what the demuxer reads
//! and holds stays bounded. A Top-Level master is read for its checksum
//! only when its first child is a `CRC-32`, and then in fixed chunks; a
//! `SeekHead` beyond the two RFC 9559 allows is not read at all; a
//! `BlockGroup` child must fit its parent; and records that occupy more
//! memory than their on-disk bytes share the Block's retention budget.
//!
//! The heap is measured by a counting global allocator. Every test holds
//! one lock, so nothing else allocates while one measures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use oxideav_core::{Demuxer, Error, NullCodecResolver, ReadSeek};
use oxideav_mkv::demux::{self, CrcStatus, MkvDemuxer};
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
