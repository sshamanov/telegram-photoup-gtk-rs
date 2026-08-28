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

/// Clamp a normalized crop into the frame `[0,1]²`, keeping at least `min_frac`
/// in each dimension. Never panics — NaN, negative, oversized, or empty inputs
/// all land on a valid in-frame crop. (The per-axis bounds are computed before
/// the clamp, so the clamp's min ≤ max is guaranteed by construction.)
pub fn clamp_crop(c: NormalizedCrop, min_frac: f32) -> NormalizedCrop {
    let min_frac = min_frac.max(1e-3);
    let x = if c.x.is_nan() { 0.0 } else { c.x };
    let y = if c.y.is_nan() { 0.0 } else { c.y };
    let mut w = if c.width.is_nan() { 1.0 } else { c.width };
    let mut h = if c.height.is_nan() { 1.0 } else { c.height };
    // Floor the size, then collapse anything oversized to the full frame.
    w = w.max(min_frac).min(1.0);
    h = h.max(min_frac).min(1.0);
    // Origin clamped so the rect stays inside — bounds are ≥ 0 here.
    NormalizedCrop {
        x: x.clamp(0.0, 1.0 - w),
        y: y.clamp(0.0, 1.0 - h),
        width: w,
        height: h,
    }
}

/// Convert a normalized crop to integer pixel coordinates for a given source
/// size. The result is clamped into the image: no `x + width` may exceed the
/// source (rounding of a crop hugging the right/bottom edge would otherwise
/// overhang by a pixel or two and panic the crop-extraction loop in
/// `process.rs`).
pub fn crop_to_pixels(crop: &NormalizedCrop, width: u32, height: u32) -> Rect {
    let width = width.max(1);
    let height = height.max(1);
    let x = ((crop.x * width as f32).round() as i64).clamp(0, width as i64 - 1);
    let y = ((crop.y * height as f32).round() as i64).clamp(0, height as i64 - 1);
    let w = ((crop.width * width as f32).round() as i64).max(1).min(width as i64 - x);
    let h = ((crop.height * height as f32).round() as i64).max(1).min(height as i64 - y);
    Rect {
        x: x as u32,
        y: y as u32,
        width: w as u32,
        height: h as u32,
    }
}

pub struct AutoExpOpts {
    pub target: f32,
    pub min_ev: f32,
    pub max_ev: f32,
    pub percentile: f32,
    /// Highlight cap (`Auto`): the `hi_percentile`-brightest pixel may ride at
    /// most to this sRGB byte value, and the cap is clamped at ≥ 0 — a photo
    /// already brighter than it keeps its white point instead of being dragged
    /// down pointlessly. Keeps a skewed histogram from blowing a mass of pixels
    /// into pure white.
    pub hi_target: f32,
    pub hi_percentile: f32,
    /// When true (`Burn`), the highlight cap is dropped entirely: the
    /// median anchor governs the lift and highlights may clip to white. The two
    /// modes therefore differ in exactly what they should — clipping and white
    /// point — not in midtone target.
    pub clip_highlights: bool,
}

impl Default for AutoExpOpts {
    fn default() -> Self {
        Self {
            target: 128.0,
            min_ev: -3.0,
            max_ev: 4.0,
            percentile: 0.6,
            hi_target: 252.0,
            hi_percentile: 0.99,
            clip_highlights: false,
        }
    }
}

