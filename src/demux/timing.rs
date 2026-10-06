//! Codec frame durations needed when a lace has no container timestamp.
//! Matroska specifies only the first timestamp in a duration-less Block;
//! subsequent audio packets advance by their codec's decoded sample count.
use oxideav_core::{CodecParameters, TimeBase};

/// The bounded PTS window used to infer DTS for reordered video. Missing
/// slots sort before real timestamps, retaining N/A during initial delay.
pub(super) struct DecodeOrder {
    delay: usize,
    pts: [Option<i64>; 17],
}

impl DecodeOrder {
    pub(super) fn new(delay: usize) -> Self {
        Self { delay, pts: [None; 17] }
    }
    pub(super) fn reset(&mut self) { self.pts.fill(None); }
    pub(super) fn dts(&mut self, pts: Option<i64>, delay: Option<usize>) -> Option<i64> {
        if let Some(delay) = delay { self.delay = delay; }
        if self.delay >= self.pts.len() { return None; }
        if let Some(pts) = pts {
            self.pts[0] = Some(pts);
            for i in 0..self.delay {
                if self.pts[i] <= self.pts[i + 1] { break; }
                self.pts.swap(i, i + 1);
            }
            self.pts[0]
        } else {
            None
        }
    }
}

pub(super) struct PacketClock {
    codec: AudioTiming,
    rate: u32,
    next: i64,
}
enum AudioTiming {
    None,
    Opus,
    Flac,
    WavPack,
    Vorbis(Vorbis),
}

impl PacketClock {
    pub(super) fn new(params: &CodecParameters) -> Self {
        let codec = match params.codec_id.as_str() {
            "opus" => AudioTiming::Opus,
            "flac" => AudioTiming::Flac,
            "wavpack" => AudioTiming::WavPack,
            "vorbis" => Vorbis::new(&params.extradata).map(AudioTiming::Vorbis).unwrap_or(AudioTiming::None),
            _ => AudioTiming::None,
        };
        Self { codec, rate: if params.codec_id.as_str() == "opus" { 48000 } else { params.sample_rate.unwrap_or(0) }, next: 0 }
    }

    pub(super) fn reset(&mut self) {
        self.next = 0;
        if let AudioTiming::Vorbis(v) = &mut self.codec { v.previous = v.sizes[0]; }
    }

    pub(super) fn frame_duration(&mut self, data: &[u8], tb: TimeBase) -> Option<i64> {
        let samples = match &mut self.codec {
            AudioTiming::None => return None,
            AudioTiming::Opus => opus_samples(data)?,
            AudioTiming::Flac => flac_samples(data)?,
            // This runs on Matroska's compact WavPack frame before rebuilding
            // its standard block headers. Its first word is block_samples.
            AudioTiming::WavPack => u32::from_le_bytes(data.get(..4)?.try_into().ok()?),
            AudioTiming::Vorbis(v) => v.samples(data)?,
        };
        let tb = tb.as_rational();
        let den = self.rate as i128 * tb.num as i128;
        if den <= 0 { return None; }
        i64::try_from(samples as i128 * tb.den as i128 / den).ok()
    }

    pub(super) fn timestamp(&mut self, pts: Option<i64>, duration: Option<i64>) -> Option<i64> {
        let pts = pts.or_else(|| duration.filter(|&d| d > 0).map(|_| self.next));
        if let Some(pts) = pts {
            self.next = pts.saturating_add(duration.unwrap_or(0));
        }
        pts
    }
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
        for n in 1..=63 {
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
        Some(Self { sizes, modes, count, mode_bits, previous: sizes[usize::from(modes[0])] })
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
}
