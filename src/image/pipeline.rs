use crate::errors::Result;
use crate::image::decode::decode_jpeg;
use crate::image::encode::{encode_jpeg_444_adaptive, MAX_PHOTO_BYTES};
use crate::image::process::{export_dimensions, Base, JpegBase, RenderResult};
use crate::image::types::Adjustments;

/// One-stop: decode → auto-process → render export → 4:4:4 encode.
/// Returns the final JPEG bytes and the auto-EV used.
pub fn process_jpeg_to_export(
    data: &[u8],
    adjustments: &Adjustments,
) -> Result<(Vec<u8>, f32)> {
    let (size, rgba) = decode_jpeg(data)?;
    let base = JpegBase::new(size.width, size.height, rgba);
    let export = export_dimensions(base.width(), base.height(), adjustments.crop.as_ref(), 2560);
    let RenderResult { rgba: export_rgba, auto_ev } = base.render(adjustments.crop.as_ref(), export, adjustments);
    let (w, h) = (export.width as usize, export.height as usize);
    // RGBA → RGB for the encoder.
    let mut rgb = Vec::with_capacity(w * h * 3);
    for px in export_rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[0..3]);
    }
    let jpeg = encode_jpeg_444_adaptive(&rgb, w, h, MAX_PHOTO_BYTES)?;
    Ok((jpeg, auto_ev))
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use image::ImageEncoder; // image 0.25: write_image is a trait method

    #[test]
    fn writes_verified_export() {
        // Build a synthetic dark test image (linear dark gray) through the image crate.
        let (w, h) = (2048, 2048);
        let mut img = image::RgbaImage::new(w, h);
        for (_, _, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([28, 28, 28, 255]); // ~1% sRGB → dark
        }
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&img, w, h, image::ExtendedColorType::Rgba8)
            .expect("png");
        let (jpeg, auto_ev) = process_jpeg_to_export(&png, &Adjustments::default()).expect("pipeline");
        assert!(jpeg.len() <= MAX_PHOTO_BYTES);
        // The whole point of the pipeline: a ~1% sRGB dark input MUST be auto-exposed up.
        assert!(auto_ev > 0.0, "auto-exposure did not lift the dark input (auto_ev={auto_ev})");
        // Write where the user can open it (dev-only; not committed).
        let out = "/tmp/photoup2-verify.jpg";
        std::fs::write(out, &jpeg).expect("write out");
        eprintln!("wrote {}", out);
    }
}
