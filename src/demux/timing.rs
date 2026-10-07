//! Codec frame durations needed when a lace has no container timestamp.
//! Matroska specifies only the first timestamp in a duration-less Block;
//! FFmpeg times each later lace by the frame durations before it: from a
//! codec parser's reading of the frame where libavformat runs one, else
//! from the decoder's frame size.
use oxideav_core::{CodecParameters, TimeBase};

/// The bounded PTS window used to infer DTS for reordered video. Missing
/// slots sort before real timestamps, retaining N/A during initial delay.
pub(super) struct DecodeOrder {
    delay: usize,
    pts: [Option<i64>; 17],
    probe: Option<H264Probe>,
}

struct H264Probe {
    frames: usize,
    explicit: bool,
    found_dts: bool,
}

impl DecodeOrder {
    pub(super) fn new(delay: usize) -> Self {
        Self { delay, pts: [None; 17], probe: None }
    }
    pub(super) fn h264(delay: usize, explicit: bool) -> Self {
        let mut order = Self::new(delay);
        if !explicit || delay > 0 {
            order.probe = Some(H264Probe { frames: 0, explicit, found_dts: false });
        }
        order
    }
    pub(super) fn reset(&mut self) {
        self.pts.fill(None);
        self.probe = None;
    }
    pub(super) fn needs_probe(&self) -> bool {
        self.probe.as_ref().is_some_and(|p| {
            if p.explicit { !p.found_dts } else { p.frames < 7 }
        })
    }
    /// Once delay is known, replay the queued prefix just as the reference
    /// parser does. A restricted SPS that never produced a DTS at EOF keeps
    /// N/A, rather than inventing a timestamp for an incomplete sequence.
    pub(super) fn finish_probe(&mut self) -> bool {
        let replay = self.probe.take().is_some_and(|p| !p.explicit || p.found_dts);
        if replay { self.pts.fill(None); }
        replay
    }
    pub(super) fn dts(&mut self, pts: Option<i64>, delay: Option<usize>) -> Option<i64> {
        if let Some(delay) = delay { self.delay = delay; }
        if let Some(probe) = &mut self.probe {
            probe.frames += 1;
            if probe.explicit && probe.frames == 1 {
                // The H.264 decoder learns SPS reorder delay after the first
                // packet was parsed; that initial PTS slot is overwritten.
                self.pts[0] = pts;
                return None;
            }
        }
        if self.delay >= self.pts.len() { return None; }
        if let Some(pts) = pts {
            self.pts[0] = Some(pts);
            for i in 0..self.delay {
                if self.pts[i] <= self.pts[i + 1] { break; }
                self.pts.swap(i, i + 1);
            }
            if let Some(probe) = &mut self.probe {
                probe.found_dts |= self.pts[0].is_some();
            }
            self.pts[0]
        } else {
            None
        }
    }
}

/// How long an audio packet lasts, as libavformat derives it to time a
/// following lace that has no timestamp (`compute_pkt_fields`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Span {
    /// Whole ticks: a container lace duration, or a codec parser's frame
    /// duration rounded down. The next lace starts exactly this much later.
    Ticks(i64),
    /// `samples / rate` seconds from the decoder's frame size, where no
    /// parser reports a duration. The packet's own duration rounds down;
    /// the next lace advances by FFmpeg's `av_add_stable`, which rounds the
    /// running time instead, so a frame that is not a whole number of ticks
    /// does not drift.
    Samples { samples: u32, rate: u32 },
}

impl Span {
    /// The packet duration in `tb` ticks.
    fn ticks(self, tb: TimeBase) -> i64 {
        match self {
            Span::Ticks(ticks) => ticks,
            Span::Samples { samples, rate } => floor_ticks(samples, rate, tb).unwrap_or(0),
        }
    }
}

pub(super) struct PacketClock {
    codec: AudioTiming,
    /// The sample rate a parsed frame duration counts in.
    rate: u32,
    next: i64,
}
enum AudioTiming {
    None,
    Opus,
    Flac,
    WavPack,
    Vorbis(Vorbis),
    /// FFmpeg runs no parser over Matroska AAC: a packet lasts the
    /// decoder's frame, `frame` samples at the core `rate`, which FFmpeg
    /// knows once stream probing has decoded the first packet.
    Aac { frame: u32, rate: u32, decoded: bool },
    Mp3(Mp3),
    Ac3(Ac3),
    Dts,
}

