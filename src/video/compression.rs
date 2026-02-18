use crate::settings::CompressionAlgorithm;

/// Return the file extension (without dot) for compressed frame files.
pub fn file_extension(algo: CompressionAlgorithm) -> &'static str {
    match algo {
        CompressionAlgorithm::Lz4 => "lz4",
        CompressionAlgorithm::Zstd => "zst",
        CompressionAlgorithm::Png => "png",
        CompressionAlgorithm::Bc1 => "bc1",
        CompressionAlgorithm::Bc7 => "bc7",
    }
}

/// Compress raw RGBA frame data using the specified algorithm.
/// `width` and `height` are needed for PNG and BC encoding.
pub fn compress(
    data: &[u8],
    algo: CompressionAlgorithm,
    level: u32,
    width: u32,
    height: u32,
) -> Vec<u8> {
    match algo {
        CompressionAlgorithm::Lz4 => lz4_flex::compress_prepend_size(data),
        CompressionAlgorithm::Zstd => {
            zstd::bulk::compress(data, level as i32).unwrap_or_default()
        }
        CompressionAlgorithm::Png => {
            use image::codecs::png::{CompressionType, FilterType, PngEncoder};
            use image::ImageEncoder;

            // FilterType::Adaptive is extremely slow (tries all 5 filters per row).
            // Only use it at the highest levels where max compression is desired.
            let (compression, filter) = match level {
                0 => (CompressionType::Fast, FilterType::NoFilter),
                1..=3 => (CompressionType::Fast, FilterType::Sub),
                4..=6 => (CompressionType::Default, FilterType::Sub),
                7..=8 => (CompressionType::Best, FilterType::Sub),
                _ => (CompressionType::Best, FilterType::Adaptive),
            };

            let mut buf = Vec::new();
            let encoder = PngEncoder::new_with_quality(&mut buf, compression, filter);
            if let Err(e) = encoder.write_image(
                data,
                width,
                height,
                image::ExtendedColorType::Rgba8,
            ) {
                log::error!("PNG encode failed: {e}");
                return Vec::new();
            }
            buf
        }
        CompressionAlgorithm::Bc1 => {
            let padded_w = (width + 3) & !3;
            let padded_h = (height + 3) & !3;
            let padded = pad_to_block_alignment(data, width, height, padded_w, padded_h);
            let surface = intel_tex_2::RgbaSurface {
                data: &padded,
                width: padded_w,
                height: padded_h,
                stride: padded_w * 4,
            };
            let compressed = intel_tex_2::bc1::compress_blocks(&surface);
            let mut out = Vec::with_capacity(8 + compressed.len());
            out.extend_from_slice(&width.to_le_bytes());
            out.extend_from_slice(&height.to_le_bytes());
            out.extend_from_slice(&compressed);
            out
        }
        CompressionAlgorithm::Bc7 => {
            let padded_w = (width + 3) & !3;
            let padded_h = (height + 3) & !3;
            let padded = pad_to_block_alignment(data, width, height, padded_w, padded_h);
            let surface = intel_tex_2::RgbaSurface {
                data: &padded,
                width: padded_w,
                height: padded_h,
                stride: padded_w * 4,
            };
            let settings = bc7_settings(level);
            let compressed = intel_tex_2::bc7::compress_blocks(&settings, &surface);
            let mut out = Vec::with_capacity(8 + compressed.len());
            out.extend_from_slice(&width.to_le_bytes());
            out.extend_from_slice(&height.to_le_bytes());
            out.extend_from_slice(&compressed);
            out
        }
    }
}

/// Decompress compressed frame data back to raw RGBA.
/// `expected_len` is `width * height * 4`, needed as a size hint for Zstd.
pub fn decompress(
    data: &[u8],
    algo: CompressionAlgorithm,
    expected_len: usize,
) -> Result<Vec<u8>, String> {
    match algo {
        CompressionAlgorithm::Lz4 => {
            lz4_flex::decompress_size_prepended(data)
                .map_err(|e| format!("LZ4 decompress failed: {e}"))
        }
        CompressionAlgorithm::Zstd => {
            zstd::bulk::decompress(data, expected_len)
                .map_err(|e| format!("Zstd decompress failed: {e}"))
        }
        CompressionAlgorithm::Png => {
            let img = image::load_from_memory_with_format(data, image::ImageFormat::Png)
                .map_err(|e| format!("PNG decode failed: {e}"))?;
            Ok(img.into_rgba8().into_raw())
        }
        CompressionAlgorithm::Bc1 | CompressionAlgorithm::Bc7 => {
            Err("BC formats are GPU-native and do not need CPU decompression".into())
        }
    }
}

/// Parse the 8-byte header from a BC1/BC7 file.
/// Returns (original_width, original_height, compressed_data_slice).
pub fn parse_bc_header(data: &[u8]) -> Option<(u32, u32, &[u8])> {
    if data.len() < 8 {
        return None;
    }
    let w = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let h = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    Some((w, h, &data[8..]))
}

/// Pad RGBA data to block-aligned dimensions (multiples of 4).
pub(crate) fn pad_to_block_alignment(data: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h {
        return data.to_vec();
    }
    let mut padded = vec![0u8; (dst_w * dst_h * 4) as usize];
    let src_stride = src_w as usize * 4;
    let dst_stride = dst_w as usize * 4;
    for row in 0..src_h as usize {
        let src_start = row * src_stride;
        let dst_start = row * dst_stride;
        padded[dst_start..dst_start + src_stride]
            .copy_from_slice(&data[src_start..src_start + src_stride]);
    }
    padded
}

/// Map BC7 quality level (0-4) to intel_tex_2 EncodeSettings.
fn bc7_settings(level: u32) -> intel_tex_2::bc7::EncodeSettings {
    match level {
        0 => intel_tex_2::bc7::opaque_ultra_fast_settings(),
        1 => intel_tex_2::bc7::opaque_very_fast_settings(),
        2 => intel_tex_2::bc7::opaque_fast_settings(),
        3 => intel_tex_2::bc7::opaque_basic_settings(),
        4 => intel_tex_2::bc7::opaque_slow_settings(),
        _ => intel_tex_2::bc7::opaque_very_fast_settings(),
    }
}
