//! Decoded frames: an 8-bit RGB buffer shared by every capture source (game PNG, bridge render,
//! Vulkan layer frames), with the pixel statistics the tools report, the game's R/B swap,
//! nearest-neighbour downscaling, PNG encode/decode (crate `png`) and the inline-size policy.
use super::INLINE_MAX_PIXELS;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;

/// A pixel counts as non-black when any colour channel is above this (same threshold as the
/// POC `tests/lib/xwd.py` `stats`, and the bridge's `world.render` `nonBlackPixels`).
pub const NONBLACK_THRESHOLD: u8 = 8;

/// Largest frame accepted from any PNG (the bridge's `WR_MAX_PIXELS`, 8192x4096). Checked from the
/// header before any pixel buffer is allocated.
pub const MAX_PIXELS: u64 = 33_554_432;

/// Packed 8-bit RGB, row-major, `width * height * 3` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageStats {
    pub width: u32,
    pub height: u32,
    /// fraction of pixels with any channel > [`NONBLACK_THRESHOLD`]
    pub nonblack_frac: f64,
    pub unique_colors: usize,
}

impl ImageStats {
    pub fn to_json(self) -> Value {
        json!({
            "width": self.width,
            "height": self.height,
            "nonblackFrac": (self.nonblack_frac * 1e6).round() / 1e6,
            "uniqueColors": self.unique_colors,
        })
    }
}

impl RgbImage {
    /// Validates the buffer length against the dimensions.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self> {
        let want = pixel_bytes(width, height, 3)?;
        if pixels.len() != want {
            bail!(
                "RGB buffer is {} bytes, expected {want} for {width}x{height}",
                pixels.len()
            );
        }
        Ok(RgbImage {
            width,
            height,
            pixels,
        })
    }

    /// Build from packed RGB or RGBA (alpha dropped). `channels` is 3 or 4.
    pub fn from_channels(width: u32, height: u32, pixels: &[u8], channels: usize) -> Result<Self> {
        if channels != 3 && channels != 4 {
            bail!("channels must be 3 (RGB) or 4 (RGBA), got {channels}");
        }
        let want = pixel_bytes(width, height, channels)?;
        if pixels.len() != want {
            bail!(
                "buffer is {} bytes, expected {want} for {width}x{height}x{channels}",
                pixels.len()
            );
        }
        let out = if channels == 3 {
            pixels.to_vec()
        } else {
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect()
        };
        Ok(RgbImage {
            width,
            height,
            pixels: out,
        })
    }

    pub fn pixel_count(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Swap R and B in place (the game's `png_screenshot` writes BGR order).
    pub fn swap_rb(&mut self) {
        for p in self.pixels.as_chunks_mut::<3>().0 {
            p.swap(0, 2);
        }
    }

    pub fn stats(&self) -> ImageStats {
        let mut nonblack = 0u64;
        let mut colors = HashSet::new();
        for p in self.pixels.as_chunks::<3>().0 {
            if p.iter().any(|&c| c > NONBLACK_THRESHOLD) {
                nonblack += 1;
            }
            colors.insert((p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32);
        }
        let n = self.pixel_count();
        ImageStats {
            width: self.width,
            height: self.height,
            nonblack_frac: if n == 0 {
                0.0
            } else {
                nonblack as f64 / n as f64
            },
            unique_colors: colors.len(),
        }
    }

    /// Nearest-neighbour resample to exactly `new_w` x `new_h`.
    pub fn resize_nearest(&self, new_w: u32, new_h: u32) -> Result<RgbImage> {
        if new_w == 0 || new_h == 0 {
            bail!("resize target must be non-zero");
        }
        let mut out = Vec::with_capacity(pixel_bytes(new_w, new_h, 3)?);
        for y in 0..new_h as u64 {
            let sy = (y * self.height as u64 / new_h as u64) as usize;
            for x in 0..new_w as u64 {
                let sx = (x * self.width as u64 / new_w as u64) as usize;
                let i = (sy * self.width as usize + sx) * 3;
                out.extend_from_slice(&self.pixels[i..i + 3]);
            }
        }
        RgbImage::new(new_w, new_h, out)
    }

    /// Shrink (keeping the aspect ratio) until `width * height <= max_pixels`; a no-op copy when
    /// already small enough.
    pub fn downscale_to_fit(&self, max_pixels: u64) -> Result<RgbImage> {
        let n = self.pixel_count();
        if n <= max_pixels || n == 0 {
            return Ok(self.clone());
        }
        let scale = (max_pixels as f64 / n as f64).sqrt();
        let mut w = ((self.width as f64 * scale).floor() as u32).max(1);
        let mut h = ((self.height as f64 * scale).floor() as u32).max(1);
        // rounding can leave the product slightly over the cap; trim the longer side
        while w as u64 * h as u64 > max_pixels {
            if w >= h {
                w -= 1;
            } else {
                h -= 1;
            }
        }
        self.resize_nearest(w.max(1), h.max(1))
    }

    /// Crop `[x, y, w, h]` (bounds-checked) into a new image.
    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Result<RgbImage> {
        let x_end = x.checked_add(w).context("crop overflows")?;
        let y_end = y.checked_add(h).context("crop overflows")?;
        if w == 0 || h == 0 || x_end > self.width || y_end > self.height {
            bail!(
                "crop [{x},{y},{w},{h}] is outside the {}x{} image or empty",
                self.width,
                self.height
            );
        }
        let mut out = Vec::with_capacity(pixel_bytes(w, h, 3)?);
        for row in y..y_end {
            let start = (row as usize * self.width as usize + x as usize) * 3;
            out.extend_from_slice(&self.pixels[start..start + w as usize * 3]);
        }
        RgbImage::new(w, h, out)
    }

    /// Integer nearest-neighbour upscale (`factor` >= 1).
    pub fn upscale(&self, factor: u32) -> Result<RgbImage> {
        if factor == 0 {
            bail!("upscale factor must be >= 1");
        }
        let w = self
            .width
            .checked_mul(factor)
            .context("upscale width overflows")?;
        let h = self
            .height
            .checked_mul(factor)
            .context("upscale height overflows")?;
        self.resize_nearest(w, h)
    }

    pub fn to_png(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut buf, self.width, self.height);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().context("png header")?;
            writer.write_image_data(&self.pixels).context("png data")?;
            writer.finish().context("png finish")?;
        }
        Ok(buf)
    }
}