impl PacketClock {
    pub(super) fn new(params: &CodecParameters) -> Self {
        let rate = params.sample_rate.unwrap_or(0);
        let codec = match params.codec_id.as_str() {
            "opus" => AudioTiming::Opus,
            "flac" => AudioTiming::Flac,
            "wavpack" => AudioTiming::WavPack,
            "vorbis" => Vorbis::new(&params.extradata).map(AudioTiming::Vorbis).unwrap_or(AudioTiming::None),
            "aac" => crate::aac::decoder_frame(&params.extradata)
                .map_or(AudioTiming::None, |(frame, rate)| AudioTiming::Aac { frame, rate, decoded: false }),
            "mp3" => AudioTiming::Mp3(Mp3 { layer: 3, rate, header: 0, count: 0, samples: None }),
            "ac3" | "eac3" => AudioTiming::Ac3(Ac3 { samples: None, rate, eac3: params.codec_id.as_str() == "eac3" }),
            "dts" => AudioTiming::Dts,
            _ => AudioTiming::None,
        };
        Self { codec, rate: if params.codec_id.as_str() == "opus" { 48000 } else { rate }, next: 0 }
    }

    pub(super) fn reset(&mut self) {
        self.next = 0;
        match &mut self.codec {
            AudioTiming::Vorbis(v) => v.previous = v.initial,
            // A seek re-creates libavformat's parsers. The decoder context,
            // with the AAC frame size and the parsers' sample rates, stays.
            AudioTiming::Mp3(m) => {
                m.header = 0;
                m.count = 0;
                m.samples = None;
            }
            AudioTiming::Ac3(a) => a.samples = None,
            AudioTiming::Aac { decoded, .. } => *decoded = true,
            _ => {}
        }
    }

    /// The duration FFmpeg gives `data`, the next frame of this track.
    /// Parsers keep state across packets, so every frame passes here.
    pub(super) fn frame_span(&mut self, data: &[u8], tb: TimeBase) -> Option<Span> {
        let ticks = |samples: u32, rate: u32| floor_ticks(samples, rate, tb).map(Span::Ticks);
        match &mut self.codec {
            AudioTiming::None => None,
            AudioTiming::Opus => ticks(opus_samples(data)?, self.rate),
            AudioTiming::Flac => ticks(flac_samples(data)?, self.rate),
            // This runs on Matroska's compact WavPack frame before rebuilding
            // its standard block headers. Its first word is block_samples.
            AudioTiming::WavPack => ticks(u32::from_le_bytes(data.get(..4)?.try_into().ok()?), self.rate),
            AudioTiming::Vorbis(v) => ticks(v.samples(data)?, self.rate),
            AudioTiming::Aac { frame, rate, decoded } => {
                std::mem::replace(decoded, true).then_some(Span::Samples { samples: *frame, rate: *rate })
            }
            AudioTiming::Mp3(m) => {
                m.parse(data);
                m.span(tb)
            }
            AudioTiming::Ac3(a) => {
                if let Some((samples, rate, eac3)) = ac3_frames(data) {
                    a.samples = Some(samples);
                    a.rate = rate;
                    a.eac3 |= eac3;
                }
                a.span(tb)
            }
            AudioTiming::Dts => {
                let (samples, core_rate) = dts_core(data)?;
                // The decoder context keeps the container's rate, or takes
                // the first core's; the parser counts the frame in it.
                if self.rate == 0 {
                    self.rate = core_rate;
                }
                let samples = rescale_near(samples.into(), self.rate.into(), core_rate.into())?;
                ticks(u32::try_from(samples).ok()?, self.rate)
            }
        }
    }

    /// The timestamp and duration of a packet lasting `span`: `pts` for a
    /// Block's first lace, the end of the previous packet for a later lace
    /// with a nonzero duration (none without one).
    pub(super) fn timestamp(&mut self, pts: Option<i64>, span: Option<Span>, tb: TimeBase) -> (Option<i64>, Option<i64>) {
        let duration = span.map(|span| span.ticks(tb));
        let pts = pts.or_else(|| duration.filter(|&d| d > 0).map(|_| self.next));
        if let Some(pts) = pts {
            self.next = match span {
                Some(Span::Samples { samples, rate }) => add_stable(tb, pts, samples, rate),
                Some(Span::Ticks(d)) => pts.saturating_add(d),
                None => pts,
            };
        }
        (pts, duration)
    }
}

