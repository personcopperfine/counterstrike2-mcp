//! Deterministic frame pattern for the live tests. Each frame encodes its own number, so a capture
//! can be matched to the present it came from, pixel for pixel.
//!
//! Layout (RGB, `w >= 256`): a horizontal R gradient, a vertical G gradient, a B value that
//! changes with the frame number, and 16 blocks of 16x16 pixels across the top rows. Block `k` is
//! white when bit `k` of the frame number is set and black otherwise.

/// Frame number carried by the pattern: the low 16 bits.
pub fn pattern_number(present_index: u64) -> u32 {
    (present_index & 0xffff) as u32
}

/// The expected packed RGB image for frame `n` (`n` < 65536).
pub fn expected_rgb(w: u32, h: u32, n: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(w as usize * h as usize * 3);
    let blue = ((n.wrapping_mul(37)) & 0xff) as u8;
    for y in 0..h {
        for x in 0..w {
            if y < 16 && x < 256 {
                let k = x / 16;
                let v = if (n >> k) & 1 == 1 { 255 } else { 0 };
                out.extend_from_slice(&[v, v, v]);
                continue;
            }
            let r = (x as u64 * 255 / (w as u64 - 1)) as u8;
            let g = (y as u64 * 255 / (h as u64 - 1)) as u8;
            out.extend_from_slice(&[r, g, blue]);
        }
    }
    out
}

/// BGRA bytes of frame `n`, as written into a `B8G8R8A8` staging buffer.
pub fn expected_bgra(w: u32, h: u32, n: u32) -> Vec<u8> {
    let rgb = expected_rgb(w, h, n);
    let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
    for px in rgb.as_chunks::<3>().0 {
        out.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    out
}

/// Read the frame number back from an RGB image (the centre of each block).
pub fn decode_number(w: u32, rgb: &[u8]) -> u32 {
    let mut n = 0u32;
    for k in 0..16u32 {
        let x = (16 * k + 8) as usize;
        let i = (8 * w as usize + x) * 3;
        if rgb[i] > 127 {
            n |= 1 << k;
        }
    }
    n
}

/// Check an RGB image against the pattern for the frame it claims to be: the frame number encoded
/// in the image must equal `present_index`, and every pixel must match exactly.
pub fn verify_rgb(w: u32, h: u32, rgb: &[u8], present_index: u64) -> Result<(), String> {
    if rgb.len() != w as usize * h as usize * 3 {
        return Err(format!("image is {} bytes, expected {w}x{h}x3", rgb.len()));
    }
    let want = pattern_number(present_index);
    let got = decode_number(w, rgb);
    if got != want {
        return Err(format!(
            "image encodes frame {got}, presentIndex {present_index} (mod 65536 = {want})"
        ));
    }
    let expected = expected_rgb(w, h, want);
    if let Some(i) = rgb.iter().zip(&expected).position(|(a, b)| a != b) {
        let px = i / 3;
        return Err(format!(
            "pixel ({}, {}) differs from frame {want}: got {:?}, want {:?}",
            px % w as usize,
            px / w as usize,
            &rgb[i - i % 3..i - i % 3 + 3],
            &expected[i - i % 3..i - i % 3 + 3]
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_round_trips_through_the_image() {
        for n in [0u32, 1, 2, 0x8001, 0xffff, 1234] {
            let img = expected_rgb(640, 480, n);
            assert_eq!(decode_number(640, &img), n);
            assert!(verify_rgb(640, 480, &img, n as u64).is_ok());
        }
    }

    #[test]
    fn a_wrong_frame_or_a_flipped_pixel_is_caught() {
        let mut img = expected_rgb(640, 480, 7);
        assert!(verify_rgb(640, 480, &img, 8).is_err());
        img[1000] ^= 1;
        assert!(verify_rgb(640, 480, &img, 7).is_err());
    }

    #[test]
    fn bgra_is_the_rgb_pattern_swizzled() {
        let rgb = expected_rgb(640, 480, 5);
        let bgra = expected_bgra(640, 480, 5);
        assert_eq!(bgra[0], rgb[2]);
        assert_eq!(bgra[2], rgb[0]);
        assert_eq!(bgra.len(), 640 * 480 * 4);
    }
}