/// Reject empty or oversized frames. Used on PNG headers before any pixel buffer exists.
pub fn check_dims(width: u32, height: u32) -> Result<()> {
    let n = (width as u64)
        .checked_mul(height as u64)
        .context("image dimensions overflow")?;
    if width == 0 || height == 0 || n > MAX_PIXELS {
        bail!("image {width}x{height} is empty or larger than {MAX_PIXELS} pixels");
    }
    Ok(())
}

/// Decode PNG bytes to RGB (alpha dropped, palette/gray/16-bit expanded). The header size is
/// checked before the output buffer is allocated.
pub fn decode_png(bytes: &[u8]) -> Result<RgbImage> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().context("not a PNG")?;
    let (w, h) = (reader.info().width, reader.info().height);
    check_dims(w, h)?;
    let size = reader
        .output_buffer_size()
        .context("png output size overflows")?;
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).context("png decode")?;
    let data = &buf[..info.buffer_size()];
    match info.color_type {
        png::ColorType::Rgb => RgbImage::new(w, h, data.to_vec()),
        png::ColorType::Rgba => RgbImage::from_channels(w, h, data, 4),
        png::ColorType::Grayscale => {
            RgbImage::new(w, h, data.iter().flat_map(|&g| [g, g, g]).collect())
        }
        png::ColorType::GrayscaleAlpha => RgbImage::new(
            w,
            h,
            data.as_chunks::<2>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[0], p[0]])
                .collect(),
        ),
        png::ColorType::Indexed => bail!("indexed PNG was not expanded"),
    }
}