/// `samples / rate` seconds in whole `tb` ticks, rounded down.
fn floor_ticks(samples: u32, rate: u32, tb: TimeBase) -> Option<i64> {
    let tb = tb.as_rational();
    let den = i128::from(rate) * i128::from(tb.num);
    if den <= 0 {
        return None;
    }
    i64::try_from(i128::from(samples) * i128::from(tb.den) / den).ok()
}

/// `a * b / c`, rounded to nearest with halves away from zero; `None` on
/// overflow or a zero divisor.
pub(super) fn rescale_near(a: i128, b: i128, c: i128) -> Option<i128> {
    if c <= 0 {
        return None;
    }
    let n = a.checked_abs()?.checked_mul(b)?.checked_add(c / 2)? / c;
    Some(if a < 0 { -n } else { n })
}

/// FFmpeg's `av_add_stable`: `ts` advanced by `samples / rate` seconds in
/// `tb` ticks. Converting to whole frames and back, it rounds the running
/// time rather than each step.
fn add_stable(tb: TimeBase, ts: i64, samples: u32, rate: u32) -> i64 {
    let tb = tb.as_rational();
    let m = i128::from(samples) * i128::from(tb.den);
    let d = i128::from(rate) * i128::from(tb.num);
    if d <= 0 {
        return ts;
    }
    if m % d == 0 {
        return ts.saturating_add(i64::try_from(m / d).unwrap_or(i64::MAX));
    }
    if m < d {
        return ts;
    }
    let stable = || {
        let frames = rescale_near(ts.into(), d, m)?;
        let back = rescale_near(frames, m, d)?;
        let next = rescale_near(frames + 1, m, d)?;
        i64::try_from(next + (i128::from(ts) - back)).ok()
    };
    stable().unwrap_or(ts)
}

/// FFmpeg's MPEG audio parser over Matroska's MP3 frames: a header updates
/// the frame duration and the decoder context's sample rate only once it
/// has been seen consistently: at once, after a change of version, layer
/// or sample rate on the fourth header, and after a change of layer from
/// the codec the context names on the second.
struct Mp3 {
    /// The layer of the codec the decoder context names.
    layer: u32,
    /// The decoder context's sample rate.
    rate: u32,
    header: u32,
    count: i32,
    samples: Option<u32>,
}

impl Mp3 {
    /// Sync, version, layer and sample-rate bits.
    const SAME_HEADER: u32 = 0xffe0_0000 | (3 << 19) | (3 << 17) | (3 << 10);

    fn parse(&mut self, data: &[u8]) {
        let Some(head) = data.get(..4).and_then(|h| h.try_into().ok()).map(u32::from_be_bytes) else {
            return;
        };
        let Some((layer, rate, samples)) = mpeg_audio_header(head) else {
            // The parser scans on through a frame that starts with no
            // header, and every position it rejects resets the count.
            if data.len() > 4 {
                self.count = -2;
            }
            return;
        };
        let threshold = i32::from(layer != self.layer);
        if self.header != 0 && (head ^ self.header) & Self::SAME_HEADER != 0 {
            self.count = -3;
        }
        self.header = head;
        self.count += 1;
        if self.count > threshold {
            self.layer = layer;
            self.rate = rate;
            self.samples = Some(samples);
        }
    }

    fn span(&self, tb: TimeBase) -> Option<Span> {
        let parsed = self.samples.and_then(|samples| floor_ticks(samples, self.rate, tb)).filter(|&t| t > 0);
        if let Some(ticks) = parsed {
            return Some(Span::Ticks(ticks));
        }
        // libavformat's default frame size for the codec.
        let samples = match self.layer {
            1 => 384,
            2 => 1152,
            _ if self.rate <= 24000 => 576,
            _ => 1152,
        };
        (self.rate > 0).then_some(Span::Samples { samples, rate: self.rate })
    }
}

