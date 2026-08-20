use crate::image::types::Rect;

/// Area-average (box) resize of a single 1-D row/column. Downscale only.
fn resize_box1d(src: &[f32], dst_len: usize) -> Vec<f32> {
    let src_len = src.len();
    if src_len == dst_len {
        return src.to_vec();
    }
    let scale = src_len as f32 / dst_len as f32;
    let mut out = vec![0.0f32; dst_len];
    for j in 0..dst_len {
        let a = j as f32 * scale;
        let b = ((j + 1) as f32 * scale).min(src_len as f32);
        let i0 = a.floor() as usize;
        let i1 = b.floor().min(src_len as f32 - 1.0) as usize;
        let mut sum = 0.0f32;
        if i0 == i1 {
            sum = (b - a) * src[i0];
        } else {
            sum += (i0 as f32 + 1.0 - a) * src[i0];
            for i in (i0 + 1)..i1 {
                sum += src[i];
            }
            sum += (b - i1 as f32) * src[i1];
        }
        out[j] = sum / (b - a);
    }
    out
}

/// Downscale a row-major f32 plane from src_w×src_h to dst_w×dst_h using a separable
/// area-average filter in linear space. Always returns a new Vec (the input is never
/// mutated); when the size is unchanged it returns a copy of the input.
pub fn downscale_plane(plane: &[f32], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<f32> {
    debug_assert!(dst_w <= src_w && dst_h <= src_h, "downscale only");
    if dst_w == src_w && dst_h == src_h {
        return plane.to_vec();
    }
    // Horizontal pass.
    let mut tmp = vec![0.0f32; (dst_w * src_h) as usize];
    for y in 0..src_h as usize {
        let row = &plane[y * src_w as usize..(y + 1) * src_w as usize];
        let out = resize_box1d(row, dst_w as usize);
        tmp[y * dst_w as usize..(y + 1) * dst_w as usize].copy_from_slice(&out);
    }
    // Vertical pass.
    let mut out = vec![0.0f32; (dst_w * dst_h) as usize];
    let mut col = vec![0.0f32; src_h as usize];
    for x in 0..dst_w as usize {
        for y in 0..src_h as usize {
            col[y] = tmp[y * dst_w as usize + x];
        }
        let out_col = resize_box1d(&col, dst_h as usize);
        for y in 0..dst_h as usize {
            out[y * dst_w as usize + x] = out_col[y];
        }
    }
    out
}

/// Downscale a rectangular crop of an f32 plane to dst_w×dst_h.
pub fn downscale_crop(
    plane: &[f32],
    src_w: u32,
    src_h: u32,
    rect: &Rect,
    dst_w: u32,
    dst_h: u32,
) -> Vec<f32> {
    if rect.x == 0 && rect.y == 0 && rect.width == src_w && rect.height == src_h {
        return downscale_plane(plane, src_w, src_h, dst_w, dst_h);
    }
    let mut cropped = vec![0.0f32; (rect.width * rect.height) as usize];
    for y in 0..rect.height as usize {
        let src_off = ((rect.y as usize + y) * src_w as usize) + rect.x as usize;
        let dst_off = y * rect.width as usize;
        cropped[dst_off..dst_off + rect.width as usize]
            .copy_from_slice(&plane[src_off..src_off + rect.width as usize]);
    }
    downscale_plane(&cropped, rect.width, rect.height, dst_w, dst_h)
}

/// Same box filter but on u8 sRGB bytes.
///
/// WARNING: single u8 plane (gray/luminance only) — do NOT feed interleaved RGBA here;
/// use [`downscale_rgba`] so channels are never mixed.
pub fn downscale_plane_u8(plane: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if dst_w == src_w && dst_h == src_h {
        return plane.to_vec();
    }
    let src: Vec<f32> = plane.iter().map(|&v| v as f32).collect();
    let down = downscale_plane(&src, src_w, src_h, dst_w, dst_h);
    down.iter()
        .map(|&v| v.round().clamp(0.0, 255.0) as u8)
        .collect()
}

/// Downscale interleaved RGBA8 bytes, running the box filter on each channel
/// separately so colors are never mixed. The JPEG render path (thumbnail + export)
/// and the luminance sampler use this — the box width in bytes is rarely an exact
/// pixel multiple, so a single-plane pass would average across channels.
pub fn downscale_rgba(plane: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if dst_w == src_w && dst_h == src_h {
        return plane.to_vec();
    }
    let src_n = (src_w * src_h) as usize;
    let dst_n = (dst_w * dst_h) as usize;
    let mut out = vec![0u8; dst_n * 4];
    let mut ch = vec![0.0f32; src_n];
    for c in 0..4 {
        for i in 0..src_n {
            ch[i] = plane[i * 4 + c] as f32;
        }
        let down = downscale_plane(&ch, src_w, src_h, dst_w, dst_h);
        for i in 0..dst_n {
            out[i * 4 + c] = down[i].round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_when_same_size() {
        let p = vec![1.0f32; 16];
        assert_eq!(downscale_plane(&p, 4, 4, 4, 4), p);
    }

    #[test]
    fn box_downscale_averages() {
        // 4x4 plane of 0..16 → 2x2 average of 2x2 blocks.
        let mut p = vec![0.0f32; 16];
        for i in 0..16 {
            p[i] = i as f32;
        }
        let out = downscale_plane(&p, 4, 4, 2, 2);
        // 2x2 blocks (row-major): [0,1,4,5]=2.5; [2,3,6,7]=4.5; [8,9,12,13]=10.5; [10,11,14,15]=12.5
        assert_eq!(out, vec![2.5, 4.5, 10.5, 12.5]);
    }

    #[test]
    fn crop_then_downscale() {
        let p: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let rect = Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        }; // [5,6,9,10]
        let out = downscale_crop(&p, 4, 4, &rect, 1, 1);
        assert!((out[0] - 7.5).abs() < 1e-5);
    }

    #[test]
    fn fractional_downscale_averages_partial_pixels() {
        // 4→3: output pixels sample [0,1.333), [1.333,2.667), [2.667,4) of the ramp 0,10,20,30.
        let plane = vec![0.0, 10.0, 20.0, 30.0];
        // Hand-check: out[0] = (1*0 + 1/3*10) / (4/3) = 2.5; out[1] = (2/3*10 + 2/3*20)/(4/3)=15;
        // out[2] = (1/3*20 + 1*30)/(4/3) = 27.5. In f32, 4/3 rounds and out[0] lands on 2.5000002.
        let out = downscale_plane(&plane, 4, 1, 3, 1);
        assert_eq!(out, vec![2.5000002, 15.0, 27.5]);
    }

    #[test]
    fn rgba_downscale_keeps_channels_separate() {
        // Two horizontal pixels (RGBA): red on the left, blue on the right. 2→1 must not mix.
        // Left px = (255,0,0,255), right px = (0,0,255,255) → average keeps R and B separate.
        let plane = vec![255, 0, 0, 255, 0, 0, 255, 255];
        let out = downscale_rgba(&plane, 2, 1, 1, 1);
        // Averaging channel-wise: R=(255+0)/2≈128, G=0, B=(0+255)/2≈128, A=255.
        assert_eq!(out, vec![128, 0, 128, 255]);
    }
}
