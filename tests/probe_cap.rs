//! Startup DTS analysis holds at most 1024 packets. A Block that arrives
//! when the held queue cannot also take all of its laces ends the
//! analysis and is queued only after the held packets have been returned,
//! so the queue never grows past the cap, and the Block's packets still
//! follow in order.
//!
//! The heap is measured by a counting global allocator; this binary holds
//! one test so nothing else allocates while it measures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxideav_core::{Demuxer, Error, NullCodecResolver, ReadSeek};
use oxideav_mkv::demux::{self, MkvDemuxer};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

struct Tracking;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

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

const CAP: usize = 1024;

fn elem(id: u32, body: &[u8]) -> Vec<u8> {
    [write_element_id(id), write_vint(body.len() as u64, 0), body.to_vec()].concat()
}

fn uint(id: u32, v: u64) -> Vec<u8> {
    elem(id, &v.to_be_bytes())
}

/// An AVC track whose reorder analysis never completes (it carries no
/// frames) and a subtitle track whose packets the analysis holds back.
fn tracks(copies: u64) -> Vec<u8> {
    let sps = [0x67, 0x42, 0x00, 0x1e, 0xf4, 0xf2];
    let pps = [0x68, 0xce, 0x38, 0x80];
    let avcc = [&[1, 0x42, 0x00, 0x1e, 0xff, 0xe1, 0, sps.len() as u8][..], &sps, &[1, 0, pps.len() as u8], &pps].concat();
    let entry = |number: u64, kind: u64, codec: &str, extra: Vec<u8>| {
        elem(ids::TRACK_ENTRY, &[
            uint(ids::TRACK_NUMBER, number), uint(ids::TRACK_UID, number),
            uint(ids::TRACK_TYPE, kind), elem(ids::CODEC_ID, codec.as_bytes()), extra,
        ].concat())
    };
    let video = elem(ids::VIDEO, &[uint(ids::PIXEL_WIDTH, 16), uint(ids::PIXEL_HEIGHT, 16)].concat());
    let mut entries = vec![
        entry(1, 1, "V_MPEG4/ISO/AVC", [elem(ids::CODEC_PRIVATE, &avcc), video].concat()),
        entry(2, 0x11, "S_TEXT/UTF8", Vec::new()),
    ];
    for number in 3..copies + 2 {
        let join = elem(ids::TRACK_JOIN_BLOCKS, &uint(ids::TRACK_JOIN_UID, 2));
        entries.push(entry(number, 0x11, "S_TEXT/UTF8", elem(ids::TRACK_OPERATION, &join)));
    }
    elem(ids::TRACKS, &entries.concat())
}

/// A keyframe SimpleBlock on the subtitle track.
fn simple(tc: i16, body: &[u8]) -> Vec<u8> {
    elem(ids::SIMPLE_BLOCK, &[&[0x82][..], &tc.to_be_bytes(), &[0x80], body].concat())
}

fn single(i: u16) -> Vec<u8> {
    simple(i as i16, &i.to_be_bytes())
}

fn file(blocks: &[Vec<u8>]) -> Vec<u8> {
    let cluster = elem(ids::CLUSTER, &[uint(ids::TIMECODE, 0), blocks.concat()].concat());
    [
        elem(ids::EBML_HEADER, &elem(ids::EBML_DOC_TYPE, b"matroska")),
        elem(ids::SEGMENT, &[tracks(1), cluster].concat()),
    ].concat()
}

/// Peak heap from opening `bytes` to its first packet, the point where
/// the analysis releases what it held; and the demuxer, ready to drain.
fn peak_to_first_packet(bytes: &[u8]) -> (usize, Vec<u8>, MkvDemuxer) {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let mut d = demux::open_typed(input, &NullCodecResolver).unwrap();
    let first = d.next_packet().unwrap().data;
    (PEAK.load(Ordering::SeqCst) - base, first, d)
}

