//! Unsafe LibRaw FFI. The safe `libraw-rs` crate does not expose the params we need
//! (use_camera_wb, half_size, ...), so we use the generated `libraw-rs-sys` bindings
//! directly, mirroring photoup's `raw.ts` options exactly.
use crate::errors::{Error, Result};
use crate::image::srgb::srgb16_to_linear;
use crate::image::types::{DecodedRaw, PhotoMeta, Size};
use libraw_sys::*;

/// A libraw fixed-size `char` field (NUL-terminated, possibly empty) as a String.
/// Non-UTF-8 bytes (some maker notes are Latin-1) are replaced rather than
/// dropped, so a lens name with an odd byte still shows up.
fn cstr_to_string(buf: &[std::os::raw::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// Format a POSIX timestamp as "YYYY-MM-DD HH:MM:SS" in LOCAL time.
///
/// LibRaw fills `other.timestamp` from EXIF `DateTimeOriginal` by reading the
/// camera's zone-less local string as local time (mktime), so the inverse —
/// `localtime_r` — reproduces the exact text the camera wrote. Rendering the
/// value as UTC instead shifts every displayed shot time by the host's UTC
/// offset (a CEST host showed 06:11 for an 08:11 capture).
fn format_epoch_local(ts: i64) -> Option<String> {
    if ts <= 0 {
        return None;
    }
    let t = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    ))
}

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

    /// Capture metadata for the editor's Image section. Available straight after
    /// `open_buffer` (LibRaw parses the IFDs and maker notes there) — no unpack or
    /// develop needed.
    ///
    /// Values LibRaw could not parse come back as 0 (and an empty `make`/`model`),
    /// which is why every field is filtered here rather than at display time: a
    /// 0 ISO or a 0s shutter is "unknown", never a real capture value.
    pub fn meta(&self) -> PhotoMeta {
        let d = unsafe { &*self.inner };
        let make = cstr_to_string(&d.idata.make);
        // Some bodies leave `model` as the bare number and put the family in
        // `normalized_model`; where both are filled they usually agree. Take
        // whichever is longer so neither variant loses information.
        let raw_model = cstr_to_string(&d.idata.model);
        let norm_model = cstr_to_string(&d.idata.normalized_model);
        let model = if norm_model.len() >= raw_model.len() {
            norm_model
        } else {
            raw_model
        };
        let lens_exif = cstr_to_string(&d.lens.Lens);
        let lens = if lens_exif.is_empty() {
            cstr_to_string(&d.lens.LensMake)
        } else {
            lens_exif
        };
        PhotoMeta::from_parts(
            &make,
            &model,
            &lens,
            d.other.shutter,
            d.other.aperture,
            d.other.iso_speed,
            format_epoch_local(d.other.timestamp as i64),
        )
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
            meta: self.meta(),
        })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        unsafe { libraw_close(self.inner) }
    }
}

#[cfg(test)]
mod time_tests {
    use super::format_epoch_local;

    #[test]
    fn capture_time_round_trips_through_the_local_zone() {
        // LibRaw's timestamp is a local-time epoch (EXIF carries no zone), so the
        // rendered text must equal the camera's own "YYYY:MM:DD HH:MM:SS" string
        // whatever the host zone is — the format is what is being checked here.
        let ts = 1_774_403_495; // 2026-03-25 12:31:35 +01:00
        let text = format_epoch_local(ts).expect("valid stamp");
        assert_eq!(text.len(), 19, "YYYY-MM-DD HH:MM:SS");
        assert_eq!(&text[4..5], "-");
        assert_eq!(&text[10..11], " ");
        assert_eq!(&text[13..14], ":");
        // 0 (unparsed) is "unknown", never 1970-01-01.
        assert_eq!(format_epoch_local(0), None);
        assert_eq!(format_epoch_local(-1), None);
    }
}
