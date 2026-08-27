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
    Aggressive,
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
            crop: None,
            rotation: 0,
        }
    }
}

/// Camera-WB, sRGB-primaries, LINEAR (gamma-decoded) planar RGB.
/// `r/g/b` are `width * height` f32 linear values, matching photoup's DecodedRaw.
pub struct DecodedRaw {
    pub width: u32,
    pub height: u32,
    pub r: Vec<f32>,
    pub g: Vec<f32>,
    pub b: Vec<f32>,
    /// Camera as-shot WB multipliers (R, G, B, G2) — used to build export WB.
    pub cam_mul: Option<[f32; 4]>,
    /// Camera→sRGB color matrix (libraw `rgb_cam[3][4]`, first 3 cols are 3x3).
    pub cam_matrix: Option<[[f32; 4]; 3]>,
}
