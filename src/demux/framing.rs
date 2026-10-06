//! Restore codec headers omitted by Matroska's ProRes and WavPack mappings.
use oxideav_core::{Error, Result};

pub(super) fn prores(data: &mut Vec<u8>) -> Result<()> {
    if data.get(4..8) == Some(b"icpf") { return Ok(()); }
    let len = data.len().checked_add(8).filter(|&n| n <= u32::MAX as usize)
        .ok_or_else(|| Error::invalid("MKV ProRes: frame too large"))?;
    data.resize(len, 0);
    data.copy_within(..len - 8, 8);
    data[..4].copy_from_slice(&(len as u32).to_be_bytes());
    data[4..8].copy_from_slice(b"icpf");
    Ok(())
}

fn block(data: &[u8]) -> Result<(u32, u32, &[u8], &[u8])> {
    if data.len() < 8 { return Err(Error::invalid("MKV WavPack: short block header")); }
    let flags = u32::from_le_bytes(data[..4].try_into().unwrap());
    let crc = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let (start, size): (usize, usize) = if flags & 0x1800 == 0x1800 {
        (8, data.len() - 8)
    } else {
        let size = data.get(8..12).ok_or_else(|| Error::invalid("MKV WavPack: missing block size"))?;
        (12, u32::from_le_bytes(size.try_into().unwrap()) as usize)
    };
    let end = start.checked_add(size).filter(|&n| n <= data.len())
        .ok_or_else(|| Error::invalid("MKV WavPack: block exceeds packet"))?;
    Ok((flags, crc, &data[start..end], &data[end..]))
}

pub(super) fn wavpack(data: Vec<u8>, version: u16) -> Result<Vec<u8>> {
    if data.len() < 12 { return Err(Error::invalid("MKV WavPack: short frame")); }
    let samples = &data[..4];
    // Validate block sizes before allocating, and allocate the exact output
    // once. No size from the input can reserve memory beyond bytes present.
    let mut rest = &data[4..];
    let mut size = 0usize;
    while rest.len() >= 8 {
        let (_, _, payload, next) = block(rest)?;
        size = size.checked_add(32 + payload.len())
            .ok_or_else(|| Error::invalid("MKV WavPack: frame too large"))?;
        rest = next;
    }
    let mut output = Vec::with_capacity(size);
    rest = &data[4..];
    while rest.len() >= 8 {
        let (flags, crc, payload, next) = block(rest)?;
        let size = u32::try_from(payload.len() + 24)
            .map_err(|_| Error::invalid("MKV WavPack: block too large"))?;
        output.extend_from_slice(b"wvpk");
        output.extend_from_slice(&size.to_le_bytes());
        output.extend_from_slice(&version.to_le_bytes());
        output.extend_from_slice(&[0; 10]); // track/index, total samples, block index
        output.extend_from_slice(samples);
        output.extend_from_slice(&flags.to_le_bytes());
        output.extend_from_slice(&crc.to_le_bytes());
        output.extend_from_slice(payload);
        rest = next;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prores_prepends_size_and_magic_only_once() {
        let mut data = vec![1, 2, 3];
        prores(&mut data).unwrap();
        assert_eq!(data, b"\0\0\0\x0bicpf\x01\x02\x03");
        prores(&mut data).unwrap();
        assert_eq!(data.len(), 11);
    }
    #[test]
    fn wavpack_single_and_multiblock_headers() {
        let compact = [4u32.to_le_bytes().to_vec(), 0x1800u32.to_le_bytes().to_vec(),
            0x1234u32.to_le_bytes().to_vec(), vec![7, 8]].concat();
        let out = wavpack(compact, 0x410).unwrap();
        assert_eq!(&out[..10], b"wvpk\x1a\0\0\0\x10\x04");
        assert_eq!(&out[20..], b"\x04\0\0\0\0\x18\0\0\x34\x12\0\0\x07\x08");
        let mut compact = 4u32.to_le_bytes().to_vec();
        for flags in [0x800u32, 0x1000] {
            compact.extend_from_slice(&flags.to_le_bytes());
            compact.extend_from_slice(&0u32.to_le_bytes());
            compact.extend_from_slice(&1u32.to_le_bytes());
            compact.push(9);
        }
        let out = wavpack(compact, 0x403).unwrap();
        assert_eq!(out.len(), 66);
        assert_eq!(&out[33..37], b"wvpk");
    }
    #[test]
    fn forged_wavpack_sizes_are_bounded() {
        let data = [vec![0; 12], u32::MAX.to_le_bytes().to_vec()].concat();
        assert!(wavpack(data, 0x410).is_err());
    }
}
