//! Zstandard compress/decompress over the reference C libzstd (`zstd` crate).
//! Relocated verbatim from sqlink's former self-contained `zstd` extension so
//! there is ONE libzstd in the catalog. Wire format is the canonical zstd frame
//! (magic 28 b5 2f fd) — the same bytes the `zstd` CLI writes.

/// Default compression level for `compress` when level 0 is passed as "use
/// default". zstd's documented default is 3; level 0 in the zstd C API also
/// means "use default", forwarded unchanged.
pub const DEFAULT_LEVEL: i32 = 3;

/// Compress `data` at `level`. Output is a self-framed zstd stream.
pub fn compress(data: &[u8], level: i32) -> Result<Vec<u8>, String> {
    zstd::stream::encode_all(data, level).map_err(|e| format!("compress: {e}"))
}

/// Decompress a self-framed zstd stream produced by `compress` (or any other
/// conforming encoder).
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    zstd::stream::decode_all(data).map_err(|e| format!("decompress: {e}"))
}

/// Compress `data` at `level` using `dictionary` as a raw dictionary. The same
/// bytes must be passed to decompression.
pub fn compress_dict(data: &[u8], dictionary: &[u8], level: i32) -> Result<Vec<u8>, String> {
    use zstd::stream::write::Encoder;
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut enc = Encoder::with_dictionary(&mut buf, level, dictionary)
            .map_err(|e| format!("compress_dict: {e}"))?;
        std::io::Write::write_all(&mut enc, data).map_err(|e| format!("compress_dict: {e}"))?;
        enc.finish().map_err(|e| format!("compress_dict: {e}"))?;
    }
    Ok(buf)
}

/// Decompress with the same raw dictionary that was used to encode.
pub fn decompress_dict(data: &[u8], dictionary: &[u8]) -> Result<Vec<u8>, String> {
    use zstd::stream::read::Decoder;
    let mut dec =
        Decoder::with_dictionary(data, dictionary).map_err(|e| format!("decompress_dict: {e}"))?;
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut dec, &mut out).map_err(|e| format!("decompress_dict: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_default_level() {
        let payload = b"the quick brown fox jumps over the lazy dog".repeat(20);
        let c = compress(&payload, DEFAULT_LEVEL).unwrap();
        assert_eq!(decompress(&c).unwrap(), payload);
    }

    #[test]
    fn round_trip_at_levels() {
        let payload = b"abcabcabc".repeat(100);
        for lvl in [1, 3, 19] {
            let c = compress(&payload, lvl).unwrap();
            assert_eq!(decompress(&c).unwrap(), payload, "level {lvl}");
        }
    }

    #[test]
    fn level_zero_matches_level_three() {
        let payload = b"deterministic output for level 0".repeat(10);
        assert_eq!(compress(&payload, 0).unwrap(), compress(&payload, 3).unwrap());
    }

    #[test]
    fn dict_round_trip() {
        let dict = b"http://example.com/api/v1/users/".repeat(8);
        let payload = b"http://example.com/api/v1/users/alice".to_vec();
        let c = compress_dict(&payload, &dict, 3).unwrap();
        assert_eq!(decompress_dict(&c, &dict).unwrap(), payload);
    }
}
