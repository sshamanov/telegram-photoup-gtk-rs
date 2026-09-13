use crate::errors::{Error, Result};
use crate::image::rawffi::Raw;
use crate::image::types::{DecodedRaw, Size};
use image::ImageDecoder;

/// Decode JPEG/PNG to sRGB RGBA8 at full resolution (JPEG base), applying the
/// EXIF Orientation tag so the pixels match what the camera captured (portrait
/// shots from phones/DSLRs land upright). `load_from_memory` ignores orientation,
/// so we must use `ImageReader` + `into_decoder().orientation()`.
/// `image` crate output is already sRGB; photoup keeps JPEGs in sRGB space and
/// applies exposure/WB/rolloff at render time.
pub fn decode_jpeg(data: &[u8]) -> Result<(Size, Vec<u8>)> {
    let reader = image::ImageReader::new(std::io::Cursor::new(data));
    let mut decoder = reader
        .with_guessed_format()
        .map_err(|e| Error::Image(e.to_string()))?
        .into_decoder()
        .map_err(|e| Error::Image(e.to_string()))?;
    let orientation = decoder
        .orientation()
        .map_err(|e| Error::Image(e.to_string()))?;
    let mut img = image::DynamicImage::from_decoder(decoder)
        .map_err(|e| Error::Image(e.to_string()))?;
    img.apply_orientation(orientation);
    let rgba = img.into_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let raw = rgba.into_raw();
    Ok((
        Size {
            width: w,
            height: h,
        },
        raw,
    ))
}

/// Decode a camera RAW (NEF/CR2) to LINEAR float RGB (camera WB, sRGB primaries).
/// Mirrors photoup `decodeRaw` options exactly. `half_size` keeps interactive
/// decodes small; exports pass `false` and get full resolution.
pub fn decode_raw(data: &[u8], opts: &RawDecodeOpts) -> Result<DecodedRaw> {
    let mut raw = Raw::new()?;
    {
        let p = raw.params();
        // A custom WB (user_mul) overrides camera WB and is applied pre-matrix.
        match opts.user_mul {
            Some(mul) => {
                p.use_camera_wb = 0;
                p.user_mul = mul;
            }
            None => {
                p.use_camera_wb = 1;
            }
        }
        p.use_camera_matrix = 1;
        p.output_color = 1; // sRGB primaries + gamma
        p.output_bps = 16;
        p.no_auto_bright = 1;
        p.half_size = if opts.full_size { 0 } else { 1 };
        p.user_qual = 3;
    }
    raw.open_buffer(data)?;
    raw.unpack()?;
    raw.process()?;
    let mut decoded = raw.make_mem_image()?;
    // Interactive RAW development intentionally uses LibRaw's half-size buffer.
    // Its pixels are exactly 1/2 width and height of the developed export, but
    // the editor's source/output labels and Pix crop geometry must describe the
    // full-resolution result. Keep the two coordinate spaces explicit here.
    decoded.developed_size = if opts.full_size {
        Size {
            width: decoded.width,
            height: decoded.height,
        }
    } else {
        Size {
            width: decoded.width * 2,
            height: decoded.height * 2,
        }
    };
    Ok(decoded)
}

pub struct RawDecodeOpts {
    pub full_size: bool,
    pub user_mul: Option<[f32; 4]>,
}

#[cfg(test)]
mod raw_tests {
    use super::*;

