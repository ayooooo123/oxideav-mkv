//! `Demuxer::packet_metadata`: a Block's own random-access indication
//! (the SimpleBlock keyframe bit, or a BlockGroup without ReferenceBlock)
//! reaches its first lace only and leaves the FFmpeg-compatible packet
//! flags alone; nothing stays exposed after end of stream or a seek.

use std::io::Cursor;

use oxideav_core::{Demuxer, Error, NullCodecResolver, PacketMetadata};
use oxideav_mkv::demux::{self, MkvDemuxer};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

fn elem(id: u32, body: &[u8]) -> Vec<u8> {
    [write_element_id(id), write_vint(body.len() as u64, 0), body.to_vec()].concat()
}

/// A Block body for track 1: timestamp `tc`, `flags`, then `frames`
/// Xiph-laced when there are several.
fn block(tc: i16, flags: u8, frames: &[&[u8]]) -> Vec<u8> {
    let mut out = [&[0x81][..], &tc.to_be_bytes()].concat();
    if frames.len() == 1 {
        out.push(flags);
    } else {
        out.push(flags | 0x02);
        out.push(frames.len() as u8 - 1);
        out.extend(frames[..frames.len() - 1].iter().map(|f| f.len() as u8));
    }
    out.extend(frames.concat());
    out
}

fn open(cluster_children: &[Vec<u8>]) -> MkvDemuxer {
    let track = elem(ids::TRACK_ENTRY, &[
        elem(ids::TRACK_NUMBER, &[1]), elem(ids::TRACK_UID, &[1]),
        elem(ids::TRACK_TYPE, &[0x11]), elem(ids::CODEC_ID, b"S_TEXT/UTF8"),
    ].concat());
    let cluster = elem(ids::CLUSTER, &[elem(ids::TIMECODE, &[0]), cluster_children.concat()].concat());
    let bytes = [
        elem(ids::EBML_HEADER, &elem(ids::EBML_DOC_TYPE, b"matroska")),
        elem(ids::SEGMENT, &[elem(ids::TRACKS, &track), cluster].concat()),
    ].concat();
    demux::open_typed(Box::new(Cursor::new(bytes)), &NullCodecResolver).unwrap()
}

#[test]
fn container_keyframe_marks_the_first_lace_of_random_access_blocks() {
    let mut d = open(&[
        elem(ids::SIMPLE_BLOCK, &block(0, 0x80, &[b"a", b"b", b"c"])),
        elem(ids::SIMPLE_BLOCK, &block(10, 0x00, &[b"d"])),
        elem(ids::BLOCK_GROUP, &elem(ids::BLOCK, &block(20, 0x00, &[b"e", b"f"]))),
        elem(ids::BLOCK_GROUP, &[
            elem(ids::BLOCK, &block(30, 0x00, &[b"g"])),
            elem(ids::REFERENCE_BLOCK, &[0xf6]),
        ].concat()),
    ]);
    let mut seen = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => {
                let metadata = d.packet_metadata();
                assert!(metadata.webvtt.is_none() && metadata.audio_trim.is_none());
                seen.push((p.data, p.flags.keyframe, metadata.container_keyframe));
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    // Subtitle packets are all keyframes by FFmpeg's rule; the container
    // marks only the first lace of a SimpleBlock with its keyframe bit and
    // of a BlockGroup without ReferenceBlock.
    let expected: Vec<(Vec<u8>, bool, bool)> = [
        ("a", true), ("b", false), ("c", false), ("d", false), ("e", true), ("f", false), ("g", false),
    ].into_iter().map(|(data, container)| (data.as_bytes().to_vec(), true, container)).collect();
    assert_eq!(seen, expected);
    assert_eq!(d.packet_metadata(), PacketMetadata::default());
}

#[test]
fn a_seek_clears_the_exposed_container_keyframe() {
    let mut d = open(&[
        elem(ids::SIMPLE_BLOCK, &block(0, 0x80, &[b"a"])),
        elem(ids::SIMPLE_BLOCK, &block(10, 0x80, &[b"b"])),
    ]);
    assert_eq!(d.next_packet().unwrap().data, b"a");
    assert!(d.packet_metadata().container_keyframe);
    d.seek_to(0, 0).unwrap();
    assert_eq!(d.packet_metadata(), PacketMetadata::default());
    assert_eq!(d.next_packet().unwrap().data, b"a");
    assert!(d.packet_metadata().container_keyframe);
}
