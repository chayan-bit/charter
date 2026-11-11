//! Format v3 pack codec (docs/p5/head-model.md section 13): brotli over a pack built
//! without per-object zlib, flagged per pack, and skipped when it would not shrink.

use std::io::{Read, Write};

use crate::{FormatError, field};

/// How the stored bytes of one pack encode the git pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Codec {
    /// The stored bytes are the git pack.
    Raw = 0,
    /// The stored bytes are the brotli stream of the git pack.
    Brotli = 1,
}

impl Codec {
    pub fn from_code(code: u8) -> Result<Self, FormatError> {
        match code {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Brotli),
            v => Err(FormatError::Unsupported {
                what: "pack codec",
                value: u64::from(v),
            }),
        }
    }
}

/// Brotli window: 16 MiB, so a snapshot finds matches across most of its objects.
const LG_WIN: u32 = 24;
/// Packs up to this size use quality 11 (MEASURED: about 0.15 s and 7% fewer bytes than q9).
pub const Q11_MAX: usize = 64 * 1024;
/// Packs up to this size use quality 9; larger ones quality 6.
pub const Q9_MAX: usize = 1 << 20;

/// Brotli quality by pack size (MEASURED on Charter's and a binary-heavy repo, head-model
/// 13.3): above 1 MiB, q9 costs 4x to 25x the CPU of q6 for 1 to 3% fewer bytes.
pub fn quality(len: usize) -> u32 {
    if len <= Q11_MAX {
        11
    } else if len <= Q9_MAX {
        9
    } else {
        6
    }
}

/// The stored form of `pack`: brotli at [`quality`] when that is smaller, else raw.
pub fn encode(pack: &[u8]) -> (Codec, Vec<u8>) {
    let params = brotli::enc::BrotliEncoderParams {
        quality: quality(pack.len()) as i32,
        lgwin: LG_WIN as i32,
        size_hint: pack.len(),
        ..Default::default()
    };
    let mut out = Vec::with_capacity(pack.len() / 2);
    let mut w = brotli::CompressorWriter::with_params(&mut out, 1 << 16, &params);
    // Writing to a Vec cannot fail.
    let ok = w.write_all(pack).and_then(|()| w.flush()).is_ok();
    drop(w);
    match ok && out.len() < pack.len() {
        true => (Codec::Brotli, out),
        false => (Codec::Raw, pack.to_vec()),
    }
}

/// The git pack of `stored`, refusing output past `max` bytes (a signed bomb).
pub fn decode(codec: Codec, stored: &[u8], max: u64) -> Result<Vec<u8>, FormatError> {
    if codec == Codec::Raw {
        return Ok(stored.to_vec());
    }
    let mut out = vec![];
    brotli::Decompressor::new(stored, 1 << 16)
        .take(max.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|_| field("pack", "codec", "invalid brotli stream"))?;
    match out.len() as u64 > max {
        true => Err(field("pack", "codec", "decodes past the pack size cap")),
        false => Ok(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(n: usize) -> Vec<u8> {
        (0..n).map(|i| b"tree blob commit "[i % 17]).collect()
    }

    #[test]
    fn quality_drops_with_pack_size() {
        let q: Vec<u32> = [1, Q11_MAX, Q11_MAX + 1, Q9_MAX, Q9_MAX + 1]
            .map(quality)
            .into();
        assert_eq!(q, [11, 11, 9, 9, 6]);
    }

    #[test]
    fn compressible_packs_round_trip_as_brotli() {
        let pack = text(200_000);
        let (codec, stored) = encode(&pack);
        assert_eq!(codec, Codec::Brotli);
        assert!(stored.len() < pack.len() / 10);
        assert_eq!(decode(codec, &stored, 1 << 20).expect("decode"), pack);
    }

    #[test]
    fn incompressible_packs_are_stored_raw() {
        // xorshift noise: brotli cannot shrink it.
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let noise: Vec<u8> = (0..50_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let (codec, stored) = encode(&noise);
        assert_eq!((codec, stored.as_slice()), (Codec::Raw, noise.as_slice()));
        assert_eq!(
            decode(codec, &stored, 0).expect("raw ignores the cap"),
            noise
        );
    }

    #[test]
    fn a_stream_past_the_cap_or_corrupt_fails() {
        let (codec, stored) = encode(&text(100_000));
        assert!(decode(codec, &stored, 99_999).is_err(), "bomb cap");
        assert!(decode(codec, &stored, 100_000).is_ok(), "exact cap");
        assert!(decode(Codec::Brotli, b"not brotli", 1 << 20).is_err());
        assert!(Codec::from_code(2).is_err());
    }
}
