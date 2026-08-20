use crate::image::math::{AutoExpOpts, auto_exposure_ev, crop_to_pixels, fit_within};
use crate::image::resize::{downscale_crop, downscale_rgba};
use crate::image::srgb::{
    gain_coefficients, jpeg_tone_lut, raw_tone_lut, srgb_to_linear, tone_index, wb_gains,
    wb_transform3x3,
};
use crate::image::types::{Adjustments, DecodedRaw, ExposureMode, NormalizedCrop, Rect, Size};

/// A decoded source ready to render at any size. Mirrors photoup's `DecodedBase`.
pub trait Base: Send + Sync {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    /// Apply exposure/WB/rolloff and return sRGB RGBA8 + the auto-exposure EV.
    fn render(
        &self,
        crop: Option<&NormalizedCrop>,
        size: Size,
        adjustments: &Adjustments,
    ) -> RenderResult;
}

pub struct RenderResult {
    pub rgba: Vec<u8>,
    pub auto_ev: f32,
}

pub fn crop_rect(width: u32, height: u32, crop: Option<&NormalizedCrop>) -> Rect {
    match crop {
        Some(c) => crop_to_pixels(c, width, height),
        None => Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
    }
}

/// Sample ≤128px-edge luminance distribution over the crop (photoup `sampleLuminances`).
fn sample_luminances_rgba(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    let scale = 1.0f32.min(128.0 / w.max(h) as f32);
    let sw = (w as f32 * scale).round().max(1.0) as u32;
    let sh = (h as f32 * scale).round().max(1.0) as u32;
    let down = downscale_rgba(rgba, w, h, sw, sh); // channel-strided, keeps colors separate
    let mut lums = Vec::with_capacity((sw * sh) as usize);
    for px in down.chunks_exact(4) {
        lums.push(
            (0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32).round() as u8,
        );
    }
    lums
}

fn auto_ev_for(lums: &[u8], aggressive: bool) -> f32 {
    auto_exposure_ev(
        lums,
        &AutoExpOpts {
            target: if aggressive { 230.0 } else { 180.0 },
            max_ev: 4.0,
            ..Default::default()
        },
    )
}

fn effective_ev(mode: ExposureMode, manual_ev: f32, auto_ev: f32) -> f32 {
    if mode == ExposureMode::Manual {
        manual_ev
    } else {
        auto_ev
    }
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
        Self {
            width,
            height,
            rgba,
        }
    }
}

