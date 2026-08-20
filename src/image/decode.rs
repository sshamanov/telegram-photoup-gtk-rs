use crate::errors::{Error, Result};
use crate::image::rawffi::Raw;
use crate::image::types::{DecodedRaw, Size};

/// Decode JPEG/PNG to sRGB RGBA8 at full resolution (JPEG base).
/// `image` crate output is already sRGB; photoup keeps JPEGs in sRGB space and
/// applies exposure/WB/rolloff at render time.
pub fn decode_jpeg(data: &[u8]) -> Result<(Size, Vec<u8>)> {
    let img = image::load_from_memory(data).map_err(|e| Error::Image(e.to_string()))?;
    let rgba = img.into_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Ok((
        Size {
            width: w,
            height: h,
        },
        rgba.into_raw(),
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
    raw.make_mem_image()
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