/// Layer, sample rate and samples per frame of an MPEG audio frame header
/// (ISO/IEC 11172-3 §2.4.2.3, 13818-3, and MPEG 2.5); `None` without sync,
/// for a reserved field, and for free format, which FFmpeg's parser
/// cannot frame.
fn mpeg_audio_header(h: u32) -> Option<(u32, u32, u32)> {
    let version = (h >> 19) & 3;
    let layer = 4 - ((h >> 17) & 3);
    let bitrate = (h >> 12) & 15;
    let rate = (h >> 10) & 3;
    if h & 0xffe0_0000 != 0xffe0_0000 || version == 1 || layer == 4 || bitrate == 0 || bitrate == 15 || rate == 3 {
        return None;
    }
    let rate = [44100, 48000, 32000][rate as usize] >> [2, 0, 1, 0][version as usize];
    let samples = match layer {
        1 => 384,
        2 => 1152,
        _ if version == 3 => 1152,
        _ => 576,
    };
    Some((layer, rate, samples))
}

/// FFmpeg's AC-3 parser over Matroska's (E-)AC-3 packets. A packet's frame
/// duration and sample rate come from its last frame; a packet that fails
/// to parse keeps the previous ones.
struct Ac3 {
    samples: Option<u32>,
    /// The decoder context's sample rate.
    rate: u32,
    /// The decoder context names E-AC-3, which has no default frame size.
    eac3: bool,
}

impl Ac3 {
    fn span(&self, tb: TimeBase) -> Option<Span> {
        let parsed = self.samples.and_then(|samples| floor_ticks(samples, self.rate, tb)).filter(|&t| t > 0);
        if let Some(ticks) = parsed {
            return Some(Span::Ticks(ticks));
        }
        // libavformat's default AC-3 frame size.
        (!self.eac3 && self.rate > 0).then_some(Span::Samples { samples: 1536, rate: self.rate })
    }
}

/// Samples, sample rate and E-AC-3-ness of the last frame of an (E-)AC-3
/// packet, as FFmpeg's parser accepts it: every frame from the first sync
/// word must parse and the frames must end with the packet, the last one
/// passing its CRC.
fn ac3_frames(data: &[u8]) -> Option<(u32, u32, bool)> {
    // The first even/odd-aligned sync word pair, either byte order.
    let start = (1..data.len()).step_by(2).find_map(|i| {
        if data[i] != 0x77 && data[i] != 0x0b {
            None
        } else if data[i] ^ data[i - 1] == 0x77 ^ 0x0b {
            Some(i - 1)
        } else if data.get(i + 1).is_some_and(|&b| data[i] ^ b == 0x77 ^ 0x0b) {
            Some(i)
        } else {
            None
        }
    })?;
    let mut frames = &data[start..];
    loop {
        let (size, samples, rate, eac3) = ac3_header(frames)?;
        match frames.len().cmp(&size) {
            std::cmp::Ordering::Less => return None,
            std::cmp::Ordering::Greater => frames = &frames[size..],
            std::cmp::Ordering::Equal => {
                // crc2 makes the CRC of everything after the sync word zero
                // (ATSC A/52 §5.4.1.4, §E.1.3.5).
                let crc = frames[2..].iter().fold(0u16, |crc, &b| (crc << 8) ^ AC3_CRC[usize::from((crc >> 8) as u8 ^ b)]);
                return (crc == 0).then_some((samples, rate, eac3));
            }
        }
    }
}

/// Frame size in bytes, samples, sample rate and E-AC-3-ness of an AC-3
/// (ATSC A/52 §5.4.1) or E-AC-3 (§E.1.2) sync frame header. Bytes past
/// the end read as zero, as FFmpeg's bit reader does.
fn ac3_header(frame: &[u8]) -> Option<(usize, u32, u32, bool)> {
    let byte = |i: usize| frame.get(i).copied().unwrap_or(0);
    if [byte(0), byte(1)] != [0x0b, 0x77] {
        return None;
    }
    const RATES: [u32; 3] = [48000, 44100, 32000];
    let bsid = byte(5) >> 3;
    let fscod = usize::from(byte(4) >> 6);
    if bsid <= 10 {
        let frmsizecod = usize::from(byte(4) & 0x3f);
        if fscod == 3 || frmsizecod > 37 {
            return None;
        }
        let words = AC3_FRAME_WORDS[frmsizecod / 2][fscod] + u16::from(fscod == 1 && frmsizecod % 2 == 1);
        // Half- and quarter-rate AC-3 (bsid 9 and 10).
        let rate = RATES[fscod] >> bsid.saturating_sub(8);
        Some((usize::from(words) * 2, 1536, rate, false))
    } else if bsid <= 16 {
        // Reserved stream type; FFmpeg parses substream 0 only.
        if byte(2) >> 6 == 3 || (byte(2) >> 3) & 7 != 0 {
            return None;
        }
        let size = ((usize::from(byte(2) & 7) << 8 | usize::from(byte(3))) + 1) * 2;
        if size < 7 {
            return None;
        }
        let (blocks, rate) = if fscod == 3 {
            let fscod2 = usize::from((byte(4) >> 4) & 3);
            (6, *RATES.get(fscod2)? / 2)
        } else {
            ([1, 2, 3, 6][usize::from((byte(4) >> 4) & 3)], RATES[fscod])
        };
        Some((size, blocks * 256, rate, true))
    } else {
        None
    }
}

