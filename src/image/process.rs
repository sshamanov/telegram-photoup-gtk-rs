use crate::image::math::{AutoExpOpts, auto_exposure_ev, crop_to_pixels, fit_within};
use crate::image::resize::{downscale_crop, downscale_plane, downscale_rgba};
use crate::image::srgb::{
    gain_coefficients, jpeg_tone_lut, linear_to_srgb_byte, raw_tone_lut, srgb_to_linear, tone_index,
    wb_gains, wb_transform3x3,
};
use crate::image::types::{Adjustments, DecodedRaw, ExposureMode, NormalizedCrop, Rect, Size};
use std::sync::Arc;

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
    ) -> RenderResult {
        self.render_with_ev(crop, size, adjustments, None)
    }
    /// Like `render`, but `ev` overrides the effective exposure EV. The editor's
    /// preview uses this to apply a crop-aware auto-EV (computed over the cropped
    /// region) while still displaying the full frame under the crop overlay.
    fn render_with_ev(
        &self,
        crop: Option<&NormalizedCrop>,
        size: Size,
        adjustments: &Adjustments,
        ev: Option<f32>,
    ) -> RenderResult;
    /// Downscaled LINEAR (0..1) RGB of the full frame — before exposure/WB/tone.
    /// Used by the WB pick/auto so the measured cast is the true sensor cast, not
    /// the tone-mapped preview's. Interleaved R,G,B, length `size.width*size.height*3`.
    fn linear_sample(&self, size: Size) -> Vec<f32>;
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

/// Clockwise quarter-turn dimensions (`q` = 0..3, 0 = none). Odd quarters swap W/H.
pub fn rotate_dims(w: u32, h: u32, q: u8) -> (u32, u32) {
    if q & 1 == 1 { (h, w) } else { (w, h) }
}

/// Rotate a `w*h*ch` interleaved buffer by `q` quarter-turns clockwise.
/// Returns `(buffer, out_w, out_h)`. Mapping (source → destination):
/// q=1: out(ox, oy) = in(oy, h-1-ox); q=2: out = in flipped both axes;
/// q=3: out(ox, oy) = in(w-1-oy, ox). dims swap for odd q.
fn rotate_channels<T: Copy + Default>(src: &[T], w: u32, h: u32, q: u8, ch: usize) -> (Vec<T>, u32, u32) {
    let (ow, oh) = rotate_dims(w, h, q);
    let n = (w as usize) * (h as usize);
    debug_assert_eq!(src.len(), n * ch);
    let mut out = vec![T::default(); n * ch];
    match q {
        0 => out.copy_from_slice(src),
        1 => {
            for y in 0..h {
                for x in 0..w {
                    let si = (y * w + x) as usize * ch;
                    let di = (x * h + (h - 1 - y)) as usize * ch;
                    out[di..di + ch].copy_from_slice(&src[si..si + ch]);
                }
            }
        }
        2 => {
            for y in 0..h {
                for x in 0..w {
                    let si = (y * w + x) as usize * ch;
                    let di = ((h - 1 - y) * w + (w - 1 - x)) as usize * ch;
                    out[di..di + ch].copy_from_slice(&src[si..si + ch]);
                }
            }
        }
        _ => {
            for y in 0..h {
                for x in 0..w {
                    let si = (y * w + x) as usize * ch;
                    let di = ((w - 1 - x) * h + y) as usize * ch;
                    out[di..di + ch].copy_from_slice(&src[si..si + ch]);
                }
            }
        }
    }
    (out, ow, oh)
}

/// Rotate a `w*h*4` RGBA buffer. See `rotate_channels`.
pub fn rotate_rgba(src: &[u8], w: u32, h: u32, q: u8) -> (Vec<u8>, u32, u32) {
    rotate_channels(src, w, h, q, 4)
}

/// Rotate a `w*h*3` interleaved LINEAR RGB sample (the WB pick/auto buffer).
fn rotate_linear3(src: &[f32], w: u32, h: u32, q: u8) -> (Vec<f32>, u32, u32) {
    rotate_channels(src, w, h, q, 3)
}

