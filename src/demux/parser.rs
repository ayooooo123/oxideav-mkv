//! The codec of a track as FFmpeg identifies it, and the frame inspection
//! FFmpeg's codec parsers perform on its packets.
//!
//! FFmpeg's `matroskadec` hands most tracks to a codec parser
//! (`need_parsing`), and libavformat then flags a packet a keyframe from
//! what the parser read in the frame rather than from the Block: an H.264
//! IDR or recovery point, a VP8 / VP9 / AV1 key frame, a Theora intra
//! frame, a TrueHD major sync. Packets of an intra-only codec (most audio,
//! ProRes, MJPEG, raw video) and of every track that isn't audio or video
//! are all keyframes. Following those rules is what makes the keyframe
//! flags of the packets equal FFmpeg's.

/// The codec of a track, as FFmpeg's `ff_mkv_codec_tags` maps its
/// `CodecID` (by prefix).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Codec {
    H264,
    Hevc,
    Vvc,
    Vp3,
    Vp8,
    Vp9,
    Av1,
    Theora,
    Aac,
    Mlp,
    TrueHd,
    /// Any other codec FFmpeg knows. `intra_only` is its descriptor's
    /// `AV_CODEC_PROP_INTRA_ONLY` — set for every audio codec but AAC /
    /// MLP / TrueHD and for JPEG 2000, MJPEG, ProRes and raw video — or
    /// the codec isn't audio or video (subtitles), which FFmpeg treats the
    /// same way.
    Other { intra_only: bool },
    /// A `CodecID` FFmpeg doesn't map (`AV_CODEC_ID_NONE`).
    Unknown,
}

/// `ff_mkv_codec_tags`, in its order (the first prefix match wins).
const MKV_CODEC_TAGS: &[(&str, Codec)] = {
    const INTRA: Codec = Codec::Other { intra_only: true };
    const INTER: Codec = Codec::Other { intra_only: false };
    &[
        ("A_AAC", Codec::Aac),
        ("A_AC3", INTRA),
        ("A_ALAC", INTRA),
        ("A_ATRAC/AT1", INTRA),
        ("A_DTS", INTRA),
        ("A_EAC3", INTRA),
        ("A_FLAC", INTRA),
        ("A_MLP", Codec::Mlp),
        ("A_MPEG/L2", INTRA),
        ("A_MPEG/L1", INTRA),
        ("A_MPEG/L3", INTRA),
        ("A_OPUS", INTRA),
        ("A_PCM/FLOAT/IEEE", INTRA),
        ("A_PCM/INT/BIG", INTRA),
        ("A_PCM/INT/LIT", INTRA),
        ("A_QUICKTIME/QDMC", INTRA),
        ("A_QUICKTIME/QDM2", INTRA),
        ("A_REAL/14_4", INTRA),
        ("A_REAL/28_8", INTRA),
        ("A_REAL/ATRC", INTRA),
        ("A_REAL/COOK", INTRA),
        ("A_REAL/SIPR", INTRA),
        ("A_TRUEHD", Codec::TrueHd),
        ("A_TTA1", INTRA),
        ("A_VORBIS", INTRA),
        ("A_WAVPACK4", INTRA),
        ("D_WEBVTT/SUBTITLES", INTRA),
        ("D_WEBVTT/CAPTIONS", INTRA),
        ("D_WEBVTT/DESCRIPTIONS", INTRA),
        ("D_WEBVTT/METADATA", INTRA),
        ("S_TEXT/UTF8", INTRA),
        ("S_TEXT/ASCII", INTRA),
        ("S_TEXT/ASS", INTRA),
        ("S_TEXT/SSA", INTRA),
        ("S_ASS", INTRA),
        ("S_SSA", INTRA),
        ("S_VOBSUB", INTRA),
        ("S_DVBSUB", INTRA),
        ("S_HDMV/PGS", INTRA),
        ("S_HDMV/TEXTST", INTRA),
        ("S_ARIBSUB", INTRA),
        ("V_AV1", Codec::Av1),
        ("V_AVS2", INTER),
        ("V_AVS3", INTER),
        ("V_DIRAC", INTER),
        ("V_FFV1", INTER),
        ("V_JPEG2000", INTRA),
        ("V_MJPEG", INTRA),
        ("V_MPEG1", INTER),
        ("V_MPEG2", INTER),
        ("V_MPEG4/ISO/ASP", INTER),
        ("V_MPEG4/ISO/AP", INTER),
        ("V_MPEG4/ISO/SP", INTER),
        ("V_MPEG4/ISO/AVC", Codec::H264),
        ("V_MPEGH/ISO/HEVC", Codec::Hevc),
        ("V_MPEGI/ISO/VVC", Codec::Vvc),
        ("V_MPEG4/MS/V3", INTER),
        ("V_PRORES", INTRA),
        ("V_REAL/RV10", INTER),
        ("V_REAL/RV20", INTER),
        ("V_REAL/RV30", INTER),
        ("V_REAL/RV40", INTER),
        ("V_SNOW", INTER),
        ("V_THEORA", Codec::Theora),
        ("V_UNCOMPRESSED", INTRA),
        ("V_VP8", Codec::Vp8),
        ("V_VP9", Codec::Vp9),
    ]
};