/// Decode a PNG file to RGB.
pub fn load_png_rgb(path: &Path) -> Result<RgbImage> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    decode_png(&bytes).with_context(|| format!("PNG {}", path.display()))
}

/// Stats of a PNG file on disk.
pub fn load_png_stats(path: &Path) -> Result<ImageStats> {
    Ok(load_png_rgb(path)?.stats())
}

/// Pixel statistics of a raw buffer (`channels` 3 = RGB, 4 = RGBA; alpha ignored).
pub fn image_stats(width: u32, height: u32, pixels: &[u8], channels: usize) -> Result<ImageStats> {
    Ok(RgbImage::from_channels(width, height, pixels, channels)?.stats())
}

/// What a tool result carries for one capture: the PNG bytes to inline (full size, or a
/// downscaled copy when the frame is larger than [`INLINE_MAX_PIXELS`]).
#[derive(Debug, Clone)]
pub struct InlinePolicy {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub downscaled: bool,
}

/// `original` is the source file's bytes when they still match `img` exactly (no R/B swap); a frame
/// that needs no downscale then inlines them as-is instead of re-encoding.
pub fn inline_policy(img: &RgbImage, original: Option<Vec<u8>>) -> Result<InlinePolicy> {
    if img.pixel_count() <= INLINE_MAX_PIXELS {
        let png = match original {
            Some(bytes) => bytes,
            None => img.to_png()?,
        };
        return Ok(InlinePolicy {
            png,
            width: img.width,
            height: img.height,
            downscaled: false,
        });
    }
    let small = img.downscale_to_fit(INLINE_MAX_PIXELS)?;
    Ok(InlinePolicy {
        png: small.to_png()?,
        width: small.width,
        height: small.height,
        downscaled: true,
    })
}

