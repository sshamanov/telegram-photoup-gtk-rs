use crate::image::types::{NormalizedCrop, Rect};

pub fn clamp(value: f32, min: f32, max: f32) -> f32 {
    value.max(min).min(max)
}

/// Scale a size down so the longest edge is at most `max_edge`, preserving aspect ratio.
pub fn fit_within(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (1, 1);
    }
    let scale = 1.0f32.min(max_edge as f32 / width.max(height) as f32);
    (
        ((width as f32 * scale).round()).max(1.0) as u32,
        ((height as f32 * scale).round()).max(1.0) as u32,
    )
}

/// Convert a normalized crop to integer pixel coordinates for a given source size.
pub fn crop_to_pixels(crop: &NormalizedCrop, width: u32, height: u32) -> Rect {
    Rect {
        x: (crop.x * width as f32).round() as u32,
        y: (crop.y * height as f32).round() as u32,
        width: ((crop.width * width as f32).round() as u32).max(1),
        height: ((crop.height * height as f32).round() as u32).max(1),
    }
}

pub struct AutoExpOpts {
    pub target: f32,
    pub min_ev: f32,
    pub max_ev: f32,
    pub percentile: f32,
}

impl Default for AutoExpOpts {
    fn default() -> Self {
        Self { target: 128.0, min_ev: -3.0, max_ev: 4.0, percentile: 0.6 }
    }
}

/// Global auto-exposure as an EV offset, from a downsampled luminance distribution.
/// Port of photoup `autoExposureEV` (math.ts).
pub fn auto_exposure_ev(luminances: &[u8], opts: &AutoExpOpts) -> f32 {
    if luminances.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<u8> = luminances.to_vec();
    sorted.sort_unstable();
    let idx = (sorted.len() - 1).min((sorted.len() as f32 * opts.percentile) as usize);
    let measured = sorted[idx] as f32;
    let ev = (opts.target / measured.max(1.0)).log2();
    clamp(ev, opts.min_ev, opts.max_ev)
}

/// Average channel means for a gray-world reference (used by neutral picker).
pub fn channel_means(rgb: &[u8]) -> (f32, f32, f32) {
    let n = rgb.len() / 3;
    if n == 0 {
        return (0.0, 0.0, 0.0);
    }
    let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
    for px in rgb.chunks_exact(3) {
        r += px[0] as u64;
        g += px[1] as u64;
        b += px[2] as u64;
    }
    (
        r as f32 / n as f32,
        g as f32 / n as f32,
        b as f32 / n as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_within_scales_longest_edge() {
        assert_eq!(fit_within(3000, 2000, 2560), (2560, 1707));
        assert_eq!(fit_within(1000, 1000, 512), (512, 512));
        assert_eq!(fit_within(100, 100, 512), (100, 100)); // never upscales
    }

    #[test]
    fn crop_to_pixels_rounds_to_int() {
        let crop = NormalizedCrop { x: 0.1, y: 0.2, width: 0.5, height: 0.5 };
        let rect = crop_to_pixels(&crop, 1000, 800);
        assert_eq!(rect, Rect { x: 100, y: 160, width: 500, height: 400 });
    }

    #[test]
    fn auto_exposure_raises_dark_image() {
        // All pixels at ~1/4 brightness → strong positive EV.
        let lums = vec![60u8; 100];
        let ev = auto_exposure_ev(&lums, &AutoExpOpts { target: 180.0, ..Default::default() });
        assert!(ev > 1.0 && ev < 4.0, "ev was {ev}");
    }

    #[test]
    fn auto_exposure_clamps_max() {
        // Zero luminance → target/measured = 180/1 → log2 ≈ 7.49 EV, above max_ev.
        let lums = vec![0u8; 100];
        let ev = auto_exposure_ev(&lums, &AutoExpOpts { target: 180.0, max_ev: 0.5, ..Default::default() });
        assert_eq!(ev, 0.5, "expected clamp to max_ev, got {ev}");
    }

    #[test]
    fn auto_exposure_clamps_min() {
        // Max-brightness, target 128 → log2(128/255) ≈ −0.99, below min_ev.
        let lums = vec![255u8; 100];
        let ev = auto_exposure_ev(&lums, &AutoExpOpts { target: 128.0, min_ev: 0.0, ..Default::default() });
        assert_eq!(ev, 0.0, "expected clamp to min_ev, got {ev}");
    }
}
