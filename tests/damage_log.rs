//! The demuxer keeps at most 4096 damage events and counts every one past
//! that exactly, however much damage a file holds.

use std::io::Cursor;

use oxideav_core::{Demuxer, Error, NullCodecResolver};
use oxideav_mkv::demux::{self, DamageKind};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

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

/// Subtitle track 1.
fn tracks() -> Vec<u8> {
    let fields = [
        uint(ids::TRACK_NUMBER, 1), uint(ids::TRACK_UID, 1),
        uint(ids::TRACK_TYPE, 0x11), elem(ids::CODEC_ID, b"S_TEXT/UTF8"),
    ].concat();
    elem(ids::TRACKS, &elem(ids::TRACK_ENTRY, &fields))
}

fn simple(payload: &[u8]) -> Vec<u8> {
    elem(ids::SIMPLE_BLOCK, &[&[0x81, 0, 0, 0x80][..], payload].concat())
}

/// Clusters in the file below that each need a recovery.
const DAMAGED: u32 = 5_000;

/// Five thousand Clusters, each a packet and then a Block declared larger
/// than its Cluster, and a last whole Cluster. Each damaged Cluster is one
/// recovery at the next Cluster. The first 4096 are kept in order and the
/// other 904 counted; every packet still plays.
#[test]
fn damage_events_stop_at_4096_and_count_the_rest() {
    let mut segment = vec![tracks()];
    for i in 0..DAMAGED {
        let overrun = header(ids::SIMPLE_BLOCK, 64);
        segment.push(elem(ids::CLUSTER, &[uint(ids::TIMECODE, u64::from(i) * 10), simple(&i.to_be_bytes()), overrun].concat()));
    }
    segment.push(elem(ids::CLUSTER, &[uint(ids::TIMECODE, u64::from(DAMAGED) * 10), simple(&DAMAGED.to_be_bytes())].concat()));
    let mut d = demux::open_typed(Box::new(Cursor::new(file(&segment))), &NullCodecResolver).unwrap();
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
    let events = d.damage_events();
    let first_kept = events.iter().all(|e| e.kind() == DamageKind::ClusterStream)
        && events.windows(2).all(|w| w[0].offset() < w[1].offset());
    assert!(
        played == DAMAGED + 1
            && in_order
            && events.len() == 4096
            && first_kept
            && d.dropped_damage_events() == u64::from(DAMAGED) - 4096,
        "played {played} in order {in_order}, {} events kept, first kept in order {first_kept}, {} dropped",
        events.len(),
        d.dropped_damage_events()
    );
}