fn pixel_bytes(width: u32, height: u32, channels: usize) -> Result<usize> {
    (width as u64)
        .checked_mul(height as u64)
        .and_then(|n| n.checked_mul(channels as u64))
        .and_then(|n| usize::try_from(n).ok())
        .context("image dimensions overflow")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> RgbImage {
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[(x * 37) as u8, (y * 91) as u8, ((x + y) * 13) as u8]);
            }
        }
        RgbImage::new(w, h, px).unwrap()
    }

    #[test]
    fn stats_nonblack_and_unique() {
        // 4 pixels: black, dim (channel 8 is NOT non-black), bright, bright-duplicate
        let px = [0, 0, 0, 8, 8, 8, 0, 0, 200, 0, 0, 200];
        let s = image_stats(2, 2, &px, 3).unwrap();
        assert_eq!(s.unique_colors, 3);
        assert!((s.nonblack_frac - 0.5).abs() < 1e-9);
        assert_eq!(s.to_json()["uniqueColors"], json!(3));
    }

    #[test]
    fn stats_rgba_ignores_alpha() {
        let px = [0, 0, 0, 255, 0, 0, 0, 0];
        let s = image_stats(2, 1, &px, 4).unwrap();
        assert_eq!(s.nonblack_frac, 0.0);
        assert_eq!(s.unique_colors, 1);
    }

    #[test]
    fn stats_rejects_bad_buffers() {
        assert!(image_stats(2, 2, &[0; 11], 3).is_err());
        assert!(image_stats(2, 2, &[0; 16], 2).is_err());
    }

    #[test]
    fn swap_rb_round_trips() {
        let mut img = RgbImage::new(1, 2, vec![1, 2, 3, 4, 5, 6]).unwrap();
        img.swap_rb();
        assert_eq!(img.pixels, vec![3, 2, 1, 6, 5, 4]);
        img.swap_rb();
        assert_eq!(img.pixels, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn downscale_fits_cap_and_keeps_aspect() {
        let img = gradient(400, 100);
        let small = img.downscale_to_fit(1000).unwrap();
        assert!(small.pixel_count() <= 1000);
        assert_eq!(small.width / small.height, 4);
        // nearest neighbour: every output pixel is an exact source pixel
        let src: HashSet<&[u8]> = img
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| p.as_slice())
            .collect();
        assert!(small
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .all(|p| src.contains(p.as_slice())));
        // already small: unchanged
        assert_eq!(
            gradient(10, 10).downscale_to_fit(1000).unwrap(),
            gradient(10, 10)
        );
    }

    #[test]
    fn crop_and_upscale() {
        let img = gradient(4, 4);
        let c = img.crop(1, 2, 2, 1).unwrap();
        assert_eq!((c.width, c.height), (2, 1));
        assert_eq!(
            &c.pixels[0..3],
            &img.pixels[(2 * 4 + 1) * 3..(2 * 4 + 1) * 3 + 3]
        );
        assert!(img.crop(3, 3, 2, 2).is_err());
        let up = c.upscale(2).unwrap();
        assert_eq!((up.width, up.height), (4, 2));
        assert_eq!(&up.pixels[0..3], &c.pixels[0..3]);
    }

    #[test]
    fn inline_policy_small_is_full_size() {
        let img = gradient(64, 32);
        let p = inline_policy(&img, None).unwrap();
        assert!(!p.downscaled);
        assert_eq!((p.width, p.height), (64, 32));
        let back = decode_bytes(&p.png);
        assert_eq!(back, img);
    }

    #[test]
    fn inline_policy_large_downscales() {
        // 2400x1000 = 2.4 Mpx > 1920*1080 (2.07 Mpx)
        let img = RgbImage::new(2400, 1000, vec![7; 2400 * 1000 * 3]).unwrap();
        let p = inline_policy(&img, None).unwrap();
        assert!(p.downscaled);
        assert!(p.width as u64 * p.height as u64 <= INLINE_MAX_PIXELS);
        assert_eq!(decode_bytes(&p.png).width, p.width);
    }

    #[test]
    fn png_file_round_trip_and_stats() {
        let dir = std::env::temp_dir().join(format!("cs2mcp-frame-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("g.png");
        let img = gradient(16, 8);
        std::fs::write(&path, img.to_png().unwrap()).unwrap();
        assert_eq!(load_png_rgb(&path).unwrap(), img);
        assert_eq!(load_png_stats(&path).unwrap(), img.stats());
        assert!(load_png_rgb(&dir.join("missing.png")).is_err());
        std::fs::write(dir.join("junk.png"), b"not a png").unwrap();
        assert!(load_png_rgb(&dir.join("junk.png")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// A valid 2x2 PNG whose IHDR claims `w x h` (CRC fixed up, so the header parses).
    fn png_claiming_size(w: u32, h: u32) -> Vec<u8> {
        let mut png = RgbImage::new(2, 2, vec![0; 12]).unwrap().to_png().unwrap();
        // signature (8) + IHDR length (4) + "IHDR" (4): IHDR data is bytes 16..29, CRC 29..33
        png[16..20].copy_from_slice(&w.to_be_bytes());
        png[20..24].copy_from_slice(&h.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        png
    }

    #[test]
    fn oversized_png_header_is_rejected_before_allocation() {
        for (w, h) in [
            (8193u32, 4096u32),
            (16384, 16384),
            (u32::MAX, u32::MAX),
            (0, 10),
        ] {
            let png = png_claiming_size(w, h);
            assert!(decode_png(&png).is_err(), "{w}x{h} must be refused");
        }
        assert!(check_dims(8192, 4096).is_ok());
        assert!(check_dims(8193, 4096).is_err());
    }

    #[test]
    fn upscale_refuses_overflow() {
        let img = RgbImage::new(1, 1, vec![0; 3]).unwrap();
        assert!(img.upscale(u32::MAX).is_err());
    }

    fn decode_bytes(png_bytes: &[u8]) -> RgbImage {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cs2mcp-frame-b-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("d.png");
        std::fs::write(&p, png_bytes).unwrap();
        let out = load_png_rgb(&p).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        out
    }
}
