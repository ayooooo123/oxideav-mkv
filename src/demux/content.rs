//! Undoing a track's `ContentEncodings` compression (RFC 9559
//! §5.1.4.1.31.5 – §5.1.4.1.31.7) on Block frames and `CodecPrivate`.
//!
//! Every compression algorithm of the §27.2 registry is reversible by the
//! container: zlib (RFC 1950), bzip2, LZO1X and Header Stripping. A chain
//! is undone highest `ContentEncodingOrder` first (§5.1.4.1.31.2); a chain
//! holding a step the container can't undo — encryption, an unregistered
//! algorithm — leaves the data as stored, never partially decoded.
//!
//! Each step's output is bounded the way FFmpeg's `matroskadec` bounds
//! it: an input of 10 MB or more is refused, and a decompressed frame may
//! not outgrow the first `input × 3^k` buffer that reaches 10 MB.

use oxideav_core::{Error, Result};

use super::{ContentCompAlgo, ContentEncodingTransform, ContentEncodings};

/// Inputs this large are refused (FFmpeg's `matroska_decode_buffer`).
const MAX_INPUT: usize = 10_000_000;

/// One reversible step of a compression chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Decompression {
    /// Header Stripping: these octets were removed from the front.
    HeaderStripping(Vec<u8>),
    Zlib,
    Bzlib,
    Lzo1x,
}

/// The steps undoing the encodings of `enc` that apply to Block frames
/// (`private == false`, scope bit `0x1`) or to `CodecPrivate`
/// (`private == true`, scope bit `0x2`), in decode order. `None` when
/// one of them can't be undone, or when nothing needs undoing.
pub(super) fn decompression_chain(
    enc: &ContentEncodings,
    private: bool,
) -> Option<Vec<Decompression>> {
    let mut chain = Vec::new();
    for e in &enc.encodings {
        if !(if private { e.scope.private() } else { e.scope.block() }) {
            continue;
        }
        let ContentEncodingTransform::Compression { algo, settings } = &e.transform else {
            return None;
        };
        chain.push(match algo {
            // Stripping nothing is a no-op.
            ContentCompAlgo::HeaderStripping if settings.is_empty() => continue,
            ContentCompAlgo::HeaderStripping => Decompression::HeaderStripping(settings.clone()),
            ContentCompAlgo::Zlib => Decompression::Zlib,
            ContentCompAlgo::Bzlib => Decompression::Bzlib,
            ContentCompAlgo::Lzo1x => Decompression::Lzo1x,
            ContentCompAlgo::Other(_) => return None,
        });
    }
    if chain.is_empty() {
        None
    } else {
        Some(chain)
    }
}

/// `data` with `chain` undone.
pub(super) fn decompress(chain: &[Decompression], mut data: Vec<u8>) -> Result<Vec<u8>> {
    for step in chain {
        if data.len() >= MAX_INPUT {
            return Err(Error::invalid(format!(
                "MKV: encoded frame of {} bytes",
                data.len()
            )));
        }
        data = match step {
            Decompression::HeaderStripping(prefix) => {
                let mut out = Vec::with_capacity(prefix.len() + data.len());
                out.extend_from_slice(prefix);
                out.extend_from_slice(&data);
                out
            }
            Decompression::Zlib => inflate::<compcol::zlib::Zlib>(&data)?,
            Decompression::Bzlib => inflate::<compcol::bzip2::Bzip2>(&data)?,
            Decompression::Lzo1x => {
                let mut out = Vec::new();
                compcol::lzo::block::decode_block(&data, &mut out, output_cap(data.len()))
                    .map_err(|e| Error::invalid(format!("MKV: lzo frame: {e:?}")))?;
                out
            }
        };
    }
    Ok(data)
}

fn inflate<A: compcol::Algorithm>(data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Err(Error::invalid(format!("MKV: empty {} frame", A::NAME)));
    }
    compcol::vec::decompress_to_vec_capped::<A>(data, output_cap(data.len()) as u64)
        .map_err(|e| Error::invalid(format!("MKV: {} frame: {e:?}", A::NAME)))
}

/// The largest output an encoded input of `len` (< 10 MB) bytes may
/// produce.
fn output_cap(len: usize) -> usize {
    let mut cap = len.max(1) * 3;
    while cap < MAX_INPUT {
        cap *= 3;
    }
    cap
}