impl Codec {
    /// The codec of a track with `CodecID` `codec_id`. A
    /// `V_MS/VFW/FOURCC`, `A_MS/ACM` or `V_QUICKTIME` track tunnels another
    /// container's codec tag, which FFmpeg maps through its own tag tables;
    /// `resolved` — the codec the registry resolved the track to — stands
    /// in for that mapping.
    pub(super) fn of(codec_id: &str, resolved: &str, audio: bool) -> Codec {
        if matches!(codec_id, "V_MS/VFW/FOURCC" | "A_MS/ACM" | "V_QUICKTIME") {
            return match resolved {
                "h264" => Codec::H264,
                "h265" | "hevc" => Codec::Hevc,
                "vp3" => Codec::Vp3,
                "vp8" => Codec::Vp8,
                "vp9" => Codec::Vp9,
                "av1" => Codec::Av1,
                "theora" => Codec::Theora,
                "aac" => Codec::Aac,
                "mlp" => Codec::Mlp,
                "truehd" => Codec::TrueHd,
                "mjpeg" | "prores" | "rawvideo" | "jpeg2000" => Codec::Other { intra_only: true },
                _ => Codec::Other { intra_only: audio },
            };
        }
        MKV_CODEC_TAGS
            .iter()
            .find(|(prefix, _)| codec_id.starts_with(prefix))
            .map_or(Codec::Unknown, |&(_, codec)| codec)
    }

    /// FFmpeg flags every packet of this codec a keyframe
    /// (`ff_is_intra_only`).
    pub(super) fn intra_only(self) -> bool {
        matches!(self, Codec::Other { intra_only: true })
    }
}

/// `AVPictureType`, as far as the keyframe rule needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PictType {
    None,
    I,
    P,
    B,
    Other,
}

/// A codec parser's view of the last frame it read: FFmpeg's
/// `AVCodecParserContext.key_frame` / `.pict_type`. Both persist from one
/// packet to the next where the parser leaves them untouched (a VP8 frame
/// too short to read, say), as FFmpeg's do.
pub(super) struct FrameParser {
    kind: Kind,
    /// `-1` (not known), `0` or `1`.
    key_frame: i8,
    pict_type: PictType,
}

enum Kind {
    H264(Box<H264>),
    Vp3,
    Vp8,
    Vp9,
    Theora,
    Av1 { reduced_still_picture_header: Option<bool> },
    Mlp,
}

impl FrameParser {
    /// The parser FFmpeg runs over the packets of a track of `codec`, or
    /// `None` when it runs none (HEVC video and AAC audio are not parsed,
    /// subtitles neither) — the Matroska keyframe signal then stands.
    /// `extradata` is the track's codec configuration.
    pub(super) fn new(codec: Codec, extradata: &[u8]) -> Option<FrameParser> {
        let kind = match codec {
            Codec::H264 => Kind::H264(Box::new(H264::new(extradata))),
            Codec::Vp3 => Kind::Vp3,
            Codec::Vp8 => Kind::Vp8,
            Codec::Vp9 => Kind::Vp9,
            Codec::Theora => Kind::Theora,
            Codec::Av1 => Kind::Av1 {
                reduced_still_picture_header: av1_config_sequence_header(extradata),
            },
            Codec::Mlp | Codec::TrueHd => Kind::Mlp,
            _ => return None,
        };
        Some(FrameParser {
            kind,
            key_frame: -1,
            pict_type: PictType::I,
        })
    }

