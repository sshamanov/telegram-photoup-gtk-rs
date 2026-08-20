use crate::image::math::{auto_exposure_ev, crop_to_pixels, fit_within, AutoExpOpts};
use crate::image::resize::downscale_rgba;
use crate::image::srgb::{gain_coefficients, jpeg_tone_lut, srgb_to_linear, tone_index};
use crate::image::types::{Adjustments, ExposureMode, NormalizedCrop, Rect, Size};

/// A decoded source ready to render at any size. Mirrors photoup's `DecodedBase`.
pub trait Base: Send + Sync {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    /// Apply exposure/WB/rolloff and return sRGB RGBA8 + the auto-exposure EV.
    fn render(&self, crop: Option<&NormalizedCrop>, size: Size, adjustments: &Adjustments) -> RenderResult;
}

pub struct RenderResult {
    pub rgba: Vec<u8>,
    pub auto_ev: f32,
}

pub fn crop_rect(width: u32, height: u32, crop: Option<&NormalizedCrop>) -> Rect {
    match crop {
        Some(c) => crop_to_pixels(c, width, height),
        None => Rect { x: 0, y: 0, width, height },
    }
}

/// Sample ≤128px-edge luminance distribution over the crop (photoup `sampleLuminances`).
fn sample_luminances_rgba(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    let scale = 1.0f32.min(128.0 / w.max(h) as f32);
    let sw = (w as f32 * scale).max(1.0) as u32;
    let sh = (h as f32 * scale).max(1.0) as u32;
    let down = downscale_rgba(rgba, w, h, sw, sh); // channel-strided, keeps colors separate
    let mut lums = Vec::with_capacity((sw * sh) as usize);
    for px in down.chunks_exact(4) {
        lums.push((0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32).round() as u8);
    }
    lums
}

fn auto_ev_for(lums: &[u8], aggressive: bool) -> f32 {
    auto_exposure_ev(
        lums,
        &AutoExpOpts { target: if aggressive { 230.0 } else { 180.0 }, max_ev: 4.0, ..Default::default() },
    )
}

fn effective_ev(mode: ExposureMode, manual_ev: f32, auto_ev: f32) -> f32 {
    if mode == ExposureMode::Manual { manual_ev } else { auto_ev }
}

/// JPEG path: downscale in sRGB space, then apply exposure/WB/rolloff per pixel
/// (photoup's `applyPixelTransform`). Color is only ever exposure + WB — never hue-saturation games.
pub struct JpegBase {
    width: u32,
    height: u32,
    /// Full-res sRGB RGBA8.
    rgba: Vec<u8>,
}

impl JpegBase {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Self {
        Self { width, height, rgba }
    }
}

impl Base for JpegBase {
    fn width(&self) -> u32 { self.width }
    fn height(&self) -> u32 { self.height }

    fn render(&self, crop: Option<&NormalizedCrop>, size: Size, adjustments: &Adjustments) -> RenderResult {
        let rect = crop_rect(self.width, self.height, crop);
        let aggressive = adjustments.exposure_mode == ExposureMode::Aggressive;

        // Materialize the crop (if any) once. The full image is the fast path; the
        // crop is extracted to its own buffer so both luminance sampling and the
        // downscale operate on the same rect-sized source (photoup's drawImage crop).
        let full = rect == Rect { x: 0, y: 0, width: self.width, height: self.height };
        let mut cropped: Vec<u8>;
        let src: &[u8] = if full {
            &self.rgba
        } else {
            cropped = Vec::with_capacity((rect.width * rect.height * 4) as usize);
            for y in 0..rect.height as usize {
                let src_off = ((rect.y as usize + y) * self.width as usize + rect.x as usize) * 4;
                cropped.extend_from_slice(&self.rgba[src_off..src_off + rect.width as usize * 4]);
            }
            &cropped
        };

        let lums = sample_luminances_rgba(src, rect.width, rect.height);
        let auto_ev = auto_ev_for(&lums, aggressive);
        let ev = effective_ev(adjustments.exposure_mode, adjustments.exposure_ev, auto_ev);

        // Downscale the (cropped) source to the target size in sRGB space.
        let mut down = downscale_rgba(src, rect.width, rect.height, size.width, size.height);

        let lut = jpeg_tone_lut(aggressive); // JPEG never gets the RAW S-curve
        let (gr, gg, gb) = gain_coefficients(ev, adjustments.wb_offset, adjustments.hue);
        let s2l = srgb_to_linear();
        let mut rgba = vec![0u8; (size.width * size.height * 4) as usize];
        for px in down.chunks_exact_mut(4).zip(rgba.chunks_exact_mut(4)) {
            let (spx, dst) = (px.0, px.1);
            let r = s2l[spx[0] as usize] * gr;
            let g = s2l[spx[1] as usize] * gg;
            let b = s2l[spx[2] as usize] * gb;
            dst[0] = lut[tone_index(r)];
            dst[1] = lut[tone_index(g)];
            dst[2] = lut[tone_index(b)];
            dst[3] = 255;
        }

        RenderResult { rgba, auto_ev }
    }
}

