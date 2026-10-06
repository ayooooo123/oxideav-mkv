//! WebM's D_WEBVTT carriage: identifier line, settings line, then cue text.

use oxideav_core::{Error, Result};

/// WebVTT side data for the most recently returned packet. The packet itself
/// contains only cue text, as in FFmpeg. Empty fields mean absent side data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebVttMetadata {
    pub identifier: Vec<u8>,
    pub settings: Vec<u8>,
}

pub(super) fn split(data: &mut Vec<u8>) -> Result<WebVttMetadata> {
    fn line(data: &[u8], start: usize) -> Result<(usize, usize)> {
        let end = data[start..].iter().position(|&c| c == b'\r' || c == b'\n')
            .map(|n| start + n)
            .ok_or_else(|| Error::invalid("MKV WebVTT: missing line ending"))?;
        let lf = end + usize::from(data[end] == b'\r');
        if data.get(lf) != Some(&b'\n') {
            return Err(Error::invalid("MKV WebVTT: invalid line ending"));
        }
        Ok((end, lf + 1))
    }
    let (id_end, settings_start) = line(data, 0)?;
    let (settings_end, text_start) = line(data, settings_start)?;
    let metadata = WebVttMetadata {
        identifier: data[..id_end].to_vec(),
        settings: data[settings_start..settings_end].to_vec(),
    };
    let mut text_end = data.len();
    while text_end > text_start && matches!(data[text_end - 1], b'\r' | b'\n') {
        text_end -= 1;
    }
    data.copy_within(text_start..text_end, 0);
    data.truncate(text_end - text_start);
    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_identifier_settings_and_text() {
        let mut data = b"cue-1\r\nalign:start position:10%\nfirst\nsecond\r\n".to_vec();
        let meta = split(&mut data).unwrap();
        assert_eq!(meta.identifier, b"cue-1");
        assert_eq!(meta.settings, b"align:start position:10%");
        assert_eq!(data, b"first\nsecond");
    }

    #[test]
    fn empty_fields_and_text_are_valid() {
        let mut data = b"\n\n".to_vec();
        let meta = split(&mut data).unwrap();
        assert!(meta.identifier.is_empty() && meta.settings.is_empty() && data.is_empty());
    }

    #[test]
    fn incomplete_lines_are_errors() {
        for data in [b"".as_slice(), b"id", b"id\n", b"id\rsettings\ntext", b"\nsettings\r"] {
            assert!(split(&mut data.to_vec()).is_err());
        }
    }
}