/// Orientation wrapper: renders the inner base, then rotates the buffer by
/// `quarters` clockwise quarter-turns (0..3). All coordinates the caller sees
/// — target `Size`, `full` dims, WB sample dims — are in the DISPLAY (rotated)
/// space: `render_with_ev` back-rotates the target size so the inner render has
/// the base aspect, and `linear_sample` returns display-oriented samples so the
/// editor's WB pick/auto map clicks straight through. Crop is normalized and
/// rotation-invariant, so it passes through unchanged.
pub struct RotatedBase {
    inner: Arc<dyn Base>,
    quarters: u8,
}

impl RotatedBase {
    pub fn new(inner: Arc<dyn Base>, quarters: u8) -> Self {
        Self { inner, quarters: quarters % 4 }
    }
}

impl Base for RotatedBase {
    fn width(&self) -> u32 {
        rotate_dims(self.inner.width(), self.inner.height(), self.quarters).0
    }
    fn height(&self) -> u32 {
        rotate_dims(self.inner.width(), self.inner.height(), self.quarters).1
    }

    fn render_with_ev(
        &self,
        crop: Option<&NormalizedCrop>,
        size: Size,
        adjustments: &Adjustments,
        ev_override: Option<f32>,
    ) -> RenderResult {
        let q = self.quarters;
        if q == 0 {
            return self.inner.render_with_ev(crop, size, adjustments, ev_override);
        }
        let (iw, ih) = rotate_dims(size.width, size.height, (4 - q) & 3);
        let inner = self.inner.render_with_ev(crop, Size { width: iw, height: ih }, adjustments, ev_override);
        let (rgba, _ow, _oh) = rotate_rgba(&inner.rgba, iw, ih, q);
        RenderResult { rgba, auto_ev: inner.auto_ev }
    }

