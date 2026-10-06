use std::io::Cursor;
use oxideav_core::{Demuxer, Error, NullCodecResolver, TimeBase};
use oxideav_mkv::{demux, ebml::{write_element_id, write_vint}, ids};

fn elem(id: u32, bytes: &[u8]) -> Vec<u8> {
    [write_element_id(id), write_vint(bytes.len() as u64, 0), bytes.to_vec()].concat()
}
fn seek(id: u32, offset: u64) -> Vec<u8> {
    elem(ids::SEEK, &[elem(ids::SEEK_ID, &write_element_id(id)), elem(ids::SEEK_POSITION, &offset.to_be_bytes())].concat())
}
fn indexed_file(cycle: bool) -> Vec<u8> {
    let header = elem(ids::EBML_HEADER, &elem(ids::EBML_DOC_TYPE, b"matroska"));
    let tracks = elem(ids::TRACKS, &elem(ids::TRACK_ENTRY, &[
        elem(ids::TRACK_NUMBER, &[1]), elem(ids::TRACK_UID, &[1]),
        elem(ids::TRACK_TYPE, &[0x11]), elem(ids::CODEC_ID, b"S_TEXT/UTF8"),
    ].concat()));
    let info = elem(ids::INFO, &elem(ids::TIMECODE_SCALE, &2_000_000u64.to_be_bytes()));
    let cluster = [write_element_id(ids::CLUSTER), vec![0xff], elem(ids::TIMECODE, &[0]),
        elem(ids::SIMPLE_BLOCK, &[0x81, 0, 0, 0]),
        elem(ids::SIMPLE_BLOCK, &[0x81, 0, 1, 0, b'x']),
        elem(ids::BLOCK_GROUP, &[
            elem(ids::BLOCK, &[0x81, 0, 2, 0]),
            elem(ids::BLOCK_ADDITIONS, &elem(ids::BLOCK_MORE, &elem(ids::BLOCK_ADDITIONAL, b"alpha"))),
        ].concat()),
    ].concat();
    let first_len = elem(ids::SEEK_HEAD, &seek(ids::SEEK_HEAD, 0)).len();
    let nested_at = (first_len + cluster.len()) as u64;
    let nested_len = elem(ids::SEEK_HEAD, &[seek(ids::TRACKS, 0), seek(ids::INFO, 0), seek(ids::SEEK_HEAD, 0)].concat()).len();
    let tracks_at = nested_at + nested_len as u64;
    let nested = elem(ids::SEEK_HEAD, &[
        seek(if cycle { ids::SEEK_HEAD } else { ids::TRACKS }, if cycle { nested_at } else { tracks_at }),
        seek(ids::INFO, tracks_at + tracks.len() as u64),
        seek(ids::SEEK_HEAD, nested_at),
    ].concat());
    [header, elem(ids::SEGMENT, &[
        elem(ids::SEEK_HEAD, &seek(ids::SEEK_HEAD, nested_at)), cluster, nested, tracks, info,
    ].concat())].concat()
}

#[test]
fn nested_index_finds_tracks_and_info_after_unknown_cluster() {
    let mut d = demux::open_typed(Box::new(Cursor::new(indexed_file(false))), &NullCodecResolver).unwrap();
    let p = d.next_packet().unwrap();
    assert_eq!(p.data, b"x");
    assert_eq!(p.pts, Some(1));
    assert_eq!(p.time_base, TimeBase::new(2_000_000, 1_000_000_000));
    let p = d.next_packet().unwrap();
    assert!(p.data.is_empty(), "empty frame with BlockAdditions survives");
    assert!(!d.block_additions().is_empty());
    assert!(matches!(d.next_packet(), Err(Error::Eof)));
}

#[test]
fn cyclic_seek_head_terminates_without_scanning_for_tracks() {
    assert!(demux::open_typed(Box::new(Cursor::new(indexed_file(true))), &NullCodecResolver).is_err());
}
