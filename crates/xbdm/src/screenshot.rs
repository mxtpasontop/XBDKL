//! Decoding of `screenshot` framebuffers into plain RGB.
//!
//! The console sends its front buffer as the GPU stores it: 32-bit pixels, usually in
//! the Xenos tiled layout (32x32 pixel tiles with interleaved micro tiles). Pixel bytes
//! arrive as B, G, R, X.

use crate::error::{Error, Result};

/// A decoded screenshot, 8-bit RGB, rows top to bottom.
#[derive(Debug, Clone)]
pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// Low 6 bits of the GPU format word: k_8_8_8_8.
const FORMAT_8_8_8_8: u32 = 6;
/// Format word bit set when the surface is tiled.
const FORMAT_TILED: u32 = 0x100;

/// Converts a raw framebuffer into RGB.
pub fn decode(fb: &[u8], width: u32, height: u32, pitch: u32, format: u32) -> Result<Screenshot> {
    if format & 0x3F != FORMAT_8_8_8_8 {
        return Err(Error::Protocol(format!("unsupported framebuffer format {format:#010x}")));
    }
    if width == 0 || height == 0 || pitch < width * 4 {
        return Err(Error::Protocol(format!("bad framebuffer geometry {width}x{height}, pitch {pitch}")));
    }
    let tiled = format & FORMAT_TILED != 0;
    let row_texels = pitch / 4;
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height {
        for x in 0..width {
            let texel = if tiled { tiled_offset(x, y, row_texels) } else { y * row_texels + x };
            let at = texel as usize * 4;
            let px = fb
                .get(at..at + 4)
                .ok_or_else(|| Error::Protocol("framebuffer smaller than its geometry".into()))?;
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    Ok(Screenshot { width, height, rgb })
}

/// Texel index of (x, y) in a tiled 32 bpp surface whose rows are `width` texels wide
/// (XGAddress2DTiledOffset with a 4-byte texel).
fn tiled_offset(x: u32, y: u32, width: u32) -> u32 {
    const LOG_BPP: u32 = 2;
    let aligned_width = (width + 31) & !31;
    let macro_ = ((x >> 5) + (y >> 5) * (aligned_width >> 5)) << (LOG_BPP + 7);
    let micro = ((x & 7) + ((y & 6) << 2)) << LOG_BPP;
    let offset = macro_ + ((micro & !15) << 1) + (micro & 15) + ((y & 8) << (3 + LOG_BPP)) + ((y & 1) << 4);
    (((offset & !511) << 3)
        + ((offset & 448) << 2)
        + (offset & 63)
        + ((y & 16) << 7)
        + (((((y & 8) >> 2) + (x >> 3)) & 3) << 6))
        >> LOG_BPP
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiled_offsets_are_a_permutation_of_each_tile_block() {
        // Every texel in a 64x64 region maps to a distinct index inside that region.
        let mut seen = vec![false; 64 * 64];
        for y in 0..64 {
            for x in 0..64 {
                let t = tiled_offset(x, y, 64) as usize;
                assert!(t < seen.len() && !seen[t], "collision at ({x},{y}) -> {t}");
                seen[t] = true;
            }
        }
    }

    #[test]
    fn linear_bgrx_to_rgb() {
        let fb = [1, 2, 3, 0, 4, 5, 6, 0];
        let shot = decode(&fb, 2, 1, 8, FORMAT_8_8_8_8).unwrap();
        assert_eq!(shot.rgb, [3, 2, 1, 6, 5, 4]);
        assert!(decode(&fb, 3, 1, 12, FORMAT_8_8_8_8).is_err());
        assert!(decode(&fb, 2, 1, 8, 0x1A).is_err());
    }
}
