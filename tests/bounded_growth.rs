//! What the demuxer keeps for each Cluster it walks stays within a fixed
//! budget however many Clusters a file holds: past the budget no new
//! Cluster record is kept, while playback and seeking go on.
//!
//! The heap is measured by a counting global allocator. The test holds one
//! lock, so nothing else allocates while it measures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use oxideav_core::{Demuxer, Error, NullCodecResolver};
use oxideav_mkv::demux;
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

struct Tracking;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

// SAFETY: every call forwards to `System` with the caller's arguments; the
// counter only observes the sizes.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::SeqCst);
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

/// Subtitle track 1.
fn tracks() -> Vec<u8> {
    let fields = [
        uint(ids::TRACK_NUMBER, 1), uint(ids::TRACK_UID, 1),
        uint(ids::TRACK_TYPE, 0x11), elem(ids::CODEC_ID, b"S_TEXT/UTF8"),
    ].concat();
    elem(ids::TRACKS, &elem(ids::TRACK_ENTRY, &fields))
}

/// A Cluster at `tc` holding one keyframe packet on track 1.
fn cluster(tc: u64, payload: &[u8]) -> Vec<u8> {
    let block = elem(ids::SIMPLE_BLOCK, &[&[0x81, 0, 0, 0x80][..], payload].concat());
    elem(ids::CLUSTER, &[uint(ids::TIMECODE, tc), block].concat())
}

/// Clusters in the file below: more than the Cluster-record budget keeps.
const CLUSTERS: u32 = 300_000;

/// Three hundred thousand Clusters 10 ms apart and no Cues: their records
/// alone would hold about 55 MB. The records stop at the 32 MiB budget,
/// every packet still plays in order, and a seek past the last recorded
/// Cluster lands as one inside the recorded range does.
#[test]
fn cluster_records_stop_at_their_budget() {
    let _serial = serial();
    let mut segment = vec![tracks()];
    segment.extend((0..CLUSTERS).map(|i| cluster(u64::from(i) * 10, &i.to_be_bytes())));
    let bytes = file(&segment);
    drop(segment);
    let base = LIVE.load(Ordering::SeqCst);
    let mut d = demux::open_typed(Box::new(Cursor::new(bytes)), &NullCodecResolver).unwrap();
    let mut played = 0u32;
    let mut in_order = true;
    loop {
        match d.next_packet() {
            Ok(p) => {
                in_order &= p.data == played.to_be_bytes();
                played += 1;
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected error after {played} packets: {e}"),
        }
    }
    let held = LIVE.load(Ordering::SeqCst).saturating_sub(base);
    let recorded = d.cluster_records().len();
    let last = CLUSTERS - 1;
    let far = d.seek_to(0, i64::from(last) * 10).ok().zip(d.next_packet().ok().map(|p| p.data));
    let near = d.seek_to(0, 1000).ok().zip(d.next_packet().ok().map(|p| p.data));
    assert!(
        played == CLUSTERS
            && in_order
            && held < (32 << 20) + (1 << 20)
            && recorded > 0
            && recorded < CLUSTERS as usize
            && far == Some((i64::from(last) * 10, last.to_be_bytes().to_vec()))
            && near == Some((1000, 100u32.to_be_bytes().to_vec())),
        "played {played} in order {in_order}, held {held} heap bytes, {recorded} records, far {far:?}, near {near:?}"
    );
}
