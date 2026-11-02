//! Claude: zstd, through the C library natively and a pure-Rust one in
//! WebAssembly, where the C library has no compiler. Each reads the other's
//! output.

use crate::DecodeError;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn compress(data: &[u8], level: i32) -> Vec<u8> {
    zstd::bulk::compress(data, level).expect("zstd compression into Vec")
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn decompress(data: &[u8], capacity: usize) -> Result<Vec<u8>, DecodeError> {
    zstd::bulk::decompress(data, capacity).map_err(|_| DecodeError::Corrupt)
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn compress(data: &[u8], _level: i32) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn decompress(data: &[u8], capacity: usize) -> Result<Vec<u8>, DecodeError> {
    if data.is_empty() {
        return Err(DecodeError::Corrupt);
    }
    let mut out = Vec::with_capacity(capacity);
    ruzstd::decoding::FrameDecoder::new()
        .decode_all_to_vec(data, &mut out)
        .map_err(|_| DecodeError::Corrupt)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        (0..20_000u32).map(|i| (i * 7 / 13 % 251) as u8).collect()
    }

    #[test]
    fn pure_rust_reads_native_output() {
        let data = sample();
        let packed = compress(&data, 9);
        let mut out = Vec::with_capacity(data.len());
        ruzstd::decoding::FrameDecoder::new()
            .decode_all_to_vec(&packed, &mut out)
            .unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn native_reads_pure_rust_output() {
        let data = sample();
        let packed = ruzstd::encoding::compress_to_vec(
            &data[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        assert_eq!(decompress(&packed, data.len()).ok(), Some(data));
    }
}