    /// Reads `frame` and tells whether FFmpeg flags it a keyframe;
    /// `container_key` is the Block's keyframe signal, which only counts
    /// when the parser read no picture type.
    pub(super) fn keyframe(&mut self, frame: &[u8], container_key: bool) -> bool {
        self.parse(frame);
        self.key_frame == 1
            || (self.key_frame == -1 && self.pict_type == PictType::I)
            || (self.key_frame == -1 && self.pict_type == PictType::None && container_key)
    }
    pub(super) fn reorder_delay(&self) -> Option<usize> {
        match &self.kind {
            Kind::H264(h) => Some(h.reorder_delay.unwrap_or(0) as usize),
            _ => None,
        }
    }
    pub(super) fn has_reorder_restriction(&self) -> bool {
        match &self.kind {
            Kind::H264(h) => h.sps.iter().flatten().any(|s| s.reorder_frames.is_some()),
            _ => false,
        }
    }


    fn parse(&mut self, frame: &[u8]) {
        match &mut self.kind {
            Kind::H264(h264) => {
                let (key_frame, pict_type) = h264.parse(frame);
                self.key_frame = key_frame;
                self.pict_type = pict_type;
            }
            // vp3_parser.c: the frame type bit of the first byte.
            Kind::Vp3 | Kind::Theora => {
                let bit = if matches!(self.kind, Kind::Theora) { 0x40 } else { 0x80 };
                let first = frame.first().copied().unwrap_or(0);
                self.pict_type = if first & bit != 0 { PictType::P } else { PictType::I };
            }
            // vp8_parser.c: the frame tag's key frame bit and version.
            Kind::Vp8 => {
                if frame.len() < 3 || (frame[0] >> 1) & 7 > 3 {
                    return;
                }
                let key = frame[0] & 1 == 0;
                self.key_frame = i8::from(key);
                self.pict_type = if key { PictType::I } else { PictType::P };
            }
            // vp9_parser.c: the uncompressed header's profile,
            // show_existing_frame and frame_type.
            Kind::Vp9 => {
                let mut r = Bits::new(frame);
                if frame.is_empty() {
                    return;
                }
                r.skip(2); // frame_marker
                let mut profile = r.bit() | (r.bit() << 1);
                if profile == 3 {
                    profile += r.bit();
                }
                if profile > 3 {
                    return;
                }
                let key = r.bit() == 0 && r.bit() == 0;
                self.key_frame = i8::from(key);
                self.pict_type = if key { PictType::I } else { PictType::P };
            }
            Kind::Av1 {
                reduced_still_picture_header,
            } => {
                let (key_frame, pict_type) = av1_temporal_unit(frame, reduced_still_picture_header);
                self.key_frame = key_frame;
                self.pict_type = pict_type;
            }
            // mlp_parser.c: an access unit starting a major sync is a
            // keyframe, any other is not.
            Kind::Mlp => {
                self.key_frame = i8::from(
                    frame.len() >= 8
                        && u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]])
                            & 0xffff_fffe
                            == 0xf872_6fba,
                );
            }
        }
    }
}

// --- AV1 -------------------------------------------------------------------

const AV1_OBU_SEQUENCE_HEADER: u8 = 1;
const AV1_OBU_FRAME_HEADER: u8 = 3;
const AV1_OBU_FRAME: u8 = 6;

/// One OBU of a low-overhead bitstream (AV1 §5.3): `(obu_type,
/// spatial_id, payload)`, and the bytes after it.
fn av1_obu(data: &[u8]) -> Option<((u8, u8, &[u8]), &[u8])> {
    let header = *data.first()?;
    let obu_type = (header >> 3) & 0x0f;
    let mut pos = 1;
    let mut spatial_id = 0;
    if header & 0x04 != 0 {
        spatial_id = (data.get(1)? >> 3) & 0x03;
        pos += 1;
    }
    let size = if header & 0x02 != 0 {
        let mut size = 0u64;
        let mut i = 0;
        loop {
            let b = *data.get(pos)?;
            size |= u64::from(b & 0x7f) << (7 * i);
            pos += 1;
            i += 1;
            if b & 0x80 == 0 {
                break;
            }
            if i == 8 {
                return None;
            }
        }
        usize::try_from(size).ok()?
    } else {
        data.len() - pos
    };
    let end = pos.checked_add(size)?;
    Some(((obu_type, spatial_id, data.get(pos..end)?), &data[end..]))
}

/// `reduced_still_picture_header` of the sequence header OBU (AV1
/// §5.5.1) in an `av1C` configuration record's `configOBUs`.
fn av1_config_sequence_header(extradata: &[u8]) -> Option<bool> {
    let mut obus = if extradata.first()? & 0x80 != 0 {
        extradata.get(4..)?
    } else {
        extradata
    };
    while let Some(((obu_type, _, payload), rest)) = av1_obu(obus) {
        if obu_type == AV1_OBU_SEQUENCE_HEADER {
            return Some(payload.first()? & 0x08 != 0);
        }
        obus = rest;
    }
    None
}

