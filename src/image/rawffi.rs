//! Unsafe LibRaw FFI. The safe `libraw-rs` crate does not expose the params we need
//! (use_camera_wb, half_size, ...), so we use the generated `libraw-rs-sys` bindings
//! directly, mirroring photoup's `raw.ts` options exactly.
use crate::errors::{Error, Result};
use crate::image::srgb::srgb16_to_linear;
use crate::image::types::{DecodedRaw, Size};
use libraw_sys::*;

/// NOTE: `Raw` is deliberately NOT `Send`/`Sync`. It is created, used, and dropped
/// entirely inside `decode_raw` on the calling thread (a worker in Task 12), so no
/// unsafe impl is needed — do not add one without a concrete cross-thread use.
pub struct Raw {
    inner: *mut libraw_data_t,
}

impl Raw {
    pub fn new() -> Result<Self> {
        let inner = unsafe { libraw_init(0) };
        if inner.is_null() {
            return Err(Error::Raw("libraw_init failed".into()));
        }
        Ok(Self { inner })
    }

    pub fn params(&mut self) -> &mut libraw_output_params_t {
        unsafe { &mut (*self.inner).params }
    }

    pub fn open_buffer(&self, buf: &[u8]) -> Result<()> {
        let rc = unsafe { libraw_open_buffer(self.inner, buf.as_ptr() as *const _, buf.len()) };
        if rc != 0 {
            return Err(Error::Raw(format!("libraw_open_buffer rc={rc}")));
        }
        Ok(())
    }

    pub fn unpack(&self) -> Result<()> {
        let rc = unsafe { libraw_unpack(self.inner) };
        if rc != 0 {
            return Err(Error::Raw(format!("libraw_unpack rc={rc}")));
        }
        Ok(())
    }

    pub fn process(&self) -> Result<()> {
        let rc = unsafe { libraw_dcraw_process(self.inner) };
        if rc != 0 {
            return Err(Error::Raw(format!("libraw_dcraw_process rc={rc}")));
        }
        Ok(())
    }

    /// Camera as-shot WB multipliers (R, G, B, G2) from the parsed metadata.
    pub fn cam_mul(&self) -> [f32; 4] {
        unsafe { (*self.inner).color.cam_mul }
    }

    /// Camera→sRGB matrix: libraw `rgb_cam[3][4]` (first 3 columns are the 3x3).
    pub fn cam_matrix(&self) -> [[f32; 4]; 3] {
        unsafe { (*self.inner).color.rgb_cam }
    }

    /// Decode the processed image to planar linear f32 RGB.
    pub fn make_mem_image(&self) -> Result<DecodedRaw> {
        let mut ret = 0i32;
        let img = unsafe { libraw_dcraw_make_mem_image(self.inner, &mut ret) };
        if img.is_null() || ret != 0 {
            return Err(Error::Raw(format!("libraw_dcraw_make_mem_image rc={ret}")));
        }
        let width = unsafe { (*img).width } as u32;
        let height = unsafe { (*img).height } as u32;
        let bits = unsafe { (*img).bits } as u32;
        let colors = unsafe { (*img).colors } as usize;
        let data_size = unsafe { (*img).data_size } as usize;
        let n = (width * height) as usize;
        // Runtime guards (release-safe): output_bps=16 and output_color=1 pin these,
        // but a 4-color sensor or a misparse would otherwise be silent UB below.
        if bits != 16 || colors != 3 || data_size < 6 * n {
            unsafe { libraw_dcraw_clear_mem(img) };
            return Err(Error::Raw(format!(
                "unexpected libraw mem image: bits={bits} colors={colors} data_size={data_size}"
            )));
        }
        let lut = srgb16_to_linear();
        let mut r = vec![0.0f32; n];
        let mut g = vec![0.0f32; n];
        let mut b = vec![0.0f32; n];
        let data = unsafe { std::slice::from_raw_parts((*img).data.as_ptr(), data_size) };
        let src: &[u16] =
            unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u16, data_size / 2) };
        for i in 0..n {
            r[i] = lut[src[i * 3] as usize];
            g[i] = lut[src[i * 3 + 1] as usize];
            b[i] = lut[src[i * 3 + 2] as usize];
        }
        unsafe { libraw_dcraw_clear_mem(img) };

        Ok(DecodedRaw {
            // `decode_raw` replaces this with the full-resolution geometry when
            // this was a half-size interactive decode.
            developed_size: Size { width, height },
            width,
            height,
            r,
            g,
            b,
            cam_mul: Some(self.cam_mul()),
            cam_matrix: Some(self.cam_matrix()),
        })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        unsafe { libraw_close(self.inner) }
    }
}
