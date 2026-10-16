use std::io::Cursor;

use oxideav_core::{CodecId, CodecResolver, CodecTag, ProbeContext, ReadSeek, SampleFormat};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

fn elem_uint(id: u32, value: u64) -> Vec<u8> {
    let n = if value == 0 {
        1
    } else {
        (64 - value.leading_zeros()).div_ceil(8) as usize
    };
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(n as u64, 0));
    for i in (0..n).rev() {
        out.push(((value >> (i * 8)) & 0xFF) as u8);
    }
    out
}

fn elem_str(id: u32, s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(s.len() as u64, 0));
    out.extend_from_slice(s.as_bytes());
    out
}

fn elem_float_be_f64(id: u32, value: f64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(8, 0));
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn simple_block(track: u8, tc_offset: i16, keyframe: bool, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.push(payload);
    elem_master(ids::SIMPLE_BLOCK, &body)
}

fn ebml_header() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    b.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    elem_master(ids::EBML_HEADER, &b)
}

fn info() -> Vec<u8> {
    let mut ib = Vec::new();
    ib.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    ib.extend_from_slice(&elem_float_be_f64(ids::DURATION, 1000.0));
    elem_master(ids::INFO, &ib)
}

fn one_cluster() -> Vec<u8> {
    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cb.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    elem_master(ids::CLUSTER, &cb)
}

fn pcm_track(codec_id_str: &str, bit_depth: Option<u64>) -> Vec<u8> {
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48000.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    if let Some(bd) = bit_depth {
        audio_body.extend_from_slice(&elem_uint(ids::BIT_DEPTH, bd));
    }
    let audio_master = elem_master(ids::AUDIO, &audio_body);
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 0x1234));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, codec_id_str));
    tb.extend_from_slice(&audio_master);
    elem_master(ids::TRACK_ENTRY, &tb)
}

fn assemble(tracks_body: &[u8]) -> Vec<u8> {
    let tracks = elem_master(ids::TRACKS, tracks_body);
    let mut seg = Vec::new();
    seg.extend_from_slice(&info());
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&one_cluster());
    let segment = elem_master(ids::SEGMENT, &seg);
    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

fn demux_track(codec_id_str: &str, bit_depth: Option<u64>) -> (CodecId, Option<SampleFormat>) {
    demux_track_with_resolver(codec_id_str, bit_depth, &oxideav_core::NullCodecResolver)
}

fn demux_track_with_resolver(
    codec_id_str: &str,
    bit_depth: Option<u64>,
    resolver: &dyn CodecResolver,
) -> (CodecId, Option<SampleFormat>) {
    let track = pcm_track(codec_id_str, bit_depth);
    let bytes = assemble(&track);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open(rs, resolver).expect("demux open");
    assert_eq!(dmx.streams().len(), 1);
    let s = &dmx.streams()[0];
    (s.params.codec_id.clone(), s.params.sample_format)
}

struct StaticPcmResolver;

impl CodecResolver for StaticPcmResolver {
    fn resolve_tag(&self, ctx: &ProbeContext) -> Option<CodecId> {
        if let CodecTag::Matroska(tag) = &ctx.tag {
            if tag == "A_PCM/INT/LIT" {
                return Some(CodecId::new("pcm_s16le"));
            }
        }
        None
    }
}

#[test]
fn pcm_int_lit_8bit_resolves_to_u8() {
    let (codec_id, sample_format) = demux_track("A_PCM/INT/LIT", Some(8));
    assert_eq!(codec_id.as_str(), "pcm_u8");
    assert_eq!(sample_format, Some(SampleFormat::U8));
}

#[test]
fn pcm_int_lit_24bit_resolves_to_s24le() {
    let (codec_id, sample_format) = demux_track("A_PCM/INT/LIT", Some(24));
    assert_eq!(codec_id.as_str(), "pcm_s24le");
    assert_eq!(sample_format, Some(SampleFormat::S24));
}

#[test]
fn pcm_int_lit_32bit_resolves_to_s32le() {
    let (codec_id, sample_format) = demux_track("A_PCM/INT/LIT", Some(32));
    assert_eq!(codec_id.as_str(), "pcm_s32le");
    assert_eq!(sample_format, Some(SampleFormat::S32));
}

#[test]
fn pcm_int_big_24bit_resolves_to_s24be() {
    let (codec_id, sample_format) = demux_track("A_PCM/INT/BIG", Some(24));
    assert_eq!(codec_id.as_str(), "pcm_s24be");
    assert_eq!(sample_format, Some(SampleFormat::S24));
}

#[test]
fn pcm_float_ieee_64bit_resolves_to_f64le() {
    let (codec_id, sample_format) = demux_track("A_PCM/FLOAT/IEEE", Some(64));
    assert_eq!(codec_id.as_str(), "pcm_f64le");
    assert_eq!(sample_format, Some(SampleFormat::F64));
}

#[test]
fn pcm_int_lit_without_bit_depth_resolves_to_s16le() {
    let (codec_id, sample_format) = demux_track("A_PCM/INT/LIT", None);
    assert_eq!(codec_id.as_str(), "pcm_s16le");
    assert_eq!(sample_format, Some(SampleFormat::S16));
}

#[test]
fn pcm_int_big_8bit_resolves_to_u8() {
    let (codec_id, sample_format) = demux_track("A_PCM/INT/BIG", Some(8));
    assert_eq!(codec_id.as_str(), "pcm_u8");
    assert_eq!(sample_format, Some(SampleFormat::U8));
}

#[test]
fn pcm_int_lit_registry_claim_overridden_by_bit_depth() {
    let (codec_id, sample_format) =
        demux_track_with_resolver("A_PCM/INT/LIT", Some(24), &StaticPcmResolver);
    assert_eq!(codec_id.as_str(), "pcm_s24le");
    assert_eq!(sample_format, Some(SampleFormat::S24));
}
