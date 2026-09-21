/// Port of photoup `src/lib/image/types.ts`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    Raw,
    Jpeg,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Crop in normalized source coordinates (0..1), orientation-independent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedCrop {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExposureMode {
    Auto,
    Burn,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Adjustments {
    pub exposure_mode: ExposureMode,
    pub exposure_ev: f32,
    /// Relative warmth offset (-1..+1, 0 = camera as-shot / no change).
    pub wb_offset: f32,
    /// Hue (green↔magenta) tint, -1..+1, 0 = neutral.
    pub hue: f32,
    /// Saturation, -1..+1: 0 = unchanged, -1 = fully gray, +1 = double. Applied
    /// after the tone curve, in display space, preserving luma.
    pub saturation: f32,
    /// Black point, -0.5..+0.5, applied LAST in display space as
    /// `x' = (x - b) / (1 - b)`: positive crushes the floor to black (more
    /// contrast), negative lifts it (faded/matte). **Manual only** — the
    /// exposure Auto/Burn modes do not touch it, and nothing derives it. A
    /// stretch that keeps white at white necessarily darkens what lies between,
    /// so a derived value would quietly undo the auto exposure's median-on-128
    /// promise; leaving it to the slider keeps that promise intact until the
    /// user asks for the crush.
    pub black_point: f32,
    pub crop: Option<NormalizedCrop>,
    /// User rotation in clockwise quarter-turns (0..3). 0 = none, 1 = 90° CW,
    /// 2 = 180°, 3 = 270° CW (90° CCW). Applied on top of any EXIF orientation.
    pub rotation: u8,
}

impl Default for Adjustments {
    fn default() -> Self {
        Self {
            exposure_mode: ExposureMode::Auto,
            exposure_ev: 0.0,
            wb_offset: 0.0,
            hue: 0.0,
            saturation: 0.0,
            black_point: 0.0,
            crop: None,
            rotation: 0,
        }
    }
}

/// Camera-WB, sRGB-primaries, LINEAR (gamma-decoded) planar RGB.
/// `r/g/b` are `width * height` f32 linear values, matching photoup's DecodedRaw.
pub struct DecodedRaw {
    /// Dimensions LibRaw will develop at full resolution. These intentionally
    /// differ from `width`/`height` for an interactive `half_size` decode.
    pub developed_size: Size,
    pub width: u32,
    pub height: u32,
    pub r: Vec<f32>,
    pub g: Vec<f32>,
    pub b: Vec<f32>,
    /// Camera as-shot WB multipliers (R, G, B, G2) — used to build export WB.
    pub cam_mul: Option<[f32; 4]>,
    /// Camera→sRGB color matrix (libraw `rgb_cam[3][4]`, first 3 cols are 3x3).
    pub cam_matrix: Option<[[f32; 4]; 3]>,
    /// EXIF-ish capture metadata for the editor's Image section.
    pub meta: PhotoMeta,
}

/// Capture metadata for the editor's Image section (camera, lens, exposure
/// triangle, date). Every field is optional and independent: a JPEG stripped of
/// EXIF, or a RAW whose maker notes LibRaw could not parse, simply shows fewer
/// lines instead of a placeholder. The struct holds raw values, not formatted
/// text — presentation belongs to the UI.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PhotoMeta {
    /// Camera make + model, already joined (e.g. "NIKON D810"); the model alone
    /// when it already contains the make (LibRaw's `normalized_model` often does).
    pub camera: Option<String>,
    pub lens: Option<String>,
    /// EXIF ExposureTime in seconds (1/500 → 0.002).
    pub shutter_s: Option<f32>,
    /// EXIF FNumber (the f/N ratio, e.g. 5.6).
    pub aperture: Option<f32>,
    pub iso: Option<f32>,
    /// Capture date, formatted "YYYY-MM-DD HH:MM:SS".
    pub date: Option<String>,
}

impl PhotoMeta {
    /// Build from raw EXIF pieces, dropping empty strings and non-positive
    /// numbers ("unknown" in both LibRaw and EXIF is 0 / empty). Shared by the
    /// LibRaw and `exif`-crate readers so both produce the same display.
    ///
    /// `make` + `model` are joined unless the model already names the maker
    /// ("NIKON CORPORATION" + "NIKON D810" must not read as the make twice).
    pub fn from_parts(
        make: &str,
        model: &str,
        lens: &str,
        shutter_s: f32,
        aperture: f32,
        iso: f32,
        date: Option<String>,
    ) -> Self {
        let make = make.trim();
        let model = model.trim();
        let camera = match (make.is_empty(), model.is_empty()) {
            (true, true) => None,
            (false, true) => Some(make.to_string()),
            (true, false) => Some(model.to_string()),
            (false, false) => {
                let (m, mk) = (model.to_lowercase(), make.to_lowercase());
                // Maker-prefixed brands write Make="NIKON CORPORATION" but
                // Model="NIKON D810" — the model's leading word already names the
                // maker, so joining would print it twice.
                let mk_first = mk.split_whitespace().next().unwrap_or("");
                let model_names_maker = m.contains(&mk)
                    || (!mk_first.is_empty() && m.split_whitespace().next() == Some(mk_first));
                if model_names_maker {
                    Some(model.to_string())
                } else {
                    Some(format!("{make} {model}"))
                }
            }
        };
        let lens = lens.trim();
        Self {
            camera,
            lens: (!lens.is_empty()).then(|| lens.to_string()),
            shutter_s: (shutter_s.is_finite() && shutter_s > 0.0).then_some(shutter_s),
            aperture: (aperture.is_finite() && aperture > 0.0).then_some(aperture),
            iso: (iso.is_finite() && iso > 0.0).then_some(iso),
            date,
        }
    }

    /// True when there is nothing at all to show (skip the whole block).
    pub fn is_empty(&self) -> bool {
        self.camera.is_none()
            && self.lens.is_none()
            && self.shutter_s.is_none()
            && self.aperture.is_none()
            && self.iso.is_none()
            && self.date.is_none()
    }

    /// Shutter as photographers write it: 1/500 rather than 0.002s. Anything at
    /// or above 1s is shown in seconds with a trailing "s" ("2.5s").
    pub fn shutter_text(&self) -> Option<String> {
        let t = self.shutter_s?;
        if !t.is_finite() || t <= 0.0 {
            return None;
        }
        if t < 1.0 {
            Some(format!("1/{}", (1.0 / t).round() as i64))
        } else {
            Some(format!("{t:.1}s"))
        }
    }

    /// "1/500 · f/5.6 · ISO 400" — only the components actually present, so a
    /// RAW with no maker notes still shows the aperture it does have.
    pub fn exposure_line(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(s) = self.shutter_text() {
            parts.push(s);
        }
        if let Some(f) = self.aperture {
            if f.is_finite() && f > 0.0 {
                parts.push(format!("f/{f:.1}"));
            }
        }
        if let Some(iso) = self.iso {
            if iso.is_finite() && iso > 0.0 {
                parts.push(format!("ISO {}", iso.round() as i64));
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" · "))
        }
    }
}