    fn linear_sample(&self, size: Size) -> Vec<f32> {
        let q = self.quarters;
        if q == 0 {
            return self.inner.linear_sample(size);
        }
        let (iw, ih) = rotate_dims(size.width, size.height, (4 - q) & 3);
        let sample = self.inner.linear_sample(Size { width: iw, height: ih });
        rotate_linear3(&sample, iw, ih, q).0
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
    // Median anchor (p50), midtones → histogram center. EV is solved in linear
    // space, so the anchor lands exactly on the target after the tone LUT.
    auto_exposure_ev(
        lums,
        &AutoExpOpts {
            target: if aggressive { 150.0 } else { 128.0 },
            percentile: 0.5,
            max_ev: 6.0,
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

    fn render_with_ev(
        &self,
        crop: Option<&NormalizedCrop>,
        size: Size,
        adjustments: &Adjustments,
        ev_override: Option<f32>,
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
        let auto_ev = ev_override.unwrap_or(auto_ev);
        let ev = effective_ev(adjustments.exposure_mode, adjustments.exposure_ev, auto_ev);

        // Downscale the (cropped) source to the target size in sRGB space.
        let mut down = downscale_rgba(src, rect.width, rect.height, size.width, size.height);

        let lut = jpeg_tone_lut(false); // highlight rolloff applies in Auto AND aggressive; JPEG never gets the RAW S-curve
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

    fn linear_sample(&self, size: Size) -> Vec<f32> {
        // The WB pick/auto sample the raw sensor cast: just the decoded sRGB frame
        // downscaled to linear light, with NO exposure/WB/rolloff/S-curve applied
        // (those tone ops distort the R/B ratio and understate a strong cast).
        let dw = size.width.min(self.width).max(1);
        let dh = size.height.min(self.height).max(1);
        let down = downscale_rgba(&self.rgba, self.width, self.height, dw, dh);
        let s2l = srgb_to_linear();
        let mut out = Vec::with_capacity((dw * dh) as usize * 3);
        for px in down.chunks_exact(4) {
            out.push(s2l[px[0] as usize]);
            out.push(s2l[px[1] as usize]);
            out.push(s2l[px[2] as usize]);
        }
        out
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
            let sr = linear_to_srgb_byte(r[i]);
            let sg = linear_to_srgb_byte(g[i]);
            let sb = linear_to_srgb_byte(b[i]);
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

    fn render_with_ev(
        &self,
        crop: Option<&NormalizedCrop>,
        size: Size,
        adjustments: &Adjustments,
        ev_override: Option<f32>,
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
        let auto_ev = ev_override.unwrap_or(auto_ev);
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
        let lut = raw_tone_lut(false); // rolloff applies in Auto AND aggressive; RAW gets the camera-Standard S-curve
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

    fn linear_sample(&self, size: Size) -> Vec<f32> {
        // Same idea as the JPEG path: the WB pick/auto need the true sensor cast,
        // so sample the decoded LINEAR planes downscaled — no exposure/WB/tone.
        let dw = size.width.min(self.width).max(1);
        let dh = size.height.min(self.height).max(1);
        let r = downscale_plane(&self.full.r, self.full.width, self.full.height, dw, dh);
        let g = downscale_plane(&self.full.g, self.full.width, self.full.height, dw, dh);
        let b = downscale_plane(&self.full.b, self.full.width, self.full.height, dw, dh);
        let n = (dw * dh) as usize;
        let mut out = Vec::with_capacity(n * 3);
        for i in 0..n {
            out.push(r[i]);
            out.push(g[i]);
            out.push(b[i]);
        }
        out
    }
}

/// 256-bin luminance histogram over the final sRGB pixels (photoup `computeHistogram`).
/// 256-bin LUMINANCE histogram (0.2126R + 0.7152G + 0.0722B). Kept for a revert;
/// the editor now shows the RGB histogram by default.
pub fn compute_histogram(rgba: &[u8]) -> Vec<u32> {
    let mut bins = vec![0u32; 256];
    for px in rgba.chunks_exact(4) {
        let l = (0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32) as usize;
        bins[l.min(255)] += 1;
    }
    bins
}

/// 3×256 RGB histogram (R then G then B, 768 bins total) over the final sRGB
/// pixels. The editor draws the three channels overlaid.
pub fn compute_histogram_rgb(rgba: &[u8]) -> Vec<u32> {
    let mut bins = vec![0u32; 768];
    for px in rgba.chunks_exact(4) {
        bins[px[0] as usize] += 1;
        bins[256 + px[1] as usize] += 1;
        bins[512 + px[2] as usize] += 1;
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
    use crate::image::srgb::{highlight_rolloff, linear_to_srgb_byte, srgb_to_linear};

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
        // Sampling the bright half → auto-EV is negative (already bright). The
        // negative sign is the proof the EV was computed from the crop, not the
        // full frame (a full-frame sample would see the dark half and go positive).
        assert!(
            out.auto_ev < 0.0,
            "auto_ev {} should be negative for a bright crop",
            out.auto_ev
        );
        // Median anchor centers the crop's midtones on the auto target (~128).
        assert!(
            out.rgba[0] > 100 && out.rgba[0] < 160,
            "crop render should center midtones on the auto target, got {}",
            out.rgba[0]
        );
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

    #[test]
    fn jpeg_linear_sample_is_linear_pre_tone() {
        // A uniform warm sRGB (200,128,80) frame → the linear sample must equal
        // srgb_to_linear(byte) per channel (downscale of a uniform image is uniform),
        // and carry NO exposure/WB/tone. Downscaling to 8×8 exercises the box filter.
        let (w, h) = (32u32, 32u32);
        let rgba = vec![200u8, 128, 80, 255].repeat((w * h) as usize);
        let base = JpegBase::new(w, h, rgba);
        let out = base.linear_sample(Size { width: 8, height: 8 });
        assert_eq!(out.len(), (8 * 8 * 3) as usize);
        let s2l = srgb_to_linear();
        for px in out.chunks_exact(3) {
            assert!((px[0] - s2l[200]).abs() < 1e-6, "r {}", px[0]);
            assert!((px[1] - s2l[128]).abs() < 1e-6, "g {}", px[1]);
            assert!((px[2] - s2l[80]).abs() < 1e-6, "b {}", px[2]);
        }
        // The sample is < 1 (0..1 linear), unlike the tone-processed preview which
        // can lift highlights past the sensor values.
        assert!(out.iter().all(|&v| v >= 0.0 && v <= 1.0));
    }
}

#[cfg(test)]
mod rotation_tests {
    use super::*;

    /// A 3-wide × 2-tall RGBA marker buffer: cell (x,y) holds `(y*w + x + 1)` in R.
    /// Row0 = 1,2,3 (top), row1 = 4,5,6 (bottom).
    fn marker_rgba(w: u32, h: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let v = (y * w + x + 1) as u8;
                buf.extend_from_slice(&[v, 0, 0, 255]);
            }
        }
        buf
    }

    fn marker_at(rgba: &[u8], w: u32, x: u32, y: u32) -> u8 {
        rgba[((y * w + x) * 4) as usize]
    }

    #[test]
    fn rotate_90cw_maps_cells_correctly() {
        let buf = marker_rgba(3, 2);
        let (out, ow, oh) = rotate_rgba(&buf, 3, 2, 1);
        assert_eq!((ow, oh), (2, 3), "90°CW swaps dims");
        // 90°CW: display(ox,oy) = base(oy, h-1-ox), h=2. Base columns become
        // display rows, reversed: left col (1,4) → top row [4,1], mid (2,5) → [5,2],
        // right col (3,6) → bottom row [6,3].
        assert_eq!(marker_at(&out, 2, 0, 0), 4, "top-left ← base(0,1)=4");
        assert_eq!(marker_at(&out, 2, 1, 0), 1, "top-right ← base(0,0)=1");
        assert_eq!(marker_at(&out, 2, 0, 1), 5, "mid-left ← base(1,1)=5");
        assert_eq!(marker_at(&out, 2, 1, 1), 2, "mid-right ← base(1,0)=2");
        assert_eq!(marker_at(&out, 2, 0, 2), 6, "bottom-left ← base(2,1)=6");
        assert_eq!(marker_at(&out, 2, 1, 2), 3, "bottom-right ← base(2,0)=3");
    }

    #[test]
    fn rotate_180_and_270() {
        let buf = marker_rgba(3, 2);
        let (out, ow, oh) = rotate_rgba(&buf, 3, 2, 2);
        assert_eq!((ow, oh), (3, 2));
        assert_eq!(marker_at(&out, 3, 2, 1), 1, "180° puts base(0,0) at bottom-right");
        assert_eq!(marker_at(&out, 3, 0, 0), 6, "180° puts base(2,1) at top-left");

        let (out3, ow3, oh3) = rotate_rgba(&buf, 3, 2, 3);
        assert_eq!((ow3, oh3), (2, 3));
        // 270°CW (90°CCW): display(ox,oy) = base(w-1-oy, ox). Top row of base
        // (1 2 3) → left column reversed [3,2,1]; bottom row (4 5 6) → right [6,5,4].
        assert_eq!(marker_at(&out3, 2, 0, 0), 3, "top-left ← base(2,0)=3");
        assert_eq!(marker_at(&out3, 2, 1, 0), 6, "top-right ← base(2,1)=6");
        assert_eq!(marker_at(&out3, 2, 0, 2), 1, "bottom-left ← base(0,0)=1");
        assert_eq!(marker_at(&out3, 2, 1, 2), 4, "bottom-right ← base(0,1)=4");
    }

    #[test]
    fn rotate_zero_passes_through_unchanged() {
        let buf = marker_rgba(3, 2);
        let (out, ow, oh) = rotate_rgba(&buf, 3, 2, 0);
        assert_eq!((ow, oh), (3, 2));
        assert_eq!(out, buf);
    }

    /// End-to-end through `RotatedBase`: dims swap and `linear_sample` comes back
    /// display-oriented (raw pre-tone values, so exact). Base 3×2 with markers;
    /// after 90°CW the display is 2×3 and the top row holds base's left column.
    #[test]
    fn rotated_base_swaps_dims_and_sample_orientation() {
        let base = JpegBase::new(3, 2, marker_rgba(3, 2));
        let wrapped = RotatedBase::new(Arc::new(base), 1);
        assert_eq!((wrapped.width(), wrapped.height()), (2, 3));
        let sample = wrapped.linear_sample(Size { width: 2, height: 3 });
        let s2l = srgb_to_linear();
        // sample[y*w+x*3] == s2l[marker]. Top-left display = base(0,1) = 4.
        let at = |x: u32, y: u32| sample[((y * 2 + x) * 3) as usize];
        assert!((at(0, 0) - s2l[4]).abs() < 1e-6, "top-left display ← base(0,1)");
        assert!((at(1, 0) - s2l[1]).abs() < 1e-6, "top-right display ← base(0,0)");
        assert!((at(1, 2) - s2l[3]).abs() < 1e-6, "bottom-right display ← base(2,0)");
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

    #[test]
    fn raw_linear_sample_is_raw_plane_values() {
        // A uniform warm RAW plane (r=0.578, g=0.216, b=0.080 linear) → the linear
        // sample returns those plane values unchanged (pre-exposure/WB/tone), so the
        // WB pick/auto measure the true sensor cast.
        let (w, h) = (32u32, 32u32);
        let n = (w * h) as usize;
        let dr = DecodedRaw {
            width: w,
            height: h,
            r: vec![0.578; n],
            g: vec![0.216; n],
            b: vec![0.080; n],
            cam_mul: None,
            cam_matrix: None,
        };
        let base = RawBase::new(dr);
        let out = base.linear_sample(Size { width: 8, height: 8 });
        assert_eq!(out.len(), (8 * 8 * 3) as usize);
        for px in out.chunks_exact(3) {
            assert!((px[0] - 0.578).abs() < 1e-6, "r {}", px[0]);
            assert!((px[1] - 0.216).abs() < 1e-6, "g {}", px[1]);
            assert!((px[2] - 0.080).abs() < 1e-6, "b {}", px[2]);
        }
    }

    fn uniform_dr(w: u32, h: u32, value: f32) -> DecodedRaw {
        let n = (w * h) as usize;
        DecodedRaw {
            width: w,
            height: h,
            r: vec![value; n],
            g: vec![value; n],
            b: vec![value; n],
            cam_mul: None,
            cam_matrix: None,
        }
    }

    fn max_byte_diff(a: &[u8], b: &[u8]) -> i32 {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| (*x as i32 - *y as i32).abs())
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn raw_matrix_matches_per_channel_for_identity() {
        // Identity camera matrix → T = M·diag(wb)·M⁻¹ = diag(wb), so the matrix path
        // must produce the same result as per-channel gains (within 1 ulp of float order).
        let eye = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]];
        let mut dr = uniform_dr(32, 32, 0.5);
        dr.cam_matrix = Some(eye);
        let with = RawBase::new(dr);
        let without = RawBase::new(uniform_dr(32, 32, 0.5));
        let mut adj = Adjustments::default();
        adj.wb_offset = 0.5;
        adj.hue = -0.2;
        let a = with.render(None, Size { width: 16, height: 16 }, &adj);
        let b = without.render(None, Size { width: 16, height: 16 }, &adj);
        let d = max_byte_diff(&a.rgba, &b.rgba);
        assert!(d <= 1, "identity matrix must match per-channel WB, max diff {d}");
    }

    #[test]
    fn raw_matrix_mixes_channels_unlike_per_channel() {
        // A camera-like matrix with cross-channel terms + non-neutral WB must produce
        // a DIFFERENT result than per-channel gains (which keep gray → gray). This
        // proves the matrix branch actually mixes channels.
        let m = [[1.0, 0.2, 0.1, 0.0], [0.05, 1.0, 0.05, 0.0], [0.1, 0.2, 1.0, 0.0]];
        let mut dr = uniform_dr(16, 16, 0.5);
        dr.cam_matrix = Some(m);
        let with = RawBase::new(dr);
        let without = RawBase::new(uniform_dr(16, 16, 0.5));
        let mut adj = Adjustments::default();
        adj.wb_offset = 0.5;
        adj.hue = 0.2;
        let a = with.render(None, Size { width: 8, height: 8 }, &adj);
        let b = without.render(None, Size { width: 8, height: 8 }, &adj);
        let d = max_byte_diff(&a.rgba, &b.rgba);
        assert!(d > 1, "cross-channel matrix must mix WB differently than per-channel, max diff {d}");
    }
}