/// av1_parser.c over a temporal unit: the key frame flag and picture type
/// of the last frame it shows (spatial layer 0); `(-1, None)` when it
/// shows none or can't be read. Two passes over the OBUs, holding none of
/// them: the first checks they all parse and takes the last sequence
/// header, the second reads the frame headers against it.
fn av1_temporal_unit(data: &[u8], seq: &mut Option<bool>) -> (i8, PictType) {
    let mut obus = data;
    while !obus.is_empty() {
        let Some(((obu_type, _, payload), rest)) = av1_obu(obus) else {
            return (-1, PictType::None);
        };
        if obu_type == AV1_OBU_SEQUENCE_HEADER {
            match payload.first() {
                Some(b) => *seq = Some(b & 0x08 != 0),
                None => return (-1, PictType::None),
            }
        }
        obus = rest;
    }
    let Some(reduced_still_picture_header) = *seq else {
        return (-1, PictType::None);
    };
    let (mut key_frame, mut pict_type) = (-1, PictType::None);
    let mut obus = data;
    while let Some(((obu_type, spatial_id, header), rest)) = av1_obu(obus) {
        obus = rest;
        if !matches!(obu_type, AV1_OBU_FRAME_HEADER | AV1_OBU_FRAME) || spatial_id != 0 {
            continue;
        }
        // §5.9.2 uncompressed_header(): a reduced still picture is a shown
        // key frame; otherwise show_existing_frame, frame_type and
        // show_frame lead the header.
        let mut r = Bits::new(header);
        let (show_existing, frame_type, show_frame) = if reduced_still_picture_header {
            (false, 0, true)
        } else if r.bit() == 1 {
            (true, 1, true)
        } else {
            let frame_type = r.bits(2);
            (false, frame_type, r.bit() == 1)
        };
        if r.overread() {
            return (-1, PictType::None);
        }
        if !show_frame {
            continue;
        }
        // A shown existing frame is never a key frame; its picture type
        // doesn't count then.
        key_frame = i8::from(frame_type == 0 && !show_existing);
        pict_type = match frame_type {
            0 | 2 => PictType::I,
            1 => PictType::P,
            _ => PictType::Other,
        };
    }
    (key_frame, pict_type)
}

// --- H.264 -----------------------------------------------------------------

const H264_NAL_SLICE: u8 = 1;
const H264_NAL_IDR_SLICE: u8 = 5;
const H264_NAL_SEI: u8 = 6;
const H264_NAL_SPS: u8 = 7;
const H264_NAL_PPS: u8 = 8;

/// What h264_parser.c keeps of a sequence parameter set (H.264 §7.3.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct H264Sps {
    /// `max_num_ref_frames`.
    ref_frame_count: u32,
    reorder_frames: Option<u32>,
}

/// What h264_parser.c keeps of a picture parameter set (H.264 §7.3.2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct H264Pps {
    /// The SPS the PPS was decoded against.
    sps: H264Sps,
    /// `num_ref_idx_l0_default_active_minus1 + 1`.
    ref_count0: u32,
}

/// h264_parser.c's state: the parameter sets of the configuration record
/// and of the stream, and the NAL unit framing.
struct H264 {
    /// The `lengthSizeMinusOne + 1` of an `avcC` configuration record;
    /// `None` when the stream uses start codes.
    nal_length_size: Option<usize>,
    sps: [Option<H264Sps>; 32],
    pps: Vec<Option<H264Pps>>,
    reorder_delay: Option<u32>,
}

impl H264 {
    fn new(extradata: &[u8]) -> H264 {
        let mut h = H264 {
            nal_length_size: None,
            sps: [None; 32],
            pps: vec![None; 256],
            reorder_delay: None,
        };
        h.decode_extradata(extradata);
        h.reorder_delay = h.sps.iter().flatten().find_map(|s| s.reorder_frames);
        h
    }