#[cfg(test)]
mod meta_tests {
    use super::PhotoMeta;

    #[test]
    fn camera_never_repeats_the_maker() {
        // Canon writes the maker inside the model; Nikon writes it in front of
        // the model under a longer Make, and Sony's model names no maker at all.
        let canon = PhotoMeta::from_parts("Canon", "Canon EOS 5D Mark IV", "", 0.0, 0.0, 0.0, None);
        assert_eq!(canon.camera.as_deref(), Some("Canon EOS 5D Mark IV"));
        let nikon = PhotoMeta::from_parts(
            "NIKON CORPORATION",
            "NIKON D810",
            "",
            0.0,
            0.0,
            0.0,
            None,
        );
        assert_eq!(nikon.camera.as_deref(), Some("NIKON D810"));
        let sony = PhotoMeta::from_parts("SONY", "ILCE-7RM3", "", 0.0, 0.0, 0.0, None);
        assert_eq!(sony.camera.as_deref(), Some("SONY ILCE-7RM3"));
    }

    #[test]
    fn missing_parts_produce_no_placeholder_text() {
        // LibRaw reports unparsed numbers as 0 and an unparsed maker note as an
        // empty string; none of that may reach the UI as "ISO 0" or "f/0.0".
        let m = PhotoMeta::from_parts("", "", "", 0.0, 0.0, 0.0, None);
        assert!(m.is_empty());
        assert_eq!(m.exposure_line(), None);
        // A partial capture still shows the parts it has.
        let m = PhotoMeta::from_parts("", "", "", 0.002, 5.6, 400.0, None);
        assert_eq!(m.exposure_line().as_deref(), Some("1/500 · f/5.6 · ISO 400"));
        // ≥1s reads in seconds instead of "1/1".
        let m = PhotoMeta::from_parts("", "", "", 2.5, 0.0, 0.0, None);
        assert_eq!(m.exposure_line().as_deref(), Some("2.5s"));
    }
}
