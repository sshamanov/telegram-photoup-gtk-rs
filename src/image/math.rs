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
    /// Highlight cap: the `hi_percentile`-brightest pixel is allowed to ride at
    /// most to this sRGB byte value. Caps the EV on skewed histograms so a mass
    /// of pixels never blows into pure white (the median-anchor failure: photos
    /// that aren't a normal distribution got hard-clipped, up to 20–40% white).
    pub hi_target: f32,
    pub hi_percentile: f32,
}

impl Default for AutoExpOpts {
    fn default() -> Self {
        Self {
            target: 128.0,
            min_ev: -3.0,
            max_ev: 4.0,
            percentile: 0.6,
            hi_target: 245.0,
            hi_percentile: 0.99,
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
/// Two bounds sit around the median anchor, because a single global EV can't both
/// center the midtones and respect a skewed histogram:
/// - **Highlight cap** (`hi_target`/`hi_percentile`): the brightest few percent of
///   pixels may reach at most `hi_target`, so dark-photo lifting never shoves a
///   big bright area into pure white. A skewed histogram therefore gets as much
///   lift as its highlights allow, and the dark bulk simply stays dark — the
///   alternative (centering the median) is what clipped 20–40% of pixels white.
/// - **No-darkening floor**: auto never goes negative, except as far as the
///   highlight cap demands (a photo already brighter than `hi_target` is pulled
///   back to it). Bright photos are left alone instead of "centered" down to gray.
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
    let median_ev = (srgb_to_linear(opts.target) / srgb_to_linear(at(opts.percentile)).max(1e-6)).log2();
    let cap_ev =
        (srgb_to_linear(opts.hi_target) / srgb_to_linear(at(opts.hi_percentile)).max(1e-6)).log2();
    let ev = clamp(median_ev, cap_ev.min(0.0), cap_ev.min(opts.max_ev));
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