    /// ff_h264_decode_extradata: the SPS / PPS NAL units of an `avcC`
    /// record (ISO/IEC 14496-15 §5.3.3.1), or of an Annex B byte stream.
    fn decode_extradata(&mut self, data: &[u8]) {
        if data.first() != Some(&1) {
            for nal in annex_b_nals(data) {
                self.parameter_set(nal);
            }
            return;
        }
        self.nal_length_size = Some(usize::from(data.get(4).map_or(3, |b| b & 3)) + 1);
        if data.len() < 7 {
            return;
        }
        let mut p = 6;
        let mut count = data[5] & 0x1f;
        for set in 0..2 {
            if set == 1 {
                let Some(&n) = data.get(p) else { return };
                count = n;
                p += 1;
            }
            for _ in 0..count {
                let Some(len) = data.get(p..p + 2) else { return };
                let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
                let Some(nal) = data.get(p + 2..p + 2 + len) else {
                    return;
                };
                self.parameter_set(nal);
                p += 2 + len;
            }
        }
    }

    /// Decodes an SPS or PPS NAL unit; other NAL units are ignored.
    fn parameter_set(&mut self, nal: &[u8]) {
        let Some(&header) = nal.first() else { return };
        let rbsp = rbsp(&nal[1..]);
        match header & 0x1f {
            H264_NAL_SPS => {
                if let Some((id, sps)) = decode_sps(&rbsp) {
                    self.sps[id] = Some(sps);
                }
            }
            H264_NAL_PPS => {
                if let Some((id, pps)) = self.decode_pps(&rbsp) {
                    self.pps[id] = Some(pps);
                }
            }
            _ => {}
        }
    }

    /// ff_h264_decode_picture_parameter_set up to the reference counts.
    fn decode_pps(&self, rbsp: &[u8]) -> Option<(usize, H264Pps)> {
        let mut r = Bits::new(rbsp);
        let pps_id = r.ue() as usize;
        let sps_id = r.ue() as usize;
        let sps = (*self.sps.get(sps_id)?)?;
        if pps_id >= 256 {
            return None;
        }
        r.skip(2); // entropy_coding_mode_flag, bottom_field_pic_order_in_frame_present_flag
        if r.ue() > 0 {
            // FFmpeg reads slice_group_map_type and goes on with the
            // reference counts (FMO is not supported).
            r.ue();
        }
        let ref_count0 = r.ue().wrapping_add(1);
        let ref_count1 = r.ue().wrapping_add(1);
        if r.overread() || ref_count0.wrapping_sub(1) > 31 || ref_count1.wrapping_sub(1) > 31 {
            return None;
        }
        Some((pps_id, H264Pps { sps, ref_count0 }))
    }

    /// h264_parser.c `parse_nal_units`: `(key_frame, pict_type)` of an
    /// access unit — a keyframe when it holds an IDR slice, a recovery
    /// point SEI, or an I slice of a stream with a single reference frame;
    /// the picture type of its first slice. The parameter sets it carries
    /// update the parser's.
    fn parse(&mut self, frame: &[u8]) -> (i8, PictType) {
        let mut key_frame = 0;
        let mut pict_type = PictType::I;
        let mut recovery_point = false;
        let nals: Box<dyn Iterator<Item = &[u8]>> = match self.nal_length_size {
            Some(n) => Box::new(length_prefixed_nals(frame, n)),
            None => Box::new(annex_b_nals(frame)),
        };
        for nal in nals {
            let Some(&header) = nal.first() else { continue };
            match header & 0x1f {
                H264_NAL_SPS | H264_NAL_PPS => self.parameter_set(nal),
                H264_NAL_SEI => recovery_point |= sei_recovery_point(&rbsp(&nal[1..])),
                nal_type @ (H264_NAL_SLICE | H264_NAL_IDR_SLICE) => {
                    if nal_type == H264_NAL_IDR_SLICE {
                        key_frame = 1;
                    }
                    // Only the first bytes of a slice are read.
                    let rbsp = rbsp(&nal[1..nal.len().min(1000)]);
                    let mut r = Bits::new(&rbsp);
                    r.ue(); // first_mb_in_slice
                    pict_type = match r.ue() % 5 {
                        0 => PictType::P,
                        1 => PictType::B,
                        2 => PictType::I,
                        _ => PictType::Other,
                    };
                    if recovery_point {
                        key_frame = 1;
                    }
                    let pps = self.pps.get(r.ue() as usize).copied().flatten();
                    if let Some(pps) = pps {
                        if let Some(delay) = pps.sps.reorder_frames {
                            self.reorder_delay = Some(delay);
                        }
                        if pps.sps.ref_frame_count <= 1
                            && pps.ref_count0 <= 1
                            && pict_type == PictType::I
                        {
                            key_frame = 1;
                        }
                    }
                    if pict_type == PictType::B && self.reorder_delay.unwrap_or(0) == 0 {
                        self.reorder_delay = Some(1);
                    }
                    // The first slice decides; the rest is not read.
                    break;
                }
                _ => {}
            }
        }
        (key_frame, pict_type)
    }
}

