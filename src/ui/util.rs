use gdk4::Texture;
use glib::prelude::*;
use std::path::Path;

/// Build a `GdkTexture` from RGBA8 pixels (mpd-client does the same for cover art).
pub fn rgba_to_texture(rgba: &[u8], width: i32, height: i32) -> Texture {
    let bytes = glib::Bytes::from(rgba);
    gdk4::MemoryTexture::new(width, height, gdk4::MemoryFormat::R8g8b8a8, &bytes, (width * 4) as usize)
        .upcast()
}

/// The file name of a photo path (fallback "?" for weird paths).
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "?".into())
}