/// Global auto-exposure as an EV offset, from a downsampled luminance distribution.
///
/// The luminance bytes are sRGB-encoded, but the exposure gain is applied in
/// LINEAR space (linear value × 2^EV, then a linear→sRGB tone LUT). So the EV is
/// solved in linear space too: the anchor percentile pixel lands exactly on
/// `opts.target` in the tone-mapped output. (The naive sRGB ratio would land the
/// anchor ~25–30% below target because of gamma — that's the old "exposure feels
/// weak" bug.)
///
/// Auto never darkens a photo: EV is floored at 0, so bright photos are left
/// alone instead of "centered" down to gray. The two exposure modes differ only
/// in how they treat highlights:
/// - **Auto** (`clip_highlights: false`): a highlight cap binds the lift — the
///   `hi_percentile`-brightest pixel may ride at most to `hi_target` (just under
///   white), so a skewed histogram never blows a mass of pixels into pure white
///   and the dark bulk simply stays dark. The cap is clamped at ≥ 0: a photo
///   already brighter than `hi_target` keeps its white point (EV 0), never a
///   pointless drag-down.
/// - **Burn** (`clip_highlights: true`): no cap — the median anchor
///   governs the lift up to `max_ev` and highlights are free to clip to white.
pub fn auto_exposure_ev(luminances: &[u8], opts: &AutoExpOpts) -> f32 {
    if luminances.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<u8> = luminances.to_vec();
    sorted.sort_unstable();
    let srgb_to_linear = |v: f32| {
        let c = (v / 255.0).clamp(0.0, 1.0);
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let at = |p: f32| {
        let idx = (sorted.len() - 1).min((sorted.len() as f32 * p) as usize);
        sorted[idx] as f32
    };
    let median_ev =
        (srgb_to_linear(opts.target) / srgb_to_linear(at(opts.percentile)).max(1e-6)).log2();
    let ev = if opts.clip_highlights {
        clamp(median_ev, 0.0, opts.max_ev)
    } else {
        let cap_ev = (srgb_to_linear(opts.hi_target)
            / srgb_to_linear(at(opts.hi_percentile)).max(1e-6))
        .log2()
        .max(0.0);
        clamp(median_ev, 0.0, cap_ev.min(opts.max_ev))
    };
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
        let crop = NormalizedCrop {
            x: 0.1,
            y: 0.2,
            width: 0.5,
            height: 0.5,
        };
        let rect = crop_to_pixels(&crop, 1000, 800);
        assert_eq!(
            rect,
            Rect {
                x: 100,
                y: 160,
                width: 500,
                height: 400
            }
        );
    }

    /// Regression: the "one photo stuck" panic. A crop hugging the right edge
    /// rounds its x and width up independently, so a crop whose normalized
    /// x + width is ≈ 1.0 used to produce a pixel rect that overhangs the image
    /// by a couple of pixels — the crop-extraction loop then read past the
    /// buffer (`range end index ... out of range`).
    #[test]
    fn crop_to_pixels_clamps_overhang() {
        let (w, h) = (3680u32, 2456u32);
        // x + width ≈ 1.0 exactly; both terms round up → old code gave W+2.
        let edge = NormalizedCrop { x: 0.4999, y: 0.0, width: 0.5001, height: 1.0 };
        let r = crop_to_pixels(&edge, w, h);
        assert!(r.x + r.width <= w, "right overhang: {:?}", r);
        assert!(r.y + r.height <= h, "bottom overhang: {:?}", r);
        assert!(r.width >= 1 && r.height >= 1, "collapsed: {:?}", r);
        // Full frame stays full frame.
        let full = NormalizedCrop { x: 0.0, y: 0.0, width: 1.0, height: 1.0 };
        assert_eq!(crop_to_pixels(&full, w, h), Rect { x: 0, y: 0, width: 3680, height: 2456 });
        // Degenerate inputs never panic and never produce an empty rect.
        let bad = NormalizedCrop { x: f32::NAN, y: -2.0, width: 3.0, height: f32::INFINITY };
        let r = crop_to_pixels(&bad, w, h);
        assert!(r.x + r.width <= w && r.y + r.height <= h && r.width >= 1 && r.height >= 1);
    }

    #[test]
    fn clamp_crop_sanitizes_degenerate_input() {
        let c = clamp_crop(
            NormalizedCrop { x: f32::NAN, y: -1.0, width: 0.0, height: 2.0 },
            0.05,
        );
        assert!(c.x >= 0.0 && c.x + c.width <= 1.0 + 1e-6);
        assert!(c.y >= 0.0 && c.y + c.height <= 1.0 + 1e-6);
        assert!(c.width >= 0.05 && c.height >= 0.05);
        // In-frame input is unchanged (identity).
        let ok = NormalizedCrop { x: 0.3, y: 0.2, width: 0.4, height: 0.5 };
        assert_eq!(clamp_crop(ok, 0.05), ok);
    }

    #[test]
    fn auto_exposure_raises_dark_image() {
        // All pixels at ~1/4 brightness → strong positive EV.
        let lums = vec![60u8; 100];
        let ev = auto_exposure_ev(
            &lums,
            &AutoExpOpts {
                target: 180.0,
                ..Default::default()
            },
        );
        assert!(ev > 1.0 && ev < 4.0, "ev was {ev}");
    }

    #[test]
    fn auto_exposure_clamps_max() {
        // Zero luminance → target/measured = 180/1 → log2 ≈ 7.49 EV, above max_ev.
        let lums = vec![0u8; 100];
        let ev = auto_exposure_ev(
            &lums,
            &AutoExpOpts {
                target: 180.0,
                max_ev: 0.5,
                ..Default::default()
            },
        );
        assert_eq!(ev, 0.5, "expected clamp to max_ev, got {ev}");
    }

    #[test]
    fn auto_exposure_clamps_min() {
        // Max-brightness, target 128 → log2(128/255) ≈ −0.99, below min_ev.
        let lums = vec![255u8; 100];
        let ev = auto_exposure_ev(
            &lums,
            &AutoExpOpts {
                target: 128.0,
                min_ev: 0.0,
                ..Default::default()
            },
        );
        assert_eq!(ev, 0.0, "expected clamp to min_ev, got {ev}");
    }
}