fn virtual_blocks(copies: u64) -> MkvDemuxer {
    let laces: Vec<u8> = (0..=255).collect();
    let laced = elem(ids::SIMPLE_BLOCK, &[&[0x82, 0, 1, 0x84, 255][..], &laces].concat());
    let first = elem(ids::CLUSTER, &[uint(ids::TIMECODE, 0), single(0), laced].concat());
    let later = elem(ids::CLUSTER, &[uint(ids::TIMECODE, 2000), simple(0, b"later")].concat());
    let bytes = [
        elem(ids::EBML_HEADER, &elem(ids::EBML_DOC_TYPE, b"matroska")),
        elem(ids::SEGMENT, &[tracks(copies), first, later].concat()),
    ].concat();
    let mut d = demux::open_typed(Box::new(Cursor::new(bytes)), &NullCodecResolver).unwrap();
    d.set_apply_track_operations(true);
    d
}

fn check_virtual_boundary() {
    // Four copies of 256 laces exactly fill the cap. The preceding four
    // packets must drain first; copies preserve per-lace metadata/order.
    let mut d = virtual_blocks(4);
    for stream in 1..=4 {
        let p = d.next_packet().unwrap();
        assert_eq!((p.stream_index, p.data), (stream, vec![0, 0]));
    }
    for lace in 0..=255 {
        for stream in 1..=4 {
            let p = d.next_packet().unwrap();
            assert_eq!((p.stream_index, p.data), (stream, vec![lace]));
            assert_eq!(d.packet_metadata().container_keyframe, lace == 0);
        }
    }
    for stream in 1..=4 {
        let p = d.next_packet().unwrap();
        assert_eq!((p.stream_index, p.data), (stream, b"later".to_vec()));
    }
    assert!(matches!(d.next_packet(), Err(Error::Eof)));

    // Five copies exceed the cap. Damage recovery must discard the whole
    // oversized Block, not leak an initial subset, then find the next
    // Cluster. Packets preceding the rejected Block remain intact.
    let mut d = virtual_blocks(5);
    for data in [vec![0, 0], b"later".to_vec()] {
        for stream in 1..=5 {
            let p = d.next_packet().unwrap();
            assert_eq!((p.stream_index, p.data), (stream, data.clone()));
        }
    }
    assert!(matches!(d.next_packet(), Err(Error::Eof)));
}

#[test]
fn a_laced_block_at_the_cap_waits_until_the_held_packets_are_returned() {
    // At the cap: 1024 single packets held, then more.
    let at_cap: Vec<Vec<u8>> = (0..CAP as u16 + 261).map(single).collect();
    // One short of it, then a Block of 256 fixed-size laces.
    let mut laced = (0..CAP as u16 - 1).map(single).collect::<Vec<_>>();
    let laces: Vec<u8> = (0..=255).collect();
    laced.push(elem(ids::SIMPLE_BLOCK, &[&[0x82, 0x03, 0xff, 0x84, 255][..], &laces].concat()));
    laced.extend((CAP as u16..CAP as u16 + 4).map(single));

    let (at_cap_peak, first, d) = peak_to_first_packet(&file(&at_cap));
    assert_eq!(first, [0, 0]);
    drop(d);
    let (laced_peak, first, mut d) = peak_to_first_packet(&file(&laced));
    assert_eq!(first, [0, 0]);
    assert!(
        laced_peak <= at_cap_peak + (8 << 10),
        "a 256-lace Block at the cap peaked at {laced_peak} heap bytes; the full queue peaks at {at_cap_peak}"
    );

    let mut rest = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => rest.push(p.data),
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    let mut expected: Vec<Vec<u8>> = (1..CAP as u16 - 1).map(|i| i.to_be_bytes().to_vec()).collect();
    expected.extend(laces.iter().map(|&b| vec![b]));
    expected.extend((CAP as u16..CAP as u16 + 4).map(|i| i.to_be_bytes().to_vec()));
    assert_eq!(rest, expected);
    drop(d);
    check_virtual_boundary();
}