/// 16-bit words per AC-3 frame for each bit rate (frmsizecod / 2) at 48,
/// 44.1 and 32 kHz (ATSC A/52 Table 5.18); the odd 44.1 kHz codes add a
/// word.
const AC3_FRAME_WORDS: [[u16; 3]; 19] = [
    [64, 69, 96], [80, 87, 120], [96, 104, 144], [112, 121, 168], [128, 139, 192],
    [160, 174, 240], [192, 208, 288], [224, 243, 336], [256, 278, 384], [320, 348, 480],
    [384, 417, 576], [448, 487, 672], [512, 557, 768], [640, 696, 960], [768, 835, 1152],
    [896, 975, 1344], [1024, 1114, 1536], [1152, 1253, 1728], [1280, 1393, 1920],
];

/// CRC-16 with generator x^16 + x^15 + x^2 + 1, most significant bit first.
const AC3_CRC: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// Samples and sample rate of a DTS Coherent Acoustics core frame (ETSI
/// TS 102 114 §5.3.1) in 16-bit big- or little-endian words, with the
/// header checks FFmpeg's DCA parser makes. `None` for 14-bit and
/// substream-only (DTS Express, lossless-only) frames, which are not timed.
fn dts_core(data: &[u8]) -> Option<(u32, u32)> {
    let raw = data.get(..18)?;
    let mut header = [0u8; 18];
    match u32::from_be_bytes(raw[..4].try_into().ok()?) {
        0x7ffe_8001 => header.copy_from_slice(raw),
        0xfe7f_0180 => {
            for (to, from) in header.chunks_exact_mut(2).zip(raw.chunks_exact(2)) {
                to.copy_from_slice(&[from[1], from[0]]);
            }
        }
        _ => return None,
    }
    let bits = |at: usize, n: usize| (0..n).fold(0u32, |v, i| v << 1 | u32::from(header[(at + i) / 8] >> (7 - (at + i) % 8) & 1));
    const RATES: [u32; 16] = [0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 96000, 192000];
    let deficit = bits(33, 5) + 1;
    let crc = bits(38, 1) == 1;
    let blocks = bits(39, 7) + 1;
    let size = bits(46, 14) + 1;
    let audio_mode = bits(60, 6);
    let rate = RATES[bits(66, 4) as usize];
    let pcm = bits(88 + if crc { 16 } else { 0 } + 7, 3);
    // Full-length blocks of 8 subband samples, a real frame size, one of
    // the ten core channel arrangements, the reserved bit clear, a valid
    // LFE flag and source resolution.
    let valid = deficit == 32 && blocks % 8 == 0 && size >= 96 && audio_mode < 10 && rate != 0
        && bits(75, 1) == 0 && bits(85, 2) != 3 && pcm != 4 && pcm != 7;
    valid.then_some((blocks * 32, rate))
}

fn opus_samples(data: &[u8]) -> Option<u32> {
    let toc = *data.first()?;
    let config = toc >> 3;
    let samples = if config >= 16 { 120 << (config & 3) }
        else if config >= 12 { 480 << (config & 1) }
        else { [480, 960, 1920, 2880][(config & 3) as usize] };
    let count = match toc & 3 { 0 => 1, 1 | 2 => 2, _ => (data.get(1)? & 63) as u32 };
    let total = samples * count;
    (count > 0 && total <= 5760).then_some(total)
}

fn flac_samples(data: &[u8]) -> Option<u32> {
    if data.len() < 6 || data[0] != 0xff || data[1] & 0xfe != 0xf8 { return None; }
    let code = data[2] >> 4;
    Some(match code {
        1 => 192,
        2..=5 => 576 << (code - 2),
        6 | 7 => {
            let lead = data[4].leading_ones() as usize;
            if lead == 1 || lead > 7 { return None; }
            let at = 4 + lead.max(1);
            if code == 6 { *data.get(at)? as u32 + 1 }
            else { u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?) as u32 + 1 }
        }
        8..=15 => 256 << (code - 8),
        _ => return None,
    })
}