    /// Decode a sample NEF/CR2 if one exists locally. Sample photos are never committed.
    fn sample() -> Option<std::path::PathBuf> {
        for dir in ["samples", "../photoup/samples"] {
            let d = std::path::Path::new(dir);
            if let Ok(rd) = std::fs::read_dir(d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if matches!(
                        p.extension().and_then(|s| s.to_str()),
                        Some("NEF") | Some("nef") | Some("CR2") | Some("cr2")
                    ) {
                        return Some(p);
                    }
                }
            }
        }
        None
    }

    #[test]
    fn decodes_real_raw_if_sample_present() {
        let Some(path) = sample() else {
            eprintln!("skipping: no NEF/CR2 sample found");
            return;
        };
        let data = std::fs::read(&path).expect("read sample");
        let decoded = decode_raw(
            &data,
            &RawDecodeOpts {
                full_size: false,
                user_mul: None,
            },
        )
        .expect("decode raw");
        assert!(decoded.width > 0 && decoded.height > 0);
        assert_eq!(decoded.r.len(), (decoded.width * decoded.height) as usize);
        assert!(decoded.r.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
        assert!(decoded.g.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
        assert!(decoded.b.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
        assert!(decoded.cam_mul.is_some() && decoded.cam_matrix.is_some());
        eprintln!(
            "decoded {}x{} from {}",
            decoded.width,
            decoded.height,
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;

    /// TEMPORARY debug aid: reproduce the editor's WB pick/auto on every real RAW
    /// sample and print the linear-sample stats that feed the math, so we can see
    /// the actual cast the picker measures (issue: pick/auto under-report warm casts).
    #[test]
    fn wb_debug_print_sample_casts() {
        use crate::image::math::fit_within;
        use crate::image::process::{Base, RawBase};
        use crate::image::srgb::{auto_wb, wb_from_pick};

        let mut files = Vec::new();
        for dir in ["samples", "../photoup/samples"] {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if matches!(
                        p.extension().and_then(|s| s.to_str()),
                        Some("NEF") | Some("nef") | Some("CR2") | Some("cr2")
                    ) {
                        files.push(p);
                    }
                }
            }
        }
        files.sort();
        println!("[wb-debug] {} RAW sample(s)", files.len());
        for path in &files {
            let Ok(data) = std::fs::read(path) else { continue };
            let Ok(decoded) =
                decode_raw(&data, &RawDecodeOpts { full_size: false, user_mul: None })
            else {
                println!("=== {} decode failed", path.display());
                continue;
            };
            let cam3 = decoded.cam_matrix.map(|m| [
                [m[0][0] as f32, m[0][1] as f32, m[0][2] as f32],
                [m[1][0] as f32, m[1][1] as f32, m[1][2] as f32],
                [m[2][0] as f32, m[2][1] as f32, m[2][2] as f32],
            ]);
            let base = RawBase::new(decoded);
            let (w, h) = fit_within(base.width(), base.height(), 96);
            let sample = base.linear_sample(Size { width: w, height: h });
            let np = ((w * h) as usize).max(1);

            // Full-sample min/max/mean per channel.
            let (mut mn, mut mx, mut sm) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3], [0.0f64; 3]);
            // Neutral-filtered mean (the auto path's reference) + per-pixel pick sweep.
            let (mut fsr, mut fsg, mut fsb) = (0.0f64, 0.0f64, 0.0f64);
            let mut fnn = 0u32;
            let (mut p_off, mut p_hue) = (Vec::with_capacity(np), Vec::with_capacity(np));
            let (mut nc, mut c_lo, mut c_hi, mut hc) = (0u32, 0u32, 0u32, 0u32);
            for px in sample.chunks_exact(3) {
                let (r, g, b) = (px[0], px[1], px[2]);
                for c in 0..3 {
                    mn[c] = mn[c].min(px[c]);
                    mx[c] = mx[c].max(px[c]);
                    sm[c] += px[c] as f64;
                }
                if r.max(g).max(b) - r.min(g).min(b) < 0.2 && 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.1 {
                    fsr += r as f64;
                    fsg += g as f64;
                    fsb += b as f64;
                    fnn += 1;
                }
                let (o, hh) = wb_from_pick(r, g, b, cam3);
                if o <= -3.99 { c_lo += 1; }
                if o >= 3.99 { c_hi += 1; }
                if hh.abs() >= 0.99 { hc += 1; }
                nc += 1;
                p_off.push(o);
                p_hue.push(hh);
            }
            let m = |c: usize| sm[c] / np as f64;
            println!("=== {}", path.display());
            println!(
                "  linear sample {}x{}: mean r={:.4} g={:.4} b={:.4} | min r={:.4} g={:.4} b={:.4} | max r={:.4} g={:.4} b={:.4}",
                w, h, m(0), m(1), m(2), mn[0], mn[1], mn[2], mx[0], mx[1], mx[2]
            );
            println!(
                "  ratios: mean r/g={:.3} b/g={:.3} | neutral-ish pixels {} of {} ({:.1}%)",
                m(0) / m(1).max(1e-6), m(2) / m(1).max(1e-6), fnn, np, 100.0 * fnn as f64 / np as f64
            );
            let (fdr, fdg, fdb) = (fsr / fnn.max(1) as f64, fsg / fnn.max(1) as f64, fsb / fnn.max(1) as f64);
            let (fmr, fmg, fmb) = (sm[0] / np as f64, sm[1] / np as f64, sm[2] / np as f64);
            let (a1, b1) = wb_from_pick(fmr as f32, fmg as f32, fmb as f32, cam3);
            let (a2, b2) = wb_from_pick(fdr as f32, fdg as f32, fdb as f32, cam3);
            let (a1g, b1g) = wb_from_pick(fmr as f32, fmg as f32, fmb as f32, None);
            let (a2g, b2g) = wb_from_pick(fdr as f32, fdg as f32, fdb as f32, None);
            let (aa1, ab1) = auto_wb(fmr as f32, fmg as f32, fmb as f32, cam3);
            let (aa2, ab2) = auto_wb(fdr as f32, fdg as f32, fdb as f32, cam3);
            println!(
                "  pick full mean: matrix={:.3}/{:.3} grey-world={:.3}/{:.3} | neutral mean(n={}): matrix={:.3}/{:.3} grey-world={:.3}/{:.3}",
                a1, b1, a1g, b1g, fnn, a2, b2, a2g, b2g
            );
            println!(
                "  auto_wb(full mean)={:.3}/{:.3} | auto_wb(neutral mean)={:.3}/{:.3}",
                aa1, ab1, aa2, ab2
            );
            if let Some(c) = cam3 {
                println!("  cam_matrix: {:?}", c);
            }
            p_off.sort_by(|a, b| a.total_cmp(b));
            p_hue.sort_by(|a, b| a.total_cmp(b));
            let q = |v: &[f32], t: f32| v[((v.len() - 1) as f32 * t) as usize];
            println!(
                "  per-pixel pick offset: p10={:.3} p50={:.3} p90={:.3} | clamps off<-4:{} off>+4:{} hue±1:{} ({} px)",
                q(&p_off, 0.10), q(&p_off, 0.50), q(&p_off, 0.90), c_lo, c_hi, hc, nc
            );
        }
    }

    /// TEMPORARY: render every RAW sample as-shot vs old-pick vs new-pick WB to
    /// PNGs the user can visually verify (the pick's warmth was under-stated ~2×).
    #[test]
    fn wb_debug_render_before_after() {
        use crate::image::math::{clamp, fit_within};
        use crate::image::process::{Base, RawBase};
        use crate::image::srgb::wb_from_pick;
        use crate::image::types::Adjustments;

        let mut files = Vec::new();
        for dir in ["samples", "../photoup/samples"] {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if matches!(
                        p.extension().and_then(|s| s.to_str()),
                        Some("NEF") | Some("nef") | Some("CR2") | Some("cr2")
                    ) {
                        files.push(p);
                    }
                }
            }
        }
        files.sort();
        let outdir = std::path::Path::new("out").join("wb_before_after");
        std::fs::create_dir_all(&outdir).ok();
        for path in files {
            let Ok(data) = std::fs::read(&path) else { continue };
            let Ok(decoded) =
                decode_raw(&data, &RawDecodeOpts { full_size: false, user_mul: None })
            else {
                continue;
            };
            let cam3 = decoded.cam_matrix.map(|m| [
                [m[0][0] as f32, m[0][1] as f32, m[0][2] as f32],
                [m[1][0] as f32, m[1][1] as f32, m[1][2] as f32],
                [m[2][0] as f32, m[2][1] as f32, m[2][2] as f32],
            ]);
            let base = RawBase::new(decoded);
            let (w, h) = fit_within(base.width(), base.height(), 48);
            let sample = base.linear_sample(Size { width: w, height: h });
            let n = ((w * h) as usize).max(1);
            let mut sm = [0.0f64; 3];
            for px in sample.chunks_exact(3) {
                for c in 0..3 {
                    sm[c] += px[c] as f64;
                }
            }
            let (mr, mg, mb) = (sm[0] / n as f64, sm[1] / n as f64, sm[2] / n as f64);

            // New pick (the fix) vs the previous mapping (which dropped gb).
            let (new_off, new_hue) = wb_from_pick(mr as f32, mg as f32, mb as f32, cam3);
            let (gr, gg, _gb) = match cam3.and_then(|m| crate::image::srgb::invert3x3(&m)) {
                Some(minv) => {
                    let q0 = minv[0][0] * mr as f32 + minv[0][1] * mg as f32 + minv[0][2] * mb as f32;
                    let q1 = minv[1][0] * mr as f32 + minv[1][1] * mg as f32 + minv[1][2] * mb as f32;
                    let q2 = minv[2][0] * mr as f32 + minv[2][1] * mg as f32 + minv[2][2] * mb as f32;
                    let s0 = minv[0][0] + minv[0][1] + minv[0][2];
                    let s1 = minv[1][0] + minv[1][1] + minv[1][2];
                    let s2 = minv[2][0] + minv[2][1] + minv[2][2];
                    let gray = (mr as f32 + mg as f32 + mb as f32) / 3.0;
                    (gray * s0 / q0.max(1e-6), gray * s1 / q1.max(1e-6), gray * s2 / q2.max(1e-6))
                }
                None => {
                    let gray = (mr as f32 + mg as f32 + mb as f32) / 3.0;
                    (gray / mr as f32, gray / mg as f32, gray / mb as f32)
                }
            };
            let old_hue = clamp(-2.0 * gg.max(1e-6).log2(), -1.0, 1.0);
            let old_rb = 2.0f32.powf(old_hue * 0.25);
            let old_off = clamp(2.0 * (gr / old_rb).log2(), -4.0, 4.0);
            println!("old={old_off:.3}/{old_hue:.3} new={new_off:.3}/{new_hue:.3} ({})", path.display());

            let (rw, rh) = fit_within(base.width(), base.height(), 480);
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("photo");
            for (tag, off, hue) in [
                ("as-shot", 0.0, 0.0),
                ("old-pick", old_off, old_hue),
                ("new-pick", new_off, new_hue),
                // Auto2 (warm, Nikon AUTO2 style): 60% of the pick + 0.15 warm bias.
                ("auto2-warm", 0.6 * new_off + 0.15, 0.3 * new_hue),
            ] {
                let adj = Adjustments {
                    exposure_mode: crate::image::types::ExposureMode::Manual,
                    exposure_ev: 0.0,
                    wb_offset: off,
                    hue,
                    crop: None,
                    rotation: 0,
                };
                let r = base.render_with_ev(None, Size { width: rw, height: rh }, &adj, None);
                let fname = outdir.join(format!("{stem}-{tag}.png"));
                if let Ok(f) = std::fs::File::create(&fname) {
                    let mut w = std::io::BufWriter::new(f);
                    image::codecs::png::PngEncoder::new(&mut w)
                        .write_image(&r.rgba, rw, rh, image::ExtendedColorType::Rgba8)
                        .expect("write png");
                }
                println!("  wrote {}", fname.display());
            }
        }
    }

    /// Diagnostic: render every sample in Manual0 / Auto / Burn at preview
    /// (1024) and export (2560) edge, print rendered-luminance percentiles and
    /// clip %, and the preview-vs-export tone diff at matched scale. Dumps the
    /// 1024 renders to out/ev_curves/ for visual (VLM) inspection. Run with
    /// `PHOTOUP2_DEBUG_CURVES=1 cargo test --lib ev_debug_curves -- --nocapture`.
    #[test]
    fn ev_debug_curves() {
        // Opt-in: this decodes EVERY sample and renders 3 modes at 1024+2560,
        // which takes ~15 min in the slow debug-mode JPEG decoder over the dev
        // instance's 19 samples — it would hang a plain `cargo test`.
        if std::env::var_os("PHOTOUP2_DEBUG_CURVES").is_none() {
            eprintln!("skipping ev_debug_curves — set PHOTOUP2_DEBUG_CURVES=1 to render curves over ./samples");
            return;
        }
        use crate::image::math::fit_within;
        use crate::image::process::{Base, JpegBase, RawBase};
        use crate::image::resize::downscale_rgba;
        use crate::image::types::{Adjustments, ExposureMode, Size};

        // (p50, p60, p90, p99, mean, clip%) over rendered luminance.
        fn lums_stats(rgba: &[u8]) -> (f32, f32, f32, f32, f32, f32) {
            let mut lums: Vec<u8> = rgba
                .chunks_exact(4)
                .map(|p| {
                    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32)
                        .round() as u8
                })
                .collect();
            lums.sort_unstable();
            let n = lums.len();
            let pct = |q: f32| lums[((n as f32 * q) as usize).min(n - 1)] as f32;
            let mean = lums.iter().map(|&v| v as f32).sum::<f32>() / n as f32;
            let clipped = lums.iter().filter(|&&v| v > 250).count();
            (
                pct(0.5),
                pct(0.6),
                pct(0.9),
                pct(0.99),
                mean,
                clipped as f32 / n as f32 * 100.0,
            )
        }

        let mut files = Vec::new();
        for dir in ["samples", "../photoup/samples"] {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                        if matches!(ext, "JPG" | "jpg" | "jpeg" | "NEF" | "nef" | "CR2" | "cr2") {
                            files.push(p);
                        }
                    }
                }
            }
        }
        files.sort();
        let outdir = std::path::Path::new("out").join("ev_curves");
        std::fs::create_dir_all(&outdir).ok();

        for path in &files {
            let Ok(data) = std::fs::read(path) else { continue };
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("photo")
                .to_string();
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            let is_raw = matches!(ext.as_str(), "nef" | "cr2");
            let base: std::sync::Arc<dyn Base> = if is_raw {
                let Ok(decoded) =
                    decode_raw(&data, &RawDecodeOpts { full_size: false, user_mul: None })
                else {
                    continue;
                };
                std::sync::Arc::new(RawBase::new(decoded))
            } else {
                let Ok((size, rgba)) = decode_jpeg(&data) else { continue };
                std::sync::Arc::new(JpegBase::new(size.width, size.height, rgba))
            };
            println!(
                "=== {stem} ({ext}) {}x{} ===",
                base.width(),
                base.height()
            );
            for (label, mode) in [
                ("Manual0", ExposureMode::Manual),
                ("Auto", ExposureMode::Auto),
                ("Burn", ExposureMode::Burn),
            ] {
                let adj = Adjustments {
                    exposure_mode: mode,
                    exposure_ev: 0.0,
                    wb_offset: 0.0,
                    hue: 0.0,
                    crop: None,
                    rotation: 0,
                };
                let (pw, ph) = fit_within(base.width(), base.height(), 1024);
                let (xw, xh) = fit_within(base.width(), base.height(), 2560);
                let preview = base.render_with_ev(None, Size { width: pw, height: ph }, &adj, None);
                let export = base.render_with_ev(None, Size { width: xw, height: xh }, &adj, None);
                let (p50, p60, p90, p99, mean, clip) = lums_stats(&preview.rgba);
                let scaled = downscale_rgba(&export.rgba, xw, xh, pw, ph);
                let mut maxdiff = 0u16;
                let mut gt8 = 0usize;
                for (a, b) in preview.rgba.chunks_exact(4).zip(scaled.chunks_exact(4)) {
                    for c in 0..3 {
                        let d = a[c].abs_diff(b[c]);
                        maxdiff = maxdiff.max(d as u16);
                        if d > 8 {
                            gt8 += 1;
                        }
                    }
                }
                let gt8pct = gt8 as f32 / (preview.rgba.len() / 4) as f32 * 100.0;
                println!(
                    "  {label:11} ev={ev:.2} p50={p50:.0} p60={p60:.0} p90={p90:.0} p99={p99:.0} mean={mean:.0} clip%={clip:.1} 1024-vs-2560(->1024): maxdiff={maxdiff} >8px={gt8pct:.2}%",
                    ev = preview.auto_ev
                );
                let fname = outdir.join(format!("{stem}-{label}.png"));
                if let Ok(f) = std::fs::File::create(&fname) {
                    let mut w = std::io::BufWriter::new(f);
                    image::codecs::png::PngEncoder::new(&mut w)
                        .write_image(&preview.rgba, pw, ph, image::ExtendedColorType::Rgba8)
                        .expect("write png");
                }
            }
        }
    }

    fn make_png_png(w: u32, h: u32) -> Vec<u8> {
        // Build a solid red PNG using the image crate's encoder.
        let mut buf = Vec::new();
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([255, 0, 0, 255]));
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(&img, w, h, image::ExtendedColorType::Rgba8)
            .expect("encode png");
        buf
    }

    #[test]
    fn decodes_png_dimensions_and_pixels() {
        let png = make_png_png(8, 8);
        let (size, rgba) = decode_jpeg(&png).expect("decode");
        assert_eq!(
            size,
            Size {
                width: 8,
                height: 8
            }
        );
        assert_eq!(rgba.len(), 8 * 8 * 4);
        assert!(
            rgba.chunks_exact(4).all(|p| p == [255, 0, 0, 255]),
            "not all pixels are red"
        );
    }
}
