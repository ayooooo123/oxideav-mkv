use std::io::Cursor;
use oxideav_core::{Demuxer, Error, MediaType, NullCodecResolver};
use oxideav_mkv::{demux, ebml::{write_element_id, write_vint}, ids};

fn elem(id: u32, bytes: &[u8]) -> Vec<u8> {
    [write_element_id(id), write_vint(bytes.len() as u64, 0), bytes.to_vec()].concat()
}

fn file(codec: &str, kind: u8, payload: &[u8]) -> Vec<u8> {
    let header = elem(ids::EBML_HEADER, &elem(ids::EBML_DOC_TYPE, b"matroska"));
    let track = elem(ids::TRACK_ENTRY, &[
        elem(ids::TRACK_NUMBER, &[1]), elem(ids::TRACK_UID, &[1]),
        elem(ids::TRACK_TYPE, &[kind]), elem(ids::CODEC_ID, codec.as_bytes()),
    ].concat());
    let group = elem(ids::BLOCK_GROUP, &[
        elem(ids::BLOCK, &[&[0x81, 0, 10, 0], payload].concat()),
        elem(ids::BLOCK_DURATION, &[100]),
    ].concat());
    [header, elem(ids::SEGMENT, &[
        elem(ids::TRACKS, &track),
        elem(ids::CLUSTER, &[elem(ids::TIMECODE, &[0]), group].concat()),
    ].concat())].concat()
}

#[test]
fn d_webvtt_variants_split_side_data_and_resolve_subtitles() {
    for codec in ["D_WEBVTT/SUBTITLES", "D_WEBVTT/CAPTIONS", "D_WEBVTT/DESCRIPTIONS", "D_WEBVTT/METADATA"] {
        let mut d = demux::open_typed(Box::new(Cursor::new(file(codec, 0x21, b"cue\r\nalign:start\nhello\r\n"))), &NullCodecResolver).unwrap();
        let stream = &d.streams()[0];
        assert_eq!(stream.params.codec_id.as_str(), "webvtt");
        assert_eq!(stream.params.media_type, MediaType::Subtitle);
        let p = d.next_packet().unwrap();
        assert_eq!(p.data, b"hello");
        assert_eq!((p.pts, p.dts, p.duration), (Some(10), Some(10), Some(100)));
        assert!(p.flags.keyframe);
        let side = d.webvtt_metadata().unwrap();
        assert_eq!(side.identifier, b"cue");
        assert_eq!(side.settings, b"align:start");
        assert!(matches!(d.next_packet(), Err(Error::Eof)));
    }
}

#[test]
fn s_text_webvtt_stays_raw() {
    let raw = b"line one\nline two\r\n";
    let mut d = demux::open_typed(Box::new(Cursor::new(file("S_TEXT/WEBVTT", 0x11, raw))), &NullCodecResolver).unwrap();
    assert_eq!(d.next_packet().unwrap().data, raw);
    assert!(d.webvtt_metadata().is_none());
}

#[test]
fn laced_cues_keep_metadata_association_across_seek() {
    let first = b"first\nalign:start\none";
    let second = b"second\nalign:end\ntwo";
    let mut tracks = Vec::new();
    for (number, codec) in [(1, "D_WEBVTT/SUBTITLES"), (2, "S_TEXT/UTF8")] {
        tracks.extend(elem(ids::TRACK_ENTRY, &[
            elem(ids::TRACK_NUMBER, &[number]), elem(ids::TRACK_UID, &[number]),
            elem(ids::TRACK_TYPE, &[0x11]), elem(ids::CODEC_ID, codec.as_bytes()),
        ].concat()));
    }
    let laced = elem(ids::SIMPLE_BLOCK,
        &[&[0x81, 0, 10, 0x82, 1, first.len() as u8], first.as_slice(), second.as_slice()].concat());
    let plain = elem(ids::SIMPLE_BLOCK, b"\x82\x00\x14\x80plain");
    let bytes = [
        elem(ids::EBML_HEADER, &elem(ids::EBML_DOC_TYPE, b"matroska")),
        elem(ids::SEGMENT, &[
            elem(ids::TRACKS, &tracks),
            elem(ids::CLUSTER, &[elem(ids::TIMECODE, &[0]), laced, plain].concat()),
        ].concat()),
    ].concat();
    let mut d = demux::open_typed(Box::new(Cursor::new(bytes)), &NullCodecResolver).unwrap();
    for _ in 0..2 {
        let packet = d.next_packet().unwrap();
        assert_eq!(packet.data, b"one");
        assert_eq!(packet.pts, Some(10));
        let metadata = d.webvtt_metadata().unwrap();
        assert_eq!(metadata.identifier, b"first");
        assert_eq!(metadata.settings, b"align:start");
        assert_eq!(d.next_packet().unwrap().data, b"two");
        let metadata = d.webvtt_metadata().unwrap();
        assert_eq!(metadata.identifier, b"second");
        assert_eq!(metadata.settings, b"align:end");
        // The first keyframe lands on its Cluster; the next packet still
        // carries the cue's own timestamp (10), not the Cluster's zero.
        assert_eq!(d.seek_to(0, 10).unwrap(), 0);
        assert!(d.webvtt_metadata().is_none());
    }
    d.next_packet().unwrap();
    d.next_packet().unwrap();
    assert_eq!(d.next_packet().unwrap().data, b"plain");
    assert!(d.webvtt_metadata().is_none());
}