/// ff_h264_decode_seq_parameter_set, keeping what the parser needs:
/// `(seq_parameter_set_id, sps)`, `None` for an SPS FFmpeg rejects.
fn decode_sps(rbsp: &[u8]) -> Option<(usize, H264Sps)> {
    let mut r = Bits::new(rbsp);
    let profile_idc = r.bits(8);
    r.skip(16); // constraint flags, reserved bits, level_idc
    let sps_id = r.ue() as usize;
    if sps_id >= 32 {
        return None;
    }
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 144) {
        let chroma_format_idc = r.ue();
        if chroma_format_idc > 3 || (chroma_format_idc == 3 && r.bit() == 1) {
            return None;
        }
        let (luma, chroma) = (r.ue(), r.ue());
        if luma != chroma || luma > 6 {
            return None;
        }
        r.skip(1); // qpprime_y_zero_transform_bypass_flag
        if r.bit() == 1 {
            // seq_scaling_matrix_present_flag (§7.3.2.1.1.1).
            for i in 0..if chroma_format_idc == 3 { 12 } else { 8 } {
                if r.bit() == 1 {
                    let mut last = 8i64;
                    let mut next = 8i64;
                    for _ in 0..if i < 6 { 16 } else { 64 } {
                        if next != 0 {
                            next = (last + i64::from(r.se()) + 256).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    if r.ue() > 12 {
        return None; // log2_max_frame_num_minus4
    }
    match r.ue() {
        0 => {
            if r.ue() > 12 {
                return None; // log2_max_pic_order_cnt_lsb_minus4
            }
        }
        1 => {
            r.skip(1); // delta_pic_order_always_zero_flag
            r.se(); // offset_for_non_ref_pic
            r.se(); // offset_for_top_to_bottom_field
            let cycle = r.ue();
            if cycle >= 256 {
                return None;
            }
            for _ in 0..cycle {
                r.se(); // offset_for_ref_frame
            }
        }
        2 => {}
        _ => return None,
    }
    let ref_frame_count = r.ue();
    if ref_frame_count > 16 {
        return None;
    }
    if r.overread() {
        return None;
    }
    let reorder_frames = h264_reorder_frames(&mut r);
    Some((sps_id, H264Sps { ref_frame_count, reorder_frames }))
}

/// H.264 SPS tail and VUI bitstream_restriction (Annex E.1.1).
fn h264_reorder_frames(r: &mut Bits<'_>) -> Option<u32> {
    r.skip(1); // gaps_in_frame_num_value_allowed_flag
    r.ue(); r.ue(); // picture dimensions
    if r.bit() == 0 { r.skip(1); } // mb_adaptive_frame_field_flag
    r.skip(1); // direct_8x8_inference_flag
    if r.bit() != 0 { for _ in 0..4 { r.ue(); } }
    if r.bit() == 0 { return None; } // vui_parameters_present_flag
    if r.bit() != 0 && r.bits(8) == 255 { r.skip(32); }
    if r.bit() != 0 { r.skip(1); } // overscan
    if r.bit() != 0 {
        r.skip(4); // video_format, video_full_range_flag
        if r.bit() != 0 { r.skip(24); }
    }
    if r.bit() != 0 { r.ue(); r.ue(); } // chroma location
    if r.bit() != 0 { r.skip(65); } // timing information
    let nal_hrd = r.bit() != 0;
    if nal_hrd { skip_hrd(r)?; }
    let vcl_hrd = r.bit() != 0;
    if vcl_hrd { skip_hrd(r)?; }
    if nal_hrd || vcl_hrd { r.skip(1); }
    r.skip(1); // pic_struct_present_flag
    if r.bit() == 0 { return None; }
    r.skip(1);
    for _ in 0..4 { r.ue(); }
    let reorder = r.ue();
    r.ue(); // max_dec_frame_buffering
    (!r.overread() && reorder <= 16).then_some(reorder)
}

fn skip_hrd(r: &mut Bits<'_>) -> Option<()> {
    let count = r.ue();
    if count > 31 { return None; }
    r.skip(8);
    for _ in 0..=count { r.ue(); r.ue(); r.skip(1); }
    r.skip(20);
    (!r.overread()).then_some(())
}

/// Whether an SEI RBSP (H.264 §7.3.2.3) carries a valid recovery point
/// message (payload type 6, §D.1.8), as ff_h264_sei_decode reads it.
fn sei_recovery_point(rbsp: &[u8]) -> bool {
    let mut p = 0;
    while rbsp.len() - p > 2 && (rbsp[p] != 0 || rbsp[p + 1] != 0) {
        let mut field = || {
            let mut v = 0usize;
            loop {
                let b = *rbsp.get(p)?;
                p += 1;
                v += usize::from(b);
                if b != 0xff {
                    return Some(v);
                }
            }
        };
        let (Some(payload_type), Some(size)) = (field(), field()) else {
            return false;
        };
        if size > rbsp.len() - p {
            return false;
        }
        if payload_type == 6 {
            let mut r = Bits::new(&rbsp[p..p + size]);
            // recovery_frame_cnt must be below 2^16 (MAX_LOG2_MAX_FRAME_NUM).
            return r.ue_long() < 1 << 16;
        }
        p += size;
    }
    false
}

/// The NAL units of an access unit in `avcC` sample format: each prefixed
/// by its `n`-byte length. A length that runs past the data ends it.
fn length_prefixed_nals(mut data: &[u8], n: usize) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        if data.len() <= n {
            return None;
        }
        let len = data[..n].iter().fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        if len == 0 || len > data.len() - n {
            return None;
        }
        let nal = &data[n..n + len];
        data = &data[n + len..];
        Some(nal)
    })
}

/// The NAL units of an Annex B byte stream, split at start codes: each runs
/// from after its start code to the next one, less the zero bytes before
/// it. Found as the iteration goes, so splitting holds no memory however
/// many start codes the data packs.
fn annex_b_nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    // The offset just past the first start code at or after `from`.
    let next_start = move |from: usize| {
        let mut i = from;
        while i + 3 <= data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                return Some(i + 3);
            }
            i += 1;
        }
        None
    };
    let mut start = next_start(0);
    std::iter::from_fn(move || {
        let nal_start = start?;
        let following = next_start(nal_start);
        let mut end = following.map_or(data.len(), |next| next - 3);
        while end > nal_start && data[end - 1] == 0 {
            end -= 1;
        }
        start = following;
        Some(&data[nal_start..end])
    })
}