struct Vorbis {
    sizes: [u32; 2],
    modes: [bool; 64],
    count: usize,
    mode_bits: u32,
    /// The window before the first packet: mode 0's (FFmpeg's
    /// `previous_blocksize` initialisation), restored by a seek.
    initial: u32,
    previous: u32,
}

impl Vorbis {
    fn new(extra: &[u8]) -> Option<Self> {
        if extra.first() != Some(&2) { return None; }
        let mut pos = 1;
        let mut sizes = [0usize; 2];
        for size in &mut sizes {
            loop {
                let b = *extra.get(pos)?;
                pos += 1;
                *size = size.checked_add(b as usize)?;
                if b < 255 { break; }
            }
        }
        let id = extra.get(pos..pos.checked_add(sizes[0])?)?;
        if id.len() < 30 || !id.starts_with(b"\x01vorbis") || id[29] & 1 == 0 { return None; }
        let setup = extra.get(pos.checked_add(sizes[0])?.checked_add(sizes[1])?..)?;
        if setup.len() < 7 || !setup.starts_with(b"\x05vorbis") { return None; }
        // Modes are the fixed-width terminal list of the setup packet: six
        // count bits, then {blockflag:1, windowtype:16, transformtype:16,
        // mapping:8}, then a framing one and zero padding. Read the suffix
        // without allocating a reversed copy of the (large) setup header.
        let last = setup.iter().rposition(|&b| b != 0)?;
        let framing = last * 8 + (7 - setup[last].leading_zeros() as usize);
        let mut count = 0;
        for n in 1..=64 {
            let Some(start) = framing.checked_sub(n * 41) else { break };
            if start < 7 * 8 + 6 { break; }
            if bits(setup, start + 1, 16)? != 0 || bits(setup, start + 17, 16)? != 0
                || bits(setup, start + 33, 8)? > 63 { break; }
            if bits(setup, start - 6, 6)? as usize + 1 == n { count = n; }
        }
        if count == 0 { return None; }
        let mut modes = [false; 64];
        let start = framing - count * 41;
        for (i, mode) in modes[..count].iter_mut().enumerate() { *mode = bits(setup, start + i * 41, 1)? != 0; }
        let sizes = [1u32 << (id[28] & 15), 1u32 << (id[28] >> 4)];
        let mode_bits = usize::BITS - (count - 1).leading_zeros();
        let initial = sizes[usize::from(modes[0])];
        Some(Self { sizes, modes, count, mode_bits, initial, previous: initial })
    }

    fn samples(&mut self, data: &[u8]) -> Option<u32> {
        let first = *data.first()?;
        if first & 1 != 0 { return None; }
        let mode = (first as usize >> 1) & ((1 << self.mode_bits) - 1);
        if mode >= self.count { return None; }
        let long = self.modes[mode];
        let previous = if long { self.sizes[((first >> (self.mode_bits + 1)) & 1) as usize] } else { self.previous };
        let current = self.sizes[usize::from(long)];
        self.previous = current;
        Some((previous + current) / 4)
    }
}