impl Base for JpegBase {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }

    fn render(
        &self,
        crop: Option<&NormalizedCrop>,
        size: Size,
        adjustments: &Adjustments,
    ) -> RenderResult {
        let rect = crop_rect(self.width, self.height, crop);
        let aggressive = adjustments.exposure_mode == ExposureMode::Aggressive;

        // Materialize the crop (if any) once. The full image is the fast path; the
        // crop is extracted to its own buffer so both luminance sampling and the
        // downscale operate on the same rect-sized source (photoup's drawImage crop).
        let full = rect
            == Rect {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
            };
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

/// RAW path: downscale in LINEAR space, then apply exposure/WB (through the camera
/// matrix when available) + rolloff + camera-Standard curve. Port of `LinearRgbBase`.
pub struct RawBase {
    width: u32,
    height: u32,
    full: DecodedRaw,
    cam_matrix3: Option<[[f32; 3]; 3]>,
}

impl RawBase {
    pub fn new(full: DecodedRaw) -> Self {
        // rgb_cam[3][4]; use first 3 columns as the 3x3.
        let cam_matrix3 = full.cam_matrix.map(|m| [
            [m[0][0] as f32, m[0][1] as f32, m[0][2] as f32],
            [m[1][0] as f32, m[1][1] as f32, m[1][2] as f32],
            [m[2][0] as f32, m[2][1] as f32, m[2][2] as f32],
        ]);
        Self {
            width: full.width,
            height: full.height,
            full,
            cam_matrix3,
        }
    }

    fn luminance_sample(&self, rect: &Rect, size: Size) -> Vec<u8> {
        let r = downscale_crop(
            &self.full.r,
            self.full.width,
            self.full.height,
            rect,
            size.width,
            size.height,
        );
        let g = downscale_crop(
            &self.full.g,
            self.full.width,
            self.full.height,
            rect,
            size.width,
            size.height,
        );
        let b = downscale_crop(
            &self.full.b,
            self.full.width,
            self.full.height,
            rect,
            size.width,
            size.height,
        );
        let mut lums = vec![0u8; (size.width * size.height) as usize];
        for i in 0..(size.width * size.height) as usize {
            let to_byte = |lin: f32| -> u8 {
                let c = lin.clamp(0.0, 1.0);
                let out = if c <= 0.0031308 {
                    c * 12.92
                } else {
                    1.055 * c.powf(1.0 / 2.4) - 0.055
                };
                (out.clamp(0.0, 1.0) * 255.0).round() as u8
            };
            let sr = to_byte(r[i]);
            let sg = to_byte(g[i]);
            let sb = to_byte(b[i]);
            lums[i] = (0.2126 * sr as f32 + 0.7152 * sg as f32 + 0.0722 * sb as f32).round() as u8;
        }
        lums
    }
}

impl Base for RawBase {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }

    fn render(
        &self,
        crop: Option<&NormalizedCrop>,
        size: Size,
        adjustments: &Adjustments,
    ) -> RenderResult {
        let rect = crop_rect(self.width, self.height, crop);
        let aggressive = adjustments.exposure_mode == ExposureMode::Aggressive;

        let ev_sample = fit_within(rect.width, rect.height, 128);
        let lums = self.luminance_sample(
            &rect,
            Size {
                width: ev_sample.0,
                height: ev_sample.1,
            },
        );
        let auto_ev = auto_ev_for(&lums, aggressive);
        let ev = effective_ev(adjustments.exposure_mode, adjustments.exposure_ev, auto_ev);

        let r = downscale_crop(
            &self.full.r,
            self.full.width,
            self.full.height,
            &rect,
            size.width,
            size.height,
        );
        let g = downscale_crop(
            &self.full.g,
            self.full.width,
            self.full.height,
            &rect,
            size.width,
            size.height,
        );
        let b = downscale_crop(
            &self.full.b,
            self.full.width,
            self.full.height,
            &rect,
            size.width,
            size.height,
        );

        let gain = 2.0f32.powf(ev);
        let (wr, wg, wb) = wb_gains(adjustments.wb_offset, adjustments.hue);
        let lut = raw_tone_lut(aggressive); // RAW gets the camera-Standard S-curve
        let mut rgba = vec![0u8; (size.width * size.height * 4) as usize];

        // With the camera color matrix, apply WB as T = M·diag(wb)·M⁻¹ so the preview
        // matches the pre-matrix userMul export (photoup `applyLinearTransform`).
        let t = self
            .cam_matrix3
            .as_ref()
            .and_then(|m| wb_transform3x3(m, (wr, wg, wb)));

        let n = (size.width * size.height) as usize;
        if let Some(t) = t {
            for i in 0..n {
                let o = i * 4;
                let r1 = (t[0] * r[i] + t[1] * g[i] + t[2] * b[i]) * gain;
                let g1 = (t[3] * r[i] + t[4] * g[i] + t[5] * b[i]) * gain;
                let b1 = (t[6] * r[i] + t[7] * g[i] + t[8] * b[i]) * gain;
                rgba[o] = lut[tone_index(r1)];
                rgba[o + 1] = lut[tone_index(g1)];
                rgba[o + 2] = lut[tone_index(b1)];
                rgba[o + 3] = 255;
            }
        } else {
            for i in 0..n {
                let o = i * 4;
                rgba[o] = lut[tone_index(r[i] * gain * wr)];
                rgba[o + 1] = lut[tone_index(g[i] * gain * wg)];
                rgba[o + 2] = lut[tone_index(b[i] * gain * wb)];
                rgba[o + 3] = 255;
            }
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
pub fn export_dimensions(
    width: u32,
    height: u32,
    crop: Option<&NormalizedCrop>,
    export_edge: u32,
) -> Size {
    let rect = crop_rect(width, height, crop);
    let (w, h) = fit_within(rect.width, rect.height, export_edge);
    Size {
        width: w,
        height: h,
    }
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
        let out = base.render(
            None,
            Size {
                width: 64,
                height: 64,
            },
            &Adjustments::default(),
        );
        assert!(out.auto_ev > 0.0, "auto_ev {}", out.auto_ev);
        // Auto-exposed 0.08 linear lands ~sRGB 117 → well above the unexposed ~80.
        let mid = out.rgba[0];
        assert!(mid > 110, "mid {} too dark", mid);
    }

    #[test]
    fn manual_exposure_controls_brightness() {
        let base = uniform_base(0.5, 64, 64);
        let mut adj = Adjustments::default();
        adj.exposure_mode = ExposureMode::Manual;
        adj.exposure_ev = -2.0; // darken 2 stops
        let out = base.render(
            None,
            Size {
                width: 32,
                height: 32,
            },
            &adj,
        );
        // 0.5 linear at -2EV → 0.125 linear → sRGB ~99.
        assert!(
            (out.rgba[0] as f32 - 99.0).abs() < 6.0,
            "got {}",
            out.rgba[0]
        );
    }

    #[test]
    fn crop_selects_region_for_exposure() {
        // Two-tone base: left half dark (linear 0.02), right half bright (linear 0.5).
        let (w, h) = (64, 32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let byte = if x < w / 2 {
                    gray_linear(0.02)
                } else {
                    gray_linear(0.5)
                };
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = byte;
                rgba[i + 1] = byte;
                rgba[i + 2] = byte;
                rgba[i + 3] = 255;
            }
        }
        let base = JpegBase::new(w, h, rgba);
        // Crop to the bright right half.
        let crop = NormalizedCrop {
            x: 0.5,
            y: 0.0,
            width: 0.5,
            height: 1.0,
        };
        let out = base.render(
            Some(&crop),
            Size {
                width: 16,
                height: 16,
            },
            &Adjustments::default(),
        );
        // Sampling the bright half → auto-EV is slightly negative (already bright).
        assert!(
            out.auto_ev < 0.0,
            "auto_ev {} should be negative for a bright crop",
            out.auto_ev
        );
        // The rendered crop stays bright (linear 0.5 at ~-0.06EV → sRGB ~184).
        assert!(out.rgba[0] > 150, "crop render too dark: {}", out.rgba[0]);
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
        let out = base.render(
            None,
            Size {
                width: 16,
                height: 16,
            },
            &Adjustments::default(),
        );
        let hist = compute_histogram(&out.rgba);
        assert_eq!(hist.iter().sum::<u32>(), 16 * 16);
    }
}

#[cfg(test)]
mod raw_base_tests {
    use super::*;
    use crate::image::types::DecodedRaw;

    fn synth_raw(w: u32, h: u32, value: f32) -> RawBase {
        let n = (w * h) as usize;
        let mut dr = DecodedRaw {
            width: w,
            height: h,
            r: vec![value; n],
            g: vec![value; n],
            b: vec![value; n],
            cam_mul: None,
            cam_matrix: None,
        };
        // give it a slight gradient so downscale is exercised
        for i in 0..n {
            dr.r[i] = value + (i % 7) as f32 * 0.001;
        }
        RawBase::new(dr)
    }

    #[test]
    fn raw_auto_exposure_lifts_dark() {
        let base = synth_raw(256, 256, 0.06);
        let out = base.render(
            None,
            Size {
                width: 128,
                height: 128,
            },
            &Adjustments::default(),
        );
        assert!(out.auto_ev > 0.0);
        assert!(out.rgba[0] > 60, "got {}", out.rgba[0]);
    }

    #[test]
    fn raw_matches_jpeg_rolloff_baseline() {
        // Neutral WB, auto exposure, mid-gray linear 0.5 → should not clip or vanish.
        let base = synth_raw(64, 64, 0.5);
        let out = base.render(
            None,
            Size {
                width: 32,
                height: 32,
            },
            &Adjustments::default(),
        );
        assert!(out.rgba[0] > 100 && out.rgba[0] < 255);
    }
}