/// The RBSP of a NAL unit payload: emulation prevention bytes (`00 00
/// 03`) removed, and the payload ended at a start code (`00 00 00` – `00
/// 00 02`), as ff_h2645_extract_rbsp does.
fn rbsp(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut i = 0;
    while i < ebsp.len() {
        if i + 2 < ebsp.len() && ebsp[i] == 0 && ebsp[i + 1] == 0 {
            match ebsp[i + 2] {
                3 => {
                    out.extend_from_slice(&[0, 0]);
                    i += 3;
                    continue;
                }
                0..=2 => break,
                _ => {}
            }
        }
        out.push(ebsp[i]);
        i += 1;
    }
    out
}

// --- Bits ------------------------------------------------------------------

/// A big-endian bit reader that, like FFmpeg's `GetBitContext`, reads
/// zeros past the end of its data and records that it did.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }

    fn bit(&mut self) -> u32 {
        let b = self
            .data
            .get(self.pos / 8)
            .map_or(0, |byte| u32::from(byte >> (7 - self.pos % 8)) & 1);
        self.pos += 1;
        b
    }

    /// `u(n)`, `n <= 32`.
    fn bits(&mut self, n: u32) -> u32 {
        (0..n).fold(0, |v, _| (v << 1) | self.bit())
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    /// Whether a read went past the end of the data.
    fn overread(&self) -> bool {
        self.pos > self.data.len() * 8
    }

    /// `ue(v)` limited to 31 leading zeros (`get_ue_golomb_31` /
    /// `get_ue_golomb`); a longer prefix reads as `u32::MAX`.
    fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while self.bit() == 0 {
            zeros += 1;
            if zeros > 31 || self.overread() {
                return u32::MAX;
            }
        }
        ((1u64 << zeros) - 1 + u64::from(self.bits(zeros))) as u32
    }

    /// `ue(v)` up to 32 bits (`get_ue_golomb_long`).
    fn ue_long(&mut self) -> u64 {
        let mut zeros = 0;
        while self.bit() == 0 {
            zeros += 1;
            if zeros > 32 || self.overread() {
                return u64::MAX;
            }
        }
        (1u64 << zeros) - 1 + u64::from(self.bits(zeros))
    }

    /// `se(v)`.
    fn se(&mut self) -> i32 {
        let k = i64::from(self.ue());
        (if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) }) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
        let mut r = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
        r.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        r.extend_from_slice(sps);
        r.push(1);
        r.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        r.extend_from_slice(pps);
        r
    }

    fn sample(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            out.extend_from_slice(nal);
        }
        out
    }

    // Baseline SPS 0: log2_max_frame_num-4 = 0, poc type 2, two
    // reference frames: 1 1 011 011 …
    const SPS_TWO_REFS: &[u8] = &[0x67, 66, 0xc0, 0x1e, 0b1101_1011, 0b0000_1000];
    // PPS 0 on SPS 0, CAVLC, one slice group, ref counts 1 / 1:
    // 1 1 0 0 1 1 1 …
    const PPS: &[u8] = &[0x68, 0b1100_1110, 0b0000_0000];

    #[test]
    fn h264_idr_recovery_point_and_slice_types() {
        let mut p = FrameParser::new(Codec::H264, &avcc(SPS_TWO_REFS, PPS)).unwrap();
        // IDR slice (type 5), I slice: first_mb 0 `1`, slice_type 7 `0001000`.
        assert!(p.keyframe(&sample(&[&[0x65, 0b1000_1000, 0x80]]), false));
        // Non-IDR I slice of a two-reference stream: not a keyframe…
        assert!(!p.keyframe(&sample(&[&[0x41, 0b1000_1000, 0x80]]), true));
        // …unless a recovery point SEI precedes it: payload 6, size 1,
        // recovery_frame_cnt 0 `1`.
        let sei: &[u8] = &[0x06, 0x06, 0x01, 0b1000_0000, 0x80];
        assert!(p.keyframe(&sample(&[sei, &[0x41, 0b1000_1000, 0x80]]), false));
        // A P slice (type 5 → `00110`).
        assert!(!p.keyframe(&sample(&[&[0x41, 0b1001_1000, 0x80]]), true));
    }

    #[test]
    fn vp8_vp9_theora_mlp_frame_types() {
        let mut vp8 = FrameParser::new(Codec::Vp8, &[]).unwrap();
        assert!(vp8.keyframe(&[0x10, 0, 0], false));
        assert!(!vp8.keyframe(&[0x11, 0, 0], true));
        // Too short to read: the previous verdict stands.
        assert!(!vp8.keyframe(&[0x10], true));
        let mut vp9 = FrameParser::new(Codec::Vp9, &[]).unwrap();
        // frame_marker 10, profile 0, show_existing 0, frame_type 0.
        assert!(vp9.keyframe(&[0b1000_0000], false));
        assert!(!vp9.keyframe(&[0b1000_0100], true));
        let mut theora = FrameParser::new(Codec::Theora, &[]).unwrap();
        assert!(theora.keyframe(&[0x00], false));
        assert!(!theora.keyframe(&[0x40], true));
        let mut mlp = FrameParser::new(Codec::TrueHd, &[]).unwrap();
        assert!(mlp.keyframe(&[0, 0, 0, 0, 0xf8, 0x72, 0x6f, 0xba], false));
        assert!(!mlp.keyframe(&[0, 0, 0, 0, 0, 0, 0, 0], true));
    }

    #[test]
    fn av1_shown_key_frames() {
        // Sequence header OBU (type 1, sized): profile 0, not still.
        let seq = [0x0a, 0x01, 0x00];
        let mut av1 = FrameParser::new(Codec::Av1, &[]).unwrap();
        // Frame OBU (type 6, sized): show_existing 0, KEY_FRAME, shown.
        assert!(av1.keyframe(&[&seq[..], &[0x32, 0x01, 0b0001_0000]].concat(), false));
        // An INTER frame.
        assert!(!av1.keyframe(&[0x32, 0x01, 0b0011_0000], true));
        // No shown frame: the Block's signal decides.
        assert!(av1.keyframe(&[0x32, 0x01, 0b0000_0000], true));
        assert!(!av1.keyframe(&[0x32, 0x01, 0b0000_0000], false));
    }

    #[test]
    fn codec_identity_follows_ffmpeg_tags() {
        assert_eq!(Codec::of("A_AAC/MPEG4/LC", "aac", true), Codec::Aac);
        assert_eq!(Codec::of("V_MPEG4/ISO/AVC", "h264", false), Codec::H264);
        assert!(Codec::of("A_OPUS", "opus", true).intra_only());
        assert!(Codec::of("S_TEXT/UTF8", "subrip", false).intra_only());
        assert!(!Codec::of("S_TEXT/WEBVTT", "webvtt", false).intra_only());
        assert_eq!(Codec::of("V_MS/VFW/FOURCC", "h264", false), Codec::H264);
    }
}
