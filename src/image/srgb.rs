use crate::image::math::clamp;
use crate::image::types::Adjustments;

/// sRGB 8-bit byte → linear (0..1). Index = byte.
pub fn srgb_to_linear() -> &'static [f32; 256] {
    static LUT: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        let mut lut = [0.0f32; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            let c = i as f32 / 255.0;
            *v = if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            };
        }
        lut
    })
}

/// 16-bit sRGB → linear. Index = u16. (8-bit values are scaled by 257.)
pub fn srgb16_to_linear() -> &'static [f32; 65536] {
    static LUT: std::sync::OnceLock<[f32; 65536]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        let s = srgb_to_linear();
        let mut lut = [0.0f32; 65536];
        for (i, v) in lut.iter_mut().enumerate() {
            *v = s[((i / 257) % 256) as usize];
        }
        lut
    })
}

pub fn linear_to_srgb_byte(v: f32) -> u8 {
    let c = clamp(v, 0.0, 1.0);
    let out = if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (clamp(out, 0.0, 1.0) * 255.0).round() as u8
}

/// Mild highlight rolloff in linear space — knee near 1.0, short transition, then hard clip.
pub fn highlight_rolloff(x: f32) -> f32 {
    const KNEE: f32 = 0.9;
    const SOFTNESS: f32 = 0.12;
    if x <= KNEE {
        return x;
    }
    let t = (x - KNEE) / SOFTNESS;
    KNEE + SOFTNESS * (1.0 - (-t).exp())
}

/// Camera "Standard"-style tone curve: mid-tone contrast + slight shadow lift, in sRGB.
const CAMERA_CONTRAST: f32 = 1.4;
const CAMERA_SHADOW_LIFT: f32 = 0.04;

pub fn camera_curve_byte(v: u8) -> u8 {
    let x = clamp(v as f32 / 255.0, 0.0, 1.0);
    let p = x.powf(CAMERA_CONTRAST);
    let mut y = p / (p + (1.0 - x).powf(CAMERA_CONTRAST));
    y = y * (1.0 - CAMERA_SHADOW_LIFT) + CAMERA_SHADOW_LIFT;
    (clamp(y, 0.0, 1.0) * 255.0).round() as u8
}

/// Precomputed linear→sRGB-byte LUT folding in rolloff (and optionally the camera curve).
/// Index = linear * 32767.5 covering linear [0, 2]; larger values clip to white.
pub fn build_tone_lut(apply_camera_curve: bool, hard_clip: bool) -> Vec<u8> {
    let mut lut = Vec::with_capacity(65536);
    for i in 0..65536u32 {
        let x = (i as f32 / 65535.0) * 2.0;
        let b = if hard_clip {
            linear_to_srgb_byte(x)
        } else {
            linear_to_srgb_byte(highlight_rolloff(x))
        };
        lut.push(if apply_camera_curve {
            camera_curve_byte(b)
        } else {
            b
        });
    }
    lut
}

pub fn tone_index(v: f32) -> usize {
    clamp(v * 32767.5, 0.0, 65535.0) as usize
}

pub static JPEG_TONE_LUT: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
pub static RAW_TONE_LUT: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
pub static JPEG_HARD_LUT: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
pub static RAW_HARD_LUT: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

/// JPEG tone LUT (no camera S-curve). `hard_clip` = aggressive auto (no rolloff).
pub fn jpeg_tone_lut(hard_clip: bool) -> &'static [u8] {
    if hard_clip {
        JPEG_HARD_LUT.get_or_init(|| build_tone_lut(false, true))
    } else {
        JPEG_TONE_LUT.get_or_init(|| build_tone_lut(false, false))
    }
}

/// RAW tone LUT (with the camera-Standard S-curve). `hard_clip` = aggressive auto (no rolloff).
pub fn raw_tone_lut(hard_clip: bool) -> &'static [u8] {
    if hard_clip {
        RAW_HARD_LUT.get_or_init(|| build_tone_lut(true, true))
    } else {
        RAW_TONE_LUT.get_or_init(|| build_tone_lut(true, false))
    }
}

/// Warmth offset + hue as per-channel WB gains (exposure is separate).
/// Port of photoup `wbGains`.
pub fn wb_gains(wb_offset: f32, hue: f32) -> (f32, f32, f32) {
    let temp_r = 2.0f32.powf(wb_offset * 0.5);
    let temp_b = 2.0f32.powf(-wb_offset * 0.5);
    let hue_g = 2.0f32.powf(-hue * 0.5);
    let hue_rb = 2.0f32.powf(hue * 0.25);
    (temp_r * hue_rb, hue_g, temp_b * hue_rb)
}

/// Combined exposure gain + WB, per channel.
pub fn gain_coefficients(ev: f32, wb_offset: f32, hue: f32) -> (f32, f32, f32) {
    let gain = 2.0f32.powf(ev);
    let (wr, wg, wb) = wb_gains(wb_offset, hue);
    (gain * wr, gain * wg, gain * wb)
}

