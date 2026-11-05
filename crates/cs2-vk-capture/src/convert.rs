//! Swapchain pixel formats to packed 8-bit RGB (alpha dropped).
use ash::vk;

/// Formats the layer can read back. Every one is a 32-bit packed pixel.
pub fn supported(format: vk::Format) -> bool {
    matches!(
        format,
        vk::Format::B8G8R8A8_UNORM
            | vk::Format::B8G8R8A8_SRGB
            | vk::Format::R8G8B8A8_UNORM
            | vk::Format::R8G8B8A8_SRGB
            | vk::Format::A2B10G10R10_UNORM_PACK32
            | vk::Format::A2R10G10B10_UNORM_PACK32
    )
}

/// Bytes per pixel of a supported format in the readback buffer.
pub const SRC_BYTES_PER_PIXEL: usize = 4;

fn ten_to_eight(v: u32) -> u8 {
    let x = v & 0x3ff;
    ((x * 255 + 511) / 1023) as u8
}

/// Convert a tightly packed readback buffer (`width * height * 4` bytes) to RGB8.
pub fn to_rgb(format: vk::Format, width: u32, height: u32, src: &[u8]) -> Result<Vec<u8>, String> {
    let n = width as usize * height as usize;
    if src.len() != n * SRC_BYTES_PER_PIXEL {
        return Err(format!(
            "readback is {} bytes, expected {} for {width}x{height}",
            src.len(),
            n * SRC_BYTES_PER_PIXEL
        ));
    }
    let mut out = Vec::with_capacity(n * 3);
    match format {
        vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB => {
            for px in src.as_chunks::<4>().0 {
                out.extend_from_slice(&[px[2], px[1], px[0]]);
            }
        }
        vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB => {
            for px in src.as_chunks::<4>().0 {
                out.extend_from_slice(&[px[0], px[1], px[2]]);
            }
        }
        vk::Format::A2B10G10R10_UNORM_PACK32 => {
            for px in src.as_chunks::<4>().0 {
                let v = u32::from_le_bytes(*px);
                out.extend_from_slice(&[
                    ten_to_eight(v),
                    ten_to_eight(v >> 10),
                    ten_to_eight(v >> 20),
                ]);
            }
        }
        vk::Format::A2R10G10B10_UNORM_PACK32 => {
            for px in src.as_chunks::<4>().0 {
                let v = u32::from_le_bytes(*px);
                out.extend_from_slice(&[
                    ten_to_eight(v >> 20),
                    ten_to_eight(v >> 10),
                    ten_to_eight(v),
                ]);
            }
        }
        other => return Err(format!("unsupported swapchain format {other:?}")),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_swizzles_to_rgb() {
        let rgb = to_rgb(
            vk::Format::B8G8R8A8_UNORM,
            2,
            1,
            &[1, 2, 3, 255, 4, 5, 6, 0],
        )
        .unwrap();
        assert_eq!(rgb, vec![3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn rgba_keeps_order() {
        let rgb = to_rgb(vk::Format::R8G8B8A8_SRGB, 1, 1, &[9, 8, 7, 255]).unwrap();
        assert_eq!(rgb, vec![9, 8, 7]);
    }

    #[test]
    fn a2b10g10r10_extremes() {
        // R = 1023 (bits 0..9), G = 0, B = 1023 (bits 20..29), A = 3
        let v: u32 = 1023 | (1023 << 20) | (3 << 30);
        let rgb = to_rgb(vk::Format::A2B10G10R10_UNORM_PACK32, 1, 1, &v.to_le_bytes()).unwrap();
        assert_eq!(rgb, vec![255, 0, 255]);
    }

    #[test]
    fn a2r10g10b10_order() {
        // R = 1023 in bits 20..29, B = 1023 in bits 0..9
        let v: u32 = (1023 << 20) | 1023;
        let rgb = to_rgb(vk::Format::A2R10G10B10_UNORM_PACK32, 1, 1, &v.to_le_bytes()).unwrap();
        assert_eq!(rgb, vec![255, 0, 255]);
    }

    #[test]
    fn rejects_wrong_size_and_unknown_format() {
        assert!(to_rgb(vk::Format::B8G8R8A8_UNORM, 2, 2, &[0; 4]).is_err());
        assert!(to_rgb(vk::Format::R5G6B5_UNORM_PACK16, 1, 1, &[0; 4]).is_err());
        assert!(!supported(vk::Format::R5G6B5_UNORM_PACK16));
        assert!(supported(vk::Format::B8G8R8A8_SRGB));
    }
}
