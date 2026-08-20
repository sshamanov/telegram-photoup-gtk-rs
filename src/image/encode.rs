use mozjpeg::{ColorSpace, Compress, ScanMode};
use std::panic::catch_unwind;

/// Telegram rejects photos above ~10 MiB with PHOTO_SAVE_FILE_INVALID.
pub const MAX_PHOTO_BYTES: usize = 10_000_000;

/// Single export format: JPEG 4:4:4 mozjpeg. Port of photoup `makeOptions`:
/// quality + chroma_subsample=1(4:4:4), optimize_coding, trellis.
/// NOTE: mozjpeg defaults to PROGRESSIVE (its `jpeg_set_defaults` enables
/// `progressive_mode`, and the Rust crate exposes no baseline switch). photoup
/// encoded baseline (`baseline: true`), but scan order is pixel-neutral — the
/// decoded pixels are identical — so this is not a quality divergence. The trellis
/// options map onto mozjpeg's scan-optimization defaults
/// (`ScanMode::AllComponentsTogether` + use_scans_in_trellis).
pub fn encode_jpeg_444(
    rgb: &[u8],
    width: usize,
    height: usize,
    quality: f32,
) -> crate::errors::Result<Vec<u8>> {
    let expected = width * height * 3;
    if rgb.len() != expected {
        return Err(crate::errors::Error::Encode(format!(
            "encode input size mismatch: {} != {}",
            rgb.len(),
            expected
        )));
    }
    catch_unwind(|| {
        let mut comp = Compress::new(ColorSpace::JCS_RGB);
        // IMPORTANT ordering: `set_scan_optimization_mode` re-runs `jpeg_set_defaults`
        // internally, which would reset the quality/quant tables and chroma sampling
        // factors set afterwards. So it MUST be called first — then set_size/quality/
        // chroma/optimize on top of the defaults.
        comp.set_scan_optimization_mode(ScanMode::AllComponentsTogether);
        comp.set_size(width, height);
        comp.set_quality(quality);
        // 4:4:4 — no chroma subsampling.
        comp.set_chroma_sampling_pixel_sizes((1, 1), (1, 1));
        comp.set_optimize_coding(true);
        // Trellis with scan consideration (matches photoup's trellis_multipass).
        comp.set_use_scans_in_trellis(true);
        let mut started = comp
            .start_compress(Vec::new())
            .map_err(|e| crate::errors::Error::Encode(e.to_string()))?;
        for row in 0..height {
            let slice = &rgb[row * width * 3..(row + 1) * width * 3];
            started
                .write_scanlines(slice)
                .map_err(|e| crate::errors::Error::Encode(e.to_string()))?;
        }
        started
            .finish()
            .map_err(|e| crate::errors::Error::Encode(e.to_string()))
    })
    .map_err(|_| crate::errors::Error::Encode("mozjpeg panicked during encode".into()))?
}

/// Highest quality (≤100) whose file fits `max_bytes`, starting from Q100 and only
/// lowering when necessary. Falls back to the Q100 result if no quality in [40,100]
/// fits. Port of photoup's `encodeAdaptive` binary search.
pub fn encode_jpeg_444_adaptive(
    rgb: &[u8],
    width: usize,
    height: usize,
    max_bytes: usize,
) -> crate::errors::Result<Vec<u8>> {
    let mut best = encode_jpeg_444(rgb, width, height, 100.0)?;
    if best.len() <= max_bytes {
        return Ok(best);
    }
    let (mut lo, mut hi) = (40, 100);
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let buf = encode_jpeg_444(rgb, width, height, mid as f32)?;
        if buf.len() <= max_bytes {
            best = buf;
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `w`x`h` RGB gradient.
    fn gradient_rgb(w: usize, h: usize) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                v[i] = (x % 256) as u8;
                v[i + 1] = (y % 256) as u8;
                v[i + 2] = ((x + y) % 256) as u8;
            }
        }
        v
    }

    /// Parse the SOF0/2 frame header and return the per-component sampling factors.
    /// SOF segment: [0]=precision, [1..2]=height, [3..4]=width, [5]=components, then
    /// 3 bytes per component: id, hi*16|vi (sampling), quant-table index.
    fn sof_sampling(jpg: &[u8]) -> Option<Vec<(u8, u8)>> {
        let mut i = 2; // skip SOI
        while i + 10 < jpg.len() {
            if jpg[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = jpg[i + 1];
            let seg_len = (jpg[i + 2] as usize) << 8 | jpg[i + 3] as usize;
            if matches!(marker, 0xC0 | 0xC2) {
                let ncomp = (jpg[i + 9] as usize).min(3);
                let mut factors = Vec::new();
                for c in 0..ncomp {
                    let byte = jpg[i + 10 + c * 3 + 1];
                    factors.push((byte >> 4, byte & 0x0F));
                }
                return Some(factors);
            }
            // skip any other marker with a length
            i += 2 + seg_len;
        }
        None
    }

    #[test]
    fn encodes_valid_444_jpeg() {
        let (w, h) = (256, 256);
        let rgb = gradient_rgb(w, h);
        let jpg = encode_jpeg_444(&rgb, w, h, 90.0).expect("encode");
        assert!(jpg.len() > 100);
        // SOI marker
        assert_eq!(&jpg[0..2], &[0xFF, 0xD8]);
        // Must be 4:4:4 — every component sampled at (1,1), not (2,2)-subsampled chroma.
        let factors = sof_sampling(&jpg).expect("SOF header");
        assert_eq!(factors.len(), 3);
        assert!(
            factors.iter().all(|&(h, v)| h == 1 && v == 1),
            "expected 4:4:4 sampling (1,1)x3, got {factors:?}"
        );
    }

    #[test]
    fn adaptive_fits_budget() {
        let (w, h) = (2560, 2560);
        let rgb = gradient_rgb(w, h);
        // Budget must be satisfiable by the search range: Q40 yields ~187 KB for this
        // gradient, Q100 ~2 MB. A budget of 200 KB forces the binary search to lower
        // quality while still leaving a fitting quality in [40, 100].
        let jpg = encode_jpeg_444_adaptive(&rgb, w, h, 200_000).expect("encode");
        assert!(jpg.len() <= 200_000, "size {}", jpg.len());
    }

    #[test]
    fn q100_fits_10mb_budget() {
        let (w, h) = (2560, 2560);
        let rgb = gradient_rgb(w, h);
        let jpg = encode_jpeg_444(&rgb, w, h, 100.0).expect("encode");
        assert!(jpg.len() <= MAX_PHOTO_BYTES);
    }
}