/// Invert a 3x3 matrix (port of photoup `invert3x3`).
pub fn invert3x3(m: &[[f32; 3]; 3]) -> Option<[[f32; 3]; 3]> {
    let [a0, a1, a2] = m[0];
    let [b0, b1, b2] = m[1];
    let [c0, c1, c2] = m[2];
    let c00 = b1 * c2 - b2 * c1;
    let c01 = b2 * c0 - b0 * c2;
    let c02 = b0 * c1 - b1 * c0;
    let c10 = a2 * c1 - a1 * c2;
    let c11 = a0 * c2 - a2 * c0;
    let c12 = a1 * c0 - a0 * c1;
    let c20 = a1 * b2 - a2 * b1;
    let c21 = a2 * b0 - a0 * b2;
    let c22 = a0 * b1 - a1 * b0;
    let det = a0 * c00 + b0 * c10 + c0 * c20;
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    Some([
        [inv * c00, inv * c10, inv * c20],
        [inv * c01, inv * c11, inv * c21],
        [inv * c02, inv * c12, inv * c22],
    ])
}

/// Preview WB transform that reproduces the pre-matrix userMul export:
/// T = M · diag(wb) · M⁻¹. Returns row-major 3x3, or None.
pub fn wb_transform3x3(m: &[[f32; 3]; 3], wb: (f32, f32, f32)) -> Option<[f32; 9]> {
    let minv = invert3x3(m)?;
    // M·diag(wb) — scale columns of M.
    let a = [
        m[0][0] * wb.0,
        m[0][1] * wb.1,
        m[0][2] * wb.2,
        m[1][0] * wb.0,
        m[1][1] * wb.1,
        m[1][2] * wb.2,
        m[2][0] * wb.0,
        m[2][1] * wb.1,
        m[2][2] * wb.2,
    ];
    let mut t = [0.0f32; 9];
    for i in 0..3 {
        for j in 0..3 {
            t[i * 3 + j] =
                a[i * 3] * minv[0][j] + a[i * 3 + 1] * minv[1][j] + a[i * 3 + 2] * minv[2][j];
        }
    }
    Some(t)
}

/// Effective WB multipliers for the final export (baked into libraw `user_mul`).
/// Port of photoup `exportWbMul`.
pub fn export_wb_mul(cam_mul: [f32; 4], adjustments: &Adjustments) -> [f32; 4] {
    let rc = cam_mul[0].max(1e-6);
    let gc = cam_mul[1].max(1e-6);
    let bc = cam_mul[2].max(1e-6);
    let g2c = cam_mul[3].max(1e-6);
    let temp_r = 2.0f32.powf(adjustments.wb_offset * 0.5);
    let temp_b = 2.0f32.powf(-adjustments.wb_offset * 0.5);
    let hue_g = 2.0f32.powf(-adjustments.hue * 0.5);
    let hue_rb = 2.0f32.powf(adjustments.hue * 0.25);
    let r = rc * temp_r * hue_rb;
    let g = gc * hue_g;
    let b = bc * temp_b * hue_rb;
    let g2 = g2c * hue_g;
    [r / g, 1.0, b / g, g2 / g]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_roundtrip_midgray() {
        // 0.5 sRGB → linear ≈ 0.216, and back → 128.
        let lin = srgb_to_linear()[128];
        assert!((lin - 0.2140).abs() < 0.002, "lin {lin}");
        assert_eq!(linear_to_srgb_byte(lin), 128);
    }

    #[test]
    fn rolloff_identity_below_knee() {
        assert_eq!(highlight_rolloff(0.5), 0.5);
        assert!(highlight_rolloff(1.0) < 1.0); // softens near top
    }

    #[test]
    fn tone_lut_saturates() {
        let lut = build_tone_lut(false, false);
        assert_eq!(lut[65535], 255); // x=2 clips to white
        assert_eq!(lut[0], 0);
    }

    #[test]
    fn wb_gains_neutral() {
        let (r, g, b) = wb_gains(0.0, 0.0);
        assert!((r - 1.0).abs() < 1e-6 && (g - 1.0).abs() < 1e-6 && (b - 1.0).abs() < 1e-6);
    }

    #[test]
    fn export_wb_mul_normalizes_by_g() {
        let out = export_wb_mul([2.0, 1.0, 1.5, 1.0], &Adjustments::default());
        assert_eq!(out[1], 1.0);
        assert!((out[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn invert3x3_roundtrips_to_identity() {
        let m = [[0.7, 0.2, 0.1], [0.1, 0.8, 0.1], [0.05, 0.1, 0.85]];
        let inv = invert3x3(&m).expect("invertible");
        // M * M⁻¹ ≈ I
        for i in 0..3 {
            for j in 0..3 {
                let mut dot = 0.0;
                for k in 0..3 {
                    dot += m[i][k] * inv[k][j];
                }
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!((dot - expected).abs() < 1e-5, "M·M⁻¹[{i}][{j}]={dot}");
            }
        }
    }

    #[test]
    fn wb_transform_identity_matrix_is_diagonal_gains() {
        // Identity camera matrix → T = diag(wb), i.e. per-channel gains only.
        let eye = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        let wb = (2.0, 1.0, 1.5);
        let t = wb_transform3x3(&eye, wb).expect("transform");
        // row-major 3x3: [t00,t01,t02, t10,t11,t12, t20,t21,t22]
        assert!(
            (t[0] - 2.0).abs() < 1e-6 && (t[4] - 1.0).abs() < 1e-6 && (t[8] - 1.5).abs() < 1e-6
        );
        assert_eq!(t[1], 0.0); // off-diagonals zero
        assert_eq!(t[3], 0.0);
        assert_eq!(t[5], 0.0);
        assert_eq!(t[6], 0.0);
        assert_eq!(t[7], 0.0);
    }
}
