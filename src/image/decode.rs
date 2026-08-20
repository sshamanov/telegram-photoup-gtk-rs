use crate::errors::{Error, Result};
use crate::image::types::Size;

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