/// 256-bin luminance histogram over the final sRGB pixels (photoup `computeHistogram`).
pub fn compute_histogram(rgba: &[u8]) -> Vec<u32> {
    let mut bins = vec![0u32; 256];
    for px in rgba.chunks_exact(4) {
        let l = (0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32) as usize;
        bins[l.min(255)] += 1;
    }
    bins
}

/// The size an export would be rendered at (photoup `exportDimensions`).
pub fn export_dimensions(width: u32, height: u32, crop: Option<&NormalizedCrop>, export_edge: u32) -> Size {
    let rect = crop_rect(width, height, crop);
    let (w, h) = fit_within(rect.width, rect.height, export_edge);
    Size { width: w, height: h }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::srgb::{highlight_rolloff, linear_to_srgb_byte};

    fn gray_linear(x: f32) -> u8 {
        linear_to_srgb_byte(x)
    }

    /// A JPEG base that is a uniform gray at linear level `lin` (sRGB byte = srgb(lin)).
    fn uniform_base(lin: f32, w: u32, h: u32) -> JpegBase {
        let byte = gray_linear(lin);
        let rgba = vec![byte; (w * h * 4) as usize];
        JpegBase::new(w, h, rgba)
    }

    #[test]
    fn render_raises_dark_jpeg() {
        // Uniform 8% linear gray (very dark). Auto exposure should lift it well above the source.
        let base = uniform_base(0.08, 128, 128);
        let out = base.render(None, Size { width: 64, height: 64 }, &Adjustments::default());
        assert!(out.auto_ev > 0.0, "auto_ev {}", out.auto_ev);
        // After exposure, mid-gray should be clearly brighter than 0.08 linear → > sRGB 77.
        let mid = out.rgba[0];
        assert!(mid > 77, "mid {} too dark", mid);
    }

    #[test]
    fn manual_exposure_controls_brightness() {
        let base = uniform_base(0.5, 64, 64);
        let mut adj = Adjustments::default();
        adj.exposure_mode = ExposureMode::Manual;
        adj.exposure_ev = -2.0; // darken 2 stops
        let out = base.render(None, Size { width: 32, height: 32 }, &adj);
        // 0.5 linear at -2EV → 0.125 linear → sRGB ~99.
        assert!((out.rgba[0] as f32 - 99.0).abs() < 6.0, "got {}", out.rgba[0]);
    }

    #[test]
    fn rolloff_preserves_highlight_detail() {
        // A value in the rolloff band must be below its linear→srgb identity.
        let x = 0.95f32;
        let with_rolloff = linear_to_srgb_byte(highlight_rolloff(x));
        let without = linear_to_srgb_byte(x);
        assert!(with_rolloff < without);
    }

    #[test]
    fn histogram_bins_sum_to_pixels() {
        let base = uniform_base(0.5, 32, 32);
        let out = base.render(None, Size { width: 16, height: 16 }, &Adjustments::default());
        let hist = compute_histogram(&out.rgba);
        assert_eq!(hist.iter().sum::<u32>(), 16 * 16);
    }
}