fn bits(data: &[u8], pos: usize, count: usize) -> Option<u32> {
    if pos.checked_add(count)? > data.len().checked_mul(8)? { return None; }
    let mut value = 0;
    for i in 0..count { value |= (((data[(pos + i) / 8] >> ((pos + i) % 8)) & 1) as u32) << i; }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reordered_pts_window_preserves_initial_na() {
        let mut clock = DecodeOrder::new(2);
        let actual: Vec<_> = [0, 160, 80, 40, 120].into_iter()
            .map(|pts| clock.dts(Some(pts), None)).collect();
        assert_eq!(actual, [None, None, Some(0), Some(40), Some(80)]);
        clock.reset();
        assert_eq!(clock.dts(Some(200), None), None);
    }
    #[test]
    fn h264_delayed_sps_keeps_short_sequence_dts_unknown() {
        let mut order = DecodeOrder::h264(4, true);
        for pts in [1086, 1003, 1044, 1211, 1128] {
            assert_eq!(order.dts(Some(pts), Some(4)), None);
        }
        assert!(order.needs_probe());
        assert!(!order.finish_probe());
    }
    #[test]
    fn h264_inferred_delay_replays_prefix() {
        let mut order = DecodeOrder::h264(0, false);
        let pts = [0, 42, 83, 125, 167, 250, 209];
        for (i, &pts) in pts.iter().enumerate() {
            order.dts(Some(pts), Some(usize::from(i >= 6)));
        }
        assert!(!order.needs_probe());
        assert!(order.finish_probe());
        let dts: Vec<_> = pts.into_iter().map(|pts| order.dts(Some(pts), None)).collect();
        assert_eq!(dts, [None, Some(0), Some(42), Some(83), Some(125), Some(167), Some(209)]);
    }
    #[test]
    fn opus_toc_durations() {
        assert_eq!(opus_samples(&[0]), Some(480));
        assert_eq!(opus_samples(&[24]), Some(2880));
        assert_eq!(opus_samples(&[128]), Some(120));
        assert_eq!(opus_samples(&[155, 6]), Some(5760));
        assert_eq!(opus_samples(&[155, 7]), None);
        assert_eq!(opus_samples(&[3]), None);
    }
    #[test]
    fn flac_block_size_codes() {
        assert_eq!(flac_samples(&[255, 248, 0xc0, 0, 0, 0]), Some(4096));
        assert_eq!(flac_samples(&[255, 248, 0x60, 0, 0xc2, 0x80, 127]), Some(128));
        assert_eq!(flac_samples(&[255, 248, 0x70, 0, 0, 0x0f, 0xff]), Some(4096));
        assert_eq!(flac_samples(&[255, 248, 0x70, 0, 0]), None);
    }
    #[test]
    fn vorbis_short_long_window_durations() {
        let mut id = [0u8; 30];
        id[..7].copy_from_slice(b"\x01vorbis");
        id[28] = 0xb8;
        id[29] = 1;
        let mut setup = b"\x05vorbis".to_vec();
        setup.resize(32, 0);
        let start = 120;
        setup[start / 8] |= 1 << (start % 8); // two modes
        let long_flag = start + 6 + 41;
        setup[long_flag / 8] |= 1 << (long_flag % 8);
        let framing = start + 6 + 82;
        setup[framing / 8] |= 1 << (framing % 8);
        setup.truncate(framing / 8 + 1);
        let mut extra = vec![2, 30, 0];
        extra.extend_from_slice(&id);
        extra.extend_from_slice(&setup);
        let mut v = Vorbis::new(&extra).unwrap();
        assert_eq!(v.samples(&[0]), Some(128));
        assert_eq!(v.samples(&[2]), Some(576));
        assert_eq!(v.samples(&[6]), Some(1024));
        assert_eq!(v.samples(&[0]), Some(576));
    }
    #[test]
    fn vorbis_64_modes_and_seek_reset_keep_the_initial_window() {
        fn short(clock: &mut PacketClock) -> Option<u32> {
            match &mut clock.codec {
                AudioTiming::Vorbis(v) => v.samples(&[2]),
                _ => None,
            }
        }
        let mut id = [0u8; 30];
        id[..7].copy_from_slice(b"\x01vorbis");
        id[28] = 0xb8; // 256 / 2048-sample windows
        id[29] = 1;
        let (start, count) = (120, 64);
        let framing = start + count * 41;
        let mut setup = vec![0u8; framing / 8 + 1];
        setup[..7].copy_from_slice(b"\x05vorbis");
        let mut set = |bit: usize| setup[bit / 8] |= 1 << (bit % 8);
        for bit in start - 6..start {
            set(bit); // mode count - 1 = 63
        }
        set(start); // mode 0 is long; modes 1..64 are short
        set(framing);
        let mut extra = vec![2, 30, 0];
        extra.extend_from_slice(&id);
        extra.extend_from_slice(&setup);
        let codec = AudioTiming::Vorbis(Vorbis::new(&extra).unwrap());
        let mut clock = PacketClock { codec, rate: 48000, next: 0 };
        // A short first packet overlaps mode 0's long window, after open and
        // after a seek alike.
        let fresh = [short(&mut clock), short(&mut clock)];
        assert_eq!(fresh, [Some(576), Some(128)]);
        clock.reset();
        assert_eq!([short(&mut clock), short(&mut clock)], fresh);
    }
}
